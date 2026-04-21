use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use app_test_support::McpProcess;
use app_test_support::to_response;
use app_test_support::write_models_cache;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::ModelListParams;
use codex_app_server_protocol::ModelListResponse;
use codex_app_server_protocol::RequestId;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const INVALID_REQUEST_ERROR_CODE: i64 = -32600;

async fn list_models_page(
    mcp: &mut McpProcess,
    params: ModelListParams,
) -> Result<ModelListResponse> {
    let request_id = mcp.send_list_models_request(params).await?;
    let response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    to_response::<ModelListResponse>(response)
}

#[tokio::test]
async fn list_models_returns_non_empty_list_with_single_default() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_models_cache(codex_home.path())?;
    let mut mcp = McpProcess::new(codex_home.path()).await?;

    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;

    let ModelListResponse {
        data: items,
        next_cursor,
    } = list_models_page(
        &mut mcp,
        ModelListParams {
            limit: None,
            cursor: None,
        },
    )
    .await?;

    assert!(next_cursor.is_none());
    assert!(!items.is_empty());

    let default_models = items.iter().filter(|model| model.is_default).count();
    assert_eq!(default_models, 1, "expected exactly one default model");
    assert!(
        items.first().is_some_and(|model| model.is_default),
        "expected the first model to be the default",
    );

    let ids: HashSet<&str> = items.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids.len(), items.len(), "expected model ids to be unique");

    for model in &items {
        assert!(
            !model.supported_reasoning_efforts.is_empty(),
            "expected reasoning effort options for {}",
            model.id
        );
        assert!(
            model
                .supported_reasoning_efforts
                .iter()
                .any(|effort| effort.reasoning_effort == model.default_reasoning_effort),
            "expected default reasoning effort to be supported for {}",
            model.id
        );
        assert!(
            !model.input_modalities.is_empty(),
            "expected input modalities for {}",
            model.id
        );
    }

    Ok(())
}

#[tokio::test]
async fn list_models_pagination_matches_full_list() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_models_cache(codex_home.path())?;
    let mut mcp = McpProcess::new(codex_home.path()).await?;

    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;

    let full = list_models_page(
        &mut mcp,
        ModelListParams {
            limit: None,
            cursor: None,
        },
    )
    .await?;

    let mut cursor = None;
    let mut paged = Vec::new();
    loop {
        let page = list_models_page(
            &mut mcp,
            ModelListParams {
                limit: Some(1),
                cursor: cursor.clone(),
            },
        )
        .await?;

        assert!(
            page.data.len() <= 1,
            "expected page size <= 1, got {}",
            page.data.len()
        );
        paged.extend(page.data);

        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
        assert!(
            paged.len() <= full.data.len(),
            "pagination returned more models than the full list",
        );
    }

    assert_eq!(paged, full.data);
    Ok(())
}

#[tokio::test]
async fn list_models_rejects_invalid_cursor() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_models_cache(codex_home.path())?;
    let mut mcp = McpProcess::new(codex_home.path()).await?;

    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;

    let request_id = mcp
        .send_list_models_request(ModelListParams {
            limit: None,
            cursor: Some("invalid".to_string()),
        })
        .await?;

    let error: JSONRPCError = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;

    assert_eq!(error.id, RequestId::Integer(request_id));
    assert_eq!(error.error.code, INVALID_REQUEST_ERROR_CODE);
    assert_eq!(error.error.message, "invalid cursor: invalid");
    Ok(())
}
