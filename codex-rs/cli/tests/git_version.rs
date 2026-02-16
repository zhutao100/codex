mod shared_build_rs {
    #![allow(dead_code)]
    include!("../../build.rs");

    #[cfg(test)]
    mod tests {
        use super::*;
        use pretty_assertions::assert_eq;
        use std::fs;
        use std::path::Path;
        use std::process::Command;
        use std::sync::Mutex;
        use tempfile::TempDir;

        static GIT_LOCK: Mutex<()> = Mutex::new(());

        fn run_git(repo: &Path, args: &[&str]) -> String {
            let out = Command::new("git")
                .args(args)
                .current_dir(repo)
                .output()
                .unwrap_or_else(|e| panic!("failed to run git {args:?}: {e}"));
            if !out.status.success() {
                panic!(
                    "git {args:?} failed (status {}): {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            String::from_utf8(out.stdout).expect("git output is valid utf-8")
        }

        fn git_trim(repo: &Path, args: &[&str]) -> String {
            run_git(repo, args).trim().to_string()
        }

        fn write_file(repo: &Path, rel: &str, contents: &str) {
            let path = repo.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }

        fn commit_all(repo: &Path, msg: &str) {
            run_git(repo, &["add", "."]);
            run_git(repo, &["commit", "-m", msg]);
        }

        fn init_repo() -> TempDir {
            let tmp = TempDir::new().unwrap();
            run_git(tmp.path(), &["init", "-b", "main"]);
            run_git(tmp.path(), &["config", "user.email", "test@example.com"]);
            run_git(tmp.path(), &["config", "user.name", "Test"]);
            tmp
        }

        fn cargo_toml_with_version(version: &str) -> String {
            format!(
                r#"[workspace.package]
version = "{version}"
"#
            )
        }

        fn compute_scenario(all_tags: &[TagInfo]) -> Scenario {
            let points_at_head =
                tags_from_names(git_lines(&["tag", "--points-at", "HEAD"]), all_tags);
            let contains_head =
                tags_from_names(git_lines(&["tag", "--contains", "HEAD"]), all_tags);
            let merged_into_head =
                tags_from_names(git_lines(&["tag", "--merged", "HEAD"]), all_tags);

            if let Some(tag) = max_tag(points_at_head) {
                Scenario::ExactTag { tag }
            } else if let Some(next) = min_tag(contains_head.clone()) {
                let baseline = max_tag_before(&merged_into_head, &next.version);
                let behind = count_commits("HEAD", &next.name).unwrap_or(0);
                Scenario::ContainedInNext {
                    baseline,
                    next,
                    behind,
                }
            } else if let Some(baseline) = max_tag(merged_into_head.clone()) {
                let ahead = count_commits(&baseline.name, "HEAD").unwrap_or(0);
                Scenario::PostLatestTag { baseline, ahead }
            } else if let Some((common, closest_next)) =
                newest_common_commit_and_next_tag(all_tags, 80)
            {
                let ahead_from_common = count_commits(&common, "HEAD").unwrap_or(0);
                let merged_into_common =
                    tags_from_names(git_lines(&["tag", "--merged", &common]), all_tags);
                let baseline = max_tag_before(&merged_into_common, &closest_next.version);
                Scenario::Diverged {
                    baseline,
                    next: closest_next,
                    ahead_from_common,
                }
            } else {
                Scenario::Fallback
            }
        }

        #[test]
        fn diverged_prefers_stable_and_drops_unrelated_prerelease_baseline() {
            let _guard = GIT_LOCK.lock().unwrap();

            let repo = init_repo();
            let repo = repo.path();

            write_file(repo, "Cargo.toml", &cargo_toml_with_version("0.0.0"));
            write_file(repo, "src.txt", "initial\n");
            commit_all(repo, "init");
            let initial = git_trim(repo, &["rev-parse", "HEAD"]);

            // Separate branch with an older tag that must not be treated as the baseline.
            run_git(repo, &["checkout", "-b", "alpha8", &initial]);
            write_file(
                repo,
                "Cargo.toml",
                &cargo_toml_with_version("0.95.0-alpha.8"),
            );
            commit_all(repo, "release alpha.8");
            run_git(repo, &["tag", "rust-v0.95.0-alpha.8"]);

            // Mainline base commit (newest common commit).
            run_git(repo, &["checkout", "-B", "main", &initial]);
            write_file(repo, "src.txt", "base\n");
            commit_all(repo, "base");
            let base = git_trim(repo, &["rev-parse", "HEAD"]);

            // Sibling prerelease tag (version-only commit).
            run_git(repo, &["checkout", "-b", "alpha9", &base]);
            write_file(
                repo,
                "Cargo.toml",
                &cargo_toml_with_version("0.95.0-alpha.9"),
            );
            commit_all(repo, "release alpha.9");
            run_git(repo, &["tag", "rust-v0.95.0-alpha.9"]);

            // Sibling stable tag (version-only commit).
            run_git(repo, &["checkout", "-b", "stable", &base]);
            write_file(repo, "Cargo.toml", &cargo_toml_with_version("0.95.0"));
            commit_all(repo, "release 0.95.0");
            run_git(repo, &["tag", "rust-v0.95.0"]);

            // Custom branch diverged from the base commit.
            run_git(repo, &["checkout", "-b", "custom", &base]);
            write_file(repo, "custom.txt", "1\n");
            commit_all(repo, "custom 1");
            write_file(repo, "custom.txt", "2\n");
            commit_all(repo, "custom 2");
            write_file(repo, "custom.txt", "3\n");
            commit_all(repo, "custom 3");

            let original_cwd = std::env::current_dir().unwrap();
            std::env::set_current_dir(repo).unwrap();
            let all_tags = list_semver_tags();
            let scenario = compute_scenario(&all_tags);
            let head_short = git_trim(repo, &["rev-parse", "--short=12", "HEAD"]);
            let version = build_version_string(&scenario, Some("0.0.0"), &head_short, false);
            std::env::set_current_dir(original_cwd).unwrap();

            match scenario {
                Scenario::Diverged {
                    baseline,
                    next,
                    ahead_from_common,
                } => {
                    assert_eq!(baseline.is_none(), true);
                    assert_eq!(next.name, "rust-v0.95.0");
                    assert_eq!(ahead_from_common, 3);
                }
                other => panic!("expected diverged scenario, got {other:?}"),
            }

            assert_eq!(version, format!("0.95.0-branch.3.{head_short}"));
        }

        #[test]
        fn diverged_branch_count_uses_merge_base_with_selected_next_tag() {
            let _guard = GIT_LOCK.lock().unwrap();

            let repo = init_repo();
            let repo = repo.path();

            write_file(repo, "Cargo.toml", &cargo_toml_with_version("0.0.0"));
            write_file(repo, "src.txt", "init\n");
            commit_all(repo, "init");

            write_file(repo, "src.txt", "base\n");
            commit_all(repo, "base");
            let base = git_trim(repo, &["rev-parse", "HEAD"]);

            // Release line: make a "release base" commit (this will be the merge-base with HEAD),
            // then tag `rust-v0.114.0` one commit later.
            run_git(repo, &["checkout", "-b", "release", &base]);
            write_file(repo, "release.txt", "release-base\n");
            commit_all(repo, "release base");
            let release_base = git_trim(repo, &["rev-parse", "HEAD"]);

            write_file(repo, "Cargo.toml", &cargo_toml_with_version("0.114.0"));
            commit_all(repo, "release 0.114.0");
            run_git(repo, &["tag", "rust-v0.114.0"]);

            // Trunk line: create >80 newer tags so `rust-v0.114.0` is outside the default scan window.
            run_git(repo, &["checkout", "-B", "trunk", &base]);
            write_file(repo, "trunk.txt", "start\n");
            commit_all(repo, "trunk start");
            for i in 1..=81 {
                write_file(repo, "trunk.txt", &format!("{i}\n"));
                commit_all(repo, &format!("trunk {i}"));
                run_git(repo, &["tag", &format!("rust-v0.115.0-alpha.{i}")]);
            }

            // Custom branch diverges from the release line (not from trunk).
            run_git(repo, &["checkout", "-b", "custom", &release_base]);
            for i in 1..=3 {
                write_file(repo, "custom.txt", &format!("{i}\n"));
                commit_all(repo, &format!("custom {i}"));
            }

            let original_cwd = std::env::current_dir().unwrap();
            std::env::set_current_dir(repo).unwrap();
            let all_tags = list_semver_tags();
            let scenario = compute_scenario(&all_tags);
            let head_short = git_trim(repo, &["rev-parse", "--short=12", "HEAD"]);
            let version = build_version_string(&scenario, Some("0.0.0"), &head_short, false);
            std::env::set_current_dir(original_cwd).unwrap();

            match scenario {
                Scenario::Diverged {
                    baseline,
                    next,
                    ahead_from_common,
                } => {
                    assert_eq!(baseline.is_none(), true);
                    assert_eq!(next.name, "rust-v0.114.0");
                    assert_eq!(ahead_from_common, 3);
                }
                other => panic!("expected diverged scenario, got {other:?}"),
            }

            assert_eq!(version, format!("0.114.0-branch.3.{head_short}"));
        }

        #[test]
        fn diverged_tag_flood_does_not_hide_relevant_stable_release_line() {
            let _guard = GIT_LOCK.lock().unwrap();

            let repo = init_repo();
            let repo = repo.path();

            write_file(repo, "Cargo.toml", &cargo_toml_with_version("0.0.0"));
            write_file(repo, "src.txt", "init\n");
            commit_all(repo, "init");

            write_file(repo, "src.txt", "diverge\n");
            commit_all(repo, "diverge");
            let diverge = git_trim(repo, &["rev-parse", "HEAD"]);

            // Trunk line: early prerelease tags for the same core version as the target stable tag.
            run_git(repo, &["checkout", "-b", "trunk", &diverge]);
            write_file(repo, "trunk.txt", "alpha.1\n");
            commit_all(repo, "alpha.1");
            run_git(repo, &["tag", "rust-v0.114.0-alpha.1"]);

            write_file(repo, "trunk.txt", "alpha.2\n");
            commit_all(repo, "alpha.2");
            run_git(repo, &["tag", "rust-v0.114.0-alpha.2"]);

            // Flood later tags so the stable `rust-v0.114.0` tag is outside the default scan window.
            for i in 1..=81 {
                write_file(repo, "trunk.txt", &format!("later {i}\n"));
                commit_all(repo, &format!("later {i}"));
                run_git(repo, &["tag", &format!("rust-v0.115.0-alpha.{i}")]);
            }

            // Release line: extra commits that trunk does not have.
            run_git(repo, &["checkout", "-b", "release", &diverge]);
            for i in 1..=10 {
                write_file(repo, "release.txt", &format!("release {i}\n"));
                commit_all(repo, &format!("release {i}"));
            }
            let release_base = git_trim(repo, &["rev-parse", "HEAD"]);

            // Stable tag is on a sibling branch so it is not merged into HEAD.
            run_git(repo, &["checkout", "-b", "stable", &release_base]);
            write_file(repo, "Cargo.toml", &cargo_toml_with_version("0.114.0"));
            commit_all(repo, "release 0.114.0");
            run_git(repo, &["tag", "rust-v0.114.0"]);

            // Custom branch diverges from the release line.
            run_git(repo, &["checkout", "-b", "custom", &release_base]);
            for i in 1..=3 {
                write_file(repo, "custom.txt", &format!("{i}\n"));
                commit_all(repo, &format!("custom {i}"));
            }

            let original_cwd = std::env::current_dir().unwrap();
            std::env::set_current_dir(repo).unwrap();
            let all_tags = list_semver_tags();
            let scenario = compute_scenario(&all_tags);
            let head_short = git_trim(repo, &["rev-parse", "--short=12", "HEAD"]);
            let version = build_version_string(&scenario, Some("0.0.0"), &head_short, false);
            std::env::set_current_dir(original_cwd).unwrap();

            match scenario {
                Scenario::Diverged {
                    baseline,
                    next,
                    ahead_from_common,
                } => {
                    assert_eq!(baseline.is_none(), true);
                    assert_eq!(next.name, "rust-v0.114.0");
                    assert_eq!(ahead_from_common, 3);
                }
                other => panic!("expected diverged scenario, got {other:?}"),
            }

            assert_eq!(version, format!("0.114.0-branch.3.{head_short}"));
        }

        #[test]
        fn select_next_tag_peels_version_backfill_commit() {
            let _guard = GIT_LOCK.lock().unwrap();

            let repo = init_repo();
            let repo = repo.path();

            write_file(repo, "Cargo.toml", &cargo_toml_with_version("0.0.0"));
            write_file(repo, "src.txt", "initial\n");
            commit_all(repo, "init");

            write_file(repo, "src.txt", "common\n");
            commit_all(repo, "common");
            let common = git_trim(repo, &["rev-parse", "HEAD"]);

            // Prerelease tag one commit after common, but *not* a pure version backfill.
            run_git(repo, &["checkout", "-b", "alpha9", &common]);
            write_file(
                repo,
                "Cargo.toml",
                &cargo_toml_with_version("0.95.0-alpha.9"),
            );
            write_file(repo, "src.txt", "alpha.9\n");
            commit_all(repo, "alpha.9");
            run_git(repo, &["tag", "rust-v0.95.0-alpha.9"]);

            // Stable tag is a pure version backfill, but is two commits after common.
            run_git(repo, &["checkout", "-b", "stable-line", &common]);
            write_file(repo, "src.txt", "pre-stable\n");
            commit_all(repo, "pre-stable");
            write_file(repo, "Cargo.toml", &cargo_toml_with_version("0.95.0"));
            commit_all(repo, "stable version bump");
            run_git(repo, &["tag", "rust-v0.95.0"]);

            let original_cwd = std::env::current_dir().unwrap();
            std::env::set_current_dir(repo).unwrap();
            let all_tags = list_semver_tags();
            let contains_common =
                tags_from_names(git_lines(&["tag", "--contains", &common]), &all_tags);
            let next = select_next_tag(&common, contains_common).expect("select next tag");
            std::env::set_current_dir(original_cwd).unwrap();

            assert_eq!(next.name, "rust-v0.95.0");
        }

        #[test]
        fn contained_in_next_keeps_linear_baseline_prerelease_prefix() {
            let _guard = GIT_LOCK.lock().unwrap();

            let repo = init_repo();
            let repo = repo.path();

            write_file(repo, "Cargo.toml", &cargo_toml_with_version("0.0.0"));
            commit_all(repo, "init");

            write_file(
                repo,
                "Cargo.toml",
                &cargo_toml_with_version("0.95.0-alpha.1"),
            );
            commit_all(repo, "release alpha.1");
            run_git(repo, &["tag", "rust-v0.95.0-alpha.1"]);

            write_file(repo, "src.txt", "between\n");
            commit_all(repo, "between");
            let between = git_trim(repo, &["rev-parse", "HEAD"]);

            write_file(
                repo,
                "Cargo.toml",
                &cargo_toml_with_version("0.95.0-alpha.2"),
            );
            commit_all(repo, "release alpha.2");
            run_git(repo, &["tag", "rust-v0.95.0-alpha.2"]);

            let original_cwd = std::env::current_dir().unwrap();
            std::env::set_current_dir(repo).unwrap();
            run_git(repo, &["checkout", &between]);
            let all_tags = list_semver_tags();
            let scenario = compute_scenario(&all_tags);
            let head_short = git_trim(repo, &["rev-parse", "--short=12", "HEAD"]);
            let version = build_version_string(&scenario, Some("0.0.0"), &head_short, false);
            std::env::set_current_dir(original_cwd).unwrap();

            assert_eq!(version, format!("0.95.0-alpha.1.pre.1.{head_short}"));
        }

        #[test]
        fn version_backfill_detection_requires_manifest_changes() {
            let _guard = GIT_LOCK.lock().unwrap();

            let repo = init_repo();
            let repo = repo.path();

            write_file(repo, "Cargo.toml", &cargo_toml_with_version("0.0.0"));
            write_file(repo, "Cargo.lock", "lock-v0\n");
            commit_all(repo, "init");

            write_file(repo, "Cargo.lock", "lock-v1\n");
            commit_all(repo, "lock only");
            let lock_only = git_trim(repo, &["rev-parse", "HEAD"]);

            let original_cwd = std::env::current_dir().unwrap();
            std::env::set_current_dir(repo).unwrap();
            assert_eq!(is_version_backfill_commit(&lock_only), false);
            std::env::set_current_dir(original_cwd).unwrap();
        }
    }
}
