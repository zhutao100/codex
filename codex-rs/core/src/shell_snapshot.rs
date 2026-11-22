use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use crate::config::types::ShellEnvironmentPolicy;
use crate::rollout::list::find_thread_path_by_id_str;
use crate::shell::Shell;
use crate::shell::ShellType;
use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use codex_otel::OtelManager;
use codex_protocol::ThreadId;
use tokio::fs;
use tokio::process::Command;
use tokio::sync::watch;
use tokio::time::timeout;
use tracing::Instrument;
use tracing::info_span;

#[derive(Debug, PartialEq)]
pub struct ShellSnapshot {
    pub path: PathBuf,
    pub(crate) policy: ShellEnvironmentPolicy,
}

const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
const SNAPSHOT_RETENTION: Duration = Duration::from_secs(60 * 60 * 24 * 3); // 3 days retention.
const SNAPSHOT_DIR: &str = "shell_snapshots";

impl ShellSnapshot {
    pub fn start_snapshotting(
        codex_home: PathBuf,
        session_id: ThreadId,
        shell: &mut Shell,
        policy: ShellEnvironmentPolicy,
        otel_manager: OtelManager,
    ) {
        let (shell_snapshot_tx, shell_snapshot_rx) = watch::channel(None);
        shell.shell_snapshot = shell_snapshot_rx;

        let snapshot_shell = shell.clone();
        let snapshot_session_id = session_id;
        let snapshot_span = info_span!("shell_snapshot", thread_id = %snapshot_session_id);
        tokio::spawn(
            async move {
                let timer = otel_manager.start_timer("codex.shell_snapshot.duration_ms", &[]);
                let snapshot = ShellSnapshot::try_new(
                    &codex_home,
                    snapshot_session_id,
                    &snapshot_shell,
                    &policy,
                )
                .await
                .map(Arc::new);
                let success = if snapshot.is_some() { "true" } else { "false" };
                let _ = timer.map(|timer| timer.record(&[("success", success)]));
                otel_manager.counter("codex.shell_snapshot", 1, &[("success", success)]);
                let _ = shell_snapshot_tx.send(snapshot);
            }
            .instrument(snapshot_span),
        );
    }

    async fn try_new(
        codex_home: &Path,
        session_id: ThreadId,
        shell: &Shell,
        policy: &ShellEnvironmentPolicy,
    ) -> Option<Self> {
        let path = codex_home
            .join(SNAPSHOT_DIR)
            .join(format!("{session_id}.{}.sh", uuid::Uuid::new_v4()));

        // Clean the (unlikely) leaked snapshot files.
        let codex_home = codex_home.to_path_buf();
        let cleanup_session_id = session_id;
        tokio::spawn(async move {
            if let Err(err) = cleanup_stale_snapshots(&codex_home, cleanup_session_id).await {
                tracing::warn!("Failed to clean up shell snapshots: {err:?}");
            }
        });

        // Make the new snapshot.
        let snapshot = match write_shell_snapshot(shell, &path, policy).await {
            Ok(path) => {
                tracing::info!("Shell snapshot successfully created: {}", path.display());
                Some(Self {
                    path,
                    policy: policy.clone(),
                })
            }
            Err(err) => {
                tracing::warn!(
                    "Failed to create shell snapshot for {}: {err:?}",
                    shell.name()
                );
                None
            }
        };

        if let Some(snapshot) = snapshot.as_ref()
            && let Err(err) = validate_snapshot(shell, &snapshot.path, policy).await
        {
            tracing::error!("Shell snapshot validation failed: {err:?}");
            return None;
        }

        snapshot
    }
}

impl Drop for ShellSnapshot {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.path) {
            tracing::warn!(
                "Failed to delete shell snapshot at {:?}: {err:?}",
                self.path
            );
        }
    }
}

async fn write_shell_snapshot(
    shell: &Shell,
    output_path: &Path,
    policy: &ShellEnvironmentPolicy,
) -> Result<PathBuf> {
    let script = match shell.shell_type {
        ShellType::Bash => bash_snapshot_script(),
        ShellType::Zsh => zsh_snapshot_script(),
        ShellType::Sh => sh_snapshot_script(),
        ShellType::PowerShell | ShellType::Cmd => {
            bail!("Snapshot not supported for {:?}", shell.shell_type)
        }
    };
    let raw = run_script_with_timeout(shell, script, SNAPSHOT_TIMEOUT, true, policy).await?;
    let snapshot = render_snapshot(&raw, policy)?;

    if let Some(parent) = output_path.parent() {
        let parent_display = parent.display();
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Failed to create snapshot parent {parent_display}"))?;
    }

    let snapshot_path = output_path.display();
    // tempfile creates a private file. Publish only after the complete script has been written.
    let mut file =
        tempfile::NamedTempFile::new_in(output_path.parent().context("no snapshot directory")?)?;
    std::io::Write::write_all(&mut file, snapshot.as_bytes())?;
    file.persist(output_path)
        .with_context(|| format!("Failed to write snapshot to {snapshot_path}"))?;

    Ok(output_path.to_path_buf())
}

fn render_snapshot(raw: &str, policy: &ShellEnvironmentPolicy) -> Result<String> {
    use std::fmt::Write;
    let (script, exports) = raw
        .split_once('\0')
        .context("missing snapshot environment")?;
    let mut snapshot = strip_snapshot_preamble(script)?;
    let vars = exports
        .split_terminator('\0')
        .filter_map(|entry| entry.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()));
    let mut exports = crate::exec_env::populate_env(vars, policy, None).into_iter().filter(|(name, _)| {
        let mut bytes = name.bytes();
        bytes.next().is_some_and(|b| b == b'_' || b.is_ascii_alphabetic())
            && bytes.all(|b| b == b'_' || b.is_ascii_alphanumeric())
            && !matches!(name.as_str(), "PWD" | "OLDPWD" | crate::exec_env::CODEX_THREAD_ID_ENV_VAR)
            // Explicit overrides are supplied live for each command; do not bake them into a file.
            && !policy.r#set.contains_key(name)
            && !["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|word| name.to_ascii_uppercase().contains(word))
    }).collect::<Vec<_>>();
    exports.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    writeln!(snapshot, "\n# exports {}", exports.len())?;
    for (name, value) in exports {
        writeln!(snapshot, "export {name}={}", shlex::try_quote(&value)?)?;
    }
    Ok(snapshot)
}

fn strip_snapshot_preamble(snapshot: &str) -> Result<String> {
    let marker = "# Snapshot file";
    let Some(start) = snapshot.find(marker) else {
        bail!("Snapshot output missing marker {marker}");
    };

    Ok(snapshot[start..].to_string())
}

async fn validate_snapshot(
    shell: &Shell,
    snapshot_path: &Path,
    policy: &ShellEnvironmentPolicy,
) -> Result<()> {
    let snapshot_path =
        shlex::try_quote(snapshot_path.to_str().context("non-UTF-8 snapshot path")?)?;
    let script = format!("set -e; . {snapshot_path}");
    run_script_with_timeout(shell, &script, SNAPSHOT_TIMEOUT, false, policy)
        .await
        .map(|_| ())
}

async fn run_script_with_timeout(
    shell: &Shell,
    script: &str,
    snapshot_timeout: Duration,
    use_login_shell: bool,
    policy: &ShellEnvironmentPolicy,
) -> Result<String> {
    let args = shell.derive_exec_args(script, use_login_shell);
    let shell_name = shell.name();

    // Handler is kept as guard to control the drop. The `mut` pattern is required because .args()
    // returns a ref of handler.
    let mut handler = Command::new(&args[0]);
    handler
        .env_clear()
        .envs(crate::exec_env::create_env(policy, None));
    handler.args(&args[1..]);
    handler.stdin(Stdio::null());
    #[cfg(unix)]
    unsafe {
        handler.pre_exec(|| {
            codex_utils_pty::process_group::detach_from_tty()?;
            Ok(())
        });
    }
    handler.kill_on_drop(true);
    let output = timeout(snapshot_timeout, handler.output())
        .await
        .map_err(|_| anyhow!("Snapshot command timed out for {shell_name}"))?
        .with_context(|| format!("Failed to execute {shell_name}"))?;

    if !output.status.success() {
        let status = output.status;
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Snapshot command exited with status {status}: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn zsh_snapshot_script() -> &'static str {
    r##"if [[ -n "$ZDOTDIR" ]]; then
  rc="$ZDOTDIR/.zshrc"
else
  rc="$HOME/.zshrc"
fi
[[ -r "$rc" ]] && . "$rc"
print '# Snapshot file'
print '# Unset all aliases to avoid conflicts with functions'
print 'unalias -a 2>/dev/null || true'
print '# Functions'
functions
print ''
setopt_count=$(setopt | wc -l | tr -d ' ')
print "# setopts $setopt_count"
setopt | sed 's/^/setopt /'
print ''
alias_count=$(alias -L | wc -l | tr -d ' ')
print "# aliases $alias_count"
alias -L
print ''
printf '\0'
/usr/bin/env -0
"##
}

fn bash_snapshot_script() -> &'static str {
    r##"if [ -z "$BASH_ENV" ] && [ -r "$HOME/.bashrc" ]; then
  . "$HOME/.bashrc"
fi
echo '# Snapshot file'
echo '# Unset all aliases to avoid conflicts with functions'
unalias -a 2>/dev/null || true
echo '# Functions'
declare -f
echo ''
bash_opts=$(set -o | awk '$2=="on"{print $1}')
bash_opt_count=$(printf '%s\n' "$bash_opts" | sed '/^$/d' | wc -l | tr -d ' ')
echo "# setopts $bash_opt_count"
if [ -n "$bash_opts" ]; then
  printf 'set -o %s\n' $bash_opts
fi
echo ''
alias_count=$(alias -p | wc -l | tr -d ' ')
echo "# aliases $alias_count"
alias -p
echo ''
printf '\0'
/usr/bin/env -0
"##
}

fn sh_snapshot_script() -> &'static str {
    r##"if [ -n "$ENV" ] && [ -r "$ENV" ]; then
  . "$ENV"
fi
echo '# Snapshot file'
echo '# Unset all aliases to avoid conflicts with functions'
unalias -a 2>/dev/null || true
echo '# Functions'
if command -v typeset >/dev/null 2>&1; then
  typeset -f
elif command -v declare >/dev/null 2>&1; then
  declare -f
fi
echo ''
if set -o >/dev/null 2>&1; then
  sh_opts=$(set -o | awk '$2=="on"{print $1}')
  sh_opt_count=$(printf '%s\n' "$sh_opts" | sed '/^$/d' | wc -l | tr -d ' ')
  echo "# setopts $sh_opt_count"
  if [ -n "$sh_opts" ]; then
    printf 'set -o %s\n' $sh_opts
  fi
else
  echo '# setopts 0'
fi
echo ''
if alias >/dev/null 2>&1; then
  alias_count=$(alias | wc -l | tr -d ' ')
  echo "# aliases $alias_count"
  alias
  echo ''
else
  echo '# aliases 0'
fi
printf '\0'
/usr/bin/env -0
"##
}

/// Use rollout age for persistent sessions and file age for orphaned snapshots.
/// The active session is exempt even when a second process has resumed it.
pub async fn cleanup_stale_snapshots(codex_home: &Path, active_session_id: ThreadId) -> Result<()> {
    let snapshot_dir = codex_home.join(SNAPSHOT_DIR);

    let mut entries = match fs::read_dir(&snapshot_dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };

    let now = SystemTime::now();
    let active_session_id = active_session_id.to_string();

    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_file() {
            continue;
        }

        let path = entry.path();

        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name.starts_with('.') {
            continue;
        }
        let (session_id, _) = match file_name.split_once('.') {
            Some((stem, ext)) => (stem, ext),
            None => {
                remove_snapshot_file(&path).await;
                continue;
            }
        };
        if session_id == active_session_id {
            continue;
        }

        let rollout_path = find_thread_path_by_id_str(codex_home, session_id).await?;
        // A young snapshot can belong to an ephemeral session or to one whose first
        // rollout record is not yet visible. Its owner will unlink it on drop.
        let age_source = rollout_path.as_deref().unwrap_or(&path);
        let modified = match fs::metadata(age_source).await.and_then(|m| m.modified()) {
            Ok(modified) => modified,
            Err(err) => {
                tracing::warn!(
                    "Failed to check retention age for snapshot {}: {err:?}",
                    path.display()
                );
                continue;
            }
        };

        if now
            .duration_since(modified)
            .ok()
            .is_some_and(|age| age >= SNAPSHOT_RETENTION)
        {
            remove_snapshot_file(&path).await;
        }
    }

    Ok(())
}

async fn remove_snapshot_file(path: &Path) {
    if let Err(err) = fs::remove_file(path).await {
        tracing::warn!("Failed to delete shell snapshot at {:?}: {err:?}", path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    #[cfg(unix)]
    use std::process::Command;
    #[cfg(target_os = "linux")]
    use std::process::Command as StdCommand;

    use tempfile::tempdir;

    #[cfg(unix)]
    struct BlockingStdinPipe {
        original: i32,
        write_end: i32,
    }

    #[cfg(unix)]
    impl BlockingStdinPipe {
        fn install() -> Result<Self> {
            let mut fds = [0i32; 2];
            if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
                return Err(std::io::Error::last_os_error()).context("create stdin pipe");
            }

            let original = unsafe { libc::dup(libc::STDIN_FILENO) };
            if original == -1 {
                let err = std::io::Error::last_os_error();
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                return Err(err).context("dup stdin");
            }

            if unsafe { libc::dup2(fds[0], libc::STDIN_FILENO) } == -1 {
                let err = std::io::Error::last_os_error();
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                    libc::close(original);
                }
                return Err(err).context("replace stdin");
            }

            unsafe {
                libc::close(fds[0]);
            }

            Ok(Self {
                original,
                write_end: fds[1],
            })
        }
    }

    #[cfg(unix)]
    impl Drop for BlockingStdinPipe {
        fn drop(&mut self) {
            unsafe {
                libc::dup2(self.original, libc::STDIN_FILENO);
                libc::close(self.original);
                libc::close(self.write_end);
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn assert_posix_snapshot_sections(snapshot: &str) {
        assert!(snapshot.contains("# Snapshot file"));
        assert!(snapshot.contains("aliases "));
        assert!(snapshot.contains("exports "));
        assert!(
            snapshot.contains("PATH"),
            "snapshot should capture a PATH export"
        );
        assert!(snapshot.contains("setopts "));
    }

    async fn get_snapshot(shell_type: ShellType) -> Result<String> {
        let dir = tempdir()?;
        let path = dir.path().join("snapshot.sh");
        let shell = crate::shell::get_shell(shell_type, None).context("shell unavailable")?;
        write_shell_snapshot(&shell, &path, &ShellEnvironmentPolicy::default()).await?;
        let content = fs::read_to_string(&path).await?;
        Ok(content)
    }

    #[test]
    fn strip_snapshot_preamble_removes_leading_output() {
        let snapshot = "noise\n# Snapshot file\nexport PATH=/bin\n";
        let cleaned = strip_snapshot_preamble(snapshot).expect("snapshot marker exists");
        assert_eq!(cleaned, "# Snapshot file\nexport PATH=/bin\n");
    }

    #[test]
    fn strip_snapshot_preamble_requires_marker() {
        let result = strip_snapshot_preamble("missing header");
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn bash_snapshot_filters_invalid_exports() -> Result<()> {
        let output = Command::new("/bin/bash")
            .arg("-c")
            .arg(bash_snapshot_script())
            .env("BASH_ENV", "/dev/null")
            .env("VALID_NAME", "ok")
            .env("EXAMPLE_SECRET", "not-on-disk")
            .env("PWD", "/tmp/stale")
            .env("NEXTEST_BIN_EXE_codex-write-config-schema", "/path/to/bin")
            .env("BAD-NAME", "broken")
            .output()?;

        assert!(output.status.success());

        let stdout = render_snapshot(
            &String::from_utf8_lossy(&output.stdout),
            &ShellEnvironmentPolicy::default(),
        )?;
        assert!(stdout.contains("VALID_NAME=ok"));
        assert!(!stdout.contains("not-on-disk"));
        assert!(!stdout.contains("PWD=/tmp/stale"));
        assert!(!stdout.contains("NEXTEST_BIN_EXE_codex-write-config-schema"));
        assert!(!stdout.contains("BAD-NAME"));

        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn try_new_creates_and_deletes_snapshot_file() -> Result<()> {
        let dir = tempdir()?;
        let shell = Shell {
            shell_type: ShellType::Bash,
            shell_path: PathBuf::from("/bin/bash"),
            shell_snapshot: crate::shell::empty_shell_snapshot_receiver(),
        };

        let snapshot = ShellSnapshot::try_new(
            dir.path(),
            ThreadId::new(),
            &shell,
            &ShellEnvironmentPolicy::default(),
        )
        .await
        .expect("snapshot should be created");
        let path = snapshot.path.clone();
        assert!(path.exists());

        drop(snapshot);

        assert!(!path.exists());

        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn snapshot_shell_does_not_inherit_stdin() -> Result<()> {
        let _stdin_guard = BlockingStdinPipe::install()?;

        let dir = tempdir()?;
        let home = dir.path();
        fs::write(home.join(".bashrc"), "read -r ignored\n").await?;

        let shell = Shell {
            shell_type: ShellType::Bash,
            shell_path: PathBuf::from("/bin/bash"),
            shell_snapshot: crate::shell::empty_shell_snapshot_receiver(),
        };

        let home_display = home.display();
        let script = format!(
            "HOME=\"{home_display}\"; export HOME; {}",
            bash_snapshot_script()
        );
        let output = run_script_with_timeout(
            &shell,
            &script,
            Duration::from_secs(2),
            true,
            &ShellEnvironmentPolicy::default(),
        )
        .await
        .context("run snapshot command")?;

        assert!(
            output.contains("# Snapshot file"),
            "expected snapshot marker in output; output={output:?}"
        );

        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn timed_out_snapshot_shell_is_terminated() -> Result<()> {
        use std::process::Stdio;
        use tokio::time::Duration as TokioDuration;
        use tokio::time::Instant;
        use tokio::time::sleep;

        let dir = tempdir()?;
        let pid_path = dir.path().join("pid");
        let script = format!("echo $$ > \"{}\"; sleep 30", pid_path.display());

        let shell = Shell {
            shell_type: ShellType::Sh,
            shell_path: PathBuf::from("/bin/sh"),
            shell_snapshot: crate::shell::empty_shell_snapshot_receiver(),
        };

        let err = run_script_with_timeout(
            &shell,
            &script,
            Duration::from_secs(1),
            true,
            &ShellEnvironmentPolicy::default(),
        )
        .await
        .expect_err("snapshot shell should time out");
        assert!(
            err.to_string().contains("timed out"),
            "expected timeout error, got {err:?}"
        );

        let pid = fs::read_to_string(&pid_path)
            .await
            .expect("snapshot shell writes its pid before timing out")
            .trim()
            .parse::<i32>()?;

        let deadline = Instant::now() + TokioDuration::from_secs(1);
        loop {
            let kill_status = StdCommand::new("kill")
                .arg("-0")
                .arg(pid.to_string())
                .stderr(Stdio::null())
                .stdout(Stdio::null())
                .status()?;
            if !kill_status.success() {
                break;
            }
            if Instant::now() >= deadline {
                panic!("timed out snapshot shell is still alive after grace period");
            }
            sleep(TokioDuration::from_millis(50)).await;
        }

        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_zsh_snapshot_includes_sections() -> Result<()> {
        let snapshot = get_snapshot(ShellType::Zsh).await?;
        assert_posix_snapshot_sections(&snapshot);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_bash_snapshot_includes_sections() -> Result<()> {
        let snapshot = get_snapshot(ShellType::Bash).await?;
        assert_posix_snapshot_sections(&snapshot);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_sh_snapshot_includes_sections() -> Result<()> {
        let snapshot = get_snapshot(ShellType::Sh).await?;
        assert_posix_snapshot_sections(&snapshot);
        Ok(())
    }

    #[tokio::test]
    async fn unsupported_shells_do_not_publish_a_snapshot() -> Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("snapshot");
        for shell_type in [ShellType::PowerShell, ShellType::Cmd] {
            let shell = Shell {
                shell_type,
                shell_path: dir.path().join("unused"),
                shell_snapshot: crate::shell::empty_shell_snapshot_receiver(),
            };
            let error = write_shell_snapshot(&shell, &path, &ShellEnvironmentPolicy::default())
                .await
                .unwrap_err();
            assert!(error.to_string().starts_with("Snapshot not supported"));
            assert!(!path.exists());
        }
        Ok(())
    }

    async fn write_rollout_stub(codex_home: &Path, session_id: ThreadId) -> Result<PathBuf> {
        let dir = codex_home
            .join("sessions")
            .join("2025")
            .join("01")
            .join("01");
        fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("rollout-2025-01-01T00-00-00-{session_id}.jsonl"));
        fs::write(&path, "").await?;
        Ok(path)
    }

    #[tokio::test]
    async fn cleanup_keeps_recent_snapshots_with_or_without_a_rollout() -> Result<()> {
        let dir = tempdir()?;
        let codex_home = dir.path();
        let snapshot_dir = codex_home.join(SNAPSHOT_DIR);
        fs::create_dir_all(&snapshot_dir).await?;

        let live_session = ThreadId::new();
        let orphan_session = ThreadId::new();
        let live_snapshot = snapshot_dir.join(format!("{live_session}.sh"));
        let orphan_snapshot = snapshot_dir.join(format!("{orphan_session}.sh"));
        let invalid_snapshot = snapshot_dir.join("not-a-snapshot.txt");

        write_rollout_stub(codex_home, live_session).await?;
        fs::write(&live_snapshot, "live").await?;
        fs::write(&orphan_snapshot, "orphan").await?;
        fs::write(&invalid_snapshot, "invalid").await?;

        cleanup_stale_snapshots(codex_home, ThreadId::new()).await?;

        assert_eq!(live_snapshot.exists(), true);
        assert!(orphan_snapshot.exists());
        assert!(invalid_snapshot.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_stale_snapshots_removes_stale_rollouts() -> Result<()> {
        let dir = tempdir()?;
        let codex_home = dir.path();
        let snapshot_dir = codex_home.join(SNAPSHOT_DIR);
        fs::create_dir_all(&snapshot_dir).await?;

        let stale_session = ThreadId::new();
        let stale_snapshot = snapshot_dir.join(format!("{stale_session}.sh"));
        let rollout_path = write_rollout_stub(codex_home, stale_session).await?;
        fs::write(&stale_snapshot, "stale").await?;

        set_file_mtime(&rollout_path, SNAPSHOT_RETENTION + Duration::from_secs(60))?;

        cleanup_stale_snapshots(codex_home, ThreadId::new()).await?;

        assert_eq!(stale_snapshot.exists(), false);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_stale_snapshots_skips_active_session() -> Result<()> {
        let dir = tempdir()?;
        let codex_home = dir.path();
        let snapshot_dir = codex_home.join(SNAPSHOT_DIR);
        fs::create_dir_all(&snapshot_dir).await?;

        let active_session = ThreadId::new();
        let active_snapshot = snapshot_dir.join(format!("{active_session}.sh"));
        let rollout_path = write_rollout_stub(codex_home, active_session).await?;
        fs::write(&active_snapshot, "active").await?;

        set_file_mtime(&rollout_path, SNAPSHOT_RETENTION + Duration::from_secs(60))?;

        cleanup_stale_snapshots(codex_home, active_session).await?;

        assert_eq!(active_snapshot.exists(), true);
        Ok(())
    }

    #[cfg(unix)]
    fn set_file_mtime(path: &Path, age: Duration) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs()
            .saturating_sub(age.as_secs());
        let tv_sec = now
            .try_into()
            .map_err(|_| anyhow!("Snapshot mtime is out of range for libc::timespec"))?;
        let ts = libc::timespec { tv_sec, tv_nsec: 0 };
        let times = [ts, ts];
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        let result = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}
