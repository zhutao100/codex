use super::*;
use crate::config::ConfigBuilder;
use crate::config::ConfigOverrides;
use crate::config::edit::ConfigEdit;
use crate::config::edit::ConfigEditsBuilder;
use crate::skills::SkillsManager;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::fs;
use tempfile::TempDir;

struct Fixture {
    home: TempDir,
    source: TempDir,
    cwd: TempDir,
    store: PluginStore,
}

impl Fixture {
    fn new() -> Result<Self> {
        let home = tempfile::tempdir()?;
        let source = tempfile::tempdir()?;
        let cwd = tempfile::tempdir()?;
        let store = PluginStore::new(home.path().to_path_buf());
        fs::create_dir_all(source.path().join(".agents/plugins"))?;
        fs::create_dir_all(source.path().join(".codex-plugin"))?;
        fs::create_dir_all(source.path().join("skills/fetch"))?;
        fs::create_dir_all(source.path().join("bin"))?;
        fs::write(
            source.path().join(".agents/plugins/marketplace.json"),
            serde_json::to_vec(&json!({
                "name": "local-test",
                "plugins": [{
                    "name": "sample",
                    "source": { "source": "local", "path": "." },
                    "policy": { "installation": "AVAILABLE", "authentication": "ON_INSTALL" }
                }]
            }))?,
        )?;
        fs::write(
            source.path().join(".codex-plugin/plugin.json"),
            serde_json::to_vec(&json!({
                "name": "sample", "version": "1.0.0", "skills": "./skills",
                "description": "Plugin fixture",
                "interface": { "displayName": "Sample Plugin" }
            }))?,
        )?;
        fs::write(
            source.path().join("skills/fetch/SKILL.md"),
            "---\nname: fetch\ndescription: Fetch a page\n---\nUse the bundled program.\n",
        )?;
        fs::write(
            source.path().join("bin/fetch"),
            "#!/bin/sh\nprintf plugin-result\n",
        )?;
        Ok(Self {
            home,
            source,
            cwd,
            store,
        })
    }

    fn install(&self) -> Result<PluginSummary> {
        self.store.add_marketplace(self.source.path())?;
        self.store.install("sample@local-test")
    }

    async fn config(&self) -> Result<crate::config::Config> {
        Ok(ConfigBuilder::default()
            .codex_home(self.home.path().to_path_buf())
            .harness_overrides(ConfigOverrides {
                cwd: Some(self.cwd.path().to_path_buf()),
                ..Default::default()
            })
            .build()
            .await?)
    }
}

#[tokio::test]
async fn local_bundle_lifecycle_preserves_config_and_namespaces_skills() -> Result<()> {
    let fixture = Fixture::new()?;
    fs::write(
        fixture.home.path().join("config.toml"),
        "# keep this\nmodel = 'test-model'\n",
    )?;
    let installed = fixture.install()?;
    assert_eq!(
        installed.clone(),
        PluginSummary {
            id: "sample@local-test".into(),
            name: "sample".into(),
            display_name: "Sample Plugin".into(),
            version: Some("1.0.0".into()),
            description: Some("Plugin fixture".into()),
            enabled: true,
            installed_path: dunce::canonicalize(
                fixture
                    .home
                    .path()
                    .join("plugins/cache/local-test/sample/local")
            )?,
            error: None,
        }
    );
    assert_eq!(fixture.store.list()?, vec![installed.clone()]);
    assert_eq!(
        fs::read(installed.installed_path.join("bin/fetch"))?,
        fs::read(fixture.source.path().join("bin/fetch"))?
    );
    let config = fixture.config().await?;
    let manager = SkillsManager::new(fixture.home.path().into());
    let loaded = manager.skills_for_config(&config);
    let skills = loaded
        .skills
        .iter()
        .filter(|s| s.name == "sample:fetch")
        .collect::<Vec<_>>();
    assert_eq!(skills.len(), 1);
    assert_eq!(
        skills[0].path,
        installed.installed_path.join("skills/fetch/SKILL.md")
    );
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    assert!(fixture.store.remove_marketplace("local-test").is_err());

    fixture.store.set_enabled("sample@local-test", false)?;
    let disabled =
        SkillsManager::new(fixture.home.path().into()).skills_for_config(&fixture.config().await?);
    assert!(!disabled.skills.iter().any(|s| s.name == "sample:fetch"));
    fixture.store.set_enabled("sample@local-test", true)?;
    let mut reloaded = manager.skills_for_cwd(fixture.cwd.path(), true).await;
    reloaded.skills.retain(|s| s.name == "sample:fetch");
    assert_eq!(
        reloaded.skills,
        skills.into_iter().cloned().collect::<Vec<_>>()
    );

    ConfigEditsBuilder::new(fixture.home.path())
        .with_edits([ConfigEdit::SetSkillConfig {
            path: installed.installed_path.join("skills/fetch/SKILL.md"),
            enabled: false,
        }])
        .apply_blocking()?;
    let disabled_skill = manager.skills_for_cwd(fixture.cwd.path(), true).await;
    assert!(
        !disabled_skill
            .enabled_skills()
            .iter()
            .any(|s| s.name == "sample:fetch")
    );

    fixture.store.remove("sample@local-test")?;
    fixture.store.remove_marketplace("local-test")?;
    assert_eq!(fixture.store.list()?, Vec::new());
    assert!(!installed.installed_path.exists());
    assert!(fixture.source.path().join("bin/fetch").exists());
    let contents = fs::read_to_string(fixture.home.path().join("config.toml"))?;
    assert!(contents.contains("# keep this"));
    assert_eq!(fixture.store.config()?.model, Some("test-model".into()));
    Ok(())
}

#[test]
fn reinstall_replaces_bundle_and_invalid_source_leaves_installation_unchanged() -> Result<()> {
    let fixture = Fixture::new()?;
    let installed = fixture.install()?;
    fs::write(fixture.source.path().join("bin/fetch"), "updated program")?;
    fixture.install()?;
    assert_eq!(
        fs::read_to_string(installed.installed_path.join("bin/fetch"))?,
        "updated program"
    );
    fs::write(
        fixture.source.path().join(".codex-plugin/plugin.json"),
        r#"{"name":"sample","skills":"../outside"}"#,
    )?;
    assert!(fixture.install().is_err());
    assert_eq!(
        fs::read_to_string(installed.installed_path.join("bin/fetch"))?,
        "updated program"
    );
    assert!(fixture.store.config()?.plugins["sample@local-test"].enabled);
    Ok(())
}

#[test]
fn rejects_invalid_ids_sources_and_unsupported_capabilities() -> Result<()> {
    let fixture = Fixture::new()?;
    for id in [
        "../sample@local-test",
        "sample@../test",
        "sample",
        "sample@a@b",
        ".@test",
    ] {
        assert!(fixture.store.install(id).is_err(), "{id}");
    }
    for declaration in [
        json!({"name":"sample","hooks":{}}),
        json!({"name":"sample","apps":"./apps.json"}),
        json!({"name":"sample","skills":"/tmp"}),
        json!({"name":"sample","skills":"./missing"}),
        json!({"name":"other","skills":"./skills"}),
        json!({"name":"sample","mcpServers":{"bad":{"type":"sse","url":"http://localhost"}}}),
    ] {
        fs::write(
            fixture.source.path().join(".codex-plugin/plugin.json"),
            serde_json::to_vec(&declaration)?,
        )?;
        assert!(fixture.install().is_err(), "{declaration}");
        assert!(fixture.store.config()?.plugins.is_empty());
    }
    fs::write(
        fixture
            .source
            .path()
            .join(".agents/plugins/marketplace.json"),
        r#"{"name":"local-test","plugins":[{"name":"sample","source":{"source":"git","path":"."}}]}"#,
    )?;
    assert!(fixture.store.install("sample@local-test").is_err());
    Ok(())
}

#[cfg(unix)]
#[test]
fn copies_executable_modes_and_internal_symlinks_but_rejects_external_links() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new()?;
    fs::set_permissions(
        fixture.source.path().join("bin/fetch"),
        fs::Permissions::from_mode(0o755),
    )?;
    symlink("fetch", fixture.source.path().join("bin/alias"))?;
    let installed = fixture.install()?;
    assert_eq!(
        fs::read_link(installed.installed_path.join("bin/alias"))?,
        PathBuf::from("fetch")
    );
    assert_eq!(
        fs::metadata(installed.installed_path.join("bin/fetch"))?
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    symlink(fixture.cwd.path(), fixture.source.path().join("outside"))?;
    assert!(fixture.install().is_err());
    assert!(installed.installed_path.join("bin/fetch").exists());
    Ok(())
}

#[tokio::test]
async fn plugin_mcp_uses_package_root_and_respects_explicit_configuration_and_toggle() -> Result<()>
{
    let fixture = Fixture::new()?;
    fs::write(
        fixture.source.path().join(".mcp.json"),
        serde_json::to_vec(&json!({"mcpServers":{
            "bundled": {
                "type": "stdio", "command": "${CLAUDE_PLUGIN_ROOT}/bin/fetch",
                "args": ["${CODEX_PLUGIN_ROOT}/payload \"with quotes\""],
                "env": { "PACKAGE": "${CODEX_PLUGIN_ROOT}" }
            },
            "explicit": { "command": "ignored" },
            "remote": { "type": "http", "url": "http://127.0.0.1/mcp", "headers": {"X-Test":"yes"} }
        }}))?,
    )?;
    fs::write(
        fixture.home.path().join("config.toml"),
        "[mcp_servers.explicit]\ncommand = 'configured'\nenabled = false\n",
    )?;
    let installed = fixture.install()?;
    let config = fixture.config().await?;
    let root = installed.installed_path.display();
    let expected: McpServerConfig = serde_json::from_value(json!({
        "command": format!("{root}/bin/fetch"),
        "args": [format!("{root}/payload \"with quotes\"")],
        "env": {"PACKAGE": root.to_string()},
        "cwd": root.to_string()
    }))?;
    assert_eq!(config.mcp_servers.get().get("bundled"), Some(&expected));
    assert!(!config.mcp_servers.get()["explicit"].enabled);
    let remote: McpServerConfig = serde_json::from_value(json!({
        "url": "http://127.0.0.1/mcp", "http_headers": {"X-Test":"yes"}
    }))?;
    assert_eq!(config.mcp_servers.get().get("remote"), Some(&remote));
    fixture.store.set_enabled("sample@local-test", false)?;
    let config = fixture.config().await?;
    assert_eq!(
        config.mcp_servers.get().keys().collect::<Vec<_>>(),
        vec!["explicit"]
    );
    Ok(())
}

#[tokio::test]
async fn invalid_installed_plugin_is_reported_without_hiding_other_skills() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.install()?;
    ConfigEditsBuilder::new(fixture.home.path())
        .with_edits([ConfigEdit::SetPath {
            segments: vec![
                "plugins".into(),
                "missing@local-test".into(),
                "enabled".into(),
            ],
            value: toml_edit::value(true),
        }])
        .apply_blocking()?;
    let loaded =
        SkillsManager::new(fixture.home.path().into()).skills_for_config(&fixture.config().await?);
    assert!(loaded.skills.iter().any(|s| s.name == "sample:fetch"));
    assert_eq!(loaded.errors.len(), 1);
    assert!(loaded.errors[0].message.contains("missing@local-test"));
    assert!(fixture.store.list()?[0].error.is_some());
    Ok(())
}
