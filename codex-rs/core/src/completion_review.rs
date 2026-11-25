//! Host-owned completion snapshots and their request-only prompt frame, never transcript items.

use std::path::PathBuf;

use crate::context_manager::estimate_item_token_count;
use codex_protocol::models::DeveloperInstructions;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

/// Host-only selections for one consumed user scope, up to the next history replacement.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ReviewPromptFrames(pub(crate) Vec<ReviewPromptFrame>);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReviewPromptFrame {
    item: ResponseItem,
    pub(crate) tokens: i64,
    after_boundaries: usize,
    pub(crate) attempt: Option<String>,
}

impl ReviewPromptFrame {
    pub(crate) fn anchor(work: &[ResponseItem]) -> usize {
        work.iter().filter(|item| is_frame_boundary(item)).count()
    }

    pub(crate) fn new(
        instructions: String,
        after_boundaries: usize,
        attempt: Option<String>,
    ) -> Self {
        let item = DeveloperInstructions::new(instructions).into();
        Self {
            tokens: estimate_item_token_count(&item),
            item,
            after_boundaries,
            attempt,
        }
    }

    fn matches_message(&self, item: &ResponseItem) -> bool {
        // A unary compact response may echo the message with a server-assigned id/status.
        matches!((&self.item, item),
            (ResponseItem::Message { content: expected, .. }, ResponseItem::Message { role, content, .. })
                if role == "developer" && content == expected)
    }
}

fn is_frame_boundary(item: &ResponseItem) -> bool {
    matches!(item, ResponseItem::Message { role, .. } if matches!(role.as_str(), "system" | "developer" | "user"))
        || matches!(item, ResponseItem::Compaction { .. })
}

impl ReviewPromptFrames {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn tokens(&self) -> i64 {
        self.0.iter().map(|frame| frame.tokens).sum()
    }

    pub(crate) fn insert(
        &self,
        input: &mut Vec<ResponseItem>,
        work_end: Option<usize>,
    ) -> crate::error::Result<()> {
        if self.is_empty() {
            return Ok(());
        }
        // Raw history may also contain ghost snapshots or unpaired tool items that normalization
        // removes. Boundary ordinals, unlike raw/projected offsets, agree in both representations.
        let anchors: Vec<_> = std::iter::once(0)
            .chain(
                input[..work_end.unwrap_or(input.len())]
                    .iter()
                    .enumerate()
                    .filter_map(|(i, item)| is_frame_boundary(item).then_some(i + 1)),
            )
            .collect();
        for frame in self.0.iter().rev() {
            let index = anchors.get(frame.after_boundaries).ok_or_else(|| crate::error::CodexErr::Fatal(
                "The sampled review frame's history boundary is unavailable. No incomplete request was sent.".into()
            ))?;
            input.insert(*index, frame.item.clone());
        }
        Ok(())
    }

    pub(crate) fn matches_message(&self, item: &ResponseItem) -> bool {
        self.0.iter().any(|frame| frame.matches_message(item))
    }

    pub(crate) fn retained_tokens(&self, input: &[ResponseItem]) -> i64 {
        input
            .iter()
            .filter_map(|item| self.0.iter().find(|frame| frame.matches_message(item)))
            .map(|frame| frame.tokens)
            .sum()
    }
}

pub(crate) enum ReviewSampling {
    Ready(ReviewPromptFrames),
    Compact,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum CompletionReviewReservationError {
    #[error("Cannot review a completed turn while another task is running.")]
    TaskRunning,
    #[error(
        "No completed Codex turn with exact consumed-request provenance is available to review. This session has missing or invalidated request context. Reloading or completing another prompt cannot restore that context. Start a new session with the complete request, or recover the missing context from verified host-owned rollout metadata."
    )]
    InexactRequest,
    #[error(
        "No completed Codex turn with exact consumed-request provenance is available to review yet. Pending user input must be consumed by completed main work. Continue the task and wait for completion before reviewing."
    )]
    PendingInput,
    #[error(
        "No completed Codex turn is available to review yet. Run a normal prompt and wait for completion before reviewing."
    )]
    NoCompletedTurn,
    #[error(
        "Completion review state could not be persisted. Repair storage and reload the last durable rollout before retrying."
    )]
    Persistence,
}

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
    Deferred,
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
    pub(crate) recovery_model: Option<String>,
    pub(crate) request_frame: ReviewRequestFrame,
    pub(crate) episode: Option<CompletedTurnForReview>,
    pub(crate) attempt: Option<ReviewTicket>,
    pub(crate) handoff: Option<ReviewHandoff>,
    #[serde(default, skip_serializing_if = "ReviewPromptFrames::is_empty")]
    pub(crate) prompt_frames: ReviewPromptFrames,
    /// A checkpoint must finish with the outgoing frames, even if resumed with more headroom.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) defer_new_prompt_frame: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) deferred_capture_model: Option<String>,
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
            recovery_model: None,
            request_frame: ReviewRequestFrame {
                text_only: true,
                exact: true,
                ..Default::default()
            },
            episode: None,
            attempt: None,
            handoff: None,
            prompt_frames: ReviewPromptFrames::default(),
            defer_new_prompt_frame: false,
            deferred_capture_model: None,
        }
    }
}

impl CompletionReviewState {
    pub(crate) fn clear_prompt_frames(&mut self) {
        self.prompt_frames = ReviewPromptFrames::default();
        self.defer_new_prompt_frame = false;
        self.deferred_capture_model = None;
    }

    pub(crate) fn invalidate_request_provenance(&mut self) {
        self.request_frame.exact = false;
        self.clear_prompt_frames();
    }

    pub(crate) fn for_unknown_history() -> Self {
        let mut state = Self::default();
        state.invalidate_request_provenance();
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

    pub(crate) fn unstarted_handoff(&self) -> Option<&ReviewHandoff> {
        self.handoff.as_ref().filter(|handoff| {
            matches!(
                handoff.disposition,
                ReviewHandoffDisposition::Prepared | ReviewHandoffDisposition::Deferred
            ) && self.ticket_is_current(&handoff.ticket)
        })
    }

    pub(crate) fn defer_handoff(&mut self) -> bool {
        if self.unstarted_handoff().is_none() {
            return false;
        }
        if let Some(handoff) = self.handoff.as_mut() {
            handoff.disposition = ReviewHandoffDisposition::Deferred;
        }
        true
    }

    pub(crate) fn accept_input(
        &mut self,
        new_task: bool,
        content: Vec<UserInput>,
        client_id: Option<String>,
    ) -> u64 {
        if new_task {
            self.replacement_answer_required = false;
            self.recovery_model = None;
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
        self.clear_prompt_frames();
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
            });
        }
        self.attempt = None;
        self.handoff = None;
        self.replacement_answer_required = false;
        self.main_task_id = None;
        self.recovery_model = None;
    }

    pub(crate) fn begin_review(&mut self, automatic: bool) -> Option<ReviewTicket> {
        let episode = self.eligible_episode()?;
        let ticket = ReviewTicket {
            episode_id: episode.episode_id.clone(),
            request_revision: episode.request_revision,
            history_generation: episode.history_generation.clone(),
            candidate_completion_id: episode.turn_id.clone(),
            attempt_id: Uuid::new_v4().to_string(),
            automatic,
        };
        self.attempt = Some(ticket.clone());
        self.handoff = None;
        Some(ticket)
    }

    pub(crate) fn clear_operations(&mut self) {
        self.attempt = None;
        self.handoff = None;
    }

    pub(crate) fn rebranch(&mut self) {
        self.clear_prompt_frames();
        self.history_generation = Uuid::new_v4().to_string();
        if let Some(episode) = self.episode.as_mut() {
            episode.history_generation = self.history_generation.clone();
        }
        self.clear_operations();
        self.main_task_id = None;
        self.replacement_answer_required = false;
        self.recovery_model = None;
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
