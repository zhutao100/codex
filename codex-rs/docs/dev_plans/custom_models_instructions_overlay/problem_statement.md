# Problem Statement

## Current Model Management Shape

Primary surfaces inspected:

- `core/src/models_manager/model_info.rs`
- `core/src/models_manager/manager.rs`
- `core/src/models_manager/cache.rs`
- `core/models.json`
- `protocol/src/openai_models.rs`
- `core/src/config/mod.rs`
- `core/src/config/profile.rs`
- `core/src/codex.rs`

### Model metadata sources

This branch currently has three relevant model metadata sources:

1. **Bundled metadata** in `core/models.json`.
   - `ModelsManager::new(...)` initializes `remote_models` from this file via `load_remote_models_from_file()`.
   - The name `remote_models` is therefore slightly misleading: it starts as bundled metadata and may later be overlaid by live `/models` results.
2. **Live `/models` metadata** fetched by `ModelsManager::fetch_and_update_models()` when remote model refresh is enabled and the auth mode permits it.
   - `apply_remote_models(...)` starts from bundled `core/models.json` every time, replaces matching slugs with fetched metadata, and appends fetched slugs that are not bundled.
   - This means fetched models can replace or add entries, but bundled entries remain available unless the code changes.
3. **User config overrides** in `Config`.
   - Current global runtime overrides include `model_context_window`, `model_auto_compact_token_limit`, `tool_output_token_limit`, `model_supports_reasoning_summaries`, and `base_instructions`.
   - These overrides are applied in `model_info::with_config_overrides(...)` after a `ModelInfo` candidate is selected.

`MODEL_CACHE_FILE` is `models_cache.json` under `codex_home`. It stores the fetched live `/models` response with `fetched_at`, `etag`, `client_version`, and `models`. It is a server metadata cache, not a user customization layer. Editing it by hand is not durable because the TTL, client-version check, ETag refresh, and next fetch can replace it.

### Built-in bundle observations

The inspected `core/models.json` contains six `ModelInfo` entries:

|Slug|Picker visibility|Context window|`base_instructions`|`instructions_template`|
|---|---|---:|---:|---:|
|`gpt-5.4`|`list`|`272000`|present|present|
|`gpt-5.4-mini`|`list`|`272000`|present|present|
|`gpt-5.3-codex`|`list`|`272000`|present|present|
|`gpt-5.3-codex-spark`|`list`|`128000`|present|present|
|`gpt-5.2`|`list`|`272000`|present|absent|
|`codex-auto-review`|`hide`|`272000`|present|present|

Important details:

- `gpt-5.4` and `codex-auto-review` share the same `base_instructions` and `instructions_template` bodies.
- The personality-enabled models have `model_messages.instructions_template` plus `personality_default`, `personality_friendly`, and `personality_pragmatic` variables.
- `personality_default` is an empty string in the inspected bundle.
- `gpt-5.2` has `base_instructions` but no `model_messages`, so its effective instructions fall back directly to `base_instructions`.

### Runtime instruction resolution

`ModelInfo::get_model_instructions(personality)` has the key behavior that makes the overlay problem non-trivial:

- If `model_messages.instructions_template` exists, the template is always used.
- The `{{ personality }}` placeholder is replaced with the selected personality message, or an empty string when no message is available.
- If no template exists, it falls back to `base_instructions`.

`Config.base_instructions` is a stronger existing override:

- `model_info::with_config_overrides(...)` replaces `model.base_instructions` with `config.base_instructions`.
- It also clears `model.model_messages`, which disables the personality template path.

At session creation in `core/src/codex.rs`, the final stored `SessionConfiguration.base_instructions` currently uses this priority order:

1. `config.base_instructions`
2. resumed conversation history `session_meta.base_instructions`
3. `model_info.get_model_instructions(config.personality)`

That stored session value is what later goes to the Responses API request as `instructions`, and also what the compact endpoint receives as compaction `instructions`.

## Gaps

### 1. No durable first-class custom model overlay

Users can select an unknown slug and the code falls back to `model_info_from_slug(slug)`, but there is no structured config layer to add persistent metadata for that slug.

The fallback is intentionally generic:

- `display_name = slug`
- `visibility = none`
- `priority = 99`
- `base_instructions = OTHERS_INSTRUCTIONS`
- `context_window = 163840`
- `auto_compact_token_limit = 2/3 * 163840`

That is acceptable as a safety fallback, but not sufficient for real custom deployments that need accurate context windows, compaction thresholds, shell/tool settings, picker visibility, or instruction behavior.

### 2. No per-field override for existing live or bundled models

A user cannot durably say "use upstream/bundled `gpt-5.4`, but override only `context_window`" or "use the normal `gpt-5.4` metadata, but replace only `personality_pragmatic` from a local file."

The existing config-level fields are coarse and cross-model:

- `model_context_window` applies to whichever model is selected.
- `base_instructions` replaces all model instructions for the session and disables templates.
- `model_instructions_file` is similarly global/profile-scoped rather than model-scoped.

### 3. `base_instructions` is not always the effective runtime instruction source

For personality-enabled models, `instructions_template` overrides `base_instructions` during `get_model_instructions(...)`.

This means an overlay that only changes `base_instructions` is a true metadata field override, but it is not necessarily an effective runtime instruction override. For `gpt-5.4`, `gpt-5.4-mini`, `gpt-5.3-codex`, `gpt-5.3-codex-spark`, and `codex-auto-review`, users need one of the following if they want guaranteed effective instruction changes:

- override `instructions_template` or its personality variables;
- clear/disable `model_messages`; or
- use an explicit final effective instruction override that bypasses the template/personality machinery.

### 4. Long instruction bodies are unsuitable for inline TOML

The bundled instruction fields are large Markdown-like strings. Maintaining corrected versions inline in `config.toml` would be brittle and noisy.

This branch already has `model_instructions_file` for one global base-instruction override. The custom model overlay needs the same file-backed pattern for model-scoped and personality-scoped instruction fields.

### 5. Cache mutation is the wrong extension point

`models_cache.json` is refreshed according to cache TTL and client version, and it stores only fetched live models. It should remain a cache artifact. A user customization layer must be applied after bundled/live metadata is loaded, not persisted into the cache.

## Objective

Add an optional user-controlled model overlay that works on top of the existing model management pipeline.

The overlay must support:

- custom model entries not present in `core/models.json` or live `/models` responses;
- fallback-aware custom models that start from `model_info_from_slug(slug)` and override only specified fields;
- per-field overrides for existing bundled or fetched models matched by `slug`;
- cross-model field overrides, such as a global `context_window`, applied to every selected model;
- a cross-personality final effective instruction override that bypasses `base_instructions`, `instructions_template`, and `personality_*` churn;
- file-backed instruction fields for long Markdown values;
- no behavior change when the overlay config is absent.

## Design Constraints

- Keep the overlay as an optional add-on. If no overlay config is present, `list_models`, `get_model_info`, instruction resolution, and cache behavior must remain byte-for-byte or structurally equivalent to the current behavior.
- Do not edit or reinterpret `models_cache.json` as user state.
- Keep the patch localized around config loading, model metadata resolution, and session instruction resolution.
- Avoid protocol/API schema churn unless strictly necessary. The overlay is local client config, not a new `/models` wire contract.
- Preserve existing explicit config override semantics. Existing `instructions` / `model_instructions_file` users should not be broken.
- Keep future upstream rebases easy by isolating overlay-specific code in a small module rather than threading ad hoc overlay checks through every model call site.

## Success Criteria

The design is successful if:

- a custom model can be configured with only `slug`, `context_window`, and `auto_compact_token_limit`, inheriting all other fields from `model_info_from_slug(slug)`;
- an existing model can override one field without copying the full `ModelInfo` object;
- a single cross-model `context_window` override affects every selected model unless a higher-priority override supersedes it;
- a `final_instruction_override_file` reliably controls the runtime effective system instructions regardless of the selected personality path;
- long instruction fields can be sourced from Markdown files;
- remote model refresh continues to work and the overlay is reapplied after every cache or network refresh;
- the no-overlay path remains functionally identical to the current branch.
