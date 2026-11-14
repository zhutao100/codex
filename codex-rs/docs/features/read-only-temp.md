# Dedicated writable temp directories in read-only sessions

Use this when changing read-only policy construction, sandbox roots or permissions messages. The opt-in exception grants writes inside generated temporary children while preserving the `read-only` mode and its restricted network policy.

```toml
sandbox_mode = "read-only"

[sandbox_read_only]
writeable_slash_tmp_subdir = true
writeable_tmpdir_env_var_subdir = true
```

Both fields default to `false`; the spelling `writeable` is the implemented config key. They have no effect when the effective policy is not read-only.

## Runtime contract

- During session initialization, create a `codex-readonly-<UUID>` child under each enabled parent, `/tmp` and/or `$TMPDIR`. Unix directories use mode `0700`. The runtime policy retains the canonical child paths across that runtime's turns; a restarted/resumed session initializes temp roots again.
- `$TMPDIR` must be present, nonempty and absolute. Creation/canonicalization failures produce warnings and omit that root. A rejected constrained policy is also reported. Never substitute the parent temp directory as a writable root.
- Reuse the same UUID for both requested parents and deduplicate equal canonical children. Distinct nested parents can still produce distinct children; nesting alone is not a deduplication rule.
- `SandboxPolicy::ReadOnly { temp_writable_roots }` returns only those roots. It does not add CWD, arbitrary `--add-dir` roots, `/tmp`, or `$TMPDIR` defaults from workspace-write mode.
- Permission instructions enumerate the exact effective canonical children. Their presence does not make sibling paths, workspace files or parent temp directories writable.

The implementation uses created directories rather than an owning `TempDir` guard. Do not promise automatic deletion or persistent scratch-file recovery across restarts.

## Enforcement and maintenance

The shared writable-root representation feeds macOS Seatbelt and Linux Landlock. Apply-patch eligibility also uses those roots. macOS has a dedicated integration test for allowed child writes and denied sibling writes; do not infer equivalent tested coverage for every platform from the shared type alone. Windows-specific temp-root support was not added by this feature.

Preserve the empty-root default, workspace-write root calculation and existing escalation semantics. Any change to materialization must keep runtime policy, enforcement and rendered permissions in agreement; the warning path must never describe an uninstalled root as writable.

## Source and verification

- [Materialization and unit tests](../../core/src/session/read_only_temp.rs), [session initialization](../../core/src/session/session.rs) and [config fields](../../core/src/config/types.rs).
- [Sandbox root computation](../../protocol/src/protocol.rs), [permission rendering](../../protocol/src/models.rs), [Seatbelt](../../core/src/seatbelt.rs), [Landlock](../../linux-sandbox/src/landlock.rs) and [apply-patch eligibility](../../core/src/safety.rs).
- Existing unit tests cover two roots, canonical duplicate parents, invalid/missing `TMPDIR` and exact permission text. [Seatbelt integration tests](../../core/tests/suite/seatbelt.rs) contain `read_only_temp_root_allows_only_dedicated_child_writes`. Platform enforcement checks require the appropriate sandbox-capable test environment; a skipped test is not enforcement evidence.
