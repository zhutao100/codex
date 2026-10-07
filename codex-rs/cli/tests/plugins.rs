use anyhow::Result;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::fs;
use std::path::Path;

fn command(home: &Path) -> Result<assert_cmd::Command> {
    let mut command = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    command.env("CODEX_HOME", home);
    Ok(command)
}

#[test]
fn plugin_cli_install_list_toggle_remove() -> Result<()> {
    let home = tempfile::tempdir()?;
    let source = tempfile::tempdir()?;
    fs::create_dir_all(source.path().join(".agents/plugins"))?;
    fs::create_dir_all(source.path().join(".codex-plugin"))?;
    fs::create_dir_all(source.path().join("skills/demo"))?;
    fs::write(
        source.path().join(".agents/plugins/marketplace.json"),
        r#"{"name":"test","plugins":[{"name":"demo","source":{"source":"local","path":"."}}]}"#,
    )?;
    fs::write(
        source.path().join(".codex-plugin/plugin.json"),
        r#"{"name":"demo"}"#,
    )?;
    fs::write(
        source.path().join("skills/demo/SKILL.md"),
        "---\nname: demo\ndescription: CLI fixture\n---\nFixture body\n",
    )?;
    command(home.path())?
        .args(["plugin", "marketplace", "add"])
        .arg(source.path())
        .assert()
        .success();
    command(home.path())?
        .args(["plugin", "add", "demo", "--marketplace", "test"])
        .assert()
        .success();
    let listing = command(home.path())?
        .args(["plugin", "list", "--json"])
        .output()?;
    assert!(listing.status.success());
    let listing: Vec<Value> = serde_json::from_slice(&listing.stdout)?;
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0]["id"], "demo@test");
    let root = listing[0]["installed_path"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing root"))?;
    assert!(Path::new(root).join("skills/demo/SKILL.md").exists());
    for (action, enabled) in [("disable", false), ("enable", true)] {
        command(home.path())?
            .args(["plugin", action, "demo@test"])
            .assert()
            .success();
        let output = command(home.path())?
            .args(["plugin", "list", "--json"])
            .output()?;
        let plugins: Vec<Value> = serde_json::from_slice(&output.stdout)?;
        let mut expected = listing.clone();
        expected[0]["enabled"] = enabled.into();
        assert_eq!(plugins, expected);
    }
    command(home.path())?
        .args(["plugin", "remove", "demo@test"])
        .assert()
        .success();
    command(home.path())?
        .args(["plugin", "marketplace", "remove", "test"])
        .assert()
        .success();
    assert!(!Path::new(root).exists());
    let output = command(home.path())?
        .args(["plugin", "list", "--json"])
        .output()?;
    assert_eq!(
        serde_json::from_slice::<Vec<Value>>(&output.stdout)?,
        Vec::<Value>::new()
    );
    Ok(())
}

#[test]
fn plugin_cli_rejects_invalid_identifiers_and_conflicting_marketplaces() -> Result<()> {
    let home = tempfile::tempdir()?;
    for args in [
        vec!["plugin", "add", "../demo@test"],
        vec!["plugin", "add", "demo@test", "--marketplace", "other"],
        vec!["plugin", "remove", "demo"],
    ] {
        command(home.path())?.args(args).assert().failure();
    }
    Ok(())
}
