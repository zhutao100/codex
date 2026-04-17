## Plan: Contrast-aware diff styling hardening (auto/theme/custom) + off-mode readability

### Summary
Fix two classes of macOS Terminal.app issues in the diff renderer:
1) **“green-on-green” still happens in `diff_background="auto"`** because the effective foreground (terminal default and/or sign colors) can be too close to the chosen background tint.
2) **`diff_background="off"` looks “over dim”** because the `+/-` sign visually dominates and deletion rendering currently dims content in the syntax-highlight path.

The implementation makes diff rendering **contrast-aware** whenever a tinted background is applied (covers `auto`, `theme`, `custom`), and makes `off` mode **less sign-dominant** (dim sign only; never dim content).

---

## Goals / Success Criteria
### Terminal.app (primary)
- In `diff_background="auto"`: no “green-on-green” (sign and plain content remain readable) in the same Terminal.app profile where the glitch currently persists.
- In `diff_background="off"`: diff content after `+/-` is **not perceived dimmer** than the sign; specifically:
  - `+/-` is dimmed (only), content is not dimmed.
  - Delete lines do **not** apply a DIM overlay to syntax-highlighted spans.

### All modes (coverage)
- `theme` and `custom` do not regress into low-contrast combinations when backgrounds are enabled.
- No behavior change in ANSI-16 mode: still **foreground-only**, no backgrounds.

---

## Design Decisions (locked)
- **Contrast policy:** when a diff background tint is applied and effective fg contrast is too low, adjust **both**:
  - line/content baseline foreground (neutral high-contrast), and
  - sign foreground (fallback to the same neutral if needed).
- **Threshold:** minimum contrast ratio **3.0** (WCAG-like).
- **Off-mode cue:** `diff_background="off"` dims **sign only** (and optionally gutter), never content.

---

## Step-by-step Implementation

### 1) Add contrast utilities (pure, testable)
**Files**
- `tui/src/color.rs` (preferred home for color math) OR `tui/src/diff_render.rs` (if kept private).

**Add**
- `relative_luminance_srgb(rgb: (u8,u8,u8)) -> f32`
- `contrast_ratio(fg: (u8,u8,u8), bg: (u8,u8,u8)) -> f32` (ratio ≥ 1.0)
- Unit tests for the math (e.g., black/white ≈ 21, identical colors = 1).

### 2) Convert `ratatui::style::Color` → RGB when we can
**Files**
- `tui/src/diff_render.rs`

**Add helper**
- `color_to_rgb_for_contrast(color: Color) -> Option<(u8,u8,u8)>`
  - `Color::Rgb(r,g,b)` → Some
  - `Color::Indexed(i)` → Some(`XTERM_COLORS[i]`) (safe for our usage because diff backgrounds are always `Rgb` or `Indexed >= 16`)
  - otherwise → None

### 3) Capture needed runtime inputs for contrast decisions
**Files**
- `tui/src/diff_render.rs`
- `tui/src/terminal_palette.rs` (already has `default_fg()`)

**At render-frame snapshot time (`current_diff_render_style_context`)**
- Read and store:
  - `DiffBackgroundMode` (from current settings) so `off` can be distinguished from “no theme scopes”.
  - `terminal_default_fg_rgb: Option<(u8,u8,u8)>` using `terminal_palette::default_fg()`.
- Extend `DiffRenderStyleContext` to include these (no globals in per-line logic).

### 4) Compute a neutral “safe foreground” when backgrounds are enabled
**Files**
- `tui/src/diff_render.rs`

**Logic**
- When rendering Insert/Delete lines and `diff_backgrounds.add/del` is `Some(bg)`:
  - Determine `bg_rgb` via `color_to_rgb_for_contrast(bg)`. If `None`, skip contrast logic.
  - Determine `effective_default_fg_rgb`:
    - If `terminal_default_fg_rgb` is `Some`, use it.
    - If `None`, skip content override (sign logic still uses deterministic non-system colors; see next step).
  - If `contrast_ratio(default_fg, bg_rgb) < 3.0`:
    - Set a **line-level fg** to a neutral high-contrast color derived from bg lightness:
      - For dark bg: `Rgb(235,235,235)` (near-white but not pure white).
      - For light bg: reuse existing `LIGHT_TC_GUTTER_FG_RGB` (dark neutral).
    - Keep syntax-highlight spans intact; only spans with “no fg” inherit the line fg.

### 5) Stop using ANSI “system green/red” for sign when backgrounds are enabled
This avoids “unknown palette” colors (Terminal.app profile dependent) being placed on a known tinted background.

**Files**
- `tui/src/diff_render.rs`

**Replace**
- `style_sign_add()` / `style_sign_del()` with a function that depends on:
  - line kind (insert/delete),
  - whether background is present for that line,
  - color level (TrueColor/Ansi256/Ansi16),
  - background RGB (when available),
  - computed neutral safe fg (if computed).

**Policy**
- If **background is present** and color level is rich:
  - Prefer fixed, deterministic sign colors:
    - TrueColor: explicit RGB “diff sign green/red” (choose specific constants).
    - ANSI-256: `Indexed(46)` for green, `Indexed(196)` for red.
  - If contrast vs bg is still < 3.0 (based on known RGB for those indices), fall back to the **neutral safe fg** computed in step 4 (and add `BOLD` if needed).
- If **background is absent**:
  - Keep existing behavior (system green/red) to preserve familiar look.
- If **ANSI-16**:
  - Keep existing behavior (foreground-only).

### 6) Fix “off mode over-dim” by changing intensity cues
**Files**
- `tui/src/diff_render.rs`

**Changes**
- Remove the delete-only DIM overlay applied to syntax spans:
  - Today: `DiffLineType::Delete` adds `Modifier::DIM` to every syntax span.
  - New: do **not** dim content spans in any mode.
- In `diff_background="off"` specifically:
  - Apply `Modifier::DIM` to the `+/-` sign style (and optionally gutter) for both insert and delete.
  - Do not dim content.

This directly implements your preference: “reverse the dim: dim the `+/-`, do not dim the content”.

### 7) Ensure behavior applies to `auto`, `theme`, `custom`
**Files**
- `tui/src/diff_render.rs`

**Coverage check**
- The contrast logic triggers purely from “resolved bg is Some”:
  - `auto`: always has fallback bg → contrast logic active
  - `theme`: active only if theme scopes define bg → contrast logic active when present
  - `custom`: active if custom/fallback yields bg → contrast logic active

---

## Tests (must be added/updated)
**Files**
- `tui/src/diff_render.rs` tests module (extend existing unit tests)

### Add unit tests for:
1) **Contrast math**
- sanity checks for ratio ordering and bounds.

2) **Sign style is deterministic and contrast-safe when bg is present**
- Construct `ResolvedDiffBackgrounds { add: Some(Indexed(22)), del: Some(Indexed(52)) }`
- Simulate `terminal_default_fg_rgb = Some(green-ish)` and confirm:
  - sign style uses `Indexed(46/196)` or neutral fallback, not `Color::Green/Red` when bg present.

3) **Off-mode dims sign, not content**
- Set settings to `DiffBackgroundMode::Off`, render a syntax-highlighted insert/delete line via `push_wrapped_diff_line_with_syntax_and_style_context`.
- Assert:
  - sign span style contains `DIM`
  - content spans do **not** contain `DIM` (for delete too).

4) **No background in ANSI-16 remains unchanged**
- Existing ansi16 tests stay valid; update if signature changes.

---

## Manual Verification Checklist (Terminal.app)
1) In Terminal.app with the profile that reproduces the glitch:
   - Set `[tui] diff_background="auto"` and confirm:
     - `+` line is readable (sign and content not “green-on-green”).
2) Set `diff_background="theme"`:
   - Use a theme that defines `markup.inserted/deleted` backgrounds; verify no low-contrast.
3) Set `diff_background="custom"` with intentionally strong greens/reds:
   - Verify contrast hardening still keeps text readable.
4) Set `diff_background="off"`:
   - Confirm sign is dimmer than content; content is not dimmed.

---

## Optional Follow-up (only if still broken): Terminal.app capability guardrail (Plan D)
If after contrast hardening the glitch persists and evidence suggests Terminal.app is treated as TrueColor incorrectly:
- Add a Terminal.app-specific cap: require `COLORTERM=truecolor|24bit` to use TrueColor; otherwise downgrade to ANSI-256.
- Add unit tests for the policy table (`diff_color_level_for_terminal`) covering `TerminalName::AppleTerminal`.

---

## Assumptions
- `crossterm` foreground query (`default_fg()`) works often enough in Terminal.app to meaningfully apply content overrides; if it returns `None`, the sign changes alone still improve the “green-on-green” perception.
- Using fixed ANSI-256 indices (>=16) is acceptable for determinism and avoids user-palette-dependent system colors when backgrounds are tinted.
