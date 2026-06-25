# `/usage` Minimal Backport

Status: Proposed

Goal: add account token-activity support to the `custom-0.98.0` branch with the smallest surfaced change set that fits the older TUI architecture.

Primary reference: upstream commit `c884536d8 feat(tui): reland token activity command (#27925)`.

## Scope

- Add built-in `/usage`, `/usage daily`, `/usage weekly`, and `/usage cumulative`.
- Fetch account token activity from the ChatGPT/Codex backend profile endpoint.
- Render a compact token activity card in the TUI.
- Keep `/status` as the rate-limit and credits surface.
- Do not port upstream app-server session plumbing, reset-credit redemption, remote plugin changes, or terminal resize reflow changes.

## Documents

- [upstream_audit.md](upstream_audit.md) records the reference behavior and dependency split.
- [implementation_plan.md](implementation_plan.md) lists the minimal port phases and validation.
