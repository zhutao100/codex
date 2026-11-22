use std::path::Path;

use anyhow::Result;
use predicates::str::contains;
use tempfile::TempDir;

fn codex_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut cmd = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    cmd.env("CODEX_HOME", codex_home);
    Ok(cmd)
}

#[tokio::test]
async fn features_enable_writes_feature_flag_to_config() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["features", "enable", "unified_exec"])
        .assert()
        .success()
        .stdout(contains("Enabled feature `unified_exec` in config.toml."));

    let config = std::fs::read_to_string(codex_home.path().join("config.toml"))?;
    assert!(config.contains("[features]"));
    assert!(config.contains("unified_exec = true"));

    Ok(())
}

#[tokio::test]
async fn features_disable_writes_feature_flag_to_config() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["features", "disable", "shell_tool"])
        .assert()
        .success()
        .stdout(contains("Disabled feature `shell_tool` in config.toml."));

    let config = std::fs::read_to_string(codex_home.path().join("config.toml"))?;
    assert!(config.contains("[features]"));
    assert!(config.contains("shell_tool = false"));

    Ok(())
}

#[test]
fn superseded_controls_report_the_owner_without_modifying_config() -> Result<()> {
    let home = TempDir::new()?;
    let path = home.path().join("config.toml");
    let original =
        "web_search = 'disabled'\n[agents]\nenabled = false\n[windows]\nsandbox = 'elevated'\n";
    std::fs::write(&path, original)?;
    for (action, key, owner) in [
        ("enable", "collab", "[agents].enabled"),
        ("disable", "elevated_windows_sandbox", "[windows].sandbox"),
        ("enable", "web_search_request", "Use `web_search`"),
        ("disable", "use_linux_sandbox_bwrap", "Remove this setting"),
    ] {
        codex_command(home.path())?
            .args(["features", action, key])
            .assert()
            .failure()
            .stderr(contains(owner));
        pretty_assertions::assert_eq!(std::fs::read_to_string(&path)?, original);
    }
    Ok(())
}

#[tokio::test]
async fn features_enable_under_development_feature_prints_warning() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["features", "enable", "output_token_replay"])
        .assert()
        .success()
        .stderr(contains(
            "Under-development features enabled: output_token_replay.",
        ));

    Ok(())
}
