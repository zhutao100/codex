# Bundled Models

## Contract

- `core/models.json` is the bundled fallback for server model metadata, including exact prompts, reasoning levels, tools, context limits, visibility, and upgrade notices.
- Offline presets derive from that catalog through `ModelInfo` to `ModelPreset` conversion. The first picker-visible model by server priority is the default; do not maintain a second handwritten catalog.
- Automatic thread naming uses `gpt-6-luna` with low reasoning effort and title-only instructions for API-key and ChatGPT authentication. If Luna is hidden or unavailable, retain the configured/default-model fallback.
- Explorer agents and the rate-limit model switch also use `gpt-6-luna`.
- The refreshed catalog retains GPT-5.5 as hidden with its server-provided migration to GPT-6.1 Sol. Its announced ChatGPT retirement is October 14, 2026.
- Removed bundled entries do not prohibit explicitly configured API or custom-provider model identifiers. Historical configuration and mock fixtures may still exercise those identifiers without advertising them as available models.

## Refresh

1. Extract only the `models` array from a current server-populated `models_cache.json` into the bundled `{"models": [...]}` object. Do not publish cache timestamps, ETags, credentials, or machine paths.
2. Preserve model metadata and prompt strings exactly. Inspect structured summaries, not full prompt or JSON dumps.
3. Update built-in model selectors and catalog-dependent tests, examples, and snapshots. Remove unreferenced retired-model prompt assets.
4. Verify offline selection, localhost model discovery, automatic naming request bodies, and model/reasoning pickers.

## Verification

- `models_manager::model_presets::tests` checks the default, complete metadata conversion, and exclusion of retired presets.
- `suite::auto_rename_thread` checks Luna selection under API-key, Pro, and ProLite authentication, cached model visibility, low reasoning effort, private title instructions, and server-model mismatch handling.
- TUI model, reasoning, migration, rate-limit, and status snapshots track the bundled defaults and capabilities.
- `suite::v2::model_list` in app-server checks JSON-RPC model metadata, a single supported default, pagination, and invalid cursors.

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test --quiet --all-features -p codex-core -p codex-tui -p codex-mcp-server -j 2 -- --test-threads=2
cargo-local insta pending-snapshots --manifest-path tui/Cargo.toml
# Review every .snap.new before accepting the crate's snapshots.
cargo-local insta accept --manifest-path tui/Cargo.toml --quiet
```
