//! Connector loading and connector selection UI for ChatWidget.

use super::*;

#[derive(Debug, Clone, Default)]
pub(super) enum ConnectorsCacheState {
    #[default]
    Uninitialized,
    Loading,
    Ready(ConnectorsSnapshot),
    Failed(String),
}

impl ChatWidget {
    pub(super) fn prefetch_connectors(&mut self) {
        if !self.connectors_enabled() {
            return;
        }
        if matches!(self.connectors_cache, ConnectorsCacheState::Loading) {
            return;
        }

        self.connectors_cache = ConnectorsCacheState::Loading;
        let config = self.config.clone();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result: Result<ConnectorsSnapshot, anyhow::Error> = async {
                let connectors = chatgpt_connectors::list_connectors(&config).await?;
                Ok(ConnectorsSnapshot { connectors })
            }
            .await;
            let result = result.map_err(|err| format!("Failed to load apps: {err}"));
            app_event_tx.send(AppEvent::ConnectorsLoaded(result));
        });
    }

    pub(super) fn connectors_enabled(&self) -> bool {
        self.config.features.enabled(Feature::Apps)
    }

    pub(super) fn connectors_for_mentions(&self) -> Option<&[chatgpt_connectors::AppInfo]> {
        if !self.connectors_enabled() {
            return None;
        }

        match &self.connectors_cache {
            ConnectorsCacheState::Ready(snapshot) => Some(snapshot.connectors.as_slice()),
            _ => None,
        }
    }

    pub(crate) fn add_connectors_output(&mut self) {
        if !self.connectors_enabled() {
            self.add_info_message(
                "Apps are disabled.".to_string(),
                Some("Enable the apps feature to use $ or /apps.".to_string()),
            );
            return;
        }

        match self.connectors_cache.clone() {
            ConnectorsCacheState::Ready(snapshot) => {
                if snapshot.connectors.is_empty() {
                    self.add_info_message("No apps available.".to_string(), None);
                } else {
                    self.open_connectors_popup(&snapshot.connectors);
                }
            }
            ConnectorsCacheState::Failed(err) => {
                self.add_to_history(history_cell::new_error_event(err));
                // Retry on demand so `/apps` can recover after transient failures.
                self.prefetch_connectors();
            }
            ConnectorsCacheState::Loading => {
                self.add_to_history(history_cell::new_info_event(
                    "Apps are still loading.".to_string(),
                    Some("Try again in a moment.".to_string()),
                ));
            }
            ConnectorsCacheState::Uninitialized => {
                self.prefetch_connectors();
                self.add_to_history(history_cell::new_info_event(
                    "Apps are still loading.".to_string(),
                    Some("Try again in a moment.".to_string()),
                ));
            }
        }
        self.request_redraw();
    }

    pub(super) fn open_connectors_popup(&mut self, connectors: &[chatgpt_connectors::AppInfo]) {
        let total = connectors.len();
        let installed = connectors
            .iter()
            .filter(|connector| connector.is_accessible)
            .count();
        let mut header = ColumnRenderable::new();
        header.push(Line::from("Apps".bold()));
        header.push(Line::from(
            "Use $ to insert an installed app into your prompt.".dim(),
        ));
        header.push(Line::from(
            format!("Installed {installed} of {total} available apps.").dim(),
        ));
        let mut items: Vec<SelectionItem> = Vec::with_capacity(connectors.len());
        for connector in connectors {
            let connector_label = chatgpt_connectors::connector_display_label(connector);
            let connector_title = connector_label.clone();
            let link_description = Self::connector_description(connector);
            let description = Self::connector_brief_description(connector);
            let search_value = format!("{connector_label} {}", connector.id);
            let mut item = SelectionItem {
                name: connector_label,
                description: Some(description),
                search_value: Some(search_value),
                ..Default::default()
            };
            let is_installed = connector.is_accessible;
            let (selected_label, missing_label, instructions) = if connector.is_accessible {
                (
                    "Press Enter to view the app link.",
                    "App link unavailable.",
                    "Manage this app in your browser.",
                )
            } else {
                (
                    "Press Enter to view the install link.",
                    "Install link unavailable.",
                    "Install this app in your browser, then reload Codex.",
                )
            };
            if let Some(install_url) = connector.install_url.clone() {
                let title = connector_title.clone();
                let instructions = instructions.to_string();
                let description = link_description.clone();
                item.actions = vec![Box::new(move |tx| {
                    tx.send(AppEvent::OpenAppLink {
                        title: title.clone(),
                        description: description.clone(),
                        instructions: instructions.clone(),
                        url: install_url.clone(),
                        is_installed,
                    });
                })];
                item.dismiss_on_select = true;
                item.selected_description = Some(selected_label.to_string());
            } else {
                item.actions = vec![Box::new(move |tx| {
                    tx.send(AppEvent::InsertHistoryCell(Box::new(
                        history_cell::new_info_event(missing_label.to_string(), None),
                    )));
                })];
                item.dismiss_on_select = true;
                item.selected_description = Some(missing_label.to_string());
            }
            items.push(item);
        }

        self.bottom_pane.show_selection_view(SelectionViewParams {
            header: Box::new(header),
            footer_hint: Some(Self::connectors_popup_hint_line()),
            items,
            is_searchable: true,
            search_placeholder: Some("Type to search apps".to_string()),
            col_width_mode: ColumnWidthMode::AutoAllRows,
            ..Default::default()
        });
    }

    pub(super) fn connector_brief_description(connector: &chatgpt_connectors::AppInfo) -> String {
        let status_label = if connector.is_accessible {
            "Connected"
        } else {
            "Can be installed"
        };
        match Self::connector_description(connector) {
            Some(description) => format!("{status_label} · {description}"),
            None => status_label.to_string(),
        }
    }

    pub(super) fn connector_description(connector: &chatgpt_connectors::AppInfo) -> Option<String> {
        connector
            .description
            .as_deref()
            .map(str::trim)
            .filter(|description| !description.is_empty())
            .map(str::to_string)
    }

    pub(crate) fn on_connectors_loaded(&mut self, result: Result<ConnectorsSnapshot, String>) {
        self.connectors_cache = match result {
            Ok(connectors) => ConnectorsCacheState::Ready(connectors),
            Err(err) => ConnectorsCacheState::Failed(err),
        };
        if let ConnectorsCacheState::Ready(snapshot) = &self.connectors_cache {
            self.bottom_pane
                .set_connectors_snapshot(Some(snapshot.clone()));
        } else {
            self.bottom_pane.set_connectors_snapshot(None);
        }
    }
}
