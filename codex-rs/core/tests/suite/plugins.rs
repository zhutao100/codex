use anyhow::Result;
use codex_core::features::Feature;
use codex_core::plugins::PluginStore;
use core_test_support::responses;
use core_test_support::skip_if_no_network_unless_localhost;
use core_test_support::test_codex::test_codex;
use std::fs;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_plugin_skill_reaches_catalog_and_explicit_turn_instructions() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let home = Arc::new(tempfile::tempdir()?);
    let source = tempfile::tempdir()?;
    fs::create_dir_all(source.path().join(".codex-plugin"))?;
    fs::create_dir_all(source.path().join(".agents/plugins"))?;
    fs::create_dir_all(source.path().join("skills/fetch"))?;
    fs::write(
        source.path().join(".codex-plugin/plugin.json"),
        r#"{"name":"browser","skills":"./skills"}"#,
    )?;
    fs::write(
        source.path().join(".agents/plugins/marketplace.json"),
        r#"{"name":"test","plugins":[{"name":"browser","source":{"source":"local","path":"."}}]}"#,
    )?;
    fs::write(
        source.path().join("skills/fetch/SKILL.md"),
        "---\nname: fetch\ndescription: Fetch an authenticated page\n---\nBUNDLED_FETCH_INSTRUCTIONS\n",
    )?;
    let store = PluginStore::new(home.path().into());
    store.add_marketplace(source.path())?;
    let plugin = store.install("browser@test")?;

    let server = responses::start_mock_server().await;
    let mut builder = test_codex().with_home(home).with_config(|config| {
        config.features.disable(Feature::RemoteCompactionV2);
    });
    let test = builder.build(&server).await?;
    let mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("plugin-response"),
            responses::ev_assistant_message("plugin-message", "done"),
            responses::ev_completed("plugin-response"),
        ]),
    )
    .await;
    test.submit_turn("Use $browser:fetch.").await?;
    let request = mock.single_request();
    let messages = request.message_input_texts("user");
    assert!(messages.iter().any(|text| {
        text.contains("browser:fetch") && text.contains("Fetch an authenticated page")
    }));
    assert!(messages.iter().any(|text| {
        text.contains("<skill>\n<name>browser:fetch</name>")
            && text.contains("BUNDLED_FETCH_INSTRUCTIONS")
            && text.contains(
                plugin
                    .installed_path
                    .join("skills/fetch/SKILL.md")
                    .to_string_lossy()
                    .as_ref(),
            )
    }));
    Ok(())
}
