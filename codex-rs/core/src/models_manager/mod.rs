pub mod cache;
pub mod collaboration_mode_presets;
pub mod manager;
pub mod model_info;
pub mod model_presets;
pub mod overlay;
pub mod refresh_worker;

#[cfg(any(test, feature = "test-support"))]
pub use collaboration_mode_presets::test_builtin_collaboration_mode_presets;

/// Return the compile-time client version without prerelease or build metadata.
pub fn client_version_to_whole() -> String {
    version_to_whole(crate::CODEX_VERSION)
}

fn version_to_whole(version: &str) -> String {
    version
        .split_once(['-', '+'])
        .map_or(version, |(whole, _)| whole)
        .to_string()
}

#[cfg(test)]
mod version_tests;
