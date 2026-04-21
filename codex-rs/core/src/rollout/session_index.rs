use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;

use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::USER_MESSAGE_BEGIN;
use serde::Deserialize;
use serde::Serialize;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;

const SESSION_INDEX_FILE: &str = "session_index.jsonl";
const READ_CHUNK_SIZE: usize = 8192;
const IMAGE_ONLY_USER_MESSAGE_PLACEHOLDER: &str = "[Image]";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionIndexEntry {
    pub id: ThreadId,
    pub thread_name: String,
    pub updated_at: String,
}

/// Append a thread name update to the session index.
/// The index is append-only; the most recent entry wins when resolving names or ids.
pub async fn append_thread_name(
    codex_home: &Path,
    thread_id: ThreadId,
    name: &str,
) -> std::io::Result<()> {
    use time::OffsetDateTime;
    use time::format_description::well_known::Rfc3339;

    let updated_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string());
    let entry = SessionIndexEntry {
        id: thread_id,
        thread_name: name.to_string(),
        updated_at,
    };
    append_session_index_entry(codex_home, &entry).await
}

/// Append a raw session index entry to `session_index.jsonl`.
/// The file is append-only; consumers scan from the end to find the newest match.
pub async fn append_session_index_entry(
    codex_home: &Path,
    entry: &SessionIndexEntry,
) -> std::io::Result<()> {
    let path = session_index_path(codex_home);
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;
    let mut line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
    line.push('\n');
    file.write_all(line.as_bytes()).await?;
    file.flush().await?;
    Ok(())
}

/// Find the latest thread name for a thread id, if any.
pub async fn find_thread_name_by_id(
    codex_home: &Path,
    thread_id: &ThreadId,
) -> std::io::Result<Option<String>> {
    let path = session_index_path(codex_home);
    if !path.exists() {
        return Ok(None);
    }
    let id = *thread_id;
    let entry = tokio::task::spawn_blocking(move || scan_index_from_end_by_id(&path, &id))
        .await
        .map_err(std::io::Error::other)??;
    Ok(entry.map(|entry| entry.thread_name))
}

/// Find a display label for a thread id.
///
/// Uses the explicit thread name from session index when available, otherwise
/// falls back to the first user message from rollout history.
pub async fn find_thread_label_by_id(
    codex_home: &Path,
    thread_id: &ThreadId,
) -> std::io::Result<Option<String>> {
    if let Some(name) = find_thread_name_by_id(codex_home, thread_id).await? {
        return Ok(Some(name));
    }

    find_first_user_message_by_id(codex_home, thread_id).await
}

/// Find the latest thread names for a batch of thread ids.
pub async fn find_thread_names_by_ids(
    codex_home: &Path,
    thread_ids: &HashSet<ThreadId>,
) -> std::io::Result<HashMap<ThreadId, String>> {
    let path = session_index_path(codex_home);
    if thread_ids.is_empty() || !path.exists() {
        return Ok(HashMap::new());
    }

    let file = tokio::fs::File::open(&path).await?;
    let reader = tokio::io::BufReader::new(file);
    let mut lines = reader.lines();
    let mut names = HashMap::with_capacity(thread_ids.len());

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<SessionIndexEntry>(trimmed) else {
            continue;
        };
        let name = entry.thread_name.trim();
        if !name.is_empty() && thread_ids.contains(&entry.id) {
            names.insert(entry.id, name.to_string());
        }
    }

    Ok(names)
}

/// Find display labels for a batch of thread ids.
///
/// Prefers explicit thread names from session index, then falls back to first
/// user message previews from rollout history for ids without a saved name.
pub async fn find_thread_labels_by_ids(
    codex_home: &Path,
    thread_ids: &HashSet<ThreadId>,
) -> std::io::Result<HashMap<ThreadId, String>> {
    let mut labels = find_thread_names_by_ids(codex_home, thread_ids).await?;

    for thread_id in thread_ids {
        if labels.contains_key(thread_id) {
            continue;
        }
        if let Some(message) = find_first_user_message_by_id(codex_home, thread_id).await? {
            labels.insert(*thread_id, message);
        }
    }

    Ok(labels)
}

/// Find the most recently updated thread id for a thread name, if any.
pub async fn find_thread_id_by_name(
    codex_home: &Path,
    name: &str,
) -> std::io::Result<Option<ThreadId>> {
    if name.trim().is_empty() {
        return Ok(None);
    }
    let path = session_index_path(codex_home);
    if !path.exists() {
        return Ok(None);
    }
    let name = name.to_string();
    let entry = tokio::task::spawn_blocking(move || scan_index_from_end_by_name(&path, &name))
        .await
        .map_err(std::io::Error::other)??;
    Ok(entry.map(|entry| entry.id))
}

/// Locate a recorded thread rollout file by thread name using newest-first ordering.
/// Returns `Ok(Some(path))` if found, `Ok(None)` if not present.
pub async fn find_thread_path_by_name_str(
    codex_home: &Path,
    name: &str,
) -> std::io::Result<Option<PathBuf>> {
    let Some(thread_id) = find_thread_id_by_name(codex_home, name).await? else {
        return Ok(None);
    };
    super::list::find_thread_path_by_id_str(codex_home, &thread_id.to_string()).await
}

/// Compute the next fork number for a parent thread id.
///
/// Fork numbering is based on the number of recorded sessions (active + archived)
/// whose session metadata contains `forked_from_id == parent_id`.
pub async fn next_fork_number_for_parent(
    codex_home: &Path,
    parent_id: &ThreadId,
) -> std::io::Result<usize> {
    let codex_home = codex_home.to_path_buf();
    let parent_id = *parent_id;
    tokio::task::spawn_blocking(move || {
        let active = count_forks_in_root(codex_home.join(super::SESSIONS_SUBDIR), parent_id)?;
        let archived =
            count_forks_in_root(codex_home.join(super::ARCHIVED_SESSIONS_SUBDIR), parent_id)?;
        Ok(active.saturating_add(archived).saturating_add(1))
    })
    .await
    .map_err(std::io::Error::other)?
}

fn session_index_path(codex_home: &Path) -> PathBuf {
    codex_home.join(SESSION_INDEX_FILE)
}

fn count_forks_in_root(root: PathBuf, parent_id: ThreadId) -> std::io::Result<usize> {
    if !root.exists() {
        return Ok(0);
    }

    let mut stack = vec![root];
    let mut count = 0usize;
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let is_rollout = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"));
            if !is_rollout {
                continue;
            }

            if rollout_is_fork_of(path.as_path(), parent_id)? {
                count = count.saturating_add(1);
            }
        }
    }

    Ok(count)
}

fn rollout_is_fork_of(path: &Path, parent_id: ThreadId) -> std::io::Result<bool> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(rollout_line) =
            serde_json::from_str::<codex_protocol::protocol::RolloutLine>(trimmed)
        else {
            return Ok(false);
        };
        return Ok(matches!(
            rollout_line.item,
            RolloutItem::SessionMeta(meta_line) if meta_line.meta.forked_from_id == Some(parent_id)
        ));
    }

    Ok(false)
}

async fn find_rollout_path_by_id(
    codex_home: &Path,
    thread_id: &ThreadId,
) -> std::io::Result<Option<PathBuf>> {
    let id = thread_id.to_string();
    if let Some(path) = super::list::find_thread_path_by_id_str(codex_home, &id).await? {
        return Ok(Some(path));
    }
    super::list::find_archived_thread_path_by_id_str(codex_home, &id).await
}

fn strip_user_message_prefix(text: &str) -> &str {
    match text.find(USER_MESSAGE_BEGIN) {
        Some(idx) => text[idx + USER_MESSAGE_BEGIN.len()..].trim(),
        None => text.trim(),
    }
}

fn first_user_message_from_items(items: &[RolloutItem]) -> Option<String> {
    for item in items {
        if let RolloutItem::EventMsg(EventMsg::UserMessage(user)) = item {
            let message = strip_user_message_prefix(user.message.as_str());
            if !message.is_empty() {
                return Some(message.to_string());
            }

            if user
                .images
                .as_ref()
                .is_some_and(|images| !images.is_empty())
                || !user.local_images.is_empty()
            {
                return Some(IMAGE_ONLY_USER_MESSAGE_PLACEHOLDER.to_string());
            }
        }
    }

    None
}

async fn find_first_user_message_by_id(
    codex_home: &Path,
    thread_id: &ThreadId,
) -> std::io::Result<Option<String>> {
    let Some(path) = find_rollout_path_by_id(codex_home, thread_id).await? else {
        return Ok(None);
    };

    match super::recorder::RolloutRecorder::load_rollout_items(path.as_path()).await {
        Ok((items, _, _)) => Ok(first_user_message_from_items(items.as_slice())),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn scan_index_from_end_by_id(
    path: &Path,
    thread_id: &ThreadId,
) -> std::io::Result<Option<SessionIndexEntry>> {
    scan_index_from_end(path, |entry| entry.id == *thread_id)
}

fn scan_index_from_end_by_name(
    path: &Path,
    name: &str,
) -> std::io::Result<Option<SessionIndexEntry>> {
    scan_index_from_end(path, |entry| entry.thread_name == name)
}

fn scan_index_from_end<F>(
    path: &Path,
    mut predicate: F,
) -> std::io::Result<Option<SessionIndexEntry>>
where
    F: FnMut(&SessionIndexEntry) -> bool,
{
    let mut file = File::open(path)?;
    let mut remaining = file.metadata()?.len();
    let mut line_rev: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; READ_CHUNK_SIZE];

    while remaining > 0 {
        let read_size = usize::try_from(remaining.min(READ_CHUNK_SIZE as u64))
            .map_err(std::io::Error::other)?;
        remaining -= read_size as u64;
        file.seek(SeekFrom::Start(remaining))?;
        file.read_exact(&mut buf[..read_size])?;

        for &byte in buf[..read_size].iter().rev() {
            if byte == b'\n' {
                if let Some(entry) = parse_line_from_rev(&mut line_rev, &mut predicate)? {
                    return Ok(Some(entry));
                }
                continue;
            }
            line_rev.push(byte);
        }
    }

    if let Some(entry) = parse_line_from_rev(&mut line_rev, &mut predicate)? {
        return Ok(Some(entry));
    }

    Ok(None)
}

fn parse_line_from_rev<F>(
    line_rev: &mut Vec<u8>,
    predicate: &mut F,
) -> std::io::Result<Option<SessionIndexEntry>>
where
    F: FnMut(&SessionIndexEntry) -> bool,
{
    if line_rev.is_empty() {
        return Ok(None);
    }
    line_rev.reverse();
    let line = std::mem::take(line_rev);
    let Ok(mut line) = String::from_utf8(line) else {
        return Ok(None);
    };
    if line.ends_with('\r') {
        line.pop();
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let Ok(entry) = serde_json::from_str::<SessionIndexEntry>(trimmed) else {
        return Ok(None);
    };
    if predicate(&entry) {
        return Ok(Some(entry));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::protocol::RolloutLine;
    use codex_protocol::protocol::SessionMeta;
    use codex_protocol::protocol::SessionMetaLine;
    use codex_protocol::protocol::SessionSource;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use std::collections::HashSet;
    use tempfile::TempDir;
    fn write_index(path: &Path, lines: &[SessionIndexEntry]) -> std::io::Result<()> {
        let mut out = String::new();
        for entry in lines {
            out.push_str(&serde_json::to_string(entry).unwrap());
            out.push('\n');
        }
        std::fs::write(path, out)
    }

    #[test]
    fn find_thread_id_by_name_prefers_latest_entry() -> std::io::Result<()> {
        let temp = TempDir::new()?;
        let path = session_index_path(temp.path());
        let id1 = ThreadId::new();
        let id2 = ThreadId::new();
        let lines = vec![
            SessionIndexEntry {
                id: id1,
                thread_name: "same".to_string(),
                updated_at: "2024-01-01T00:00:00Z".to_string(),
            },
            SessionIndexEntry {
                id: id2,
                thread_name: "same".to_string(),
                updated_at: "2024-01-02T00:00:00Z".to_string(),
            },
        ];
        write_index(&path, &lines)?;

        let found = scan_index_from_end_by_name(&path, "same")?;
        assert_eq!(found.map(|entry| entry.id), Some(id2));
        Ok(())
    }

    #[test]
    fn find_thread_name_by_id_prefers_latest_entry() -> std::io::Result<()> {
        let temp = TempDir::new()?;
        let path = session_index_path(temp.path());
        let id = ThreadId::new();
        let lines = vec![
            SessionIndexEntry {
                id,
                thread_name: "first".to_string(),
                updated_at: "2024-01-01T00:00:00Z".to_string(),
            },
            SessionIndexEntry {
                id,
                thread_name: "second".to_string(),
                updated_at: "2024-01-02T00:00:00Z".to_string(),
            },
        ];
        write_index(&path, &lines)?;

        let found = scan_index_from_end_by_id(&path, &id)?;
        assert_eq!(
            found.map(|entry| entry.thread_name),
            Some("second".to_string())
        );
        Ok(())
    }

    #[test]
    fn scan_index_returns_none_when_entry_missing() -> std::io::Result<()> {
        let temp = TempDir::new()?;
        let path = session_index_path(temp.path());
        let id = ThreadId::new();
        let lines = vec![SessionIndexEntry {
            id,
            thread_name: "present".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        }];
        write_index(&path, &lines)?;

        let missing_name = scan_index_from_end_by_name(&path, "missing")?;
        assert_eq!(missing_name, None);

        let missing_id = scan_index_from_end_by_id(&path, &ThreadId::new())?;
        assert_eq!(missing_id, None);
        Ok(())
    }

    #[tokio::test]
    async fn find_thread_names_by_ids_prefers_latest_entry() -> std::io::Result<()> {
        let temp = TempDir::new()?;
        let path = session_index_path(temp.path());
        let id1 = ThreadId::new();
        let id2 = ThreadId::new();
        let lines = vec![
            SessionIndexEntry {
                id: id1,
                thread_name: "first".to_string(),
                updated_at: "2024-01-01T00:00:00Z".to_string(),
            },
            SessionIndexEntry {
                id: id2,
                thread_name: "other".to_string(),
                updated_at: "2024-01-01T00:00:00Z".to_string(),
            },
            SessionIndexEntry {
                id: id1,
                thread_name: "latest".to_string(),
                updated_at: "2024-01-02T00:00:00Z".to_string(),
            },
        ];
        write_index(&path, &lines)?;

        let mut ids = HashSet::new();
        ids.insert(id1);
        ids.insert(id2);

        let mut expected = HashMap::new();
        expected.insert(id1, "latest".to_string());
        expected.insert(id2, "other".to_string());

        let found = find_thread_names_by_ids(temp.path(), &ids).await?;
        assert_eq!(found, expected);
        Ok(())
    }

    #[test]
    fn scan_index_finds_latest_match_among_mixed_entries() -> std::io::Result<()> {
        let temp = TempDir::new()?;
        let path = session_index_path(temp.path());
        let id_target = ThreadId::new();
        let id_other = ThreadId::new();
        let expected = SessionIndexEntry {
            id: id_target,
            thread_name: "target".to_string(),
            updated_at: "2024-01-03T00:00:00Z".to_string(),
        };
        let expected_other = SessionIndexEntry {
            id: id_other,
            thread_name: "target".to_string(),
            updated_at: "2024-01-02T00:00:00Z".to_string(),
        };
        // Resolution is based on append order (scan from end), not updated_at.
        let lines = vec![
            SessionIndexEntry {
                id: id_target,
                thread_name: "target".to_string(),
                updated_at: "2024-01-01T00:00:00Z".to_string(),
            },
            expected_other.clone(),
            expected.clone(),
            SessionIndexEntry {
                id: ThreadId::new(),
                thread_name: "another".to_string(),
                updated_at: "2024-01-04T00:00:00Z".to_string(),
            },
        ];
        write_index(&path, &lines)?;

        let found_by_name = scan_index_from_end_by_name(&path, "target")?;
        assert_eq!(found_by_name, Some(expected.clone()));

        let found_by_id = scan_index_from_end_by_id(&path, &id_target)?;
        assert_eq!(found_by_id, Some(expected));

        let found_other_by_id = scan_index_from_end_by_id(&path, &id_other)?;
        assert_eq!(found_other_by_id, Some(expected_other));
        Ok(())
    }

    #[test]
    fn first_user_message_from_items_uses_trimmed_message_or_image_placeholder() {
        let user = RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                message: "help me debug this test".to_string(),
                images: None,
                text_elements: Vec::new(),
                local_images: Vec::new(),
            },
        ));
        assert_eq!(
            first_user_message_from_items(&[user]),
            Some("help me debug this test".to_string())
        );

        let image_only = RolloutItem::EventMsg(EventMsg::UserMessage(
            codex_protocol::protocol::UserMessageEvent {
                message: String::new(),
                images: Some(vec!["https://example.com/image.png".to_string()]),
                text_elements: Vec::new(),
                local_images: Vec::new(),
            },
        ));
        assert_eq!(
            first_user_message_from_items(&[image_only]),
            Some(IMAGE_ONLY_USER_MESSAGE_PLACEHOLDER.to_string())
        );
    }

    fn write_rollout_with_parent(
        codex_home: &Path,
        subdir: &str,
        thread_id: ThreadId,
        forked_from_id: Option<ThreadId>,
    ) -> std::io::Result<()> {
        let root = codex_home.join(subdir);
        std::fs::create_dir_all(&root)?;
        let path = root.join(format!("rollout-2025-01-01T00-00-00-{thread_id}.jsonl"));
        let rollout_line = RolloutLine {
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            item: RolloutItem::SessionMeta(SessionMetaLine {
                meta: SessionMeta {
                    id: thread_id,
                    forked_from_id,
                    timestamp: "2025-01-01T00:00:00Z".to_string(),
                    cwd: codex_home.to_path_buf(),
                    originator: "test".to_string(),
                    cli_version: "test".to_string(),
                    source: SessionSource::Cli,
                    model_provider: None,
                    base_instructions: None,
                    dynamic_tools: None,
                },
                git: None,
            }),
        };
        std::fs::write(path, format!("{}\n", serde_json::to_string(&rollout_line)?))
    }

    #[tokio::test]
    async fn next_fork_number_for_parent_counts_active_and_archived() -> std::io::Result<()> {
        let temp = TempDir::new()?;
        let parent_id = ThreadId::new();
        write_rollout_with_parent(
            temp.path(),
            super::super::SESSIONS_SUBDIR,
            ThreadId::new(),
            Some(parent_id),
        )?;
        write_rollout_with_parent(
            temp.path(),
            super::super::SESSIONS_SUBDIR,
            ThreadId::new(),
            Some(parent_id),
        )?;
        write_rollout_with_parent(
            temp.path(),
            super::super::ARCHIVED_SESSIONS_SUBDIR,
            ThreadId::new(),
            Some(parent_id),
        )?;
        write_rollout_with_parent(
            temp.path(),
            super::super::SESSIONS_SUBDIR,
            ThreadId::new(),
            Some(ThreadId::new()),
        )?;

        let next = next_fork_number_for_parent(temp.path(), &parent_id).await?;
        assert_eq!(next, 4);
        Ok(())
    }
}
