use anyhow::Result;
use codex_core::ModelProviderInfo;
use codex_core::WireApi;
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
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_completed;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;

fn responses_provider(name: &str, base_url: String) -> ModelProviderInfo {
    ModelProviderInfo {
        name: name.to_string(),
        base_url: Some(base_url),
        env_key: None,
        env_key_instructions: None,
        experimental_bearer_token: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        request_max_retries: Some(0),
        stream_max_retries: Some(0),
        stream_idle_timeout_ms: Some(5_000),
        requires_openai_auth: false,
        supports_websockets: false,
    }
}

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
async fn overlay_model_provider_routes_initial_model_to_provider() -> Result<()> {
    let primary_server = start_mock_server().await;
    let secondary_server = start_mock_server().await;
    let secondary_mock =
        mount_sse_once(&secondary_server, sse_completed("secondary-initial")).await;

    let custom_model = "deepseek-v4-pro";
    let secondary_base_url = format!("{}/v1", secondary_server.uri());
    let mut builder = test_codex()
        .with_model(custom_model)
        .with_config(move |config| {
            config.model_providers.insert(
                "deepseek".to_string(),
                responses_provider("DeepSeek", secondary_base_url),
            );
            config.model_overlay = Some(ModelOverlay {
                models: vec![ModelOverlayEntry {
                    slug: custom_model.to_string(),
                    model_provider: Some("deepseek".to_string()),
                    patch: ModelInfoPatch {
                        display_name: Some("DeepSeek V4 Pro".to_string()),
                        visibility: Some(codex_protocol::openai_models::ModelVisibility::List),
                        ..Default::default()
                    },
                    final_instruction_override: None,
                }],
                ..Default::default()
            });
        });
    let test = builder.build(&primary_server).await?;
    assert_eq!(test.session_configured.model, custom_model);
    assert_eq!(test.session_configured.model_provider_id, "deepseek");

    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "custom initial turn".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: test.cwd_path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            model: custom_model.to_string(),
            effort: test.config.model_reasoning_effort,
            summary: ReasoningSummary::Auto,
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let secondary_request = secondary_mock.single_request();
    assert_eq!(
        secondary_request.body_json()["model"].as_str(),
        Some(custom_model)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlay_model_provider_routes_model_switch_between_providers() -> Result<()> {
    let primary_server = start_mock_server().await;
    let secondary_server = start_mock_server().await;
    let primary_mock = mount_sse_sequence(
        &primary_server,
        vec![sse_completed("primary-1"), sse_completed("primary-2")],
    )
    .await;
    let secondary_mock = mount_sse_once(&secondary_server, sse_completed("secondary-1")).await;

    let custom_model = "deepseek-v4-pro";
    let secondary_base_url = format!("{}/v1", secondary_server.uri());
    let mut builder = test_codex()
        .with_model("gpt-4.1")
        .with_config(move |config| {
            config.model_providers.insert(
                "deepseek".to_string(),
                responses_provider("DeepSeek", secondary_base_url),
            );
            config.model_overlay = Some(ModelOverlay {
                models: vec![ModelOverlayEntry {
                    slug: custom_model.to_string(),
                    model_provider: Some("deepseek".to_string()),
                    patch: ModelInfoPatch {
                        display_name: Some("DeepSeek V4 Pro".to_string()),
                        visibility: Some(codex_protocol::openai_models::ModelVisibility::List),
                        ..Default::default()
                    },
                    final_instruction_override: None,
                }],
                ..Default::default()
            });
        });
    let test = builder.build(&primary_server).await?;
    assert_eq!(test.session_configured.model, "gpt-4.1");
    assert_eq!(test.session_configured.model_provider_id, "openai");

    test.codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "first primary turn".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: test.cwd_path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            model: "gpt-4.1".to_string(),
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
            model: Some(custom_model.to_string()),
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
                text: "custom provider turn".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: test.cwd_path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            model: custom_model.to_string(),
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
            model: Some("gpt-4.1".to_string()),
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
                text: "second primary turn".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: test.cwd_path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::new_read_only_policy(),
            model: "gpt-4.1".to_string(),
            effort: test.config.model_reasoning_effort,
            summary: ReasoningSummary::Auto,
            collaboration_mode: None,
            personality: None,
            service_tier: None,
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let primary_requests = primary_mock.requests();
    assert_eq!(primary_requests.len(), 2);
    assert_eq!(
        primary_requests[0].body_json()["model"].as_str(),
        Some("gpt-4.1")
    );
    assert_eq!(
        primary_requests[1].body_json()["model"].as_str(),
        Some("gpt-4.1")
    );

    let secondary_request = secondary_mock.single_request();
    assert_eq!(
        secondary_request.body_json()["model"].as_str(),
        Some(custom_model)
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
                        model_provider: None,
                        patch: ModelInfoPatch {
                            context_window: Some(Some(20_000)),
                            auto_compact_token_limit: Some(Some(18_000)),
                            ..Default::default()
                        },
                        final_instruction_override: None,
                    },
                    ModelOverlayEntry {
                        slug: small_model.to_string(),
                        model_provider: None,
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
