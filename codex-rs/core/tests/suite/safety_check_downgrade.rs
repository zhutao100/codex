use anyhow::Result;
use codex_core::protocol::CodexErrorInfo;
use codex_core::protocol::EventMsg;
use codex_core::protocol::ModelRerouteReason;
use codex_core::protocol::ModelVerification;
use codex_core::protocol::Op;
use codex_core::protocol::TurnPauseReason;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once;
use core_test_support::responses::mount_response_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use wiremock::ResponseTemplate;

const REQUESTED_MODEL: &str = "gpt-5.3-codex";
const SERVER_MODEL: &str = "gpt-5.2";
const TRUSTED_ACCESS_FOR_CYBER_VERIFICATION: &str = "trusted_access_for_cyber";
const CYBER_POLICY_MESSAGE: &str =
    "This request has been flagged for potentially high-risk cyber activity.";

fn text_turn(text: &str) -> Op {
    Op::UserInput {
        items: vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }],
        final_output_json_schema: None,
    }
}

fn ev_model_verification_metadata(response_id: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "response.metadata",
        "response": {
            "id": response_id
        },
        "metadata": {
            "openai_verification_recommendation": [TRUSTED_ACCESS_FOR_CYBER_VERIFICATION]
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_model_mismatch_emits_general_warning_and_pauses() -> Result<()> {
    let server = start_mock_server().await;
    let first_response = sse_response(sse(vec![
        ev_response_created("resp-1"),
        ev_completed("resp-1"),
    ]))
    .insert_header("OpenAI-Model", SERVER_MODEL);
    let second_response = sse_response(sse(vec![
        ev_response_created("resp-2"),
        ev_assistant_message("msg-2", "done"),
        ev_completed("resp-2"),
    ]))
    .insert_header("OpenAI-Model", REQUESTED_MODEL);
    let mock = mount_response_sequence(&server, vec![first_response, second_response]).await;

    let mut builder = test_codex().with_model(REQUESTED_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(text_turn("trigger server model mismatch"))
        .await?;

    let reroute = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ModelReroute(_))
    })
    .await;
    let EventMsg::ModelReroute(reroute) = reroute else {
        panic!("expected model reroute event");
    };
    assert_eq!(reroute.from_model, REQUESTED_MODEL);
    assert_eq!(reroute.to_model, SERVER_MODEL);
    assert_eq!(
        reroute.reason,
        ModelRerouteReason::ServerSelectedDifferentModel
    );

    let warning = wait_for_event(&test.codex, |event| matches!(event, EventMsg::Warning(_))).await;
    let EventMsg::Warning(warning) = warning else {
        panic!("expected warning event");
    };
    assert!(warning.message.contains(REQUESTED_MODEL));
    assert!(warning.message.contains(SERVER_MODEL));
    assert!(warning.message.contains("several reasons"));
    assert!(warning.message.contains("high-risk cybersecurity activity"));
    assert!(warning.message.contains("/continue"));

    let paused = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnPaused(_))
    })
    .await;
    let EventMsg::TurnPaused(paused) = paused else {
        panic!("expected paused event");
    };
    assert_eq!(paused.reason, TurnPauseReason::ServerSelectedDifferentModel);

    test.codex.submit(Op::Continue).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body_json()["model"], REQUESTED_MODEL);
    assert_eq!(requests[1].body_json()["model"], REQUESTED_MODEL);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cyber_policy_response_emits_typed_error_without_retry() -> Result<()> {
    let server = start_mock_server().await;
    let response = ResponseTemplate::new(400).set_body_json(serde_json::json!({
        "error": {
            "message": CYBER_POLICY_MESSAGE,
            "type": "invalid_request",
            "param": null,
            "code": "cyber_policy"
        }
    }));
    let mock = mount_response_once(&server, response).await;

    let mut builder = test_codex().with_model(REQUESTED_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(text_turn("trigger cyber policy error"))
        .await?;

    let error = wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
    let EventMsg::Error(error) = error else {
        panic!("expected error event");
    };
    assert_eq!(error.message, CYBER_POLICY_MESSAGE);
    assert_eq!(error.codex_error_info, Some(CodexErrorInfo::CyberPolicy));
    mock.single_request();

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_verification_emits_structured_event_without_reroute() -> Result<()> {
    let server = start_mock_server().await;
    let response = sse_response(sse(vec![
        ev_response_created("resp-1"),
        ev_model_verification_metadata("resp-1"),
        ev_assistant_message("msg-1", "done"),
        ev_completed("resp-1"),
    ]));
    let _mock = mount_response_once(&server, response).await;

    let mut builder = test_codex().with_model(REQUESTED_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(text_turn("trigger model verification"))
        .await?;

    let verification = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ModelVerification(_))
    })
    .await;
    let EventMsg::ModelVerification(verification) = verification else {
        panic!("expected model verification event");
    };
    assert_eq!(
        verification.verifications,
        vec![ModelVerification::TrustedAccessForCyber]
    );

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    Ok(())
}
