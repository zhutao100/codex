# Native feature alignment

The platform-independent migrations and intentional divergences are documented in [feature controls](../../features/feature-controls.md). The following changes are conditional, not enabled by matching an upstream default.

|Decision|Current behavior|Acceptance before changing it|
|---|---|---|
|Retire `use_linux_sandbox_bwrap`|The [Linux build](../../../linux-sandbox/build.rs) embeds bubblewrap only when requested; the [helper](../../../linux-sandbox/src/vendored_bwrap.rs) cannot run that backend in an ordinary build without it. The existing Landlock route remains selectable.|Build and package the supported backend on Linux. In a disposable guest run the ordinary helper, CLI, retained shell and MCP subprocess paths, including an unavailable-backend case. Remove the switch and old route only after the packaged default has an explicit failure contract.|
|Promote unified exec on Windows|Explicit `[windows].sandbox` selects sandbox mode independently of whether `unified_exec` is enabled. Its Windows default has not changed.|On Windows run ConPTY and non-TTY input, cancellation, large output and cleanup under each supported sandbox mode. Verify fallback on a machine without ConPTY, and test retained-input authority and background-job ownership before changing the default.|
|Promote shell snapshots|Opt-in for supported POSIX shells; unsupported shells run normally without a snapshot.|Before promotion, verify startup, reuse, user profile behavior and private file cleanup on each target shell/OS. Enabling PowerShell/Cmd requires their own capture and source contract.|

Do not treat skipped tests, registry metadata or a different platform's unit tests as native execution evidence.
