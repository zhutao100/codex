//! Host-owned completion snapshots. These are rollout metadata, never model history.

use std::path::PathBuf;

use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

pub(crate) const AUTOMATIC_REVIEW_LIMIT: u32 = 3;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletedTurnReviewRound {
    pub(crate) user_messages: Vec<String>,
    pub(crate) final_agent_message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReviewRequestFrame {
    pub(crate) prior_user_rounds: Vec<CompletedTurnReviewRound>,
    pub(crate) current_user_messages: Vec<String>,
    pub(crate) text_only: bool,
    pub(crate) exact: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletedTurnForReview {
    pub(crate) episode_id: String,
    pub(crate) request_revision: u64,
    pub(crate) history_generation: String,
    pub(crate) turn_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) request_frame: ReviewRequestFrame,
    pub(crate) initial_answer_reference: Option<String>,
    pub(crate) final_agent_message: String,
    pub(crate) automatic_reviews_started: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReviewTicket {
    pub(crate) episode_id: String,
    pub(crate) request_revision: u64,
    pub(crate) history_generation: String,
    pub(crate) candidate_completion_id: String,
    pub(crate) attempt_id: String,
    pub(crate) automatic: bool,
}

impl ReviewTicket {
    pub(crate) fn matches(&self, episode: &CompletedTurnForReview) -> bool {
        self.episode_id == episode.episode_id
            && self.request_revision == episode.request_revision
            && self.history_generation == episode.history_generation
            && self.candidate_completion_id == episode.turn_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ReviewHandoffDisposition {
    Prepared,
    Started { turn_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReviewHandoff {
    pub(crate) ticket: ReviewTicket,
    pub(crate) advisory: String,
    pub(crate) model: String,
    pub(crate) disposition: ReviewHandoffDisposition,
    pub(crate) advisory_recorded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ReviewInputInstallation {
    Accepted,
    Installing,
    Recorded,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct AcceptedReviewInput {
    pub(crate) revision: u64,
    pub(crate) content: Vec<UserInput>,
    pub(crate) client_id: Option<String>,
    pub(crate) response_item: ResponseItem,
    pub(crate) installation: ReviewInputInstallation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CompletionReviewState {
    pub(crate) version: u32,
    pub(crate) history_generation: String,
    pub(crate) input_revision: u64,
    pub(crate) consumed_input_revision: u64,
    pub(crate) pending_inputs: Vec<AcceptedReviewInput>,
    pub(crate) replacement_answer_required: bool,
    pub(crate) main_task_id: Option<String>,
    pub(crate) request_frame: ReviewRequestFrame,
    pub(crate) episode: Option<CompletedTurnForReview>,
    pub(crate) attempt: Option<ReviewTicket>,
    pub(crate) handoff: Option<ReviewHandoff>,
}

impl Default for CompletionReviewState {
    fn default() -> Self {
        Self {
            version: 2,
            history_generation: Uuid::new_v4().to_string(),
            input_revision: 0,
            consumed_input_revision: 0,
            pending_inputs: Vec::new(),
            replacement_answer_required: false,
            main_task_id: None,
            request_frame: ReviewRequestFrame {
                text_only: true,
                exact: true,
                ..Default::default()
            },
            episode: None,
            attempt: None,
            handoff: None,
        }
    }
}

impl CompletionReviewState {
    pub(crate) fn for_unknown_history() -> Self {
        let mut state = Self::default();
        state.request_frame.exact = false;
        state
    }

    pub(crate) fn eligible_episode(&self) -> Option<&CompletedTurnForReview> {
        self.episode.as_ref().filter(|episode| {
            episode.request_revision == self.input_revision
                && self.consumed_input_revision == self.input_revision
                && self.request_frame.exact
                && self.pending_inputs.is_empty()
                && episode.request_frame.exact
                && !episode.final_agent_message.trim().is_empty()
        })
    }

    pub(crate) fn ticket_is_current(&self, ticket: &ReviewTicket) -> bool {
        self.attempt.as_ref() == Some(ticket)
            && self
                .eligible_episode()
                .is_some_and(|episode| ticket.matches(episode))
    }

    pub(crate) fn accept_input(
        &mut self,
        new_task: bool,
        content: Vec<UserInput>,
        client_id: Option<String>,
    ) -> u64 {
        if new_task {
            self.replacement_answer_required = false;
        }
        if let Some(episode) = self.eligible_episode() {
            let mut prior = episode.request_frame.prior_user_rounds.clone();
            prior.push(CompletedTurnReviewRound {
                user_messages: episode.request_frame.current_user_messages.clone(),
                final_agent_message: episode.final_agent_message.clone(),
            });
            self.request_frame = ReviewRequestFrame {
                prior_user_rounds: prior,
                text_only: episode.request_frame.text_only,
                exact: episode.request_frame.exact,
                ..Default::default()
            };
        }
        self.input_revision += 1;
        self.attempt = None;
        self.handoff = None;
        self.pending_inputs.push(AcceptedReviewInput {
            revision: self.input_revision,
            response_item: ResponseInputItem::from(content.clone()).into(),
            content,
            client_id,
            installation: ReviewInputInstallation::Accepted,
        });
        self.input_revision
    }

    pub(crate) fn consume_input(&mut self, revision: u64) {
        let Some(index) = self.pending_inputs.iter().position(|input| {
            input.revision == revision && input.installation == ReviewInputInstallation::Recorded
        }) else {
            return;
        };
        let input = self.pending_inputs.remove(index);
        for item in input.content {
            let message = match item {
                UserInput::Text { text, .. } => text,
                UserInput::Skill { name, path } => {
                    format!("Selected skill: {name} ({})", path.display())
                }
                UserInput::Mention { name, path } => format!("Selected mention: {name} ({path})"),
                UserInput::Image { .. } | UserInput::LocalImage { .. } => {
                    self.request_frame.text_only = false;
                    "[Nontext user input is not available in this review projection.]".to_string()
                }
                _ => {
                    self.request_frame.text_only = false;
                    "[Unsupported user input.]".to_string()
                }
            };
            self.request_frame.current_user_messages.push(message);
        }
        self.consumed_input_revision = self.consumed_input_revision.max(revision);
    }

    pub(crate) fn observe_input_installation(&mut self, item: &ResponseItem) -> bool {
        let Some(input) = self.pending_inputs.iter_mut().find(|input| {
            input.installation == ReviewInputInstallation::Installing
                && input.response_item == *item
        }) else {
            return false;
        };
        input.installation = ReviewInputInstallation::Recorded;
        true
    }

    pub(crate) fn capture(&mut self, turn_id: String, cwd: PathBuf, text: String) {
        if text.trim().is_empty()
            || self.input_revision != self.consumed_input_revision
            || !self.pending_inputs.is_empty()
            || self.request_frame.current_user_messages.is_empty()
        {
            return;
        }
        if let Some(episode) = self.episode.as_mut()
            && episode.request_revision == self.input_revision
            && episode.history_generation == self.history_generation
        {
            episode.turn_id = turn_id;
            episode.cwd = cwd;
            episode.final_agent_message = text;
        } else {
            self.episode = Some(CompletedTurnForReview {
                episode_id: Uuid::new_v4().to_string(),
                request_revision: self.input_revision,
                history_generation: self.history_generation.clone(),
                turn_id,
                cwd,
                request_frame: self.request_frame.clone(),
                initial_answer_reference: Some(text.clone()),
                final_agent_message: text,
                automatic_reviews_started: 0,
            });
        }
        self.attempt = None;
        self.handoff = None;
        self.replacement_answer_required = false;
        self.main_task_id = None;
    }

    pub(crate) fn begin_review(&mut self, automatic: bool) -> Option<ReviewTicket> {
        let episode = self.eligible_episode()?;
        if automatic && episode.automatic_reviews_started >= AUTOMATIC_REVIEW_LIMIT {
            return None;
        }
        let ticket = ReviewTicket {
            episode_id: episode.episode_id.clone(),
            request_revision: episode.request_revision,
            history_generation: episode.history_generation.clone(),
            candidate_completion_id: episode.turn_id.clone(),
            attempt_id: Uuid::new_v4().to_string(),
            automatic,
        };
        if automatic {
            self.episode.as_mut()?.automatic_reviews_started += 1;
        }
        self.attempt = Some(ticket.clone());
        self.handoff = None;
        Some(ticket)
    }

    pub(crate) fn clear_operations(&mut self) {
        self.attempt = None;
        self.handoff = None;
    }

    pub(crate) fn rebranch(&mut self) {
        self.history_generation = Uuid::new_v4().to_string();
        if let Some(episode) = self.episode.as_mut() {
            episode.history_generation = self.history_generation.clone();
        }
        self.clear_operations();
        self.main_task_id = None;
        self.replacement_answer_required = false;
    }

    pub(crate) fn from_rollout(value: &serde_json::Value) -> Option<Self> {
        serde_json::from_value::<Self>(value.clone())
            .ok()
            .filter(|state| state.version == 2)
    }
}

#[derive(Clone, Debug)]
pub(crate) enum InternalReviewOperation {
    Review(ReviewTicket),
    Continue(ReviewTicket),
}

#[cfg(test)]
#[path = "completion_review_tests.rs"]
mod tests;
