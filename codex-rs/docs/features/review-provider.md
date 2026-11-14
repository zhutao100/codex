# Review model and provider selection

Use this when changing `/review`, `/review-completed-turn`, or detached review routing. The shared configuration selects a review model/provider without changing the parent's model selection or account auth manager. See [completion review](post_turn_completion_review/README.md) for the separate review lifecycle and provenance contract.

## Configuration

Set review keys at the top level, before any TOML table header:

```toml
review_model = "custom-reviewer"

[model_providers.review-service]
name = "Review service"
base_url = "https://review.example.com/v1"
env_key = "REVIEW_API_KEY"
wire_api = "responses"
requires_openai_auth = false
supports_websockets = false

[[model_overlay.models]]
slug = "custom-reviewer"
model_provider = "review-service"
display_name = "Custom reviewer"
visibility = "list"
context_window = 200000
```

The endpoint and context window are examples to replace with the provider's actual contract. An explicit top-level `review_model_provider = "review-service"` overrides the overlay binding. Placing either review key under `model_overlay` or a model entry is rejected with a targeted TOML-scoping error.

## Resolution and isolation

|Choice|Resolution|
|---|---|
|Review model|`review_model`, otherwise the parent turn's model.|
|Review provider|Explicit `review_model_provider`, otherwise the selected model's exact overlay binding when overlay routing is enabled, otherwise the inherited provider.|
|Explicit task provider|Pins the cloned delegate config, so the model overlay cannot redirect it. Unknown provider ids surface an error.|
|Secondary model metadata|Use a local model overlay. Provider-selected inline delegates disable remote model refresh and reuse the existing metadata manager.|
|Review instructions|The task's review prompt is an explicit base-instruction override and wins over overlay final instructions.|

Ordinary review and completion review share `configure_review_delegate_config`; their sandbox and instruction profiles differ deliberately. Ordinary review receives the parent turn's sandbox. Completion review requests a read-only policy and the completed candidate's working directory. Both disable web search and recursive collaboration and use the task's host-instruction filename precedence. Read-only temp exceptions, if configured, are applied during session initialization; see [read-only temp directories](read-only-temp.md).

Detached app-server review applies the same configured model/provider overrides before creating its review thread. It remains a distinct thread delivery mode, not a replacement for inline [delegate status](delegate-runtime-status.md).

## Request authentication

`resolve_request_auth` selects a provider-local `env_key` credential first, then `experimental_bearer_token`, then shared account auth. A provider-local credential is classified as API-key auth, omits the ChatGPT account id, and disables ChatGPT unauthorized recovery. Shared ChatGPT auth enables recovery only when `requires_openai_auth` is true.

Provider selection alone does not isolate credentials: without a provider-local key/token, the resolver can fall back to shared account auth. Configure the secondary provider's `env_key` explicitly. A missing named environment variable is an error; do not silently retry that provider with a different credential source.

Keep account/session auth ownership separate from per-request credential resolution. A secondary API-key provider does not require a second persisted login or mutation of the parent's model cache. Its 401 response must not become an unrelated ChatGPT refresh failure.

## Source and verification

- [Review delegate configuration](../../core/src/tasks/review.rs), [completion-review entry](../../core/src/tasks/post_turn_completion_review.rs) and [detached app-server review](../../app-server/src/codex_message_processor.rs).
- [Provider selection](../../core/src/config/mod.rs) and [request authentication](../../core/src/api_bridge.rs).
- [Core review tests](../../core/tests/suite/review.rs) capture primary and secondary requests, overlay fallback, credentials and unknown-provider errors. [App-server review tests](../../app-server/tests/suite/v2/review.rs) cover inline/detached delivery. Auth unit tests distinguish provider-local API-key classification from ChatGPT recovery. Preserve these boundary assertions when changing provider routing.
