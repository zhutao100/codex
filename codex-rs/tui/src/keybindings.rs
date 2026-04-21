use crate::key_hint::KeyBinding;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub(crate) struct Keybindings {
    pub(crate) submit: Vec<KeyBinding>,
    pub(crate) newline: Vec<KeyBinding>,
    pub(crate) paste: Vec<KeyBinding>,
    pub(crate) copy_prompt: Vec<KeyBinding>,
    pub(crate) copy_last_output: Vec<KeyBinding>,
    pub(crate) copy_code_block: Vec<KeyBinding>,
    pub(crate) editor: EditorKeybindings,
}

#[derive(Debug, Clone)]
pub(crate) struct EditorKeybindings {
    pub(crate) move_left: Vec<KeyBinding>,
    pub(crate) move_right: Vec<KeyBinding>,
    pub(crate) move_up: Vec<KeyBinding>,
    pub(crate) move_down: Vec<KeyBinding>,
    pub(crate) move_word_left: Vec<KeyBinding>,
    pub(crate) move_word_right: Vec<KeyBinding>,
    pub(crate) delete_backward: Vec<KeyBinding>,
    pub(crate) delete_forward: Vec<KeyBinding>,
    pub(crate) delete_word_backward: Vec<KeyBinding>,
    pub(crate) delete_word_forward: Vec<KeyBinding>,
    pub(crate) home: Vec<KeyBinding>,
    pub(crate) end: Vec<KeyBinding>,
}

impl Keybindings {
    pub(crate) fn from_config(
        keybindings: &HashMap<String, Vec<String>>,
        enhanced_keys_supported: bool,
        is_wsl: bool,
    ) -> Self {
        let defaults = Self::defaults(enhanced_keys_supported, is_wsl);

        Self {
            submit: bindings_or_default(keybindings, "submit", defaults.submit),
            newline: bindings_or_default(keybindings, "newline", defaults.newline),
            paste: bindings_or_default(keybindings, "paste", defaults.paste),
            copy_prompt: bindings_or_default(keybindings, "copy_prompt", defaults.copy_prompt),
            copy_last_output: bindings_or_default(
                keybindings,
                "copy_last_output",
                defaults.copy_last_output,
            ),
            copy_code_block: bindings_or_default(
                keybindings,
                "copy_code_block",
                defaults.copy_code_block,
            ),
            editor: EditorKeybindings {
                move_left: bindings_or_default(keybindings, "editor_move_left", Vec::new()),
                move_right: bindings_or_default(keybindings, "editor_move_right", Vec::new()),
                move_up: bindings_or_default(keybindings, "editor_move_up", Vec::new()),
                move_down: bindings_or_default(keybindings, "editor_move_down", Vec::new()),
                move_word_left: bindings_or_default(
                    keybindings,
                    "editor_move_word_left",
                    Vec::new(),
                ),
                move_word_right: bindings_or_default(
                    keybindings,
                    "editor_move_word_right",
                    Vec::new(),
                ),
                delete_backward: bindings_or_default(
                    keybindings,
                    "editor_delete_backward",
                    Vec::new(),
                ),
                delete_forward: bindings_or_default(
                    keybindings,
                    "editor_delete_forward",
                    Vec::new(),
                ),
                delete_word_backward: bindings_or_default(
                    keybindings,
                    "editor_delete_word_backward",
                    Vec::new(),
                ),
                delete_word_forward: bindings_or_default(
                    keybindings,
                    "editor_delete_word_forward",
                    Vec::new(),
                ),
                home: bindings_or_default(keybindings, "editor_home", Vec::new()),
                end: bindings_or_default(keybindings, "editor_end", Vec::new()),
            },
        }
    }

    fn defaults(enhanced_keys_supported: bool, is_wsl: bool) -> Self {
        let submit = vec![KeyBinding::new(KeyCode::Enter, KeyModifiers::NONE)];

        let newline = if enhanced_keys_supported {
            vec![KeyBinding::new(KeyCode::Enter, KeyModifiers::SHIFT)]
        } else {
            vec![KeyBinding::new(KeyCode::Char('j'), KeyModifiers::CONTROL)]
        };

        let paste = if is_wsl {
            vec![
                KeyBinding::new(
                    KeyCode::Char('v'),
                    KeyModifiers::CONTROL.union(KeyModifiers::ALT),
                ),
                KeyBinding::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
            ]
        } else {
            vec![KeyBinding::new(KeyCode::Char('v'), KeyModifiers::CONTROL)]
        };

        #[cfg(target_os = "macos")]
        let paste = {
            let mut paste = paste;
            paste.insert(0, KeyBinding::new(KeyCode::Char('v'), KeyModifiers::SUPER));
            paste
        };

        let copy_prompt = vec![KeyBinding::new(KeyCode::Char('l'), KeyModifiers::CONTROL)];
        let copy_last_output = vec![
            KeyBinding::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
            KeyBinding::new(KeyCode::F(8), KeyModifiers::NONE),
        ];
        let copy_code_block = vec![
            KeyBinding::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            KeyBinding::new(KeyCode::F(6), KeyModifiers::NONE),
        ];

        Self {
            submit,
            newline,
            paste,
            copy_prompt,
            copy_last_output,
            copy_code_block,
            editor: EditorKeybindings {
                move_left: Vec::new(),
                move_right: Vec::new(),
                move_up: Vec::new(),
                move_down: Vec::new(),
                move_word_left: Vec::new(),
                move_word_right: Vec::new(),
                delete_backward: Vec::new(),
                delete_forward: Vec::new(),
                delete_word_backward: Vec::new(),
                delete_word_forward: Vec::new(),
                home: Vec::new(),
                end: Vec::new(),
            },
        }
    }
}

fn bindings_or_default(
    keybindings: &HashMap<String, Vec<String>>,
    action: &str,
    defaults: Vec<KeyBinding>,
) -> Vec<KeyBinding> {
    let Some(raw) = keybindings.get(action) else {
        return defaults;
    };

    let parsed: Vec<KeyBinding> = raw
        .iter()
        .filter_map(|binding| match parse_keybinding(binding) {
            Ok(binding) => Some(binding),
            Err(err) => {
                tracing::warn!("invalid keybinding for {action}: {err}");
                None
            }
        })
        .collect();

    if parsed.is_empty() { defaults } else { parsed }
}

fn parse_keybinding(raw: &str) -> Result<KeyBinding, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty binding".to_string());
    }

    let mut modifiers = KeyModifiers::NONE;
    let mut fn_modifier = false;

    let mut parts: Vec<&str> = raw
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();

    let Some(key_part) = parts.pop() else {
        return Err("empty binding".to_string());
    };

    for modifier in parts {
        match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "ctl" => modifiers |= KeyModifiers::CONTROL,
            "alt" | "option" | "opt" | "meta" => modifiers |= KeyModifiers::ALT,
            "shift" => modifiers |= KeyModifiers::SHIFT,
            "cmd" | "command" | "super" => modifiers |= KeyModifiers::SUPER,
            "fn" => fn_modifier = true,
            other => return Err(format!("unknown modifier '{other}'")),
        }
    }

    let key = parse_keycode(key_part, fn_modifier)?;
    Ok(KeyBinding::new(key, modifiers))
}

fn parse_keycode(raw: &str, fn_modifier: bool) -> Result<KeyCode, String> {
    let key = raw.trim();
    if key.is_empty() {
        return Err("missing key".to_string());
    }

    let lower = key.to_ascii_lowercase();
    if fn_modifier {
        return match lower.as_str() {
            "up" => Ok(KeyCode::PageUp),
            "down" => Ok(KeyCode::PageDown),
            "left" => Ok(KeyCode::Home),
            "right" => Ok(KeyCode::End),
            _ => Err("fn modifier only supports Up/Down/Left/Right".to_string()),
        };
    }

    match lower.as_str() {
        "enter" | "return" => Ok(KeyCode::Enter),
        "esc" | "escape" => Ok(KeyCode::Esc),
        "tab" => Ok(KeyCode::Tab),
        "backspace" | "bs" => Ok(KeyCode::Backspace),
        "delete" | "del" => Ok(KeyCode::Delete),
        "space" | "spc" => Ok(KeyCode::Char(' ')),
        "up" => Ok(KeyCode::Up),
        "down" => Ok(KeyCode::Down),
        "left" => Ok(KeyCode::Left),
        "right" => Ok(KeyCode::Right),
        "pageup" | "pgup" => Ok(KeyCode::PageUp),
        "pagedown" | "pgdn" => Ok(KeyCode::PageDown),
        "home" => Ok(KeyCode::Home),
        "end" => Ok(KeyCode::End),
        _ => {
            if lower.starts_with('f')
                && let Ok(n) = lower[1..].parse::<u8>()
                && (1..=24).contains(&n)
            {
                return Ok(KeyCode::F(n));
            }
            let mut chars = key.chars();
            let Some(ch) = chars.next() else {
                return Err("missing key".to_string());
            };
            if chars.next().is_some() {
                return Err(format!("unknown key '{key}'"));
            }
            Ok(KeyCode::Char(ch))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn defaults_include_copy_last_output_shortcuts() {
        let bindings = Keybindings::from_config(&HashMap::new(), true, false);
        assert_eq!(
            bindings.copy_last_output,
            vec![
                KeyBinding::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
                KeyBinding::new(KeyCode::F(8), KeyModifiers::NONE),
            ],
        );
    }

    #[test]
    fn defaults_include_copy_code_block_shortcuts() {
        let bindings = Keybindings::from_config(&HashMap::new(), true, false);
        assert_eq!(
            bindings.copy_code_block,
            vec![
                KeyBinding::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
                KeyBinding::new(KeyCode::F(6), KeyModifiers::NONE),
            ],
        );
    }

    #[test]
    fn defaults_include_copy_prompt_shortcut() {
        let bindings = Keybindings::from_config(&HashMap::new(), true, false);
        assert_eq!(
            bindings.copy_prompt,
            vec![KeyBinding::new(KeyCode::Char('l'), KeyModifiers::CONTROL),],
        );
    }

    #[test]
    fn parses_function_key_binding() {
        let binding = parse_keybinding("f8").expect("f8 should parse");
        assert_eq!(binding, KeyBinding::new(KeyCode::F(8), KeyModifiers::NONE));
    }
}
