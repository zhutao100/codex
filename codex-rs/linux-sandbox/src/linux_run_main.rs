use clap::Parser;
use std::ffi::CString;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use crate::bwrap::BwrapOptions;
use crate::bwrap::create_bwrap_command_args;
use crate::seccomp::apply_sandbox_policy_to_current_thread;

#[derive(Debug, Parser)]
/// CLI surface for the Linux sandbox helper.
struct LinuxSandboxCommand {
    /// It is possible that the cwd used in the context of the sandbox policy
    /// is different from the cwd of the process to spawn.
    #[arg(long = "sandbox-policy-cwd")]
    pub sandbox_policy_cwd: PathBuf,

    #[arg(long = "sandbox-policy")]
    pub sandbox_policy: codex_core::protocol::SandboxPolicy,

    /// Internal: apply seccomp and `no_new_privs` in the already-sandboxed
    /// process, then exec the user command.
    ///
    /// This exists so we can run bubblewrap first (which may rely on setuid)
    /// and only tighten with seccomp after the filesystem view is established.
    #[arg(long = "apply-seccomp-then-exec", hide = true, default_value_t = false)]
    pub apply_seccomp_then_exec: bool,

    /// When set, skip mounting a fresh `/proc` even though PID isolation is
    /// still enabled. This is primarily intended for restrictive container
    /// environments that deny `--proc /proc`.
    #[arg(long = "no-proc", default_value_t = false)]
    pub no_proc: bool,

    /// Full command args to run under the Linux sandbox helper.
    #[arg(trailing_var_arg = true)]
    pub command: Vec<String>,
}

/// Entry point for the Linux sandbox helper.
///
/// The sequence is:
/// 1. When needed, wrap the command with bubblewrap to construct the
///    filesystem view.
/// 2. Apply in-process restrictions (no_new_privs + seccomp).
/// 3. `execvp` into the final command.
pub fn run_main() -> ! {
    let LinuxSandboxCommand {
        sandbox_policy_cwd,
        sandbox_policy,
        apply_seccomp_then_exec,
        no_proc,
        command,
    } = LinuxSandboxCommand::parse();

    if command.is_empty() {
        panic!("No command specified to execute.");
    }

    // Inner stage: apply seccomp/no_new_privs after bubblewrap has already
    // established the filesystem view.
    if apply_seccomp_then_exec || sandbox_policy.has_full_disk_write_access() {
        if let Err(e) = apply_sandbox_policy_to_current_thread(&sandbox_policy) {
            panic!("error applying Linux sandbox restrictions: {e:?}");
        }
        exec_or_panic(command);
    }

    let inner = build_inner_seccomp_command(&sandbox_policy_cwd, &sandbox_policy, command);
    run_bwrap_with_proc_fallback(&sandbox_policy_cwd, &sandbox_policy, inner, !no_proc);
}

fn run_bwrap_with_proc_fallback(
    sandbox_policy_cwd: &Path,
    sandbox_policy: &codex_core::protocol::SandboxPolicy,
    inner: Vec<String>,
    mount_proc: bool,
) -> ! {
    let bwrap = crate::bwrap_program::resolve().unwrap_or_else(|err| {
        eprintln!("codex-linux-sandbox: {err}");
        std::process::exit(1);
    });
    let mut mount_proc = mount_proc;

    if mount_proc && !preflight_proc_mount_support(&bwrap, sandbox_policy_cwd, sandbox_policy) {
        eprintln!("codex-linux-sandbox: bwrap could not mount /proc; retrying with --no-proc");
        mount_proc = false;
    }

    let options = BwrapOptions { mount_proc };
    let argv = build_bwrap_argv(inner, sandbox_policy, sandbox_policy_cwd, options);
    let err = Command::new(&bwrap).args(argv).exec();
    eprintln!(
        "codex-linux-sandbox: could not start {}: {err}",
        bwrap.display()
    );
    std::process::exit(1);
}

fn build_bwrap_argv(
    inner: Vec<String>,
    sandbox_policy: &codex_core::protocol::SandboxPolicy,
    sandbox_policy_cwd: &Path,
    options: BwrapOptions,
) -> Vec<String> {
    create_bwrap_command_args(inner, sandbox_policy, sandbox_policy_cwd, options)
        .unwrap_or_else(|err| panic!("error building bubblewrap command: {err:?}"))
}

fn preflight_proc_mount_support(
    bwrap: &Path,
    sandbox_policy_cwd: &Path,
    sandbox_policy: &codex_core::protocol::SandboxPolicy,
) -> bool {
    let preflight_command = vec!["/bin/true".to_owned()];
    let preflight_argv = build_bwrap_argv(
        preflight_command,
        sandbox_policy,
        sandbox_policy_cwd,
        BwrapOptions { mount_proc: true },
    );
    let output = Command::new(bwrap)
        .args(preflight_argv)
        .output()
        .unwrap_or_else(|err| panic!("could not start {}: {err}", bwrap.display()));
    !is_proc_mount_failure(&String::from_utf8_lossy(&output.stderr))
}

fn is_proc_mount_failure(stderr: &str) -> bool {
    stderr.contains("Can't mount proc")
        && stderr.contains("/newroot/proc")
        && stderr.contains("Invalid argument")
}

/// Build the inner command that applies seccomp after bubblewrap.
fn build_inner_seccomp_command(
    sandbox_policy_cwd: &Path,
    sandbox_policy: &codex_core::protocol::SandboxPolicy,
    command: Vec<String>,
) -> Vec<String> {
    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(err) => panic!("failed to resolve current executable path: {err}"),
    };
    let policy_json = match serde_json::to_string(sandbox_policy) {
        Ok(json) => json,
        Err(err) => panic!("failed to serialize sandbox policy: {err}"),
    };

    let mut inner = vec![
        current_exe.to_string_lossy().to_string(),
        crate::INNER_STAGE_ARG.to_string(),
        "--sandbox-policy-cwd".to_string(),
        sandbox_policy_cwd.to_string_lossy().to_string(),
        "--sandbox-policy".to_string(),
        policy_json,
    ];
    inner.push("--".to_string());
    inner.extend(command);
    inner
}

/// Exec the provided argv, panicking with context if it fails.
fn exec_or_panic(command: Vec<String>) -> ! {
    #[expect(clippy::expect_used)]
    let c_command =
        CString::new(command[0].as_str()).expect("Failed to convert command to CString");
    #[expect(clippy::expect_used)]
    let c_args: Vec<CString> = command
        .iter()
        .map(|arg| CString::new(arg.as_str()).expect("Failed to convert arg to CString"))
        .collect();

    let mut c_args_ptrs: Vec<*const libc::c_char> = c_args.iter().map(|arg| arg.as_ptr()).collect();
    c_args_ptrs.push(std::ptr::null());

    unsafe {
        libc::execvp(c_command.as_ptr(), c_args_ptrs.as_ptr());
    }

    // If execvp returns, there was an error.
    let err = std::io::Error::last_os_error();
    panic!("Failed to execvp {}: {err}", command[0].as_str());
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_core::protocol::SandboxPolicy;
    use pretty_assertions::assert_eq;

    #[test]
    fn detects_proc_mount_invalid_argument_failure() {
        let stderr = "bwrap: Can't mount proc on /newroot/proc: Invalid argument";
        assert_eq!(is_proc_mount_failure(stderr), true);
    }

    #[test]
    fn ignores_non_proc_mount_errors() {
        let stderr = "bwrap: Can't bind mount /dev/null: Operation not permitted";
        assert_eq!(is_proc_mount_failure(stderr), false);
    }

    #[test]
    fn passes_the_literal_command_after_the_bwrap_separator() {
        let argv = build_bwrap_argv(
            vec!["/bin/true".to_string()],
            &SandboxPolicy::new_read_only_policy(),
            Path::new("/"),
            BwrapOptions { mount_proc: true },
        );
        assert_eq!(
            argv,
            vec![
                "--new-session".to_string(),
                "--die-with-parent".to_string(),
                "--ro-bind".to_string(),
                "/".to_string(),
                "/".to_string(),
                "--dev-bind".to_string(),
                "/dev/null".to_string(),
                "/dev/null".to_string(),
                "--unshare-pid".to_string(),
                "--proc".to_string(),
                "/proc".to_string(),
                "--".to_string(),
                "/bin/true".to_string(),
            ]
        );
    }
}
