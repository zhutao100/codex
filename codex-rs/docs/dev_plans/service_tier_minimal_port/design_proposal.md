# Minimal Service Tier Backport

## Status

Implemented in this branch. This document records the current selective backport
of upstream service-tier behavior after the original minimal port.

The update aligns with these upstream changes while preserving this branch's
local default policy:

- `317213fd`: config TOML accepts string service-tier request ids.
- `178c3d30`: persisted `priority` tier selections are written as `fast`.
- Branch policy: absent config keeps default-tier semantics, but is not emitted
  as explicit `"flex"` because some accounts/models reject that request value.

## Runtime Semantics

| User/config intent | Effective runtime request id | OpenAI Responses JSON |
| --- | --- | --- |
| field absent | none (default tier semantics) | omit `service_tier` |
| `service_tier = "flex"` | `flex` | `"service_tier": "flex"` |
| `service_tier = "fast"` | `priority` | `"service_tier": "priority"` |
| `service_tier = "priority"` | `priority` | `"service_tier": "priority"` |
| `service_tier = "<unknown>"` | `<unknown>` | `"service_tier": "<unknown>"` |

The request body still omits `service_tier` for built-in non-OpenAI providers
that share the Responses-shaped transport. This keeps local OSS providers from
receiving OpenAI-specific fields they may reject.

## Non-Goals

- Do not port upstream's managed fast default resolver.
- Do not port `Feature::FastMode`.
- Do not port `notice.fast_default_opt_out`.
- Do not add a `Standard` variant.
- Do not add TUI `/fast` command surface in this backport.
- Do not introduce config-lock behavior; this branch has no config-lock module.

## Selected Design

### Config Shape

TOML-facing config accepts service-tier strings:

```rust
pub service_tier: Option<String>
```

This applies to:

- `ConfigToml`
- `ConfigProfile`
- `ConfigOverrides`

Precedence remains conventional:

```rust
let service_tier = service_tier_override
    .or(config_profile.service_tier)
    .or(cfg.service_tier)
    .map(ServiceTier::normalize_request_value);
```

The effective runtime `Config.service_tier` is also `Option<String>`, but it
stores only explicit config/API selections. It stays `None` when all explicit
config sources are absent so the request path can omit `service_tier`.

### Normalization Helpers

`ServiceTier` remains in `protocol/src/config_types.rs` as a small helper enum
and compatibility type:

```rust
pub enum ServiceTier {
    Fast,
    Flex,
}
```

Helper behavior:

- `ServiceTier::Fast.request_value()` returns `priority`.
- `ServiceTier::Flex.request_value()` returns `flex`.
- `ServiceTier::from_request_value("fast" | "priority")` returns `Fast`.
- `ServiceTier::from_request_value("flex")` returns `Flex`.
- unknown request ids return `None` and pass through unchanged.

### Runtime Propagation

Carry the explicit string id through the runtime path:

- `Config.service_tier`
- `SessionConfiguration.service_tier`
- `SessionSettingsUpdate.service_tier`
- `TurnContext.service_tier`
- `ThreadConfigSnapshot.service_tier`
- `SessionConfiguredEvent.service_tier`
- app-server v2 thread/turn params and responses

Sticky updates normalize known aliases before storing:

```rust
next_configuration.service_tier =
    Some(ServiceTier::normalize_request_value(service_tier));
```

### Wire Emission

`core/src/client.rs` maps runtime strings to request body strings only for the
OpenAI provider:

```rust
fn service_tier_for_wire(
    provider: &ModelProviderInfo,
    service_tier: Option<String>,
) -> Option<String> {
    if !provider.is_openai() {
        return None;
    }

    service_tier.map(ServiceTier::normalize_request_value)
}
```

Because absence remains `None`, OpenAI requests omit `service_tier` unless
config/profile/API override supplies an explicit value. This fixes the
`Unsupported service_tier: flex` failure caused by treating default behavior as
an explicit flex request.

### Persistence Edits

`ConfigEdit::SetServiceTier` persists config-friendly spellings:

| Input selection | Persisted TOML |
| --- | --- |
| `priority` | `service_tier = "fast"` |
| `fast` | `service_tier = "fast"` |
| `flex` | `service_tier = "flex"` |
| unknown string | unchanged |

This keeps config files aligned with user-facing tier names while preserving
unknown experimental ids.

## Schema Impact

- `core/config.schema.json` exposes `service_tier` as a string, not a closed enum.
- App-server v2 schema exposes service-tier params/responses as strings.
- `ServiceTier` may still appear where legacy protocol helper types are exported,
  but active service-tier config and runtime APIs should not restrict values to
  `fast | flex`.

## Compatibility Notes

- Existing `service_tier = "fast"` config continues to work and sends
  `priority`.
- Existing `service_tier = "flex"` config continues to work.
- Existing clients sending v2 `serviceTier: "fast"` or `"flex"` continue to work.
- New clients may send arbitrary string request ids through config or v2
  service-tier fields.
- Existing omitted service-tier config continues to use default-tier semantics
  without sending an explicit `service_tier` field.

## Verification Plan

Minimum targeted verification for this backport:

```bash
just write-config-schema
just write-app-server-schema
just fmt
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-protocol
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-core service_tier
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-core --lib
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-app-server-protocol schema_fixtures
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-app-server service_tier
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-app-server thread_start_creates_thread_and_emits_started
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-tui service_tier
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-exec
CODEX_SANDBOX_NETWORK_DISABLED=1 just fix -p codex-protocol -p codex-core -p codex-app-server-protocol -p codex-app-server -p codex-tui -p codex-exec
```
