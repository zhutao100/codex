use anyhow::Context;
use anyhow::Result;
use codex_core::features::Feature;
use codex_core::models_manager::overlay::ModelInfoPatch;
use codex_core::models_manager::overlay::ModelOverlay;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::openai_models::ApplyPatchToolType;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::InputModality;
use core_test_support::responses;
use core_test_support::skip_if_no_network_unless_localhost;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_selects_expected_tools() -> Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    use ApplyPatchToolType::Freeform;
    use ApplyPatchToolType::Function;
    use ConfigShellToolType::Default as DefaultShell;
    use ConfigShellToolType::Disabled;
    use ConfigShellToolType::Local;
    use ConfigShellToolType::ShellCommand;
    use ConfigShellToolType::UnifiedExec;

    for (shell, patch, shell_tools, patch_wire_type) in [
        (Local, None, &["local_shell"][..], None),
        (DefaultShell, None, &["shell"], None),
        (
            ShellCommand,
            Some(Function),
            &["shell_command"],
            Some("function"),
        ),
        (
            UnifiedExec,
            Some(Freeform),
            &["exec_command", "write_stdin"],
            Some("custom"),
        ),
        (Disabled, None, &[], None),
    ] {
        let server = responses::start_mock_server().await;
        let mock = responses::mount_sse_once(
            &server,
            responses::sse(vec![
                responses::ev_response_created("turn"),
                responses::ev_completed("turn"),
            ]),
        )
        .await;
        let test = test_codex()
            .with_model("fixture-model-tools")
            .with_config(move |config| {
                // Exercise the model's shell choice, not the host's unified-exec override.
                config.features.disable(Feature::UnifiedExec);
                config.web_search_mode = Some(WebSearchMode::Cached);
                config.model_overlay = Some(ModelOverlay {
                    cross_model: ModelInfoPatch {
                        shell_type: Some(shell),
                        apply_patch_tool_type: Some(patch),
                        input_modalities: Some(vec![InputModality::Text, InputModality::Image]),
                        ..Default::default()
                    },
                    ..Default::default()
                });
            })
            .build(&server)
            .await?;
        test.submit_turn("hello tools").await?;

        let body = mock.single_request().body_json();
        let tools = body["tools"].as_array().context("request tools")?;
        let names: Vec<_> = tools
            .iter()
            .map(|tool| tool["name"].as_str().or_else(|| tool["type"].as_str()))
            .collect::<Option<_>>()
            .context("tool identifier")?;
        let mut expected = shell_tools.to_vec();
        expected.extend(["update_plan", "request_user_input"]);
        if patch_wire_type.is_some() {
            expected.push("apply_patch");
        }
        expected.extend(["web_search", "view_image"]);
        assert_eq!(names, expected);
        assert_eq!(
            tools
                .iter()
                .find(|tool| tool["name"] == "apply_patch")
                .and_then(|tool| tool["type"].as_str()),
            patch_wire_type
        );
    }
    Ok(())
}
