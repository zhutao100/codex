# Design Proposal

## Status

Implemented for the global `model_overlay` path. Per-model entries can bind a slug to a configured `model_provider`. Profile-scoped overlays, generalized null/clear markers, and broader clear semantics remain deferred; `clear_model_messages = true` is the only first-class clear operation.

## Target Base

This proposal targets this project's customized branch shape.

## Design Summary

Add a new optional `model_overlay` section to `config.toml` and resolve it into an in-memory overlay object during config loading.

Then apply the overlay in two places:

1. **Model metadata resolution**
   - Start from the same bundled/live candidate models this branch uses today.
   - For custom slugs absent from candidates, start from `model_info_from_slug(slug)`.
   - Apply cross-model metadata overrides.
   - Apply per-model metadata overrides.
   - Keep existing `model_info::with_config_overrides(...)` as the final metadata override layer.
2. **Effective instruction resolution**
   - Keep the current `ModelInfo::get_model_instructions(personality)` behavior for normal model metadata.
   - Add an explicit final instruction override path for cases where the user wants to bypass `base_instructions`, `instructions_template`, and `personality_*` substitution.
   - Treat final instruction overrides as local config, not `ModelInfo` wire fields.

No overlay config means no behavior change.

## Non-goals

- Do not mutate `core/models.json` at runtime.
- Do not mutate or persist overlay-expanded models into `models_cache.json`.
- Do not add a new provider abstraction.
- Do not change the `/models` response shape.
- Do not make custom model availability guarantees. The overlay can select a provider id for a model slug, but the selected provider still must accept the model slug.
- Do not solve hot-reload. The first implementation can resolve overlay config at process/session startup like other config fields.

## Proposed Config Shape

Use TOML because the existing user config file is TOML.

### Minimal custom model

This matches the requested fallback-aware custom model behavior. The model starts from `model_info_from_slug("new-model")`, then overrides only the two provided fields.

```toml
model = "new-model"

[model_overlay]

[[model_overlay.models]]
slug = "new-model"
context_window = 1048576
auto_compact_token_limit = 950000
```

With only those fields, the rest of the model metadata remains the fallback metadata from `model_info_from_slug(...)`.

Important picker note: `model_info_from_slug(...)` currently uses `visibility = "none"`. The minimal custom model above is usable when explicitly selected by `model = "new-model"`, but it will not appear in picker UI unless the overlay also sets `visibility = "list"`.

```toml
[[model_overlay.models]]
slug = "new-model"
model_provider = "private-provider"
display_name = "New Model"
description = "Private high-context provider model."
visibility = "list"
priority = 30
context_window = 1048576
auto_compact_token_limit = 950000
```

### Per-field override for an existing model

```toml
[model_overlay]

[[model_overlay.models]]
slug = "gpt-5.4"
base_instructions_file = "~/.codex/model-overlays/gpt-5.4-base.md"
```

This is an exact field override of `ModelInfo.base_instructions`.

For personality-enabled models, overriding only `base_instructions` does not necessarily change runtime effective instructions because `model_messages.instructions_template` takes precedence. If the user wants a guaranteed effective override, use one of the following instead:

```toml
[[model_overlay.models]]
slug = "gpt-5.4"
final_instruction_override_file = "~/.codex/model-overlays/gpt-5.4-effective.md"
```

or:

```toml
[[model_overlay.models]]
slug = "gpt-5.4"
instructions_template_file = "~/.codex/model-overlays/gpt-5.4-template.md"
```

or:

```toml
[[model_overlay.models]]
slug = "gpt-5.4"
base_instructions_file = "~/.codex/model-overlays/gpt-5.4-base.md"
clear_model_messages = true
```

`clear_model_messages = true` is a convenience for users who want the overridden `base_instructions` to become effective through the existing fallback path.

### Per-personality override for an existing model

This supports the requested exact field override mode for a single personality variable without copying the whole template.

```toml
[[model_overlay.models]]
slug = "gpt-5.4"
model_messages.instructions_variables.personality_pragmatic_file = "~/.codex/model-overlays/pragmatic.md"
```

Equivalent expanded TOML is also acceptable if the serde shape is implemented as nested structs:

```toml
[[model_overlay.models]]
slug = "gpt-5.4"

[model_overlay.models.model_messages.instructions_variables]
personality_pragmatic_file = "~/.codex/model-overlays/pragmatic.md"
```

Recommendation for implementation: support dotted keys and nested tables through the normal TOML parser; avoid a second bespoke config syntax.

### Cross-model field override

Top-level overlay fields are cross-model metadata patches.

```toml
[model_overlay]
context_window = 1048576
auto_compact_token_limit = 950000
```

These fields apply to every selected model after the bundled/live base is resolved and before per-model overrides. Existing `Config.model_context_window` and `Config.model_auto_compact_token_limit` should remain higher priority for backwards compatibility.

### Cross-personality final effective instruction override

```toml
[model_overlay]
final_instruction_override_file = "~/.codex/model-overlays/final-effective-instructions.md"
```

This bypasses the normal effective-instruction derivation from:

- `ModelInfo.base_instructions`
- `ModelInfo.model_messages.instructions_template`
- `personality_default`
- `personality_friendly`
- `personality_pragmatic`

Per-model final overrides win over the top-level final override:

```toml
[model_overlay]
final_instruction_override_file = "~/.codex/model-overlays/default-effective.md"

[[model_overlay.models]]
slug = "gpt-5.4"
final_instruction_override_file = "~/.codex/model-overlays/gpt-5.4-effective.md"
```

## Data Model

Add a local overlay module rather than extending `protocol/src/openai_models.rs` with non-wire fields.

Suggested module:

```text
core/src/models_manager/overlay.rs
```

Runtime structs should contain resolved strings, not unresolved file paths:

```rust
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ModelOverlay {
    pub(crate) cross_model: ModelInfoPatch,
    pub(crate) final_instruction_override: Option<String>,
    pub(crate) models: Vec<ModelOverlayEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ModelOverlayEntry {
    pub(crate) slug: String,
    pub(crate) patch: ModelInfoPatch,
    pub(crate) final_instruction_override: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ModelInfoPatch {
    pub(crate) display_name: Option<String>,
    pub(crate) description: Option<Option<String>>,
    pub(crate) default_reasoning_level: Option<Option<ReasoningEffort>>,
    pub(crate) supported_reasoning_levels: Option<Vec<ReasoningEffortPreset>>,
    pub(crate) shell_type: Option<ConfigShellToolType>,
    pub(crate) visibility: Option<ModelVisibility>,
    pub(crate) supported_in_api: Option<bool>,
    pub(crate) priority: Option<i32>,
    pub(crate) upgrade: Option<Option<ModelInfoUpgrade>>,
    pub(crate) base_instructions: Option<String>,
    pub(crate) model_messages: Option<ModelMessagesPatch>,
    pub(crate) clear_model_messages: bool,
    pub(crate) supports_reasoning_summaries: Option<bool>,
    pub(crate) support_verbosity: Option<bool>,
    pub(crate) default_verbosity: Option<Option<Verbosity>>,
    pub(crate) apply_patch_tool_type: Option<Option<ApplyPatchToolType>>,
    pub(crate) truncation_policy: Option<TruncationPolicyConfig>,
    pub(crate) supports_parallel_tool_calls: Option<bool>,
    pub(crate) context_window: Option<Option<i64>>,
    pub(crate) auto_compact_token_limit: Option<Option<i64>>,
    pub(crate) effective_context_window_percent: Option<i64>,
    pub(crate) experimental_supported_tools: Option<Vec<String>>,
    pub(crate) input_modalities: Option<Vec<InputModality>>,
}
```

For `Option<T>` fields in `ModelInfo`, use `Option<Option<T>>` in patches so the overlay can distinguish:

- absent patch field: keep existing value;
- `field = ...`: set value;
- explicit clear: set to `None` where TOML shape supports it.

TOML cannot directly express `null`, so clears should use explicit booleans for common cases. Minimal first implementation can avoid generalized clears and only support `clear_model_messages = true`; future work can add clear markers for other optional fields if needed.

Nested patch structs:

```rust
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ModelMessagesPatch {
    pub(crate) instructions_template: Option<String>,
    pub(crate) instructions_variables: Option<ModelInstructionsVariablesPatch>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ModelInstructionsVariablesPatch {
    pub(crate) personality_default: Option<String>,
    pub(crate) personality_friendly: Option<String>,
    pub(crate) personality_pragmatic: Option<String>,
}
```

The config-facing TOML structs can live in `core/src/config/mod.rs` or a small `core/src/config/model_overlay.rs`. They should include both inline and `_file` variants, then resolve into the runtime structs above.

## File-backed Instruction Fields

Support file variants for long instruction strings:

|Inline field|File field|
|---|---|
|`base_instructions`|`base_instructions_file`|
|`instructions_template`|`instructions_template_file`|
|`model_messages.instructions_template`|`model_messages.instructions_template_file`|
|`model_messages.instructions_variables.personality_default`|`...personality_default_file`|
|`model_messages.instructions_variables.personality_friendly`|`...personality_friendly_file`|
|`model_messages.instructions_variables.personality_pragmatic`|`...personality_pragmatic_file`|
|`final_instruction_override`|`final_instruction_override_file`|

Validation rules:

- Error if both inline and `_file` are set for the same logical field.
- Error if a configured file cannot be read.
- Error if a configured file is empty or whitespace-only.
- Use UTF-8 text, matching existing `std::fs::read_to_string(...)` behavior.
- Resolve paths consistently with existing config path fields such as `model_instructions_file` to avoid a second path-resolution model.

Implementation note: instruction files should preserve their contents as much as practical. If reusing `Config::try_read_non_empty_file(...)`, remember that it trims the loaded text. If exact trailing newlines matter, introduce a sibling helper that validates with `trim()` but returns the original contents.

## Resolution Pipeline

### Current pipeline

The existing metadata lookup is effectively:

```text
bundled core/models.json
  + optional fetched /models replacement/additions
  -> candidates
  -> longest-prefix or namespaced-suffix match
  -> fallback model_info_from_slug(slug) if no match
  -> model_info::with_config_overrides(...)
```

### Proposed metadata pipeline

```text
bundled core/models.json
  + optional fetched /models replacement/additions
  -> base candidates
  -> apply overlay cross-model patch
  -> apply overlay per-model patches for matching slugs
  -> append overlay custom models absent from candidates,
       each built from model_info_from_slug(slug), then patched
  -> longest-prefix or namespaced-suffix match
  -> fallback model_info_from_slug(slug) if still no match
  -> model_info::with_config_overrides(...)
```

Precedence for metadata fields:

|Priority|Layer|Notes|
|---:|---|---|
|1|Built-in fallback / bundled / live fetched metadata|Existing source of truth.|
|2|`model_overlay` top-level cross-model patch|Applies to every resolved model candidate.|
|3|`model_overlay.models[]` entry matching `slug`|Per-model exact field override.|
|4|Existing `Config` overrides|Preserve current behavior; config-level `model_context_window`, `model_auto_compact_token_limit`, `base_instructions`, and tool-output truncation remain strongest metadata overrides.|

Custom model behavior:

- If an overlay model slug is absent from bundled/live candidates, construct `ModelInfo` with `model_info_from_slug(slug)` and apply the per-model patch.
- If the same slug later appears in live `/models`, the overlay becomes a patch over the live model instead of the fallback model. This is useful for future server rollout, but it should be documented because inherited defaults may change.

### Candidate selection and aliases

Keep the current matching behavior:

- exact/longest-prefix matching via `find_model_by_longest_prefix(...)`;
- one-segment namespace suffix retry via `find_model_by_namespaced_suffix(...)`.

Overlay matching should be exact on the candidate slug before the current lookup rewrites the returned `ModelInfo.slug` to the requested model string.

Examples:

- `slug = "gpt-5.4"` patches the bundled/live `gpt-5.4` entry.
- Selecting `custom/gpt-5.4` can still reuse the patched `gpt-5.4` candidate through the existing namespaced-suffix behavior.
- `slug = "custom/gpt-5.4"` creates or patches that exact namespaced slug and will win because it is a longer exact prefix for `custom/gpt-5.4`.

## Effective Instruction Pipeline

### Why this must be separate from generic metadata patches

`final_instruction_override` is not a `ModelInfo` field. It is local policy for the effective system instructions sent at runtime.

Do not add `final_instruction_override` to `protocol/src/openai_models.rs`; that type is shared across core, TUI, app-server, SDK boundaries, and the `/models` wire response. Keep final overrides in local config/model-manager code.

### Proposed precedence

Resolve `SessionConfiguration.base_instructions` using:

|Priority|Source|Rationale|
|---:|---|---|
|1|Existing `config.base_instructions`|Preserve current explicit `instructions` / `model_instructions_file` semantics.|
|2|Per-model `final_instruction_override`|Strong local correction for the selected model.|
|3|Top-level `model_overlay.final_instruction_override`|Strong local correction across models.|
|4|Resumed conversation `session_meta.base_instructions`|Preserve old session behavior when no explicit current override exists.|
|5|`model_info.get_model_instructions(config.personality)`|Existing model-derived behavior.|

This differs slightly from the current code only when an overlay final override is configured. It makes the final override truly effective for both new sessions and resumed sessions unless the user also sets the pre-existing explicit `instructions` / `model_instructions_file` override.

Implementation sketch in `core/src/codex.rs`:

```rust
let overlay_final_instructions = models_manager
    .get_final_instruction_override(model.as_str(), &config)
    .await;

let base_instructions = config
    .base_instructions
    .clone()
    .or(overlay_final_instructions)
    .or_else(|| conversation_history.get_base_instructions().map(|s| s.text))
    .unwrap_or_else(|| model_info.get_model_instructions(config.personality));
```

The helper should apply the same exact/namespaced matching semantics as `get_model_info(...)`.

### Model switch update messages

`build_model_instructions_update_item(...)` currently calls:

```rust
next.model_info.get_model_instructions(next.personality)
```

If final instruction overrides are separate from `ModelInfo`, model-switch update messages must also use a shared resolver. Otherwise the initial session instructions and later model-switch instructions can diverge.

Recommended local helper:

```rust
fn effective_model_instructions(
    model_info: &ModelInfo,
    personality: Option<Personality>,
    final_instruction_override: Option<&str>,
) -> String {
    final_instruction_override
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| model_info.get_model_instructions(personality))
}
```

Then ensure `TurnContext` either carries the resolved final instruction override or carries enough model identity/config to recompute it consistently.

Minimal first implementation may avoid model-switch final override support only if model switching cannot change the system instructions in this branch. On this base, there is explicit model-switch update code, so the resolver should be shared.

## ModelsManager Integration

### Add overlay-aware candidate helpers

Keep `load_remote_models_from_file()` unchanged.

Add helpers conceptually like:

```rust
fn overlaid_candidates_for_lookup(
    candidates: &[ModelInfo],
    config: &Config,
) -> Vec<ModelInfo>;

fn overlaid_candidates_for_picker(
    candidates: &[ModelInfo],
    config: &Config,
) -> Vec<ModelInfo>;
```

Why two helpers:

- Lookup should always be able to apply per-model overlays against bundled/live metadata because `get_model_info(...)` already does so today through `remote_models` initialized from `core/models.json`.
- Picker listing currently respects `Feature::RemoteModels`; when that feature is disabled, `list_models(...)` falls back to local built-in presets. The overlay should not accidentally expose every bundled `ModelInfo` through a different code path just because one overlay is configured.

Recommended picker behavior:

- If no overlay exists: keep exact current behavior.
- If overlay exists and `Feature::RemoteModels` is enabled: apply overlay to the full remote/bundled candidate list before building presets.
- If overlay exists and `Feature::RemoteModels` is disabled: include only overlay-mentioned model entries as remote-style presets, then merge them with local presets. This allows custom models and patched picker metadata to appear without changing unrelated picker entries.

### Do not write overlay-expanded data to cache

Keep `ModelsCacheManager::persist_cache(...)` unchanged. It should continue to write fetched server models only.

Apply overlays after:

- bundled file load;
- fresh cache load;
- network fetch and `apply_remote_models(...)`.

This keeps cache invalidation simple:

- changing overlay config does not require deleting `models_cache.json`;
- server ETag behavior remains untouched;
- fetched metadata can change, and the local overlay is reapplied deterministically.

## Patch Application Semantics

### Applying `ModelInfoPatch`

A patch updates only fields explicitly set in the overlay.

Pseudo-code:

```rust
fn apply_patch(mut model: ModelInfo, patch: &ModelInfoPatch) -> ModelInfo {
    if let Some(display_name) = &patch.display_name {
        model.display_name = display_name.clone();
    }
    if let Some(context_window) = patch.context_window {
        model.context_window = context_window;
    }
    if let Some(auto_limit) = patch.auto_compact_token_limit {
        model.auto_compact_token_limit = auto_limit;
    }
    if let Some(base) = &patch.base_instructions {
        model.base_instructions = base.clone();
    }
    if patch.clear_model_messages {
        model.model_messages = None;
    }
    if let Some(model_messages_patch) = &patch.model_messages {
        let mut messages = model.model_messages.unwrap_or(ModelMessages {
            instructions_template: None,
            instructions_variables: None,
        });
        apply_model_messages_patch(&mut messages, model_messages_patch);
        model.model_messages = Some(messages);
    }
    model
}
```

Ordering detail: if both `clear_model_messages = true` and `model_messages.*` fields are present on the same patch, prefer rejecting the config as invalid. It is clearer than silently clearing and then recreating.

### Warning for ineffective base-instruction patches

If a per-model patch sets `base_instructions` while the resulting model still has `model_messages.instructions_template`, log a warning unless the same patch also sets:

- `instructions_template` / `model_messages.instructions_template`,
- `final_instruction_override`, or
- `clear_model_messages = true`.

The warning should explain that `instructions_template` remains the effective runtime instruction source for personality-enabled models.

## Config Loading Plan

### Minimal new fields

Add to `ConfigToml`:

```rust
#[serde(default)]
pub model_overlay: Option<ModelOverlayToml>,
```

Add to `Config`:

```rust
pub model_overlay: Option<ModelOverlay>,
```

The first implementation supports only global `model_overlay`. Profile-scoped model overlays are useful, but they increase merge semantics and validation surface. When added later, `ConfigProfile` can grow its own `model_overlay: Option<ModelOverlayToml>` field and use the existing profile precedence convention: profile overlay is merged over global overlay.

### File resolution timing

Resolve all `_file` fields while building `Config`, not inside `ModelsManager`.

Reasons:

- config loading already owns path-resolution and IO error reporting patterns;
- `ModelsManager` can remain mostly pure over already-resolved strings;
- tests can instantiate `Config` with a fully-resolved `ModelOverlay` without touching disk.

### Merge semantics for global + profile overlay

If profile overlays are implemented:

1. Start with global top-level cross-model patch.
2. Merge profile top-level cross-model patch over it.
3. Concatenate model entries by slug, with profile entries overriding same-slug global entries field-by-field.
4. Per-model final overrides follow the same merge rule.

The first patch keeps profile-level `model_overlay` unsupported; `ConfigProfile` continues to deny unknown fields, so profile-level overlays fail config validation instead of being silently ignored.

## Implementation Plan

### 1. Add overlay structs and parser support

Files:

- `core/src/config/mod.rs`
- optionally `core/src/config/model_overlay.rs`
- `core/src/models_manager/overlay.rs`

Work:

- Add TOML-facing structs with inline and `_file` fields.
- Add resolved runtime structs with strings only.
- Add validation for duplicate inline/file fields and empty files.
- Add tests for parsing, file loading, and invalid combinations.

### 2. Add overlay patch application

Files:

- `core/src/models_manager/overlay.rs`
- `core/src/models_manager/model_info.rs` or keep patch application in `overlay.rs`

Work:

- Implement `apply_cross_model_patch(...)`.
- Implement `apply_per_model_patch(...)`.
- Implement custom model construction from `model_info_from_slug(slug)`.
- Implement warnings for ineffective `base_instructions` patches.

### 3. Wire overlay into `ModelsManager`

File:

- `core/src/models_manager/manager.rs`

Work:

- In `get_model_info(...)`, apply overlay to candidate models before `construct_model_info_from_candidates(...)`.
- In `list_models(...)` / `try_list_models(...)`, apply overlay to picker candidates without changing no-overlay behavior.
- Keep `load_remote_models_from_file()`, `apply_remote_models(...)`, and `ModelsCacheManager` cache serialization unchanged.

### 4. Add final instruction override resolver

Files:

- `core/src/models_manager/manager.rs`
- `core/src/codex.rs`

Work:

- Add a helper to resolve per-model or top-level final instruction override for the selected model.
- Update session base-instruction selection to insert overlay final override between `config.base_instructions` and resumed history.
- Update model-switch instruction update code to use the same final resolver.

### 5. Preserve existing config override priority

File:

- `core/src/models_manager/model_info.rs`

Work:

- Keep `with_config_overrides(...)` as the final metadata override layer.
- Do not reinterpret existing `base_instructions` or `model_instructions_file` semantics.
- Add tests proving existing config overrides still win over overlay metadata fields.

## Test Plan

### No-overlay regression tests

- `construct_model_info_offline("gpt-5.4", config_without_overlay)` equals current output.
- `list_models(...)` ordering and default selection are unchanged without overlay.
- Bundled `core/models.json` still round-trips through `ModelsResponse`.
- `models_cache.json` payload is unchanged by overlay code when overlay is absent.

### Custom model tests

- A custom slug with only `context_window` and `auto_compact_token_limit` inherits all other fields from `model_info_from_slug(slug)`.
- A custom slug selected by `model = "new-model"` returns `ModelInfo.slug == "new-model"`.
- A custom slug with `visibility = "list"` appears in picker output.
- A custom slug without `visibility = "list"` remains hidden but selectable by explicit config.

### Existing model per-field override tests

- `gpt-5.4` with only `context_window` changes only `context_window`.
- `gpt-5.4` with `base_instructions_file` changes `ModelInfo.base_instructions`.
- `gpt-5.4` with `personality_pragmatic_file` changes only that personality variable.
- `clear_model_messages = true` makes `get_model_instructions(...)` return `base_instructions`.
- Setting both `clear_model_messages = true` and `model_messages.*` in the same entry is invalid.

### Cross-model override tests

- Top-level `context_window` applies to all selected models.
- Per-model `context_window` wins over top-level `context_window` for that slug.
- Existing `Config.model_context_window` wins over overlay top-level and per-model values.

### Final instruction override tests

- Top-level `final_instruction_override_file` wins over `base_instructions`, `instructions_template`, and selected personality.
- Per-model `final_instruction_override_file` wins over top-level final override.
- Existing `config.base_instructions` wins over overlay final override.
- Resumed history base instructions are bypassed when overlay final override is configured.
- Model-switch update items use the same final override as session initialization.

### Cache interaction tests

- Fresh cache load still avoids network fetch.
- Stale cache still refetches from `/models`.
- Overlay custom models are present after cache load and after network refresh.
- Persisted cache JSON contains only fetched server models, not overlay-generated custom models.

### File-backed config tests

- Inline and `_file` conflict returns a config error.
- Missing file returns a contextual config error.
- Empty file returns a config error.
- File content is preserved according to the chosen helper semantics.

## Rebase-Sensitive Surfaces

These files are likely to drift in upstream rebases and should be inspected first:

- `protocol/src/openai_models.rs`
  - `ModelInfo` fields
  - `ModelMessages` / `ModelInstructionsVariables` shape
  - `ModelInfo::get_model_instructions(...)`
  - `ModelPreset::from(ModelInfo)` and merge/filter behavior
- `core/src/models_manager/manager.rs`
  - `remote_models` initialization
  - `load_remote_models_from_file()`
  - live `/models` refresh and cache application
  - candidate lookup behavior for prefix and namespaced suffixes
- `core/src/models_manager/model_info.rs`
  - `model_info_from_slug(...)`
  - `with_config_overrides(...)`
- `core/src/config/mod.rs` and `core/src/config/profile.rs`
  - config schema deny-unknown-fields behavior
  - path resolution for file-backed config
  - profile/global merge rules
- `core/src/codex.rs`
  - session base-instruction precedence
  - model-switch instruction update item construction
  - `TurnContext` fields if final overrides need to be carried per turn
- `core/src/models_manager/cache.rs`
  - cache payload shape and TTL/version checks

## Rollout Strategy

### Phase 1: metadata overlay and file-backed instruction fields

Implement:

- global `model_overlay` config;
- per-model patches;
- custom model fallback construction;
- file-backed instruction fields;
- no-overlay regression tests;
- cache interaction tests.

This phase already enables custom models and most per-field overrides.

### Phase 2: final effective instruction override

Implement:

- top-level and per-model `final_instruction_override(_file)`;
- session initialization resolver;
- model-switch update resolver;
- resumed-session precedence tests.

This phase is the critical fix path for cases where changing developer/user messages is insufficient and the runtime effective system instructions must be corrected.

### Phase 3: optional profile overlays and broader clear semantics

Consider after Phase 1/2 are stable:

- profile-scoped overlay merging;
- generalized clear markers for optional `ModelInfo` fields;
- diagnostics surfaced in UI instead of logs only;
- an inspection/debug command that prints the effective overlaid `ModelInfo` and effective instructions source.

## Operational Notes

- Users should prefer `final_instruction_override_file` when they need guaranteed effective instruction replacement.
- Users should prefer `instructions_template_file` or per-personality file overrides when they want to preserve personality support while patching the instruction body.
- Users should not edit `models_cache.json`; it is still only a cache.
- Users should set `visibility = "list"` for custom models they want to appear in picker UI.
- Users should set `model_provider` on custom models that are served by a non-default provider.
- Runtime provider resolution is overlay-aware by default: `model_provider` is the fallback provider, and per-model overlay bindings can route matching slugs elsewhere. Delegate/task-specific provider overrides pin the provider and intentionally ignore overlay provider bindings.
- Provider compatibility is still enforced by the provider. A locally configured slug can still fail at request time if the selected model provider rejects it.
