use codex_core::config::types::ProgressTraceCategoryStyleConfig;
use codex_core::config::types::ProgressTraceStyleConfig;
use codex_protocol::protocol::ProgressTraceCategory;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProgressTraceCategoryStyle {
    pub(crate) fg: Option<Color>,
    pub(crate) dim: bool,
    pub(crate) bold: bool,
}

impl ProgressTraceCategoryStyle {
    pub(crate) fn to_style(self) -> Style {
        let mut style = Style::default();
        if let Some(color) = self.fg {
            style = style.fg(color);
        }
        if self.dim {
            style = style.add_modifier(Modifier::DIM);
        }
        if self.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        style
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProgressTraceStyles {
    pub(crate) tool: ProgressTraceCategoryStyle,
    pub(crate) edit: ProgressTraceCategoryStyle,
    pub(crate) waiting: ProgressTraceCategoryStyle,
    pub(crate) network: ProgressTraceCategoryStyle,
    pub(crate) prefill: ProgressTraceCategoryStyle,
    pub(crate) reasoning: ProgressTraceCategoryStyle,
    pub(crate) r#gen: ProgressTraceCategoryStyle,
}

impl Default for ProgressTraceStyles {
    fn default() -> Self {
        // Distinct-C defaults chosen to keep every category on a unique base color.
        Self {
            tool: ProgressTraceCategoryStyle {
                fg: Some(Color::Cyan),
                dim: false,
                bold: false,
            },
            edit: ProgressTraceCategoryStyle {
                fg: Some(Color::Red),
                dim: false,
                bold: false,
            },
            waiting: ProgressTraceCategoryStyle {
                fg: Some(Color::DarkGray),
                dim: true,
                bold: false,
            },
            network: ProgressTraceCategoryStyle {
                fg: Some(Color::Blue),
                dim: false,
                bold: false,
            },
            prefill: ProgressTraceCategoryStyle {
                fg: Some(Color::Yellow),
                dim: true,
                bold: false,
            },
            reasoning: ProgressTraceCategoryStyle {
                fg: Some(Color::White),
                dim: true,
                bold: false,
            },
            r#gen: ProgressTraceCategoryStyle {
                fg: Some(Color::Green),
                dim: false,
                bold: true,
            },
        }
    }
}

pub(crate) fn resolve_progress_trace_styles(
    config: Option<&ProgressTraceStyleConfig>,
) -> (ProgressTraceStyles, Vec<String>) {
    let mut styles = ProgressTraceStyles::default();
    let mut warnings = Vec::new();

    if let Some(config) = config {
        apply_style_override(
            &mut styles.tool,
            config.tool.as_ref(),
            ProgressTraceCategory::Tool,
            &mut warnings,
        );
        apply_style_override(
            &mut styles.edit,
            config.edit.as_ref(),
            ProgressTraceCategory::Edit,
            &mut warnings,
        );
        apply_style_override(
            &mut styles.waiting,
            config.waiting.as_ref(),
            ProgressTraceCategory::Waiting,
            &mut warnings,
        );
        apply_style_override(
            &mut styles.network,
            config.network.as_ref(),
            ProgressTraceCategory::Network,
            &mut warnings,
        );
        apply_style_override(
            &mut styles.prefill,
            config.prefill.as_ref(),
            ProgressTraceCategory::Prefill,
            &mut warnings,
        );
        apply_style_override(
            &mut styles.reasoning,
            config.reasoning.as_ref(),
            ProgressTraceCategory::Reasoning,
            &mut warnings,
        );
        apply_style_override(
            &mut styles.r#gen,
            config.r#gen.as_ref(),
            ProgressTraceCategory::Gen,
            &mut warnings,
        );
    }

    (styles, warnings)
}

fn apply_style_override(
    base: &mut ProgressTraceCategoryStyle,
    override_cfg: Option<&ProgressTraceCategoryStyleConfig>,
    category: ProgressTraceCategory,
    warnings: &mut Vec<String>,
) {
    let Some(cfg) = override_cfg else {
        return;
    };

    if let Some(color_token) = cfg.color.as_deref() {
        if let Some(color) = parse_config_color_token(color_token) {
            base.fg = color;
        } else {
            warnings.push(format!(
                "Invalid progress trace color '{color_token}' for category '{}'; using default.",
                progress_trace_category_label(category)
            ));
        }
    }

    if let Some(dim) = cfg.dim {
        base.dim = dim;
    }
    if let Some(bold) = cfg.bold {
        base.bold = bold;
    }
}

fn parse_config_color_token(token: &str) -> Option<Option<Color>> {
    let normalized = token.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }
    if matches!(normalized.as_str(), "default" | "reset") {
        return Some(None);
    }
    if let Some(color) = parse_named_ansi_color(&normalized) {
        return Some(Some(color));
    }
    parse_hex_to_nearest_ansi(&normalized).map(Some)
}

fn parse_named_ansi_color(value: &str) -> Option<Color> {
    match value {
        "black" => Some(Color::Black),
        "dark-gray" | "dark_grey" => Some(Color::DarkGray),
        "gray" | "grey" => Some(Color::Gray),
        "white" => Some(Color::White),
        "red" => Some(Color::Red),
        "light-red" | "light_red" => Some(Color::LightRed),
        "green" => Some(Color::Green),
        "light-green" | "light_green" => Some(Color::LightGreen),
        "yellow" => Some(Color::Yellow),
        "light-yellow" | "light_yellow" => Some(Color::LightYellow),
        "blue" => Some(Color::Blue),
        "light-blue" | "light_blue" => Some(Color::LightBlue),
        "magenta" => Some(Color::Magenta),
        "light-magenta" | "light_magenta" => Some(Color::LightMagenta),
        "cyan" => Some(Color::Cyan),
        "light-cyan" | "light_cyan" => Some(Color::LightCyan),
        _ => None,
    }
}

fn parse_hex_to_nearest_ansi(value: &str) -> Option<Color> {
    let bytes = value.as_bytes();
    if bytes.len() != 7 || bytes.first().copied() != Some(b'#') {
        return None;
    }
    if !bytes[1..].iter().all(u8::is_ascii_hexdigit) {
        return None;
    }

    let r = u8::from_str_radix(&value[1..3], 16).ok()?;
    let g = u8::from_str_radix(&value[3..5], 16).ok()?;
    let b = u8::from_str_radix(&value[5..7], 16).ok()?;
    Some(nearest_ansi_color(r, g, b))
}

fn nearest_ansi_color(r: u8, g: u8, b: u8) -> Color {
    // Keep this list to ANSI-safe named colors for broad terminal compatibility.
    let palette: [(Color, (u8, u8, u8)); 16] = [
        (Color::Black, (0, 0, 0)),
        (Color::DarkGray, (85, 85, 85)),
        (Color::Gray, (170, 170, 170)),
        (Color::White, (255, 255, 255)),
        (Color::Red, (205, 49, 49)),
        (Color::LightRed, (241, 76, 76)),
        (Color::Green, (13, 188, 121)),
        (Color::LightGreen, (35, 209, 139)),
        (Color::Yellow, (229, 229, 16)),
        (Color::LightYellow, (245, 245, 67)),
        (Color::Blue, (36, 114, 200)),
        (Color::LightBlue, (59, 142, 234)),
        (Color::Magenta, (188, 63, 188)),
        (Color::LightMagenta, (214, 112, 214)),
        (Color::Cyan, (17, 168, 205)),
        (Color::LightCyan, (41, 184, 219)),
    ];

    let mut best = palette[0].0;
    let mut best_distance = i32::MAX;
    for (color, (pr, pg, pb)) in palette {
        let dr = i32::from(r) - i32::from(pr);
        let dg = i32::from(g) - i32::from(pg);
        let db = i32::from(b) - i32::from(pb);
        let distance = dr * dr + dg * dg + db * db;
        if distance < best_distance {
            best_distance = distance;
            best = color;
        }
    }
    best
}

pub(crate) fn progress_trace_style_for_category(
    styles: &ProgressTraceStyles,
    category: ProgressTraceCategory,
) -> ProgressTraceCategoryStyle {
    match category {
        ProgressTraceCategory::Tool => styles.tool,
        ProgressTraceCategory::Edit => styles.edit,
        ProgressTraceCategory::Waiting => styles.waiting,
        ProgressTraceCategory::Network => styles.network,
        ProgressTraceCategory::Prefill => styles.prefill,
        ProgressTraceCategory::Reasoning => styles.reasoning,
        ProgressTraceCategory::Gen => styles.r#gen,
    }
}

pub(crate) fn progress_trace_span(
    category: ProgressTraceCategory,
    styles: &ProgressTraceStyles,
) -> Span<'static> {
    Span::styled(
        "▮",
        progress_trace_style_for_category(styles, category).to_style(),
    )
}

pub(crate) fn progress_trace_category_label(category: ProgressTraceCategory) -> &'static str {
    match category {
        ProgressTraceCategory::Tool => "tool",
        ProgressTraceCategory::Edit => "edit",
        ProgressTraceCategory::Waiting => "waiting",
        ProgressTraceCategory::Network => "network",
        ProgressTraceCategory::Prefill => "prefill",
        ProgressTraceCategory::Reasoning => "reasoning",
        ProgressTraceCategory::Gen => "gen",
    }
}

pub(crate) fn progress_trace_style_description(
    category: ProgressTraceCategory,
    styles: &ProgressTraceStyles,
) -> String {
    let style = progress_trace_style_for_category(styles, category);
    let mut out = color_name(style.fg).to_string();
    if style.dim {
        out.push_str(" + dim");
    }
    if style.bold {
        out.push_str(" + bold");
    }
    out
}

fn color_name(color: Option<Color>) -> &'static str {
    match color {
        None => "default",
        Some(Color::Black) => "black",
        Some(Color::DarkGray) => "dark-gray",
        Some(Color::Gray) => "gray",
        Some(Color::White) => "white",
        Some(Color::Red) => "red",
        Some(Color::LightRed) => "light-red",
        Some(Color::Green) => "green",
        Some(Color::LightGreen) => "light-green",
        Some(Color::Yellow) => "yellow",
        Some(Color::LightYellow) => "light-yellow",
        Some(Color::Blue) => "blue",
        Some(Color::LightBlue) => "light-blue",
        Some(Color::Magenta) => "magenta",
        Some(Color::LightMagenta) => "light-magenta",
        Some(Color::Cyan) => "cyan",
        Some(Color::LightCyan) => "light-cyan",
        Some(_) => "custom",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn resolves_hex_to_nearest_ansi() {
        let cfg = ProgressTraceStyleConfig {
            tool: Some(ProgressTraceCategoryStyleConfig {
                color: Some("#4aa3ff".to_string()),
                dim: None,
                bold: None,
            }),
            ..Default::default()
        };

        let (styles, warnings) = resolve_progress_trace_styles(Some(&cfg));
        assert!(warnings.is_empty());
        assert_eq!(styles.tool.fg, Some(Color::LightBlue));
    }

    #[test]
    fn warns_and_falls_back_for_invalid_color() {
        let defaults = ProgressTraceStyles::default();
        let cfg = ProgressTraceStyleConfig {
            r#gen: Some(ProgressTraceCategoryStyleConfig {
                color: Some("banana".to_string()),
                dim: Some(true),
                bold: Some(false),
            }),
            ..Default::default()
        };
        let (styles, warnings) = resolve_progress_trace_styles(Some(&cfg));

        assert_eq!(styles.r#gen.fg, defaults.r#gen.fg);
        assert_eq!(styles.r#gen.dim, true);
        assert_eq!(styles.r#gen.bold, false);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn default_palette_uses_unique_base_colors() {
        let styles = ProgressTraceStyles::default();
        let colors = [
            styles.tool.fg,
            styles.edit.fg,
            styles.waiting.fg,
            styles.network.fg,
            styles.prefill.fg,
            styles.reasoning.fg,
            styles.r#gen.fg,
        ];
        let unique = std::collections::BTreeSet::from_iter(colors.into_iter().map(color_name));
        assert_eq!(unique.len(), 7);
    }
}
