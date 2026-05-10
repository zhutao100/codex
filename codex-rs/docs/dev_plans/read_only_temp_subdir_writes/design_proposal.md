# ReadOnly dedicated temp subdir writes

## Goal

Add a narrow, opt-in write surface for `ReadOnly` sandbox sessions:

```toml
sandbox_mode = "read-only"

[sandbox_read_only]
writeable_slash_tmp_subdir = true
writeable_tmpdir_env_var_subdir = true
```

When enabled, a `ReadOnly` session may write only to dedicated per-session subdirectories under `/tmp` and/or `$TMPDIR`. The parent temp directories and pre-existing files under those parents remain protected from mutation.

The `<permissions instructions>` block must include the exact writable subdirectory paths for the active session so the agent does not infer that all of `/tmp` or `$TMPDIR` is writable.

## Non-goals

- Do not make `/tmp` writable in `ReadOnly`.
- Do not make `$TMPDIR` writable in `ReadOnly`.
- Do not add arbitrary `ReadOnly` writable roots.
- Do not change `WorkspaceWrite` semantics.
- Do not add Windows sandbox behavior.
- Do not spend dedicated effort on Linux beyond keeping the abstraction portable where straightforward.

## Current behavior summary

Current `ReadOnly` behavior is effectively:

```text
allow filesystem reads
block normal filesystem writes
allow only base device/PTY write exceptions needed to run commands
block network
```

`ReadOnly` currently returns no writable roots. `WorkspaceWrite` is the mode that computes writable roots from configured roots, `sandbox_policy_cwd`, `/tmp`, and `$TMPDIR`, with `exclude_slash_tmp` and `exclude_tmpdir_env_var` controlling the default temp parent roots.

On macOS, enforcement is generated as a Seatbelt policy executed via `/usr/bin/sandbox-exec`. `WorkspaceWrite` already emits `file-write*` rules for specific writable roots after canonicalization.

Relevant implementation anchors:

- `protocol/src/protocol.rs`: sandbox policy model and writable-root computation.
- `core/src/config/types.rs`: config structs.
- `core/src/config/mod.rs`: config parsing and policy construction.
- `core/src/seatbelt.rs`: macOS Seatbelt policy generation.
- the prompt/turn context path that renders the `<permissions instructions>` block.

## Proposed user-visible config

Add a small `ReadOnly`-specific config table:

```toml
[sandbox_read_only]
# Default: false
writeable_slash_tmp_subdir = false

# Default: false
writeable_tmpdir_env_var_subdir = false
```

| Field | Default | Effect |
| --- | ---: | --- |
| `writeable_slash_tmp_subdir` | `false` | Create and allow writes only under a dedicated per-session child of `/tmp`. |
| `writeable_tmpdir_env_var_subdir` | `false` | Create and allow writes only under a dedicated per-session child of `$TMPDIR`, when `$TMPDIR` is set to an absolute path. |

This table applies only when the effective sandbox mode is `ReadOnly`. In other modes, these fields should be ignored or warned as unused; they must not alter `WorkspaceWrite` root computation.

## Runtime path semantics

For each enabled field, create one session-owned temp subdirectory:

```text
/tmp/codex-readonly-<session-or-random-id>/
$TMPDIR/codex-readonly-<session-or-random-id>/
```

Implementation rules:

1. Create the directory once per session and reuse it across turns.
2. Use a collision-resistant suffix, not a predictable user-controlled path.
3. Set permissions to owner-only where the platform supports it, e.g. `0700` on Unix.
4. Canonicalize the created path before passing it to Seatbelt, so macOS `/tmp` resolves consistently to `/private/tmp`.
5. Deduplicate canonical paths. If `$TMPDIR` resolves inside `/tmp` or resolves to the same directory, expose one writable root.
6. Fail closed. If an enabled subdirectory cannot be created or canonicalized, omit that writable root and surface a clear diagnostic rather than widening to the parent temp directory.
7. Cleanup is best-effort only. Correctness must come from the sandbox root restriction, not from deletion at session end.

## Sandbox policy model

Keep the external mode name as `read-only`, but allow the runtime policy to carry an explicit list of temporary writable roots:

```rust
SandboxPolicy::ReadOnly {
    temp_writable_roots: Vec<WritableRoot>,
}
```

The empty vector preserves existing behavior.

The dedicated temp roots should use the same low-level root representation and macOS Seatbelt emission path as `WorkspaceWrite` roots where possible. This avoids introducing a second write-policy generator.

Conceptually:

```text
ReadOnly without temp roots =
  base Seatbelt policy
  + allow file-read*

ReadOnly with temp roots =
  base Seatbelt policy
  + allow file-read*
  + allow file-write* only under the dedicated temp subdir roots
```

Do not reuse `WorkspaceWrite` policy construction wholesale if doing so would accidentally add `sandbox_policy_cwd`, `/tmp`, `$TMPDIR`, configured `writable_roots`, or network behavior.

## macOS Seatbelt behavior

For macOS, generate write allowances only for the dedicated roots:

```scheme
(allow file-write* (subpath (param "READONLY_TEMP_WRITABLE_ROOT_0")))
```

or reuse the existing writable-root parameter machinery if the naming can remain clear in logs/tests.

The resulting policy must deny writes to:

```text
/tmp/<anything except the exact dedicated child>
$TMPDIR/<anything except the exact dedicated child>
workspace files
other filesystem locations
```

It should allow writes to:

```text
/private/tmp/codex-readonly-<id>/...
/private/var/folders/.../T/codex-readonly-<id>/...
```

when those exact roots correspond to enabled, successfully created session directories.

## `<permissions instructions>` update

The session prompt must describe exact writable paths, not parent capabilities.

Example when both knobs are enabled:

```xml
<permissions instructions>
Filesystem access is read-only except for these dedicated session temp directories:
- /private/tmp/codex-readonly-a1b2c3d4
- /private/var/folders/xx/.../T/codex-readonly-a1b2c3d4

You may write inside those exact directories. Do not write to other paths under /tmp or $TMPDIR.
Network access is disabled unless separately approved.
</permissions instructions>
```

Example when neither knob is enabled: keep the current `ReadOnly` wording unchanged.

If one requested temp directory cannot be created, omit it from the writable list and include a concise warning in the instructions or session diagnostics. Do not print a template path that is not actually writable.

## Config and implementation touch points

Recommended minimal implementation sequence:

1. Add `SandboxReadOnlyConfig` to `core/src/config/types.rs`:

   ```rust
   #[derive(Debug, Clone, Default, Deserialize, PartialEq)]
   pub struct SandboxReadOnlyConfig {
       #[serde(default)]
       pub writeable_slash_tmp_subdir: bool,
       #[serde(default)]
       pub writeable_tmpdir_env_var_subdir: bool,
   }
   ```

2. Add `sandbox_read_only: SandboxReadOnlyConfig` to the root config type and parsing path in `core/src/config/mod.rs`.
3. During session/config materialization, if effective mode is `ReadOnly`, create the requested per-session directories and attach their canonical paths to the runtime `SandboxPolicy::ReadOnly` value.
4. Extend `protocol/src/protocol.rs` policy helpers so `ReadOnly` can return only these temp writable roots for Seatbelt emission without inheriting `WorkspaceWrite` defaults.
5. Extend `core/src/seatbelt.rs` so `ReadOnly` with non-empty temp writable roots emits the same shape of `file-write*` rule used for exact root writes.
6. Update the `<permissions instructions>` rendering path to enumerate the exact roots from the runtime policy.
7. Add focused macOS tests.

## Test plan

### macOS sandbox tests

Add coverage for `ReadOnly` with both knobs disabled:

- writing in the workspace fails;
- writing `/tmp/codex-parent-probe` fails;
- writing `$TMPDIR/codex-parent-probe` fails;
- network remains blocked.

Add coverage for `writeable_slash_tmp_subdir = true`:

- writing inside the generated `/tmp/...` child succeeds;
- writing to a sibling path under `/tmp` fails;
- writing to the workspace still fails;
- the emitted instructions contain the exact canonical child path and do not describe `/tmp` as writable.

Add coverage for `writeable_tmpdir_env_var_subdir = true`:

- writing inside the generated `$TMPDIR/...` child succeeds;
- writing to a sibling path under `$TMPDIR` fails;
- missing or relative `$TMPDIR` fails closed;
- the emitted instructions contain the exact canonical child path.

Add coverage for both knobs enabled:

- both roots are writable when distinct;
- duplicate canonical roots are deduplicated;
- instructions contain each effective root exactly once.

### Regression tests

- `WorkspaceWrite` still honors `exclude_slash_tmp` and `exclude_tmpdir_env_var` unchanged.
- `--add-dir` remains ignored for effective `ReadOnly`.
- `sandbox_permissions = "require_escalated"` behavior is unchanged: an approved escalated command bypasses the sandbox rather than merely expanding temp roots.

## Acceptance criteria

- Default `ReadOnly` behavior is unchanged.
- Enabling either config field creates exactly one dedicated per-session writable child under the corresponding temp parent.
- The parent `/tmp` and `$TMPDIR` directories are not writable as a consequence of this feature.
- The macOS Seatbelt policy allows writes only under the canonical dedicated child paths.
- The `<permissions instructions>` block lists the exact writable paths for the current session.
- `WorkspaceWrite` behavior and config compatibility are unchanged.
