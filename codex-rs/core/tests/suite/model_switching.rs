use anyhow::Result;
use codex_core::compact::SUMMARIZATION_PROMPT;
use codex_core::config::types::Personality;
use codex_core::features::Feature;
use codex_core::models_manager::overlay::ModelInfoPatch;
use codex_core::models_manager::overlay::ModelOverlay;
use codex_core::models_manager::overlay::ModelOverlayEntry;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_core::protocol::SandboxPolicy;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::mount_compact_json_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_completed;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_change_appends_model_instructions_developer_message() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let resp_mock = mount_sse_sequence(
        &server,
        vec![sse_completed("resp-1"), sse_completed("resp-2")],
    )
    .await;

    let mut builder = test_codex().with_model("gpt-5.2-codex");
    let test = builder.build(&server).await?;
    let next_model = "gpt-5.1-codex-max";

    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: test.cwd_path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            model: test.session_configured.model.clone(),
            effort: test.config.model_reasoning_effort,
            summary: ReasoningSummary::Auto,
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    test.codex
        .submit(Op::OverrideTurnContext {
            cwd: None,
            approval_policy: None,
            sandbox_policy: None,
            windows_sandbox_level: None,
            model: Some(next_model.to_string()),
            effort: None,
            summary: None,
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;

    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "switch models".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: test.cwd_path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            model: next_model.to_string(),
            effort: test.config.model_reasoning_effort,
            summary: ReasoningSummary::Auto,
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let requests = resp_mock.requests();
    assert_eq!(requests.len(), 2, "expected two model requests");

    let second_request = requests.last().expect("expected second request");
    let developer_texts = second_request.message_input_texts("developer");
    let model_switch_text = developer_texts
        .iter()
        .find(|text| text.contains("<model_switch>"))
        .expect("expected model switch message in developer input");
    assert!(
        model_switch_text.contains("The user was previously using a different model."),
        "expected model switch preamble, got: {model_switch_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_downshift_uses_previous_model_for_pre_sampling_compaction() -> Result<()> {
    let server = start_mock_server().await;
    let resp_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("msg-1", "large model reply"),
                ev_completed_with_tokens("resp-1", 10_000),
            ]),
            sse(vec![
                ev_assistant_message("msg-3", "small model reply"),
                ev_completed_with_tokens("resp-3", 10),
            ]),
        ],
    )
    .await;
    let compact_mock = mount_compact_json_once(
        &server,
        serde_json::json!({
            "output": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "downshift summary"}]
                }
            ]
        }),
    )
    .await;

    let large_model = "large-context-model";
    let small_model = "small-context-model";
    let mut builder = test_codex()
        .with_model(large_model)
        .with_config(move |config| {
            config.compact_prompt = Some(SUMMARIZATION_PROMPT.to_string());
            config.model_overlay = Some(ModelOverlay {
                models: vec![
                    ModelOverlayEntry {
                        slug: large_model.to_string(),
                        patch: ModelInfoPatch {
                            context_window: Some(Some(20_000)),
                            auto_compact_token_limit: Some(Some(18_000)),
                            ..Default::default()
                        },
                        final_instruction_override: None,
                    },
                    ModelOverlayEntry {
                        slug: small_model.to_string(),
                        patch: ModelInfoPatch {
                            context_window: Some(Some(12_000)),
                            auto_compact_token_limit: Some(Some(9_000)),
                            ..Default::default()
                        },
                        final_instruction_override: None,
                    },
                ],
                ..Default::default()
            });
        });
    let test = builder.build(&server).await?;

    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "large model turn".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: test.cwd_path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            model: large_model.to_string(),
            effort: test.config.model_reasoning_effort,
            summary: ReasoningSummary::Auto,
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    test.codex
        .submit(Op::OverrideTurnContext {
            cwd: None,
            approval_policy: None,
            sandbox_policy: None,
            windows_sandbox_level: None,
            model: Some(small_model.to_string()),
            effort: None,
            summary: None,
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;

    let downshift_prompt = "small model turn";
    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: downshift_prompt.into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: test.cwd_path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            model: small_model.to_string(),
            effort: test.config.model_reasoning_effort,
            summary: ReasoningSummary::Auto,
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;
    wait_for_event(&test.codex, |ev| {
        matches!(ev, EventMsg::AgentMessage(message) if message.message == "small model reply")
    })
    .await;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let requests = resp_mock.requests();
    assert_eq!(
        requests.len(),
        2,
        "expected requests before and after downshift compaction"
    );
    assert_eq!(
        requests[0]
            .body_json()
            .get("model")
            .and_then(|v| v.as_str()),
        Some(large_model)
    );
    assert_eq!(
        compact_mock
            .single_request()
            .body_json()
            .get("model")
            .and_then(|v| v.as_str()),
        Some(large_model),
        "downshift compaction should use the previous larger model"
    );
    let compact_body = compact_mock.single_request().body_json();
    assert!(
        compact_body.to_string().contains("large model turn"),
        "expected previous-model compaction request"
    );
    assert!(
        !compact_body.to_string().contains(downshift_prompt),
        "compaction should run before current-turn user input is recorded"
    );
    assert_eq!(
        requests[1]
            .body_json()
            .get("model")
            .and_then(|v| v.as_str()),
        Some(small_model)
    );
    assert!(
        requests[1]
            .message_input_texts("user")
            .iter()
            .any(|text| text == downshift_prompt),
        "current turn should sample with the smaller model after compaction"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_and_personality_change_only_appends_model_instructions() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let resp_mock = mount_sse_sequence(
        &server,
        vec![sse_completed("resp-1"), sse_completed("resp-2")],
    )
    .await;

    let mut builder = test_codex()
        .with_model("gpt-5.2-codex")
        .with_config(|config| {
            config.features.enable(Feature::Personality);
        });
    let test = builder.build(&server).await?;
    let next_model = "exp-codex-personality";

    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: test.cwd_path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            model: test.session_configured.model.clone(),
            effort: test.config.model_reasoning_effort,
            summary: ReasoningSummary::Auto,
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    test.codex
        .submit(Op::OverrideTurnContext {
            cwd: None,
            approval_policy: None,
            sandbox_policy: None,
            windows_sandbox_level: None,
            model: Some(next_model.to_string()),
            effort: None,
            summary: None,
            collaboration_mode: None,
            personality: Some(Personality::Pragmatic),
            service_tier: None,
        })
        .await?;

    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "switch model and personality".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: test.cwd_path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            model: next_model.to_string(),
            effort: test.config.model_reasoning_effort,
            summary: ReasoningSummary::Auto,
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let requests = resp_mock.requests();
    assert_eq!(requests.len(), 2, "expected two model requests");

    let second_request = requests.last().expect("expected second request");
    let developer_texts = second_request.message_input_texts("developer");
    assert!(
        developer_texts
            .iter()
            .any(|text| text.contains("<model_switch>")),
        "expected model switch message when model changes"
    );
    assert!(
        !developer_texts
            .iter()
            .any(|text| text.contains("<personality_spec>")),
        "did not expect personality update message when model changed in same turn"
    );

    Ok(())
}
