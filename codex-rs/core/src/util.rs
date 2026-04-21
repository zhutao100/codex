use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use codex_protocol::ThreadId;
use rand::Rng;
use tracing::debug;
use tracing::error;

use crate::parse_command::shlex_join;

const INITIAL_DELAY_MS: u64 = 200;
const BACKOFF_FACTOR: f64 = 2.0;

pub const MAX_THREAD_NAME_CHARS: usize = 80;

/// Emit structured feedback metadata as key/value pairs.
///
/// This logs a tracing event with `target: "feedback_tags"`. If
/// `codex_feedback::CodexFeedback::metadata_layer()` is installed, these fields are captured and
/// later attached as tags when feedback is uploaded.
///
/// Values are wrapped with [`tracing::field::DebugValue`], so the expression only needs to
/// implement [`std::fmt::Debug`].
///
/// Example:
///
/// ```rust
/// codex_core::feedback_tags!(model = "gpt-5", cached = true);
/// codex_core::feedback_tags!(provider = provider_id, request_id = request_id);
/// ```
#[macro_export]
macro_rules! feedback_tags {
    ($( $key:ident = $value:expr ),+ $(,)?) => {
        ::tracing::info!(
            target: "feedback_tags",
            $( $key = ::tracing::field::debug(&$value) ),+
        );
    };
}

pub fn backoff(attempt: u64) -> Duration {
    let exp = BACKOFF_FACTOR.powi(attempt.saturating_sub(1) as i32);
    let base = (INITIAL_DELAY_MS as f64 * exp) as u64;
    let jitter = rand::rng().random_range(0.9..1.1);
    Duration::from_millis((base as f64 * jitter) as u64)
}

pub(crate) fn error_or_panic(message: impl std::string::ToString) {
    if cfg!(debug_assertions) {
        panic!("{}", message.to_string());
    } else {
        error!("{}", message.to_string());
    }
}

pub(crate) fn try_parse_error_message(text: &str) -> String {
    debug!("Parsing server error response: {}", text);
    let json = serde_json::from_str::<serde_json::Value>(text).unwrap_or_default();
    if let Some(error) = json.get("error")
        && let Some(message) = error.get("message")
        && let Some(message_str) = message.as_str()
    {
        return message_str.to_string();
    }
    if text.is_empty() {
        return "Unknown error".to_string();
    }
    text.to_string()
}

pub fn resolve_path(base: &Path, path: &PathBuf) -> PathBuf {
    if path.is_absolute() {
        path.clone()
    } else {
        base.join(path)
    }
}

/// Normalize a thread name to a single line and clamp its length.
///
/// Returns `None` when the name is empty after trimming/collapsing whitespace.
pub fn normalize_thread_name(name: &str) -> Option<String> {
    let collapsed = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let collapsed = collapsed.trim();
    if collapsed.is_empty() {
        None
    } else {
        let mut normalized = collapsed.to_string();
        if normalized.chars().count() > MAX_THREAD_NAME_CHARS {
            normalized = normalized.chars().take(MAX_THREAD_NAME_CHARS).collect();
            normalized = normalized.trim().to_string();
        }
        (!normalized.is_empty()).then_some(normalized)
    }
}

fn strip_fork_prefix_once(name: &str) -> Option<&str> {
    let name = name.trim();
    if let Some(rest) = name.strip_prefix("Fork#") {
        let bytes = rest.as_bytes();
        let mut i = 0;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == 0 {
            return None;
        }
        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j == i {
            return None;
        }
        return Some(rest[j..].trim());
    }

    let Some(rest) = name.strip_prefix("Fork") else {
        return None;
    };
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let start_digits = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start_digits {
        return None;
    }
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let rest = rest[i..].trim_start();
    let Some(rest) = rest.strip_prefix("of") else {
        return None;
    };
    let rest = rest.trim_start();
    (!rest.is_empty()).then_some(rest.trim())
}

pub fn strip_fork_prefixes(name: &str) -> &str {
    let mut current = name.trim();
    while let Some(next) = strip_fork_prefix_once(current) {
        if next == current {
            break;
        }
        current = next;
    }
    current
}

pub fn format_fork_thread_name(fork_number: usize, parent_name: &str) -> Option<String> {
    let base = strip_fork_prefixes(parent_name);
    if base.is_empty() {
        return None;
    }

    normalize_thread_name(&format!("Fork#{fork_number} {base}"))
}

fn resume_command_for_target(target: String) -> String {
    let needs_double_dash = target.starts_with('-');
    let escaped = shlex_join(&[target]);
    if needs_double_dash {
        format!("codex resume -- {escaped}")
    } else {
        format!("codex resume {escaped}")
    }
}

pub fn resume_commands(thread_name: Option<&str>, thread_id: Option<ThreadId>) -> Vec<String> {
    let mut commands = Vec::new();
    if let Some(name) = thread_name.filter(|name| !name.is_empty()) {
        commands.push(resume_command_for_target(name.to_string()));
    }

    if let Some(thread_id) = thread_id {
        let id_command = resume_command_for_target(thread_id.to_string());
        if !commands.contains(&id_command) {
            commands.push(id_command);
        }
    }

    commands
}

pub fn resume_command(thread_name: Option<&str>, thread_id: Option<ThreadId>) -> Option<String> {
    resume_commands(thread_name, thread_id).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_try_parse_error_message() {
        let text = r#"{
  "error": {
    "message": "Your refresh token has already been used to generate a new access token. Please try signing in again.",
    "type": "invalid_request_error",
    "param": null,
    "code": "refresh_token_reused"
  }
}"#;
        let message = try_parse_error_message(text);
        assert_eq!(
            message,
            "Your refresh token has already been used to generate a new access token. Please try signing in again."
        );
    }

    #[test]
    fn test_try_parse_error_message_no_error() {
        let text = r#"{"message": "test"}"#;
        let message = try_parse_error_message(text);
        assert_eq!(message, r#"{"message": "test"}"#);
    }

    #[test]
    fn feedback_tags_macro_compiles() {
        #[derive(Debug)]
        struct OnlyDebug;

        feedback_tags!(model = "gpt-5", cached = true, debug_only = OnlyDebug);
    }

    #[test]
    fn normalize_thread_name_rejects_empty() {
        assert_eq!(normalize_thread_name("   "), None);
    }

    #[test]
    fn normalize_thread_name_trims_collapses_whitespace_and_clamps() {
        assert_eq!(
            normalize_thread_name("  my thread  "),
            Some("my thread".to_string())
        );
        assert_eq!(
            normalize_thread_name("my\nthread\tname"),
            Some("my thread name".to_string())
        );

        let long = "a".repeat(MAX_THREAD_NAME_CHARS + 10);
        let normalized = normalize_thread_name(&long).expect("normalized");
        assert_eq!(normalized.chars().count(), MAX_THREAD_NAME_CHARS);
    }

    #[test]
    fn strip_fork_prefixes_removes_old_and_new_formats() {
        assert_eq!(
            strip_fork_prefixes("Fork 2 of Parent thread title"),
            "Parent thread title"
        );
        assert_eq!(
            strip_fork_prefixes("Fork#3 Parent thread title"),
            "Parent thread title"
        );
        assert_eq!(
            strip_fork_prefixes("Fork#3 Fork 2 of Parent thread title"),
            "Parent thread title"
        );
    }

    #[test]
    fn format_fork_thread_name_de_nests_and_normalizes() {
        assert_eq!(
            format_fork_thread_name(4, "Fork#1   Parent\nthread\ttitle"),
            Some("Fork#4 Parent thread title".to_string())
        );
    }

    #[test]
    fn resume_command_prefers_name_over_id() {
        let thread_id = ThreadId::from_string("123e4567-e89b-12d3-a456-426614174000").unwrap();
        let command = resume_command(Some("my-thread"), Some(thread_id));
        assert_eq!(command, Some("codex resume my-thread".to_string()));
    }

    #[test]
    fn resume_command_with_only_id() {
        let thread_id = ThreadId::from_string("123e4567-e89b-12d3-a456-426614174000").unwrap();
        let command = resume_command(None, Some(thread_id));
        assert_eq!(
            command,
            Some("codex resume 123e4567-e89b-12d3-a456-426614174000".to_string())
        );
    }

    #[test]
    fn resume_command_with_no_name_or_id() {
        let command = resume_command(None, None);
        assert_eq!(command, None);
    }

    #[test]
    fn resume_command_quotes_thread_name_when_needed() {
        let command = resume_command(Some("-starts-with-dash"), None);
        assert_eq!(
            command,
            Some("codex resume -- -starts-with-dash".to_string())
        );

        let command = resume_command(Some("two words"), None);
        assert_eq!(command, Some("codex resume 'two words'".to_string()));

        let command = resume_command(Some("quote'case"), None);
        assert_eq!(command, Some("codex resume \"quote'case\"".to_string()));
    }

    #[test]
    fn resume_commands_include_name_and_id() {
        let thread_id = ThreadId::from_string("123e4567-e89b-12d3-a456-426614174000").unwrap();
        let commands = resume_commands(Some("my-thread"), Some(thread_id));
        assert_eq!(
            commands,
            vec![
                "codex resume my-thread".to_string(),
                "codex resume 123e4567-e89b-12d3-a456-426614174000".to_string()
            ]
        );
    }
}
