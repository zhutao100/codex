use anyhow::Context;
use anyhow::Result;
use app_test_support::McpProcess;
use app_test_support::create_fake_rollout;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::to_response;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStartedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;
use std::path::Path;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn thread_fork_creates_new_thread_and_emits_started() -> Result<()> {
    let server = create_mock_responses_server_repeating_assistant("Done").await;
    let codex_home = TempDir::new()?;
    let workspaces = TempDir::new()?;
    let fork_cwd = workspaces.path().canonicalize()?.join("fork");
    let later_cwd = workspaces.path().canonicalize()?.join("later");
    std::fs::create_dir(&fork_cwd)?;
    std::fs::create_dir(&later_cwd)?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let preview = "Saved user message";
    let conversation_id = create_fake_rollout(
        codex_home.path(),
        "2025-01-05T12-00-00",
        "2025-01-05T12:00:00Z",
        preview,
        Some("mock_provider"),
        None,
    )?;

    let original_path = codex_home
        .path()
        .join("sessions")
        .join("2025")
        .join("01")
        .join("05")
        .join(format!(
            "rollout-2025-01-05T12-00-00-{conversation_id}.jsonl"
        ));
    assert!(
        original_path.exists(),
        "expected original rollout to exist at {}",
        original_path.display()
    );
    let original_contents = std::fs::read_to_string(&original_path)?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let fork_id = mcp
        .send_thread_fork_request(ThreadForkParams {
            thread_id: conversation_id.clone(),
            cwd: Some(fork_cwd.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    let fork_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(fork_id)),
    )
    .await??;
    let ThreadForkResponse { thread, .. } = to_response::<ThreadForkResponse>(fork_resp)?;

    let after_contents = std::fs::read_to_string(&original_path)?;
    assert_eq!(
        after_contents, original_contents,
        "fork should not mutate the original rollout file"
    );

    assert_ne!(thread.id, conversation_id);
    assert_eq!(thread.preview, preview);
    assert_eq!(thread.model_provider, "mock_provider");
    let thread_path = thread.path.clone().expect("thread path");
    assert!(thread_path.is_absolute());
    assert_ne!(thread_path, original_path);
    assert_eq!(thread.cwd, fork_cwd);
    assert_eq!(thread.source, SessionSource::VsCode);

    assert_eq!(
        thread.turns.len(),
        1,
        "expected forked thread to include one turn"
    );
    let turn = &thread.turns[0];
    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(turn.items.len(), 1, "expected user message item");
    match &turn.items[0] {
        ThreadItem::UserMessage { content, .. } => {
            assert_eq!(
                content,
                &vec![UserInput::Text {
                    text: preview.to_string(),
                    text_elements: Vec::new(),
                }]
            );
        }
        other => panic!("expected user message item, got {other:?}"),
    }

    // A corresponding thread/started notification should arrive.
    let notif: JSONRPCNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("thread/started"),
    )
    .await??;
    let started: ThreadStartedNotification =
        serde_json::from_value(notif.params.expect("params must be present"))?;
    assert_eq!(started.thread, thread);

    let turn = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            cwd: Some(later_cwd.clone()),
            input: vec![UserInput::Text {
                text: "Change workspace".into(),
                text_elements: vec![],
            }],
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn)),
    )
    .await??;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    let db = codex_core::state_db::open_if_present(codex_home.path(), "mock_provider")
        .await
        .context("missing fork index")?;
    let id = codex_protocol::ThreadId::from_string(&thread.id)?;
    assert_eq!(
        db.get_thread(id).await?.context("missing latest turn")?.cwd,
        later_cwd
    );
    assert_history_workspace(&mut mcp, &thread.id, &fork_cwd).await?;

    let jsonl_home = TempDir::new()?;
    create_config_toml(jsonl_home.path(), &server.uri())?;
    let copy = jsonl_home.path().join(
        thread_path
            .canonicalize()?
            .strip_prefix(codex_home.path().canonicalize()?)?,
    );
    std::fs::create_dir_all(copy.parent().context("missing rollout parent")?)?;
    std::fs::copy(thread_path, copy)?;
    let mut jsonl = McpProcess::new(jsonl_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, jsonl.initialize()).await??;
    assert!(
        codex_core::state_db::open_if_present(jsonl_home.path(), "mock_provider")
            .await
            .is_none()
    );
    assert_history_workspace(&mut jsonl, &thread.id, &fork_cwd).await?;
    Ok(())
}

async fn assert_history_workspace(mcp: &mut McpProcess, id: &str, cwd: &Path) -> Result<()> {
    let read = mcp
        .send_thread_read_request(ThreadReadParams {
            thread_id: id.into(),
            include_turns: false,
        })
        .await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(read)),
    )
    .await??;
    assert_eq!(to_response::<ThreadReadResponse>(response)?.thread.cwd, cwd);
    let list = mcp
        .send_thread_list_request(ThreadListParams {
            model_providers: None,
            cursor: None,
            limit: None,
            sort_key: None,
            source_kinds: None,
            archived: None,
        })
        .await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(list)),
    )
    .await??;
    let listed = to_response::<ThreadListResponse>(response)?
        .data
        .into_iter()
        .find(|it| it.id == id)
        .context("fork missing from thread/list")?;
    assert_eq!(listed.cwd, cwd);
    let fork = mcp
        .send_thread_fork_request(ThreadForkParams {
            thread_id: id.into(),
            ..Default::default()
        })
        .await?;
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(fork)),
    )
    .await??;
    let fork = to_response::<ThreadForkResponse>(response)?;
    assert_eq!((fork.cwd.as_path(), fork.thread.cwd.as_path()), (cwd, cwd));
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
