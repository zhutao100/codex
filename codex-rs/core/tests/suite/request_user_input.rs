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
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::StreamingSseServer;
use core_test_support::streaming_sse::start_streaming_sse_server;
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

fn select_mode(test: &TestCodex, mode: ModeKind) -> Op {
    Op::OverrideTurnContext {
        collaboration_mode: Some(CollaborationMode {
            mode,
            settings: Settings {
                model: test.session_configured.model.clone(),
                reasoning_effort: None,
                developer_instructions: None,
            },
        }),
        cwd: None,
        approval_policy: None,
        sandbox_policy: None,
        windows_sandbox_level: None,
        model: None,
        effort: None,
        summary: None,
        personality: None,
        service_tier: None,
    }
}

fn input(text: &str) -> Op {
    Op::UserInput {
        items: vec![UserInput::Text {
            text: text.into(),
            text_elements: vec![],
        }],
        final_output_json_schema: None,
    }
}

fn ask_fixture() -> String {
    let args = json!({"questions":[{"id":"confirm","header":"Confirm","question":"Proceed?","options":[{"label":"Yes","description":"Proceed"},{"label":"No","description":"Stop"}]}]});
    sse(vec![
        ev_response_created("ask"),
        ev_function_call("mode-call", "request_user_input", &args.to_string()),
        ev_completed("ask"),
    ])
}

fn done_fixture() -> String {
    sse(vec![
        ev_assistant_message("answer", "done"),
        ev_completed("done"),
    ])
}

async fn wait_for_sample(server: &StreamingSseServer) -> anyhow::Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while server.requests().await.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await?;
    Ok(())
}

async fn answer_fixture(test: &TestCodex) -> anyhow::Result<()> {
    let request = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::RequestUserInput(request) => Some(request.clone()),
        _ => None,
    })
    .await;
    assert_eq!(request.call_id, "mode-call");
    test.codex
        .submit(Op::UserInputAnswer {
            id: request.turn_id,
            response: RequestUserInputResponse {
                answers: HashMap::from([(
                    "confirm".into(),
                    RequestUserInputAnswer {
                        answers: vec!["Yes".into()],
                    },
                )]),
            },
        })
        .await?;
    Ok(())
}

#[test_case::test_case(ModeKind::Plan, ModeKind::Default; "plan_to_default")]
#[test_case::test_case(ModeKind::Default, ModeKind::Plan; "default_to_plan")]
#[tokio::test]
async fn in_flight_question_uses_the_sampled_turn(
    initial: ModeKind,
    next: ModeKind,
) -> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let (release, gate) = tokio::sync::oneshot::channel();
    let (server, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: Some(gate),
            body: ask_fixture(),
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: done_fixture(),
        }],
    ])
    .await;
    let test = test_codex().build_with_streaming_server(&server).await?;
    test.codex.submit(select_mode(&test, initial)).await?;
    test.codex.submit(input("start turn")).await?;
    wait_for_sample(&server).await?;
    test.codex.submit(select_mode(&test, next)).await?;
    // Submission is asynchronous: observe a later operation before releasing the model.
    test.codex.submit(Op::ListCustomPrompts).await?;
    wait_for_event(&test.codex, |ev| {
        matches!(ev, EventMsg::ListCustomPromptsResponse(_))
    })
    .await;
    release.send(()).unwrap();
    if initial == ModeKind::Plan {
        answer_fixture(&test).await?;
    }
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;
    let requests = server.requests().await;
    assert_eq!(requests.len(), 2);
    let body: Value = serde_json::from_slice(&requests[1])?;
    let output = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|it| it["call_id"] == "mode-call" && it["type"] == "function_call_output")
        .unwrap()["output"]
        .as_str()
        .unwrap();
    if initial == ModeKind::Plan {
        assert_eq!(
            serde_json::from_str::<Value>(output)?,
            json!({"answers":{"confirm":{"answers":["Yes"]}}})
        );
    } else {
        assert_eq!(output, "request_user_input is unavailable in Default mode");
    }
    server.shutdown().await;
    Ok(())
}

#[test_case::test_case(false; "next_user_turn")]
#[test_case::test_case(true; "continue_paused_turn")]
#[tokio::test]
async fn restart_restores_the_durable_plan_mode(paused: bool) -> anyhow::Result<()> {
    skip_if_no_network_unless_localhost!(Ok(()));
    let (release, gate) = tokio::sync::oneshot::channel();
    let (initial, _) = start_streaming_sse_server(vec![vec![StreamingSseChunk {
        gate: Some(gate),
        body: done_fixture(),
    }]])
    .await;
    let mut builder = test_codex();
    let original = builder.build_with_streaming_server(&initial).await?;
    original
        .codex
        .submit(select_mode(&original, ModeKind::Plan))
        .await?;
    original.codex.submit(input("start in Plan")).await?;
    wait_for_sample(&initial).await?;
    if paused {
        original.codex.submit(Op::Pause).await?;
        wait_for_event(&original.codex, |ev| matches!(ev, EventMsg::TurnPaused(_))).await;
        let _ = release.send(());
    } else {
        release.send(()).unwrap();
        wait_for_event(&original.codex, |ev| {
            matches!(ev, EventMsg::TurnComplete(_))
        })
        .await;
    }
    original.codex.submit(Op::Shutdown).await?;
    wait_for_event(&original.codex, |ev| {
        matches!(ev, EventMsg::ShutdownComplete)
    })
    .await;
    initial.shutdown().await;
    let server = start_mock_server().await;
    let captured =
        responses::mount_sse_sequence(&server, vec![ask_fixture(), done_fixture()]).await;
    let resumed = builder
        .resume(
            &server,
            original.home.clone(),
            original.session_configured.rollout_path.unwrap(),
        )
        .await?;
    resumed
        .codex
        .submit(if paused {
            Op::Continue
        } else {
            input("continue in the saved mode")
        })
        .await?;
    let mode = wait_for_event_match(&resumed.codex, |ev| match ev {
        EventMsg::TurnStarted(start) => Some(start.collaboration_mode_kind),
        _ => None,
    })
    .await;
    assert_eq!(mode, ModeKind::Plan);
    answer_fixture(&resumed).await?;
    wait_for_event(&resumed.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;
    assert_eq!(
        serde_json::from_str::<Value>(&call_output(&captured.requests()[1], "mode-call"))?,
        json!({"answers":{"confirm":{"answers":["Yes"]}}})
    );
    Ok(())
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
