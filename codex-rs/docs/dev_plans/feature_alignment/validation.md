# Validation for feature alignment

Status: proposed checks for implementation work, linked from the [feature-alignment plan](README.md). This document distinguishes source inspection from executed runtime evidence. The planning change itself runs no Rust build or model evaluation.

## Match validation to the change

|Change|Necessary evidence|Evidence that is insufficient|
|---|---|---|
|Remove a flag or alias|Focused source search for enum/key/callers; config precedence and feature-list checks; updated schema when types change.|A zero match in only the registry file.|
|Retire a behavior|No active producer/dispatcher/assets left; intentional old-history reader coverage where needed; remaining shared behavior checked.|Changing `Stage` to `Removed` while leaving live branches.|
|Stabilize or enable a default|The chosen implementation works on supported platforms/routes, with explicit disabled/unsupported cases.|Copying reference metadata or a successful Linux-only compile.|
|Context, replay or compaction|Captured outbound requests, exact retained data, replacement and crash/restart behavior.|Prompt text that asks the model to preserve state.|
|Execution authority|Runtime dispatch and, where changed, a disposable process-policy probe.|Tool advertisement alone.|
|Durable job/evidence ownership|Recorder failure/reopen and stale/superseded-owner fixtures.|An ordinary successful command run.|
|Performance or model quality|Controlled live comparisons with the same task/model/configuration and observed cache/timing/quality measurements.|Source similarity or mock transport success.|

## Preservation checks

Existing names below are source entry points, not tests executed by this plan. Run the groups affected by a particular change, and add a fixture only for a concrete uncovered behavior.

|Boundary|Existing source entry points|Assertions that must remain|
|---|---|---|
|Naming|[`auto_rename_thread.rs`](../../../core/tests/suite/auto_rename_thread.rs): `auto_rename_thread_uses_luna_model_and_lightweight_instructions`, `auto_rename_thread_honors_cached_luna_visibility_for_chatgpt_prolite`.|Preserve lightweight-model/provider resolution; add the blocked-generation/manual-rename case described in [custom preservation](custom_preservation.md#focused-improvement-manual-rename-wins-over-automatic-naming).|
|Completion review|[`completion_review.rs`](../../../core/tests/suite/completion_review.rs): `automatic_review_continues_past_three_with_only_initial_and_latest_answers`, `durable_handoffs_recover_prepared_and_started_work_without_duplicate_advice`.|Bound review packet size, preserve uncapped automatic rounds and install advice once across restart.|
|Review request budget|The same suite: `review_followup_counts_request_context_once_near_budget`, `review_followup_compacts_unreported_model_output`, `review_followup_budget_recovery_preserves_scope`.|Count request-only frames once; account for unreported tail output; compact before recoverable budget rejection.|
|Stop and consumed input|[`completion_review_tests.rs`](../../../core/src/session/completion_review_tests.rs): `completion_review_late_input_remains_unreviewed_until_consumed`, `completion_review_pause_joins_inflight_installation_before_durable_stop`, `completion_review_stop_joins_finalization_after_active_entry_is_removed`.|Recorder/installation ownership completes before stop publication; accepted input is not silently promoted to consumed input.|
|Pause/restart|[`session/tests.rs`](../../../core/src/session/tests.rs): `rollout_reconstruction_uses_incomplete_history_without_pause_event`, `history_needs_continuation_ignores_preserved_work_notes`, `trims_dangling_tool_call_before_continuation`.|No synthetic user prompt, no reliance on a transient pause event, no unmatched call replay.|
|Notes and remote replacement|[`compact_remote_v2_recovery.rs`](../../../core/tests/suite/compact_remote_v2_recovery.rs): `notes_with_rejected_tools_complete_compaction_without_retry`, `plan_mode_compaction_preserves_literal_plan_tags_in_multipart_notes`, `failed_compaction_preserves_acknowledged_steering_for_continue`, `interrupt_after_notes_can_restart_and_continue`, `model_downshift_v2_uses_the_previous_provider_for_notes_and_compaction`.|Exact note bytes, runtime tool rejection, correct source model, protected replacement and preserved input after failure.|
|Steering order|[`pending_input.rs`](../../../core/tests/suite/pending_input.rs): `explicit_prompt_samples_before_pending_input`, `remote_v2_mid_turn_compaction_defers_pending_input_until_model_follow_up_finishes`; session tests for mismatched expected turn IDs and non-steerable turns.|Input stays with its observed task; compaction cannot overtake continuation; rejected steering cannot mutate pending input.|
|Retained output|[`context_harness.rs`](../../../core/tests/suite/context_harness.rs): `recover_command_output_from_replayed_receipt`, `interrupted_initial_exec_retains_a_replayable_artifact_locator`, `tight_unified_budget_preserves_command_control`; [`exec_output.rs`](../../../core/src/exec_output.rs): `short_write_then_disk_full_reopens_only_the_written_prefix`, `failed_catalog_publication_does_not_advertise_uncommitted_bytes`.|Locators survive replay; only committed bytes are promised; missing data remains explicit; tight budgets retain control/status information.|
|Owned jobs|The context-harness suite: `registered_job_delivers_once_in_the_originating_turn`, `parked_owner_cannot_sample_after_replacement`, `rolling_back_the_next_user_keeps_the_admitted_outcome_on_replay`; [`background_completion.rs`](../../../exec/tests/suite/background_completion.rs): `abrupt_exit_before_and_after_outcome_admission`.|One contextual delivery, no relaunch, no sampling by a stale owner, durable terminal state before admission.|
|Sampled-output replay|[`output_token_replay.rs`](../../../core/tests/suite/output_token_replay.rs): `output_token_replay_respects_provider_and_preserves_history`, `missing_previous_response_replays_full_hosted_search_history`.|Provider/flag transitions, preserved opaque data, full fresh-socket recovery, tool/result order.|
|WebSocket interactions|[`client_websockets.rs`](../../../core/tests/suite/client_websockets.rs): `responses_websocket_completion_review_followup_uses_incremental_create`, `responses_websocket_pause_continue_reconnects_after_in_flight_response`.|Stable review prefix and incremental suffixes; correct full input after an interrupted connection.|
|Restricted explorer|Context-harness fixture `explorer_is_fresh_and_rejects_unadvertised_side_effects`; [`grep_files.rs`](../../../core/src/tools/handlers/grep_files.rs) ignored probe `explorer_search_confines_the_child_process`.|Registration, dispatch and actual process execution restrictions agree; an ignored probe is not coverage.|
|Delegate/status consumers|[`codex_delegate.rs`](../../../core/src/codex_delegate.rs), [`menubar_bridge.rs`](../../../tui/src/menubar_bridge.rs), [`daemon.rs`](../../../codexd/src/daemon.rs): cancellation while forwarding, activation before content, duplicate completion and `stale_completion_key_does_not_remove_reused_child_turn_id`.|Review activity survives parent answer completion; composite identity and terminal ordering remain correct.|

## Implementation commands and boundaries

Follow [development guidance](../../development.md) and repository `AGENTS.md`. Use `cargo-local` and `just-local`; a separately installed shared wrapper is required. Prefix tests with `CODEX_SANDBOX_NETWORK_DISABLED=1`. Use the repository's actual localhost capability and skip guards; do not turn a skipped fixture into claimed transport coverage.

For a core-only implementation, start with its affected tests using `CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test --quiet -p codex-core <test_filter>`. Select the actual integration test target if a fixture is not exposed by that filter. Expand to `codex-exec`, `codex-tui`, `codex-codexd`, protocol or app-server consumers only when the changed contract reaches them. Scope formatting/lints as prescribed after Rust changes. Workspace-wide all-feature tests are a separate operation governed by the repository instructions; they are not a prerequisite for reviewing these docs.

If `ConfigToml` or nested config types change, regenerate `core/config.schema.json` with `just-local write-config-schema`. Protocol shape changes require the corresponding schema generation and consumer tests. Retiring only internal branches does not justify unrelated generated-file churn.

## Review each proposed patch

Before merging an implementation, record the exact source revision, selected behavior, deleted paths, config migration and affected checks. Include test outcomes and skips without claiming unsupported platform, backend or model behavior. Keep raw command logs outside permanent feature docs; retain the resulting contract and any material limitation.

For documentation-only updates, validate relative links, referenced source/test names, Markdown formatting, generated inventory coverage and patch application against the pinned base. A Rust rebuild adds no evidence to this kind of change.
