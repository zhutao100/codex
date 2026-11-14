# Service-tier selection

Use this when changing service-tier config, request construction or app-server turn overrides. The value is an optional string so unknown provider-supported request ids can pass through without expanding an enum.

|Config/API selection|Effective request value|Persisted config spelling|
|---|---|---|
|Absent|Omit `service_tier`|Absent|
|`fast`|`priority`|`fast`|
|`priority`|`priority`|`fast`|
|`flex`|`flex`|`flex`|
|Other string|Unchanged|Unchanged|

## Decisions and invariants

- Absence stays `None`. It must not become an explicit `flex` request: default service behavior and a request for Flex are distinct, and an account/model may reject the latter.
- Configuration precedence is harness override, selected profile, then top-level TOML. The effective value flows through session/turn context, session-configured events and app-server v2 string fields. Turn overrides are sticky and normalize known aliases.
- `service_tier_for_wire` emits the field only for providers classified by `ModelProviderInfo::is_openai()`. That predicate currently checks the provider name, not the endpoint URL or provider id. Responses-compatible OSS/secondary providers do not automatically accept OpenAI tier fields.
- Persistence writes `priority` as `fast`; unknown strings are preserved. `ServiceTier::{Fast, Flex}` is a normalization helper, not a closed type for active config/API fields.
- There is no `/fast` command, managed fast-default resolver, or fast-default opt-out behavior in this implementation. Do not infer those product features from the existence of the request field.

## Source and verification

- [Normalization](../../protocol/src/config_types.rs), [config precedence](../../core/src/config/mod.rs), [config persistence](../../core/src/config/edit.rs) and [wire filtering](../../core/src/client.rs).
- [App-server v2 types](../../app-server-protocol/src/protocol/v2.rs) and [sticky turn override test](../../app-server/tests/suite/v2/turn_start.rs).
- Existing `service_tier` tests cover unspecified defaults, precedence, aliases, arbitrary strings, persisted spelling and provider filtering. If fields change, regenerate the config and app-server schemas through the [development workflow](../development.md) and check both SSE and WebSocket request construction.
