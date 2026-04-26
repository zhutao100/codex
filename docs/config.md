# Configuration

For basic configuration instructions, see [this documentation](https://developers.openai.com/codex/config-basic).

For advanced configuration instructions, see [this documentation](https://developers.openai.com/codex/config-advanced).

For a full configuration reference, see [this documentation](https://developers.openai.com/codex/config-reference).

## Connecting to MCP servers

Codex can connect to MCP servers configured in `~/.codex/config.toml`. See the configuration reference for the latest MCP server options:

- https://developers.openai.com/codex/config-reference

## Apps (Connectors)

Use `$` in the composer to insert a ChatGPT connector; the popover lists accessible
apps. The `/apps` command lists available and installed apps. Connected apps appear first
and are labeled as connected; others are marked as can be installed.

## Notify

Codex can run a notification hook when the agent finishes a turn. See the configuration reference for the latest notification settings:

- https://developers.openai.com/codex/config-reference

## JSON Schema

The generated JSON Schema for `config.toml` lives at `codex-rs/core/config.schema.json`.

## Model Overlay

`model_overlay` lets local config patch bundled or fetched model metadata, or add metadata for a custom model slug. It is local client config only; it does not edit `core/models.json` or `models_cache.json`.

```toml
model = "private-model"

[model_overlay]
context_window = 1048576
auto_compact_token_limit = 950000

[[model_overlay.models]]
slug = "private-model"
display_name = "Private Model"
visibility = "list"
final_instruction_override_file = "~/.codex/model-overlays/private.md"
```

For personality-enabled models, `base_instructions` is not always the effective runtime instruction source because `model_messages.instructions_template` can take precedence. Use `final_instruction_override` or `final_instruction_override_file` when you need the final system instructions to be replaced regardless of personality.

## Service Tier

`service_tier` controls the OpenAI Responses service tier. Supported values are `"flex"` and `"fast"`. If omitted, Codex leaves the request unspecified so the API uses its default tier. Explicit `"fast"` is sent to OpenAI Responses as `"priority"`.

```toml
service_tier = "fast"
```

## Notices

Codex stores "do not show again" flags for some UI prompts under the `[notice]` table.

Ctrl+C/Ctrl+D quitting uses a ~1 second double-press hint (`ctrl + c again to quit`).
