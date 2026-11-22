use crate::rollout;
use crate::rollout::list::parse_timestamp_uuid_from_filename;
use crate::rollout::recorder::RolloutRecorder;
use chrono::DateTime;
use chrono::NaiveDateTime;
use chrono::Timelike;
use chrono::Utc;
use codex_otel::OtelManager;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_state::BackfillStats;
use codex_state::BackfillStatus;
use codex_state::DB_ERROR_METRIC;
use codex_state::DB_METRIC_BACKFILL;
use codex_state::DB_METRIC_BACKFILL_DURATION_MS;
use codex_state::ExtractionOutcome;
use codex_state::ThreadMetadataBuilder;
use codex_state::apply_rollout_item;
use std::path::Path;
use std::path::PathBuf;
use tracing::info;
use tracing::warn;

const ROLLOUT_PREFIX: &str = "rollout-";
const ROLLOUT_SUFFIX: &str = ".jsonl";
const BACKFILL_BATCH_SIZE: usize = 200;

pub(crate) fn builder_from_session_meta(
    session_meta: &SessionMetaLine,
    rollout_path: &Path,
) -> Option<ThreadMetadataBuilder> {
    let created_at = parse_timestamp_to_utc(session_meta.meta.timestamp.as_str())?;
    let mut builder = ThreadMetadataBuilder::new(
        session_meta.meta.id,
        rollout_path.to_path_buf(),
        created_at,
        session_meta.meta.source.clone(),
    );
    builder.model_provider = session_meta.meta.model_provider.clone();
    builder.cwd = session_meta.meta.cwd.clone();
    builder.cli_version = Some(session_meta.meta.cli_version.clone());
    builder.sandbox_policy = SandboxPolicy::new_read_only_policy();
    builder.approval_mode = AskForApproval::OnRequest;
    if let Some(git) = session_meta.git.as_ref() {
        builder.git_sha = git.commit_hash.clone();
        builder.git_branch = git.branch.clone();
        builder.git_origin_url = git.repository_url.clone();
    }
    Some(builder)
}

pub(crate) fn builder_from_items(
    items: &[RolloutItem],
    rollout_path: &Path,
) -> Option<ThreadMetadataBuilder> {
    if let Some(session_meta) = items.iter().find_map(|item| match item {
        RolloutItem::SessionMeta(meta_line) => Some(meta_line),
        RolloutItem::ResponseItem(_)
        | RolloutItem::Compacted(_)
        | RolloutItem::TurnContext(_)
        | RolloutItem::TokenUsageContext { .. }
        | RolloutItem::PostTurnCompletionReviewState(_)
        | RolloutItem::EventMsg(_) => None,
    }) && let Some(builder) = builder_from_session_meta(session_meta, rollout_path)
    {
        return Some(builder);
    }

    let file_name = rollout_path.file_name()?.to_str()?;
    if !file_name.starts_with(ROLLOUT_PREFIX) || !file_name.ends_with(ROLLOUT_SUFFIX) {
        return None;
    }
    let (created_ts, uuid) = parse_timestamp_uuid_from_filename(file_name)?;
    let created_at =
        DateTime::<Utc>::from_timestamp(created_ts.unix_timestamp(), 0)?.with_nanosecond(0)?;
    let id = ThreadId::from_string(&uuid.to_string()).ok()?;
    Some(ThreadMetadataBuilder::new(
        id,
        rollout_path.to_path_buf(),
        created_at,
        SessionSource::default(),
    ))
}

pub(crate) async fn extract_metadata_from_rollout(
    rollout_path: &Path,
    default_provider: &str,
    otel: Option<&OtelManager>,
) -> anyhow::Result<ExtractionOutcome> {
    let (items, _thread_id, parse_errors) =
        RolloutRecorder::load_rollout_items(rollout_path).await?;
    if items.is_empty() {
        return Err(anyhow::anyhow!(
            "empty session file: {}",
            rollout_path.display()
        ));
    }
    let builder = builder_from_items(items.as_slice(), rollout_path).ok_or_else(|| {
        anyhow::anyhow!(
            "rollout missing metadata builder: {}",
            rollout_path.display()
        )
    })?;
    let mut metadata = builder.build(default_provider);
    for item in &items {
        apply_rollout_item(&mut metadata, item, default_provider);
    }
    if let Some(updated_at) = file_modified_time_utc(rollout_path).await {
        metadata.updated_at = updated_at;
    }
    if parse_errors > 0
        && let Some(otel) = otel
    {
        otel.counter(
            DB_ERROR_METRIC,
            parse_errors as i64,
            &[("stage", "extract_metadata_from_rollout")],
        );
    }
    Ok(ExtractionOutcome {
        metadata,
        rollout_cwd: builder.cwd,
        dynamic_tools: items.iter().find_map(|item| match item {
            RolloutItem::SessionMeta(line) if line.meta.id == builder.id => {
                line.meta.dynamic_tools.clone()
            }
            _ => None,
        }),
        parse_errors,
    })
}

pub(crate) async fn backfill_sessions(
    runtime: &codex_state::StateRuntime,
    default_provider: &str,
    otel: Option<&OtelManager>,
) {
    let timer = otel.and_then(|otel| otel.start_timer(DB_METRIC_BACKFILL_DURATION_MS, &[]).ok());
    let backfill_state = match runtime.get_backfill_state().await {
        Ok(state) => state,
        Err(err) => {
            warn!(
                "failed to read backfill state at {}: {err}",
                runtime.codex_home().display()
            );
            if let Some(otel) = otel {
                otel.counter(DB_ERROR_METRIC, 1, &[("stage", "backfill_state_read")]);
            }
            return;
        }
    };
    if backfill_state.status == BackfillStatus::Complete {
        return;
    }
    if let Err(err) = runtime
        .mark_backfill_running(backfill_state.generation)
        .await
    {
        warn!(
            "failed to mark backfill running at {}: {err}",
            runtime.codex_home().display()
        );
        if let Some(otel) = otel {
            otel.counter(
                DB_ERROR_METRIC,
                1,
                &[("stage", "backfill_state_mark_running")],
            );
        }
        return;
    }

    let sessions_root = runtime.codex_home().join(rollout::SESSIONS_SUBDIR);
    let archived_root = runtime.codex_home().join(rollout::ARCHIVED_SESSIONS_SUBDIR);
    let mut rollout_paths: Vec<BackfillRolloutPath> = Vec::new();
    for (root, archived) in [(sessions_root, false), (archived_root, true)] {
        match tokio::fs::try_exists(&root).await {
            Ok(false) => continue,
            Ok(true) => {}
            Err(err) => {
                warn!(
                    "failed to inspect rollout directory {}: {err}",
                    root.display()
                );
                return;
            }
        }
        match collect_rollout_paths(&root).await {
            Ok(paths) => {
                rollout_paths.extend(paths.into_iter().map(|path| BackfillRolloutPath {
                    watermark: backfill_watermark_for_path(runtime.codex_home(), &path),
                    path,
                    archived,
                }));
            }
            Err(err) => {
                warn!(
                    "failed to collect rollout paths under {}: {err}",
                    root.display()
                );
                return;
            }
        }
    }
    let mut deleted =
        match crate::state_db::missed_deletions(runtime.codex_home(), backfill_state.generation)
            .await
        {
            Ok(ids) => ids,
            Err(err) => {
                warn!("failed to read rollout deletion records: {err}");
                return;
            }
        };
    if !deleted.is_empty() {
        for rollout in &rollout_paths {
            if let Some((_, id)) = rollout
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(parse_timestamp_uuid_from_filename)
            {
                deleted.remove(&id);
            }
        }
        for id in deleted {
            if let Err(err) = runtime
                .delete_backfilled_thread(id, backfill_state.generation)
                .await
            {
                warn!("failed to reconcile deleted rollout {id}: {err}");
                return;
            }
        }
    }
    rollout_paths.sort_by(|a, b| a.watermark.cmp(&b.watermark));
    if let Some(last_watermark) = backfill_state.last_watermark.as_deref() {
        rollout_paths.retain(|entry| entry.watermark.as_str() > last_watermark);
    }

    let mut stats = BackfillStats {
        scanned: 0,
        upserted: 0,
        failed: 0,
    };
    let mut last_watermark = backfill_state.last_watermark.clone();
    'backfill: for batch in rollout_paths.chunks(BACKFILL_BATCH_SIZE) {
        for rollout in batch {
            stats.scanned = stats.scanned.saturating_add(1);
            if let Err(err) = backfill_rollout(
                runtime,
                default_provider,
                rollout,
                backfill_state.generation,
                otel,
            )
            .await
            {
                // Only successful batches are checkpointed; retry this batch after recovery.
                warn!("failed to index rollout {}: {err}", rollout.path.display());
                if let Some(otel) = otel {
                    otel.counter(DB_ERROR_METRIC, 1, &[("stage", "backfill_sessions")]);
                }
                stats.failed = stats.failed.saturating_add(1);
                break 'backfill;
            }
            stats.upserted = stats.upserted.saturating_add(1);
        }

        if let Some(last_entry) = batch.last() {
            if let Err(err) = runtime
                .checkpoint_backfill(backfill_state.generation, last_entry.watermark.as_str())
                .await
            {
                warn!(
                    "failed to checkpoint backfill at {}: {err}",
                    runtime.codex_home().display()
                );
                if let Some(otel) = otel {
                    otel.counter(
                        DB_ERROR_METRIC,
                        1,
                        &[("stage", "backfill_state_checkpoint")],
                    );
                }
                stats.failed = stats.failed.saturating_add(1);
                break;
            } else {
                last_watermark = Some(last_entry.watermark.clone());
            }
        }
    }
    if stats.failed == 0
        && let Err(err) = runtime
            .mark_backfill_complete(backfill_state.generation, last_watermark.as_deref())
            .await
    {
        stats.failed = stats.failed.saturating_add(1);
        warn!(
            "failed to mark backfill complete at {}: {err}",
            runtime.codex_home().display()
        );
        if let Some(otel) = otel {
            otel.counter(
                DB_ERROR_METRIC,
                1,
                &[("stage", "backfill_state_mark_complete")],
            );
        }
    }

    info!(
        "state db backfill scanned={}, upserted={}, failed={}",
        stats.scanned, stats.upserted, stats.failed
    );
    if let Some(otel) = otel {
        otel.counter(
            DB_METRIC_BACKFILL,
            stats.upserted as i64,
            &[("status", "upserted")],
        );
        otel.counter(
            DB_METRIC_BACKFILL,
            stats.failed as i64,
            &[("status", "failed")],
        );
    }
    if let Some(timer) = timer.as_ref() {
        let status = if stats.failed == 0 {
            "success"
        } else if stats.upserted == 0 {
            "failed"
        } else {
            "partial_failure"
        };
        let _ = timer.record(&[("status", status)]);
    }
}

#[derive(Debug, Clone)]
struct BackfillRolloutPath {
    watermark: String,
    path: PathBuf,
    archived: bool,
}

fn backfill_watermark_for_path(codex_home: &Path, path: &Path) -> String {
    path.strip_prefix(codex_home)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

async fn file_modified_time_utc(path: &Path) -> Option<DateTime<Utc>> {
    let modified = tokio::fs::metadata(path).await.ok()?.modified().ok()?;
    let updated_at: DateTime<Utc> = modified.into();
    updated_at.with_nanosecond(0)
}

pub(super) fn parse_timestamp_to_utc(ts: &str) -> Option<DateTime<Utc>> {
    const FILENAME_TS_FORMAT: &str = "%Y-%m-%dT%H-%M-%S";
    if let Ok(naive) = NaiveDateTime::parse_from_str(ts, FILENAME_TS_FORMAT) {
        let dt = DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc);
        return dt.with_nanosecond(0);
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
        return dt.with_timezone(&Utc).with_nanosecond(0);
    }
    None
}

async fn backfill_rollout(
    runtime: &codex_state::StateRuntime,
    default_provider: &str,
    rollout: &BackfillRolloutPath,
    generation: i64,
    otel: Option<&OtelManager>,
) -> anyhow::Result<()> {
    let outcome = extract_metadata_from_rollout(&rollout.path, default_provider, otel).await?;
    let mut metadata = outcome.metadata;
    if rollout.archived && metadata.archived_at.is_none() {
        metadata.archived_at = file_modified_time_utc(&rollout.path)
            .await
            .or(Some(metadata.updated_at));
    }
    if runtime
        .upsert_backfilled_thread(&metadata, &outcome.rollout_cwd, generation)
        .await?
    {
        runtime
            .persist_dynamic_tools(metadata.id, outcome.dynamic_tools.as_deref())
            .await?;
    }
    Ok(())
}

pub(crate) async fn collect_rollout_paths(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut stack = vec![root.to_path_buf()];
    let mut paths = Vec::new();
    while let Some(dir) = stack.pop() {
        let mut read_dir = tokio::fs::read_dir(&dir).await?;
        while let Some(entry) = read_dir.next_entry().await? {
            let path = entry.path();
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            if name.starts_with(ROLLOUT_PREFIX) && name.ends_with(ROLLOUT_SUFFIX) {
                paths.push(path);
            }
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use chrono::NaiveDateTime;
    use chrono::Timelike;
    use chrono::Utc;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::CompactedItem;
    use codex_protocol::protocol::RolloutItem;
    use codex_protocol::protocol::RolloutLine;
    use codex_protocol::protocol::SessionMeta;
    use codex_protocol::protocol::SessionMetaLine;
    use codex_protocol::protocol::SessionSource;
    use codex_state::BackfillStatus;
    use codex_state::ThreadMetadataBuilder;
    use pretty_assertions::assert_eq;
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;
    use std::path::PathBuf;
    use tempfile::tempdir;
    use uuid::Uuid;

    #[tokio::test]
    async fn extract_metadata_from_rollout_uses_session_meta() {
        let dir = tempdir().expect("tempdir");
        let uuid = Uuid::new_v4();
        let id = ThreadId::from_string(&uuid.to_string()).expect("thread id");
        let path = dir
            .path()
            .join(format!("rollout-2026-01-27T12-34-56-{uuid}.jsonl"));

        let session_meta = SessionMeta {
            id,
            forked_from_id: None,
            timestamp: "2026-01-27T12:34:56Z".to_string(),
            cwd: dir.path().to_path_buf(),
            originator: "cli".to_string(),
            cli_version: "0.0.0".to_string(),
            source: SessionSource::default(),
            model_provider: Some("openai".to_string()),
            base_instructions: None,
            dynamic_tools: None,
        };
        let session_meta_line = SessionMetaLine {
            meta: session_meta,
            git: None,
        };
        let rollout_line = RolloutLine {
            timestamp: "2026-01-27T12:34:56Z".to_string(),
            item: RolloutItem::SessionMeta(session_meta_line.clone()),
        };
        let json = serde_json::to_string(&rollout_line).expect("rollout json");
        let mut file = File::create(&path).expect("create rollout");
        writeln!(file, "{json}").expect("write rollout");

        let outcome = extract_metadata_from_rollout(&path, "openai", None)
            .await
            .expect("extract");

        let builder =
            builder_from_session_meta(&session_meta_line, path.as_path()).expect("builder");
        let mut expected = builder.build("openai");
        apply_rollout_item(&mut expected, &rollout_line.item, "openai");
        expected.updated_at = file_modified_time_utc(&path).await.expect("mtime");

        assert_eq!(outcome.metadata, expected);
        assert_eq!(outcome.parse_errors, 0);
    }

    #[test]
    fn builder_from_items_falls_back_to_filename() {
        let dir = tempdir().expect("tempdir");
        let uuid = Uuid::new_v4();
        let path = dir
            .path()
            .join(format!("rollout-2026-01-27T12-34-56-{uuid}.jsonl"));
        let items = vec![RolloutItem::Compacted(CompactedItem {
            message: "noop".to_string(),
            replacement_history: None,
        })];

        let builder = builder_from_items(items.as_slice(), path.as_path()).expect("builder");
        let naive = NaiveDateTime::parse_from_str("2026-01-27T12-34-56", "%Y-%m-%dT%H-%M-%S")
            .expect("timestamp");
        let created_at = DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc)
            .with_nanosecond(0)
            .expect("nanosecond");
        let expected = ThreadMetadataBuilder::new(
            ThreadId::from_string(&uuid.to_string()).expect("thread id"),
            path,
            created_at,
            SessionSource::default(),
        );

        assert_eq!(builder, expected);
    }

    #[tokio::test]
    async fn incomplete_backfill_retries_failed_rollouts_and_indexes_original_workspace()
    -> anyhow::Result<()> {
        let dir = tempdir()?;
        let id = Uuid::new_v4();
        let path = write_rollout_in_sessions(
            dir.path(),
            "2026-01-27T12-34-56",
            "2026-01-27T12:34:56Z",
            id,
        );
        append_picker_message(&path)?;
        let contents = std::fs::read(&path)?;
        std::fs::write(&path, [])?;
        let mut config = crate::config::test_config();
        config.codex_home = dir.path().to_path_buf();
        let runtime = codex_state::StateRuntime::init(
            config.codex_home.clone(),
            config.model_provider_id.clone(),
            None,
        )
        .await?;
        backfill_sessions(&runtime, &config.model_provider_id, None).await;
        let state = runtime.get_backfill_state().await?;
        assert_ne!(state.status, BackfillStatus::Complete);
        assert!(state.last_watermark.is_none());
        assert!(crate::state_db::get_state_db(&config, None).await.is_some());
        assert!(
            crate::state_db::open_if_present(dir.path(), &config.model_provider_id)
                .await
                .is_none()
        );
        std::fs::write(&path, contents)?;
        let list = || {
            RolloutRecorder::list_threads(
                dir.path(),
                10,
                None,
                crate::ThreadSortKey::UpdatedAt,
                &[],
                None,
                &config.model_provider_id,
            )
        };
        let from_jsonl = list().await?;
        let id = ThreadId::from_string(&id.to_string())?;
        assert_eq!(
            from_jsonl
                .items
                .iter()
                .map(|item| item.thread_id)
                .collect::<Vec<_>>(),
            vec![Some(id)]
        );
        backfill_sessions(&runtime, &config.model_provider_id, None).await;
        assert_eq!(
            runtime.get_backfill_state().await?.status,
            BackfillStatus::Complete
        );
        let mut changed = runtime.get_thread(id).await?.expect("recovered thread");
        let original = changed.clone();
        changed.cwd = dir.path().join("later-workspace");
        runtime.upsert_thread(&changed).await?;
        assert!(
            !runtime
                .upsert_backfilled_thread(&original, dir.path(), 0)
                .await?
        );
        assert_eq!(runtime.get_thread(id).await?, Some(changed));
        assert_eq!(runtime.get_thread_for_history(id).await?, Some(original));
        let indexed = list().await?;
        assert_eq!(indexed.items[0].cwd, from_jsonl.items[0].cwd);
        assert_eq!(indexed.items[0].cwd.as_deref(), Some(dir.path()));
        Ok(())
    }

    fn picker_message() -> RolloutItem {
        user_message("visible in the picker")
    }

    fn user_message(text: &str) -> RolloutItem {
        RolloutItem::EventMsg(codex_protocol::protocol::EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                message: text.into(),
                client_user_message_id: None,
                images: None,
                local_images: vec![],
                text_elements: vec![],
            },
        ))
    }

    #[tokio::test]
    async fn providerless_path_lookup_defers_legacy_backfill_to_a_configured_reader()
    -> anyhow::Result<()> {
        let home = tempdir()?;
        let mut config = crate::config::test_config();
        config.codex_home = home.path().into();
        config.model_provider_id = "configured-provider".into();
        let explicit = write_rollout_in_sessions(
            home.path(),
            "2026-01-27T11-00-00",
            "2026-01-27T19:00:00Z",
            Uuid::new_v4(),
        );
        append_picker_message(&explicit)?;
        let db = crate::state_db::init_for_rollout(&config, None)
            .await
            .unwrap();
        let id = Uuid::new_v4();
        let path = write_rollout_in_sessions(
            home.path(),
            "2026-01-27T12-00-00",
            "2026-01-27T20:00:00Z",
            id,
        );
        let mut header: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        header["payload"]
            .as_object_mut()
            .unwrap()
            .remove("model_provider");
        writeln!(File::create(&path)?, "{header}")?;
        append_picker_message(&path)?;
        crate::state_db::note_missed_write(home.path()).await?;
        assert_eq!(
            crate::find_thread_path_by_id_str(home.path(), &id.to_string()).await?,
            Some(path.clone())
        );
        let thread = ThreadId::from_string(&id.to_string())?;
        assert!(db.get_thread(thread).await?.is_none());
        let page = RolloutRecorder::list_threads(
            home.path(),
            5,
            None,
            crate::ThreadSortKey::CreatedAt,
            &[],
            Some(std::slice::from_ref(&config.model_provider_id)),
            &config.model_provider_id,
        )
        .await?;
        assert_eq!(
            page.items.iter().map(|item| &item.path).collect::<Vec<_>>(),
            [&path]
        );
        assert_eq!(
            db.get_thread(thread).await?.unwrap().model_provider,
            config.model_provider_id
        );
        let other = "test-provider".to_owned();
        let queried = crate::state_db::history_index(home.path(), &other, false)
            .await
            .unwrap();
        assert_eq!(
            queried
                .get_thread_for_history(thread)
                .await?
                .unwrap()
                .model_provider,
            other
        );
        for sql in [true, false] {
            if !sql {
                // An unavailable journal makes the same public APIs use JSONL.
                let journal = home.path().join("state_index_misses");
                std::fs::remove_file(&journal)?;
                std::fs::create_dir(journal)?;
            }
            assert_eq!(
                crate::state_db::history_index(home.path(), &other, false)
                    .await
                    .is_some(),
                sql
            );
            for (reader, expected) in [
                (
                    &other,
                    vec![(&path, None), (&explicit, Some(other.as_str()))],
                ),
                (
                    &config.model_provider_id,
                    vec![(&explicit, Some(other.as_str()))],
                ),
            ] {
                let filters = Some(std::slice::from_ref(&other));
                let page = RolloutRecorder::list_threads(
                    home.path(),
                    5,
                    None,
                    crate::ThreadSortKey::CreatedAt,
                    &[],
                    filters,
                    reader,
                )
                .await?;
                assert_eq!(
                    page.items
                        .iter()
                        .map(|item| (&item.path, item.model_provider.as_deref()))
                        .collect::<Vec<_>>(),
                    expected
                );
                assert_eq!(
                    RolloutRecorder::find_latest_thread_path(
                        home.path(),
                        1,
                        None,
                        crate::ThreadSortKey::CreatedAt,
                        &[],
                        filters,
                        reader,
                        Some(home.path())
                    )
                    .await?,
                    Some(expected[0].0.clone())
                );
            }
        }
        Ok(())
    }

    #[test_case::test_case(false; "active")]
    #[test_case::test_case(true; "archived")]
    #[tokio::test]
    async fn created_at_cursor_survives_both_reader_switches_with_local_filenames(
        archived: bool,
    ) -> anyhow::Result<()> {
        let home = tempdir()?;
        let mut config = crate::config::test_config();
        config.codex_home = home.path().into();
        let write = |filename, utc| -> anyhow::Result<PathBuf> {
            let path = write_rollout_in_sessions(home.path(), filename, utc, Uuid::new_v4());
            append_picker_message(&path)?;
            if !archived {
                return Ok(path);
            }
            let root = home.path().join(crate::rollout::ARCHIVED_SESSIONS_SUBDIR);
            std::fs::create_dir_all(&root)?;
            let target = root.join(path.file_name().unwrap());
            std::fs::rename(path, &target)?;
            Ok(target)
        };
        let oldest = write("2026-01-27T16-00-00", "2026-01-27T12:00:00Z")?;
        let middle = write("2026-01-27T05-00-00", "2026-01-27T13:00:00Z")?;
        let latest = write("2026-01-27T06-00-00", "2026-01-27T14:00:00Z")?;
        crate::state_db::init_for_rollout(&config, None)
            .await
            .unwrap();
        let mut cursor = None;
        let mut inserted = None;
        for i in 0..4 {
            if i == 1 {
                inserted = Some(write("2026-01-27T05-30-00", "2026-01-27T13:30:00Z")?);
            }
            if i == 2 {
                crate::state_db::note_missed_write(home.path()).await?;
            }
            assert_eq!(
                crate::state_db::history_index(home.path(), &config.model_provider_id, archived)
                    .await
                    .is_some(),
                i != 1
            );
            let page = if archived {
                RolloutRecorder::list_archived_threads(
                    home.path(),
                    1,
                    cursor.as_ref(),
                    crate::ThreadSortKey::CreatedAt,
                    &[],
                    None,
                    &config.model_provider_id,
                )
                .await?
            } else {
                RolloutRecorder::list_threads(
                    home.path(),
                    1,
                    cursor.as_ref(),
                    crate::ThreadSortKey::CreatedAt,
                    &[],
                    None,
                    &config.model_provider_id,
                )
                .await?
            };
            let expected = match i {
                0 => &latest,
                1 => inserted.as_ref().unwrap(),
                2 => &middle,
                _ => &oldest,
            };
            assert_eq!(
                page.items.iter().map(|item| &item.path).collect::<Vec<_>>(),
                [expected]
            );
            cursor = page.next_cursor;
            assert_eq!(cursor.is_some(), i < 3);
        }
        Ok(())
    }

    #[tokio::test]
    async fn older_writer_without_an_index_journal_is_visible_to_list_and_resume()
    -> anyhow::Result<()> {
        use crate::ThreadSortKey::UpdatedAt;
        let home = tempdir()?;
        let mut config = crate::config::test_config();
        config.codex_home = home.path().into();
        let old = write_rollout_in_sessions(
            home.path(),
            "2026-01-27T12-00-00",
            "2026-01-27T12:00:00Z",
            Uuid::new_v4(),
        );
        append_picker_message(&old)?;
        let earlier = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_760_000_000);
        File::open(&old)?.set_modified(earlier)?;
        let db = crate::state_db::init_for_rollout(&config, None)
            .await
            .unwrap();
        assert!(
            crate::state_db::history_index(home.path(), &config.model_provider_id, false)
                .await
                .is_some()
        );
        let newer = write_rollout_in_sessions(
            home.path(),
            "2026-01-27T13-00-00",
            "2026-01-27T13:00:00Z",
            Uuid::new_v4(),
        );
        append_picker_message(&newer)?;
        File::open(&newer)?.set_modified(earlier + std::time::Duration::from_secs(60))?;
        assert!(!home.path().join("state_index_misses").exists());
        assert_eq!(
            db.get_backfill_state().await?.status,
            BackfillStatus::Complete
        );
        let first = RolloutRecorder::list_threads(
            home.path(),
            1,
            None,
            UpdatedAt,
            &[],
            None,
            &config.model_provider_id,
        )
        .await?;
        assert_eq!(
            first
                .items
                .iter()
                .map(|item| &item.path)
                .collect::<Vec<_>>(),
            [&newer]
        );
        let second = RolloutRecorder::list_threads(
            home.path(),
            1,
            Some(&first.next_cursor.unwrap()),
            UpdatedAt,
            &[],
            None,
            &config.model_provider_id,
        )
        .await?;
        assert_eq!(
            second
                .items
                .iter()
                .map(|item| &item.path)
                .collect::<Vec<_>>(),
            [&old]
        );
        assert_eq!(
            RolloutRecorder::find_latest_thread_path(
                home.path(),
                1,
                None,
                UpdatedAt,
                &[],
                None,
                &config.model_provider_id,
                Some(home.path())
            )
            .await?,
            Some(newer)
        );
        Ok(())
    }

    #[tokio::test]
    async fn same_second_external_first_message_makes_a_header_only_thread_visible()
    -> anyhow::Result<()> {
        let home = tempdir()?;
        let mut config = crate::config::test_config();
        config.codex_home = home.path().into();
        let path = write_rollout_in_sessions(
            home.path(),
            "2026-01-27T12-00-00",
            "2026-01-27T12:00:00Z",
            Uuid::new_v4(),
        );
        let timestamp = File::open(&path)?.metadata()?.modified()?;
        crate::state_db::init_for_rollout(&config, None)
            .await
            .unwrap();
        let index =
            || crate::state_db::history_index(home.path(), &config.model_provider_id, false);
        assert!(
            index().await.is_some(),
            "an empty rollout is not a missing history entry"
        );
        append_picker_message(&path)?;
        File::open(&path)?.set_modified(timestamp)?;
        assert!(
            index().await.is_none(),
            "a new user event needs authoritative discovery"
        );
        let latest = RolloutRecorder::find_latest_thread_path(
            home.path(),
            1,
            None,
            crate::ThreadSortKey::UpdatedAt,
            &[],
            None,
            &config.model_provider_id,
            Some(home.path()),
        )
        .await?;
        assert_eq!(latest, Some(path));
        Ok(())
    }

    #[tokio::test]
    async fn indexed_message_survives_an_abrupt_writer_exit() -> anyhow::Result<()> {
        const CHILD_HOME: &str = "CODEX_TEST_ABRUPT_RECORDER_CHILD_HOME";
        if let Some(path) = std::env::var_os(CHILD_HOME) {
            let mut config = crate::config::test_config();
            config.codex_home = path.into();
            config.cwd = config.codex_home.clone();
            let db = crate::state_db::init_for_rollout(&config, None)
                .await
                .unwrap();
            let id = ThreadId::new();
            let params = crate::RolloutRecorderParams::new(
                id,
                None,
                SessionSource::Cli,
                config.model_provider_id.clone(),
                Default::default(),
                vec![],
            );
            let recorder = RolloutRecorder::new(&config, params, Some(db.clone()), None).await?;
            recorder.record_items(&[picker_message()]).await?;
            while db
                .get_thread(id)
                .await?
                .is_none_or(|row| row.first_user_message.is_none())
            {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            // No flush, shutdown, or destructors after the row is observable by another reader.
            std::process::exit(0);
        }
        let home = tempdir()?;
        let child = tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "rollout::metadata::tests::indexed_message_survives_an_abrupt_writer_exit",
            ])
            .env(CHILD_HOME, home.path())
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(std::time::Duration::from_secs(15), child).await??;
        anyhow::ensure!(
            output.status.success(),
            "recorder child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let paths = collect_rollout_paths(&home.path().join("sessions")).await?;
        assert_eq!(paths.len(), 1);
        let (items, _, errors) = RolloutRecorder::load_rollout_items(&paths[0]).await?;
        assert_eq!(errors, 0);
        assert_eq!(
            serde_json::to_value(items.last())?,
            serde_json::to_value(picker_message())?
        );
        Ok(())
    }

    #[tokio::test]
    async fn unavailable_index_and_journal_do_not_interrupt_jsonl_recording() -> anyhow::Result<()>
    {
        let home = tempdir()?;
        let mut config = crate::config::test_config();
        config.codex_home = home.path().into();
        config.cwd = home.path().into();
        std::fs::create_dir(codex_state::state_db_path(home.path()))?;
        std::fs::create_dir(home.path().join("state_index_misses"))?;
        let db = crate::state_db::init_for_rollout(&config, None).await;
        assert!(db.is_none());
        let params = crate::RolloutRecorderParams::new(
            ThreadId::new(),
            None,
            SessionSource::Cli,
            config.model_provider_id.clone(),
            Default::default(),
            vec![],
        );
        let recorder = RolloutRecorder::new(&config, params, db, None).await?;
        for text in ["first batch", "next batch"] {
            recorder.record_items(&[user_message(text)]).await?;
            recorder.flush().await?;
        }
        recorder.shutdown().await?;
        let (items, _, errors) =
            RolloutRecorder::load_rollout_items(recorder.rollout_path()).await?;
        assert_eq!(errors, 0);
        assert_eq!(
            serde_json::to_value(&items[1..])?,
            serde_json::to_value([user_message("first batch"), user_message("next batch")])?
        );
        Ok(())
    }

    fn append_picker_message(path: &Path) -> anyhow::Result<()> {
        let message = RolloutLine {
            timestamp: "2026-01-27T12:35:00Z".into(),
            item: picker_message(),
        };
        writeln!(
            File::options().append(true).open(path)?,
            "{}",
            serde_json::to_string(&message)?
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn completed_index_recovers_jsonl_only_recording_before_resume_last() -> anyhow::Result<()>
    {
        let home = tempdir()?;
        let mut config = crate::config::test_config();
        config.codex_home = home.path().into();
        config.cwd = home.path().into();
        let provider = &config.model_provider_id;
        let old_id = Uuid::new_v4();
        let old = write_rollout_in_sessions(
            home.path(),
            "2026-01-27T12-34-56",
            "2026-01-27T12:34:56Z",
            old_id,
        );
        append_picker_message(&old)?;
        let db = crate::state_db::init_for_rollout(&config, None)
            .await
            .expect("complete index");
        let last = || {
            RolloutRecorder::find_latest_thread_path(
                home.path(),
                10,
                None,
                crate::ThreadSortKey::CreatedAt,
                &[],
                None,
                provider,
                Some(&config.cwd),
            )
        };
        assert_eq!(last().await?, Some(old));
        let id = ThreadId::new();
        let recorder = RolloutRecorder::new(
            &config,
            crate::RolloutRecorderParams::new(
                id,
                None,
                SessionSource::Cli,
                provider.clone(),
                Default::default(),
                vec![],
            ),
            None,
            None,
        )
        .await?;
        recorder.record_items(&[picker_message()]).await?;
        recorder.shutdown().await?;
        assert!(db.get_thread(id).await?.is_none());
        let stale = db.get_backfill_state().await?;
        assert_eq!(
            (stale.status, stale.generation),
            (BackfillStatus::Complete, 0)
        );
        assert_eq!(last().await?.as_deref(), Some(recorder.rollout_path()));
        let page = RolloutRecorder::list_threads(
            home.path(),
            10,
            None,
            crate::ThreadSortKey::CreatedAt,
            &[],
            None,
            provider,
        )
        .await?;
        assert_eq!(
            page.items.iter().map(|it| it.thread_id).collect::<Vec<_>>(),
            vec![Some(id), Some(ThreadId::from_string(&old_id.to_string())?)]
        );
        assert!(db.get_thread(id).await?.is_some());
        let recovered = db.get_backfill_state().await?;
        assert_eq!(recovered.status, BackfillStatus::Complete);
        assert!(recovered.generation > stale.generation);
        Ok(())
    }

    #[tokio::test]
    async fn missed_moves_and_deletion_reconcile_a_complete_index() -> anyhow::Result<()> {
        let home = tempdir()?;
        let mut config = crate::config::test_config();
        config.codex_home = home.path().into();
        let id = Uuid::new_v4();
        let path = write_rollout_in_sessions(
            home.path(),
            "2026-01-27T12-00-00",
            "2026-01-27T12:00:00Z",
            id,
        );
        append_picker_message(&path)?;
        let id = ThreadId::from_string(&id.to_string())?;
        let unrelated = Uuid::new_v4();
        let unmounted = write_rollout_in_sessions(
            home.path(),
            "2026-01-27T10-00-00",
            "2026-01-27T10:00:00Z",
            unrelated,
        );
        let unrelated = ThreadId::from_string(&unrelated.to_string())?;
        let db = crate::state_db::init_for_rollout(&config, None)
            .await
            .unwrap();
        db.upsert_thread_memory(unrelated, "keep", "memory").await?;
        let memory = db
            .get_thread_memory(unrelated)
            .await?
            .expect("stored memory");
        std::fs::remove_file(unmounted)?;
        let archive = home.path().join(rollout::ARCHIVED_SESSIONS_SUBDIR);
        std::fs::create_dir_all(&archive)?;
        let moved = archive.join(path.file_name().unwrap());
        let active = || {
            RolloutRecorder::list_threads(
                home.path(),
                10,
                None,
                crate::ThreadSortKey::UpdatedAt,
                &[],
                None,
                &config.model_provider_id,
            )
        };
        let archived = || {
            RolloutRecorder::list_archived_threads(
                home.path(),
                10,
                None,
                crate::ThreadSortKey::UpdatedAt,
                &[],
                None,
                &config.model_provider_id,
            )
        };
        for (from, to, is_archive) in [
            (&path, &moved, true),
            (&moved, &path, false),
            (&path, &moved, true),
        ] {
            std::fs::rename(from, to)?;
            crate::state_db::record_rollout_move(home.path(), None, id, to, is_archive).await?;
            assert_eq!(
                (active().await?.items.len(), archived().await?.items.len()),
                if is_archive { (0, 1) } else { (1, 0) }
            );
            assert_eq!(db.get_thread(id).await?.unwrap().rollout_path, *to);
        }
        std::fs::remove_file(&moved)?;
        crate::state_db::delete_thread_metadata(home.path(), None, id).await?;
        assert!(db.get_thread(id).await?.is_some());
        assert!(archived().await?.items.is_empty());
        assert!(db.get_thread(id).await?.is_none());
        assert_eq!(db.get_thread_memory(unrelated).await?, Some(memory));
        Ok(())
    }

    #[tokio::test]
    async fn updated_at_resume_matches_jsonl_after_flushing_an_older_thread() -> anyhow::Result<()>
    {
        let home = tempdir()?;
        let mut config = crate::config::test_config();
        config.codex_home = home.path().into();
        config.cwd = home.path().into();
        let old = write_rollout_in_sessions(
            home.path(),
            "2026-01-27T12-00-00",
            "2026-01-27T12:00:00Z",
            Uuid::new_v4(),
        );
        let newer = write_rollout_in_sessions(
            home.path(),
            "2026-01-27T13-00-00",
            "2026-01-27T13:00:00Z",
            Uuid::new_v4(),
        );
        for (path, ts) in [(&old, 1_600_000_000), (&newer, 1_700_000_000)] {
            append_picker_message(path)?;
            File::options()
                .write(true)
                .open(path)?
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(ts))?;
        }
        let db = crate::state_db::init_for_rollout(&config, None)
            .await
            .unwrap();
        let last = || {
            RolloutRecorder::find_latest_thread_path(
                home.path(),
                10,
                None,
                crate::ThreadSortKey::UpdatedAt,
                &[],
                None,
                &config.model_provider_id,
                Some(&config.cwd),
            )
        };
        assert_eq!(last().await?, Some(newer));
        let recorder = RolloutRecorder::new(
            &config,
            crate::RolloutRecorderParams::resume(old.clone()),
            Some(db),
            builder_from_items(&[], &old),
        )
        .await?;
        recorder.record_items(&[picker_message()]).await?;
        recorder.flush().await?;
        let jsonl = crate::rollout::list::get_threads(
            home.path(),
            10,
            None,
            crate::ThreadSortKey::UpdatedAt,
            &[],
            None,
            &config.model_provider_id,
        )
        .await?;
        assert_eq!(jsonl.items[0].path, old);
        assert_eq!(last().await?, Some(old));
        recorder.shutdown().await?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unlinked_open_rollout_does_not_recreate_its_home_or_fail_flush()
    -> anyhow::Result<()> {
        let root = tempdir()?;
        let mut config = crate::config::test_config();
        config.codex_home = root.path().join("home");
        config.cwd = root.path().into();
        let recorder = RolloutRecorder::new(
            &config,
            crate::RolloutRecorderParams::new(
                ThreadId::new(),
                None,
                SessionSource::Cli,
                config.model_provider_id.clone(),
                Default::default(),
                vec![],
            ),
            None,
            None,
        )
        .await?;
        recorder.flush().await?;
        std::fs::remove_dir_all(&config.codex_home)?;
        recorder.record_items(&[picker_message()]).await?;
        recorder.shutdown().await?;
        assert!(!config.codex_home.exists());
        Ok(())
    }

    #[tokio::test]
    async fn backfill_sessions_resumes_from_watermark_and_marks_complete() {
        let dir = tempdir().expect("tempdir");
        let codex_home = dir.path().to_path_buf();
        let first_uuid = Uuid::new_v4();
        let second_uuid = Uuid::new_v4();
        let first_path = write_rollout_in_sessions(
            codex_home.as_path(),
            "2026-01-27T12-34-56",
            "2026-01-27T12:34:56Z",
            first_uuid,
        );
        let second_path = write_rollout_in_sessions(
            codex_home.as_path(),
            "2026-01-27T12-35-56",
            "2026-01-27T12:35:56Z",
            second_uuid,
        );

        let runtime =
            codex_state::StateRuntime::init(codex_home.clone(), "test-provider".to_string(), None)
                .await
                .expect("initialize runtime");
        let first_watermark =
            backfill_watermark_for_path(codex_home.as_path(), first_path.as_path());
        runtime
            .mark_backfill_running(0)
            .await
            .expect("mark running");
        runtime
            .checkpoint_backfill(0, first_watermark.as_str())
            .await
            .expect("checkpoint first watermark");

        let mut config = crate::config::test_config();
        config.codex_home = codex_home.clone();
        config.model_provider_id = "test-provider".to_string();
        backfill_sessions(runtime.as_ref(), &config.model_provider_id, None).await;

        let first_id = ThreadId::from_string(&first_uuid.to_string()).expect("first thread id");
        let second_id = ThreadId::from_string(&second_uuid.to_string()).expect("second thread id");
        assert_eq!(
            runtime
                .get_thread(first_id)
                .await
                .expect("get first thread"),
            None
        );
        assert!(
            runtime
                .get_thread(second_id)
                .await
                .expect("get second thread")
                .is_some()
        );

        let state = runtime
            .get_backfill_state()
            .await
            .expect("get backfill state");
        assert_eq!(state.status, BackfillStatus::Complete);
        assert_eq!(
            state.last_watermark,
            Some(backfill_watermark_for_path(
                codex_home.as_path(),
                second_path.as_path()
            ))
        );
        assert!(state.last_success_at.is_some());
    }

    fn write_rollout_in_sessions(
        codex_home: &Path,
        filename_ts: &str,
        event_ts: &str,
        thread_uuid: Uuid,
    ) -> PathBuf {
        let id = ThreadId::from_string(&thread_uuid.to_string()).expect("thread id");
        let sessions_dir = codex_home.join("sessions/2026/01/27");
        std::fs::create_dir_all(sessions_dir.as_path()).expect("create sessions dir");
        let path = sessions_dir.join(format!("rollout-{filename_ts}-{thread_uuid}.jsonl"));
        let session_meta = SessionMeta {
            id,
            forked_from_id: None,
            timestamp: event_ts.to_string(),
            cwd: codex_home.to_path_buf(),
            originator: "cli".to_string(),
            cli_version: "0.0.0".to_string(),
            source: SessionSource::default(),
            model_provider: Some("test-provider".to_string()),
            base_instructions: None,
            dynamic_tools: None,
        };
        let session_meta_line = SessionMetaLine {
            meta: session_meta,
            git: None,
        };
        let rollout_line = RolloutLine {
            timestamp: event_ts.to_string(),
            item: RolloutItem::SessionMeta(session_meta_line),
        };
        let json = serde_json::to_string(&rollout_line).expect("serialize rollout");
        let mut file = File::create(&path).expect("create rollout");
        writeln!(file, "{json}").expect("write rollout");
        path
    }
}
