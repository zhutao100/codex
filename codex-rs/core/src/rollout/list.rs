use futures::StreamExt;
use std::cmp::Reverse;
use std::ffi::OsStr;
use std::io::{self};
use std::num::NonZero;
use std::path::Path;
use std::path::PathBuf;
use time::OffsetDateTime;
use time::PrimitiveDateTime;
use time::format_description::FormatItem;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use uuid::Uuid;

use super::ARCHIVED_SESSIONS_SUBDIR;
use super::SESSIONS_SUBDIR;
use crate::protocol::EventMsg;
use crate::state_db;
use codex_file_search as file_search;
use codex_protocol::ThreadId;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;

/// Returned page of thread (thread) summaries.
#[derive(Debug, Default, PartialEq)]
pub struct ThreadsPage {
    /// Thread summaries ordered newest first.
    pub items: Vec<ThreadItem>,
    /// Opaque pagination token to resume after the last item, or `None` if end.
    pub next_cursor: Option<Cursor>,
    /// Total number of files touched while scanning this request.
    pub num_scanned_files: usize,
    /// True if a hard scan cap was hit; consider resuming with `next_cursor`.
    pub reached_scan_cap: bool,
}

/// Summary information for a thread rollout file.
#[derive(Debug, PartialEq)]
pub struct ThreadItem {
    /// Absolute path to the rollout file.
    pub path: PathBuf,
    /// Thread ID from session metadata.
    pub thread_id: Option<ThreadId>,
    /// First user message captured for this thread, if any.
    pub first_user_message: Option<String>,
    /// Working directory from session metadata.
    pub cwd: Option<PathBuf>,
    /// Git branch from session metadata.
    pub git_branch: Option<String>,
    /// Git commit SHA from session metadata.
    pub git_sha: Option<String>,
    /// Git origin URL from session metadata.
    pub git_origin_url: Option<String>,
    /// Session source from session metadata.
    pub source: Option<SessionSource>,
    /// Model provider from session metadata.
    pub model_provider: Option<String>,
    /// CLI version from session metadata.
    pub cli_version: Option<String>,
    /// RFC3339 timestamp string for when the session was created, if available.
    /// The rollout header is authoritative; older filenames may encode local wall time.
    pub created_at: Option<String>,
    /// RFC3339 timestamp string for the most recent update (from file mtime).
    /// updated_at is truncated to second precision to match created_at.
    pub updated_at: Option<String>,
}

#[allow(dead_code)]
#[deprecated(note = "use ThreadItem")]
pub type ConversationItem = ThreadItem;
#[allow(dead_code)]
#[deprecated(note = "use ThreadsPage")]
pub type ConversationsPage = ThreadsPage;

#[derive(Default)]
struct HeadTailSummary {
    saw_session_meta: bool,
    saw_user_event: bool,
    thread_id: Option<ThreadId>,
    first_user_message: Option<String>,
    cwd: Option<PathBuf>,
    git_branch: Option<String>,
    git_sha: Option<String>,
    git_origin_url: Option<String>,
    source: Option<SessionSource>,
    model_provider: Option<String>,
    cli_version: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

/// Hard cap to bound worst‑case work per request.
const MAX_SCAN_FILES: usize = 10000;
const HEAD_RECORD_LIMIT: usize = 10;
const USER_EVENT_SCAN_LIMIT: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadSortKey {
    CreatedAt,
    UpdatedAt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThreadListLayout {
    NestedByDate,
    Flat,
}

pub(crate) struct ThreadListConfig<'a> {
    pub(crate) allowed_sources: &'a [SessionSource],
    pub(crate) model_providers: Option<&'a [String]>,
    pub(crate) default_provider: &'a str,
    pub(crate) layout: ThreadListLayout,
}

/// Pagination cursor identifying a file by timestamp and UUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    ts: OffsetDateTime,
    id: Uuid,
}

impl Cursor {
    fn new(ts: OffsetDateTime, id: Uuid) -> Self {
        Self { ts, id }
    }
}

impl serde::Serialize for Cursor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let ts_str = self
            .ts
            .format(&Rfc3339)
            .map_err(|e| serde::ser::Error::custom(format!("format error: {e}")))?;
        serializer.serialize_str(&format!("{ts_str}|{}", self.id))
    }
}

impl<'de> serde::Deserialize<'de> for Cursor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        parse_cursor(&s).ok_or_else(|| serde::de::Error::custom("invalid cursor"))
    }
}

impl From<codex_state::Anchor> for Cursor {
    fn from(anchor: codex_state::Anchor) -> Self {
        let ts = OffsetDateTime::from_unix_timestamp(anchor.ts.timestamp())
            .unwrap_or(OffsetDateTime::UNIX_EPOCH);
        Self::new(ts, anchor.id)
    }
}

/// Retrieve recorded thread file paths with token pagination. The returned `next_cursor`
/// can be supplied on the next call to resume after the last returned item, resilient to
/// concurrent new sessions being appended. Ordering is stable by the requested sort key
/// (timestamp desc, then UUID desc).
pub(crate) async fn get_threads(
    codex_home: &Path,
    page_size: usize,
    cursor: Option<&Cursor>,
    sort_key: ThreadSortKey,
    allowed_sources: &[SessionSource],
    model_providers: Option<&[String]>,
    default_provider: &str,
) -> io::Result<ThreadsPage> {
    let root = codex_home.join(SESSIONS_SUBDIR);
    get_threads_in_root(
        root,
        page_size,
        cursor,
        sort_key,
        ThreadListConfig {
            allowed_sources,
            model_providers,
            default_provider,
            layout: ThreadListLayout::NestedByDate,
        },
    )
    .await
}

pub(crate) async fn get_threads_in_root(
    root: PathBuf,
    page_size: usize,
    cursor: Option<&Cursor>,
    sort_key: ThreadSortKey,
    config: ThreadListConfig<'_>,
) -> io::Result<ThreadsPage> {
    if !root.exists() {
        return Ok(ThreadsPage {
            items: Vec::new(),
            next_cursor: None,
            num_scanned_files: 0,
            reached_scan_cap: false,
        });
    }

    let provider_matcher = config
        .model_providers
        .and_then(|filters| ProviderMatcher::new(filters, config.default_provider));

    let files = match config.layout {
        ThreadListLayout::NestedByDate => collect_nested_rollout_files(&root).await?,
        ThreadListLayout::Flat => collect_flat_rollout_files(&root).await?,
    };
    let num_scanned_files = files.len();
    let mut candidates: Vec<_> = futures::stream::iter(files)
        .map(|(filename_ts, file_id, path)| async move {
            let updated = file_modified_time(&path).await.ok().flatten();
            let (ts, id) = match sort_key {
                ThreadSortKey::CreatedAt => {
                    let header = read_session_meta_line(&path).await.ok();
                    let ts = header
                        .as_ref()
                        .and_then(|line| {
                            super::metadata::parse_timestamp_to_utc(&line.meta.timestamp)
                        })
                        .and_then(|ts| OffsetDateTime::from_unix_timestamp(ts.timestamp()).ok())
                        .unwrap_or(filename_ts);
                    let id = header
                        .and_then(|line| Uuid::parse_str(&line.meta.id.to_string()).ok())
                        .unwrap_or(file_id);
                    (ts, id)
                }
                ThreadSortKey::UpdatedAt => {
                    (updated.unwrap_or(OffsetDateTime::UNIX_EPOCH), file_id)
                }
            };
            (ts, id, path, updated)
        })
        .buffer_unordered(8)
        .collect()
        .await;
    candidates.sort_unstable_by_key(|(ts, id, ..)| (Reverse(*ts), Reverse(*id)));
    let mut items = Vec::new();
    let mut last = None;
    let mut more = false;
    for (ts, id, path, updated) in candidates {
        if cursor.is_some_and(|anchor| (ts, id) >= (anchor.ts, anchor.id)) {
            continue;
        }
        if let Some(item) = build_thread_item(
            path,
            config.allowed_sources,
            provider_matcher.as_ref(),
            updated.and_then(format_rfc3339),
        )
        .await
        {
            if items.len() == page_size {
                more = true;
                break;
            }
            items.push(item);
            last = Some(Cursor::new(ts, id));
        }
    }
    let reached_scan_cap = num_scanned_files >= MAX_SCAN_FILES;
    Ok(ThreadsPage {
        items,
        next_cursor: if more || reached_scan_cap { last } else { None },
        num_scanned_files,
        reached_scan_cap,
    })
}

/// Pagination cursor token format: "<ts>|<uuid>" where `ts` uses
/// YYYY-MM-DDThh-mm-ss (UTC, second precision).
/// The cursor orders files by the requested sort key (timestamp desc, then UUID desc).
pub fn parse_cursor(token: &str) -> Option<Cursor> {
    let (file_ts, uuid_str) = token.split_once('|')?;

    let Ok(uuid) = Uuid::parse_str(uuid_str) else {
        return None;
    };

    let ts = OffsetDateTime::parse(file_ts, &Rfc3339).ok().or_else(|| {
        let format: &[FormatItem] =
            format_description!("[year]-[month]-[day]T[hour]-[minute]-[second]");
        PrimitiveDateTime::parse(file_ts, format)
            .ok()
            .map(PrimitiveDateTime::assume_utc)
    })?;

    Some(Cursor::new(ts, uuid))
}

async fn build_thread_item(
    path: PathBuf,
    allowed_sources: &[SessionSource],
    provider_matcher: Option<&ProviderMatcher<'_>>,
    updated_at: Option<String>,
) -> Option<ThreadItem> {
    // Read head and detect message events; stop once meta + user are found.
    let summary = read_head_summary(&path, HEAD_RECORD_LIMIT)
        .await
        .unwrap_or_default();
    if !allowed_sources.is_empty()
        && !summary
            .source
            .as_ref()
            .is_some_and(|source| allowed_sources.contains(source))
    {
        return None;
    }
    if let Some(matcher) = provider_matcher
        && !matcher.matches(summary.model_provider.as_deref())
    {
        return None;
    }
    // Apply filters: must have session meta and at least one user message event
    if summary.saw_session_meta && summary.saw_user_event {
        let HeadTailSummary {
            thread_id,
            first_user_message,
            cwd,
            git_branch,
            git_sha,
            git_origin_url,
            source,
            model_provider,
            cli_version,
            created_at,
            updated_at: mut summary_updated_at,
            ..
        } = summary;
        if summary_updated_at.is_none() {
            summary_updated_at = updated_at.or_else(|| created_at.clone());
        }
        return Some(ThreadItem {
            path,
            thread_id,
            first_user_message,
            cwd,
            git_branch,
            git_sha,
            git_origin_url,
            source,
            model_provider,
            cli_version,
            created_at,
            updated_at: summary_updated_at,
        });
    }
    None
}

/// Collects immediate subdirectories of `parent`, parses their (string) names with `parse`,
/// and returns them sorted descending by the parsed key.
async fn collect_dirs_desc<T, F>(parent: &Path, parse: F) -> io::Result<Vec<(T, PathBuf)>>
where
    T: Ord + Copy,
    F: Fn(&str) -> Option<T>,
{
    let mut dir = tokio::fs::read_dir(parent).await?;
    let mut vec: Vec<(T, PathBuf)> = Vec::new();
    while let Some(entry) = dir.next_entry().await? {
        if entry
            .file_type()
            .await
            .map(|ft| ft.is_dir())
            .unwrap_or(false)
            && let Some(s) = entry.file_name().to_str()
            && let Some(v) = parse(s)
        {
            vec.push((v, entry.path()));
        }
    }
    vec.sort_by_key(|(v, _)| Reverse(*v));
    Ok(vec)
}

/// Collects files in a directory and parses them with `parse`.
async fn collect_files<T, F>(parent: &Path, parse: F) -> io::Result<Vec<T>>
where
    F: Fn(&str, &Path) -> Option<T>,
{
    let mut dir = tokio::fs::read_dir(parent).await?;
    let mut collected: Vec<T> = Vec::new();
    while let Some(entry) = dir.next_entry().await? {
        if entry
            .file_type()
            .await
            .map(|ft| ft.is_file())
            .unwrap_or(false)
            && let Some(s) = entry.file_name().to_str()
            && let Some(v) = parse(s, &entry.path())
        {
            collected.push(v);
        }
    }
    Ok(collected)
}

async fn collect_flat_rollout_files(
    root: &Path,
) -> io::Result<Vec<(OffsetDateTime, Uuid, PathBuf)>> {
    let mut dir = tokio::fs::read_dir(root).await?;
    let mut collected = Vec::new();
    while let Some(entry) = dir.next_entry().await? {
        if collected.len() >= MAX_SCAN_FILES {
            break;
        }
        if !entry
            .file_type()
            .await
            .map(|ft| ft.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let file_name = entry.file_name();
        let Some(name_str) = file_name.to_str() else {
            continue;
        };
        if !name_str.starts_with("rollout-") || !name_str.ends_with(".jsonl") {
            continue;
        }
        let Some((ts, id)) = parse_timestamp_uuid_from_filename(name_str) else {
            continue;
        };
        collected.push((ts, id, entry.path()));
    }
    Ok(collected)
}

async fn collect_nested_rollout_files(
    root: &Path,
) -> io::Result<Vec<(OffsetDateTime, Uuid, PathBuf)>> {
    let mut files = Vec::new();
    for (_, year) in collect_dirs_desc(root, |s| s.parse::<u16>().ok()).await? {
        for (_, month) in collect_dirs_desc(&year, |s| s.parse::<u8>().ok()).await? {
            for (_, day) in collect_dirs_desc(&month, |s| s.parse::<u8>().ok()).await? {
                let remaining = MAX_SCAN_FILES - files.len();
                files.extend(
                    collect_rollout_day_files(&day)
                        .await?
                        .into_iter()
                        .take(remaining),
                );
                if files.len() == MAX_SCAN_FILES {
                    return Ok(files);
                }
            }
        }
    }
    Ok(files)
}

async fn collect_rollout_day_files(
    day_path: &Path,
) -> io::Result<Vec<(OffsetDateTime, Uuid, PathBuf)>> {
    let mut day_files = collect_files(day_path, |name_str, path| {
        if !name_str.starts_with("rollout-") || !name_str.ends_with(".jsonl") {
            return None;
        }

        parse_timestamp_uuid_from_filename(name_str).map(|(ts, id)| (ts, id, path.to_path_buf()))
    })
    .await?;
    // Stable ordering within the same second: (timestamp desc, uuid desc)
    day_files.sort_by_key(|(ts, sid, _path)| (Reverse(*ts), Reverse(*sid)));
    Ok(day_files)
}

pub(crate) fn parse_timestamp_uuid_from_filename(name: &str) -> Option<(OffsetDateTime, Uuid)> {
    // Expected: rollout-YYYY-MM-DDThh-mm-ss-<uuid>.jsonl
    let core = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;

    // Scan from the right for a '-' such that the suffix parses as a UUID.
    let (sep_idx, uuid) = core
        .match_indices('-')
        .rev()
        .find_map(|(i, _)| Uuid::parse_str(&core[i + 1..]).ok().map(|u| (i, u)))?;

    let ts_str = &core[..sep_idx];
    let format: &[FormatItem] =
        format_description!("[year]-[month]-[day]T[hour]-[minute]-[second]");
    let ts = PrimitiveDateTime::parse(ts_str, format).ok()?.assume_utc();
    Some((ts, uuid))
}

struct ProviderMatcher<'a> {
    filters: &'a [String],
    matches_default_provider: bool,
}

impl<'a> ProviderMatcher<'a> {
    fn new(filters: &'a [String], default_provider: &'a str) -> Option<Self> {
        if filters.is_empty() {
            return None;
        }

        let matches_default_provider = filters.iter().any(|provider| provider == default_provider);
        Some(Self {
            filters,
            matches_default_provider,
        })
    }

    fn matches(&self, session_provider: Option<&str>) -> bool {
        match session_provider {
            Some(provider) => self.filters.iter().any(|candidate| candidate == provider),
            None => self.matches_default_provider,
        }
    }
}

pub(crate) async fn is_visible_rollout(path: &Path) -> io::Result<bool> {
    let summary = read_head_summary(path, HEAD_RECORD_LIMIT).await?;
    Ok(summary.saw_session_meta && summary.saw_user_event)
}

/// Use the same user-event preview for a single rollout as for picker discovery.
pub async fn read_rollout_preview(path: &Path) -> io::Result<Option<String>> {
    Ok(read_head_summary(path, HEAD_RECORD_LIMIT)
        .await?
        .first_user_message)
}

async fn read_head_summary(path: &Path, head_limit: usize) -> io::Result<HeadTailSummary> {
    use tokio::io::AsyncBufReadExt;

    let file = tokio::fs::File::open(path).await?;
    let reader = tokio::io::BufReader::new(file);
    let mut lines = reader.lines();
    let mut summary = HeadTailSummary::default();
    let mut lines_scanned = 0usize;

    while lines_scanned < head_limit
        || (summary.saw_session_meta
            && summary.first_user_message.is_none()
            && lines_scanned < head_limit + USER_EVENT_SCAN_LIMIT)
    {
        let line_opt = lines.next_line().await?;
        let Some(line) = line_opt else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        lines_scanned += 1;

        let parsed: Result<RolloutLine, _> = serde_json::from_str(trimmed);
        let Ok(rollout_line) = parsed else { continue };

        match rollout_line.item {
            RolloutItem::SessionMeta(session_meta_line) => {
                if summary.saw_session_meta {
                    // Fork history may carry a parent header after the fork's own header.
                    continue;
                }
                summary.source = Some(session_meta_line.meta.source.clone());
                summary.model_provider = session_meta_line.meta.model_provider.clone();
                if summary.thread_id.is_none() {
                    summary.thread_id = Some(session_meta_line.meta.id);
                }
                summary.cwd = Some(session_meta_line.meta.cwd.clone());
                summary.git_branch = session_meta_line
                    .git
                    .as_ref()
                    .and_then(|git| git.branch.clone());
                summary.git_sha = session_meta_line
                    .git
                    .as_ref()
                    .and_then(|git| git.commit_hash.clone());
                summary.git_origin_url = session_meta_line
                    .git
                    .as_ref()
                    .and_then(|git| git.repository_url.clone());
                summary.cli_version = Some(session_meta_line.meta.cli_version);
                summary.created_at = Some(session_meta_line.meta.timestamp.clone());
                summary.saw_session_meta = true;
            }
            RolloutItem::ResponseItem(_) => {
                summary.created_at = summary
                    .created_at
                    .clone()
                    .or_else(|| Some(rollout_line.timestamp.clone()));
            }
            RolloutItem::TurnContext(_)
            | RolloutItem::TokenUsageContext { .. }
            | RolloutItem::PostTurnCompletionReviewState(_) => {
                // Not included in `head`; skip.
            }
            RolloutItem::Compacted(_) => {
                // Not included in `head`; skip.
            }
            RolloutItem::EventMsg(ev) => {
                if let EventMsg::UserMessage(user) = ev {
                    summary.saw_user_event = true;
                    if summary.first_user_message.is_none() {
                        summary.first_user_message = user.history_preview().map(str::to_owned);
                    }
                }
            }
        }

        if summary.saw_session_meta && summary.first_user_message.is_some() {
            break;
        }
    }

    Ok(summary)
}

/// Read up to `HEAD_RECORD_LIMIT` records from the start of the rollout file at `path`.
/// This should be enough to produce a summary including the session meta line.
pub async fn read_head_for_summary(path: &Path) -> io::Result<Vec<serde_json::Value>> {
    read_head_records(path, HEAD_RECORD_LIMIT).await
}

async fn read_head_records(path: &Path, limit: usize) -> io::Result<Vec<serde_json::Value>> {
    use tokio::io::AsyncBufReadExt;

    let file = tokio::fs::File::open(path).await?;
    let reader = tokio::io::BufReader::new(file);
    let mut lines = reader.lines();
    let mut head = Vec::new();

    while head.len() < limit {
        let Some(line) = lines.next_line().await? else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(rollout_line) = serde_json::from_str::<RolloutLine>(trimmed) {
            match rollout_line.item {
                RolloutItem::SessionMeta(session_meta_line) => {
                    if let Ok(value) = serde_json::to_value(session_meta_line) {
                        head.push(value);
                    }
                }
                RolloutItem::ResponseItem(item) => {
                    if let Ok(value) = serde_json::to_value(item) {
                        head.push(value);
                    }
                }
                RolloutItem::Compacted(_)
                | RolloutItem::TurnContext(_)
                | RolloutItem::TokenUsageContext { .. }
                | RolloutItem::PostTurnCompletionReviewState(_)
                | RolloutItem::EventMsg(_) => {}
            }
        }
    }

    Ok(head)
}

/// Read the SessionMetaLine from the head of a rollout file for reuse by
/// callers that need the session metadata (e.g. to derive a cwd for config).
pub async fn read_session_meta_line(path: &Path) -> io::Result<SessionMetaLine> {
    let head = read_head_records(path, 1).await?;
    let Some(first) = head.first() else {
        return Err(io::Error::other(format!(
            "rollout at {} is empty",
            path.display()
        )));
    };
    serde_json::from_value::<SessionMetaLine>(first.clone()).map_err(|_| {
        io::Error::other(format!(
            "rollout at {} does not start with session metadata",
            path.display()
        ))
    })
}

async fn file_modified_time(path: &Path) -> io::Result<Option<OffsetDateTime>> {
    let meta = tokio::fs::metadata(path).await?;
    let modified = meta.modified().ok();
    let Some(modified) = modified else {
        return Ok(None);
    };
    let dt = OffsetDateTime::from(modified);
    // Truncate to seconds so ordering and cursor comparisons align with the
    // cursor timestamp format (which exposes seconds), keeping pagination stable.
    Ok(truncate_to_seconds(dt))
}

fn format_rfc3339(dt: OffsetDateTime) -> Option<String> {
    dt.format(&Rfc3339).ok()
}

fn truncate_to_seconds(dt: OffsetDateTime) -> Option<OffsetDateTime> {
    dt.replace_nanosecond(0).ok()
}

async fn find_thread_path_by_id_str_in_subdir(
    codex_home: &Path,
    subdir: &str,
    id_str: &str,
) -> io::Result<Option<PathBuf>> {
    // Validate UUID format early.
    if Uuid::parse_str(id_str).is_err() {
        return Ok(None);
    }

    // Prefer DB lookup, then fall back to rollout file search.
    // TODO(jif): sqlite migration phase 1
    let archived_only = match subdir {
        SESSIONS_SUBDIR => Some(false),
        ARCHIVED_SESSIONS_SUBDIR => Some(true),
        _ => None,
    };
    let thread_id = ThreadId::from_string(id_str).ok();
    let state_db_ctx = state_db::index_for_path_lookup(codex_home).await;
    if let Some(state_db_ctx) = state_db_ctx.as_deref()
        && let Some(thread_id) = thread_id
        && let Some(db_path) = state_db::find_rollout_path_by_id(
            Some(state_db_ctx),
            thread_id,
            archived_only,
            "find_path_query",
        )
        .await
    {
        if tokio::fs::try_exists(&db_path).await.unwrap_or(false) {
            return Ok(Some(db_path));
        }
        tracing::error!(
            "state db returned stale rollout path for thread {id_str}: {}",
            db_path.display()
        );
        state_db::record_discrepancy("find_thread_path_by_id_str_in_subdir", "stale_db_path");
    }

    let mut root = codex_home.to_path_buf();
    root.push(subdir);
    if !root.exists() {
        return Ok(None);
    }
    // This is safe because we know the values are valid.
    #[allow(clippy::unwrap_used)]
    let limit = NonZero::new(1).unwrap();
    let options = file_search::FileSearchOptions {
        limit,
        compute_indices: false,
        respect_gitignore: false,
        ..Default::default()
    };

    let results = file_search::run(id_str, vec![root], options, None)
        .map_err(|e| io::Error::other(format!("file search failed: {e}")))?;

    let found = results.matches.into_iter().next().map(|m| m.full_path());
    if let Some(found_path) = found.as_ref() {
        tracing::error!("state db missing rollout path for thread {id_str}");
        state_db::record_discrepancy("find_thread_path_by_id_str_in_subdir", "falling_back");
        state_db::read_repair_rollout_path(
            state_db_ctx.as_deref(),
            thread_id,
            archived_only,
            found_path.as_path(),
        )
        .await;
    }

    Ok(found)
}

/// Locate a recorded thread rollout file by its UUID string using the existing
/// paginated listing implementation. Returns `Ok(Some(path))` if found, `Ok(None)` if not present
/// or the id is invalid.
pub async fn find_thread_path_by_id_str(
    codex_home: &Path,
    id_str: &str,
) -> io::Result<Option<PathBuf>> {
    find_thread_path_by_id_str_in_subdir(codex_home, SESSIONS_SUBDIR, id_str).await
}

/// Locate an archived thread rollout file by its UUID string.
pub async fn find_archived_thread_path_by_id_str(
    codex_home: &Path,
    id_str: &str,
) -> io::Result<Option<PathBuf>> {
    find_thread_path_by_id_str_in_subdir(codex_home, ARCHIVED_SESSIONS_SUBDIR, id_str).await
}

/// Extract the `YYYY/MM/DD` directory components from a rollout filename.
pub fn rollout_date_parts(file_name: &OsStr) -> Option<(String, String, String)> {
    let name = file_name.to_string_lossy();
    let date = name.strip_prefix("rollout-")?.get(..10)?;
    let year = date.get(..4)?.to_string();
    let month = date.get(5..7)?.to_string();
    let day = date.get(8..10)?.to_string();
    Some((year, month, day))
}
