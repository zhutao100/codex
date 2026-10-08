pub mod cache;
mod catalog;
pub mod collaboration_mode_presets;
pub(crate) mod config;
pub mod manager;
pub mod model_info;
pub mod model_presets;
mod originator;
pub mod test_support;

pub use catalog::CatalogModel;
pub use catalog::ModelOrigin;
pub use codex_protocol::auth::AuthMode;
pub use config::ModelsManagerConfig;
pub use originator::ModelCatalogOriginator;

/// Load the bundled model catalog shipped with `codex-models-manager`.
pub fn bundled_models_response()
-> std::result::Result<codex_protocol::openai_models::ModelsResponse, serde_json::Error> {
    serde_json::from_str(include_str!("../models.json"))
}

/// Return the compile-time client version without prerelease or build metadata.
pub fn client_version_to_whole() -> String {
    version_to_whole(option_env!("CODEX_VERSION_OVERRIDE").unwrap_or(env!("CARGO_PKG_VERSION")))
}

fn version_to_whole(version: &str) -> String {
    version
        .split_once(['-', '+'])
        .map_or(version, |(whole, _)| whole)
        .to_string()
}

#[cfg(test)]
mod version_tests;
