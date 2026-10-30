# Source Audit

## Evidence Scope

Inspection date: 2026-10-30. Conclusions below describe pinned source, not an assertion about every installed client or server deployment.

|ID|Source identity|Use|
|---|---|---|
|C|Codex fork `5c94707977024dc761cc0783ca72754cdc91670a`|Primary implementation target, checked out as `custom-0.98.0`.|
|U|Codex `ac192cd7937b0d73edc6dffe009940ae53782dd4`|Actual local `main` reference, inspected through immutable Git objects.|
|R|Codex fork `d340b0fab20d25932a9d1caa6d0747caa9f6aad4`|User-designated reference worktree; its actual checkout is `rebase-main`, not `main`. Do not label all of its behavior pristine upstream.|
|H|Codex `f6e251c3ac6573a4d6eccddea6ad12587868485f`|Article's cited 2026-07-07 revision, checked only for disputed historical claims.|
|M|`codeaashu/claude-code` `b983a3b8106bf77cbae6d439f700cc74d74ccd15`|Local community reconstruction, not an authenticated official source release.|
|A|`keli-wen/One-Poem-Suffices` `76bdd76fcc64b1ba873ca8de374362aa5cac2ebd`|Article at `docs/zen-of-harness-engineering/why-codex-compacts/index.md` and its adjacent figures.|

The supplied prior assessment was treated as a claim checklist, not primary evidence. Its archive labels "Custom" and "Latest" are not interchangeable with C, U or R. No official v0.98 tag inspection, checkout, fetch or rebase was needed.

Local evidence notation is `snapshot: path:line (symbol)`. Codex paths are relative to `codex-rs/`; M paths are relative to the mirror root; A paths are relative to the article repository. Line numbers apply to the pinned revision, not future edits. Reproduce Codex evidence with `git -C "$repo" show <commit>:codex-rs/<path>`.

## Claim Matrix

|Article claim|Verified result|Consequence|
|---|---|---|
|Wide searches can repeatedly add truncated output to one history.|Supported by C's capture, tool-result and history paths; the private run's repeated-search cause is not independently established.|Bound acquisition and preserve a retrieval path.|
|A 1 MiB output buffer can be as large as the context window.|Only a raw-byte comparison; those bytes are not all delivered to or billed as model input.|Separate produced, captured, retained, rendered and provider-reported tokens.|
|Codex has two truncation layers.|Useful simplification for unified execution, not a complete pipeline description; C additionally processes outputs at history insertion.|Trace the actual caller and final request, not constants alone.|
|Claude Grep defaults to filenames and 250 results.|Supported by M, with pagination and an explicit unlimited option; limiting is applied after the search.|This bounds returned entries, not necessarily search work or line bytes.|
|Claude Read enforces a 2,000-line default and returns partial text on token overflow.|M's prompt advertises 2,000, but runtime uses `limit=undefined` and throws on token overflow. Current documentation describes a newer partial-view path.|Do not substitute prompt text or current documentation for the inspected implementation.|
|Claude Bash never loses oversized output.|M normally preserves large successful output, but caps the file at 64 MiB and can fail or take a different error path.|Adopt recoverability with explicit completeness/failure status, not a lossless claim.|
|Codex compaction always means 20k user text plus a summary.|That is the local summarizer's reconstruction, not C's default qualifying-provider path or all paths already present at H.|Document dispatch before retention details.|
|Removing tool items means their information is necessarily forgotten.|Not established: information may survive in summary text, opaque compaction state or C's exact work notes.|Distinguish byte-exact evidence, summarized knowledge and unavailable information.|
|The Codex default delegation policy explains C's reluctance to spawn.|The article describes a newer backend. C defaults `collab` off but strongly encourages explorer use once exposed; U/R enable the older capability but restrict spontaneous spawning in tool guidance.|Availability, policy wording and actual model behavior are distinct.|
|Proactive delegation is inaccessible except through an internal effort tier.|Overstated even at H: configured mode text overrides effort-derived policy. U/R additionally have catalog policy selection; their V2 feature is stable but defaults off.|Do not infer entitlement or effective backend from one flag.|
|Fresh workers use `fork_context=false`.|Not portable: C exposes no history-fork argument; U/R V1 use `fork_context`; V2 uses `fork_turns` and defaults to full-history inheritance.|Use the selected tool schema, not copied instructions.|
|Explore has a read-only tool whitelist.|M uses a denylist of editing/delegation tools; Bash and other eligible tools can remain and use ordinary permissions.|Prompt specialization is real; universal write prevention does not follow.|
|The parent must always wait to preserve isolation.|A valid policy for fully delegated research, not a structural requirement. Both implementations support independent parent work and discourage duplication.|Assign non-overlapping ownership; wait when blocked, not by universal rule.|
|Delegation alone is the tested causal factor.|The intervention also changed history inheritance, handoff shape, worker effort guidance and parent scheduling.|Use a factorial study with quality gates.|

## Acquisition Pipeline

### C: unified execution

```text
process output -> HeadTailBuffer -> collected bytes -> tool rendering/truncation
               -> FunctionCallOutput -> history processing -> request projection
```

- C `core/src/unified_exec/mod.rs:51` defines the 10,000 approximate-token default and 1 MiB in-memory bound. `core/src/unified_exec/head_tail_buffer.rs:65` (`push_chunk`) fills a stable prefix and rotating suffix, discarding the middle.
- Earlier transport can also omit data: C `utils/pty/src/pipe.rs:160` and `utils/pty/src/pty.rs:92` use 256-entry broadcast channels, and `core/src/unified_exec/process.rs:65` continues after a lagged receiver. A new complete-output writer must tap before that fanout or explicitly report gaps, not merely copy a downstream buffer.
- C `core/src/unified_exec/process_manager.rs:137` creates another bounded transcript; `:154` resolves the requested output limit and `:177` applies `formatted_truncate_text`. A process/session handle supports future process interaction, not random access to discarded past bytes.
- C `core/src/truncate.rs:162` (`truncate_with_token_budget`) estimates tokens from bytes; it is not the selected model's tokenizer.
- C `core/src/context_manager/history.rs:400` (`process_item`) additionally processes tool outputs using the model policy multiplied by 1.2 for serialization allowance. Increasing the shell call's limit therefore does not guarantee more retained history.
- C's legacy shell path is different: `core/src/exec.rs:54` defines a 1 MiB bound, and `:806` (`read_capped`) keeps draining the pipe while `append_capped` retains a bounded prefix. Do not describe every shell capture as a head/tail buffer.
- A terminal/UI delta stream is not automatically a complete, durable, model-addressable output artifact. The inspected paths do not provide a complete-output retrieval contract for omitted bytes.

### C: existing structured tools

C already contains filenames-only `grep_files`: `core/src/tools/handlers/grep_files.rs:19` defaults to 100 entries and caps requests at 2,000; `:112` (`run_rg_search`) captures ripgrep output before selecting the returned entries. It has no cursor contract.

C `core/src/tools/handlers/read_file.rs:24` (`ReadFileArgs`) really defaults to a 2,000-line slice, with indentation-aware mode available. `:18` limits displayed line length to 500. That is a bounded view, not byte-exact file recovery.

Exposure is conditional in C `core/src/tools/spec.rs:1363`: model `experimental_supported_tools` must name these handlers. The bundled GPT-5.5 list is empty and the explorer model's list does not include these tools. Existence is not default availability, and simply exposing these old handlers would not solve pagination or exact-retrieval requirements.

### M: tool-specific disclosure and storage

- M `src/tools/GrepTool/GrepTool.ts:108` (`DEFAULT_HEAD_LIMIT`), `:316` (default output mode), and `:351` (`-l`) support the Grep comparison. Its content path limits columns to 500 and filters before generic result storage. Storage cannot restore entries already omitted by tool pagination.
- M `src/tools/FileReadTool/prompt.ts:10` supplies the advertised line limit, but `src/tools/FileReadTool/FileReadTool.ts:497` passes an unspecified limit through `:1022` to `src/utils/readFileInRange.ts:135`, where it means through EOF.
- M `src/tools/FileReadTool/limits.ts:18` defaults to 25,000 output tokens; the normal whole-file byte guard is 256 KiB. `src/tools/FileReadTool/FileReadTool.ts:755` validates tokens by throwing with recovery guidance. An explicit slice bypasses the whole-file byte guard, not the token validation.
- M `src/tools/BashTool/BashTool.tsx:714` can throw for a command error before successful-output persistence at `:728`. That persistence links/copies the session output, capped at 64 MiB.
- M `src/utils/task/TaskOutput.ts:297` reads the initial file range for normal file-backed shell output. M `src/utils/toolResultStorage.ts:137` (`persistToolResult`) and `:189` build a beginning preview and addressable path, not the article diagram's universal tail preview.
- Generic storage is another layer: M `src/utils/toolResultStorage.ts:272` (`maybePersistLargeToolResult`) leaves original content unchanged if persistence fails. `:769` (`enforceToolResultBudget`) freezes already-decided replacements; `src/query.ts:379` applies the budget before requests. This is relevant to stable prompt prefixes, not proof of total cache preservation.

The article reports Bash `grep` calls for the Claude experiments, not dedicated Grep calls. M `src/utils/embeddedTools.ts:3` and `src/tools/AgentTool/built-in/exploreAgent.ts:14` also support a build configuration that routes search through Bash. The figures cannot isolate dedicated Grep defaults as the cause of the reported savings.

## Compaction and Recovery

### C: dispatch and exact working state

C `core/src/compact.rs:87` (`compaction_implementation`) selects:

|Condition|Path|
|---|---|
|Provider's `is_openai()` is false, or `remote_compaction` is false|Local summarization.|
|Eligible provider, both remote features true|Streaming remote V2.|
|Eligible provider, only V2 false|Legacy remote endpoint.|

The branch predicate is an exact provider-name check at `core/src/model_provider_info.rs:268`, not a general claim about every OpenAI-compatible URL. Both remote flags default true at C `core/src/features.rs:522`.

C `core/src/compact.rs:431` (`build_compacted_history_with_limit`) implements the local recent-user-text reconstruction. C `core/src/compact_remote_v2_history.rs:19` instead budgets eligible retained user messages and plaintext summaries to at most 64,000 approximate tokens. Neither preserves ordinary assistant/tool items verbatim.

C `core/src/session/turn.rs:36` requests structured work notes including validated findings, dead ends, file relevance and next action. Actual tool execution is rejected during capture. `core/src/compact.rs:486` separates the capture transaction from source history; `core/src/compact_remote_v2.rs:156` reserves exact notes outside the truncatable retained-message pool. `:278` rejects a replacement whose fixed contents cannot fit.

This is substantial mitigation, not a guarantee of complete recall. Fresh note capture is bounded, can fail, and is skipped in some constrained paths. Its source and destination models can differ. Preserve the existing [streaming compaction contract](../../features/remote_compaction_v2/README.md), including capture recovery and installation cancellation.

Relevant source coverage exists at C `core/src/compact_remote_v2_tests.rs:161` (fixed-content budgets) and `core/tests/suite/compact_remote_v2.rs:231` (exact notes and excluded capture suffix). These tests were inspected, not rerun for this documentation-only task.

### Historical and reference corrections

H `core/src/session/turn.rs:938` already dispatches to remote compaction, and H `model-provider-info/src/lib.rs:413` supports OpenAI/Azure. The article's universal-local-path account was incomplete at its cited revision, not merely outdated by later changes.

U `core/src/session/turn.rs:1275` dispatches by provider remote-compaction capability. R has since separated capability resolution into `model-provider/src/capabilities.rs:30`; qualifying OpenAI/Azure defaults can be overridden. Keep those paths distinct from C's narrower predicate.

R `core/src/tools/context.rs:448` (`code_mode_result`) passes captured raw bytes to programmatic callers when no nested output limit is supplied, allowing filtering before model emission. The Code Mode path also exists at U `core/src/tools/context.rs:435`. It cannot recover bytes lost earlier in capture and is not an automatic durable complete-output store.

R `ext/history-notes/src/extension.rs:46` gates optional history retrieval on token-budget configuration, OpenAI provider and backend authentication. It is not universally exposed and does not restore output discarded before recording.

### M: selective reconstruction, not immunity

M `src/services/compact/compact.ts:122` limits restored tracked files to five, with a 5,000-token per-file cap and aggregate budget. `:518` snapshots and clears the old read cache; `:1415` (`createPostCompactFileAttachments`) rereads selected files and avoids duplicating preserved tail reads. M `src/services/compact/prompt.ts:349` can provide a transcript path for exact omitted details.

M `src/tools/FileReadTool/FileReadTool.ts:523` can return an unchanged-file stub for a previously read matching range and modification time. Because compaction clears the cache, this should not be copied as an unqualified "mtime unchanged means never reread" optimization.

M `src/services/compact/microCompact.ts:253` gates time-based and cached editing separately. Time-based editing defaults off in `src/services/compact/timeBasedMCConfig.ts:30`; the internal cached implementation is not supplied. Neither path proves the reconstructed build always edits old tool results in production.

## Budget Arithmetic

For the bundled GPT-5.5 metadata in C:

```text
resolved raw window                  272,000
effective input window (95%)          258,400
default auto-compaction limit (90%)   244,800
auto limit / effective window         94.74%
```

C `protocol/src/openai_models.rs:259` derives the auto limit from the raw resolved window; `core/src/session/turn_context.rs:62` applies the effective percentage separately. `core/src/session/turn.rs:693` checks usage after sampling, with pre-turn/downshift checks elsewhere. The trigger is not a continuous interrupt at exactly 90% of the displayed effective window. Remote catalog values and explicit overrides may change these defaults.

M `src/services/compact/autoCompact.ts:33` and `:72` instead derive the normal threshold as resolved context minus `min(model maximum output, 20,000)` minus 13,000, with lowering overrides. A universal 256k-versus-1M comparison is not supported by these implementations.

## Delegation and Instruction Ownership

|Dimension|C|U/R|M|
|---|---|---|---|
|Default capability|`collab=false`.|Older `multi_agent=true`; V2 flag false, with model/backend selection also relevant.|Explore registration depends on build flags in this reconstruction.|
|Spawn guidance|No explicit-request prohibition; explorer description favors use for all codebase questions.|Older spawn description requires an explicit applicable request and says research depth alone is insufficient.|Main prompt prefers direct simple lookup; Explore for broader or more-than-three-query work.|
|New worker history|New thread, no fork argument.|V1 fresh by default; V2 full history by default unless `fork_turns` says otherwise.|Separate initial messages unless explicit fork context is supplied.|
|Explorer profile|Catalog-supported `gpt-6-luna`, Medium effort; no dedicated prompt; `read_only=false` inherited from profile defaults.|Empty built-in explorer configuration; role description and configured overlays matter.|Dedicated prompt; editing/delegation denylist; external Haiku default in this source.|
|Instruction load|Inherited effective config, developer/user instructions and provider; fresh history is not an empty prompt.|Backend/config dependent.|Initial CLAUDE.md and git snapshot omitted for Explore; later context loading is a separate path.|

C evidence: `core/src/features.rs:558`; `core/src/tools/spec.rs:457`; `core/src/agent/role.rs:93`; `core/src/tools/handlers/collab.rs:586`; `core/src/agent/control.rs:38`. The actual wait tool is named `wait`, not `wait_agent`.

U evidence: `features/src/lib.rs:1258`; `core/src/tools/handlers/multi_agents_spec.rs:696`; `core/src/tools/handlers/multi_agents/spawn.rs:94`; `core/src/tools/handlers/multi_agents_v2/spawn.rs:292`; `core/src/session/multi_agents.rs:155`. R's analogous feature entries are at `features/src/lib.rs:1423`. Both U/R place the empty explorer asset at `core/assets/agent/builtins/explorer.toml`, not the article's older source-directory path.

H `core/src/session/multi_agents.rs:44` already supports configured mode-hint text before deriving policy from Ultra. U additionally checks catalog hints. These are instruction-policy selectors, not proof of account entitlements. An explicit-request sentence in a tool description is not a semantic natural-language authorization check in the spawn handler.

M evidence: `src/tools/AgentTool/built-in/exploreAgent.ts:64`; `src/tools/AgentTool/runAgent.ts:368`; `src/tools/AgentTool/agentToolUtils.ts:122`; `:276` (`finalizeAgentTool`); `src/constants/prompts.ts:374`; `src/tools/AgentTool/prompt.ts:259`. The parent receives returned assistant text rather than all child tool results; there is no proof that every returned report is short or complete.

C `core/src/tools/registry.rs:36` defaults `ToolHandler::is_mutating` to false. An unmarked handler is therefore not proof of a read-only capability. Explorer restrictions need explicit construction/dispatch decisions plus the effective shell policy, not blind reuse of this boolean.

M's supplied build is an important counterexample to inferring availability from a source definition: `scripts/build-bundle.ts:83` aliases `bun:bundle` to `src/shims/bun-bundle.ts`, whose `:39` returns false for unknown flags. It does not define `BUILTIN_EXPLORE_PLAN_AGENTS`; `src/tools/AgentTool/builtInAgents.ts:13` therefore leaves Explore/Plan unregistered under that configuration. A scoped Git comparison found no changes in the audited Read/Grep/Bash/Explore, compaction, storage or build-shim files between the article's `6a25909` and M.

C `core/src/project_doc.rs:92` loads root-to-CWD project instructions within a default 32 KiB aggregate project-file budget, with per-directory override precedence. C `core/src/config/mod.rs:2026` loads global instructions separately. Referenced Markdown is not recursively expanded by that loader, and fresh child history does not discard inherited safety or project obligations. M's omission optimization is not a reason to blindly remove C's AGENTS.md context.

## Experimental Limits

A reports one sample per setting. Its table reports approximate parent peaks of 231k (baseline), 241k (bounded-search instructions), and 93k (delegation), with costs $3.93, $2.73 and $5.30 respectively. The prose compares a baseline-relative cost change to the refined run's 241k peak; the paired baseline peak is 231k.

The reported arithmetic is consistent with 30.5% lower refined cost, 34.9% higher delegate cost than baseline and 94.1% higher delegate cost than refined. It does not identify billed subscription usage. The article archive contains figures and an index, but no raw task trajectories, private task repository, token-accounting records or runner for reproducing those measurements.

Do not infer any of the following from that sample:

- OR-branch count alone caused savings: the refined run's reported mean rises from 9.0 to 13.3.
- `head` usage came only from training: model choices are conditioned on prompts/tool descriptions, and no forced rewrite is not an isolated training experiment.
- Summed child peaks form one usable combined context, or eliminate duplicated instruction/evidence costs.
- No compaction implies better coverage, fewer false statements or lower total resource use.
- Anthropic's approximately 15x observation relative to ordinary chat is the multiplier for adding workers to a single-agent coding run.
- Waiting is resource-free in every sense: avoiding parent inference does not eliminate worker usage, wall time, quotas or task limits.

The source supports isolation as a mechanism and recoverability as a useful contract. It cannot reproduce this author's outcomes or establish the semantic completeness of opaque compaction state.

### Issue evidence

The article's cited `https://github.com/openai/codex/issues/16839` does contain a firsthand report of a 610 KB file read 53 times. Treat that as a reported symptom, not an independently reproduced measurement here.

The same reporter's controlled follow-up at `https://github.com/openai/codex/issues/16812` withdrew the proposed threshold-regression explanation after the older version compacted more often on the matched task. Repeated reading remained a concern; the causal correction matters.

The other cited issue, `https://github.com/openai/codex/issues/26889`, proposes bounded thread-scoped text pins and compaction reinjection. It is a design discussion, not another controlled reread experiment or proof that such an API is present in C. A new pin service is not required for the two selected branch-local improvements.

## Official Documentation Check

Current primary documentation was checked on the inspection date as a separate evidence class, not used to rewrite historical defaults:

- OpenAI compaction documentation describes opaque state and potentially retained items: `https://developers.openai.com/api/docs/guides/compaction`. C's request-only trigger protocol must not be conflated with the public API's server-side `compact_threshold`.
- OpenAI subagent documentation distinguishes explicit delegation, proactive policy and additional usage: `https://learn.chatgpt.com/docs/agent-configuration/subagents`.
- Claude Code's current tools reference describes conditional partial views and tool availability: `https://code.claude.com/docs/en/tools-reference`.
- Claude Code's current subagent documentation gives Explore a different default model policy from M: `https://code.claude.com/docs/en/sub-agents`.
- Anthropic's context-editing documentation distinguishes cache effects by strategy: `https://platform.claude.com/docs/en/build-with-claude/context-editing`.
- Anthropic's research-system report compares multi-agent token usage to ordinary chats: `https://www.anthropic.com/engineering/multi-agent-research-system`.

## Decisions

1. Preserve existing notes and compaction semantics; do not reimplement them from the article's simplified account.
2. Prioritize [recoverable output](recoverable_tool_output.md): the missing complete-output retrieval contract is confirmed in C and remains distinct from notes or optional history retrieval.
3. Pilot [bounded exploration](bounded_exploration.md) on C's actual opt-in API; do not transplant newer flags or universally force parent waiting.
4. Retain bounded acquisition inside every context. Test its combination with delegation rather than treating the two mechanisms as alternatives.
5. Require the [evaluation gates](evaluation_plan.md) before default changes. Neither article figures nor static source inspection count as a live behavioral result.
