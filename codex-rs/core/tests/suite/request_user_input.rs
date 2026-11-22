#![allow(clippy::unwrap_used)]

use std::collections::HashMap;

use codex_core::protocol::AskForApproval;
use codex_core::protocol::EventMsg;
use codex_core::protocol::Op;
use codex_core::protocol::SandboxPolicy;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::Settings;
use codex_protocol::request_user_input::RequestUserInputAnswer;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network_unless_localhost;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

fn call_output(req: &ResponsesRequest, call_id: &str) -> String {
    let raw = req.function_call_output(call_id);
    assert_eq!(
        raw.get("call_id").and_then(Value::as_str),
        Some(call_id),
        "mismatched call_id in function_call_output"
    );
    let (content_opt, _success) = match req.function_call_output_content_and_success(call_id) {
        Some(values) => values,
        None => panic!("function_call_output present"),
    };
    match content_opt {
        Some(content) => content,
        None => panic!("function_call_output content present"),
    }
}

fn call_output_content_and_success(
    req: &ResponsesRequest,
    call_id: &str,
) -> (String, Option<bool>) {
    let raw = req.function_call_output(call_id);
    assert_eq!(
        raw.get("call_id").and_then(Value::as_str),
        Some(call_id),
        "mismatched call_id in function_call_output"
    );
    let (content_opt, success) = match req.function_call_output_content_and_success(call_id) {
        Some(values) => values,
        None => panic!("function_call_output present"),
    };
    let content = match content_opt {
        Some(content) => content,
        None => panic!("function_call_output content present"),
    };
    (content, success)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_user_input_round_trip_resolves_pending() -> anyhow::Result<()> {
    request_user_input_round_trip_for_mode(ModeKind::Plan).await
}

async fn request_user_input_round_trip_for_mode(mode: ModeKind) -> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));

    let server = start_mock_server().await;

    let test = test_codex().build(&server).await?;
    let TestCodex {
        codex,
        cwd,
        session_configured,
        ..
    } = &test;

    let call_id = "user-input-call";
    let request_args = json!({
        "questions": [{
            "id": "confirm_path",
            "header": "Confirm",
            "question": "Proceed with the plan?",
            "options": [{
                "label": "Yes (Recommended)",
                "description": "Continue the current plan."
            }, {
                "label": "No",
                "description": "Stop and revisit the approach."
            }]
        }]
    })
    .to_string();

    let first_response = sse(vec![
        ev_response_created("resp-1"),
        ev_function_call(call_id, "request_user_input", &request_args),
        ev_completed("resp-1"),
    ]);
    let first = responses::mount_sse_once(&server, first_response).await;

    let second_response = sse(vec![
        ev_assistant_message("msg-1", "thanks"),
        ev_completed("resp-2"),
    ]);
    let second_mock = responses::mount_sse_once(&server, second_response).await;

    let session_model = session_configured.model.clone();

    codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "please confirm".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: cwd.path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: session_model,
            effort: None,
            summary: ReasoningSummary::Auto,
            collaboration_mode: Some(CollaborationMode {
                mode,
                settings: Settings {
                    model: session_configured.model.clone(),
                    reasoning_effort: None,
                    developer_instructions: None,
                },
            }),
            personality: None,
            service_tier: None,
        })
        .await?;

    let request = wait_for_event_match(codex, |event| match event {
        EventMsg::RequestUserInput(request) => Some(request.clone()),
        _ => None,
    })
    .await;
    assert_eq!(request.call_id, call_id);
    assert_eq!(request.questions.len(), 1);
    assert_eq!(request.questions[0].is_other, true);

    let mut answers = HashMap::new();
    answers.insert(
        "confirm_path".to_string(),
        RequestUserInputAnswer {
            answers: vec!["yes".to_string()],
        },
    );
    let response = RequestUserInputResponse { answers };
    codex
        .submit(Op::UserInputAnswer {
            id: request.turn_id.clone(),
            response,
        })
        .await?;

    wait_for_event(codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let req = second_mock.single_request();
    assert!(
        first.single_request().body_json()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "request_user_input")
    );
    let output_text = call_output(&req, call_id);
    let output_json: Value = serde_json::from_str(&output_text)?;
    assert_eq!(
        output_json,
        json!({
            "answers": {
                "confirm_path": { "answers": ["yes"] }
            }
        })
    );

    Ok(())
}

async fn assert_request_user_input_rejected(wire_mode: &str) -> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));

    let server = start_mock_server().await;

    let test = test_codex().build(&server).await?;
    let TestCodex {
        codex,
        cwd,
        session_configured,
        ..
    } = &test;

    let call_id = format!("user-input-{wire_mode}-call");
    let request_args = json!({
        "questions": [{
            "id": "confirm_path",
            "header": "Confirm",
            "question": "Proceed with the plan?",
            "options": [{
                "label": "Yes (Recommended)",
                "description": "Continue the current plan."
            }, {
                "label": "No",
                "description": "Stop and revisit the approach."
            }]
        }]
    })
    .to_string();

    let first_response = sse(vec![
        ev_response_created("resp-1"),
        ev_function_call(&call_id, "request_user_input", &request_args),
        ev_completed("resp-1"),
    ]);
    let first = responses::mount_sse_once(&server, first_response).await;

    let second_response = sse(vec![
        ev_assistant_message("msg-1", "thanks"),
        ev_completed("resp-2"),
    ]);
    let second_mock = responses::mount_sse_once(&server, second_response).await;

    let session_model = session_configured.model.clone();
    let collaboration_mode = CollaborationMode {
        mode: serde_json::from_value(json!(wire_mode))?,
        settings: Settings {
            model: session_model.clone(),
            reasoning_effort: None,
            developer_instructions: None,
        },
    };
    assert_eq!(collaboration_mode.mode, ModeKind::Default);

    codex
        .submit(Op::UserTurn {
            items: vec![UserInput::Text {
                text: "please confirm".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            cwd: cwd.path().to_path_buf(),
            approval_policy: AskForApproval::Never,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            model: session_model,
            effort: None,
            summary: ReasoningSummary::Auto,
            collaboration_mode: Some(collaboration_mode),
            personality: None,
            service_tier: None,
        })
        .await?;

    wait_for_event(codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let req = second_mock.single_request();
    assert!(
        first.single_request().body_json()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "request_user_input")
    );
    let (output, success) = call_output_content_and_success(&req, &call_id);
    assert_eq!(success, None);
    assert_eq!(output, "request_user_input is unavailable in Default mode");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_user_input_rejected_in_execute_mode_alias() -> anyhow::Result<()> {
    assert_request_user_input_rejected("execute").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_user_input_rejected_in_default_mode() -> anyhow::Result<()> {
    assert_request_user_input_rejected("default").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_user_input_rejected_in_pair_mode_alias() -> anyhow::Result<()> {
    assert_request_user_input_rejected("pair_programming").await
}
