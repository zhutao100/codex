# Native feature alignment

The platform-independent migrations and intentional divergences are documented in [feature controls](../../features/feature-controls.md). The following changes are conditional, not enabled by matching an upstream default.

|Decision|Current behavior|Acceptance before changing it|
|---|---|---|
|Promote unified exec on Windows|The Windows default has not changed. A requested token sandbox is rejected before the retained PTY or pipe launcher runs; the one-shot shell executor is the enforcing path.|Implement token-enforced retained process creation first. On Windows run ConPTY and non-TTY input, cancellation, large output and cleanup under each supported mode. Verify unsupported-ConPTY fallback, retained-input authority and background-job ownership before promotion.|
|Promote shell snapshots|Opt-in for supported POSIX shells; unsupported shells run normally without a snapshot.|Before promotion, verify startup, reuse, user profile behavior and private file cleanup on each target shell/OS. Enabling PowerShell/Cmd requires their own capture and source contract.|

Do not treat skipped tests, registry metadata or a different platform's unit tests as native execution evidence.
