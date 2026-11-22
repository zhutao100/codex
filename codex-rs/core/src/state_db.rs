use crate::config::Config;
use crate::rollout::list::Cursor;
use crate::rollout::list::ThreadSortKey;
use crate::rollout::metadata;
use chrono::DateTime;
use chrono::NaiveDateTime;
use chrono::Timelike;
use chrono::Utc;
use codex_otel::OtelManager;
use codex_protocol::ThreadId;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_state::DB_METRIC_COMPARE_ERROR;
pub use codex_state::LogEntry;
use codex_state::STATE_DB_VERSION;
use codex_state::ThreadMetadataBuilder;
pub use codex_state::decode_session_source;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::warn;
use uuid::Uuid;

/// Core-facing handle to the optional SQLite-backed state runtime.
pub type StateDbHandle = Arc<codex_state::StateRuntime>;

/// Start or recover the history index, but keep independent services (memories and
/// logs) available even when one rollout cannot be indexed.
pub(crate) async fn init_for_rollout(
    config: &Config,
    otel: Option<&OtelManager>,
) -> Option<StateDbHandle> {
    let runtime = match codex_state::StateRuntime::init(
        config.codex_home.clone(),
        config.model_provider_id.clone(),
        otel.cloned(),
    )
    .await
    {
        Ok(runtime) => runtime,
        Err(err) => {
            warn!(
                "failed to initialize state runtime at {}: {err}",
                config.codex_home.display()
            );
            if let Some(otel) = otel {
                otel.counter("codex.db.init", 1, &[("status", "init_error")]);
            }
            return None;
        }
    };
    verified_index(runtime.clone(), &config.model_provider_id, otel, true).await;
    Some(runtime)
}

/// Open the existing state store for services that do not require a complete history index.
pub async fn get_state_db(config: &Config, otel: Option<&OtelManager>) -> Option<StateDbHandle> {
    if config.ephemeral {
        return None;
    }
    open_database(&config.codex_home, &config.model_provider_id, otel).await
}

/// Open the index only if the file and a complete backfill already exist.
pub async fn open_if_present(codex_home: &Path, default_provider: &str) -> Option<StateDbHandle> {
    open_existing(codex_home, Some(default_provider), None).await
}

/// A path-only caller does not own the fallback provider for legacy records.
pub(crate) async fn index_for_path_lookup(home: &Path) -> Option<StateDbHandle> {
    open_existing(home, None, None).await
}

/// Other builds may change JSONL without writing our journal. Certify membership and
/// ordering before a SQL-only page can hide a file. Do not delete unrelated thread memories.
pub(crate) async fn history_index(
    home: &Path,
    provider: &str,
    archived: bool,
) -> Option<StateDbHandle> {
    let runtime = open_if_present(home, provider).await?;
    let mut indexed = runtime.indexed_history_files(archived).await.ok()?;
    let root = home.join(if archived {
        crate::rollout::ARCHIVED_SESSIONS_SUBDIR
    } else {
        crate::rollout::SESSIONS_SUBDIR
    });
    let paths = match metadata::collect_rollout_paths(&root).await {
        Ok(paths) => paths,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(_) => return None,
    };
    let unchecked =
        tokio::task::spawn_blocking(move || -> std::io::Result<Option<Vec<PathBuf>>> {
            let mut unchecked = Vec::new();
            for path in paths {
                let Some(updated) = indexed.remove(&path) else {
                    if path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .and_then(crate::rollout::list::parse_timestamp_uuid_from_filename)
                        .is_some()
                    {
                        unchecked.push(path);
                    }
                    continue;
                };
                if DateTime::<Utc>::from(path.metadata()?.modified()?).timestamp() != updated {
                    return Ok(None);
                }
            }
            Ok(indexed.is_empty().then_some(unchecked))
        })
        .await
        .ok()?
        .ok()??;
    // A newly started thread has a header but no user event. It must neither
    // invalidate every SQL page nor mask a first message appended in the same second.
    for path in unchecked {
        if crate::rollout::list::is_visible_rollout(&path).await.ok()? {
            return None;
        }
    }
    Some(runtime)
}

async fn open_existing(
    codex_home: &Path,
    default_provider: Option<&str>,
    otel: Option<&OtelManager>,
) -> Option<StateDbHandle> {
    let runtime = open_database(codex_home, default_provider.unwrap_or_default(), otel).await?;
    if let Some(provider) = default_provider {
        return verified_index(runtime, provider, otel, false).await;
    }
    let generation = missed_generation(codex_home).await.ok()?;
    let state = runtime.get_backfill_state().await.ok()?;
    (state.status == codex_state::BackfillStatus::Complete && state.generation >= generation)
        .then_some(runtime)
}

async fn open_database(
    codex_home: &Path,
    default_provider: &str,
    otel: Option<&OtelManager>,
) -> Option<StateDbHandle> {
    let db_path = codex_state::state_db_path(codex_home);
    if !tokio::fs::try_exists(&db_path).await.unwrap_or(false) {
        return None;
    }
    codex_state::StateRuntime::init(
        codex_home.to_path_buf(),
        default_provider.to_owned(),
        otel.cloned(),
    )
    .await
    .ok()
}

const MISSED_INDEX_WRITES: &str = "state_index_misses";

/// Ordinary misses are one byte. Explicit deletions also record the UUID: a scan cannot
/// infer deletion from a missing file alone. Each record is one append, even across processes.
pub(crate) async fn note_missed_write(home: &Path) -> std::io::Result<()> {
    append_miss(home, &[1]).await
}

/// An open descriptor can outlive its directory entry. Only a still-recoverable rollout
/// should invalidate the index; never recreate a removed home just to write the journal.
pub(crate) async fn note_missed_append(home: &Path, rollout: &Path) -> std::io::Result<()> {
    if tokio::fs::try_exists(rollout).await?
        && let Err(err) = note_missed_write(home).await
        && (err.kind() != std::io::ErrorKind::NotFound || tokio::fs::try_exists(rollout).await?)
    {
        return Err(err);
    }
    Ok(())
}

async fn append_miss(home: &Path, record: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(home.join(MISSED_INDEX_WRITES))
        .await?;
    file.write_all(record).await?;
    file.flush().await
}

pub(crate) async fn missed_deletions(
    home: &Path,
    generation: i64,
) -> anyhow::Result<std::collections::HashSet<Uuid>> {
    use tokio::io::AsyncReadExt;
    let mut ids = std::collections::HashSet::new();
    if generation > 0 {
        let mut records = Vec::new();
        tokio::fs::File::open(home.join(MISSED_INDEX_WRITES))
            .await?
            .take(generation.try_into()?)
            .read_to_end(&mut records)
            .await?;
        for record in records
            .split(|byte| matches!(byte, 1 | b'\n'))
            .filter(|record| !record.is_empty())
        {
            ids.insert(Uuid::try_parse_ascii(record)?);
        }
    }
    Ok(ids)
}

async fn missed_generation(home: &Path) -> std::io::Result<i64> {
    match tokio::fs::metadata(home.join(MISSED_INDEX_WRITES)).await {
        Ok(meta) if meta.is_file() => i64::try_from(meta.len()).map_err(std::io::Error::other),
        Ok(_) => Err(std::io::Error::other("index journal is not a regular file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

async fn verified_index(
    runtime: StateDbHandle,
    provider: &str,
    otel: Option<&OtelManager>,
    initialize: bool,
) -> Option<StateDbHandle> {
    let generation = missed_generation(runtime.codex_home()).await.ok()?;
    let state = runtime.get_backfill_state().await.ok()?;
    if state.generation < generation {
        runtime.invalidate_backfill(generation).await.ok()?;
    } else if state.status == codex_state::BackfillStatus::Complete {
        return Some(runtime);
    } else if !initialize && generation == 0 {
        return None;
    }
    metadata::backfill_sessions(&runtime, provider, otel).await;
    let completed = runtime.get_backfill_state().await.ok()?;
    let latest = missed_generation(runtime.codex_home()).await.ok()?;
    (completed.status == codex_state::BackfillStatus::Complete && completed.generation >= latest)
        .then_some(runtime)
}

fn cursor_to_anchor(cursor: Option<&Cursor>) -> Option<codex_state::Anchor> {
    let cursor = cursor?;
    let value = serde_json::to_value(cursor).ok()?;
    let cursor_str = value.as_str()?;
    let (ts_str, id_str) = cursor_str.split_once('|')?;
    if id_str.contains('|') {
        return None;
    }
    let id = Uuid::parse_str(id_str).ok()?;
    let ts = if let Ok(naive) = NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H-%M-%S") {
        DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc)
    } else if let Ok(dt) = DateTime::parse_from_rfc3339(ts_str) {
        dt.with_timezone(&Utc)
    } else {
        return None;
    }
    .with_nanosecond(0)?;
    Some(codex_state::Anchor { ts, id })
}

/// List thread metadata from SQLite without rollout directory traversal.
#[allow(clippy::too_many_arguments)]
pub async fn list_threads_db(
    context: Option<&codex_state::StateRuntime>,
    codex_home: &Path,
    page_size: usize,
    cursor: Option<&Cursor>,
    sort_key: ThreadSortKey,
    allowed_sources: &[SessionSource],
    model_providers: Option<&[String]>,
    archived: bool,
) -> Option<codex_state::ThreadsPage> {
    let ctx = context?;
    if ctx.codex_home() != codex_home {
        warn!(
            "state db codex_home mismatch: expected {}, got {}",
            ctx.codex_home().display(),
            codex_home.display()
        );
    }

    let anchor = cursor_to_anchor(cursor);
    let allowed_sources: Vec<String> = allowed_sources
        .iter()
        .map(|value| match serde_json::to_value(value) {
            Ok(Value::String(s)) => s,
            Ok(other) => other.to_string(),
            Err(_) => String::new(),
        })
        .collect();
    let model_providers = model_providers.map(<[String]>::to_vec);
    match ctx
        .list_threads(
            page_size,
            anchor.as_ref(),
            match sort_key {
                ThreadSortKey::CreatedAt => codex_state::SortKey::CreatedAt,
                ThreadSortKey::UpdatedAt => codex_state::SortKey::UpdatedAt,
            },
            allowed_sources.as_slice(),
            model_providers.as_deref(),
            archived,
        )
        .await
    {
        Ok(page) => Some(page),
        Err(err) => {
            warn!("state db list_threads failed: {err}");
            None
        }
    }
}

/// Look up the rollout path for a thread id using SQLite.
pub async fn find_rollout_path_by_id(
    context: Option<&codex_state::StateRuntime>,
    thread_id: ThreadId,
    archived_only: Option<bool>,
    stage: &str,
) -> Option<PathBuf> {
    let ctx = context?;
    ctx.find_rollout_path_by_id(thread_id, archived_only)
        .await
        .unwrap_or_else(|err| {
            warn!("state db find_rollout_path_by_id failed during {stage}: {err}");
            None
        })
}

/// Get dynamic tools for a thread id using SQLite.
pub async fn get_dynamic_tools(
    context: Option<&codex_state::StateRuntime>,
    thread_id: ThreadId,
    stage: &str,
) -> Option<Vec<DynamicToolSpec>> {
    let ctx = context?;
    match ctx.get_dynamic_tools(thread_id).await {
        Ok(tools) => tools,
        Err(err) => {
            warn!("state db get_dynamic_tools failed during {stage}: {err}");
            None
        }
    }
}

/// Persist dynamic tools for a thread id using SQLite, if none exist yet.
pub async fn persist_dynamic_tools(
    context: Option<&codex_state::StateRuntime>,
    thread_id: ThreadId,
    tools: Option<&[DynamicToolSpec]>,
    stage: &str,
) {
    let Some(ctx) = context else {
        return;
    };
    if let Err(err) = ctx.persist_dynamic_tools(thread_id, tools).await {
        warn!("state db persist_dynamic_tools failed during {stage}: {err}");
    }
}

/// Get memory summaries for a thread id using SQLite.
pub async fn get_thread_memory(
    context: Option<&codex_state::StateRuntime>,
    thread_id: ThreadId,
    stage: &str,
) -> Option<codex_state::ThreadMemory> {
    let ctx = context?;
    match ctx.get_thread_memory(thread_id).await {
        Ok(memory) => memory,
        Err(err) => {
            warn!("state db get_thread_memory failed during {stage}: {err}");
            None
        }
    }
}

/// Upsert memory summaries for a thread id using SQLite.
pub async fn upsert_thread_memory(
    context: Option<&codex_state::StateRuntime>,
    thread_id: ThreadId,
    trace_summary: &str,
    memory_summary: &str,
    stage: &str,
) -> Option<codex_state::ThreadMemory> {
    let ctx = context?;
    match ctx
        .upsert_thread_memory(thread_id, trace_summary, memory_summary)
        .await
    {
        Ok(memory) => Some(memory),
        Err(err) => {
            warn!("state db upsert_thread_memory failed during {stage}: {err}");
            None
        }
    }
}

/// Record a completed filesystem move, including when the derived index is unavailable.
pub async fn record_rollout_move(
    home: &Path,
    context: Option<&codex_state::StateRuntime>,
    id: ThreadId,
    path: &Path,
    archived: bool,
) -> std::io::Result<()> {
    if let Some(ctx) = context
        && matches!(ctx.set_rollout_location(id, path, archived).await, Ok(true))
    {
        return Ok(());
    }
    note_missed_write(home).await
}

/// Delete a thread row and dependent metadata, or leave a durable deletion record.
pub async fn delete_thread_metadata(
    home: &Path,
    context: Option<&codex_state::StateRuntime>,
    thread_id: ThreadId,
) -> std::io::Result<()> {
    if let Some(ctx) = context
        && matches!(ctx.delete_thread_if_complete(thread_id).await, Ok(true))
    {
        return Ok(());
    }
    append_miss(home, format!("\n{thread_id}\n").as_bytes()).await
}

/// Get the last N memories corresponding to a cwd using an exact path match.
pub async fn get_last_n_thread_memories_for_cwd(
    context: Option<&codex_state::StateRuntime>,
    cwd: &Path,
    n: usize,
    stage: &str,
) -> Option<Vec<codex_state::ThreadMemory>> {
    let ctx = context?;
    match ctx.get_last_n_thread_memories_for_cwd(cwd, n).await {
        Ok(memories) => Some(memories),
        Err(err) => {
            warn!("state db get_last_n_thread_memories_for_cwd failed during {stage}: {err}");
            None
        }
    }
}

/// Reconcile rollout items into SQLite, falling back to scanning the rollout file.
pub async fn reconcile_rollout(
    context: Option<&codex_state::StateRuntime>,
    rollout_path: &Path,
    default_provider: &str,
    builder: Option<&ThreadMetadataBuilder>,
    items: &[RolloutItem],
    archived_only: Option<bool>,
) {
    let Some(ctx) = context else {
        return;
    };
    if builder.is_some() || !items.is_empty() {
        apply_rollout_items(
            Some(ctx),
            rollout_path,
            default_provider,
            builder,
            items,
            "reconcile_rollout",
        )
        .await;
        return;
    }
    let outcome =
        match metadata::extract_metadata_from_rollout(rollout_path, default_provider, None).await {
            Ok(outcome) => outcome,
            Err(err) => {
                warn!(
                    "state db reconcile_rollout extraction failed {}: {err}",
                    rollout_path.display()
                );
                return;
            }
        };
    let mut metadata = outcome.metadata;
    match archived_only {
        Some(true) if metadata.archived_at.is_none() => {
            metadata.archived_at = Some(metadata.updated_at);
        }
        Some(false) => {
            metadata.archived_at = None;
        }
        Some(true) | None => {}
    }
    if let Err(err) = ctx
        .upsert_rollout_thread(&metadata, Some(&outcome.rollout_cwd))
        .await
    {
        warn!(
            "state db reconcile_rollout upsert failed {}: {err}",
            rollout_path.display()
        );
        return;
    }
    persist_dynamic_tools(
        Some(ctx),
        metadata.id,
        outcome.dynamic_tools.as_deref(),
        "reconcile_rollout",
    )
    .await;
}

/// Repair a thread's rollout path after filesystem fallback succeeds.
pub async fn read_repair_rollout_path(
    context: Option<&codex_state::StateRuntime>,
    thread_id: Option<ThreadId>,
    archived_only: Option<bool>,
    rollout_path: &Path,
) {
    let Some(ctx) = context else {
        return;
    };

    if let Some(thread_id) = thread_id
        && let Ok(Some(mut metadata)) = ctx.get_thread(thread_id).await
    {
        metadata.rollout_path = rollout_path.to_path_buf();
        match archived_only {
            Some(true) if metadata.archived_at.is_none() => {
                metadata.archived_at = Some(metadata.updated_at);
            }
            Some(false) => {
                metadata.archived_at = None;
            }
            Some(true) | None => {}
        }
        if let Err(err) = ctx.upsert_thread(&metadata).await {
            warn!(
                "state db read-repair upsert failed for {}: {err}",
                rollout_path.display()
            );
        } else {
            return;
        }
    }

    let Some(provider) = crate::rollout::list::read_session_meta_line(rollout_path)
        .await
        .ok()
        .and_then(|meta| meta.meta.model_provider)
    else {
        // A configured reader can supply the fallback; never cache an invented provider.
        return;
    };
    reconcile_rollout(Some(ctx), rollout_path, &provider, None, &[], archived_only).await;
}

/// Apply rollout items incrementally to SQLite.
pub async fn apply_rollout_items(
    context: Option<&codex_state::StateRuntime>,
    rollout_path: &Path,
    _default_provider: &str,
    builder: Option<&ThreadMetadataBuilder>,
    items: &[RolloutItem],
    stage: &str,
) -> bool {
    let Some(ctx) = context else {
        return false;
    };
    let mut builder = match builder {
        Some(builder) => builder.clone(),
        None => match metadata::builder_from_items(items, rollout_path) {
            Some(builder) => builder,
            None => {
                warn!(
                    "state db apply_rollout_items missing builder during {stage}: {}",
                    rollout_path.display()
                );
                record_discrepancy(stage, "missing_builder");
                return false;
            }
        },
    };
    builder.rollout_path = rollout_path.to_path_buf();
    if let Err(err) = ctx.apply_rollout_items(&builder, items, None).await {
        warn!(
            "state db apply_rollout_items failed during {stage} for {}: {err}",
            rollout_path.display()
        );
        return false;
    }
    true
}

/// Record a state discrepancy metric with a stage and reason tag.
pub fn record_discrepancy(stage: &str, reason: &str) {
    // We access the global metric because the call sites might not have access to the broader
    // OtelManager.
    tracing::warn!("state db record_discrepancy: {stage}, {reason}");
    if let Some(metric) = codex_otel::metrics::global() {
        let _ = metric.counter(
            DB_METRIC_COMPARE_ERROR,
            1,
            &[
                ("stage", stage),
                ("reason", reason),
                ("version", &STATE_DB_VERSION.to_string()),
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rollout::list::parse_cursor;
    use pretty_assertions::assert_eq;

    #[test]
    fn cursor_to_anchor_normalizes_timestamp_format() {
        let uuid = Uuid::new_v4();
        let ts_str = "2026-01-27T12-34-56";
        let token = format!("{ts_str}|{uuid}");
        let cursor = parse_cursor(token.as_str()).expect("cursor should parse");
        let anchor = cursor_to_anchor(Some(&cursor)).expect("anchor should parse");

        let naive =
            NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%dT%H-%M-%S").expect("ts should parse");
        let expected_ts = DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc)
            .with_nanosecond(0)
            .expect("nanosecond");

        assert_eq!(anchor.id, uuid);
        assert_eq!(anchor.ts, expected_ts);
    }
}
