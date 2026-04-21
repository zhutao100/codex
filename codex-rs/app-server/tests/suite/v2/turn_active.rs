#![cfg(unix)]

use anyhow::Result;
use app_test_support::McpProcess;
use app_test_support::create_mock_responses_server_sequence;
use app_test_support::create_shell_command_sse_response;
use app_test_support::to_response;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::NewConversationParams;
use codex_app_server_protocol::NewConversationResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SendUserMessageParams;
use codex_app_server_protocol::SendUserMessageResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnActiveParams;
use codex_app_server_protocol::TurnActiveResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnInterruptParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput as V2UserInput;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn turn_active_lists_and_clears_running_turns() -> Result<()> {
    let shell_command = vec!["sleep".to_string(), "10".to_string()];

    let tmp = TempDir::new()?;
    let codex_home = tmp.path().join("codex_home");
    std::fs::create_dir(&codex_home)?;
    let working_directory = tmp.path().join("workdir");
    std::fs::create_dir(&working_directory)?;

    let server = create_mock_responses_server_sequence(vec![create_shell_command_sse_response(
        shell_command,
        Some(&working_directory),
        Some(10_000),
        "call_sleep",
    )?])
    .await;
    create_config_toml(&codex_home, &server.uri())?;

    let mut mcp = McpProcess::new(&codex_home).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_req = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            ..Default::default()
        })
        .await?;
    let thread_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_req)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_resp)?;

    let turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![V2UserInput::Text {
                text: "run sleep".to_string(),
                text_elements: Vec::new(),
            }],
            cwd: Some(working_directory),
            ..Default::default()
        })
        .await?;
    let turn_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_req)),
    )
    .await??;
    let TurnStartResponse { turn } = to_response::<TurnStartResponse>(turn_resp)?;

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let active_req = mcp.send_turn_active_request(TurnActiveParams {}).await?;
    let active_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(active_req)),
    )
    .await??;
    let TurnActiveResponse { data } = to_response::<TurnActiveResponse>(active_resp)?;
    assert!(
        data.iter()
            .any(|entry| entry.thread_id == thread.id && entry.turn_id == turn.id)
    );

    let interrupt_id = mcp
        .send_turn_interrupt_request(TurnInterruptParams {
            thread_id: thread.id.clone(),
            turn_id: turn.id,
        })
        .await?;
    let _: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(interrupt_id)),
    )
    .await??;

    let completed_notif: JSONRPCNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    let _: TurnCompletedNotification =
        serde_json::from_value(completed_notif.params.expect("turn/completed params"))?;

    let active_req = mcp.send_turn_active_request(TurnActiveParams {}).await?;
    let active_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(active_req)),
    )
    .await??;
    let TurnActiveResponse { data } = to_response::<TurnActiveResponse>(active_resp)?;
    assert!(!data.iter().any(|entry| entry.thread_id == thread.id));

    Ok(())
}

#[tokio::test]
async fn turn_active_uses_live_turn_state_without_listener() -> Result<()> {
    let shell_command = vec!["sleep".to_string(), "2".to_string()];

    let tmp = TempDir::new()?;
    let codex_home = tmp.path().join("codex_home");
    std::fs::create_dir(&codex_home)?;
    let working_directory = tmp.path().join("workdir");
    std::fs::create_dir(&working_directory)?;

    let server = create_mock_responses_server_sequence(vec![create_shell_command_sse_response(
        shell_command,
        Some(&working_directory),
        Some(2_000),
        "call_sleep",
    )?])
    .await;
    create_config_toml(&codex_home, &server.uri())?;

    let mut mcp = McpProcess::new(&codex_home).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let new_conversation_id = mcp
        .send_new_conversation_request(NewConversationParams {
            cwd: Some(working_directory.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    let new_conversation_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(new_conversation_id)),
    )
    .await??;
    let NewConversationResponse {
        conversation_id, ..
    } = to_response::<NewConversationResponse>(new_conversation_resp)?;
    let conversation_id_str = conversation_id.to_string();

    let send_user_message_id = mcp
        .send_send_user_message_request(SendUserMessageParams {
            conversation_id,
            items: vec![codex_app_server_protocol::InputItem::Text {
                text: "run sleep".to_string(),
                text_elements: Vec::new(),
            }],
        })
        .await?;
    let send_user_message_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(send_user_message_id)),
    )
    .await??;
    let _response: SendUserMessageResponse =
        to_response::<SendUserMessageResponse>(send_user_message_resp)?;

    let mut saw_active_turn = false;
    for _ in 0..20 {
        let active_req = mcp.send_turn_active_request(TurnActiveParams {}).await?;
        let active_resp: JSONRPCResponse = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_response_message(RequestId::Integer(active_req)),
        )
        .await??;
        let TurnActiveResponse { data } = to_response::<TurnActiveResponse>(active_resp)?;

        if data
            .iter()
            .any(|entry| entry.thread_id == conversation_id_str)
        {
            saw_active_turn = true;
            break;
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    assert!(saw_active_turn);
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    Ok(())
}

#[tokio::test]
async fn turn_active_stays_present_while_turn_is_running() -> Result<()> {
    let shell_command = vec!["sleep".to_string(), "2".to_string()];

    let tmp = TempDir::new()?;
    let codex_home = tmp.path().join("codex_home");
    std::fs::create_dir(&codex_home)?;
    let working_directory = tmp.path().join("workdir");
    std::fs::create_dir(&working_directory)?;

    let server = create_mock_responses_server_sequence(vec![create_shell_command_sse_response(
        shell_command,
        Some(&working_directory),
        Some(2_000),
        "call_sleep",
    )?])
    .await;
    create_config_toml(&codex_home, &server.uri())?;

    let mut mcp = McpProcess::new(&codex_home).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let new_conversation_id = mcp
        .send_new_conversation_request(NewConversationParams {
            cwd: Some(working_directory.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    let new_conversation_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(new_conversation_id)),
    )
    .await??;
    let NewConversationResponse {
        conversation_id, ..
    } = to_response::<NewConversationResponse>(new_conversation_resp)?;
    let conversation_id_str = conversation_id.to_string();

    let send_user_message_id = mcp
        .send_send_user_message_request(SendUserMessageParams {
            conversation_id,
            items: vec![codex_app_server_protocol::InputItem::Text {
                text: "run sleep".to_string(),
                text_elements: Vec::new(),
            }],
        })
        .await?;
    let send_user_message_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(send_user_message_id)),
    )
    .await??;
    let _response: SendUserMessageResponse =
        to_response::<SendUserMessageResponse>(send_user_message_resp)?;

    let mut running_turn_id: Option<String> = None;
    for _ in 0..40 {
        let active_req = mcp.send_turn_active_request(TurnActiveParams {}).await?;
        let active_resp: JSONRPCResponse = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_response_message(RequestId::Integer(active_req)),
        )
        .await??;
        let TurnActiveResponse { data } = to_response::<TurnActiveResponse>(active_resp)?;

        if let Some(entry) = data
            .iter()
            .find(|entry| entry.thread_id == conversation_id_str)
        {
            running_turn_id = Some(entry.turn_id.clone());
            break;
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let running_turn_id = running_turn_id.expect("expected active turn to appear");

    for _ in 0..8 {
        let active_req = mcp.send_turn_active_request(TurnActiveParams {}).await?;
        let active_resp: JSONRPCResponse = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_response_message(RequestId::Integer(active_req)),
        )
        .await??;
        let TurnActiveResponse { data } = to_response::<TurnActiveResponse>(active_resp)?;

        assert!(
            data.iter()
                .any(|entry| entry.thread_id == conversation_id_str
                    && entry.turn_id == running_turn_id),
            "active turn disappeared while still running"
        );

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    Ok(())
}

fn create_config_toml(codex_home: &std::path::Path, server_uri: &str) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        config_toml,
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "danger-full-access"

model_provider = "mock_provider"

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{server_uri}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
        ),
    )
}
