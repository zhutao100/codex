# Build optimization

Status: partially implemented. Developer profiles, native-async adapters and hidden proxy sidecars are implemented; broader CLI dependency-closure changes remain unimplemented and require measurements before choosing a split.

## Preserve the implemented decisions

- Keep concrete tool/task handlers native-async and box only at object-safe storage boundaries: [tool registry](../../../core/src/tools/registry.rs), [tasks](../../../core/src/tasks/mod.rs).
- Use `release-fast` for release-like local work; preserve the production release profile. See [development](../../development.md).
- `responses-api-proxy` and `stdio-to-uds` already use sidecar dispatch. Preserve arguments, exit status, missing-binary diagnostics and packaging support; do not plan this split again.

## Remaining work

1. Measure the selected CLI binary's dependency graph and release/release-fast package-clean rebuilds with [measure.sh](measure.sh). This cleans only `codex-cli`; dependencies remain warm, so it is not a cold-workspace benchmark. Capture compiler/toolchain, target and profile alongside timings outside the repository.
2. Identify a material cost before changing architecture. [CLI dependencies](../../../cli/Cargo.toml) still include MCP server, app-server tooling and cloud tasks; [shared CLI helpers](../../../common/Cargo.toml) still depend on OSS-provider helpers. A package in Cargo's compile output is not evidence that its standalone binary is being built.
3. If a cold command is worth splitting, change one dispatch/dependency boundary and its packaging together. Preserve CLI argument handling, exit status and missing-sidecar behavior, then compare dependency closure and timings.
4. Evaluate a separate OSS helper crate or optional feature only if callers and feature unification actually remove dependencies from the measured target. Moving files alone does not reduce the graph.
5. Investigate build-script invalidation or compiler hot spots only when timing/attribution points there. Avoid speculative derive removal or additional mandatory cache services.

## Completion criteria

The selected change preserves command and provider behavior, demonstrates a measured improvement under a described build condition, and leaves production packaging intact. Move the retained implementation decision to [development](../../development.md) and remove this plan when no accepted work remains.

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 bash docs/dev_plans/cargo_build_optimization/measure.sh --skip-builds
CODEX_SANDBOX_NETWORK_DISABLED=1 bash docs/dev_plans/cargo_build_optimization/measure.sh
```
