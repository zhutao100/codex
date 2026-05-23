# Review Effectiveness Hardening Proposal

## Status

Implemented. The baseline post-turn completion review workflow exists in this project; this document records the hardening layer that makes the reviewer less likely to repeat the main session's failure modes.

## Problem

The reviewer was intended to catch blind spots from context-efficient coding turns: missed files, partial reads, duplicated logic, missing paired updates, incomplete requested scope, and final-answer overclaims. Test runs showed that the reviewer often used the same strategy as the main session: infer intent, run keyword searches, read narrow line ranges, and decide from those slices. When the reviewer repeats that strategy, it can miss the same files and context the main session missed.

The current prompt describes what to catch, but it does not define a distinct review methodology. The delegate also receives the normal instruction stream assembled from host-level `AGENTS.md`, project-level `AGENTS.md`, skills, and environment context. Project-level docs are useful to the reviewer, but host-level instructions can mix machine resource notes with main-session editing and efficient-search guidance.

## Goals

|Goal|Requirement|
|---|---|
|Complementary inspection|The reviewer must use coverage-driven end-state inspection rather than replaying the main session's context-saving path.|
|Read-only safety|The delegate remains read-only, web-disabled, and unable to request write approval.|
|Project context retained|Project-level `AGENTS.md` and repository-specific technical notes stay available.|
|Host instruction hygiene|Machine resource notes can remain available, but main-session-only editing and efficient-search instructions can be excluded from review delegates.|
|Runtime customization|Users can override review prompts and review-specific host instruction filenames from `~/.codex/config.toml` without rebuilding.|
|Stable protocol|Keep `PostTurnCompletionReviewOutputEvent { evaluation, fix_actions_advised }` unchanged while treating the boolean as a concrete follow-up signal for bug fixes and incomplete-scope delivery.|

## Proposal

### 1. Harden and make review prompts overridable

Keep `core/review_prompt.md` and `core/post_turn_completion_review_prompt.md` as built-in fallback prompts, but add runtime prompt-file overrides:

```toml
review_prompt_file = "~/.codex/prompts/review.md"
post_turn_completion_review_prompt_file = "~/.codex/prompts/post-turn-review.md"
```

When a prompt file is configured, read it during config loading or delegate setup, trim surrounding whitespace, reject empty files, and use the file contents as the delegate `base_instructions`. If unset, use the built-in `include_str!` prompt. The configured files are absolute or `~`-expanded paths, not filenames under the repository.

Also add explicit post-turn methodology requirements to `core/post_turn_completion_review_prompt.md`:

1. Do not perform a generic code review and do not merely replay keyword search plus narrow range reads.
2. Build a request-fulfillment checklist from every user message in the supplied interaction history.
3. Derive a coverage checklist from the request-fulfillment checklist, final assistant answer, changed or untracked files when available, repository manifests, adjacent modules, tests, schemas, protocol definitions, generated bindings, and registration points.
4. Prefer whole-file inspection for small and medium changed files. For large files, inspect whole symbol or module contexts plus imports, exports, registration tables, nearby tests, and paired helper functions.
5. Search for duplicate or existing functionality with multiple signals: new symbol names, semantic concepts, config keys, protocol variants, UI labels, test names, file families, and neighboring directories.
6. Require concrete evidence for each finding in `evaluation`: file path, missing paired surface, conflicting existing helper, unsupported final-answer claim, unfulfilled user requirement, or test gap.
7. Keep `fix_actions_advised = true` for concrete follow-up work the main session should verify and potentially perform, including incomplete requested scope.

The prompt should also state that when inherited instructions conflict with this review methodology, the post-turn review prompt wins.

### 2. Add review delegate instruction profiles

Extend the shared delegate configuration with an instruction profile:

```rust
pub(crate) enum ReviewDelegateInstructionProfile {
    Review,
    PostTurnCompletionReview,
}
```

`/review` should use `Review`. `PostTurnCompletionReviewTask` should use `PostTurnCompletionReview`.

Both profiles should keep project docs from `core/src/project_doc.rs` unchanged but replace the cloned parent host-level instruction text with audience-specific host instructions loaded from `~/.codex`.

Default host filename keys:

```toml
host_agents_filename = "AGENTS.md"
review_agents_filename = "AGENTS.review.md"
post_turn_completion_review_agents_filename = "AGENTS.post-turn-review.md"
```

Selection hierarchy:

1. Generic `/review`: `~/.codex/{review_agents_filename}` if it exists and is non-empty, otherwise `~/.codex/{host_agents_filename}`.
2. Post-turn completion review: `~/.codex/{post_turn_completion_review_agents_filename}` if it exists and is non-empty, then `~/.codex/{review_agents_filename}`, otherwise `~/.codex/{host_agents_filename}`.

Treat these config values as filenames under `codex_home`, not arbitrary paths. Reject path separators or absolute paths so review delegates cannot accidentally read project files as host instructions. If a configured filename is empty after trimming, fall back to the default for that slot. Missing files should fall through to the next candidate; unreadable files should surface a config/task error.

Use this hierarchy only for review delegate configs in the first pass; do not change normal main-session host instruction loading.

Do not add host section-marker parsing in the first hardening pass. Separate `AGENTS.*.md` files are simpler, avoid parsing already-concatenated instruction text, and minimize rebase-sensitive changes.

### 3. Keep project-level instructions unfiltered by default

Project-level `AGENTS.md` usually contains repository-specific build, test, architecture, and safety constraints. Those are still important for post-turn review. Do not globally remove project docs just because host docs need scoping. If a project later needs review-specific sections, add that as a separate project-doc audience feature rather than mixing it with the host split.

### 4. Add lightweight review coverage discipline

Do not change the protocol payload yet. Instead, require the `evaluation` Markdown to include a compact coverage statement, for example:

```markdown
Inspection coverage: checked request-fulfillment checklist, changed files, sibling modules, protocol registrations, and tests around ...

Findings:
- ...

Fix actions advised: yes
```

This gives the main session and the user visibility into whether the reviewer actually inspected the surfaces that matter, without adding a new schema migration.

## Implementation Surfaces

|Surface|Change|
|---|---|
|`core/review_prompt.md`|Remain the built-in `/review` fallback prompt.|
|`core/post_turn_completion_review_prompt.md`|Remain the built-in post-turn fallback prompt and add the coverage-driven review protocol and inherited-instruction precedence language.|
|`core/src/config/mod.rs`|Add `host_agents_filename`, `review_agents_filename`, `post_turn_completion_review_agents_filename`, `review_prompt_file`, and `post_turn_completion_review_prompt_file`; load prompt files and validate host filenames.|
|`core/config.schema.json`|Regenerate after adding config keys.|
|`core/src/tasks/review.rs`|Extend `ReviewDelegateConfigParams` with an instruction profile, use the resolved `/review` prompt, and load host instructions with the `/review` hierarchy.|
|`core/src/tasks/post_turn_completion_review.rs`|Use the resolved post-turn prompt and pass `PostTurnCompletionReview` as the instruction profile.|
|`core/src/project_doc.rs`|Keep project doc discovery unchanged, but expose or reuse a path for audience-aware host instruction composition before concatenating project docs.|
|`core/src/instructions/user_instructions.rs`|Host-instruction rendering stays source-separated; no marker parser is required for this pass.|

## Test Plan

- Prompt fixture test: `core/post_turn_completion_review_prompt.md` contains explicit language rejecting replay of keyword-search plus narrow range reads and requiring coverage-driven inspection.
- Prompt override tests: `review_prompt_file` and `post_turn_completion_review_prompt_file` replace the built-in prompts when set, reject empty files, and produce clear errors for unreadable files.
- Config helper test: `/review` loads `AGENTS.review.md` when present and otherwise falls back to `AGENTS.md`.
- Config helper test: post-turn review loads `AGENTS.post-turn-review.md`, then `AGENTS.review.md`, then `AGENTS.md`.
- Config helper test: `host_agents_filename`, `review_agents_filename`, and `post_turn_completion_review_agents_filename` override only the filename candidates for their hierarchy slots.
- Integration test: project-level `AGENTS.md` remains present in the post-turn delegate initial context while host main-session-only guidance is absent.
- Behavior test: a delegate response with coverage text and `fix_actions_advised = false` does not continue the main session.
- Behavior test: a delegate response with concrete findings and `fix_actions_advised = true` records one advisory developer message and triggers `TurnContinuationSource::PostTurnCompletionReview`.
- Behavior test: a multi-round session passes every real round's user messages and final assistant message to the post-turn delegate, while omitting tool transcript and review synthetic turns.

## Rollout

1. Land prompt hardening first because it is low-risk and directly addresses the observed behavior.
2. Add runtime prompt-file overrides for `/review` and post-turn completion review.
3. Add instruction-profile plumbing with `/review` using the `Review` profile and post-turn review using the `PostTurnCompletionReview` profile.
4. Add review-specific host file support with the explicit `AGENTS.*.md` hierarchy and config filename overrides.
5. Document the recommended host split: machine resources in `AGENTS.review.md` or `AGENTS.post-turn-review.md`, editing and efficient-search workflow in main-session host instructions.
6. Re-run the same test scenarios and compare whether the reviewer now checks changed files, adjacent modules, registration surfaces, duplicate implementations, and final-answer claims before deciding the boolean signal.
