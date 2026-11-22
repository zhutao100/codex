# Feature controls and migrations

[Feature registry](../../core/src/features.rs) entries describe configurable behavior. Tools still require model capability, role permission, and runtime dispatch. A provider protocol marker is not a configuration switch.

|Former control|Current owner and behavior|
|---|---|
|`undo`, `child_agents_md`, `skill_env_var_dependency_prompt`|No new worktree snapshots, extra hierarchy prompt, or secret-question workflow. Old rollout records remain readable and stay out of model input. The legacy undo operation returns a bad-request error. Normal project instruction discovery, skill metadata loading, shell environment configuration and MCP dependencies are independent.|
|`exec_policy`, `request_rule`, `powershell_utf8`|Execution rules and approved-prefix persistence are always active. The selected shell, not the host OS, determines PowerShell UTF-8 handling.|
|`steer`, `collaboration_modes`|[Steering and explicit queueing](steering-and-queue.md) and Plan/Default modes are always available. Mode-specific tool execution restrictions remain in force.|
|`sqlite`|Persistent recorders initialize the metadata index. A failed backfill batch remains retryable; readers use JSONL until backfill completes. Pickers and `resume --last` use the original workspace, while memory lookup uses the latest turn's directory. Upgrading rebuilds those indexed fields without removing memories or logs. Ephemeral sessions do not initialize the index.|
|`remote_models`, `apply_patch_freeform`|[Model discovery](model-catalog.md) and model `apply_patch_tool_type` select metadata and patch format. A published v1 client override only supplies a missing format; it cannot replace a model's explicit choice. An explicit disabled model shell suppresses both advertised command tools and their dispatch aliases.|
|`personality` feature and style picker|`personality = "none"` strips the literal `# Personality` section from a catalog template. Explicit base/final instructions and persisted thread instructions retain precedence. Old friendly/pragmatic values are readable but do not select a style or inject a developer message.|
|`responses_websockets`, `responses_websockets_v2`|Use [`responses_transport`](responses-websocket.md). If neither the profile nor the top level specifies it, explicit legacy flags retain their historical OR semantics.|
|`remote_compaction`, `remote_compaction_v2`|Use [`compaction_mode`](remote_compaction_v2/README.md). In the absence of a canonical choice the old master `false` means local; otherwise V2 `false` means unary. A profile's value for a legacy key overrides the top-level value for that key.|
|`collab`|`[agents].enabled` selects the existing subagent tools; it takes precedence over the deprecated feature. Role, depth and reviewer restrictions still apply. No scheduler migration is implied.|
|Windows feature flags|`[windows].sandbox` selects `disabled`, `restricted-token` or `elevated`; an explicit profile mode wins over the top-level mode. Without either, the historical elevated flag wins. If a Windows token sandbox is required, use the one-shot shell executor: the retained pipe and PTY launchers reject this mode rather than starting an unrestricted process.|
|Legacy web-search flags|Use `web_search = "disabled"`, `"cached"` or `"live"`; the loader retains legacy conversions.|

## Runtime boundaries

- After a child selects its model and provider, inherited window and compaction overrides survive only when both match the parent. A different child uses its own catalog/overlay limits.
- A nonempty retained-terminal write uses the actual launch sandbox and the current turn's authority. Changed authority requires a fresh decision or rejection. An empty poll is a read. The manager checks the process identity again after any wait before enqueueing bytes, and registered background jobs still reject polling. Run the [retained-terminal fixtures](../../core/src/unified_exec/mod.rs) in a PTY-capable disposable guest to cover cancellation and ID reuse; a nested-sandbox skip is not process evidence.
- Skill MCP installation offers only missing, admitted server identities. After the prompt, installation checks the offered subset again against configured/runtime servers, managed constraints and pending refreshes. `never` does not prompt. Hosted apps remain opt-in and require ChatGPT authentication or the existing explicit connector credential.
- Automatic titles are opt-in and are generated after an eligible completed turn. An explicit rename wins over in-flight generation; the index writer serializes the conditional append across processes. Reopening a thread reads the persisted winner.
- Shell snapshots are opt-in. Capture uses the selected executable and shell environment policy. Only a matching executable and policy can reuse the private, session-instance-specific file. Snapshot exports omit explicit environment overrides and credential-like variable names; command execution supplies its own environment. Shell startup scripts, functions and aliases are still user-provided shell code.

The four custom controls (`auto_rename_thread`, `auto_post_turn_completion_review`, `recoverable_exec_output`, `exec_job_completion`) remain opt-in. See [completion review](post_turn_completion_review/README.md), [context harness](context-harness.md), [exact notes](session-work-notes.md), and [output replay](responses-output-replay.md) for their ownership and recovery boundaries. Additional upstream architecture or hosted-product gates are not prerequisites for these contracts.

## Intentional differences

- The opt-in `memory_tool` is a metadata lookup, not an extraction or consolidation service. The existing replay, request-compression and runtime-metrics settings retain their own provider and exporter contracts.
- The Linux backend and Windows unified-exec defaults depend on packaging and runtime evidence. See [native acceptance](../dev_plans/feature_alignment/README.md).
- Async compaction, token-budget windows, a second agent scheduler, Code Mode and desktop or hosted product features were not selected. They would change the owner of history, execution or approvals rather than complete a registry migration.
