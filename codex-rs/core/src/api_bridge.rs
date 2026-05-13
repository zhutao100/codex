use chrono::DateTime;
use chrono::Utc;
use codex_api::AuthProvider as ApiAuthProvider;
use codex_api::TransportError;
use codex_api::error::ApiError;
use codex_api::rate_limits::parse_promo_message;
use codex_api::rate_limits::parse_rate_limit;
use http::HeaderMap;
use serde::Deserialize;
use serde_json::Value;

use crate::auth::AuthMode;
use crate::auth::CodexAuth;
use crate::error::CodexErr;
use crate::error::ModelCapError;
use crate::error::RetryLimitReachedError;
use crate::error::UnexpectedResponseError;
use crate::error::UsageLimitReachedError;
use crate::model_provider_info::ModelProviderInfo;
use crate::token_data::PlanType;

pub(crate) fn map_api_error(err: ApiError) -> CodexErr {
    match err {
        ApiError::ContextWindowExceeded => CodexErr::ContextWindowExceeded,
        ApiError::QuotaExceeded => CodexErr::QuotaExceeded,
        ApiError::UsageNotIncluded => CodexErr::UsageNotIncluded,
        ApiError::Retryable { message, delay } => CodexErr::Stream(message, delay),
        ApiError::Stream(msg) => CodexErr::Stream(msg, None),
        ApiError::Api { status, message } => CodexErr::UnexpectedStatus(UnexpectedResponseError {
            status,
            body: message,
            url: None,
            cf_ray: None,
            request_id: None,
        }),
        ApiError::InvalidRequest { message } => CodexErr::InvalidRequest(message),
        ApiError::CyberPolicy { message } => CodexErr::CyberPolicy { message },
        ApiError::ServerOverloaded => CodexErr::InternalServerError,
        ApiError::Transport(transport) => match transport {
            TransportError::Http {
                status,
                url,
                headers,
                body,
            } => {
                let body_text = body.unwrap_or_default();

                if status == http::StatusCode::BAD_REQUEST {
                    if let Some(message) = cyber_policy_message_from_body(&body_text) {
                        CodexErr::CyberPolicy { message }
                    } else if body_text
                        .contains("The image data you provided does not represent a valid image")
                    {
                        CodexErr::InvalidImageRequest()
                    } else {
                        CodexErr::InvalidRequest(body_text)
                    }
                } else if status == http::StatusCode::INTERNAL_SERVER_ERROR {
                    CodexErr::InternalServerError
                } else if status == http::StatusCode::TOO_MANY_REQUESTS {
                    if let Some(model) = headers
                        .as_ref()
                        .and_then(|map| map.get(MODEL_CAP_MODEL_HEADER))
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string)
                    {
                        let reset_after_seconds = headers
                            .as_ref()
                            .and_then(|map| map.get(MODEL_CAP_RESET_AFTER_HEADER))
                            .and_then(|value| value.to_str().ok())
                            .and_then(|value| value.parse::<u64>().ok());
                        return CodexErr::ModelCap(ModelCapError {
                            model,
                            reset_after_seconds,
                        });
                    }

                    if let Ok(err) = serde_json::from_str::<UsageErrorResponse>(&body_text) {
                        if err.error.error_type.as_deref() == Some("usage_limit_reached") {
                            let rate_limits = headers.as_ref().and_then(parse_rate_limit);
                            let promo_message = headers.as_ref().and_then(parse_promo_message);
                            let resets_at = err
                                .error
                                .resets_at
                                .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0));
                            return CodexErr::UsageLimitReached(UsageLimitReachedError {
                                plan_type: err.error.plan_type,
                                resets_at,
                                rate_limits,
                                promo_message,
                            });
                        } else if err.error.error_type.as_deref() == Some("usage_not_included") {
                            return CodexErr::UsageNotIncluded;
                        }
                    }

                    CodexErr::RetryLimit(RetryLimitReachedError {
                        status,
                        request_id: extract_request_tracking_id(headers.as_ref()),
                    })
                } else {
                    CodexErr::UnexpectedStatus(UnexpectedResponseError {
                        status,
                        body: body_text,
                        url,
                        cf_ray: extract_header(headers.as_ref(), CF_RAY_HEADER),
                        request_id: extract_request_id(headers.as_ref()),
                    })
                }
            }
            TransportError::RetryLimit => CodexErr::RetryLimit(RetryLimitReachedError {
                status: http::StatusCode::INTERNAL_SERVER_ERROR,
                request_id: None,
            }),
            TransportError::Timeout => CodexErr::Timeout,
            TransportError::Network(msg) | TransportError::Build(msg) => {
                CodexErr::Stream(msg, None)
            }
        },
        ApiError::RateLimit(msg) => CodexErr::Stream(msg, None),
    }
}

const MODEL_CAP_MODEL_HEADER: &str = "x-codex-model-cap-model";
const MODEL_CAP_RESET_AFTER_HEADER: &str = "x-codex-model-cap-reset-after-seconds";
const REQUEST_ID_HEADER: &str = "x-request-id";
const OAI_REQUEST_ID_HEADER: &str = "x-oai-request-id";
const CF_RAY_HEADER: &str = "cf-ray";
const CYBER_POLICY_ERROR_CODE: &str = "cyber_policy";
const CYBER_POLICY_FALLBACK_MESSAGE: &str =
    "This request has been flagged for possible cybersecurity risk.";

fn cyber_policy_message_from_body(body_text: &str) -> Option<String> {
    let parsed = serde_json::from_str::<Value>(body_text).ok()?;
    let error = parsed.get("error")?;
    if error.get("code").and_then(Value::as_str) != Some(CYBER_POLICY_ERROR_CODE) {
        return None;
    }

    Some(
        error
            .get("message")
            .and_then(Value::as_str)
            .filter(|message| !message.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| CYBER_POLICY_FALLBACK_MESSAGE.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_provider_info::WireApi;
    use codex_api::TransportError;
    use http::HeaderMap;
    use http::StatusCode;
    use pretty_assertions::assert_eq;
    use serial_test::serial;
    use std::ffi::OsString;

    #[test]
    fn map_api_error_maps_model_cap_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            MODEL_CAP_MODEL_HEADER,
            http::HeaderValue::from_static("boomslang"),
        );
        headers.insert(
            MODEL_CAP_RESET_AFTER_HEADER,
            http::HeaderValue::from_static("120"),
        );
        let err = map_api_error(ApiError::Transport(TransportError::Http {
            status: StatusCode::TOO_MANY_REQUESTS,
            url: Some("http://example.com/v1/responses".to_string()),
            headers: Some(headers),
            body: Some(String::new()),
        }));

        let CodexErr::ModelCap(model_cap) = err else {
            panic!("expected CodexErr::ModelCap, got {err:?}");
        };
        assert_eq!(model_cap.model, "boomslang");
        assert_eq!(model_cap.reset_after_seconds, Some(120));
    }

    #[test]
    fn map_api_error_maps_cyber_policy() {
        let err = map_api_error(ApiError::CyberPolicy {
            message: "This request was flagged.".to_string(),
        });

        let CodexErr::CyberPolicy { message } = err else {
            panic!("expected CodexErr::CyberPolicy, got {err:?}");
        };
        assert_eq!(message, "This request was flagged.");
    }

    #[test]
    fn map_api_error_maps_cyber_policy_from_400_body() {
        let body = serde_json::json!({
            "error": {
                "message": "This request has been flagged for potentially high-risk cyber activity.",
                "type": "invalid_request",
                "param": null,
                "code": "cyber_policy"
            }
        })
        .to_string();

        let err = map_api_error(ApiError::Transport(TransportError::Http {
            status: StatusCode::BAD_REQUEST,
            url: Some("http://example.com/v1/responses".to_string()),
            headers: None,
            body: Some(body),
        }));

        let CodexErr::CyberPolicy { message } = err else {
            panic!("expected CodexErr::CyberPolicy, got {err:?}");
        };
        assert_eq!(
            message,
            "This request has been flagged for potentially high-risk cyber activity."
        );
    }

    #[test]
    fn map_api_error_uses_cyber_policy_fallback_for_missing_message() {
        let body = serde_json::json!({
            "error": {
                "code": "cyber_policy"
            }
        })
        .to_string();

        let err = map_api_error(ApiError::Transport(TransportError::Http {
            status: StatusCode::BAD_REQUEST,
            url: Some("http://example.com/v1/responses".to_string()),
            headers: None,
            body: Some(body),
        }));

        let CodexErr::CyberPolicy { message } = err else {
            panic!("expected CodexErr::CyberPolicy, got {err:?}");
        };
        assert_eq!(
            message,
            "This request has been flagged for possible cybersecurity risk."
        );
    }

    #[serial(env_vars)]
    #[test]
    fn resolve_request_auth_classifies_provider_env_key_as_api_key() {
        let _guard = EnvGuard::set("CODEX_TEST_PROVIDER_API_KEY", "provider-key");
        let provider = test_provider_with_env_key("CODEX_TEST_PROVIDER_API_KEY");

        let resolved = resolve_request_auth(
            Some(CodexAuth::create_dummy_chatgpt_auth_for_testing()),
            &provider,
        )
        .expect("provider env key should resolve");

        assert_eq!(resolved.provider.token.as_deref(), Some("provider-key"));
        assert_eq!(resolved.provider.account_id.as_deref(), None);
        assert_eq!(resolved.auth_mode, Some(AuthMode::ApiKey));
        assert!(!resolved.enable_unauthorized_recovery);
    }

    #[test]
    fn resolve_request_auth_enables_recovery_only_for_openai_chatgpt_auth() {
        let provider = ModelProviderInfo {
            requires_openai_auth: true,
            ..test_provider()
        };

        let resolved = resolve_request_auth(
            Some(CodexAuth::create_dummy_chatgpt_auth_for_testing()),
            &provider,
        )
        .expect("chatgpt auth should resolve");

        assert_eq!(resolved.provider.token.as_deref(), Some("Access Token"));
        assert_eq!(resolved.provider.account_id.as_deref(), Some("account_id"));
        assert_eq!(resolved.auth_mode, Some(AuthMode::Chatgpt));
        assert!(resolved.enable_unauthorized_recovery);
    }

    fn test_provider_with_env_key(env_key: &str) -> ModelProviderInfo {
        ModelProviderInfo {
            env_key: Some(env_key.to_string()),
            ..test_provider()
        }
    }

    fn test_provider() -> ModelProviderInfo {
        ModelProviderInfo {
            name: "test".to_string(),
            base_url: Some("http://example.com/v1".to_string()),
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

    struct EnvGuard {
        key: &'static str,
        original: Option<OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var_os(key);
            // SAFETY: this serial test owns the process environment mutation.
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: the guard restores the original environment value before the serial test exits.
            unsafe {
                match &self.original {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }
}

fn extract_request_tracking_id(headers: Option<&HeaderMap>) -> Option<String> {
    extract_request_id(headers).or_else(|| extract_header(headers, CF_RAY_HEADER))
}

fn extract_request_id(headers: Option<&HeaderMap>) -> Option<String> {
    extract_header(headers, REQUEST_ID_HEADER)
        .or_else(|| extract_header(headers, OAI_REQUEST_ID_HEADER))
}

fn extract_header(headers: Option<&HeaderMap>, name: &str) -> Option<String> {
    headers.and_then(|map| {
        map.get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    })
}

pub(crate) struct ResolvedRequestAuth {
    pub provider: CoreAuthProvider,
    pub auth_mode: Option<AuthMode>,
    pub enable_unauthorized_recovery: bool,
}

pub(crate) fn resolve_request_auth(
    auth: Option<CodexAuth>,
    provider: &ModelProviderInfo,
) -> crate::error::Result<ResolvedRequestAuth> {
    if let Some(api_key) = provider.api_key()? {
        return Ok(ResolvedRequestAuth {
            provider: CoreAuthProvider {
                token: Some(api_key),
                account_id: None,
            },
            auth_mode: Some(AuthMode::ApiKey),
            enable_unauthorized_recovery: false,
        });
    }

    if let Some(token) = provider.experimental_bearer_token.clone() {
        return Ok(ResolvedRequestAuth {
            provider: CoreAuthProvider {
                token: Some(token),
                account_id: None,
            },
            auth_mode: Some(AuthMode::ApiKey),
            enable_unauthorized_recovery: false,
        });
    }

    if let Some(auth) = auth {
        let auth_mode = auth.auth_mode();
        let token = auth.get_token()?;
        Ok(ResolvedRequestAuth {
            provider: CoreAuthProvider {
                token: Some(token),
                account_id: auth.get_account_id(),
            },
            auth_mode: Some(auth_mode),
            enable_unauthorized_recovery: auth_mode == AuthMode::Chatgpt
                && provider.requires_openai_auth,
        })
    } else {
        Ok(ResolvedRequestAuth {
            provider: CoreAuthProvider {
                token: None,
                account_id: None,
            },
            auth_mode: None,
            enable_unauthorized_recovery: false,
        })
    }
}

#[derive(Debug, Deserialize)]
struct UsageErrorResponse {
    error: UsageErrorBody,
}

#[derive(Debug, Deserialize)]
struct UsageErrorBody {
    #[serde(rename = "type")]
    error_type: Option<String>,
    plan_type: Option<PlanType>,
    resets_at: Option<i64>,
}

#[derive(Clone, Default)]
pub(crate) struct CoreAuthProvider {
    token: Option<String>,
    account_id: Option<String>,
}

impl ApiAuthProvider for CoreAuthProvider {
    fn bearer_token(&self) -> Option<String> {
        self.token.clone()
    }

    fn account_id(&self) -> Option<String> {
        self.account_id.clone()
    }
}
