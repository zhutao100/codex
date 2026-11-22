use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use app_test_support::ChatGptAuthFixture;
use app_test_support::ChatGptIdTokenClaims;
use app_test_support::McpProcess;
use app_test_support::encode_id_token;
use app_test_support::to_response;
use app_test_support::write_chatgpt_auth;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::routing::get;
use codex_app_server_protocol::AppInfo;
use codex_app_server_protocol::AppsListParams;
use codex_app_server_protocol::AppsListResponse;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput;
use codex_core::auth::AuthCredentialsStoreMode;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use rmcp::handler::server::ServerHandler;
use rmcp::model::JsonObject;
use rmcp::model::ListToolsResult;
use rmcp::model::Meta;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::model::Tool;
use rmcp::model::ToolAnnotations;
use rmcp::transport::StreamableHttpServerConfig;
use rmcp::transport::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use serde_json::json;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

async fn start_app_thread(apps: &str, model: &str) -> Result<(TempDir, McpProcess, String)> {
    let home = TempDir::new()?;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            r#"
chatgpt_base_url = "{apps}"
model_provider = "fixture"
model = "mock-model"
approval_policy = "on-request"
sandbox_mode = "read-only"
[features]
connectors = true
[model_providers.fixture]
name = "Fixture"
base_url = "{model}/v1"
wire_api = "responses"
"#
        ),
    )?;
    write_chatgpt_auth(
        home.path(),
        ChatGptAuthFixture::new("chatgpt-token")
            .account_id("account-123")
            .chatgpt_account_id("account-123"),
        AuthCredentialsStoreMode::File,
    )?;
    let mut mcp = McpProcess::new(home.path()).await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let start = mcp.send_thread_start_request(Default::default()).await?;
    let reply = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(start)),
    )
    .await??;
    Ok((
        home,
        mcp,
        to_response::<ThreadStartResponse>(reply)?.thread.id,
    ))
}

#[tokio::test]
async fn loaded_thread_rebinds_app_tools_on_logout_and_login() -> Result<()> {
    core_test_support::skip_if_no_network_unless_localhost!(Ok(()));
    let model = responses::start_mock_server().await;
    let (apps, task, _) = start_apps_server(vec![], vec![connector_tool("beta", "Beta")?]).await?;
    let (_home, mut mcp, thread) = start_app_thread(&apps, &model.uri()).await?;
    for (step, expected) in ["beta", "", "gamma"].into_iter().enumerate() {
        let auth_request = match step {
            1 => Some(mcp.send_logout_account_request().await?),
            2 => {
                let token = encode_id_token(
                    &ChatGptIdTokenClaims::new()
                        .chatgpt_account_id("account-456")
                        .plan_type("pro"),
                )?;
                Some(
                    mcp.send_chatgpt_auth_tokens_login_request(token, "second-token".into())
                        .await?,
                )
            }
            _ => None,
        };
        if let Some(id) = auth_request {
            timeout(
                DEFAULT_TIMEOUT,
                mcp.read_stream_until_response_message(RequestId::Integer(id)),
            )
            .await??;
        }
        let done = responses::sse(vec![
            responses::ev_assistant_message("answer", "done"),
            responses::ev_completed("done"),
        ]);
        let mut bodies = vec![];
        if step == 2 {
            bodies.push(responses::sse(vec![
                responses::ev_function_call("old-app", "mcp__codex_apps__connector_beta", "{}"),
                responses::ev_completed("stale"),
            ]));
        }
        bodies.push(done);
        let mock = responses::mount_sse_sequence(&model, bodies).await;
        let request = mcp
            .send_turn_start_request(TurnStartParams {
                thread_id: thread.clone(),
                input: vec![
                    UserInput::Text {
                        text: "Use the selected apps".into(),
                        text_elements: vec![],
                    },
                    UserInput::Mention {
                        name: "Beta".into(),
                        path: "app://beta".into(),
                    },
                    UserInput::Mention {
                        name: "Gamma".into(),
                        path: "app://gamma".into(),
                    },
                ],
                ..Default::default()
            })
            .await?;
        timeout(
            DEFAULT_TIMEOUT,
            mcp.read_stream_until_response_message(RequestId::Integer(request)),
        )
        .await??;
        timeout(
            DEFAULT_TIMEOUT,
            mcp.read_stream_until_notification_message("turn/completed"),
        )
        .await??;
        let posts = mock.requests();
        let body = posts[0].body_json();
        let tools: Vec<_> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| {
                tool["name"]
                    .as_str()?
                    .strip_prefix("mcp__codex_apps__connector_")
            })
            .collect();
        assert_eq!(
            tools,
            if expected.is_empty() {
                vec![]
            } else {
                vec![expected]
            }
        );
        if step == 2 {
            assert_eq!(posts.len(), 2);
            assert!(
                posts[1]
                    .function_call_output_text("old-app")
                    .unwrap()
                    .contains("unknown")
                    || posts[1]
                        .function_call_output_text("old-app")
                        .unwrap()
                        .contains("unsupported")
            );
        }
    }
    task.abort();
    Ok(())
}

#[tokio::test]
async fn app_approval_cannot_follow_the_same_tool_to_another_account() -> Result<()> {
    core_test_support::skip_if_no_network_unless_localhost!(Ok(()));
    let model = responses::start_mock_server().await;
    let mut tool = connector_tool("shared", "Shared")?;
    tool.annotations = Some(ToolAnnotations::new().read_only(false).destructive(true));
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (apps, task, _) =
        start_apps_server_with_accounts(vec![], vec![tool.clone()], vec![tool], calls.clone())
            .await?;
    let (_home, mut mcp, thread) = start_app_thread(&apps, &model.uri()).await?;
    for (step, call_id) in ["pending-a", "fresh-b", "remembered-b", "fresh-a"]
        .into_iter()
        .enumerate()
    {
        if step == 3 {
            switch_app_account(&mut mcp, "account-123", "chatgpt-token").await?;
        }
        let model_requests = responses::mount_sse_sequence(
            &model,
            vec![
                responses::sse(vec![
                    responses::ev_function_call(call_id, "mcp__codex_apps__connector_shared", "{}"),
                    responses::ev_completed("action"),
                ]),
                responses::sse(vec![
                    responses::ev_assistant_message("answer", "done"),
                    responses::ev_completed("done"),
                ]),
            ],
        )
        .await;
        let turn = mcp
            .send_turn_start_request(TurnStartParams {
                thread_id: thread.clone(),
                input: vec![
                    UserInput::Text {
                        text: "Use the selected app".into(),
                        text_elements: vec![],
                    },
                    UserInput::Mention {
                        name: "Shared".into(),
                        path: "app://shared".into(),
                    },
                ],
                ..Default::default()
            })
            .await?;
        timeout(
            DEFAULT_TIMEOUT,
            mcp.read_stream_until_response_message(RequestId::Integer(turn)),
        )
        .await??;
        if step != 2 {
            let request =
                timeout(DEFAULT_TIMEOUT, mcp.read_stream_until_request_message()).await??;
            let ServerRequest::ToolRequestUserInput { request_id, params } = request else {
                anyhow::bail!("expected app approval, got {request:?}")
            };
            assert_eq!(params.item_id, call_id);
            if step == 0 {
                switch_app_account(&mut mcp, "account-456", "second-token").await?;
            }
            mcp.send_response(request_id, json!({"answers": {(&params.questions[0].id): {"answers": ["Approve this Session"]}}})).await?;
        }
        timeout(
            DEFAULT_TIMEOUT,
            mcp.read_stream_until_notification_message("turn/completed"),
        )
        .await??;
        let requests = model_requests.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0].body_json()["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == "mcp__codex_apps__connector_shared")
        );
        if step == 0 {
            assert!(
                requests[1]
                    .function_call_output_text(call_id)
                    .unwrap()
                    .contains("app connection changed")
            );
            assert!(calls.lock().unwrap().is_empty());
        }
    }
    assert_eq!(
        *calls.lock().unwrap(),
        ["account-456", "account-456", "account-123"]
    );
    task.abort();
    Ok(())
}

async fn switch_app_account(mcp: &mut McpProcess, account: &str, access: &str) -> Result<()> {
    let token = encode_id_token(
        &ChatGptIdTokenClaims::new()
            .chatgpt_account_id(account)
            .plan_type("pro"),
    )?;
    let id = mcp
        .send_chatgpt_auth_tokens_login_request(token, access.into())
        .await?;
    timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(id)),
    )
    .await??;
    Ok(())
}

#[tokio::test]
async fn list_apps_returns_empty_when_connectors_disabled() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp = McpProcess::new(codex_home.path()).await?;

    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp
        .send_apps_list_request(AppsListParams {
            limit: Some(50),
            cursor: None,
        })
        .await?;

    let response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;

    let AppsListResponse { data, next_cursor } = to_response(response)?;

    assert!(data.is_empty());
    assert!(next_cursor.is_none());
    Ok(())
}

#[tokio::test]
async fn list_apps_returns_connectors_with_accessible_flags() -> Result<()> {
    let connectors = vec![
        AppInfo {
            id: "alpha".to_string(),
            name: "Alpha".to_string(),
            description: Some("Alpha connector".to_string()),
            logo_url: Some("https://example.com/alpha.png".to_string()),
            logo_url_dark: None,
            distribution_channel: None,
            install_url: None,
            is_accessible: false,
        },
        AppInfo {
            id: "beta".to_string(),
            name: "beta".to_string(),
            description: None,
            logo_url: None,
            logo_url_dark: None,
            distribution_channel: None,
            install_url: None,
            is_accessible: false,
        },
    ];

    let tools = vec![connector_tool("beta", "Beta App")?];
    let (server_url, server_handle, directory_requests) =
        start_apps_server(connectors.clone(), tools).await?;

    let codex_home = TempDir::new()?;
    write_connectors_config(codex_home.path(), &server_url)?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("chatgpt-token")
            .account_id("account-123")
            .chatgpt_user_id("user-123")
            .chatgpt_account_id("account-123"),
        AuthCredentialsStoreMode::File,
    )?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp
        .send_apps_list_request(AppsListParams {
            limit: None,
            cursor: None,
        })
        .await?;

    let response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;

    let AppsListResponse { data, next_cursor } = to_response(response)?;

    let expected = vec![
        AppInfo {
            id: "beta".to_string(),
            name: "Beta App".to_string(),
            description: None,
            logo_url: None,
            logo_url_dark: None,
            distribution_channel: None,
            install_url: Some("https://chatgpt.com/apps/beta/beta".to_string()),
            is_accessible: true,
        },
        AppInfo {
            id: "alpha".to_string(),
            name: "Alpha".to_string(),
            description: Some("Alpha connector".to_string()),
            logo_url: Some("https://example.com/alpha.png".to_string()),
            logo_url_dark: None,
            distribution_channel: None,
            install_url: Some("https://chatgpt.com/apps/alpha/alpha".to_string()),
            is_accessible: false,
        },
    ];

    assert_eq!(data, expected);
    assert!(next_cursor.is_none());

    let before_logout = directory_requests.load(std::sync::atomic::Ordering::SeqCst);
    assert!(before_logout > 0);
    let logout = mcp.send_logout_account_request().await?;
    timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(logout)),
    )
    .await??;
    let again = mcp
        .send_apps_list_request(AppsListParams {
            limit: None,
            cursor: None,
        })
        .await?;
    let response = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(again)),
    )
    .await??;
    let result: AppsListResponse = to_response(response)?;
    assert_eq!(result.data, Vec::new());
    assert_eq!(
        directory_requests.load(std::sync::atomic::Ordering::SeqCst),
        before_logout
    );

    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("second-token")
            .account_id("account-456")
            .chatgpt_user_id("user-456")
            .chatgpt_account_id("account-456"),
        AuthCredentialsStoreMode::File,
    )?;
    let next = mcp
        .send_apps_list_request(AppsListParams {
            limit: None,
            cursor: None,
        })
        .await?;
    let reply = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(next)),
    )
    .await??;
    let listed: AppsListResponse = to_response(reply)?;
    assert_eq!(
        listed
            .data
            .into_iter()
            .map(|app| (app.id, app.is_accessible))
            .collect::<Vec<_>>(),
        vec![("gamma".into(), true)]
    );

    server_handle.abort();
    Ok(())
}

#[tokio::test]
async fn list_apps_paginates_results() -> Result<()> {
    let connectors = vec![
        AppInfo {
            id: "alpha".to_string(),
            name: "Alpha".to_string(),
            description: Some("Alpha connector".to_string()),
            logo_url: None,
            logo_url_dark: None,
            distribution_channel: None,
            install_url: None,
            is_accessible: false,
        },
        AppInfo {
            id: "beta".to_string(),
            name: "beta".to_string(),
            description: None,
            logo_url: None,
            logo_url_dark: None,
            distribution_channel: None,
            install_url: None,
            is_accessible: false,
        },
    ];

    let tools = vec![connector_tool("beta", "Beta App")?];
    let (server_url, server_handle, _) = start_apps_server(connectors.clone(), tools).await?;

    let codex_home = TempDir::new()?;
    write_connectors_config(codex_home.path(), &server_url)?;
    write_chatgpt_auth(
        codex_home.path(),
        ChatGptAuthFixture::new("chatgpt-token")
            .account_id("account-123")
            .chatgpt_user_id("user-123")
            .chatgpt_account_id("account-123"),
        AuthCredentialsStoreMode::File,
    )?;

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;

    let first_request = mcp
        .send_apps_list_request(AppsListParams {
            limit: Some(1),
            cursor: None,
        })
        .await?;
    let first_response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(first_request)),
    )
    .await??;
    let AppsListResponse {
        data: first_page,
        next_cursor: first_cursor,
    } = to_response(first_response)?;

    let expected_first = vec![AppInfo {
        id: "beta".to_string(),
        name: "Beta App".to_string(),
        description: None,
        logo_url: None,
        logo_url_dark: None,
        distribution_channel: None,
        install_url: Some("https://chatgpt.com/apps/beta/beta".to_string()),
        is_accessible: true,
    }];

    assert_eq!(first_page, expected_first);
    let next_cursor = first_cursor.ok_or_else(|| anyhow::anyhow!("missing cursor"))?;

    let second_request = mcp
        .send_apps_list_request(AppsListParams {
            limit: Some(1),
            cursor: Some(next_cursor),
        })
        .await?;
    let second_response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(second_request)),
    )
    .await??;
    let AppsListResponse {
        data: second_page,
        next_cursor: second_cursor,
    } = to_response(second_response)?;

    let expected_second = vec![AppInfo {
        id: "alpha".to_string(),
        name: "Alpha".to_string(),
        description: Some("Alpha connector".to_string()),
        logo_url: None,
        logo_url_dark: None,
        distribution_channel: None,
        install_url: Some("https://chatgpt.com/apps/alpha/alpha".to_string()),
        is_accessible: false,
    }];

    assert_eq!(second_page, expected_second);
    assert!(second_cursor.is_none());

    server_handle.abort();
    Ok(())
}

#[derive(Clone)]
struct AppsServerState {
    requests: Arc<std::sync::atomic::AtomicUsize>,
    expected_bearer: String,
    expected_account_id: String,
    response: serde_json::Value,
}

#[derive(Clone)]
struct AppListMcpServer {
    tools: Arc<Vec<Tool>>,
    second_account_tools: Arc<Vec<Tool>>,
    calls: Arc<std::sync::Mutex<Vec<String>>>,
}

impl ServerHandler for AppListMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..ServerInfo::default()
        }
    }

    fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParam>,
        context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, rmcp::ErrorData>> + Send + '_
    {
        let second_account = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.headers.get(AUTHORIZATION))
            .is_some_and(|value| value == "Bearer second-token");
        let tools = if second_account {
            self.second_account_tools.clone()
        } else {
            self.tools.clone()
        };
        async move {
            Ok(ListToolsResult {
                tools: (*tools).clone(),
                next_cursor: None,
                meta: None,
            })
        }
    }

    async fn call_tool(
        &self,
        _: rmcp::model::CallToolRequestParam,
        context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<rmcp::model::CallToolResult, rmcp::ErrorData> {
        let account = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.headers.get("chatgpt-account-id"))
            .and_then(|account| account.to_str().ok())
            .ok_or_else(|| {
                rmcp::ErrorData::invalid_request("missing account on tool request", None)
            })?;
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(account.to_owned());
        Ok(rmcp::model::CallToolResult::success(vec![
            rmcp::model::Content::text("executed"),
        ]))
    }
}

async fn start_apps_server(
    connectors: Vec<AppInfo>,
    tools: Vec<Tool>,
) -> Result<(String, JoinHandle<()>, Arc<std::sync::atomic::AtomicUsize>)> {
    start_apps_server_with_accounts(
        connectors,
        tools,
        vec![connector_tool("gamma", "Gamma")?],
        Default::default(),
    )
    .await
}

async fn start_apps_server_with_accounts(
    connectors: Vec<AppInfo>,
    tools: Vec<Tool>,
    second_account_tools: Vec<Tool>,
    calls: Arc<std::sync::Mutex<Vec<String>>>,
) -> Result<(String, JoinHandle<()>, Arc<std::sync::atomic::AtomicUsize>)> {
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let state = AppsServerState {
        requests: requests.clone(),
        expected_bearer: "Bearer chatgpt-token".to_string(),
        expected_account_id: "account-123".to_string(),
        response: json!({ "apps": connectors, "next_token": null }),
    };
    let state = Arc::new(state);
    let mcp_server = AppListMcpServer {
        tools: Arc::new(tools),
        second_account_tools: Arc::new(second_account_tools),
        calls,
    };

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    let mcp_service = StreamableHttpService::new(
        move || Ok(mcp_server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    let router = Router::new()
        .route("/connectors/directory/list", get(list_directory_connectors))
        .route(
            "/connectors/directory/list_workspace",
            get(list_directory_connectors),
        )
        .with_state(state)
        .nest_service("/api/codex/apps", mcp_service);

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    Ok((format!("http://{addr}"), handle, requests))
}

async fn list_directory_connectors(
    State(state): State<Arc<AppsServerState>>,
    headers: HeaderMap,
) -> Result<impl axum::response::IntoResponse, StatusCode> {
    state
        .requests
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if headers
        .get(AUTHORIZATION)
        .is_some_and(|value| value == "Bearer second-token")
        && headers
            .get("chatgpt-account-id")
            .is_some_and(|value| value == "account-456")
    {
        return Ok(Json(json!({ "apps": [], "next_token": null })));
    }
    let bearer_ok = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == state.expected_bearer);
    let account_ok = headers
        .get("chatgpt-account-id")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == state.expected_account_id);

    if bearer_ok && account_ok {
        Ok(Json(state.response.clone()))
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn connector_tool(connector_id: &str, connector_name: &str) -> Result<Tool> {
    let schema: JsonObject = serde_json::from_value(json!({
        "type": "object",
        "additionalProperties": false
    }))?;
    let mut tool = Tool::new(
        Cow::Owned(format!("connector_{connector_id}")),
        Cow::Borrowed("Connector test tool"),
        Arc::new(schema),
    );
    tool.annotations = Some(ToolAnnotations::new().read_only(true));

    let mut meta = Meta::new();
    meta.0
        .insert("connector_id".to_string(), json!(connector_id));
    meta.0
        .insert("connector_name".to_string(), json!(connector_name));
    tool.meta = Some(meta);
    Ok(tool)
}

fn write_connectors_config(codex_home: &std::path::Path, base_url: &str) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        config_toml,
        format!(
            r#"
chatgpt_base_url = "{base_url}"

[features]
connectors = true
"#
        ),
    )
}
