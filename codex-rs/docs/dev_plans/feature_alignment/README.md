# Feature alignment

Status: proposed implementation work. This plan changes documentation only; it does not change runtime behavior, feature defaults or configuration. Complete one scoped change at a time, and move its resulting contract into the owning feature document when implemented.

## Objective

Reduce obsolete configuration and dead branches while preserving the useful behavior of the current harness. Adapt reference implementations that improve task ownership, request construction, execution, context or recovery. Product surfaces and provider-specific protocols are separate choices. Apply [project principles](../../principles.md), the existing [context-harness plan](../context_harness/README.md) and the [custom preservation requirements](custom_preservation.md) throughout.

A feature flag has several independent properties: its registry key, lifecycle label, compile-time default, configuration precedence, runtime activation and implementation. Match all of the relevant properties before changing it. A removed flag can leave its behavior unconditional or migrate it to another configuration owner; a newly registered flag can wrap a capability already implemented here.

## Decisions

|Area|Planned treatment|Reason|
|---|---|---|
|Obsolete experiments|Delete their runtime branches, definitions, obsolete assets and tests once the precise scope is established.|`undo` and the extra child-AGENTS prompt experiment have actual upstream removal history. Ordinary history recovery and AGENTS.md loading remain separate capabilities.|
|Graduated controls|Make the chosen implementation unconditional or move its selection to the existing provider/platform/configuration owner before removing the flag.|Execution policy, model discovery, steering, collaboration modes, SQLite, PowerShell encoding and transport are still useful after flag retirement.|
|Stable shared features|Compare behavior and prerequisites before adopting defaults.|Shell snapshots, memories and apps have materially different implementation scope. Windows unified-exec defaults also changed.|
|Shared unchanged metadata|Inspect relevant runtime changes instead of treating matching registry rows as equivalence.|Shell capability selection, skill/MCP dependency admission and sampled-output replay expose important semantics outside metadata.|
|New core features|Prioritize small correctness changes; design large architecture changes independently.|Request snapshots, command authority and cancellation are separable from Code Mode, new context windows and Multi-agent V2.|
|Host/provider product features|Defer until the branch has the corresponding consumer and supported route.|Desktop gates, backend identity, internal attribution and hosted extensions cannot be supplied by copying feature rows.|
|Custom behavior|Keep the four branch-specific gates and preserve non-flag workflows.|Automatic completion review, exact notes, pause/continue, retained command evidence and owned job completion have explicit contracts.|

## Work packages

|Package|Scope|Depends on|Completion condition|
|---|---|---|---|
|FA-1|[Retire obsolete controls](cleanup.md): isolated removals and unconditional established behavior.|Custom preservation requirements and relevant configuration callers identified.|No obsolete runtime branches; supported behavior and historical readers remain correct; docs/schema/CLI references agree.|
|FA-2|[Align stable implementations](stable_alignment.md): shell selection, dependency installation, snapshots, memories, apps and replay.|Only the specific FA-1 changes a feature requires.|Behavioral differences resolved explicitly; supported routes/platforms and defaults covered by focused checks.|
|FA-3|[Adapt core correctness](core_adoption.md#first-adapt-existing-contracts): exact request ownership, retained command authority and recovery.|Current preservation fixtures retained; compare existing equivalent behavior first.|A demonstrated current gap is fixed without broad scheduler, storage or provider replacement.|
|FA-4|[Optional architecture](core_adoption.md#optional-architecture): incremental context, async compaction, new context windows, Multi-agent V2 or Code Mode.|One design per subsystem; one owner for history, budgets and task lifecycle.|Supported activation and interactions documented; isolated experimental rollout until its own evidence supports changing defaults.|
|FA-5|Peripheral and service-specific work.|A concrete product requirement.|Independent change with its own consumer; no prerequisite for finishing core alignment.|

Begin with `child_agents_md`, the retired skill environment-prompt experiment, execution-policy gate retirement and PowerShell encoding cleanup. Keep removal of undo, personality selection and SQLite controls separately reviewable because they affect a user-visible behavior or durable state. Prioritize the source-identified shell-capability, MCP admission and automatic-title precedence differences while larger architecture work remains optional.

## Configuration and deletion policy

Update branch-owned config examples, schema, feature-list output and direct clients in the same implementation change. Remove obsolete runtime branches rather than keeping them as compatibility machinery. Do not import the reference registry's full collection of removed entries.

Retain an alias or conversion only for a demonstrated consumer that cannot migrate atomically; give it a specific owner and removal condition. Prefer a clear migration diagnostic to silently ignoring a user setting that still controlled behavior here. Historical rollout readers have a different purpose: keep the smallest reader needed to resume existing user history even when the old producer is removed. Do not classify that reader as dead solely because new sessions no longer produce its records.

For each change, record whether an old `false` override loses an intentional behavior. Examples include local versus remote compaction, disabling a database, selecting a model's patch format, transport opt-out and disabling subagents. Resolve that meaning through current configuration ownership before deleting the control.

## Evidence and scope

The comparison was pinned on 2026-11-21 to [current f645a292b7a7](https://github.com/zhutao100/codex/tree/f645a292b7a7325ef8ab584fff341c0dbfa5f675) and [reference 33411a01d7da](https://github.com/zhutao100/codex/tree/33411a01d7dae5fe89728ecfa2ab8b1c09002329). The reference feature crate is unchanged from [OpenAI bb5b9c624013](https://github.com/openai/codex/commit/bb5b9c6240136de8ff4d30215e2341c44559aa04), while other implementations include a custom overlay. Use its [customization notes](https://github.com/zhutao100/codex/blob/33411a01d7dae5fe89728ecfa2ab8b1c09002329/codex-rs/docs/rebase_customizations.md) when selecting code.

A token-based registry parser checked balanced Rust structures, unique keys/IDs, enum coverage, conditional fields and platform defaults. Matching the Rust identity as well as the public key distinguishes `memory_tool` to `memories` and `collab` to `multi_agent` from unrelated removals/additions. Exact inventories, historical transitions, source references, parser scripts and investigation limitations are accompanying research data, not permanent copies of the registry in these plans. Re-run the comparison when source revisions change.

The plans establish source and history findings plus proposed acceptance conditions. No Rust suite, live inference, cross-platform runtime or performance evaluation was executed for this documentation change. Follow the [validation plan](validation.md) for implementation work.
