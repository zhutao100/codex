# Problem Statement

Status: implemented. The pre-change assessment below remains useful context, with
one code-level update: request auth is now centralized in
`core/src/api_bridge.rs::resolve_request_auth(...)`; the older
`auth_provider_from_auth(...)` helper described in the initial assessment has
been removed.
Follow-up validation also found that TOML table scoping can silently place
`review_model` keys under `[[model_overlay.models]]`; runtime validation now
rejects that placement with a targeted error.

## Scenario

The branch has a primary Codex session that should keep using the normal OpenAI provider path:

- global `model_provider = "openai"`;
- default upstream OpenAI / ChatGPT backend URL selection;
- `Chatgpt` account authentication from `auth.json`;
- a primary model selected from the normal remote-model metadata flow and `models_cache.json`.

The proposed extension is to add a second task workflow, similar to `core/src/tasks/review.rs`, that runs through `core/src/codex_delegate.rs::run_codex_thread_one_shot(...)` but uses a different model-provider endpoint and provider-local API key credentials.

The primary session must stay intact. Only the spawned delegate task should change provider/model/auth behavior.

## Current Code Mechanics

### `model_overlay`

The custom `model_overlay` feature is implemented as a local metadata/instruction overlay:

- `ConfigToml.model_overlay` is resolved into `Config.model_overlay` during config loading.
- Runtime overlay structs live in `core/src/models_manager/overlay.rs`.
- `ModelsManager::get_model_info(...)` applies the overlay to bundled/live model candidates before selecting a model.
- Unknown overlay model slugs are materialized from `model_info::model_info_from_slug(slug)` and then patched.
- `ModelsManager::get_final_instruction_override(...)` resolves per-model or top-level final instruction overrides for session startup and model-switch updates.
- Overlay-expanded models are not persisted back into `models_cache.json`.

This is sufficient for describing a secondary provider model slug locally, including context window, compaction limit, picker visibility, tool behavior, and instruction overrides.

### `model_provider` config

The branch has multiple provider definitions but only one active provider in a resolved `Config`:

- `ConfigToml.model_providers` is merged with `built_in_model_providers()`.
- `Config.model_provider_id` selects one entry.
- `Config.model_provider` stores the selected `ModelProviderInfo`.
- Profiles can select `model` and `model_provider`, but `model_overlay` is currently global-only.
- User-defined providers are inserted with `entry(key).or_insert(provider)`, so a user-defined provider cannot replace an existing built-in provider key.

A `ModelProviderInfo` can carry provider-local credentials through:

- `env_key`, which reads an API key from an environment variable;
- `experimental_bearer_token`, which hard-codes a bearer token in config and should be avoided outside controlled programmatic setups.

`ModelProviderInfo::to_api_provider(auth_mode)` chooses the default base URL from the current auth mode only when `base_url` is absent. A secondary provider should set `base_url` explicitly.

### Auth modes

`core/src/auth.rs` models account auth as exactly one active `CodexAuth` per `AuthManager`:

- `CodexAuth::ApiKey(ApiKeyAuth)` reports `AuthMode::ApiKey` and uses the API key as the bearer token.
- `CodexAuth::Chatgpt(...)` and `CodexAuth::ChatgptAuthTokens(...)` report `AuthMode::Chatgpt` and use ChatGPT access tokens.
- `AuthManager` is session-global. It caches one current auth value and performs ChatGPT token refresh / unauthorized recovery for ChatGPT auth.

Request auth is slightly more flexible than account auth.
`core/src/api_bridge.rs::resolve_request_auth(...)` resolves bearer auth in this order:

1. provider `env_key` API key;
2. provider `experimental_bearer_token`;
3. current `CodexAuth::get_token()` from the supplied `AuthManager`;
4. no bearer token.

Therefore, a custom provider with `env_key` can send provider-local API-key bearer credentials even when the primary session `AuthManager` is ChatGPT. However, the session's account auth mode remains `Chatgpt`; session-level telemetry remains tied to the shared `AuthManager`.

The implemented helper also classifies provider-local credentials as request
`AuthMode::ApiKey` and disables ChatGPT unauthorized recovery for those
requests, while leaving the parent session `AuthManager` unchanged.

### `/review` delegate workflow

`core/src/tasks/review.rs` currently:

- clones the parent turn `Config`;
- disables review-forbidden features such as web search and collab;
- sets `base_instructions = REVIEW_PROMPT`;
- sets `approval_policy = Never`;
- selects `config.review_model` if present, otherwise the parent model slug;
- calls `run_codex_thread_one_shot(...)` with the cloned config, parent `AuthManager`, and parent `ModelsManager`.

The task changes only the model slug. It does not have a review-specific provider, auth mode, provider credential, or model-manager selection.

`run_codex_thread_one_shot(...)` itself is already usable for provider-specific delegates because it accepts a full `Config`. The missing surface is resolving and applying a task-specific provider/auth policy before spawning the sub-Codex session.

## Support Matrix

| Requirement | Current state | Assessment |
| --- | --- | --- |
| Keep primary session on default OpenAI + ChatGPT auth | Supported | Leave global `model_provider = "openai"` and current `auth.json` untouched. |
| Add a second provider definition | Supported | Add `[model_providers.<id>]` with explicit `base_url`, `wire_api = "responses"`, and `env_key`. |
| Describe a secondary model not in OpenAI `/models` | Supported | Add a per-model `[[model_overlay.models]]` entry. Do not edit `models_cache.json`. |
| Use a different model in `/review` | Supported | Existing `review_model` changes only the model slug. |
| Use a different provider in `/review` or a sibling task | Not first-class | The task must mutate `sub_agent_config.model_provider_id` and `sub_agent_config.model_provider` itself; no config field exists. |
| Use provider-local API-key bearer credentials while parent auth is ChatGPT | Partially supported | `env_key` wins over `AuthManager` token for the HTTP bearer token. This is wire-compatible API-key auth, but not an actual `AuthMode::ApiKey` session. |
| Make the delegate report/behave as `AuthMode::ApiKey` | Partially supported | Request auth is classified as `AuthMode::ApiKey` when provider-local credentials are used. The shared `AuthManager` remains ChatGPT. |
| Avoid OpenAI remote-model refresh for the secondary provider | Supported for `/review` delegate spawn | `Feature::RemoteModels` is disabled on the cloned delegate config when `review_model_provider` is set; use `model_overlay` metadata for custom slugs. |
| Maintain a provider-specific model cache for the secondary provider | Not supported | `MODEL_CACHE_FILE` is not provider-namespaced and `ModelsManager::new(...)` uses the OpenAI provider. This is unnecessary for the minimal delegate use case. |

## Conclusion

The lower-level modules are close enough to reuse:

- `model_overlay` can describe the secondary model;
- `model_providers` can describe the secondary endpoint;
- `run_codex_thread_one_shot(...)` can spawn an isolated delegate with a mutated `Config`;
- `env_key` can provide the secondary API key on the actual HTTP request.

The current code does not fully support the proposed use case as a first-class, safe, config-driven feature because provider selection and `AuthMode::ApiKey` semantics are still session-global. A small dev-plan is needed instead of only adding usage docs.
