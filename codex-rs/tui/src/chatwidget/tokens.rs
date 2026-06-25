//! Coordinates asynchronous `/usage` cards in the chat widget.

mod chart;

use std::sync::Arc;
use std::sync::RwLock;

use chrono::NaiveDate;
use chrono::Utc;
use codex_backend_client::TokenUsageProfile;
use ratatui::style::Stylize;
use ratatui::text::Line;

use super::*;
use crate::history_cell::CompositeHistoryCell;

const TOKEN_ACTIVITY_FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const USAGE_CHATGPT_LOGIN_REQUIRED: &str = "Sign in with ChatGPT to use /usage.";

pub(crate) use chart::TokenActivityView;

#[derive(Debug)]
enum TokenActivityState {
    Loading,
    Loaded {
        profile: TokenUsageProfile,
        today: NaiveDate,
    },
    Error,
}

#[derive(Clone, Debug)]
pub(super) struct TokenActivityHandle {
    state: Arc<RwLock<TokenActivityState>>,
}

pub(super) struct PendingTokenActivityOutput {
    request_id: u64,
    cell: CompositeHistoryCell,
    handle: TokenActivityHandle,
}

impl TokenActivityHandle {
    pub(super) fn finish(&self, result: Result<TokenUsageProfile, String>) {
        self.finish_with_today(result, Utc::now().date_naive());
    }

    fn finish_with_today(&self, result: Result<TokenUsageProfile, String>, today: NaiveDate) {
        let state = match result {
            Ok(profile) => TokenActivityState::Loaded { profile, today },
            Err(_) => TokenActivityState::Error,
        };
        #[expect(clippy::expect_used)]
        let mut current = self.state.write().expect("token activity state poisoned");
        *current = state;
    }
}

#[derive(Debug)]
struct TokenActivityHistoryCell {
    view: TokenActivityView,
    state: Arc<RwLock<TokenActivityState>>,
}

pub(super) fn new_token_activity_output(
    view: TokenActivityView,
) -> (CompositeHistoryCell, TokenActivityHandle) {
    let command = PlainHistoryCell::new(vec![
        format!("/usage {}", view.label().to_lowercase())
            .magenta()
            .into(),
    ]);
    let state = Arc::new(RwLock::new(TokenActivityState::Loading));
    let handle = TokenActivityHandle {
        state: Arc::clone(&state),
    };
    let card = TokenActivityHistoryCell { view, state };
    (
        CompositeHistoryCell::new(vec![Box::new(command), Box::new(card)]),
        handle,
    )
}

impl HistoryCell for TokenActivityHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        #[expect(clippy::expect_used)]
        let state = self.state.read().expect("token activity state poisoned");
        match &*state {
            TokenActivityState::Loading => {
                vec![
                    " Token activity".bold().into(),
                    "   Loading...".dim().into(),
                ]
            }
            TokenActivityState::Error => vec![
                " Token activity".bold().into(),
                "   Token activity unavailable".dim().into(),
            ],
            TokenActivityState::Loaded { profile, today } => {
                chart::loaded_lines(self.view, profile, *today, width)
            }
        }
    }
}

pub(super) async fn fetch_token_usage_profile(
    base_url: String,
    auth: CodexAuth,
) -> Result<TokenUsageProfile, String> {
    if !auth.is_chatgpt_auth() {
        return Err("chatgpt authentication required to read token usage".to_string());
    }
    let client = BackendClient::from_auth(base_url, &auth)
        .map_err(|err| format!("failed to construct backend client: {err}"))?;
    client
        .get_token_usage_profile()
        .await
        .map_err(|err| format!("failed to fetch token usage profile: {err}"))
}

impl ChatWidget {
    pub(super) fn ensure_usage_command_available(&mut self) -> bool {
        if self
            .auth_manager
            .auth_cached()
            .as_ref()
            .is_some_and(CodexAuth::is_chatgpt_auth)
        {
            return true;
        }
        self.add_error_message(USAGE_CHATGPT_LOGIN_REQUIRED.to_string());
        false
    }

    pub(crate) fn add_token_activity_output(&mut self, view: TokenActivityView) {
        let request_id = self.next_token_activity_request_id;
        self.next_token_activity_request_id = self.next_token_activity_request_id.wrapping_add(1);
        let (cell, handle) = new_token_activity_output(view);
        self.completed_token_activity_output = None;
        self.refreshing_token_activity_output = Some(PendingTokenActivityOutput {
            request_id,
            cell,
            handle,
        });
        self.bump_active_cell_revision();
        self.request_redraw();

        let base_url = self.config.chatgpt_base_url.clone();
        let app_event_tx = self.app_event_tx.clone();
        let auth_manager = Arc::clone(&self.auth_manager);
        tokio::spawn(async move {
            let result = tokio::time::timeout(TOKEN_ACTIVITY_FETCH_TIMEOUT, async move {
                let Some(auth) = auth_manager.auth().await else {
                    return Err(
                        "codex account authentication required to read token usage".to_string()
                    );
                };
                fetch_token_usage_profile(base_url, auth).await
            })
            .await
            .map_err(|_| "token usage profile fetch timed out".to_string())
            .and_then(std::convert::identity);
            app_event_tx.send(AppEvent::TokenActivityLoaded { request_id, result });
        });
    }

    pub(super) fn pending_token_activity_output(&self) -> Option<&dyn HistoryCell> {
        self.refreshing_token_activity_output
            .as_ref()
            .map(|output| &output.cell as &dyn HistoryCell)
            .or_else(|| {
                self.completed_token_activity_output
                    .as_ref()
                    .map(|cell| cell as &dyn HistoryCell)
            })
    }

    pub(crate) fn finish_token_activity_refresh(
        &mut self,
        request_id: u64,
        result: Result<TokenUsageProfile, String>,
    ) -> bool {
        let Some(output) = self.refreshing_token_activity_output.take() else {
            return false;
        };
        if output.request_id != request_id {
            self.refreshing_token_activity_output = Some(output);
            return false;
        }
        output.handle.finish(result);
        self.completed_token_activity_output = Some(output.cell);
        self.bump_active_cell_revision();
        self.request_redraw();
        true
    }

    pub(crate) fn usage_history_insertion_blocked(&self) -> bool {
        self.bottom_pane.is_task_running()
            || self.active_cell.is_some()
            || self.stream_controller.is_some()
            || self.plan_stream_controller.is_some()
    }

    pub(crate) fn take_completed_token_activity_output(&mut self) -> Option<CompositeHistoryCell> {
        let output = self.completed_token_activity_output.take()?;
        self.bump_active_cell_revision();
        Some(output)
    }

    pub(crate) fn request_pending_usage_output_insertion(&self) {
        if self.completed_token_activity_output.is_some() {
            self.app_event_tx.send(AppEvent::CommitPendingUsageOutput);
        }
    }

    pub(crate) fn clear_pending_token_activity_refreshes(&mut self) {
        let cleared_refresh = self.refreshing_token_activity_output.take().is_some();
        let cleared_completed = self.completed_token_activity_output.take().is_some();
        if cleared_refresh || cleared_completed {
            self.bump_active_cell_revision();
            self.request_redraw();
        }
    }
}

#[cfg(test)]
#[path = "tokens_tests.rs"]
mod tests;
