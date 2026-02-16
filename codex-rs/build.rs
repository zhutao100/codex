use semver::{BuildMetadata, Prerelease, Version};
use std::{
    collections::HashMap,
    env,
    path::PathBuf,
    process::Command,
};

#[derive(Clone, Debug)]
struct TagInfo {
    name: String,
    version: Version,
}

#[derive(Debug)]
enum Scenario {
    ExactTag { tag: TagInfo },
    ContainedInNext {
        baseline: Option<TagInfo>,
        next: TagInfo,
        behind: u64,
    },
    PostLatestTag { baseline: TagInfo, ahead: u64 },
    Diverged {
        baseline: Option<TagInfo>,
        next: TagInfo,
        ahead_from_common: u64,
    },
    Fallback,
}

fn main() {
    emit_rerun_hints();

    // Optional: refresh "remote tags list" (networky). Off by default for reproducible/offline builds.
    // git-fetch fetches tags/refs from remotes. :contentReference[oaicite:2]{index=2}
    if env_flag("GIT_VERSION_FETCH_TAGS") {
        let remote = env::var("GIT_VERSION_REMOTE").unwrap_or_else(|_| "origin".to_string());
        let _ = git(&["fetch", "--tags", "--force", "--prune", "--quiet", &remote]);
    }
    println!("cargo:rerun-if-env-changed=GIT_VERSION_FETCH_TAGS");
    println!("cargo:rerun-if-env-changed=GIT_VERSION_REMOTE");
    println!("cargo:rerun-if-env-changed=GIT_VERSION_MAX_TAGS");
    println!("cargo:rerun-if-env-changed=GIT_VERSION_OVERRIDE_CARGO_PKG_VERSION");

    let head = git_trim(&["rev-parse", "HEAD"]).unwrap_or_default();
    let head_short = git_trim(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let dirty = git_is_dirty();

    let all_tags = list_semver_tags();
    let points_at_head = tags_from_names(git_lines(&["tag", "--points-at", "HEAD"]), &all_tags);
    let contains_head = tags_from_names(git_lines(&["tag", "--contains", "HEAD"]), &all_tags);
    let merged_into_head = tags_from_names(git_lines(&["tag", "--merged", "HEAD"]), &all_tags);

    // git-tag semantics for --contains/--merged/--points-at are defined in git-tag docs. :contentReference[oaicite:3]{index=3}

    let scenario = if let Some(tag) = max_tag(points_at_head) {
        Scenario::ExactTag { tag }
    } else if let Some(next) = min_tag(contains_head.clone()) {
        // Step (1):
        // A future tag "contains" HEAD; use it as the next release tag.
        // Heuristic: only consider baseline tags reachable from HEAD to avoid mixing unrelated tag branches.
        let baseline = max_tag_before(&merged_into_head, &next.version);
        let behind = count_commits("HEAD", &next.name).unwrap_or(0);
        Scenario::ContainedInNext { baseline, next, behind }
    } else if let Some(baseline) = max_tag(merged_into_head.clone()) {
        // Typical dev state: HEAD is *after* the latest reachable tag; no future tag "contains" it.
        let ahead = count_commits(&baseline.name, "HEAD").unwrap_or(0);
        Scenario::PostLatestTag { baseline, ahead }
    } else {
        // Step (1) "newest common commit" best-effort:
        // find a merge-base between HEAD and some recent tags, pick the one closest to HEAD.
        let max_tags = env::var("GIT_VERSION_MAX_TAGS")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(80);

        if let Some((common, closest_next)) = newest_common_commit_and_next_tag(&all_tags, max_tags) {
            let ahead_from_common = count_commits(&common, "HEAD").unwrap_or(0);
            // Heuristic: pick baseline tags reachable from the newest common commit (same lineage),
            // not just the SemVer-adjacent tag which may live on an unrelated branch.
            let merged_into_common = tags_from_names(git_lines(&["tag", "--merged", &common]), &all_tags);
            let baseline = max_tag_before(&merged_into_common, &closest_next.version);
            Scenario::Diverged {
                baseline,
                next: closest_next,
                ahead_from_common,
            }
        } else {
            Scenario::Fallback
        }
    };

    // Step (2): generate a semver-ish version string with clear semantics per scenario.
    let cargo_manifest_version = env::var("CARGO_PKG_VERSION").ok();
    let version_str = build_version_string(&scenario, cargo_manifest_version.as_deref(), &head_short, dirty);

    // Export both: recommended APP_VERSION, plus optional override of CARGO_PKG_VERSION.
    println!("cargo:rustc-env=APP_VERSION={version_str}");
    println!("cargo:rustc-env=GIT_SHA={head}");
    println!("cargo:rustc-env=GIT_SHA_SHORT={head_short}");
    println!("cargo:rustc-env=GIT_VERSION_DIRTY={}", if dirty { "1" } else { "0" });
    println!("cargo:rustc-env=GIT_VERSION_SCENARIO={}", scenario_name(&scenario));

    // Opt-in: override compile-time CARGO_PKG_VERSION seen by env!("CARGO_PKG_VERSION") in *this crate*.
    // This does NOT change Cargo.toml's package version. :contentReference[oaicite:4]{index=4}
    if env_flag("GIT_VERSION_OVERRIDE_CARGO_PKG_VERSION") {
        println!("cargo:warning=Using git-derived version {version_str} (overriding CARGO_PKG_VERSION)");
        println!("cargo:rustc-env=CARGO_PKG_VERSION={version_str}");
    }
}

fn emit_rerun_hints() {
    // Re-run when git refs change; handle worktrees/submodules by asking git where its dir is.
    if let Some(git_dir) = git_trim(&["rev-parse", "--git-dir"]).map(PathBuf::from) {
        let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()));
        let git_dir = if git_dir.is_relative() { manifest_dir.join(git_dir) } else { git_dir };

        // Conservative set of triggers; packed-refs is common for tags.
        println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
        println!("cargo:rerun-if-changed={}", git_dir.join("packed-refs").display());
        println!("cargo:rerun-if-changed={}", git_dir.join("refs").display());
        println!("cargo:rerun-if-changed={}", git_dir.join("logs").display());
        println!("cargo:rerun-if-changed={}", git_dir.join("FETCH_HEAD").display());
    }
}

fn env_flag(key: &str) -> bool {
    matches!(env::var(key).as_deref(), Ok("1") | Ok("true") | Ok("yes") | Ok("on"))
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

fn git_trim(args: &[&str]) -> Option<String> {
    git(args).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn git_lines(args: &[&str]) -> Vec<String> {
    git(args)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn git_is_dirty() -> bool {
    // changes in working tree OR index => dirty
    let wt_clean = Command::new("git").args(["diff", "--quiet"]).status().map(|s| s.success()).unwrap_or(false);
    let idx_clean = Command::new("git").args(["diff", "--cached", "--quiet"]).status().map(|s| s.success()).unwrap_or(false);
    !(wt_clean && idx_clean)
}

fn list_semver_tags() -> Vec<TagInfo> {
    // for-each-ref is stable and avoids extra formatting churn.
    let raw = git(&["for-each-ref", "refs/tags", "--format=%(refname:short)"]).unwrap_or_default();
    let mut tags: Vec<TagInfo> = raw
        .lines()
        .filter_map(|line| {
            let name = line.trim();
            if name.is_empty() {
                return None;
            }
            let version = extract_semver_from_tag(name)?;
            Some(TagInfo { name: name.to_string(), version })
        })
        .collect();

    tags.sort_by(|a, b| a.version.cmp(&b.version).then_with(|| a.name.cmp(&b.name)));
    tags.dedup_by(|a, b| a.name == b.name);
    tags
}

fn tags_from_names(names: Vec<String>, all: &[TagInfo]) -> Vec<TagInfo> {
    let mut out = Vec::new();
    for n in names {
        if let Some(v) = extract_semver_from_tag(&n) {
            // Prefer exact TagInfo from all-tags (keeps canonical tag name if different),
            // else keep this name.
            if let Some(t) = all.iter().find(|t| t.name == n) {
                out.push(t.clone());
            } else {
                out.push(TagInfo { name: n, version: v });
            }
        }
    }
    out.sort_by(|a, b| a.version.cmp(&b.version).then_with(|| a.name.cmp(&b.name)));
    out
}

fn min_tag(mut tags: Vec<TagInfo>) -> Option<TagInfo> {
    tags.sort_by(|a, b| a.version.cmp(&b.version).then_with(|| a.name.cmp(&b.name)));
    tags.into_iter().next()
}

fn max_tag(mut tags: Vec<TagInfo>) -> Option<TagInfo> {
    tags.sort_by(|a, b| a.version.cmp(&b.version).then_with(|| a.name.cmp(&b.name)));
    tags.into_iter().last()
}

fn max_tag_before(tags: &[TagInfo], target: &Version) -> Option<TagInfo> {
    tags.iter()
        .filter(|t| t.version < *target)
        .max_by(|a, b| a.version.cmp(&b.version).then_with(|| a.name.cmp(&b.name)))
        .cloned()
}

fn count_commits(from: &str, to: &str) -> Option<u64> {
    let range = format!("{from}..{to}");
    git_trim(&["rev-list", "--count", &range])?.parse().ok()
}

fn newest_common_commit_and_next_tag(all: &[TagInfo], max_tags: usize) -> Option<(String, TagInfo)> {
    // Heuristic: scan newest tags first; pick merge-base closest to HEAD (min distance).
    let mut best: Option<(u64, String)> = None;
    for tag in all.iter().rev().take(max_tags) {
        let common = git_trim(&["merge-base", "HEAD", &tag.name])?;
        let dist = count_commits(&common, "HEAD").unwrap_or(u64::MAX);
        let replace = match best.as_ref() {
            None => true,
            Some((best_dist, _)) => dist < *best_dist,
        };
        if replace {
            best = Some((dist, common));
        }
        // Early exit: if we're extremely close, keep it fast.
        if matches!(best, Some((0 | 1, _))) {
            break;
        }
    }

    let (_, common_commit) = best?;
    // Use the *closest descendant tag that contains that common commit*.
    // Tie-break (same core): prefer stable tags over pre-releases.
    let mut contains_common = tags_from_names(git_lines(&["tag", "--contains", &common_commit]), all);
    contains_common.truncate(max_tags);
    let next = select_next_tag(&common_commit, contains_common)?;
    Some((common_commit, next))
}

#[derive(Clone, Debug)]
struct NextTagCandidate {
    tag: TagInfo,
    commit: String,
    dist: u64,
}

fn select_next_tag(common_commit: &str, tags: Vec<TagInfo>) -> Option<TagInfo> {
    let mut candidates = Vec::new();
    for tag in tags {
        let Some(commit) = tag_commit(&tag.name) else {
            continue;
        };
        let Some(dist) = count_commits(common_commit, &commit) else {
            continue;
        };
        candidates.push(NextTagCandidate { tag, commit, dist });
    }

    let min_dist = candidates.iter().map(|c| c.dist).min()?;
    let mut version_backfill_cache: HashMap<String, bool> = HashMap::new();

    let mut best: Option<(u64, TagInfo)> = None;
    for c in candidates {
        let mut effective_dist = c.dist;
        let peel_threshold = min_dist.saturating_add(1);
        if c.dist > 0 && c.dist <= peel_threshold && is_version_backfill_commit_cached(&c.commit, &mut version_backfill_cache)
        {
            effective_dist = c.dist - 1;
        }

        let replace = match best.as_ref() {
            None => true,
            Some((best_dist, best_tag)) => {
                effective_dist < *best_dist
                    || (effective_dist == *best_dist && next_tag_tiebreak(&c.tag, best_tag))
            }
        };
        if replace {
            best = Some((effective_dist, c.tag));
        }
    }

    best.map(|(_, tag)| tag)
}

fn next_tag_tiebreak(a: &TagInfo, b: &TagInfo) -> bool {
    let a_core = strip_pre_build(&a.version);
    let b_core = strip_pre_build(&b.version);
    if a_core == b_core {
        let a_stable = a.version.pre.is_empty();
        let b_stable = b.version.pre.is_empty();
        if a_stable != b_stable {
            return a_stable;
        }
    }
    a.version < b.version || (a.version == b.version && a.name < b.name)
}

fn tag_commit(tag: &str) -> Option<String> {
    git_trim(&["rev-parse", &format!("{tag}^{{}}")])
}

fn is_version_backfill_commit_cached(commit: &str, cache: &mut HashMap<String, bool>) -> bool {
    if let Some(v) = cache.get(commit) {
        return *v;
    }
    let v = is_version_backfill_commit(commit);
    cache.insert(commit.to_string(), v);
    v
}

fn is_version_backfill_commit(commit: &str) -> bool {
    let Some(parent) = single_parent(commit) else {
        return false;
    };

    let changed_files = git_lines(&["diff-tree", "--no-commit-id", "--name-only", "-r", commit]);
    if changed_files.is_empty() {
        return false;
    }

    // Only tolerate version bookkeeping in Cargo manifests/locks.
    let mut saw_manifest = false;
    for f in &changed_files {
        if f.ends_with("Cargo.toml") {
            saw_manifest = true;
        }
        if !(f.ends_with("Cargo.toml") || f.ends_with("Cargo.lock")) {
            return false;
        }
    }
    if !saw_manifest {
        return false;
    }

    // Ensure Cargo.toml changes are restricted to `version*` lines.
    for f in &changed_files {
        if f.ends_with("Cargo.toml") && !cargo_toml_only_version_lines_changed(&parent, commit, f) {
            return false;
        }
    }

    true
}

fn single_parent(commit: &str) -> Option<String> {
    let line = git_trim(&["rev-list", "--parents", "-n", "1", commit])?;
    let mut parts = line.split_whitespace();
    let _ = parts.next()?;
    let parent = parts.next()?.to_string();
    if parts.next().is_some() {
        return None;
    }
    Some(parent)
}

fn cargo_toml_only_version_lines_changed(parent: &str, commit: &str, path: &str) -> bool {
    let diff = git(&["diff", "--unified=0", parent, commit, "--", path]).unwrap_or_default();
    let mut saw_change = false;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        let Some(sig) = line.as_bytes().first() else {
            continue;
        };
        if *sig != b'+' && *sig != b'-' {
            continue;
        }
        saw_change = true;
        let content = &line[1..];
        let content = content.trim_start();
        if content.starts_with("version") {
            continue;
        }
        return false;
    }
    saw_change
}

fn build_version_string(s: &Scenario, cargo_manifest_version: Option<&str>, head_short: &str, dirty: bool) -> String {
    let base_fallback = cargo_manifest_version
        .and_then(|v| Version::parse(v).ok())
        .unwrap_or_else(|| Version::new(0, 0, 0));

    let mut v = match s {
        Scenario::ExactTag { tag } => tag.version.clone(),

        Scenario::ContainedInNext { baseline, next, behind } => {
            // You are *before* the next release tag commit, so use next_version - pre.<behind>.g<sha>.
            let prefix = format!("pre.{behind}.g{head_short}");
            let pre = with_baseline_prerelease(baseline.as_ref(), &next.version, &prefix);
            with_prerelease(&next.version, &pre)
        }

        Scenario::PostLatestTag { baseline, ahead } => {
            // Typical dev build: use next patch (or stay on same core if baseline was already pre-release).
            let core = strip_pre_build(&baseline.version);
            let bumped = if baseline.version.pre.is_empty() {
                Version { patch: core.patch.saturating_add(1), ..core }
            } else {
                core
            };

            let prefix = if baseline.version.pre.is_empty() {
                format!("dev.{ahead}.g{head_short}")
            } else {
                format!("{}.dev.{ahead}.g{head_short}", baseline.version.pre)
            };
            with_prerelease(&bumped, &prefix)
        }

        Scenario::Diverged {
            baseline,
            next,
            ahead_from_common,
        } => {
            // Branch diverged: anchor to "next" release line and mark it as a branch build.
            let prefix = format!("branch.{ahead_from_common}.g{head_short}");
            let pre = with_baseline_prerelease(baseline.as_ref(), &next.version, &prefix);
            with_prerelease(&next.version, &pre)
        }

        Scenario::Fallback => with_prerelease(&base_fallback, &format!("local.g{head_short}")),
    };

    if dirty {
        v = append_prerelease_ident(&v, "dirty");
    }

    v.to_string()
}

fn strip_pre_build(v: &Version) -> Version {
    Version {
        major: v.major,
        minor: v.minor,
        patch: v.patch,
        pre: Prerelease::EMPTY,
        build: BuildMetadata::EMPTY,
    }
}

fn with_prerelease(base: &Version, pre: &str) -> Version {
    let mut v = strip_pre_build(base);
    v.pre = Prerelease::new(pre).unwrap_or(Prerelease::EMPTY);
    v
}

fn append_prerelease_ident(v: &Version, ident: &str) -> Version {
    let mut out = v.clone();
    let new_pre = if out.pre.is_empty() {
        ident.to_string()
    } else {
        format!("{}.{}", out.pre, ident)
    };
    out.pre = Prerelease::new(&new_pre).unwrap_or(out.pre.clone());
    out
}

fn with_baseline_prerelease(baseline: Option<&TagInfo>, next: &Version, suffix: &str) -> String {
    let Some(baseline) = baseline else {
        return suffix.to_string();
    };
    if baseline.version.pre.is_empty() || strip_pre_build(&baseline.version) != strip_pre_build(next) {
        suffix.to_string()
    } else {
        format!("{}.{}", baseline.version.pre, suffix)
    }
}

fn scenario_name(s: &Scenario) -> &'static str {
    match s {
        Scenario::ExactTag { .. } => "exact_tag",
        Scenario::ContainedInNext { .. } => "contained_in_next",
        Scenario::PostLatestTag { .. } => "post_latest_tag",
        Scenario::Diverged { .. } => "diverged",
        Scenario::Fallback => "fallback",
    }
}

/// Extract the first valid semver-looking substring from a tag name, tolerating prefixes/suffixes.
/// Examples it handles:
/// - v1.2.3
/// - release/v1.2.3-rc.1
/// - myproj-1.2.3
/// - 1.2.3+build.7
fn extract_semver_from_tag(tag: &str) -> Option<Version> {
    let bytes = tag.as_bytes();

    let is_allowed = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'+' );

    let mut best: Option<(usize, usize)> = None;

    for i in 0..bytes.len() {
        let c = bytes[i];
        let starts = c.is_ascii_digit() || (c == b'v' && bytes.get(i + 1).is_some_and(|n| n.is_ascii_digit()));
        if !starts {
            continue;
        }

        let mut j = i;
        while j < bytes.len() && is_allowed(bytes[j]) {
            j += 1;
        }

        if j <= i + 2 {
            continue;
        }

        if best.map_or(true, |(bi, bj)| (j - i) > (bj - bi)) {
            best = Some((i, j));
        }
    }

    let (i, j) = best?;
    let mut s = tag[i..j].to_string();
    if let Some(stripped) = s.strip_prefix('v').or_else(|| s.strip_prefix('V')) {
        s = stripped.to_string();
    }
    Version::parse(&s).ok()
}
