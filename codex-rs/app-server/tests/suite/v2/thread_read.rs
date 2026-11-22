use anyhow::Result;
use app_test_support::McpProcess;
use app_test_support::create_fake_rollout_with_text_elements;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::to_response;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_protocol::user_input::ByteRange;
use codex_protocol::user_input::TextElement;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::path::PathBuf;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn shared_writer_preview_and_move_are_visible_to_both_read_apis() -> Result<()> {
    use codex_app_server_protocol::GetConversationSummaryParams;
    use codex_app_server_protocol::GetConversationSummaryResponse;
    use codex_app_server_protocol::ThreadStartResponse;
    use std::io::Write as _;
    core_test_support::skip_if_no_network_unless_localhost!(Ok(()));
    let home = TempDir::new()?;
    create_config_toml(home.path(), "http://127.0.0.1:1")?;
    let mut mcp = McpProcess::new(home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;
    let id = mcp.send_thread_start_request(Default::default()).await?;
    let reply = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(id)),
    )
    .await??;
    let thread = to_response::<ThreadStartResponse>(reply)?.thread;
    let active = thread.path.unwrap();
    let thread_id = codex_protocol::ThreadId::from_string(&thread.id)?;
    let db = codex_core::state_db::open_if_present(home.path(), "mock_provider")
        .await
        .unwrap();
    assert!(
        db.get_thread(thread_id)
            .await?
            .unwrap()
            .first_user_message
            .is_none()
    );
    let timestamp = std::fs::metadata(&active)?.modified()?;
    let archived = home
        .path()
        .canonicalize()?
        .join("archived_sessions")
        .join(active.file_name().unwrap());
    for (n, expected) in [&active, &archived].into_iter().enumerate() {
        if n == 0 {
            let mut writer = std::fs::File::options().append(true).open(&active)?;
            writeln!(
                writer,
                "{}",
                serde_json::json!({"timestamp":"2026-01-27T20:00:00Z","type":"event_msg","payload":{"type":"user_message","message":"written by the other CLI"}})
            )?;
            writer.set_modified(timestamp)?;
        } else {
            std::fs::create_dir_all(archived.parent().unwrap())?;
            std::fs::rename(&active, &archived)?;
        }
        for include_turns in [false, true] {
            let id = mcp
                .send_thread_read_request(ThreadReadParams {
                    thread_id: thread.id.clone(),
                    include_turns,
                })
                .await?;
            let reply = timeout(
                DEFAULT_READ_TIMEOUT,
                mcp.read_stream_until_response_message(RequestId::Integer(id)),
            )
            .await??;
            let read = to_response::<ThreadReadResponse>(reply)?.thread;
            assert_eq!(
                (read.path.unwrap().canonicalize()?, read.preview.as_str()),
                (expected.canonicalize()?, "written by the other CLI")
            );
        }
        let id = mcp
            .send_get_conversation_summary_request(GetConversationSummaryParams::ThreadId {
                conversation_id: thread_id,
            })
            .await?;
        let reply = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_response_message(RequestId::Integer(id)),
        )
        .await??;
        let read = to_response::<GetConversationSummaryResponse>(reply)?.summary;
        assert_eq!(
            (read.path.canonicalize()?, read.preview.as_str()),
            (expected.canonicalize()?, "written by the other CLI")
        );
    }
    Ok(())
}

#[tokio::test]
async fn thread_read_returns_summary_without_turns() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let preview = "Saved user message";
    let text_elements = [TextElement::new(
        ByteRange { start: 0, end: 5 },
        Some("<note>".into()),
    )];
    let conversation_id = create_fake_rollout_with_text_elements(
        codex_home.path(),
        "2025-01-05T12-00-00",
        "2025-01-05T12:00:00Z",
        preview,
        text_elements
            .iter()
            .map(|elem| serde_json::to_value(elem).expect("serialize text element"))
            .collect(),
        Some("mock_provider"),
        None,
    )?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let read_id = mcp
        .send_thread_read_request(ThreadReadParams {
            thread_id: conversation_id.clone(),
            include_turns: false,
        })
        .await?;
    let read_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(read_id)),
    )
    .await??;
    let ThreadReadResponse { thread } = to_response::<ThreadReadResponse>(read_resp)?;

    assert_eq!(thread.id, conversation_id);
    assert_eq!(thread.preview, preview);
    assert_eq!(thread.model_provider, "mock_provider");
    assert!(thread.path.as_ref().expect("thread path").is_absolute());
    assert_eq!(thread.cwd, PathBuf::from("/"));
    assert_eq!(thread.cli_version, "0.0.0");
    assert_eq!(thread.source, SessionSource::Cli);
    assert_eq!(thread.git_info, None);
    assert_eq!(thread.turns.len(), 0);

    Ok(())
}

#[tokio::test]
async fn thread_read_can_include_turns() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let preview = "Saved user message";
    let text_elements = vec![TextElement::new(
        ByteRange { start: 0, end: 5 },
        Some("<note>".into()),
    )];
    let conversation_id = create_fake_rollout_with_text_elements(
        codex_home.path(),
        "2025-01-05T12-00-00",
        "2025-01-05T12:00:00Z",
        preview,
        text_elements
            .iter()
            .map(|elem| serde_json::to_value(elem).expect("serialize text element"))
            .collect(),
        Some("mock_provider"),
        None,
    )?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let read_id = mcp
        .send_thread_read_request(ThreadReadParams {
            thread_id: conversation_id.clone(),
            include_turns: true,
        })
        .await?;
    let read_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(read_id)),
    )
    .await??;
    let ThreadReadResponse { thread } = to_response::<ThreadReadResponse>(read_resp)?;

    assert_eq!(thread.turns.len(), 1);
    let turn = &thread.turns[0];
    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(turn.items.len(), 1, "expected user message item");
    match &turn.items[0] {
        ThreadItem::UserMessage { content, .. } => {
            assert_eq!(
                content,
                &vec![UserInput::Text {
                    text: preview.to_string(),
                    text_elements: text_elements.clone().into_iter().map(Into::into).collect(),
                }]
            );
        }
        other => panic!("expected user message item, got {other:?}"),
    }

    Ok(())
}

// Helper to create a config.toml pointing at the mock model server.
fn create_config_toml(codex_home: &Path, server_uri: &str) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        config_toml,
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"

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
