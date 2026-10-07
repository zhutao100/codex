use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::path::Path;
use std::path::PathBuf;

use super::MarketplaceConfig;
use super::MarketplaceSourceType;
use super::PluginId;
use super::PluginSummary;
use super::list_plugins;
use super::load_manifest;
use super::manifest::Manifest;
use super::manifest::contained_path;
use super::manifest::validate_name;
use crate::config::ConfigToml;
use crate::config::edit::ConfigEdit;
use crate::config::edit::ConfigEditsBuilder;

const MARKETPLACE_MANIFEST: &str = ".agents/plugins/marketplace.json";

#[derive(Deserialize)]
struct Marketplace {
    name: String,
    plugins: Vec<MarketplacePlugin>,
}

#[derive(Deserialize)]
struct MarketplacePlugin {
    name: String,
    source: MarketplaceSource,
    #[serde(default)]
    policy: MarketplacePolicy,
}

#[derive(Deserialize)]
struct MarketplaceSource {
    source: String,
    path: Option<PathBuf>,
}

#[derive(Default, Deserialize)]
struct MarketplacePolicy {
    installation: Option<String>,
}

fn load_marketplace(root: &Path) -> Result<Marketplace> {
    let path = contained_path(root, Path::new(MARKETPLACE_MANIFEST))?;
    let marketplace: Marketplace =
        serde_json::from_slice(&fs::read(path)?).context("invalid marketplace.json")?;
    validate_name(&marketplace.name, "marketplace")?;
    let mut seen = HashSet::new();
    for plugin in &marketplace.plugins {
        validate_name(&plugin.name, "plugin")?;
        ensure!(
            seen.insert(&plugin.name),
            "duplicate marketplace plugin `{}`",
            plugin.name
        );
    }
    Ok(marketplace)
}

/// Mutations are serialized across CLI processes and preserve unrelated config entries.
pub struct PluginStore {
    home: PathBuf,
}

impl PluginStore {
    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }

    pub fn config(&self) -> Result<ConfigToml> {
        match fs::read_to_string(self.home.join("config.toml")) {
            Ok(contents) => toml::from_str(&contents).context("invalid config.toml"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ConfigToml::default()),
            Err(err) => Err(err.into()),
        }
    }

    fn lock(&self) -> Result<File> {
        fs::create_dir_all(self.home.join("plugins"))?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.home.join("plugins/.lock"))?;
        file.lock()?;
        Ok(file)
    }

    fn edit(&self, edits: impl IntoIterator<Item = ConfigEdit>) -> Result<()> {
        ConfigEditsBuilder::new(&self.home)
            .with_edits(edits)
            .apply_blocking()
    }

    pub fn list(&self) -> Result<Vec<PluginSummary>> {
        let _lock = self.lock()?;
        Ok(list_plugins(&self.home, &self.config()?.plugins))
    }

    pub fn marketplaces(&self) -> Result<BTreeMap<String, MarketplaceConfig>> {
        Ok(self.config()?.marketplaces)
    }

    pub fn add_marketplace(&self, source: &Path) -> Result<String> {
        let root = dunce::canonicalize(source).context("cannot resolve marketplace directory")?;
        ensure!(
            root.is_dir(),
            "marketplace source must be a local directory"
        );
        let marketplace = load_marketplace(&root)?;
        let _lock = self.lock()?;
        let config = self.config()?;
        if let Some(existing) = config.marketplaces.get(&marketplace.name) {
            ensure!(
                existing.source == root,
                "marketplace `{}` already refers to another directory; remove it first",
                marketplace.name
            );
        }
        self.edit([
            set_path(&["marketplaces", &marketplace.name, "source_type"], "local"),
            set_path(
                &["marketplaces", &marketplace.name, "source"],
                root.to_str().context("marketplace path is not UTF-8")?,
            ),
        ])?;
        Ok(marketplace.name)
    }

    pub fn remove_marketplace(&self, name: &str) -> Result<()> {
        validate_name(name, "marketplace")?;
        let _lock = self.lock()?;
        let config = self.config()?;
        ensure!(
            config.marketplaces.contains_key(name),
            "marketplace `{name}` is not configured"
        );
        ensure!(
            !config
                .plugins
                .keys()
                .any(|key| { PluginId::parse(key).is_ok_and(|id| id.marketplace == name) }),
            "remove installed plugins from `{name}` before removing the marketplace"
        );
        self.edit([clear_path(&["marketplaces", name])])
    }

    pub fn install(&self, key: &str) -> Result<PluginSummary> {
        let id = PluginId::parse(key)?;
        let _lock = self.lock()?;
        let config = self.config()?;
        let marketplace_config = config
            .marketplaces
            .get(&id.marketplace)
            .context("marketplace is not configured; run `codex plugin marketplace add PATH`")?;
        let MarketplaceSourceType::Local = marketplace_config.source_type;
        let marketplace_root = dunce::canonicalize(&marketplace_config.source)?;
        let marketplace = load_marketplace(&marketplace_root)?;
        ensure!(
            marketplace.name == id.marketplace,
            "marketplace name changed"
        );
        let entry = marketplace
            .plugins
            .iter()
            .find(|plugin| plugin.name == id.name)
            .context("plugin is not listed in this marketplace")?;
        ensure!(
            entry
                .policy
                .installation
                .as_deref()
                .is_none_or(|policy| { matches!(policy, "AVAILABLE" | "INSTALLED_BY_DEFAULT") }),
            "plugin is not available for installation"
        );
        ensure!(
            entry.source.source == "local",
            "only local plugin sources are supported"
        );
        let source = contained_path(
            &marketplace_root,
            entry
                .source
                .path
                .as_deref()
                .context("local plugin source requires a path")?,
        )?;
        let manifest = Manifest::load(&source)?;
        ensure!(
            manifest.name == id.name,
            "plugin manifest name does not match marketplace entry"
        );
        // Validate declarations before copying or changing the active installation.
        manifest.skill_roots(&source)?;
        manifest.mcp_servers(&source)?;
        let target = id.root(&self.home);
        fs::create_dir_all(target.parent().context("missing plugin parent")?)?;
        let parent = dunce::canonicalize(target.parent().context("missing plugin parent")?)?;
        ensure!(
            !parent.starts_with(&source),
            "plugin source contains the installation directory"
        );
        let staging = tempfile::tempdir_in(&parent)?;
        let staged = staging.path().join("bundle");
        copy_package(&source, &source, &staged)?;
        let copied = Manifest::load(&staged)?;
        copied.skill_roots(&staged)?;
        copied.mcp_servers(&staged)?;

        let previous = staging.path().join("previous");
        let had_previous = target.try_exists()?;
        if had_previous {
            fs::rename(&target, &previous)?;
        }
        let publish = (|| -> Result<()> {
            fs::rename(&staged, &target)?;
            self.edit([set_enabled(key, true)])
        })();
        if let Err(err) = publish {
            if target.exists() {
                fs::remove_dir_all(&target)?;
            }
            if had_previous {
                fs::rename(previous, &target)?;
            }
            return Err(err);
        }
        let mut summaries = list_plugins(
            &self.home,
            &BTreeMap::from([(key.into(), super::PluginConfig { enabled: true })]),
        );
        summaries.pop().context("installed plugin is missing")
    }

    pub fn set_enabled(&self, key: &str, enabled: bool) -> Result<()> {
        let id = PluginId::parse(key)?;
        let _lock = self.lock()?;
        ensure!(
            self.config()?.plugins.contains_key(key),
            "plugin `{key}` is not configured"
        );
        if enabled {
            let (root, manifest) = load_manifest(&self.home, &id)?;
            manifest.skill_roots(&root)?;
            manifest.mcp_servers(&root)?;
        }
        self.edit([set_enabled(key, enabled)])
    }

    pub fn remove(&self, key: &str) -> Result<()> {
        let id = PluginId::parse(key)?;
        let _lock = self.lock()?;
        ensure!(
            self.config()?.plugins.contains_key(key),
            "plugin `{key}` is not configured"
        );
        // Deactivate first, so a failed cache cleanup never leaves a configured missing bundle.
        self.edit([clear_path(&["plugins", key])])?;
        match fs::remove_dir_all(id.root(&self.home)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).context("plugin deactivated, but cache cleanup failed"),
        }
    }
}

fn set_path(path: &[&str], value: &str) -> ConfigEdit {
    ConfigEdit::SetPath {
        segments: path.iter().map(ToString::to_string).collect(),
        value: toml_edit::value(value),
    }
}

fn set_enabled(key: &str, enabled: bool) -> ConfigEdit {
    ConfigEdit::SetPath {
        segments: vec!["plugins".into(), key.into(), "enabled".into()],
        value: toml_edit::value(enabled),
    }
}

fn clear_path(path: &[&str]) -> ConfigEdit {
    ConfigEdit::ClearPath {
        segments: path.iter().map(ToString::to_string).collect(),
    }
}

fn copy_package(root: &Path, source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" || (source == root && (name == ".build" || name == "target")) {
            continue;
        }
        let path = entry.path();
        let target = destination.join(name);
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            let link = fs::read_link(&path)?;
            ensure!(
                !link.is_absolute(),
                "absolute package symlink: {}",
                path.display()
            );
            ensure!(
                dunce::canonicalize(&path)?.starts_with(root),
                "package symlink escapes its root: {}",
                path.display()
            );
            #[cfg(unix)]
            std::os::unix::fs::symlink(link, target)?;
            #[cfg(windows)]
            if path.is_dir() {
                std::os::windows::fs::symlink_dir(link, target)?;
            } else {
                std::os::windows::fs::symlink_file(link, target)?;
            }
        } else if file_type.is_dir() {
            copy_package(root, &path, &target)?;
        } else if file_type.is_file() {
            fs::copy(path, target)?;
        } else {
            bail!("unsupported package file type: {}", path.display());
        }
    }
    fs::set_permissions(destination, fs::metadata(source)?.permissions())?;
    Ok(())
}
