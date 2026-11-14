# Model metadata and instruction overlays

Use this when adding a model, changing model-specific instructions, or routing a model to a configured provider. For server discovery and bundled defaults, see [model catalog](model-catalog.md).

## Configuration contract

`[model_overlay]` patches model metadata across candidates; `[[model_overlay.models]]` patches one slug and may bind it to a provider. The overlay is applied in memory after catalog loading and before ordinary explicit config overrides. It never writes expanded entries into the server model cache.

```toml
[[model_overlay.models]]
slug = "custom-reviewer"
model_provider = "review-service"
display_name = "Custom reviewer"
visibility = "list"
context_window = 200000
final_instruction_override_file = "prompts/custom-reviewer.md"
```

`review-service` must exist under `model_providers`. The example instruction file must exist; paths resolve through the config loader relative to their defining config layer. Use the [config schema](../../core/config.schema.json) for the supported metadata fields rather than copying a full model dump into documentation.

|Operation|Semantics|
|---|---|
|Omitted field|Preserve the candidate value.|
|Cross-model patch plus per-model patch|Apply cross-model fields first, then the matching entry.|
|List field|Replace the list; do not append.|
|New slug with metadata or provider binding|Create a candidate from the existing fallback model builder, then apply patches.|
|Entry containing only a final instruction override|Affects instruction resolution but does not create a picker candidate.|
|`clear_model_messages = true`|Remove template/personality messages so base instructions can take effect. Reject a simultaneous nonempty template/message patch.|
|Inline text and its `_file` variant|Mutually exclusive. File contents are read once during config loading and preserved exactly; unreadable or whitespace-only files are errors.|

The overlay is global. Profile-scoped overlays and generalized null/clear operations are not implemented. Avoid speculative config examples for those extensions.

## Effective instructions

Changing `base_instructions` metadata alone can have no visible effect when the model still has an active instruction template. Choose the layer that expresses the intended change:

|Intent|Supported field|
|---|---|
|Replace fallback/base model text|`base_instructions` or `base_instructions_file`; clear model messages if the template must stop applying.|
|Replace the instruction template|`instructions_template` / `_file`, or the nested `model_messages.instructions_template` / `_file`; the two forms cannot both be supplied.|
|Change one personality|`model_messages.instructions_variables.personality_default`, `personality_friendly`, or `personality_pragmatic`, each with an optional `_file` alternative.|
|Replace effective model text for every personality|`final_instruction_override` or `final_instruction_override_file`, at cross-model or per-model scope.|

Session initialization resolves base instructions in this order: explicit `Config.base_instructions`, matching per-model or cross-model final override, persisted session base instructions, then rendered model template/base text. The TOML-facing explicit override is `model_instructions_file`; `Config.base_instructions` is also set programmatically by task delegates.

An explicit review prompt therefore takes precedence over an overlay final override. Model switches append the resolved model instruction update; a final override suppresses personality-specific update messages. Keep these paths consistent with initial session resolution and compaction.

## Matching and routing decisions

- Metadata uses the longest matching slug prefix, with one narrowly constrained namespace fallback; the requested slug remains the wire model identifier. Final instruction lookup follows the selected candidate so a shorter overlay prefix does not override a more specific model.
- Provider binding uses the exact configured model slug. Metadata alias/prefix matching does not imply provider alias routing.
- Normal configuration is overlay-aware: the session provider is a fallback for models without a binding. A derived config explicitly pinned by `apply_model_provider_id` keeps its selected provider even if the model has an overlay binding. [Review provider selection](review-provider.md) uses this distinction.
- Validate provider ids at configuration load. Keep credentials and endpoints in provider configuration, separate from model metadata.

## Source and verification

- [Overlay resolution](../../core/src/models_manager/overlay.rs): patch semantics, file-backed text and invalid combinations.
- [ModelsManager](../../core/src/models_manager/manager.rs): candidate selection, final overrides, picker behavior and cache separation.
- [Configuration](../../core/src/config/mod.rs) and [session instruction resolution](../../core/src/session/mod.rs): provider pinning and effective instruction precedence.
- Existing tests in those modules cover file conflicts, empty files, exact text, provider validation, metadata matching, explicit config precedence and cache non-persistence. [Session tests](../../core/src/session/tests.rs) cover override precedence and model-switch context updates. When changing this contract, extend the relevant test instead of relying on a live model's response style.
