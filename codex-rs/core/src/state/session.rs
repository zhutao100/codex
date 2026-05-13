//! Session-wide mutable state.

use codex_protocol::models::ResponseItem;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;

use crate::context_manager::ContextManager;
use crate::protocol::RateLimitSnapshot;
use crate::protocol::TokenUsage;
use crate::protocol::TokenUsageInfo;
use crate::protocol::TurnContinuationSource;
use crate::protocol::TurnPauseReason;
use crate::session::session::SessionConfiguration;
use crate::truncate::TruncationPolicy;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingContinuation {
    pub(crate) source: TurnContinuationSource,
    pub(crate) continued_from_turn_id: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) pause_reason: Option<TurnPauseReason>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletedTurnForReview {
    pub(crate) turn_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) user_messages: Vec<String>,
    pub(crate) final_agent_message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreviousTurnSettings {
    pub(crate) model: String,
}

/// Persistent, session-scoped state previously stored directly on `Session`.
pub(crate) struct SessionState {
    pub(crate) session_configuration: SessionConfiguration,
    pub(crate) history: ContextManager,
    pub(crate) latest_rate_limits: Option<RateLimitSnapshot>,
    pub(crate) server_reasoning_included: bool,
    pub(crate) dependency_env: HashMap<String, String>,
    pub(crate) mcp_dependency_prompted: HashSet<String>,
    /// Whether the session's initial context has been seeded into history.
    ///
    /// TODO(owen): This is a temporary solution to avoid updating a thread's updated_at
    /// timestamp when resuming a session. Remove this once SQLite is in place.
    pub(crate) initial_context_seeded: bool,
    /// Settings from the latest surviving real user turn.
    pub(crate) previous_turn_settings: Option<PreviousTurnSettings>,
    /// Most recent paused/interrupted turn that can be continued without a new user input.
    pub(crate) pending_continuation: Option<PendingContinuation>,
    /// Most recent completed regular turn that can be reviewed independently.
    pub(crate) last_completed_regular_turn_for_review: Option<CompletedTurnForReview>,
    /// Continuation requested by a post-turn completion review that advised fixes.
    pub(crate) pending_post_turn_completion_review_continuation: Option<PendingContinuation>,
    /// Tracks whether automatic thread naming has already been attempted.
    pub(crate) auto_rename_attempted: bool,
    /// Tracks whether this session originated from a fork.
    pub(crate) is_forked_session: bool,
}

impl SessionState {
    /// Create a new session state mirroring previous `State::default()` semantics.
    pub(crate) fn new(session_configuration: SessionConfiguration) -> Self {
        let history = ContextManager::new();
        Self {
            session_configuration,
            history,
            latest_rate_limits: None,
            server_reasoning_included: false,
            dependency_env: HashMap::new(),
            mcp_dependency_prompted: HashSet::new(),
            initial_context_seeded: false,
            previous_turn_settings: None,
            pending_continuation: None,
            last_completed_regular_turn_for_review: None,
            pending_post_turn_completion_review_continuation: None,
            auto_rename_attempted: false,
            is_forked_session: false,
        }
    }

    // History helpers
    pub(crate) fn record_items<I>(&mut self, items: I, policy: TruncationPolicy)
    where
        I: IntoIterator,
        I::Item: std::ops::Deref<Target = ResponseItem>,
    {
        self.history.record_items(items, policy);
    }

    pub(crate) fn clone_history(&self) -> ContextManager {
        self.history.clone()
    }

    pub(crate) fn replace_history(&mut self, items: Vec<ResponseItem>) {
        self.history.replace(items);
    }

    pub(crate) fn set_token_info(&mut self, info: Option<TokenUsageInfo>) {
        self.history.set_token_info(info);
    }

    // Token/rate limit helpers
    pub(crate) fn update_token_info_from_usage(
        &mut self,
        usage: &TokenUsage,
        model_context_window: Option<i64>,
    ) {
        self.history.update_token_info(usage, model_context_window);
    }

    pub(crate) fn token_info(&self) -> Option<TokenUsageInfo> {
        self.history.token_info()
    }

    pub(crate) fn set_rate_limits(&mut self, snapshot: RateLimitSnapshot) {
        self.latest_rate_limits = Some(merge_rate_limit_fields(
            self.latest_rate_limits.as_ref(),
            snapshot,
        ));
    }

    pub(crate) fn token_info_and_rate_limits(
        &self,
    ) -> (Option<TokenUsageInfo>, Option<RateLimitSnapshot>) {
        (self.token_info(), self.latest_rate_limits.clone())
    }

    pub(crate) fn set_token_usage_full(&mut self, context_window: i64) {
        self.history.set_token_usage_full(context_window);
    }

    pub(crate) fn get_total_token_usage(&self, server_reasoning_included: bool) -> i64 {
        self.history
            .get_total_token_usage(server_reasoning_included)
    }

    pub(crate) fn set_server_reasoning_included(&mut self, included: bool) {
        self.server_reasoning_included = included;
    }

    pub(crate) fn server_reasoning_included(&self) -> bool {
        self.server_reasoning_included
    }

    pub(crate) fn record_mcp_dependency_prompted<I>(&mut self, names: I)
    where
        I: IntoIterator<Item = String>,
    {
        self.mcp_dependency_prompted.extend(names);
    }

    pub(crate) fn mcp_dependency_prompted(&self) -> HashSet<String> {
        self.mcp_dependency_prompted.clone()
    }

    pub(crate) fn set_dependency_env(&mut self, values: HashMap<String, String>) {
        for (key, value) in values {
            self.dependency_env.insert(key, value);
        }
    }

    pub(crate) fn dependency_env(&self) -> HashMap<String, String> {
        self.dependency_env.clone()
    }

    pub(crate) fn auto_rename_attempted(&self) -> bool {
        self.auto_rename_attempted
    }

    pub(crate) fn mark_auto_rename_attempted(&mut self) {
        self.auto_rename_attempted = true;
    }

    pub(crate) fn is_forked_session(&self) -> bool {
        self.is_forked_session
    }

    pub(crate) fn set_is_forked_session(&mut self, is_forked_session: bool) {
        self.is_forked_session = is_forked_session;
    }
}

// Sometimes new snapshots don't include credits or plan information.
fn merge_rate_limit_fields(
    previous: Option<&RateLimitSnapshot>,
    mut snapshot: RateLimitSnapshot,
) -> RateLimitSnapshot {
    if snapshot.credits.is_none() {
        snapshot.credits = previous.and_then(|prior| prior.credits.clone());
    }
    if snapshot.plan_type.is_none() {
        snapshot.plan_type = previous.and_then(|prior| prior.plan_type);
    }
    snapshot
}
