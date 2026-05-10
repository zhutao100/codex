use crate::config::Constrained;
use crate::config::types::SandboxReadOnlyConfig;
use crate::protocol::SandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

const TEMP_SUBDIR_PREFIX: &str = "codex-readonly-";

pub(super) fn materialize_read_only_temp_writable_roots(
    sandbox_policy: &mut Constrained<SandboxPolicy>,
    config: &SandboxReadOnlyConfig,
) -> Vec<String> {
    let tmpdir_env_var = std::env::var_os("TMPDIR").map(PathBuf::from);
    materialize_read_only_temp_writable_roots_with_parents(
        sandbox_policy,
        config,
        Path::new("/tmp"),
        tmpdir_env_var.as_deref(),
    )
}

fn materialize_read_only_temp_writable_roots_with_parents(
    sandbox_policy: &mut Constrained<SandboxPolicy>,
    config: &SandboxReadOnlyConfig,
    slash_tmp_parent: &Path,
    tmpdir_env_var: Option<&Path>,
) -> Vec<String> {
    if !matches!(sandbox_policy.get(), SandboxPolicy::ReadOnly { .. })
        || (!config.writeable_slash_tmp_subdir && !config.writeable_tmpdir_env_var_subdir)
    {
        return Vec::new();
    }

    let suffix = Uuid::new_v4().to_string();
    let mut warnings = Vec::new();
    let mut roots = Vec::new();
    let mut accepted_canonical_paths = Vec::new();

    if config.writeable_slash_tmp_subdir {
        maybe_add_temp_root(
            "sandbox_read_only.writeable_slash_tmp_subdir",
            slash_tmp_parent,
            &suffix,
            &mut accepted_canonical_paths,
            &mut roots,
            &mut warnings,
        );
    }

    if config.writeable_tmpdir_env_var_subdir {
        match tmpdir_env_var {
            Some(tmpdir) if !tmpdir.as_os_str().is_empty() => maybe_add_temp_root(
                "sandbox_read_only.writeable_tmpdir_env_var_subdir",
                tmpdir,
                &suffix,
                &mut accepted_canonical_paths,
                &mut roots,
                &mut warnings,
            ),
            Some(_) => warnings.push(
                "sandbox_read_only.writeable_tmpdir_env_var_subdir was enabled, but TMPDIR is empty; no TMPDIR writable subdirectory was added."
                    .to_string(),
            ),
            None => warnings.push(
                "sandbox_read_only.writeable_tmpdir_env_var_subdir was enabled, but TMPDIR is not set; no TMPDIR writable subdirectory was added."
                    .to_string(),
            ),
        }
    }

    if roots.is_empty() {
        return warnings;
    }

    let mut next_policy = sandbox_policy.get().clone();
    if let SandboxPolicy::ReadOnly {
        temp_writable_roots,
    } = &mut next_policy
    {
        for root in roots {
            if !temp_writable_roots.iter().any(|existing| existing == &root) {
                temp_writable_roots.push(root);
            }
        }
    }

    if let Err(err) = sandbox_policy.set(next_policy) {
        warnings.push(format!(
            "Failed to enable read-only temp writable roots because the sandbox policy was rejected: {err}"
        ));
    }

    warnings
}

fn maybe_add_temp_root(
    setting_name: &str,
    parent: &Path,
    suffix: &str,
    accepted_canonical_paths: &mut Vec<PathBuf>,
    roots: &mut Vec<AbsolutePathBuf>,
    warnings: &mut Vec<String>,
) {
    if !parent.is_absolute() {
        warnings.push(format!(
            "{setting_name} was enabled, but the temp parent is not an absolute path: {}; no writable subdirectory was added.",
            parent.display()
        ));
        return;
    }

    let target = parent.join(format!("{TEMP_SUBDIR_PREFIX}{suffix}"));
    match create_owner_only_dir(&target) {
        Ok(()) => {}
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {
            let Ok(canonical_target) = target.canonicalize() else {
                warnings.push(format!(
                    "{setting_name} was enabled, but the existing temp subdirectory could not be canonicalized: {}; no writable subdirectory was added.",
                    target.display()
                ));
                return;
            };
            if !accepted_canonical_paths
                .iter()
                .any(|accepted| accepted == &canonical_target)
            {
                warnings.push(format!(
                    "{setting_name} was enabled, but the generated temp subdirectory already exists: {}; no writable subdirectory was added.",
                    target.display()
                ));
                return;
            }
        }
        Err(err) => {
            warnings.push(format!(
                "{setting_name} was enabled, but the temp subdirectory could not be created under {}: {err}; no writable subdirectory was added.",
                parent.display()
            ));
            return;
        }
    }

    let canonical_root = match target.canonicalize() {
        Ok(root) => root,
        Err(err) => {
            warnings.push(format!(
                "{setting_name} was enabled, but the temp subdirectory could not be canonicalized: {}: {err}; no writable subdirectory was added.",
                target.display()
            ));
            return;
        }
    };

    let absolute_root = match AbsolutePathBuf::from_absolute_path(&canonical_root) {
        Ok(root) => root,
        Err(err) => {
            warnings.push(format!(
                "{setting_name} was enabled, but the temp subdirectory was not accepted as an absolute path: {}: {err}; no writable subdirectory was added.",
                canonical_root.display()
            ));
            return;
        }
    };

    if !accepted_canonical_paths
        .iter()
        .any(|accepted| accepted == &canonical_root)
    {
        accepted_canonical_paths.push(canonical_root);
    }
    if !roots.iter().any(|root| root == &absolute_root) {
        roots.push(absolute_root);
    }
}

fn create_owner_only_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn read_only_policy() -> Constrained<SandboxPolicy> {
        Constrained::allow_any(SandboxPolicy::new_read_only_policy())
    }

    #[test]
    fn materializes_requested_temp_roots() {
        let slash_tmp = TempDir::new().expect("slash tmp parent");
        let tmpdir = TempDir::new().expect("tmpdir parent");
        let mut policy = read_only_policy();

        let warnings = materialize_read_only_temp_writable_roots_with_parents(
            &mut policy,
            &SandboxReadOnlyConfig {
                writeable_slash_tmp_subdir: true,
                writeable_tmpdir_env_var_subdir: true,
            },
            slash_tmp.path(),
            Some(tmpdir.path()),
        );

        assert_eq!(warnings, Vec::<String>::new());
        let SandboxPolicy::ReadOnly {
            temp_writable_roots,
        } = policy.get()
        else {
            panic!("expected read-only policy");
        };
        assert_eq!(temp_writable_roots.len(), 2);

        let slash_tmp_parent = slash_tmp
            .path()
            .canonicalize()
            .expect("slash tmp canonical");
        let tmpdir_parent = tmpdir.path().canonicalize().expect("tmpdir canonical");
        let slash_tmp_root = temp_writable_roots
            .iter()
            .find(|root| root.as_path().starts_with(&slash_tmp_parent))
            .expect("slash tmp root");
        let tmpdir_root = temp_writable_roots
            .iter()
            .find(|root| root.as_path().starts_with(&tmpdir_parent))
            .expect("tmpdir root");

        for root in [slash_tmp_root, tmpdir_root] {
            let path = root.as_path();
            assert!(path.is_dir(), "{} should exist", path.display());
            assert!(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(TEMP_SUBDIR_PREFIX)),
                "{} should use the read-only temp prefix",
                path.display()
            );
            #[cfg(unix)]
            assert_eq!(
                path.metadata().expect("metadata").permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn deduplicates_canonical_temp_roots() {
        let temp_parent = TempDir::new().expect("temp parent");
        let mut policy = read_only_policy();

        let warnings = materialize_read_only_temp_writable_roots_with_parents(
            &mut policy,
            &SandboxReadOnlyConfig {
                writeable_slash_tmp_subdir: true,
                writeable_tmpdir_env_var_subdir: true,
            },
            temp_parent.path(),
            Some(temp_parent.path()),
        );

        assert_eq!(warnings, Vec::<String>::new());
        let SandboxPolicy::ReadOnly {
            temp_writable_roots,
        } = policy.get()
        else {
            panic!("expected read-only policy");
        };
        assert_eq!(temp_writable_roots.len(), 1);
    }

    #[test]
    fn relative_tmpdir_fails_closed() {
        let slash_tmp = TempDir::new().expect("slash tmp parent");
        let mut policy = read_only_policy();

        let warnings = materialize_read_only_temp_writable_roots_with_parents(
            &mut policy,
            &SandboxReadOnlyConfig {
                writeable_slash_tmp_subdir: false,
                writeable_tmpdir_env_var_subdir: true,
            },
            slash_tmp.path(),
            Some(Path::new("relative/tmp")),
        );

        assert_eq!(
            warnings,
            vec![
                "sandbox_read_only.writeable_tmpdir_env_var_subdir was enabled, but the temp parent is not an absolute path: relative/tmp; no writable subdirectory was added."
                    .to_string()
            ]
        );
        assert_eq!(policy.get(), &SandboxPolicy::new_read_only_policy());
    }

    #[test]
    fn missing_tmpdir_fails_closed() {
        let slash_tmp = TempDir::new().expect("slash tmp parent");
        let mut policy = read_only_policy();

        let warnings = materialize_read_only_temp_writable_roots_with_parents(
            &mut policy,
            &SandboxReadOnlyConfig {
                writeable_slash_tmp_subdir: false,
                writeable_tmpdir_env_var_subdir: true,
            },
            slash_tmp.path(),
            None,
        );

        assert_eq!(
            warnings,
            vec![
                "sandbox_read_only.writeable_tmpdir_env_var_subdir was enabled, but TMPDIR is not set; no TMPDIR writable subdirectory was added."
                    .to_string()
            ]
        );
        assert_eq!(policy.get(), &SandboxPolicy::new_read_only_policy());
    }
}
