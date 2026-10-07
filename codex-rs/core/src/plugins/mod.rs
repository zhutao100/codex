//! Local compatibility-format plugins, using the existing skills and MCP runtimes.

mod manifest;
mod store;

pub use store::PluginStore;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use crate::config::types::McpServerConfig;
use crate::config_loader::ConfigLayerStack;
use crate::skills::SkillError;
use crate::skills::SkillLoadOutcome;
use crate::skills::loader::SkillRoot;
use crate::skills::loader::load_skills_from_roots;
use codex_protocol::protocol::SkillScope;
use manifest::Manifest;
use manifest::validate_name;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginConfig {
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MarketplaceConfig {
    pub source_type: MarketplaceSourceType,
    pub source: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MarketplaceSourceType {
    Local,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PluginSummary {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub enabled: bool,
    pub installed_path: PathBuf,
    pub error: Option<String>,
}

#[derive(Debug)]
struct PluginId {
    name: String,
    marketplace: String,
}

impl PluginId {
    fn parse(id: &str) -> Result<Self> {
        let (name, marketplace) = id
            .split_once('@')
            .context("expected a plugin identifier: PLUGIN@MARKETPLACE")?;
        validate_name(name, "plugin")?;
        validate_name(marketplace, "marketplace")?;
        Ok(Self {
            name: name.into(),
            marketplace: marketplace.into(),
        })
    }

    fn root(&self, home: &Path) -> PathBuf {
        home.join("plugins/cache")
            .join(&self.marketplace)
            .join(&self.name)
            .join("local")
    }
}

fn load_manifest(home: &Path, id: &PluginId) -> Result<(PathBuf, Manifest)> {
    let root = dunce::canonicalize(id.root(home)).context("plugin is not installed")?;
    let manifest = Manifest::load(&root)?;
    ensure!(
        manifest.name == id.name,
        "installed plugin name does not match its identifier"
    );
    Ok((root, manifest))
}

pub fn list_plugins(home: &Path, plugins: &BTreeMap<String, PluginConfig>) -> Vec<PluginSummary> {
    plugins
        .iter()
        .map(|(key, config)| {
            let mut summary = PluginSummary {
                id: key.clone(),
                name: key.clone(),
                display_name: key.clone(),
                version: None,
                description: None,
                enabled: config.enabled,
                installed_path: PathBuf::new(),
                error: None,
            };
            let result = PluginId::parse(key).and_then(|id| {
                summary.name = id.name.clone();
                summary.installed_path = id.root(home);
                load_manifest(home, &id)
            });
            match result {
                Ok((root, manifest)) => {
                    summary.error = manifest
                        .skill_roots(&root)
                        .and_then(|_| manifest.mcp_servers(&root))
                        .err()
                        .map(|err| format!("{err:#}"));
                    summary.installed_path = root;
                    summary.display_name = manifest.display_name();
                    summary.version = manifest.version;
                    summary.description = manifest.description;
                }
                Err(err) => summary.error = Some(format!("{err:#}")),
            }
            summary
        })
        .collect()
}

pub fn plugins_from_stack(stack: &ConfigLayerStack) -> Result<BTreeMap<String, PluginConfig>> {
    stack
        .effective_config()
        .get("plugins")
        .cloned()
        .map(toml::Value::try_into)
        .transpose()
        .context("invalid plugins configuration")
        .map(Option::unwrap_or_default)
}

pub(crate) fn append_skills(outcome: &mut SkillLoadOutcome, home: &Path, stack: &ConfigLayerStack) {
    let plugins = match plugins_from_stack(stack) {
        Ok(plugins) => plugins,
        Err(err) => {
            outcome.errors.push(SkillError {
                path: home.join("config.toml"),
                message: err.to_string(),
            });
            return;
        }
    };
    for (key, config) in plugins {
        if !config.enabled {
            continue;
        }
        let result = (|| -> Result<SkillLoadOutcome> {
            let id = PluginId::parse(&key)?;
            let (root, manifest) = load_manifest(home, &id)?;
            let mut loaded =
                load_skills_from_roots(manifest.skill_roots(&root)?.into_iter().map(|path| {
                    SkillRoot {
                        path,
                        scope: SkillScope::User,
                    }
                }));
            for skill in &mut loaded.skills {
                ensure!(
                    skill.path.starts_with(&root),
                    "plugin skill escapes its package"
                );
                skill.name = format!("{}:{}", id.name, skill.name);
            }
            Ok(loaded)
        })();
        match result {
            Ok(loaded) => {
                outcome.skills.extend(loaded.skills);
                outcome.errors.extend(loaded.errors);
            }
            Err(err) => outcome.errors.push(SkillError {
                path: home.join("plugins/cache"),
                message: format!("plugin `{key}`: {err:#}"),
            }),
        }
    }
}

/// Explicit MCP configuration wins; plugin servers still pass through requirements filtering.
pub(crate) fn merge_mcp_servers(
    servers: &mut HashMap<String, McpServerConfig>,
    home: &Path,
    plugins: &BTreeMap<String, PluginConfig>,
) {
    for (key, config) in plugins {
        if !config.enabled {
            continue;
        }
        let result = PluginId::parse(key)
            .and_then(|id| load_manifest(home, &id))
            .and_then(|(root, manifest)| manifest.mcp_servers(&root));
        match result {
            Ok(plugin_servers) => {
                for (name, server) in plugin_servers {
                    match servers.entry(name) {
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            entry.insert(server);
                        }
                        std::collections::hash_map::Entry::Occupied(entry) => {
                            tracing::warn!(
                                plugin = key,
                                server = entry.key(),
                                "ignoring duplicate plugin MCP server"
                            );
                        }
                    }
                }
            }
            Err(err) => tracing::warn!(plugin = key, "cannot load plugin MCP servers: {err:#}"),
        }
    }
}

#[cfg(test)]
mod tests;
