use super::ChatWidget;
use codex_core::plugins::list_plugins;
use codex_core::plugins::plugins_from_stack;

impl ChatWidget {
    pub(super) fn add_plugins_output(&mut self) {
        let configured = match plugins_from_stack(&self.config.config_layer_stack) {
            Ok(configured) => configured,
            Err(err) => {
                self.add_error_message(err.to_string());
                return;
            }
        };
        let plugins = list_plugins(&self.config.codex_home, &configured);
        if plugins.is_empty() {
            self.add_info_message("No plugins installed.".into(), None);
            return;
        }
        let mut lines = vec!["Installed plugins (session configuration):".to_string()];
        for plugin in plugins {
            let state = if plugin.enabled {
                "enabled"
            } else {
                "disabled"
            };
            lines.push(format!("{} ({state}) - {}", plugin.id, plugin.display_name));
            if let Some(error) = plugin.error {
                lines.push(format!("  {error}"));
            }
        }
        self.add_info_message(lines.join("\n"), None);
    }
}
