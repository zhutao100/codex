# Model catalog and discovery

## Contract

[The bundled catalog](../../core/models.json) supplies fallback model metadata, including instructions, reasoning levels, tools, visibility and context limits. Offline presets are derived from this catalog and sorted by priority; the first picker-visible model is the default. Keep one catalog rather than handwritten parallel preset/default lists.

An overlay-selected provider owns discovery from the first turn. Only a turn served by OpenAI can refresh its catalog on a response ETag. The picker and runtime resolve each model slug against the same provider: a model bound to another provider uses bundled/overlay visibility and input modalities even when OpenAI has a cached entry for the same slug. Conversely, a model explicitly bound to OpenAI can use its catalog when the configured default is another provider.

- Thread naming selects the dedicated model in [thread naming](../../core/src/thread_name.rs), with low reasoning effort and title-only instructions. If that model is unavailable or hidden, it falls back through configured/default model selection and then the current model.
- [Explorer selection](../../core/src/agent/role.rs) and [rate-limit switching](../../tui/src/chatwidget/rate_limits.rs) have their own selectors. Keep them compatible with the catalog when updating models; do not assume they share naming's fallback logic.
- Removing a bundled entry changes discovery/pickers, not the ability to configure a custom provider's explicit model identifier. Retain valid tests of custom identifiers.
- Preserve prompt strings and metadata exactly during a catalog refresh. Migration notices belong to model metadata; do not duplicate retirement dates in documentation.

## Discovery/cache version

Only the selected `openai` provider identity participates in remote discovery; fetching requires ChatGPT authentication. API-key and unauthenticated sessions may read a compatible local cache but do not fetch it. Other providers use bundled/fallback metadata and their explicit overlay; they do not inherit the OpenAI account's remote catalog or renew its ETag.

[Model discovery](../../core/src/models_manager/manager.rs) uses the compiled `CODEX_VERSION_OVERRIDE`, or `CARGO_PKG_VERSION` when unset. [Version normalization](../../core/src/models_manager/mod.rs) removes prerelease and build metadata: `1.2.3-alpha.4+build.5` becomes `1.2.3`.

Use this same normalized version in the `/models` query, persisted `models_cache.json` and compatibility checks. A mismatched cache is ignored and refreshed normally; no manual migration is needed. Runtime environment changes do not alter a compiled version. TTL and ETag behavior are independent of version normalization.

## Refresh procedure

1. Extract only the `models` array from a server-populated cache into the bundled `{"models": [...]}` object. Leave cache timestamps, ETags and local state out of the catalog.
2. Preserve metadata and instructions exactly; inspect structured summaries rather than dumping full prompts.
3. Update affected selectors, examples and snapshots; remove unused retired prompt assets. Check [custom model overlays](model-overlay.md) when changing metadata/instruction resolution.
4. Check offline presets, discovery/cache behavior, naming requests and the affected picker/app-server model-list tests.

## Test entry points

|Contract|Source/tests|
|---|---|
|Catalog conversion and one picker default|[Model presets](../../core/src/models_manager/model_presets.rs)|
|Compile-time version and suffix removal|[Version tests](../../core/src/models_manager/version_tests.rs)|
|Query/cache version, ETag renewal and reuse|[Cache TTL suite](../../core/tests/suite/models_cache_ttl.rs)|
|Naming selection and private request instructions|[Auto-rename suite](../../core/tests/suite/auto_rename_thread.rs)|
|RPC model metadata and pagination|[App-server model list](../../app-server/tests/suite/v2/model_list.rs)|

Use [scoped validation](../development.md#scoped-validation). To check a compile-time override, rebuild the focused version/cache tests with a prerelease-plus-build value such as `CODEX_VERSION_OVERRIDE=1.2.3-alpha.4+build.5`.
