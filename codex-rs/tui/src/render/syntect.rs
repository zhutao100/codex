use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Span as RtSpan;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::Color as SyntectColor;
use syntect::highlighting::FontStyle as SyntectFontStyle;
use syntect::highlighting::Style as SyntectStyle;
use syntect::highlighting::Theme;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxReference;
use syntect::parsing::SyntaxSet;
use tracing::warn;
use vscode_theme_syntect::parse_vscode_theme_file;

pub(crate) const DEFAULT_SYNTAX_THEME: &str = "base16-ocean.dark";
const VS_CODE_THEME_PREFIX: &str = "vscode:";

pub(crate) struct SyntectHighlighter {
    inner: HighlightLines<'static>,
    syntax_set: &'static SyntaxSet,
    use_truecolor: bool,
}

impl SyntectHighlighter {
    pub(crate) fn from_path(path: &Path, theme_name: &str) -> Self {
        let syntax_set = syntect_syntax_set();
        let syntax = path
            .extension()
            .and_then(|ext| ext.to_str())
            .and_then(|ext| syntax_set.find_syntax_by_extension(ext))
            .unwrap_or_else(|| syntax_set.find_syntax_plain_text());
        Self::new(syntax, theme_name)
    }

    pub(crate) fn from_language(language: Option<&str>, theme_name: &str) -> Option<Self> {
        let syntax_set = syntect_syntax_set();
        let token = language
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .and_then(|token| token.split_whitespace().next())?;
        let syntax = syntax_set
            .find_syntax_by_token(token)
            .or_else(|| syntax_set.find_syntax_by_extension(token))?;
        Some(Self::new(syntax, theme_name))
    }

    fn new(syntax: &'static SyntaxReference, theme_name: &str) -> Self {
        let theme = syntect_theme(theme_name);
        Self {
            inner: HighlightLines::new(syntax, theme),
            syntax_set: syntect_syntax_set(),
            use_truecolor: theme_name.starts_with(VS_CODE_THEME_PREFIX),
        }
    }

    pub(crate) fn highlight_line(&mut self, line: &str) -> Vec<RtSpan<'static>> {
        if line.is_empty() {
            return Vec::new();
        }
        match self.inner.highlight_line(line, self.syntax_set) {
            Ok(ranges) => ranges
                .into_iter()
                .map(|(style, text)| {
                    RtSpan::styled(
                        text.to_string(),
                        syntect_style_to_ratatui(style, self.use_truecolor),
                    )
                })
                .collect(),
            Err(_) => vec![line.to_string().into()],
        }
    }
}

fn syntect_syntax_set() -> &'static SyntaxSet {
    static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn syntect_themes() -> &'static ThemeSet {
    static THEMES: OnceLock<ThemeSet> = OnceLock::new();
    THEMES.get_or_init(ThemeSet::load_defaults)
}

pub(crate) fn validate_syntax_theme(theme_name: &str) -> Option<String> {
    if let Some(theme_path) = theme_name.strip_prefix(VS_CODE_THEME_PREFIX) {
        return validate_vscode_theme(theme_name, theme_path).err();
    }

    if syntect_themes().themes.contains_key(theme_name) {
        return None;
    }

    Some(format!(
        "Unable to load syntax highlight theme '{theme_name}'. Falling back to '{DEFAULT_SYNTAX_THEME}'."
    ))
}

fn validate_vscode_theme(theme_name: &str, theme_path: &str) -> Result<(), String> {
    if theme_path.is_empty() {
        return Err(format!(
            "Unable to load syntax highlight theme '{theme_name}': missing vscode theme path. Falling back to '{DEFAULT_SYNTAX_THEME}'."
        ));
    }

    let path = Path::new(theme_path);
    let vscode_theme = parse_vscode_theme_file(path).map_err(|err| {
        format!(
            "Unable to load syntax highlight theme '{theme_name}' from {}: {err}. Falling back to '{DEFAULT_SYNTAX_THEME}'.",
            path.display()
        )
    })?;

    Theme::try_from(vscode_theme).map_err(|err| {
        format!(
            "Unable to load syntax highlight theme '{theme_name}' from {}: {err}. Falling back to '{DEFAULT_SYNTAX_THEME}'.",
            path.display()
        )
    })?;

    Ok(())
}

fn syntect_theme(theme_name: &str) -> &'static Theme {
    if let Some(theme_path) = theme_name.strip_prefix(VS_CODE_THEME_PREFIX)
        && let Some(theme) = vscode_theme(theme_name, theme_path)
    {
        return theme;
    }

    let theme_set = syntect_themes();
    #[expect(clippy::expect_used)]
    theme_set
        .themes
        .get(theme_name)
        .or_else(|| theme_set.themes.get(DEFAULT_SYNTAX_THEME))
        .or_else(|| theme_set.themes.values().next())
        .expect("syntect themes missing")
}

fn vscode_theme(theme_name: &str, theme_path: &str) -> Option<&'static Theme> {
    let cache = vscode_theme_cache();
    let mut cache = cache.lock().ok()?;

    if let Some(cached) = cache.get(theme_name).copied() {
        return cached;
    }

    let loaded = load_vscode_theme(theme_name, theme_path);
    cache.insert(theme_name.to_string(), loaded);
    loaded
}

fn vscode_theme_cache() -> &'static Mutex<HashMap<String, Option<&'static Theme>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<&'static Theme>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn load_vscode_theme(theme_name: &str, theme_path: &str) -> Option<&'static Theme> {
    if theme_path.is_empty() {
        warn!("invalid syntax theme '{theme_name}': missing vscode theme path");
        return None;
    }

    let path = Path::new(theme_path);
    let vscode_theme = match parse_vscode_theme_file(path) {
        Ok(theme) => theme,
        Err(err) => {
            warn!(
                "failed to load vscode syntax theme '{theme_name}' from {}: {err}",
                path.display()
            );
            return None;
        }
    };

    let theme = match Theme::try_from(vscode_theme) {
        Ok(theme) => theme,
        Err(err) => {
            warn!(
                "failed to convert vscode syntax theme '{theme_name}' from {}: {err}",
                path.display()
            );
            return None;
        }
    };

    Some(Box::leak(Box::new(theme)))
}

fn syntect_style_to_ratatui(style: SyntectStyle, use_truecolor: bool) -> Style {
    let mut out = Style::default();
    if style.foreground.a != 0 {
        out = out.fg(syntect_color_to_ratatui(style.foreground, use_truecolor));
    }
    if style.font_style.contains(SyntectFontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(SyntectFontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(SyntectFontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

fn syntect_color_to_ratatui(color: SyntectColor, use_truecolor: bool) -> Color {
    if use_truecolor {
        return Color::Rgb(color.r, color.g, color.b);
    }

    let r = color.r;
    let g = color.g;
    let b = color.b;
    if r == g && g == b {
        return if r < 64 {
            Color::Black
        } else if r < 128 {
            Color::DarkGray
        } else if r < 192 {
            Color::Gray
        } else {
            Color::White
        };
    }

    let max = r.max(g).max(b);
    let bright = max > 200;
    if r >= g && r >= b {
        if g > b {
            if bright {
                Color::LightYellow
            } else {
                Color::Yellow
            }
        } else if bright {
            Color::LightRed
        } else {
            Color::Red
        }
    } else if g >= r && g >= b {
        if b > r {
            if bright {
                Color::LightCyan
            } else {
                Color::Cyan
            }
        } else if bright {
            Color::LightGreen
        } else {
            Color::Green
        }
    } else if r > g {
        if bright {
            Color::LightMagenta
        } else {
            Color::Magenta
        }
    } else if bright {
        Color::LightBlue
    } else {
        Color::Blue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn built_in_theme_is_still_supported() {
        let mut highlighter =
            SyntectHighlighter::from_language(Some("rust"), DEFAULT_SYNTAX_THEME).expect("rust");
        let spans = highlighter.highlight_line("fn main() {}");
        assert!(!spans.is_empty());
        assert_eq!(validate_syntax_theme(DEFAULT_SYNTAX_THEME), None);
    }

    #[test]
    fn unknown_built_in_theme_returns_warning() {
        let warning = validate_syntax_theme("base16-not-a-real-theme").expect("warning");
        assert!(warning.contains("base16-not-a-real-theme"));
        assert!(warning.contains(DEFAULT_SYNTAX_THEME));
    }

    #[test]
    fn invalid_vscode_theme_falls_back_to_default() {
        let mut highlighter = SyntectHighlighter::from_language(
            Some("rust"),
            "vscode:/path/that/does/not/exist.json",
        )
        .expect("rust");
        let spans = highlighter.highlight_line("fn main() {}");
        assert!(!spans.is_empty());
        let warning =
            validate_syntax_theme("vscode:/path/that/does/not/exist.json").expect("warning");
        assert!(warning.contains(DEFAULT_SYNTAX_THEME));
    }

    #[test]
    fn empty_vscode_theme_path_returns_warning() {
        let warning = validate_syntax_theme("vscode:").expect("warning");
        assert!(warning.contains("missing vscode theme path"));
        assert!(warning.contains(DEFAULT_SYNTAX_THEME));
    }

    #[test]
    fn invalid_vscode_theme_json_returns_warning() -> anyhow::Result<()> {
        let file = NamedTempFile::new()?;
        std::fs::write(file.path(), "{not valid json")?;
        let theme_name = format!("{VS_CODE_THEME_PREFIX}{}", file.path().display());
        let warning = validate_syntax_theme(&theme_name).expect("warning");
        assert!(warning.contains(DEFAULT_SYNTAX_THEME));
        Ok(())
    }

    #[test]
    fn valid_vscode_theme_file_is_supported() -> anyhow::Result<()> {
        let file = NamedTempFile::new()?;
        std::fs::write(
            file.path(),
            r##"{
  "name": "Test Theme",
  "colors": {
    "editor.background": "#1e1e1e",
    "editor.foreground": "#d4d4d4"
  },
  "tokenColors": [
    {
      "scope": "source.rust",
      "settings": {
        "foreground": "#ff0000"
      }
    }
  ]
}"##,
        )?;
        let theme_name = format!("{VS_CODE_THEME_PREFIX}{}", file.path().display());
        let mut highlighter =
            SyntectHighlighter::from_language(Some("rust"), &theme_name).expect("rust");
        let spans = highlighter.highlight_line("fn main() {}");
        assert!(!spans.is_empty());
        assert_eq!(validate_syntax_theme(&theme_name), None);
        Ok(())
    }
}
