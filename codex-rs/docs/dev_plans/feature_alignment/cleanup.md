# Retire obsolete feature controls

Status: proposed cleanup under [feature alignment](README.md). A `Removed` registry label does not itself identify dead runtime code. The reference loader ignores some retired keys explicitly, still translates some Windows values, and advertises a compaction capability independently. Classify the behavior before deleting it.

## Delete retired capabilities

|Feature|Removal boundary|Code and consumers to inspect|Acceptance condition|
|---|---|---|---|
|`undo` / `GhostCommit`|Remove live ghost snapshots and undo dispatch; keep old rollout decoding intentionally.|`core/src/tasks/ghost_snapshot.rs`, `core/src/tasks/undo.rs`, ghost scheduling in `core/src/session/mod.rs`, protocol operations/events, CLI/TUI/app-server entry points, snapshot-only Git helpers and tests.|New turns spawn no ghost snapshot work; removed UI/API paths have a clear outcome; an old rollout with ghost items can still resume/fork/compact under the chosen compatibility reader.|
|`child_agents_md`|Remove the additional hierarchical-AGENTS explanatory prompt experiment.|`core/src/project_doc.rs`, `HIERARCHICAL_AGENTS_MESSAGE`, its prompt asset/export, feature/schema entry and experiment-only tests.|Ordinary ancestor discovery, `AGENTS.override.md`, project-document limits and instruction order remain intact.|
|`skill_env_var_dependency_prompt`|Remove automatic skill environment-variable prompting and experiment-only metadata processing.|Skill dependency extraction, prompt/cache/UI dispatch, `core/src/session/turn.rs`, session prompt state and solely related tests.|A skill with old `env_var` metadata loads without this prompt; explicit environment configuration, inherited environment and MCP dependency installation remain functional.|

History: ghost removal [4e05f3053c84](https://github.com/zhutao100/codex/commit/4e05f3053c840fc77321bfab0aef65ec50448a9e), later unused undo surface removal [f50c02d7bcd4](https://github.com/zhutao100/codex/commit/f50c02d7bcd4c06a23173389da8ed2c68c03d81d), child prompt removal [bb72e151e50c](https://github.com/zhutao100/codex/commit/bb72e151e50cfd4f6282ab86418188af3c83b5df), and environment prompting removal [6b54ced108af](https://github.com/zhutao100/codex/commit/6b54ced108afdc8eba2218672ba0b9790f246782). The reference explicitly tests legacy ghost items decoding as `ResponseItem::Other` in [protocol models](https://github.com/zhutao100/codex/blob/33411a01d7dae5fe89728ecfa2ab8b1c09002329/codex-rs/protocol/src/models.rs#L3916). Adapt only the compatible decoding needed here; general Git utilities and user history are not cleanup targets.

These features still have code in the current branch. They become deletion candidates because the plan intentionally retires their behavior, not because their code is presently unreachable.

## Retire gates while preserving established behavior

|Feature|Chosen replacement|Concrete cleanup|Dependencies and checks|
|---|---|---|---|
|`exec_policy`|Unconditional execution-policy loading, evaluation and amendments.|Remove feature checks, gate-only parameters, disabled-policy branches/errors and opt-out fixtures from `core/src/exec_policy.rs` and its callers.|Keep allow/prompt/deny behavior, policy-file error handling, approval ownership and custom reviewer/explorer restrictions. Reference [4521a6e85230](https://github.com/zhutao100/codex/commit/4521a6e85230be406d87b5a89dbd931643a3d532).|
|`request_rule`|Approval parameters and prefix-rule support selected by policy and tool capability.|Remove `request_rule_enabled` plumbing from `tools/spec.rs`, permission instructions and shell/unified-exec handlers; retain validation/persistence.|Coordinate with execution-policy cleanup; capture tool schemas and assert runtime approval behavior. Reference [19afbc35c1ab](https://github.com/zhutao100/codex/commit/19afbc35c1ab9a348232467cb9fd090bc0833f11).|
|`remote_models`|Provider/auth-qualified discovery with cache and fallback.|Remove the flag's bypass branches in `models_manager/manager.rs` after preserving overlay merge/replace precedence and refresh events.|Offline/stale/missing cache, non-OpenAI providers, naming/review model selection and catalog identity. Reference [41800fc87618](https://github.com/zhutao100/codex/commit/41800fc87618591cecedd28f8778b038a22906d9).|
|`powershell_utf8`|UTF-8 handling whenever the selected shell is PowerShell.|Remove feature checks and gate-only runtime arguments; retain the shell-type condition.|Unicode, quoting and encoded commands on Windows and PowerShell-on-Unix. The current non-Windows flag default is false, so the latter is a behavior change. Reference [ea8b07e680da](https://github.com/zhutao100/codex/commit/ea8b07e680da44d8670ed4405ca4a48c468be0ae).|
|`steer`|Always-enabled steering with explicit queuing.|Remove disabled-steering UI/config branches while retaining the current submission owner and Enter/Tab behavior.|Exercise input during model work, tools, completion review, pause and restart. Keep accepted/consumed distinctions. Reference [e76b1a28535e](https://github.com/zhutao100/codex/commit/e76b1a28535e1e1fa9a4776e6f1bd90ba3dcc470).|
|`collaboration_modes`|Always-available Plan/Default modes.|Remove mode-gating branches and gate-only tests; preserve mode transitions and mode-dependent tools.|Review/steering pending during a mode change and resumed mode restoration. This flag is distinct from `collab`. Reference [6e60f724bcd1](https://github.com/zhutao100/codex/commit/6e60f724bcd18e722086e646f9d7e4f19214a9f1).|
|`sqlite`|Reliable metadata storage with explicit ownership and recovery.|Validate initialization/backfill first; then remove disabled-state branches and obsolete parity plumbing in `state_db.rs` and session callers.|Current default is off. Cover absent/corrupt/busy databases, interrupted backfill, ephemeral sessions, JSONL recovery, custom sidecars and resume/fork. Stabilization [eace7c661026](https://github.com/zhutao100/codex/commit/eace7c661026658873d54d452db1f9434d8ee8eb) preceded gate removal [fa16c26908cd](https://github.com/zhutao100/codex/commit/fa16c26908cd6881f03e6484fb60d20c1ad63a53).|

Do not merge SQLite default migration into a registry-only cleanup. Do not infer from unconditional model discovery that every provider or API-key route should call OpenAI model endpoints. The newer `api_key_model_discovery` control and newer storage architectures are separate changes.

## Move selection to the correct owner

|Controls|Owner after migration|Required work before flag deletion|
|---|---|---|
|`use_linux_sandbox_bwrap`|Linux sandbox backend selection.|Adopt the supported backend and dependency/unsupported-host behavior, then remove boolean plumbing across session, orchestrator, CLI and helper/MCP paths. Preserve actual bubblewrap execution. Test in a disposable Linux environment.|
|`experimental_windows_sandbox`, `elevated_windows_sandbox`|An explicit Windows sandbox mode such as `[windows].sandbox`.|Migrate config writers and callers together. Translate demonstrated legacy settings once at load with explicit precedence, then remove redundant gates. Keep restricted/elevated selection and setup behavior.|
|`apply_patch_freeform`|Model `apply_patch_tool_type` plus current model-overlay rules.|Ensure supported custom models declare the intended Function/Freeform capability. Then remove the force-enable fallback and its obsolete top-level aliases. Keep all patch handlers and model-selected schemas.|

Bubblewrap became default in [04892b4ceb3b](https://github.com/zhutao100/codex/commit/04892b4ceb3ba6516eddd21c3f7ed2920a977442); [a30b807efe0d](https://github.com/zhutao100/codex/commit/a30b807efe0d013d49daf0462f8e1373840a3e4d) restored the old name as a no-op for callers. Do not introduce the reference's now-deprecated `use_legacy_landlock` switch merely to match its inventory.

The Windows flags were marked removed by [87279de434f0](https://github.com/zhutao100/codex/commit/87279de434f0b3d85872877a1b14e07a2e4238a6), but [reference mode selection](https://github.com/zhutao100/codex/blob/33411a01d7dae5fe89728ecfa2ab8b1c09002329/codex-rs/core/src/windows_sandbox.rs#L115) still consumes legacy feature values. Replacing them with constant false would change behavior.

The apply-patch feature was stabilized before [51b0e94105c4](https://github.com/zhutao100/codex/commit/51b0e94105c46bca4540b4f2a488e7e9d91bd9e0) removed the fallback. Current [`ToolsConfig::new`](../../../core/src/tools/spec.rs) still uses that fallback when model metadata is absent. Test None/Function/Freeform and custom overlays before deleting it; removal of a rollout switch is not removal of the patch tool.

## Compaction and transport selection

Treat these as one selection design followed by separate implementation patches. Preserve the existing [compaction contract](../../features/remote_compaction_v2/README.md), [WebSocket contract](../../features/responses-websocket.md) and [replay contract](../../features/responses-output-replay.md).

|Current control|Reference behavior|Planned alignment|
|---|---|---|
|`remote_compaction`|Provider capability selects remote compaction; local compaction remains for unsupported providers.|Write the supported provider/mode matrix first. Preserve an explicit local choice if required for the branch's nonconforming providers; then remove the redundant parent flag.|
|`remote_compaction_v2`|Streamed compaction is the supported remote implementation. The old unary endpoint was deleted. The string remains an API beta capability marker.|Keep this branch's streaming implementation and custom extensions. Retire unary dispatch/client/types only after supported-provider parity. Decouple the capability advertisement from feature membership before deleting the gate.|
|`responses_websockets`, `responses_websockets_v2`|Provider `supports_websockets` and session fallback state select transport.|Keep one supported request protocol with complete logical history and HTTP/SSE recovery. Move capability/opt-out selection to the provider/session owner; remove obsolete experimental flag branches afterward.|

Relevant changes are parent compaction gate removal [dd80e332c45a](https://github.com/zhutao100/codex/commit/dd80e332c45aefc935d68fe067026ccf40312ccd), streamed selection [3dc1e2a58406](https://github.com/zhutao100/codex/commit/3dc1e2a58406dc69db5812539adfee7d89fa9ef7), unary removal [1ac689cc7de0](https://github.com/zhutao100/codex/commit/1ac689cc7de0e637d35271edbbc132d1553d1767) and WebSocket selection [770616414a51](https://github.com/zhutao100/codex/commit/770616414a51fa179ce4cef10f7f8df838d3f46f). The reference [session capability builder](https://github.com/zhutao100/codex/blob/33411a01d7dae5fe89728ecfa2ab8b1c09002329/codex-rs/core/src/session/mod.rs#L1459) still adds `remote_compaction_v2` independently of the retired config switch.

Before deleting unary code, enumerate actual supported providers. If a supported provider still requires unary compaction, keep the smallest explicit provider strategy until its support changes; it is then live compatibility, not dead code. Do not silently redirect it to a protocol it does not implement. Likewise, transport alignment must keep a useful provider opt-out where current integrations need it rather than simply forcing WebSockets globally.

Acceptance includes manual/automatic/review-budget compaction, notes capture with rejected tools, cancellation after notes, source-model downshift, durable replacement, history/tool-pair preservation, stable review frames, rejected continuation IDs, full fresh-socket replay and HTTP fallback. Verify the beta marker explicitly. New retry behavior must not replay already executed tool actions or let cancelled review episodes keep retrying.

## Mixed and still-active deprecations

|Control|Actual reference semantics|Planned treatment|
|---|---|---|
|`personality`|Old feature flag and friendly/pragmatic style selection are retired. `personality="none"` still removes the literal `# Personality` catalog-template section; explicit base instructions take precedence.|Remove obsolete picker/template-selection paths after checking model overlays and persisted instruction provenance. Retain the documented opt-out and explicit instruction precedence. Test absent/true/false flag, all style values, explicit base instructions and resumed threads.|
|`web_search_request`, `web_search_cached`|Already Deprecated in both branches and still translated into web-search mode.|Make `web_search = "disabled" / "cached" / "live"` the canonical configuration. Migrate branch-owned callers and keep only a demonstrated loader conversion; preserve precedence among profile, top-level and legacy settings. The search implementation remains.|
|`collab` → `multi_agent`|Renamed, stabilized, then deprecated; the reference still reads it for V1 fallback. `[agents].enabled` and Multi-agent V2 participate separately.|Preserve current subagents and reviewer restrictions. Choose a canonical enablement owner before adding aliases. Do not rename to an already deprecated key merely for cosmetics, retire V1 implicitly, or enable V2 as part of cleanup.|

Sources: personality retirement [883af106b957](https://github.com/zhutao100/codex/commit/883af106b957d21cba92121f70ce68bf58ecea50), multi-agent rename [e41536944e7e](https://github.com/zhutao100/codex/commit/e41536944e7e89763c51d7780bd1c3f3b8957836), stabilization [36dfb844277e](https://github.com/zhutao100/codex/commit/36dfb844277e79793766f96305c9633f90bc043e) and deprecation [c9296fa1a926](https://github.com/zhutao100/codex/commit/c9296fa1a92688a430fedccc33a30a05696ffbd0). Inspect the current [agent selector](https://github.com/zhutao100/codex/blob/33411a01d7dae5fe89728ecfa2ab8b1c09002329/codex-rs/core/src/config/mod.rs#L1810) for the V1/V2 exception rather than inferring removal from the lifecycle label.

## Finish each cleanup completely

After changing an owner, search the entire Rust workspace for the Rust variant, public key, legacy aliases, config fields and serialized forms. Remove unreferenced modules/assets/dependencies and update Cargo/module declarations, generated schemas, feature-list/menu output and docs. Shared utilities and old-history readers require an actual caller analysis before deletion. Capture configuration precedence and the selected enabled behavior in focused tests, then update the relevant feature contract and remove the completed work from this plan.
