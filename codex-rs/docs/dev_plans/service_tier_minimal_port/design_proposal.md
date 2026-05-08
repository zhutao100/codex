# Minimal ServiceTier Port Design

## Status

Implemented in this branch. The minimal port preserves absence as `None`, forwards `service_tier` only for explicit OpenAI Responses requests, omits the field for built-in OSS providers, exposes the tier through app-server v2 thread/turn APIs, and includes regenerated or updated schemas/docs.

## Target base

This proposal targets this project's customized branch shape. This branch already contains the custom `/pause` and `/continue` lifecycle work and the local model overlay work. This proposal therefore describes a narrow feature port into the current branch shape, not a cherry-pick of upstream branch's `ServiceTier` commits or session/app-server refactors.

## Goal

Add first-class `ServiceTier` support to this branch with these semantics:

|User/config intent|Effective client tier|Responses API JSON|
|---|---:|---|
|field absent|none|omit `service_tier`|
|`service_tier = "flex"`|`Flex`|`"service_tier": "flex"`|
|`service_tier = "fast"`|`Fast`|`"service_tier": "priority"`|

Runtime validation showed that treating absence as explicit `"flex"` can fail with `Unsupported service_tier: flex` for accounts/models where the Responses API does not accept flex. Absence must therefore remain an omitted wire field; `"flex"` is an explicit opt-in value only.

## Non-goals

- Do not port upstream branch's enterprise/business/team account-plan defaulting to `Fast`.
- Do not port `Feature::FastMode`.
- Do not port `notice.fast_default_opt_out`; there is no automatic `Fast` default to opt out from in this design.
- Do not add a `Standard` variant. The only supported explicit tiers are `Fast` and `Flex`.
- Do not add the TUI `/fast` command in the minimal patch. Config and app-server control surfaces are sufficient for the first implementation.
- Do not refactor this branch's monolithic `core/src/codex.rs` session code into the upstream branch's `session/` module layout.
- Do not change `/pause` or `/continue` semantics. Service tier is a sticky session setting and should flow through continued turns like model/reasoning settings already do.

## Inspection summary

The upstream branch implements `ServiceTier` as a small protocol enum with variants `Fast` and `Flex`. Its config/app-server spelling is lowercase, but `Fast` maps to the OpenAI Responses wire value `"priority"`; `Flex` maps to `"flex"`.

Upstream commit `02170996` enables managed Fast defaults on the CLI client side, not by relying on different server behavior for omitted requests. At session creation, core checks the configured tier, `[notice].fast_default_opt_out`, the cached ChatGPT account plan, and `Feature::FastMode`; eligible enterprise, business-like, and team-like plans resolve omitted config to `ServiceTier::Fast`. This branch intentionally does not port that resolver, feature flag, or notice opt-out marker.

This branch has no `ServiceTier` type, config field, protocol field, app-server field, TUI `/fast` command, or HTTP/WebSocket `service_tier` request field. Its streaming request path is still:

```text
Config / profile / app-server params
  -> SessionConfiguration
  -> TurnContext / per-turn Config
  -> ModelClientSession::stream(...)
  -> ResponsesRequestBuilder / ResponseCreateWsRequest
```

That path is sufficient for a minimal port. No upstream prerequisite refactor is required.

## Validation updates

Execution against the current branch found two branch-specific details to keep the proposal precise:

- v2 typed config payloads in `app-server-protocol/src/protocol/v2.rs` use the existing snake_case config-file shape (`service_tier`), while thread and turn RPC payloads remain camelCase (`serviceTier`).
- `SessionConfiguredEvent.service_tier` is represented as `Option<ServiceTier>` in Rust for additive wire compatibility, but generated TypeScript event fields must not use optional-nullable syntax outside `*Params`; the generated event type is therefore `service_tier: ServiceTier | null`.
- Runtime testing showed that implicit `"flex"` is not equivalent to omitting `service_tier`. The implementation must preserve the absent/configured distinction through config, session, TUI, exec, and app-server surfaces.

## Design decisions

### 1. Preserve optional configured tier internally

Add a `ServiceTier` enum, but keep resolved config optional:

```rust
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Display, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ServiceTier {
    Fast,
    Flex,
}
```

Prefer:

```rust
pub service_tier: Option<ServiceTier>
```

inside `Config`, `SessionConfiguration`, `ThreadConfigSnapshot`, and `TurnContext`.

Use optional fields only at override/request boundaries:

```rust
pub service_tier: Option<ServiceTier>
```

For those optional boundary fields:

|Boundary value|Meaning|
|---|---|
|omitted / `None`|leave the existing session/config value unchanged|
|`Some(Fast)`|set sticky tier to `Fast`|
|`Some(Flex)`|explicitly set sticky tier to `Flex`|

Do not collapse omitted config into `Flex`. That loses the distinction between API-default behavior and an explicit flex request.

### 2. Keep config precedence conventional

Resolve the final tier with normal override/profile/global precedence:

```rust
let service_tier = service_tier_override
    .or(config_profile.service_tier)
    .or(cfg.service_tier);
```

Precedence:

|Source|Priority|
|---|---:|
|`ConfigOverrides.service_tier`|highest|
|active profile `service_tier`|middle|
|global `service_tier`|lower|
|absent everywhere|`None`|

### 3. Gate wire emission to OpenAI-compatible first-party requests

Because this branch has built-in OSS providers (`ollama`, `lmstudio`) that also use the `Responses` wire path, the MVP should avoid sending a new default body field to providers that may reject unknown OpenAI-specific keys.

Recommended minimal guard:

```rust
fn service_tier_for_wire(
    provider: &ModelProviderInfo,
    service_tier: Option<ServiceTier>,
) -> Option<String> {
    if !provider.is_openai() {
        return None;
    }

    service_tier.map(|service_tier| match service_tier {
        ServiceTier::Fast => "priority".to_string(),
        ServiceTier::Flex => "flex".to_string(),
    })
}
```

This preserves omission for unset config while avoiding accidental local-provider regressions. If product requirements later demand service-tier forwarding to custom providers/proxies, add a provider capability such as `supports_service_tier: Option<bool>` rather than hard-coding more provider names.

## Minimal implementation surfaces

### 1. Protocol enum

Files:

- `protocol/src/config_types.rs`
- `protocol/src/lib.rs` only if the new enum needs an explicit re-export beyond existing `config_types` exports

Add `ServiceTier` near `Verbosity` / `WebSearchMode` so all crates can import it from `codex_protocol::config_types::ServiceTier`.

### 2. Config schema and resolution

Files:

- `core/src/config/mod.rs`
- `core/src/config/profile.rs`
- `core/src/config/schema.rs` / generated `core/config.schema.json` path, if this branch's schema-generation flow requires checked-in schema updates
- `app-server-protocol/src/protocol/v2.rs` config structs, if typed config RPCs need to expose the field

Add fields:

```rust
// ConfigToml
pub service_tier: Option<ServiceTier>,

// ConfigProfile
pub service_tier: Option<ServiceTier>,

// ConfigOverrides
pub service_tier: Option<ServiceTier>,

// Config
pub service_tier: Option<ServiceTier>,
```

Resolve in `Config::load_config_with_layer_stack(...)` after the active profile is known and before constructing `Config`.

Do not add `Notice.fast_default_opt_out`.

Config examples:

```toml
# Explicit flex tier. Omit the field to leave the request unspecified.
service_tier = "flex"
```

```toml
# Explicit fast/priority tier.
service_tier = "fast"
```

```toml
[profiles.fast]
service_tier = "fast"
model = "gpt-5.4"

[profiles.flex]
service_tier = "flex"
model = "gpt-5.4"
```

### 3. Session and turn propagation

Files:

- `core/src/codex.rs`
- `core/src/codex_thread.rs`
- `protocol/src/protocol.rs`

Add `service_tier` to:

```rust
SessionConfiguration {
    service_tier: Option<ServiceTier>,
}

SessionSettingsUpdate {
    service_tier: Option<ServiceTier>,
}

TurnContext {
    service_tier: Option<ServiceTier>,
}

ThreadConfigSnapshot {
    service_tier: Option<ServiceTier>,
}
```

Apply updates stickily:

```rust
if let Some(service_tier) = updates.service_tier {
    next_configuration.service_tier = Some(service_tier);
}
```

Copy into the per-turn config:

```rust
per_turn_config.service_tier = session_configuration.service_tier;
```

Set `TurnContext.service_tier` in `make_turn_context(...)` from `session_configuration.service_tier` or from the per-turn config.

### 4. Core protocol operations and events

File:

- `protocol/src/protocol.rs`

Add optional request-boundary fields:

```rust
Op::UserTurn {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    service_tier: Option<ServiceTier>,
    // ...
}

Op::OverrideTurnContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    service_tier: Option<ServiceTier>,
    // ...
}
```

Use `Option<ServiceTier>`, not `Option<Option<ServiceTier>>`.

Add the effective tier to `SessionConfiguredEvent`. For wire compatibility with the upstream branch and to keep the event field additive, prefer optional serialization even though the implementation should always set it:

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub service_tier: Option<ServiceTier>,
```

Set it from `config.service_tier` at session configuration time.

When handling `Op::UserTurn`, include `service_tier` in the generated `SessionSettingsUpdate`. When handling `Op::OverrideTurnContext`, pass it through alongside model/reasoning/personality changes.

Important branch-specific detail: app-server `turn/start` currently sends `Op::OverrideTurnContext` first and then `Op::UserInput`; therefore the service tier must be accepted by `OverrideTurnContext`. Adding it only to `UserTurn` is not enough.

### 5. Request body and transport plumbing

Files:

- `codex-api/src/common.rs`
- `codex-api/src/endpoint/responses.rs`
- `codex-api/src/requests/responses.rs`
- `core/src/client.rs`

Add optional `service_tier` string fields to both final request structs:

```rust
pub struct ResponsesApiRequest<'a> {
    // ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    // ...
}

pub struct ResponseCreateWsRequest {
    // ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    // ...
}
```

Add `service_tier: Option<String>` to `ResponsesOptions` and `ResponsesRequestBuilder`.

In `ResponsesClient::stream_prompt(...)`, call:

```rust
.service_tier(service_tier)
```

when building the request.

In `ModelClientSession::build_responses_options(...)`, compute:

```rust
let service_tier = service_tier_for_wire(&self.client.state.provider, service_tier);
```

and include it in `ApiResponsesOptions`.

Extend `ModelClientSession::stream(...)` and its HTTP/WebSocket helper calls with a `service_tier: Option<ServiceTier>` argument:

```rust
pub async fn stream(
    &mut self,
    prompt: &Prompt,
    model_info: &ModelInfo,
    otel_manager: &OtelManager,
    effort: Option<ReasoningEffortConfig>,
    summary: ReasoningSummaryConfig,
    service_tier: Option<ServiceTier>,
    turn_metadata_header: Option<&str>,
) -> Result<ResponseStream>
```

The WebSocket path uses `ResponseCreateWsRequest` equality checks with input cleared to decide whether incremental append is safe. Including `service_tier` in that struct is desirable: a tier change should make the non-input payload differ and force a full `response.create` rather than an incremental append against a request created under a different tier.

### 6. Update stream call sites

Files:

- `core/src/codex.rs`
- `core/src/compact.rs`
- `core/src/thread_name.rs`

Pass `turn_context.service_tier` or `turn_context.config.service_tier` into every `ModelClientSession::stream(...)` call.

Known this branch streaming call sites:

|File|Call purpose|Tier source|
|---|---|---|
|`core/src/codex.rs`|normal model turn|`turn_context.service_tier`|
|`core/src/compact.rs`|local streaming compaction / drain|`turn_context.service_tier`|
|`core/src/thread_name.rs`|thread-name generation|`turn_context.service_tier`|

Do not update unary `CompactClient::compact_input(...)` or `MemoriesClient::trace_summarize_input(...)` in the MVP unless the target server endpoint is confirmed to accept `service_tier` there too. Those are not the standard streaming Responses request body path in this branch.

### 7. App-server protocol and processor

Files:

- `app-server-protocol/src/protocol/v2.rs`
- `app-server/src/codex_message_processor.rs`
- generated schema fixtures under `app-server-protocol/schema/`

Import `ServiceTier` in `app-server-protocol/src/protocol/v2.rs`.

Add typed config exposure:

```rust
pub struct Config {
    // ...
    // Serialized as service_tier because v2 config structs mirror config.toml.
    pub service_tier: Option<ServiceTier>,
    // ...
}

pub struct ProfileV2 {
    // ...
    // Serialized as service_tier because v2 config structs mirror config.toml.
    pub service_tier: Option<ServiceTier>,
    // ...
}
```

Add thread lifecycle fields:

```rust
pub struct ThreadStartParams {
    #[ts(optional = nullable)]
    pub service_tier: Option<ServiceTier>,
    // ...
}

pub struct ThreadStartResponse {
    pub service_tier: Option<ServiceTier>,
    // ...
}

pub struct ThreadResumeParams {
    #[ts(optional = nullable)]
    pub service_tier: Option<ServiceTier>,
    // ...
}

pub struct ThreadResumeResponse {
    pub service_tier: Option<ServiceTier>,
    // ...
}

pub struct ThreadForkParams {
    #[ts(optional = nullable)]
    pub service_tier: Option<ServiceTier>,
    // ...
}

pub struct ThreadForkResponse {
    pub service_tier: Option<ServiceTier>,
    // ...
}
```

Add turn override field:

```rust
pub struct TurnStartParams {
    #[ts(optional = nullable)]
    pub service_tier: Option<ServiceTier>,
    // ...
}
```

Processor changes:

- Extend `build_thread_config_overrides(...)` to accept `service_tier` and set `ConfigOverrides.service_tier`.
- Include `service_tier` in `thread/start`, `thread/resume`, and `thread/fork` destructuring and response construction.
- Include `params.service_tier.is_some()` in `turn/start`'s `has_any_overrides`.
- Pass `service_tier: params.service_tier` into `Op::OverrideTurnContext`.

Legacy v1 app-server methods can be left unchanged in the MVP. They inherit the current thread/config tier; when constructing `Op::UserTurn`, pass `service_tier: None` if the enum variant now requires the field.

Regenerate app-server protocol fixtures after the Rust structs change:

```bash
cargo run -p codex-app-server-protocol --bin write_schema_fixtures
```

or the equivalent repo command if this branch has a wrapper.

### 8. TUI integration

Minimal patch:

- Add `service_tier: self.config.service_tier` to the `Op::UserTurn` constructed in `tui/src/chatwidget.rs`.
- Add `service_tier: None` to existing `Op::OverrideTurnContext` constructions unless the specific code path is meant to change the tier.
- Do not add `/fast` yet.
- Do not persist TUI state changes beyond normal `config.toml` edits.

A later UI patch can add `/fast` as syntactic sugar over writing `service_tier = "fast"` / `"flex"` and submitting `OverrideTurnContext`, but that is not required for the minimal feature.

## Interaction with `/pause` and `/continue`

`/pause` and `/continue` should require no special service-tier logic.

The current branch's `continue_last` path builds a fresh default turn context from the current `SessionConfiguration`. Because this design stores service tier in `SessionConfiguration` and copies it into every new `TurnContext`, continued turns inherit the same sticky tier as normal turns.

If an app-server client submits `OverrideTurnContext { service_tier: Some(Flex) }` or `{ service_tier: Some(Fast) }` while paused and then calls `Continue`, the continued turn should use the newly updated tier.

## Test plan

### Config tests

Add or update tests in `core/src/config/mod.rs`:

- absent global/profile/override resolves to `None`;
- global `service_tier = "fast"` resolves to `Fast`;
- profile `service_tier = "flex"` overrides a global `fast`;
- `ConfigOverrides.service_tier = Some(Fast)` overrides profile/global;
- invalid values fail TOML deserialization.

### Protocol serde tests

Add focused tests for:

- `ServiceTier::Fast` serializes to `"fast"` in config/protocol JSON;
- `ServiceTier::Flex` serializes to `"flex"`;
- `Op::OverrideTurnContext` omission leaves `service_tier = None` after deserialization.

### Request body tests

Add tests around `ResponsesRequestBuilder` and/or the existing mocked model request tests:

- default config omits `"service_tier"` on an OpenAI HTTP Responses request;
- explicit `fast` produces `"service_tier": "priority"`;
- explicit `flex` produces `"service_tier": "flex"`;
- built-in `ollama`/`lmstudio` providers omit `service_tier` if using the recommended provider guard.

### WebSocket tests

Add or update WebSocket tests to assert:

- `response.create` omits `"service_tier"` by default for the OpenAI provider;
- explicit `fast` includes `"priority"`;
- a tier change between turns prevents incremental append reuse because the non-input part of `ResponseCreateWsRequest` differs.

### App-server tests

Add v2 tests for:

- `thread/start` with `serviceTier: "fast"` returns `serviceTier: "fast"` and sends `"priority"` on the subsequent model request;
- `thread/start` with no `serviceTier` returns `serviceTier: null`;
- `turn/start` with `serviceTier: "fast"` updates the sticky tier for that and subsequent turns;
- `turn/start` with `serviceTier: "flex"` resets a prior `fast` session to `flex`.

### Regression tests

Run at minimum:

```bash
cargo test -p codex-protocol
cargo test -p codex-core model_switching service_tier
cargo test -p codex-core agent_websocket service_tier
cargo test -p codex-app-server-protocol schema_fixtures
cargo test -p codex-app-server thread_start turn_start service_tier
```

If test names differ after implementation, use the closest package-level tests.

## Compatibility and migration notes

- Existing `config.toml` files need no migration. Absence omits the request field and lets the API select its default tier.
- Existing Rust code constructing `Op::UserTurn` / `Op::OverrideTurnContext` must add `service_tier: None` or the intended concrete tier.
- Existing app-server JSON clients can omit `serviceTier`; the request remains unspecified unless config/profile sets a tier.
- Existing TypeScript clients should regenerate from the checked-in schema.
- The behavioral change from this branch is limited to explicit configuration: OpenAI Responses requests remain unspecified by default, while explicit `fast` and `flex` config values are forwarded.

## Prerequisites

No upstream branch prerequisite port is required.

The only prerequisite inside this branch is adding the shared `ServiceTier` enum in `protocol/src/config_types.rs` before touching config, core, TUI, or app-server crates. All downstream changes can then import that one type.
