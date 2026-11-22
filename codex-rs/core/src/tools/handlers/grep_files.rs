use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine;
use codex_protocol::models::FunctionCallOutputBody;
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::exec::SandboxType;
use crate::function_tool::FunctionCallError;
use crate::sandboxing::CommandSpec;
use crate::sandboxing::SandboxManager;
use crate::sandboxing::SandboxTransformRequest;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use crate::tools::sandboxing::SandboxablePreference;

pub struct GrepFilesHandler;

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 2000;
const MAX_RESULT_BYTES: usize = 24 * 1024;
const COVERAGE_BYTES: usize = 256;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

fn default_limit() -> usize {
    DEFAULT_LIMIT
}

#[derive(Deserialize)]
struct GrepFilesArgs {
    pattern: String,
    #[serde(default)]
    include: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}

impl ToolHandler for GrepFilesHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolInvocation { payload, turn, .. } = invocation;
        if !turn.sandbox_policy.has_full_disk_read_access() {
            return Err(FunctionCallError::RespondToModel(
                "grep_files requires full read access; use a sandboxed tool instead".to_string(),
            ));
        }

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "grep_files handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: GrepFilesArgs = parse_arguments(&arguments)?;

        let pattern = args.pattern.trim();
        if pattern.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "pattern must not be empty".to_string(),
            ));
        }

        if args.limit == 0 {
            return Err(FunctionCallError::RespondToModel(
                "limit must be greater than zero".to_string(),
            ));
        }

        let limit = args.limit.min(MAX_LIMIT);
        let search_path = turn.resolve_path(args.path.clone());

        verify_path_exists(&search_path).await?;

        let include = args.include.as_deref().map(str::trim).and_then(|val| {
            if val.is_empty() {
                None
            } else {
                Some(val.to_string())
            }
        });

        let mut budget = turn.truncation_policy.byte_budget();
        if turn.config.bounded_explorer {
            budget = budget.min(crate::agent::role::EXPLORER_RESULT_BYTES);
        }
        let byte_limit = budget.saturating_sub(COVERAGE_BYTES).min(MAX_RESULT_BYTES);
        let results = run_rg_search(
            pattern,
            include.as_deref(),
            &search_path,
            limit,
            byte_limit,
            search_command(&turn)?,
        )
        .await?;
        let header = results.coverage();
        let success = results.stop == SearchStop::Complete && header.len() <= budget;
        if turn.config.bounded_explorer {
            turn.exploration.observe(!success, None);
        }
        let text = crate::truncate::fit_result(&header, &results.paths.join("\n"), budget);
        Ok(ToolOutput::Function {
            body: FunctionCallOutputBody::Text(text),
            success: Some(success),
        })
    }
}

async fn verify_path_exists(path: &Path) -> Result<(), FunctionCallError> {
    tokio::fs::metadata(path).await.map_err(|err| {
        FunctionCallError::RespondToModel(format!("unable to access `{}`: {err}", path.display()))
    })?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum SearchStop {
    Complete,
    PathLimit,
    ByteLimit,
    Timeout,
    Error,
}

#[derive(Debug, PartialEq, Eq)]
struct SearchResults {
    paths: Vec<String>,
    stop: SearchStop,
}

impl SearchResults {
    fn coverage(&self) -> String {
        let count = self.paths.len();
        let coverage = match self.stop {
            SearchStop::Complete => format!("Search complete within rg's scope: {count} matches."),
            SearchStop::PathLimit => format!(
                "Search partial (path limit): at least {} matches; narrow the scope.",
                count + 1
            ),
            SearchStop::ByteLimit => format!(
                "Search partial (byte limit): {count} paths returned, remainder unknown; narrow the scope."
            ),
            SearchStop::Timeout => format!(
                "Search partial (timeout): {count} paths returned, remainder unknown; narrow the scope."
            ),
            SearchStop::Error => {
                format!("Search partial (rg error): {count} paths returned, remainder unknown.")
            }
        };
        format!("{coverage} Working-tree snapshot not established.")
    }
}

/// Streaming requires a pipe, not the ordinary shell tool's capped text result.
/// Use the same platform transform, but never retry without containment.
fn search_command(turn: &TurnContext) -> Result<Command, FunctionCallError> {
    let manager = SandboxManager::new();
    let sandbox = manager.select_initial(
        &turn.sandbox_policy,
        SandboxablePreference::Auto,
        turn.windows_sandbox_level,
    );
    if sandbox == SandboxType::WindowsRestrictedToken
        || (sandbox == SandboxType::None
            && !matches!(
                turn.sandbox_policy,
                crate::protocol::SandboxPolicy::DangerFullAccess
                    | crate::protocol::SandboxPolicy::ExternalSandbox { .. }
            ))
    {
        return Err(FunctionCallError::RespondToModel(
            "grep_files cannot stream under this platform's sandbox".into(),
        ));
    }
    let env = manager
        .transform(SandboxTransformRequest {
            spec: CommandSpec {
                program: "rg".into(),
                args: Vec::new(),
                cwd: turn.cwd.clone(),
                env: crate::exec_env::create_env(&turn.shell_environment_policy, None),
                expiration: crate::exec::ExecExpiration::Timeout(COMMAND_TIMEOUT),
                sandbox_permissions: Default::default(),
                justification: None,
            },
            policy: &turn.sandbox_policy,
            sandbox,
            sandbox_policy_cwd: &turn.cwd,
            codex_linux_sandbox_exe: turn.codex_linux_sandbox_exe.as_ref(),
            windows_sandbox_level: turn.windows_sandbox_level,
        })
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("cannot confine grep_files: {err}"))
        })?;
    let (program, args) = env.command.split_first().ok_or_else(|| {
        FunctionCallError::Fatal("sandbox transform returned an empty grep command".into())
    })?;
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(env.env)
        .current_dir(env.cwd);
    #[cfg(unix)]
    if let Some(arg0) = env.arg0 {
        command.arg0(arg0);
    }
    Ok(command)
}

async fn run_rg_search(
    pattern: &str,
    include: Option<&str>,
    search_path: &Path,
    limit: usize,
    byte_limit: usize,
    mut command: Command,
) -> Result<SearchResults, FunctionCallError> {
    command
        .arg("--files-with-matches")
        .arg("--null")
        .arg("--no-config")
        .arg("--regexp")
        .arg(pattern)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    if let Some(glob) = include {
        command.arg("--glob").arg(glob);
    }

    command.arg("--").arg(search_path);

    let mut child = command.spawn().map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to launch rg: {err}. Ensure ripgrep is installed and on PATH."
        ))
    })?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| FunctionCallError::Fatal("rg was launched without a stdout pipe".into()))?;
    let mut paths = Vec::new();
    let mut used_bytes = 0;
    let mut path = Vec::new();
    let stop = timeout(COMMAND_TIMEOUT, async {
        let mut chunk = [0; 4096];
        loop {
            let size = match stdout.read(&mut chunk).await {
                Ok(0) => break,
                Ok(size) => size,
                Err(_) => return SearchStop::Error,
            };
            for part in chunk[..size].split_inclusive(|&byte| byte == 0) {
                let terminated = part.last() == Some(&0);
                let raw = &part[..part.len() - usize::from(terminated)];
                if path.len() + raw.len() > byte_limit {
                    return SearchStop::ByteLimit;
                }
                path.extend_from_slice(raw);
                if terminated {
                    if path.is_empty() {
                        return SearchStop::Error;
                    }
                    if paths.len() == limit {
                        return SearchStop::PathLimit;
                    }
                    let rendered = render_path(&path);
                    used_bytes += rendered.len() + 1;
                    if used_bytes > byte_limit {
                        return SearchStop::ByteLimit;
                    }
                    paths.push(rendered);
                    path.clear();
                }
            }
        }
        match child.wait().await {
            Ok(status) if matches!(status.code(), Some(0 | 1)) && path.is_empty() => {
                SearchStop::Complete
            }
            _ => SearchStop::Error,
        }
    })
    .await
    .unwrap_or(SearchStop::Timeout);
    // kill_on_drop also covers cancellation and result/byte cutoffs.
    Ok(SearchResults { paths, stop })
}

fn render_path(path: &[u8]) -> String {
    match std::str::from_utf8(path) {
        Ok(path) => serde_json::Value::String(path.to_owned()).to_string(),
        Err(_) => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(path);
            serde_json::json!({ "pathBytesBase64": encoded }).to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::process::Command as StdCommand;
    use tempfile::tempdir;

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "run in a disposable macOS guest without an enclosing Seatbelt profile"]
    async fn explorer_search_confines_the_child_process() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let (_session, mut turn) = crate::session::tests::make_session_and_context().await;
        let dir = tempdir()?;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
        let probe = dir.path().join("rg");
        std::fs::write(dir.path().join("source"), "readable")?;
        std::fs::write(
            &probe,
            format!(
                r#"#!/bin/sh
test "$(cat source)" = readable || exit 2
if touch result 2>/dev/null; then w=yes; else w=no; fi
if nc -z -G 1 127.0.0.1 {} 2>/dev/null; then n=yes; else n=no; fi
printf 'read=yes write=%s network=%s\0' "$w" "$n"
"#,
                listener.local_addr()?.port()
            ),
        )?;
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o700))?;
        turn.cwd = dir.path().to_path_buf();
        turn.shell_environment_policy.r#set.insert(
            "PATH".into(),
            format!("{}:/usr/bin:/bin", dir.path().display()),
        );
        turn.sandbox_policy = crate::protocol::SandboxPolicy::DangerFullAccess;
        let run = |turn: &TurnContext| {
            let command = search_command(turn);
            async { run_rg_search(".", None, Path::new("."), 5, MAX_RESULT_BYTES, command?).await }
        };
        assert_eq!(
            run(&turn).await?.paths,
            [r#""read=yes write=yes network=yes""#]
        );
        std::fs::remove_file(dir.path().join("result"))?;
        let config = std::sync::Arc::make_mut(&mut turn.config);
        crate::agent::AgentRole::Explorer
            .apply_to_config(config)
            .map_err(anyhow::Error::msg)?;
        turn.sandbox_policy = config.sandbox_policy.get().clone();
        assert_eq!(
            run(&turn).await?,
            SearchResults {
                paths: vec![r#""read=yes write=no network=no""#.into()],
                stop: SearchStop::Complete
            }
        );
        assert!(!dir.path().join("result").exists());
        Ok(())
    }

    #[test]
    fn encodes_non_utf8_paths_losslessly() {
        assert_eq!(render_path(b"bad\xff"), r#"{"pathBytesBase64":"YmFk/w=="}"#);
    }

    #[tokio::test]
    async fn run_search_returns_results() -> anyhow::Result<()> {
        if !rg_available() {
            return Ok(());
        }
        let temp = tempdir().expect("create temp dir");
        let dir = temp.path();
        std::fs::write(dir.join("match_one.txt"), "alpha beta gamma").unwrap();
        let unusual = dir.join("match\nsecond.txt");
        std::fs::write(&unusual, "alpha delta").unwrap();
        std::fs::write(dir.join("other.txt"), "omega").unwrap();

        let mut results =
            run_rg_search("alpha", None, dir, 10, MAX_RESULT_BYTES, Command::new("rg")).await?;
        results.paths.sort();
        assert_eq!(
            results,
            SearchResults {
                paths: vec![
                    render_path(unusual.to_str().unwrap().as_bytes()),
                    render_path(dir.join("match_one.txt").to_str().unwrap().as_bytes()),
                ],
                stop: SearchStop::Complete
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_search_with_glob_filter() -> anyhow::Result<()> {
        if !rg_available() {
            return Ok(());
        }
        let temp = tempdir().expect("create temp dir");
        let dir = temp.path();
        std::fs::write(dir.join("match_one.rs"), "alpha beta gamma").unwrap();
        std::fs::write(dir.join("match_two.txt"), "alpha delta").unwrap();

        let results = run_rg_search(
            "alpha",
            Some("*.rs"),
            dir,
            10,
            MAX_RESULT_BYTES,
            Command::new("rg"),
        )
        .await?;
        assert_eq!(
            results,
            SearchResults {
                paths: vec![render_path(
                    dir.join("match_one.rs").to_str().unwrap().as_bytes()
                )],
                stop: SearchStop::Complete
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_search_respects_limit() -> anyhow::Result<()> {
        if !rg_available() {
            return Ok(());
        }
        let temp = tempdir().expect("create temp dir");
        let dir = temp.path();
        std::fs::write(dir.join("one.txt"), "alpha one").unwrap();
        std::fs::write(dir.join("two.txt"), "alpha two").unwrap();
        std::fs::write(dir.join("three.txt"), "alpha three").unwrap();

        let results =
            run_rg_search("alpha", None, dir, 2, MAX_RESULT_BYTES, Command::new("rg")).await?;
        assert_eq!(
            (results.paths.len(), results.stop),
            (2, SearchStop::PathLimit)
        );
        let results = run_rg_search("alpha", None, dir, 2, 1, Command::new("rg")).await?;
        assert_eq!(
            results,
            SearchResults {
                paths: vec![],
                stop: SearchStop::ByteLimit
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn run_search_handles_no_matches() -> anyhow::Result<()> {
        if !rg_available() {
            return Ok(());
        }
        let temp = tempdir().expect("create temp dir");
        let dir = temp.path();
        std::fs::write(dir.join("one.txt"), "omega").unwrap();

        let results =
            run_rg_search("alpha", None, dir, 5, MAX_RESULT_BYTES, Command::new("rg")).await?;
        assert_eq!(
            results,
            SearchResults {
                paths: vec![],
                stop: SearchStop::Complete
            }
        );
        Ok(())
    }

    fn rg_available() -> bool {
        StdCommand::new("rg")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }
}
