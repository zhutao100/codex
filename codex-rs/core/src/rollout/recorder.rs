//! Persist Codex session rollouts (.jsonl) so sessions can be replayed or inspected later.

use std::fs::File;
use std::fs::{self};
use std::io::Error as IoError;
use std::path::Path;
use std::path::PathBuf;

use chrono::SecondsFormat;
use codex_protocol::ThreadId;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::BaseInstructions;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::FormatItem;
use time::macros::format_description;
use tokio::io::AsyncWriteExt;
use tokio::io::BufWriter;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::{self};
use tokio::sync::oneshot;
use tracing::info;
use tracing::trace;
use tracing::warn;

use super::ARCHIVED_SESSIONS_SUBDIR;
use super::SESSIONS_SUBDIR;
use super::list::Cursor;
use super::list::ThreadItem;
use super::list::ThreadListConfig;
use super::list::ThreadListLayout;
use super::list::ThreadSortKey;
use super::list::ThreadsPage;
use super::list::get_threads;
use super::list::get_threads_in_root;
use super::metadata;
use super::policy::is_persisted_response_item;
use crate::config::Config;
use crate::context_manager::is_user_turn_boundary;
use crate::default_client::originator;
use crate::git_info::collect_git_info;
use crate::path_utils;
use crate::protocol::EventMsg;
use crate::session_prefix::TURN_ABORTED_OPEN_TAG;
use crate::state_db;
use crate::state_db::StateDbHandle;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::ResumedHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_state::ThreadMetadataBuilder;

/// Records all [`ResponseItem`]s for a session and flushes them to disk at
/// explicit durability boundaries.
///
/// Rollouts are recorded as JSONL and can be inspected with tools such as:
///
/// ```ignore
/// $ jq -C . ~/.codex/sessions/rollout-2025-05-07T17-24-21-5973b6c0-94b8-487b-a530-2aeb6098ae0e.jsonl
/// $ fx ~/.codex/sessions/rollout-2025-05-07T17-24-21-5973b6c0-94b8-487b-a530-2aeb6098ae0e.jsonl
/// ```
#[derive(Clone)]
pub struct RolloutRecorder {
    tx: Sender<RolloutCmd>,
    pub(crate) rollout_path: PathBuf,
    state_db: Option<StateDbHandle>,
}

#[derive(Clone)]
pub enum RolloutRecorderParams {
    Create {
        conversation_id: ThreadId,
        forked_from_id: Option<ThreadId>,
        source: SessionSource,
        model_provider_id: String,
        base_instructions: BaseInstructions,
        dynamic_tools: Vec<DynamicToolSpec>,
    },
    Resume {
        path: PathBuf,
    },
}

enum RolloutCmd {
    AddItems(Vec<RolloutItem>),
    /// Ensure all prior writes are processed; respond when flushed.
    Flush {
        ack: oneshot::Sender<()>,
    },
    Shutdown {
        ack: oneshot::Sender<()>,
    },
}

impl RolloutRecorderParams {
    pub fn new(
        conversation_id: ThreadId,
        forked_from_id: Option<ThreadId>,
        source: SessionSource,
        model_provider_id: String,
        base_instructions: BaseInstructions,
        dynamic_tools: Vec<DynamicToolSpec>,
    ) -> Self {
        Self::Create {
            conversation_id,
            forked_from_id,
            source,
            model_provider_id,
            base_instructions,
            dynamic_tools,
        }
    }

    pub fn resume(path: PathBuf) -> Self {
        Self::Resume { path }
    }
}

impl RolloutRecorder {
    /// List threads (rollout files) under the provided Codex home directory.
    pub async fn list_threads(
        codex_home: &Path,
        page_size: usize,
        cursor: Option<&Cursor>,
        sort_key: ThreadSortKey,
        allowed_sources: &[SessionSource],
        model_providers: Option<&[String]>,
        default_provider: &str,
    ) -> std::io::Result<ThreadsPage> {
        Self::list_threads_with_db_fallback(
            codex_home,
            page_size,
            cursor,
            sort_key,
            allowed_sources,
            model_providers,
            default_provider,
            false,
        )
        .await
    }

    /// List archived threads (rollout files) under the archived sessions directory.
    pub async fn list_archived_threads(
        codex_home: &Path,
        page_size: usize,
        cursor: Option<&Cursor>,
        sort_key: ThreadSortKey,
        allowed_sources: &[SessionSource],
        model_providers: Option<&[String]>,
        default_provider: &str,
    ) -> std::io::Result<ThreadsPage> {
        Self::list_threads_with_db_fallback(
            codex_home,
            page_size,
            cursor,
            sort_key,
            allowed_sources,
            model_providers,
            default_provider,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn list_threads_with_db_fallback(
        codex_home: &Path,
        page_size: usize,
        cursor: Option<&Cursor>,
        sort_key: ThreadSortKey,
        allowed_sources: &[SessionSource],
        model_providers: Option<&[String]>,
        default_provider: &str,
        archived: bool,
    ) -> std::io::Result<ThreadsPage> {
        let state_db_ctx = state_db::open_if_present(codex_home, default_provider).await;
        if let Some(db_page) = state_db::list_threads_db(
            state_db_ctx.as_deref(),
            codex_home,
            page_size,
            cursor,
            sort_key,
            allowed_sources,
            model_providers,
            archived,
        )
        .await
        {
            return Ok(db_page.into());
        }
        tracing::error!("Falling back on rollout system");
        state_db::record_discrepancy("list_threads_with_db_fallback", "falling_back");

        if archived {
            let root = codex_home.join(ARCHIVED_SESSIONS_SUBDIR);
            return get_threads_in_root(
                root,
                page_size,
                cursor,
                sort_key,
                ThreadListConfig {
                    allowed_sources,
                    model_providers,
                    default_provider,
                    layout: ThreadListLayout::Flat,
                },
            )
            .await;
        }

        get_threads(
            codex_home,
            page_size,
            cursor,
            sort_key,
            allowed_sources,
            model_providers,
            default_provider,
        )
        .await
    }

    /// Find the newest recorded thread path, optionally filtering to a matching cwd.
    #[allow(clippy::too_many_arguments)]
    pub async fn find_latest_thread_path(
        codex_home: &Path,
        page_size: usize,
        cursor: Option<&Cursor>,
        sort_key: ThreadSortKey,
        allowed_sources: &[SessionSource],
        model_providers: Option<&[String]>,
        default_provider: &str,
        filter_cwd: Option<&Path>,
    ) -> std::io::Result<Option<PathBuf>> {
        let state_db_ctx = state_db::open_if_present(codex_home, default_provider).await;
        if state_db_ctx.is_some() {
            let mut db_cursor = cursor.cloned();
            loop {
                let Some(db_page) = state_db::list_threads_db(
                    state_db_ctx.as_deref(),
                    codex_home,
                    page_size,
                    db_cursor.as_ref(),
                    sort_key,
                    allowed_sources,
                    model_providers,
                    false,
                )
                .await
                else {
                    break;
                };
                if let Some(path) = select_resume_path_from_db_page(&db_page, filter_cwd) {
                    return Ok(Some(path));
                }
                db_cursor = db_page.next_anchor.map(Into::into);
                if db_cursor.is_none() {
                    break;
                }
            }
        }

        let mut cursor = cursor.cloned();
        loop {
            let page = get_threads(
                codex_home,
                page_size,
                cursor.as_ref(),
                sort_key,
                allowed_sources,
                model_providers,
                default_provider,
            )
            .await?;
            if let Some(path) = select_resume_path(&page, filter_cwd) {
                return Ok(Some(path));
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(None);
            }
        }
    }

    /// Attempt to create a new [`RolloutRecorder`]. If the sessions directory
    /// cannot be created or the rollout file cannot be opened we return the
    /// error so the caller can decide whether to disable persistence.
    pub async fn new(
        config: &Config,
        params: RolloutRecorderParams,
        state_db_ctx: Option<StateDbHandle>,
        state_builder: Option<ThreadMetadataBuilder>,
    ) -> std::io::Result<Self> {
        let (file, rollout_path, meta, default_provider_id) = match params {
            RolloutRecorderParams::Create {
                conversation_id,
                forked_from_id,
                source,
                model_provider_id,
                base_instructions,
                dynamic_tools,
            } => {
                let LogFileInfo {
                    file,
                    path,
                    conversation_id: session_id,
                    timestamp,
                } = create_log_file(config, conversation_id)?;

                let timestamp_format: &[FormatItem] = format_description!(
                    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
                );
                let timestamp = timestamp
                    .to_offset(time::UtcOffset::UTC)
                    .format(timestamp_format)
                    .map_err(|e| IoError::other(format!("failed to format timestamp: {e}")))?;

                (
                    tokio::fs::File::from_std(file),
                    path,
                    Some(SessionMeta {
                        id: session_id,
                        forked_from_id,
                        timestamp,
                        cwd: config.cwd.clone(),
                        originator: originator().value,
                        cli_version: crate::CODEX_VERSION.to_string(),
                        source,
                        model_provider: Some(model_provider_id.clone()),
                        base_instructions: Some(base_instructions),
                        dynamic_tools: if dynamic_tools.is_empty() {
                            None
                        } else {
                            Some(dynamic_tools)
                        },
                    }),
                    model_provider_id,
                )
            }
            RolloutRecorderParams::Resume { path } => (
                tokio::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .await?,
                path,
                None,
                config.model_provider_id.clone(),
            ),
        };

        // Clone the cwd for the spawned task to collect git info asynchronously
        let cwd = config.cwd.clone();

        // A reasonably-sized bounded channel. If the buffer fills up the send
        // future will yield, which is fine – we only need to ensure we do not
        // perform *blocking* I/O on the caller's thread.
        let (tx, rx) = mpsc::channel::<RolloutCmd>(256);

        // Spawn a Tokio task that owns the file handle and performs async
        // writes. Using `tokio::fs::File` keeps everything on the async I/O
        // driver instead of blocking the runtime.
        tokio::task::spawn(rollout_writer(
            file,
            rx,
            meta,
            cwd,
            rollout_path.clone(),
            state_db_ctx.clone(),
            state_builder,
            default_provider_id,
        ));

        Ok(Self {
            tx,
            rollout_path,
            state_db: state_db_ctx,
        })
    }

    pub fn rollout_path(&self) -> &Path {
        self.rollout_path.as_path()
    }

    pub fn state_db(&self) -> Option<StateDbHandle> {
        self.state_db.clone()
    }

    pub(crate) async fn record_items(&self, items: &[RolloutItem]) -> std::io::Result<()> {
        let mut filtered = Vec::new();
        for item in items {
            // Note that function calls may look a bit strange if they are
            // "fully qualified MCP tool calls," so we could consider
            // reformatting them in that case.
            if is_persisted_response_item(item) {
                filtered.push(item.clone());
            }
        }
        if filtered.is_empty() {
            return Ok(());
        }
        self.tx
            .send(RolloutCmd::AddItems(filtered))
            .await
            .map_err(|e| IoError::other(format!("failed to queue rollout items: {e}")))
    }

    /// Flush all queued writes and wait until they are committed by the writer task.
    pub async fn flush(&self) -> std::io::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(RolloutCmd::Flush { ack: tx })
            .await
            .map_err(|e| IoError::other(format!("failed to queue rollout flush: {e}")))?;
        rx.await
            .map_err(|e| IoError::other(format!("failed waiting for rollout flush: {e}")))
    }

    pub(crate) async fn load_rollout_items(
        path: &Path,
    ) -> std::io::Result<(Vec<RolloutItem>, Option<ThreadId>, usize)> {
        trace!("Resuming rollout from {path:?}");
        let text = tokio::fs::read_to_string(path).await?;
        if text.trim().is_empty() {
            return Err(IoError::other("empty session file"));
        }

        let mut items: Vec<RolloutItem> = Vec::new();
        let mut thread_id: Option<ThreadId> = None;
        let mut parse_errors = 0usize;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    warn!("failed to parse line as JSON: {line:?}, error: {e}");
                    parse_errors = parse_errors.saturating_add(1);
                    continue;
                }
            };

            // Parse the rollout line structure
            match serde_json::from_value::<RolloutLine>(v.clone()) {
                Ok(rollout_line) => match rollout_line.item {
                    RolloutItem::SessionMeta(session_meta_line) => {
                        // Use the FIRST SessionMeta encountered in the file as the canonical
                        // thread id and main session information. Keep all items intact.
                        if thread_id.is_none() {
                            thread_id = Some(session_meta_line.meta.id);
                        }
                        items.push(RolloutItem::SessionMeta(session_meta_line));
                    }
                    RolloutItem::ResponseItem(item) => {
                        items.push(RolloutItem::ResponseItem(item));
                    }
                    RolloutItem::Compacted(item) => {
                        items.push(RolloutItem::Compacted(item));
                    }
                    RolloutItem::TurnContext(item) => {
                        items.push(RolloutItem::TurnContext(item));
                    }
                    RolloutItem::EventMsg(ev) => {
                        if is_custom_control_event(&ev) {
                            continue;
                        }
                        items.push(RolloutItem::EventMsg(ev));
                    }
                },
                Err(e) => {
                    trace!("failed to parse rollout line: {e}");
                    parse_errors = parse_errors.saturating_add(1);
                }
            }
        }

        tracing::debug!(
            "Resumed rollout with {} items, thread ID: {:?}, parse errors: {}",
            items.len(),
            thread_id,
            parse_errors,
        );
        Ok((items, thread_id, parse_errors))
    }

    /// Remove rollout records that make an interrupted turn visible to future readers.
    ///
    /// This is best-effort cleanup for `/continue`: if the user continues from an interrupted
    /// turn, the prompt should look as if the interruption never happened. The live in-memory
    /// history is cleaned separately; this method keeps the JSONL compatible with vanilla
    /// branches and removes trailing interrupt markers from disk.
    pub(crate) async fn clean_for_continue(
        path: &Path,
        remove_interrupted_abort: bool,
    ) -> std::io::Result<bool> {
        let text = tokio::fs::read_to_string(path).await?;
        let mut kept_lines = Vec::new();
        let mut changed = false;

        for line in text.lines() {
            if line.trim().is_empty() {
                changed = true;
                continue;
            }

            match serde_json::from_str::<RolloutLine>(line) {
                Ok(rollout_line) => {
                    if matches!(
                        &rollout_line.item,
                        RolloutItem::EventMsg(ev) if is_custom_control_event(ev)
                    ) {
                        changed = true;
                        continue;
                    }
                    kept_lines.push((line.to_string(), rollout_line.item));
                }
                Err(e) => {
                    warn!("dropping unparsable rollout line while cleaning continuation: {e}");
                    changed = true;
                }
            }
        }

        while let Some((_line, item)) = kept_lines.last() {
            if remove_interrupted_abort && is_interrupted_abort_signal(item) {
                kept_lines.pop();
                changed = true;
            } else {
                break;
            }
        }

        if let Some(index) = first_dangling_tool_call_line_index(&kept_lines) {
            kept_lines.truncate(index);
            changed = true;
        }

        if changed {
            let mut cleaned = kept_lines
                .into_iter()
                .map(|(line, _item)| line)
                .collect::<Vec<_>>()
                .join("\n");
            if !cleaned.is_empty() {
                cleaned.push('\n');
            }
            tokio::fs::write(path, cleaned).await?;
        }

        Ok(changed)
    }

    pub async fn get_rollout_history(path: &Path) -> std::io::Result<InitialHistory> {
        let (items, thread_id, _parse_errors) = Self::load_rollout_items(path).await?;
        let conversation_id = thread_id
            .ok_or_else(|| IoError::other("failed to parse thread ID from rollout file"))?;

        if items.is_empty() {
            return Ok(InitialHistory::New);
        }

        info!("Resumed rollout successfully from {path:?}");
        Ok(InitialHistory::Resumed(ResumedHistory {
            conversation_id,
            history: items,
            rollout_path: path.to_path_buf(),
        }))
    }

    pub async fn shutdown(&self) -> std::io::Result<()> {
        let (tx_done, rx_done) = oneshot::channel();
        match self.tx.send(RolloutCmd::Shutdown { ack: tx_done }).await {
            Ok(_) => rx_done
                .await
                .map_err(|e| IoError::other(format!("failed waiting for rollout shutdown: {e}"))),
            Err(e) => {
                warn!("failed to send rollout shutdown command: {e}");
                Err(IoError::other(format!(
                    "failed to send rollout shutdown command: {e}"
                )))
            }
        }
    }
}

struct LogFileInfo {
    /// Opened file handle to the rollout file.
    file: File,

    /// Full path to the rollout file.
    path: PathBuf,

    /// Session ID (also embedded in filename).
    conversation_id: ThreadId,

    /// Timestamp for the start of the session.
    timestamp: OffsetDateTime,
}

fn create_log_file(config: &Config, conversation_id: ThreadId) -> std::io::Result<LogFileInfo> {
    // Resolve ~/.codex/sessions/YYYY/MM/DD and create it if missing.
    let timestamp = OffsetDateTime::now_local()
        .map_err(|e| IoError::other(format!("failed to get local time: {e}")))?;
    let mut dir = config.codex_home.clone();
    dir.push(SESSIONS_SUBDIR);
    dir.push(timestamp.year().to_string());
    dir.push(format!("{:02}", u8::from(timestamp.month())));
    dir.push(format!("{:02}", timestamp.day()));
    fs::create_dir_all(&dir)?;

    // Custom format for YYYY-MM-DDThh-mm-ss. Use `-` instead of `:` for
    // compatibility with filesystems that do not allow colons in filenames.
    let format: &[FormatItem] =
        format_description!("[year]-[month]-[day]T[hour]-[minute]-[second]");
    let date_str = timestamp
        .format(format)
        .map_err(|e| IoError::other(format!("failed to format timestamp: {e}")))?;

    let filename = format!("rollout-{date_str}-{conversation_id}.jsonl");

    let path = dir.join(filename);
    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)?;

    Ok(LogFileInfo {
        file,
        path,
        conversation_id,
        timestamp,
    })
}

#[allow(clippy::too_many_arguments)]
async fn rollout_writer(
    file: tokio::fs::File,
    mut rx: mpsc::Receiver<RolloutCmd>,
    mut meta: Option<SessionMeta>,
    cwd: std::path::PathBuf,
    rollout_path: PathBuf,
    state_db_ctx: Option<StateDbHandle>,
    mut state_builder: Option<ThreadMetadataBuilder>,
    default_provider: String,
) -> std::io::Result<()> {
    let mut writer = JsonlWriter {
        file: BufWriter::new(file),
    };
    if let Some(builder) = state_builder.as_mut() {
        builder.rollout_path = rollout_path.clone();
    }

    // If we have a meta, collect git info asynchronously and write meta first
    if let Some(session_meta) = meta.take() {
        let git_info = collect_git_info(&cwd).await;
        let session_meta_line = SessionMetaLine {
            meta: session_meta,
            git: git_info,
        };
        if state_db_ctx.is_some() {
            state_builder =
                metadata::builder_from_session_meta(&session_meta_line, rollout_path.as_path());
        }

        // Write the SessionMeta as the first item in the file, wrapped in a rollout line
        let rollout_item = RolloutItem::SessionMeta(session_meta_line);
        writer.write_rollout_item(&rollout_item).await?;
        state_db::reconcile_rollout(
            state_db_ctx.as_deref(),
            rollout_path.as_path(),
            default_provider.as_str(),
            state_builder.as_ref(),
            std::slice::from_ref(&rollout_item),
            None,
        )
        .await;
    }

    // Process rollout commands
    while let Some(cmd) = rx.recv().await {
        match cmd {
            RolloutCmd::AddItems(items) => {
                if items.is_empty() {
                    continue;
                }
                for item in &items {
                    writer.write_rollout_item(item).await?;
                }
                if let Some(builder) = state_builder.as_mut() {
                    builder.rollout_path = rollout_path.clone();
                }
                state_db::apply_rollout_items(
                    state_db_ctx.as_deref(),
                    rollout_path.as_path(),
                    default_provider.as_str(),
                    state_builder.as_ref(),
                    items.as_slice(),
                    "rollout_writer",
                )
                .await;
            }
            RolloutCmd::Flush { ack } => {
                // Ensure underlying file is flushed and then ack.
                if let Err(e) = writer.file.flush().await {
                    let _ = ack.send(());
                    return Err(e);
                }
                let _ = ack.send(());
            }
            RolloutCmd::Shutdown { ack } => {
                if let Err(e) = writer.file.flush().await {
                    let _ = ack.send(());
                    return Err(e);
                }
                let _ = ack.send(());
            }
        }
    }

    writer.file.flush().await?;

    Ok(())
}

struct JsonlWriter {
    file: BufWriter<tokio::fs::File>,
}

#[derive(serde::Serialize)]
struct RolloutLineRef<'a> {
    timestamp: String,
    #[serde(flatten)]
    item: &'a RolloutItem,
}

impl JsonlWriter {
    async fn write_rollout_item(&mut self, rollout_item: &RolloutItem) -> std::io::Result<()> {
        let timestamp_format: &[FormatItem] = format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        );
        let timestamp = OffsetDateTime::now_utc()
            .format(timestamp_format)
            .map_err(|e| IoError::other(format!("failed to format timestamp: {e}")))?;

        let line = RolloutLineRef {
            timestamp,
            item: rollout_item,
        };
        self.write_line(&line).await
    }
    async fn write_line(&mut self, item: &impl serde::Serialize) -> std::io::Result<()> {
        let mut json = serde_json::to_string(item)?;
        json.push('\n');
        self.file.write_all(json.as_bytes()).await?;
        Ok(())
    }
}

fn is_custom_control_event(ev: &EventMsg) -> bool {
    matches!(ev, EventMsg::TurnPaused(_) | EventMsg::TurnContinued(_))
}

fn is_interrupted_abort_signal(item: &RolloutItem) -> bool {
    match item {
        RolloutItem::EventMsg(EventMsg::TurnAborted(ev)) => {
            ev.reason == TurnAbortReason::Interrupted
        }
        RolloutItem::ResponseItem(item) => is_turn_aborted_interrupted_marker(item),
        _ => false,
    }
}

fn first_dangling_tool_call_line_index(lines: &[(String, RolloutItem)]) -> Option<usize> {
    let last_user_line_index = lines.iter().rposition(|(_line, item)| {
        matches!(item, RolloutItem::ResponseItem(response_item) if is_user_turn_boundary(response_item))
    })?;

    let mut open_calls: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut first_incomplete_local_shell: Option<usize> = None;
    for (line_index, (_line, item)) in lines.iter().enumerate().skip(last_user_line_index + 1) {
        match item {
            RolloutItem::ResponseItem(ResponseItem::FunctionCall { call_id, .. })
            | RolloutItem::ResponseItem(ResponseItem::CustomToolCall { call_id, .. }) => {
                open_calls.entry(call_id.clone()).or_insert(line_index);
            }
            RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput { call_id, .. })
            | RolloutItem::ResponseItem(ResponseItem::CustomToolCallOutput { call_id, .. }) => {
                open_calls.remove(call_id);
            }
            RolloutItem::ResponseItem(ResponseItem::LocalShellCall { status, .. })
                if !matches!(status, codex_protocol::models::LocalShellStatus::Completed) =>
            {
                first_incomplete_local_shell = Some(
                    first_incomplete_local_shell
                        .map_or(line_index, |current| current.min(line_index)),
                );
            }
            _ => {}
        }
    }

    open_calls
        .values()
        .copied()
        .chain(first_incomplete_local_shell)
        .min()
}

fn is_turn_aborted_interrupted_marker(item: &ResponseItem) -> bool {
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };

    role == "user"
        && content.iter().any(|content_item| {
            matches!(
                content_item,
                ContentItem::InputText { text } if text.starts_with(TURN_ABORTED_OPEN_TAG)
            )
        })
}

impl From<codex_state::ThreadsPage> for ThreadsPage {
    fn from(db_page: codex_state::ThreadsPage) -> Self {
        let items = db_page
            .items
            .into_iter()
            .map(|item| ThreadItem {
                path: item.rollout_path,
                thread_id: Some(item.id),
                first_user_message: item.first_user_message,
                cwd: Some(item.cwd),
                git_branch: item.git_branch,
                git_sha: item.git_sha,
                git_origin_url: item.git_origin_url,
                source: Some(
                    serde_json::from_value(Value::String(item.source))
                        .unwrap_or(SessionSource::Unknown),
                ),
                model_provider: Some(item.model_provider),
                cli_version: Some(item.cli_version),
                created_at: Some(item.created_at.to_rfc3339_opts(SecondsFormat::Secs, true)),
                updated_at: Some(item.updated_at.to_rfc3339_opts(SecondsFormat::Secs, true)),
            })
            .collect();
        Self {
            items,
            next_cursor: db_page.next_anchor.map(Into::into),
            num_scanned_files: db_page.num_scanned_rows,
            reached_scan_cap: false,
        }
    }
}

fn select_resume_path(page: &ThreadsPage, filter_cwd: Option<&Path>) -> Option<PathBuf> {
    match filter_cwd {
        Some(cwd) => page.items.iter().find_map(|item| {
            if item
                .cwd
                .as_ref()
                .is_some_and(|session_cwd| cwd_matches(session_cwd, cwd))
            {
                Some(item.path.clone())
            } else {
                None
            }
        }),
        None => page.items.first().map(|item| item.path.clone()),
    }
}

fn select_resume_path_from_db_page(
    page: &codex_state::ThreadsPage,
    filter_cwd: Option<&Path>,
) -> Option<PathBuf> {
    match filter_cwd {
        Some(cwd) => page.items.iter().find_map(|item| {
            if cwd_matches(item.cwd.as_path(), cwd) {
                Some(item.rollout_path.clone())
            } else {
                None
            }
        }),
        None => page.items.first().map(|item| item.rollout_path.clone()),
    }
}

fn cwd_matches(session_cwd: &Path, cwd: &Path) -> bool {
    if let (Ok(ca), Ok(cb)) = (
        path_utils::normalize_for_path_comparison(session_cwd),
        path_utils::normalize_for_path_comparison(cwd),
    ) {
        return ca == cb;
    }
    session_cwd == cwd
}
