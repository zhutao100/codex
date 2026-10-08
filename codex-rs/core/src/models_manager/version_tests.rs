use super::client_version_to_whole;
use super::version_to_whole;
use pretty_assertions::assert_eq;

#[test]
fn uses_compile_time_version() {
    let version = option_env!("CODEX_VERSION_OVERRIDE").unwrap_or(env!("CARGO_PKG_VERSION"));
    assert_eq!(client_version_to_whole(), version_to_whole(version));
}

#[test]
fn strips_prerelease_and_build_metadata() {
    for (version, expected) in [
        ("0.0.0", "0.0.0"),
        ("0.98.1", "0.98.1"),
        ("1.2.3-alpha.4", "1.2.3"),
        ("1.2.3+build.5", "1.2.3"),
        ("1.2.3-alpha.4+build.5", "1.2.3"),
        ("1.2.3+build-with-hyphens", "1.2.3"),
    ] {
        assert_eq!(version_to_whole(version), expected, "{version}");
    }
}
