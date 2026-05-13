# Design Proposal

## Status

Implemented for `/review`. Validation added app-server v2 typed config/schema exposure, request-auth centralization, clearer unknown-provider errors, consistent inline/detached review provider application, targeted validation for misplaced `review_model` keys, and `model_overlay.models[].model_provider` fallback for review models without an explicit `review_model_provider`.

## Objective

Add a minimal first-class path for a delegate task to use a secondary model provider and provider-local API key while the primary session remains on default OpenAI + ChatGPT auth.

The design should reuse the existing delegate runner and avoid turning model-provider selection into a broad multi-provider runtime. The goal is a task-local provider override, not a global provider architecture rewrite.

## Non-goals

- Do not change the primary session's global `model_provider`, selected model, or `AuthManager`.
- Do not mutate `models_cache.json` or store secondary-provider model metadata in it.
- Do not add provider-specific live `/models` cache management in the first patch.
- Do not add profile-scoped `model_overlay`; the current overlay remains global-only.
- Do not require a second persisted `auth.json` or a second login flow.

## Recommended User-Facing Shape

For the existing `/review` task, add a provider sibling to the existing `review_model` setting when an explicit task override is needed. In common cases, prefer binding the provider on the overlay model entry:

```toml
# Primary session remains unchanged.
model_provider = "openai"
model = "gpt-5.4"

# Existing review model setting.
review_model = "external-reviewer"

[model_providers.external-review]
name = "External Review Provider"
base_url = "https://review-provider.example.com/v1"
env_key = "EXTERNAL_REVIEW_API_KEY"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false

[[model_overlay.models]]
slug = "external-reviewer"
model_provider = "external-review"
display_name = "External Reviewer"
visibility = "list"
context_window = 200000
auto_compact_token_limit = 160000
```

`review_model_provider = "external-review"` remains supported as an explicit override and wins over the overlay entry.

For a new sibling task rather than `/review`, use the same pattern with task-specific names, for example:

```toml
external_review_model = "external-reviewer"
external_review_model_provider = "external-review"
```

Keep the provider id separate from the model slug. The model slug is what is sent in the Responses request body. The provider id selects endpoint, headers, query parameters, retry policy, websocket support, and provider-local credentials.

## Minimal Code Surface

### 1. Add task-specific provider config

For `/review`, add:

```rust
pub review_model_provider: Option<String>
```

to:

- `core/src/config/mod.rs::ConfigToml`;
- `core/src/config/mod.rs::Config`;
- `app-server-protocol/src/protocol/v2.rs::Config`;
- schema generation / expected config-schema output;
- any test fixture expected `Config` values.

`review_model_provider` should use the same provider-id namespace as global `model_provider`.

For a new sibling task, add equivalent task-specific fields instead of overloading global `model_provider`.

### 2. Add a provider-selection helper

Add a small helper near config or task code so multiple delegate tasks do not reimplement provider lookup:

```rust
pub(crate) fn apply_delegate_model_provider(
    config: &mut Config,
    provider_id: &str,
) -> Result<(), CodexErr> {
    config.apply_model_provider_id(provider_id)
}
```

The helper should not modify the parent session config. It operates only on the cloned sub-agent config. `apply_model_provider_id(...)` pins provider resolution for that derived config, so an explicit task provider wins over any `model_overlay.models[].model_provider` binding for the selected model.

### 3. Apply provider/model override before `run_codex_thread_one_shot(...)`

For `/review`, change `start_review_conversation(...)` conceptually to:

```rust
let config = ctx.config.clone();
let mut sub_agent_config = config.as_ref().clone();

// Existing review restrictions remain.
sub_agent_config.web_search_mode = Some(WebSearchMode::Disabled);
sub_agent_config
    .features
    .disable(Feature::WebSearchRequest)
    .disable(Feature::WebSearchCached)
    .disable(Feature::Collab);

sub_agent_config.base_instructions = Some(crate::REVIEW_PROMPT.to_string());
sub_agent_config.approval_policy = crate::config::Constrained::allow_any(AskForApproval::Never);

let model = config
    .review_model
    .clone()
    .unwrap_or_else(|| ctx.model_info.slug.clone());
sub_agent_config.model = Some(model);

if let Some(provider_id) = config.review_model_provider.as_deref() {
    apply_delegate_model_provider(&mut sub_agent_config, provider_id)?;

    // The secondary provider's model metadata should come from model_overlay, not
    // the primary OpenAI remote model cache.
    sub_agent_config.features.disable(Feature::RemoteModels);
}

run_codex_thread_one_shot(
    sub_agent_config,
    session.auth_manager(),
    session.models_manager(),
    input,
    session.clone_session(),
    ctx.clone(),
    cancellation_token,
    None,
)
.await
```

The existing `start_review_conversation(...) -> Option<Receiver<Event>>` currently swallows errors with `.ok()`. For a provider config error, prefer returning/logging a `CodexErr` and surfacing an error event rather than silently skipping the task.

### 4. Keep `ModelsManager` cache architecture unchanged in the first patch

The spawned delegate can use the parent `ModelsManager` because:

- the model slug is explicit;
- model metadata is resolved from bundled/live OpenAI candidates plus `model_overlay`;
- a custom secondary model can be added by `model_overlay`;
- no provider-specific `/models` fetch is required for the delegate.

Disable `Feature::RemoteModels` in the delegate config when a task-specific non-primary provider is selected. This avoids an incidental OpenAI `/models` refresh during sub-agent spawn and keeps the secondary task independent of `MODEL_CACHE_FILE`.

## Auth Handling

### Current provider-local API-key behavior

With a custom provider that sets `env_key`, the request bearer token is already provider-local because `auth_provider_from_auth(...)` checks `provider.api_key()?` before using the parent `AuthManager` token.

That means this works at the wire level:

```toml
[model_providers.external-review]
base_url = "https://review-provider.example.com/v1"
env_key = "EXTERNAL_REVIEW_API_KEY"
requires_openai_auth = false
```

The outgoing request uses:

```text
Authorization: Bearer $EXTERNAL_REVIEW_API_KEY
```

However, this is not a true session-level `AuthMode::ApiKey`. The delegate still receives the parent `AuthManager`, and current telemetry / 401 recovery still see the primary ChatGPT auth state.

### Minimal fix for request-level auth mode

Centralize request-auth resolution so provider-local credentials are classified as API-key request auth even when account auth is ChatGPT.

Replace direct bearer-token resolution plus separate `auth.auth_mode()` checks with a helper shaped like:

```rust
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
        let mode = auth.auth_mode();
        let recovery = matches!(mode, AuthMode::Chatgpt) && provider.requires_openai_auth;
        return Ok(ResolvedRequestAuth {
            provider: CoreAuthProvider {
                token: Some(auth.get_token()?),
                account_id: auth.get_account_id(),
            },
            auth_mode: Some(mode),
            enable_unauthorized_recovery: recovery,
        });
    }

    Ok(ResolvedRequestAuth {
        provider: CoreAuthProvider {
            token: None,
            account_id: None,
        },
        auth_mode: None,
        enable_unauthorized_recovery: false,
    })
}
```

Use `ResolvedRequestAuth.auth_mode` for `ModelProviderInfo::to_api_provider(...)` so a provider-local `env_key` behaves as API-key auth for default URL selection and request classification.

Use `enable_unauthorized_recovery` to construct `UnauthorizedRecovery` only when the request is actually using OpenAI ChatGPT auth. This prevents a 401 from a secondary API-key provider from triggering ChatGPT token refresh and accidentally reporting `RefreshTokenFailed` instead of the provider's real 401.

### Why not create a second `AuthManager`?

A second production `AuthManager` would imply another persisted account auth source, reload behavior, forced-login handling, and token-refresh policy. That is unnecessary for this use case. Provider-local API keys already belong on `ModelProviderInfo` through `env_key`.

Keep `AuthManager` as account/session auth. Add request-auth classification for provider-local credentials.

## Model Overlay Guidance

Use per-model overlay entries for the secondary provider. Avoid top-level cross-model overlay fields unless the same metadata should apply to the primary OpenAI model too.

Recommended:

```toml
[[model_overlay.models]]
slug = "external-reviewer"
display_name = "External Reviewer"
visibility = "list"
context_window = 200000
auto_compact_token_limit = 160000
supports_parallel_tool_calls = false
```

Do not edit `models_cache.json`. It is an OpenAI remote-model cache in this branch and can be refreshed/replaced by cache TTL, ETag, and client-version rules.

If the secondary provider needs different effective instructions, prefer a task-specific `base_instructions` in the delegate config, or use a per-model overlay final override:

```toml
[[model_overlay.models]]
slug = "external-reviewer"
final_instruction_override_file = "/absolute/path/to/external-reviewer-instructions.md"
```

For `/review`, `ReviewTask` currently sets `sub_agent_config.base_instructions = REVIEW_PROMPT`, so that explicit config instruction remains stronger than overlay final instruction overrides during session initialization.

## Validation Plan

### Config tests

- `review_model_provider` selects the configured provider id from `model_providers`.
- `model_overlay.models[].model_provider` selects the provider when `review_model_provider` is unset.
- Unknown `review_model_provider` produces a clear config/task error.
- A primary config with `model_provider = "openai"` and `review_model_provider = "external-review"` keeps the primary `Config.model_provider_id == "openai"`.

### Request routing tests

Use two mock servers:

1. primary server for the parent turn;
2. secondary server for the delegate task.

Assert:

- the parent request goes to the primary provider URL;
- the delegate request goes to the secondary provider URL;
- the delegate request body uses `review_model` / task-specific model;
- the delegate request includes `Authorization: Bearer <secondary key>` from the secondary provider `env_key`;
- no secondary provider request uses the ChatGPT access token.
- when `review_model_provider` is unset, the same routing works through the overlay entry's `model_provider`.

### Auth-mode tests

- Provider `env_key` resolves request auth mode to `AuthMode::ApiKey` even if the parent `AuthManager` is ChatGPT.
- Provider-local API-key 401 does not run ChatGPT unauthorized recovery.
- OpenAI ChatGPT requests still run the existing ChatGPT unauthorized recovery path.
- Missing provider env var returns the existing `CodexErr::EnvVar` path with provider instructions.

### Model metadata tests

- Secondary model metadata is resolved from `model_overlay` when the slug is absent from OpenAI `/models`.
- Disabling `Feature::RemoteModels` in the delegate config does not prevent explicit `review_model` from resolving through overlay fallback.
- The primary session still lists/selects normal OpenAI remote models from `models_cache.json`.

## Implementation Order

1. Add `review_model_provider` or equivalent new-task-specific provider field.
2. Add `apply_delegate_model_provider(...)` and use it in the delegate task before `run_codex_thread_one_shot(...)`.
3. Disable `Feature::RemoteModels` in the delegate config when a task-specific secondary provider is selected.
4. Add `resolve_request_auth(...)` and migrate `ModelClient` plus live-model fetch request paths from separate auth/provider resolution to the centralized helper.
5. Add request-routing and 401-recovery tests.
6. Add user-facing config docs only after the above behavior is covered by tests.

## Expected Outcome

After the minimal patch, the primary session can keep using default OpenAI + ChatGPT auth, while a delegate task can use:

- `run_codex_thread_one_shot(...)`;
- a task-local model slug;
- a task-local `ModelProviderInfo` selected from `model_providers`;
- provider-local API-key bearer auth from `env_key`;
- local model metadata from `model_overlay`;
- no secondary mutation of `MODEL_CACHE_FILE` or primary session auth.
