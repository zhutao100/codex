# Instruction Audit

## Scope and Status

Applied navigation corrections to the canonical global files `~/.codex/AGENTS.md` and `~/.codex/AGENTS_structured_search.md`, plus the repository's root and Rust instructions. Global files live outside this Git repository; the repository commit records their rationale and portable project guidance, not ownership of those external files.

The goal is bounded, evidence-complete navigation, not a fixed command ritual. These corrections do not enable a runtime feature or manufacture delegation tools.

## Findings and Corrections

|Observed problem|Correction|
|---|---|
|`rg -m N` was presented alongside global result caps.|Describe it as matching lines per file. Scope files first and independently bound aggregate response bytes/tokens.|
|An overview requested up to 400 files despite a smaller wrapper display cap.|Use one smaller overview only when layout is unknown, inspect omission metadata, then narrow or obtain a complete raw manifest.|
|Annotated output was suggested as a machine manifest after taking its first field.|Use native `fd`/`rg`, structured output or `git ls-files`; stripping a field does not remove annotation rows or restore omitted paths.|
|Large-file relevance could override response budgeting; several acceptable reads were batched into a truncated response.|Budget the combined response. Use whole small files when they fit, and symbol-bounded reads for large files; bytes matter even when line counts are low.|
|Historical indexing prefixed old path names with the live repository root.|Extract the selected revision before indexing. A historical path list does not make live file contents historical.|
|Example indexes used shared temporary filenames without an ownership rule.|Use a per-task temporary directory, reset extraction outputs per run and avoid cross-agent index collisions.|
|Subdirectory Git pathspecs and cross-repository CWD were easy to mix up.|Use explicit repository roots/workdirs and root-relative paths. Verify nonempty selections before extracting or interpreting absence.|
|Rust function extraction enumerated a few visibility/async/return variants.|Use AST `function_item` for body-bearing functions; account separately for declaration-only methods, macro expansion and grammar gaps.|
|AST line conversion looked suitable for exact block extraction.|Document zero-based positions and exclusive ends; line slices are previews, not byte-exact extraction.|
|Structured search was framed as mandatory before all meaningful navigation.|Use it when it adds structural information. A known filename or literal lookup does not need a workspace-wide symbol index.|
|Scope expansion was allowed mainly for missing symbols/parser gaps.|Follow actual caller/dispatch/configuration dependencies, not definitions alone. Feature availability and output limits often live in callers.|
|The article's delegation instructions assumed newer field/tool names and universal waiting.|Require the exposed backend schema, distinct ownership, selective evidence checks and waiting only when blocked. Preserve higher-priority capability/instruction boundaries.|
|Repeated reading of already-injected instructions adds output without new knowledge.|Read relevant referenced files rather than dumping the entire instruction stack again; links are not automatically expanded by the loader.|
|Unqualified defaults could be transferred between the fork, the reference worktree and official documentation.|Pin revisions, verify checkout identity, and distinguish source presence, effective exposure, guidance and observed behavior.|

The research itself exposed two of these mistakes: a root-prefixed `ls-tree` pathspec used from the Rust subdirectory selected nothing, and a mixed-repository read used the wrong CWD. Neither empty output was treated as evidence of code absence; both were corrected with explicit roots. An early oversized batch was also truncated, confirming that individual read bounds do not bound the aggregate tool response.

## Deterministic Checks

Small temporary fixtures were executed on 2026-10-30, with no model calls, network use, application launch or broad build:

|Probe|Observed result|
|---|---|
|Three files, each with 25 matches; native `rg -m 2`|Six matching lines total, not two.|
|`rg-x -n -m 2` on the same fixture|Annotated stdout includes `@file` records, so it is not a raw match stream.|
|`fd-x --max-results 2` over three files|Two paths displayed; metadata reports three rows and two returned.|
|`fd-x --max-results 205` over 208 files|200 paths displayed; metadata distinguishes `shown_rows=200`, `returned_rows=205`, and `rows=208`. This is the observed wrapper configuration, not a universal native `fd` limit.|
|`sed-x` reading one 50,001-byte line|All 50,001 bytes returned in this probe. Do not assume line counts or the existence of a wrapper protects the outer tool-response budget.|
|AST kind query over Rust visibility/generic/async/unsafe/impl/trait-default variants|All six body-bearing functions found; the declaration-only trait method was intentionally outside `function_item`.|

The source research also generated narrow native manifests and compact ctags indexes: 31 C files and 44 M files. Index diagnostics were retained and the known TOML-parser warning inspected rather than confused with missing Rust/TypeScript evidence.

These checks validate the navigation contracts, not live token savings or model behavior.

Document verification passed for six research/plan files, 19 local links and 90 revision-qualified source locators, including shorthand anchors. Catalog-default and reported-cost arithmetic checks passed. Source anchors were also reviewed against the pinned lines; existence checks alone are not semantic verification. No Rust source changed, so Rust formatting/build/test suites and live model evaluations were not run.

## Guidance Retained

- Prefer local inspection wrappers for readable views and native/structured streams for exact machine inputs.
- Respect ignore rules initially; deliberately expand hidden, ignored or binary scope when a traced question requires it.
- Prefer a complete small-file read over repeated tiny overlapping slices.
- Combine bounded acquisition with independent exploration; neither is a substitute for the other.
- Keep applicable instructions and selective verification. Do not copy the article's universal instruction omission, blind trust, or "parent must never read while workers run" policies.
- Preserve local Cargo wrappers, explicit test environment settings, exclusions and VM boundaries.

## Maintenance

Do not copy the complete global instruction files into this repository; duplicate canonical text will drift. Keep project-specific navigation rules in AGENTS.md and substantive source conclusions here. Future changes to wrapper behavior or tool APIs should rerun small contract probes and revise the relevant rule, not preserve an obsolete numeric limit as a permanent fact.
