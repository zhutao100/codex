use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use crate::config::types::McpServerConfig;
use crate::config::types::McpServerTransportConfig;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Manifest {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    skills: Option<SkillPaths>,
    mcp_servers: Option<Value>,
    apps: Option<Value>,
    hooks: Option<Value>,
    commands: Option<Value>,
    #[serde(default)]
    interface: Interface,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Interface {
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SkillPaths {
    One(String),
    Many(Vec<String>),
}

pub(super) fn validate_name(name: &str, kind: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && !name.starts_with('.')
            && !name.ends_with('.')
            && !name.contains("..")
            && name.bytes().all(|b| b.is_ascii_alphanumeric()
                || matches!(b, b'-' | b'_')
                || (kind == "plugin" && b == b'.')),
        "invalid {kind} name `{name}`"
    );
    Ok(())
}

/// Resolve package input paths without allowing them to leave the package.
pub(super) fn contained_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|part| { matches!(part, Component::CurDir | Component::Normal(_)) }),
        "expected a relative package path, got {}",
        relative.display()
    );
    let path = dunce::canonicalize(root.join(relative))
        .with_context(|| format!("cannot resolve {}", root.join(relative).display()))?;
    ensure!(
        path.starts_with(root),
        "package path escapes its root: {}",
        relative.display()
    );
    Ok(path)
}

impl Manifest {
    pub(super) fn load(root: &Path) -> Result<Self> {
        let path = contained_path(root, Path::new(".codex-plugin/plugin.json"))?;
        let manifest: Self = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("invalid manifest {}", path.display()))?;
        validate_name(&manifest.name, "plugin")?;
        ensure!(
            manifest.apps.is_none()
                && manifest.hooks.is_none()
                && manifest.commands.is_none()
                && !root.join(".app.json").exists()
                && !root.join("hooks.json").exists()
                && !root.join("hooks/hooks.json").exists(),
            "bundled apps, hooks, and legacy commands are not supported by this backport"
        );
        Ok(manifest)
    }

    pub(super) fn display_name(&self) -> String {
        self.interface
            .display_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&self.name)
            .to_string()
    }

    pub(super) fn skill_roots(&self, root: &Path) -> Result<Vec<PathBuf>> {
        let paths = match &self.skills {
            Some(SkillPaths::One(path)) => vec![path.as_str()],
            Some(SkillPaths::Many(paths)) => paths.iter().map(String::as_str).collect(),
            None if root.join("skills").exists() => vec!["./skills"],
            None => Vec::new(),
        };
        paths
            .into_iter()
            .map(|path| {
                ensure!(path.starts_with("./"), "skill paths must start with `./`");
                let path = contained_path(root, Path::new(path))?;
                ensure!(path.is_dir(), "skill roots must be directories");
                Ok(path)
            })
            .collect()
    }

    pub(super) fn mcp_servers(&self, root: &Path) -> Result<BTreeMap<String, McpServerConfig>> {
        let declaration = match &self.mcp_servers {
            Some(value) => value.clone(),
            None if root.join(".mcp.json").exists() => Value::String("./.mcp.json".into()),
            None => return Ok(BTreeMap::new()),
        };
        let mut value = match declaration {
            Value::String(path) => {
                ensure!(path.starts_with("./"), "MCP paths must start with `./`");
                serde_json::from_slice(&fs::read(contained_path(root, Path::new(&path))?)?)?
            }
            Value::Object(_) => declaration,
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => {
                bail!("mcpServers must be an object or a relative JSON file path")
            }
        };
        expand_root(&mut value, &root.to_string_lossy());
        let servers = value.get("mcpServers").unwrap_or(&value);
        let servers = servers
            .as_object()
            .context("MCP configuration must be an object")?;
        servers
            .iter()
            .map(|(name, value)| {
                validate_name(name, "MCP server")?;
                let mut value = value.clone();
                let object = value
                    .as_object_mut()
                    .context("MCP server must be an object")?;
                if let Some(transport) = object.remove("type") {
                    ensure!(
                        transport == "stdio" || transport == "http",
                        "unsupported MCP transport for `{name}`; use stdio or http"
                    );
                    ensure!(
                        (transport == "stdio" && object.contains_key("command"))
                            || (transport == "http" && object.contains_key("url")),
                        "MCP transport does not match the configuration for `{name}`"
                    );
                }
                if let Some(headers) = object.remove("headers") {
                    ensure!(
                        !object.contains_key("http_headers"),
                        "use only one of headers and http_headers"
                    );
                    object.insert("http_headers".into(), headers);
                }
                let mut config: McpServerConfig = serde_json::from_value(value)
                    .with_context(|| format!("invalid MCP server `{name}`"))?;
                if let McpServerTransportConfig::Stdio { cwd, .. } = &mut config.transport {
                    *cwd = Some(match cwd.take() {
                        Some(path) if path.is_absolute() => path,
                        Some(path) => contained_path(root, &path)?,
                        None => root.to_path_buf(),
                    });
                }
                Ok((name.clone(), config))
            })
            .collect()
    }
}

fn expand_root(value: &mut Value, root: &str) {
    match value {
        Value::String(text) => {
            *text = text
                .replace("${CLAUDE_PLUGIN_ROOT}", root)
                .replace("${CODEX_PLUGIN_ROOT}", root);
        }
        Value::Array(values) => values.iter_mut().for_each(|value| expand_root(value, root)),
        Value::Object(values) => values
            .values_mut()
            .for_each(|value| expand_root(value, root)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}
