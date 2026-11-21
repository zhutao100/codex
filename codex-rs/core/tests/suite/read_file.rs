#![cfg(not(target_os = "windows"))]

use codex_core::models_manager::overlay::ModelOverlay;
use core_test_support::responses::mount_function_call_agent_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network_unless_localhost;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_file_tool_returns_requested_lines() -> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(|config| {
            let mut overlay = ModelOverlay::default();
            overlay.cross_model.experimental_supported_tools = Some(vec!["read_file".to_string()]);
            config.model_overlay = Some(overlay);
        })
        .build(&server)
        .await?;

    let file_path = test.cwd.path().join("sample.txt");
    std::fs::write(&file_path, "first\nsecond\nthird\nfourth\n")?;
    let file_path = file_path.to_string_lossy().to_string();

    let call_id = "read-file-call";
    let arguments = json!({
        "file_path": file_path,
        "offset": 2,
        "limit": 2,
    })
    .to_string();

    let mocks = mount_function_call_agent_response(&server, call_id, &arguments, "read_file").await;

    test.submit_turn("please inspect sample.txt").await?;

    let req = mocks.completion.single_request();
    let (output_text_opt, _) = req
        .function_call_output_content_and_success(call_id)
        .expect("output present");
    let output_text = output_text_opt.expect("output text present");
    assert!(output_text.contains("\nFile revision: "));
    assert_eq!(
        output_text.lines().skip(2).collect::<Vec<_>>(),
        vec!["L2: second", "L3: third"]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indentation_inner_limit_reports_partial_coverage() -> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let server = start_mock_server().await;
    let test = test_codex()
        .with_config(|config| {
            let mut overlay = ModelOverlay::default();
            overlay.cross_model.experimental_supported_tools = Some(vec!["read_file".into()]);
            config.model_overlay = Some(overlay);
        })
        .build(&server)
        .await?;
    let file_path = test.cwd.path().join("block.py");
    std::fs::write(&file_path, "def block():\n    first()\n    second()\n")?;
    let mocks = mount_function_call_agent_response(
        &server,
        "read-block",
        &json!({
            "file_path": file_path,
            "mode": "indentation",
            "indentation": {"anchor_line": 2, "max_lines": 1},
        })
        .to_string(),
        "read_file",
    )
    .await;
    test.submit_turn("Read the anchor.").await?;
    let output = mocks
        .completion
        .single_request()
        .function_call_output_text("read-block")
        .expect("missing output");
    assert_eq!(
        output.lines().next(),
        Some(
            "Source display: 1 numbered lines. Indentation line limit reached; additional adjacent lines not checked."
        )
    );
    assert_eq!(output.lines().last(), Some("L2:     first()"));
    Ok(())
}
