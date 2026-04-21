//! Utility to compute the current Git diff for the working directory.
//!
//! The implementation mirrors the behaviour of the TypeScript version in
//! `codex-cli`: it returns the diff for tracked changes as well as any
//! untracked files. When the current directory is not inside a Git
//! repository, the function returns `Ok(GitDiffResult::NotGitRepo)`.

use codex_core::config::types::DiffView;
use codex_core::protocol::FileChange;
use diffy::create_patch;
use ratatui::style::Stylize;
use ratatui::text::Line as RtLine;
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

use crate::diff_render::render_diff_view;

#[derive(Debug)]
pub(crate) enum GitDiffResult {
    NotGitRepo,
    Error(String),
    Lines(Vec<RtLine<'static>>),
}

pub(crate) async fn get_git_diff(
    cwd: &Path,
    view: DiffView,
    width: usize,
    syntax_theme: &str,
) -> io::Result<GitDiffResult> {
    // First check if we are inside a Git repository.
    if !inside_git_repo(cwd).await? {
        return Ok(GitDiffResult::NotGitRepo);
    }

    let GitChanges { changes, warnings } = collect_git_changes(cwd).await?;
    let view_lines = render_diff_view(&changes, cwd, width, view, syntax_theme);
    let mut lines = if view_lines.is_empty() {
        vec![RtLine::from("(no changes)".dim().italic())]
    } else {
        view_lines
    };
    append_warnings(&mut lines, &warnings);

    Ok(GitDiffResult::Lines(lines))
}

struct GitChanges {
    changes: HashMap<PathBuf, FileChange>,
    warnings: Vec<String>,
}

async fn collect_git_changes(cwd: &Path) -> io::Result<GitChanges> {
    let repo_root = git_repo_root(cwd).await?;
    let status_output =
        run_git_capture_stdout(&repo_root, &["diff", "--name-status", "-z", "-M"]).await?;
    let mut changes = HashMap::new();
    let mut warnings = Vec::new();

    let mut parts = status_output.split('\0').filter(|s| !s.is_empty());
    while let Some(status) = parts.next() {
        let status_char = status.chars().next().unwrap_or('\0');
        match status_char {
            'R' | 'C' => {
                let Some(old_path) = parts.next() else {
                    break;
                };
                let Some(new_path) = parts.next() else {
                    break;
                };
                if let Some((path, change)) = build_change(
                    &repo_root,
                    status_char,
                    old_path,
                    Some(new_path),
                    &mut warnings,
                )
                .await?
                {
                    changes.insert(path, change);
                }
            }
            'A' | 'D' | 'M' => {
                let Some(path) = parts.next() else {
                    break;
                };
                if let Some((path, change)) =
                    build_change(&repo_root, status_char, path, None, &mut warnings).await?
                {
                    changes.insert(path, change);
                }
            }
            _ => {}
        }
    }

    let untracked_output = run_git_capture_stdout(
        &repo_root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .await?;
    for path in untracked_output.split('\0').filter(|s| !s.is_empty()) {
        if let Some((path, change)) =
            build_change(&repo_root, 'A', path, None, &mut warnings).await?
        {
            changes.insert(path, change);
        }
    }

    Ok(GitChanges { changes, warnings })
}

async fn build_change(
    repo_root: &Path,
    status: char,
    path: &str,
    renamed_to: Option<&str>,
    warnings: &mut Vec<String>,
) -> io::Result<Option<(PathBuf, FileChange)>> {
    let display_path = if let Some(new_path) = renamed_to {
        format!("{path} -> {new_path}")
    } else {
        path.to_string()
    };

    let old_bytes = if matches!(status, 'A') {
        Vec::new()
    } else {
        match run_git_capture_bytes(repo_root, &["show", &format!(":{path}")]).await {
            Ok(bytes) => bytes,
            Err(err) => {
                warnings.push(format!("Skipping {display_path}: {err}"));
                return Ok(None);
            }
        }
    };
    let new_bytes = if matches!(status, 'D') {
        Vec::new()
    } else {
        match tokio::fs::read(repo_root.join(renamed_to.unwrap_or(path))).await {
            Ok(bytes) => bytes,
            Err(err) => {
                warnings.push(format!("Skipping {display_path}: {err}"));
                return Ok(None);
            }
        }
    };

    if is_binary(&old_bytes) || is_binary(&new_bytes) {
        warnings.push(format!("Skipping {display_path}: binary file"));
        return Ok(None);
    }

    let old_text = if matches!(status, 'A') {
        String::new()
    } else {
        match String::from_utf8(old_bytes) {
            Ok(text) => text,
            Err(err) => {
                warnings.push(format!(
                    "Skipping {display_path}: file is not UTF-8 ({err})"
                ));
                return Ok(None);
            }
        }
    };
    let new_text = if matches!(status, 'D') {
        String::new()
    } else {
        match String::from_utf8(new_bytes) {
            Ok(text) => text,
            Err(err) => {
                warnings.push(format!(
                    "Skipping {display_path}: file is not UTF-8 ({err})"
                ));
                return Ok(None);
            }
        }
    };

    let move_path = renamed_to.map(|p| repo_root.join(p));
    let path = repo_root.join(path);
    let change = match status {
        'A' => FileChange::Add { content: new_text },
        'D' => FileChange::Delete { content: old_text },
        _ => FileChange::Update {
            unified_diff: create_patch(&old_text, &new_text).to_string(),
            move_path,
        },
    };

    Ok(Some((path, change)))
}

fn append_warnings(lines: &mut Vec<RtLine<'static>>, warnings: &[String]) {
    if warnings.is_empty() {
        return;
    }
    lines.push(RtLine::from(""));
    lines.push(RtLine::from("Warnings:".dim().bold()));
    lines.extend(
        warnings
            .iter()
            .map(|warning| RtLine::from(warning.clone().dim())),
    );
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

async fn git_repo_root(cwd: &Path) -> io::Result<PathBuf> {
    let output = run_git_capture_stdout(cwd, &["rev-parse", "--show-toplevel"]).await?;
    Ok(PathBuf::from(output.trim()))
}

/// Helper that executes `git` with the given `args` and returns `stdout` as a
/// UTF-8 string. Any non-zero exit status is considered an *error*.
async fn run_git_capture_stdout(cwd: &Path, args: &[&str]) -> io::Result<String> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(io::Error::other(format!(
            "git {args:?} failed with status {status}",
            status = output.status
        )))
    }
}

async fn run_git_capture_bytes(cwd: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await?;

    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(io::Error::other(format!(
            "git {args:?} failed with status {status}",
            status = output.status
        )))
    }
}

/// Determine if the current directory is inside a Git repository.
async fn inside_git_repo(cwd: &Path) -> io::Result<bool> {
    let status = Command::new("git")
        .current_dir(cwd)
        .args(["rev-parse", "--is-inside-work-tree"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;

    match status {
        Ok(s) if s.success() => Ok(true),
        Ok(_) => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false), // git not installed
        Err(e) => Err(e),
    }
}
