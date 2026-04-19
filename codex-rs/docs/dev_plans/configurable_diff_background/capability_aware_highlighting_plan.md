# Capability-aware syntax highlighting plan (terminal color levels)

## Problem statement (why “diff background off” still looks wrong in Terminal.app)

Observed on macOS 15 **Terminal.app** with `tui.diff_background = "off"`:

- For files where syntax highlighting resolves (e.g. `.yaml`), both the diff content and surrounding context lines can appear “dimmed”.
- For files where syntax highlighting does **not** resolve (e.g. no extension), the diff content + context lines look normal.

This is consistent with the current rendering pipeline:

- When syntax highlighting resolves, `diff_render` uses the syntax spans’ foreground colors **as-is** for content, and does *not* apply the plain diff content style path.
- Our syntect → ratatui conversion currently emits `Color::Rgb` for most themes (truecolor output), regardless of what the terminal can actually render.
- Per `context_macos_terminal_color_support.md`, Terminal.app on macOS 14/15 should be treated as **ANSI-256** (no truecolor until macOS 26), so truecolor sequences are at best approximated and can look low-intensity/muted.

Goal: **Stop emitting truecolor colors when the terminal is not truecolor-capable**, and choose a default syntax theme that matches the terminal’s capability.

---

## Goals

1. **Capability-correct output:** never emit `Color::Rgb` (truecolor escape sequences) when the effective terminal capability is ANSI-256 or ANSI-16.
2. **Readable diffs in `diff_background="off"`:** syntax-highlighted content and context lines should not appear unintentionally dim in Terminal.app.
3. **Future-proof for macOS 26+:** allow truecolor on Terminal.app once it explicitly advertises it (while remaining conservative on macOS 14/15).
4. **Minimal surprise:** preserve existing behavior for terminals that already report truecolor correctly (iTerm2, Alacritty, etc.).

## Non-goals

- Perfect color matching between truecolor themes and quantized output.
- Changing diff-background behavior itself (this plan is about syntax highlighting).
- Adding new user-facing configuration knobs unless needed for overrides/debugging.

---

## Proposed design

### A) Introduce a single “highlight color level” decision (frame-scoped)

Add a `HighlightColorLevel` (or reuse `StdoutColorLevel`) that is computed once per render frame (similar to `DiffRenderStyleContext`), based on:

- `terminal_palette::stdout_color_level()` (via `supports-color`)
- `terminal_info().name`
- guardrails for `TerminalName::AppleTerminal` (see section D)

This level is then threaded through syntax highlighting conversion so we can decide whether `Color::Rgb` is allowed.

### B) Quantize syntect RGB colors when not truecolor

Adjust syntect → ratatui conversion in `tui/src/render/highlight.rs`:

1. Preserve existing alpha-based semantics (bat-compatible):
   - `ANSI_ALPHA_INDEX` (palette index) → keep mapping (`ansi_palette_color` / `Indexed(n)`).
   - `ANSI_ALPHA_DEFAULT` → keep `None` (terminal default).

2. For `OPAQUE_ALPHA` (true RGB theme colors):
   - If highlight level is **TrueColor**: keep `Color::Rgb(r,g,b)`.
   - If highlight level is **Ansi256**: quantize `(r,g,b)` to the nearest xterm fixed color:
     - Use `terminal_palette::best_color((r,g,b))`, but ensure it never returns `Color::default()` in this mode.
   - If highlight level is **Ansi16**: map `(r,g,b)` to a named ANSI color (optionally including bright variants), never `Rgb`.

Important: this must happen **before** diff rendering sees the spans, so that `diff_background="off"` doesn’t accidentally send truecolor sequences to a 256-color terminal.

### C) Make the adaptive default syntax theme capability-aware

Update `adaptive_default_theme_selection()` in `tui/src/render/highlight.rs` to select defaults based on both:

- terminal background lightness (`default_bg()`), and
- highlight color level (TrueColor / Ansi256 / Ansi16).

Proposed defaults:

- **TrueColor**:
  - light bg → `catppuccin-latte`
  - dark bg → `catppuccin-mocha` (current behavior)
- **Ansi256**:
  - use `base16-256` (palette-aware, avoids `Rgb` by construction)
- **Ansi16 / Unknown**:
  - use `ansi`

Rationale: even with RGB quantization, a theme designed for truecolor can compress poorly; choosing a palette-native theme improves readability and stability.

### D) Apple Terminal guardrails (macOS 14/15 vs 26+)

Per `context_macos_terminal_color_support.md`:

- macOS 14/15 Terminal.app: treat as **ANSI-256**.
- macOS 26 Terminal.app: truecolor exists, but terminfo may still declare `xterm-256color`; prefer explicit markers like `COLORTERM=truecolor`.

Policy for `TerminalName::AppleTerminal`:

- Only allow TrueColor highlighting when `COLORTERM` is `truecolor` or `24bit`.
- Otherwise, cap highlighting at ANSI-256 even if `supports-color` reports richer.

This is conservative for macOS 14/15 and still allows macOS 26+ to opt into truecolor when Terminal.app advertises it.

---

## Tests and verification

### Unit tests

- `highlight` conversion:
  - Given `StdoutColorLevel::Ansi256`, opaque syntect RGB produces `Color::Indexed(_)`, never `Color::Rgb`.
  - Given `StdoutColorLevel::Ansi16`, opaque syntect RGB produces a named ANSI color, never `Rgb`.
  - Given `StdoutColorLevel::TrueColor`, opaque syntect RGB stays `Rgb`.
- Apple Terminal guardrail:
  - `TerminalName::AppleTerminal` + no `COLORTERM` → highlight level is at most ANSI-256.
  - `TerminalName::AppleTerminal` + `COLORTERM=truecolor` → allow truecolor.

### Snapshot / rendering tests

- Add a focused snapshot in `tui/src/diff_render.rs` (or chatwidget snapshots) that renders:
  - a syntax-highlighted diff chunk under an ANSI-256 “simulated” environment,
  - with `diff_background="off"`,
  - and asserts no dim/unreadable output regressions.

### Manual verification (macOS Terminal.app)

On macOS 15 Terminal.app:

1. `tui.diff_background = "off"`, view a diff containing a highlighted file (YAML/Rust) and an unrecognized file.
2. Verify:
   - diff sign is dimmed (as intended),
   - diff content and surrounding context lines are not unintentionally dim/muted compared to unhighlighted files.

On a truecolor terminal (iTerm2/kitty):

- Verify truecolor themes still render as before.

---

## Rollout / compatibility notes

- This changes the *default* syntax theme choice on ANSI-256/ANSI-16 terminals.
- Users who explicitly configured a theme should keep their choice, but the output will be capability-quantized if it is an RGB theme on a non-truecolor terminal.
- Expect snapshot churn for any UI that renders syntax-highlighted text on ANSI-256 terminals.

