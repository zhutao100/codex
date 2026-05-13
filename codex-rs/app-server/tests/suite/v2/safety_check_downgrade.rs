use anyhow::Result;
use app_test_support::McpProcess;
use app_test_support::to_response;
use app_test_support::write_mock_responses_config_toml;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::ModelRerouteReason;
use codex_app_server_protocol::ModelReroutedNotification;
use codex_app_server_protocol::ModelVerification;
use codex_app_server_protocol::ModelVerificationNotification;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput as V2UserInput;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const REQUESTED_MODEL: &str = "gpt-5.3-codex";
const SERVER_MODEL: &str = "gpt-5.2";

fn ev_model_verification_metadata(response_id: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "response.metadata",
        "response": {
            "id": response_id
        },
        "metadata": {
            "openai_verification_recommendation": ["trusted_access_for_cyber"]
        }
    })
}

async fn start_thread(mcp: &mut McpProcess) -> Result<String> {
    let thread_req = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some(REQUESTED_MODEL.to_string()),
            ..Default::default()
        })
        .await?;
    let thread_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_req)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_resp)?;
    Ok(thread.id)
}

async fn start_turn(mcp: &mut McpProcess, thread_id: String, text: &str) -> Result<()> {
    let turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id,
            input: vec![V2UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_req)),
    )
    .await??;
    Ok(())
}

#[tokio::test]
async fn model_mismatch_emits_reroute_notification_and_pauses_turn_v2() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response = responses::sse_response(responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_completed("resp-1"),
    ]))
    .insert_header("OpenAI-Model", SERVER_MODEL);
    responses::mount_response_once(&server, response).await;

    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml(
        codex_home.path(),
        &server.uri(),
        &BTreeMap::new(),
        1_000_000,
        None,
        "mock_provider",
        "compact",
    )?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;
    let thread_id = start_thread(&mut mcp).await?;
    start_turn(&mut mcp, thread_id.clone(), "trigger model mismatch").await?;

    let notification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("model/rerouted"),
    )
    .await??;
    let rerouted: ModelReroutedNotification =
        serde_json::from_value(notification.params.expect("model/rerouted params"))?;
    assert_eq!(
        rerouted,
        ModelReroutedNotification {
            thread_id: thread_id.clone(),
            turn_id: rerouted.turn_id.clone(),
            from_model: REQUESTED_MODEL.to_string(),
            to_model: SERVER_MODEL.to_string(),
            reason: ModelRerouteReason::ServerSelectedDifferentModel,
        }
    );

    let completed = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    let completed: codex_app_server_protocol::TurnCompletedNotification =
        serde_json::from_value(completed.params.expect("turn/completed params"))?;
    assert_eq!(completed.turn.status, TurnStatus::Paused);

    Ok(())
}

#[tokio::test]
async fn model_verification_emits_notification_v2() -> Result<()> {
    let server = responses::start_mock_server().await;
    let response = responses::sse_response(responses::sse(vec![
        responses::ev_response_created("resp-1"),
        ev_model_verification_metadata("resp-1"),
        responses::ev_assistant_message("msg-1", "done"),
        responses::ev_completed("resp-1"),
    ]));
    responses::mount_response_once(&server, response).await;

    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml(
        codex_home.path(),
        &server.uri(),
        &BTreeMap::new(),
        1_000_000,
        None,
        "mock_provider",
        "compact",
    )?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;
    let thread_id = start_thread(&mut mcp).await?;
    start_turn(&mut mcp, thread_id.clone(), "trigger model verification").await?;

    let notification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("model/verification"),
    )
    .await??;
    let verification: ModelVerificationNotification =
        serde_json::from_value(notification.params.expect("model/verification params"))?;
    assert_eq!(
        verification,
        ModelVerificationNotification {
            thread_id,
            turn_id: verification.turn_id.clone(),
            verifications: vec![ModelVerification::TrustedAccessForCyber],
        }
    );

    Ok(())
}
