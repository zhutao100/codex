use anyhow::Result;
use codex_core::config::ConfigBuilder;
use codex_core::features::Feature;
use codex_core::plugins::PluginStore;
use codex_core::plugins::PluginSummary;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use core_test_support::responses;
use core_test_support::skip_if_no_network_unless_localhost;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use std::fs;
use std::sync::Arc;

fn install_browser_plugin() -> Result<(Arc<tempfile::TempDir>, PluginSummary)> {
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
    Ok((home, plugin))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_plugin_skill_reaches_catalog_and_explicit_turn_instructions() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let (home, plugin) = install_browser_plugin()?;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plugin_session_overrides_survive_forced_listing_and_instruction_injection() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    for enabled in [false, true] {
        let (home, _plugin) = install_browser_plugin()?;
        PluginStore::new(home.path().into()).set_enabled("browser@test", !enabled)?;
        let overridden = ConfigBuilder::default()
            .codex_home(home.path().into())
            .cli_overrides(vec![(
                "plugins.browser@test.enabled".into(),
                toml::Value::Boolean(enabled),
            )])
            .build()
            .await?;
        let server = responses::start_mock_server().await;
        let mut builder = test_codex().with_home(home).with_config(move |config| {
            config.config_layer_stack = overridden.config_layer_stack;
            config.features.disable(Feature::RemoteCompactionV2);
        });
        let test = builder.build(&server).await?;
        let mut listed = Vec::new();
        for force_reload in [true, false] {
            test.codex
                .submit(Op::ListSkills {
                    cwds: Vec::new(),
                    force_reload,
                })
                .await?;
            let response =
                core_test_support::wait_for_event_match(test.codex.as_ref(), |event| match event {
                    EventMsg::ListSkillsResponse(response) => Some(response.clone()),
                    _ => None,
                })
                .await;
            listed.push(response.skills.iter().any(|entry| {
                entry
                    .skills
                    .iter()
                    .any(|skill| skill.name == "browser:fetch")
            }));
        }
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
        let catalog = messages
            .iter()
            .any(|text| text.contains("- browser:fetch:"));
        let injected = messages.iter().any(|text| {
            text.contains("<name>browser:fetch</name>")
                && text.contains("BUNDLED_FETCH_INSTRUCTIONS")
        });
        assert_eq!(
            (listed, catalog, injected),
            (vec![enabled; 2], enabled, enabled)
        );
    }
    Ok(())
}
