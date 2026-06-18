use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use crate::AuthManager;
use crate::CodexAuth;
use crate::SandboxState;
use crate::agent::AgentControl;
use crate::agent::AgentStatus;
use crate::agent::MAX_THREAD_SPAWN_DEPTH;
use crate::agent::agent_status_from_event;
use crate::analytics_client::AnalyticsEventsClient;
use crate::analytics_client::build_track_events_context;
use crate::compact;
use crate::compact::AUTO_COMPACT_WORK_NOTES_REQUEST_TAG;
use crate::compact::AUTO_COMPACT_WORK_NOTES_TAG;
use crate::compact::run_inline_auto_compact_task;
use crate::compact::should_use_remote_compact_task;
use crate::compact_remote::run_inline_remote_auto_compact_task;
use crate::connectors;
use crate::exec_policy::ExecPolicyManager;
use crate::features::FEATURES;
use crate::features::Feature;
use crate::features::Features;
use crate::features::maybe_push_unstable_features_warning;
use crate::hooks::HookEvent;
use crate::hooks::HookEventAfterAgent;
use crate::hooks::Hooks;
use crate::models_manager::manager::ModelsManager;
use crate::parse_command::parse_command;
use crate::parse_turn_item;
use crate::rollout::session_index;
use crate::state::TurnInput;
use crate::stream_events_utils::HandleOutputCtx;
use crate::stream_events_utils::handle_non_tool_response_item;
use crate::stream_events_utils::handle_output_item_done;
use crate::stream_events_utils::last_assistant_message_from_item;
use crate::terminal;
use crate::truncate::TruncationPolicy;
use crate::turn_metadata::build_turn_metadata_header;
use crate::util::error_or_panic;
use async_channel::Receiver;
use async_channel::Sender;
use codex_protocol::ThreadId;
use codex_protocol::approvals::ExecPolicyAmendment;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::items::PlanItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::format_allow_prefixes;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::HasLegacyEvent;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::ProgressTraceCategory;
use codex_protocol::protocol::ProgressTraceEvent;
use codex_protocol::protocol::ProgressTraceState;
use codex_protocol::protocol::RawResponseItemEvent;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_rmcp_client::ElicitationResponse;
use codex_rmcp_client::OAuthCredentialsStoreMode;
use futures::future::BoxFuture;
use futures::prelude::*;
use futures::stream::FuturesOrdered;
use rmcp::model::ListResourceTemplatesResult;
use rmcp::model::ListResourcesResult;
use rmcp::model::PaginatedRequestParam;
use rmcp::model::ReadResourceRequestParam;
use rmcp::model::ReadResourceResult;
use rmcp::model::RequestId;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::OnceCell;
use tokio::sync::RwLock;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use tracing::debug;
use tracing::error;
use tracing::field;
use tracing::info;
use tracing::info_span;
use tracing::instrument;
use tracing::trace;
use tracing::trace_span;
use tracing::warn;

use crate::ModelProviderInfo;
use crate::client::ModelClient;
use crate::client::ModelClientSession;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::compact::InitialContextInjection;
use crate::compact::collect_user_messages;
use crate::config::Config;
use crate::config::Constrained;
use crate::config::ConstraintResult;
use crate::config::GhostSnapshotConfig;
use crate::config::resolve_web_search_mode_for_turn;
use crate::config::types::McpServerConfig;
use crate::config::types::ShellEnvironmentPolicy;
use crate::context_manager::ContextManager;
use crate::environment_context::EnvironmentContext;
use crate::error::CodexErr;
use crate::error::Result as CodexResult;
#[cfg(test)]
use crate::exec::StreamOutput;
use crate::exec_policy::ExecPolicyUpdateError;
use crate::feedback_tags;
use crate::file_watcher::FileWatcher;
use crate::file_watcher::FileWatcherEvent;
use crate::git_info::get_git_repo_root;
use crate::instructions::UserInstructions;
use crate::mcp::CODEX_APPS_MCP_SERVER_NAME;
use crate::mcp::auth::compute_auth_statuses;
use crate::mcp::effective_mcp_servers;
use crate::mcp::maybe_prompt_and_install_mcp_dependencies;
use crate::mcp::with_codex_apps_mcp;
use crate::mcp_connection_manager::McpConnectionManager;
use crate::mentions::build_connector_slug_counts;
use crate::mentions::build_skill_name_counts;
use crate::mentions::collect_explicit_app_paths;
use crate::mentions::collect_tool_mentions_from_messages;
use crate::project_doc::get_user_instructions;
use crate::proposed_plan_parser::ProposedPlanParser;
use crate::proposed_plan_parser::ProposedPlanSegment;
use crate::proposed_plan_parser::extract_proposed_plan_text;
use crate::protocol::AgentMessageContentDeltaEvent;
use crate::protocol::AgentReasoningSectionBreakEvent;
use crate::protocol::ApplyPatchApprovalRequestEvent;
use crate::protocol::AskForApproval;
use crate::protocol::BackgroundEventEvent;
use crate::protocol::DeprecationNoticeEvent;
use crate::protocol::ErrorEvent;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::ExecApprovalRequestEvent;
use crate::protocol::McpServerRefreshConfig;
use crate::protocol::ModelRerouteEvent;
use crate::protocol::ModelRerouteReason;
use crate::protocol::ModelVerification;
use crate::protocol::ModelVerificationEvent;
use crate::protocol::Op;
use crate::protocol::PlanDeltaEvent;
use crate::protocol::RateLimitSnapshot;
use crate::protocol::ReasoningContentDeltaEvent;
use crate::protocol::ReasoningRawContentDeltaEvent;
use crate::protocol::RequestUserInputEvent;
use crate::protocol::ReviewDecision;
use crate::protocol::SandboxPolicy;
use crate::protocol::SessionConfiguredEvent;
use crate::protocol::SkillDependencies as ProtocolSkillDependencies;
use crate::protocol::SkillErrorInfo;
use crate::protocol::SkillInterface as ProtocolSkillInterface;
use crate::protocol::SkillMetadata as ProtocolSkillMetadata;
use crate::protocol::SkillToolDependency as ProtocolSkillToolDependency;
use crate::protocol::StreamErrorEvent;
use crate::protocol::Submission;
use crate::protocol::ThreadRolledBackEvent;
use crate::protocol::TokenCountEvent;
use crate::protocol::TokenUsage;
use crate::protocol::TokenUsageInfo;
use crate::protocol::TurnContinuationSource;
use crate::protocol::TurnContinuedEvent;
use crate::protocol::TurnDiffEvent;
use crate::protocol::TurnPauseReason;
use crate::protocol::WarningEvent;
use crate::rollout::RolloutRecorder;
use crate::rollout::RolloutRecorderParams;
use crate::rollout::map_session_init_error;
use crate::rollout::metadata;
use crate::session_prefix::TURN_ABORTED_OPEN_TAG;
use crate::shell;
use crate::shell_snapshot::ShellSnapshot;
use crate::skills::SkillError;
use crate::skills::SkillInjections;
use crate::skills::SkillMetadata;
use crate::skills::SkillsManager;
use crate::skills::build_skill_injections;
use crate::skills::collect_env_var_dependencies;
use crate::skills::collect_explicit_skill_mentions;
use crate::skills::injection::ToolMentionKind;
use crate::skills::injection::app_id_from_path;
use crate::skills::injection::tool_kind_for_path;
use crate::skills::resolve_skill_dependencies_for_turn;
use crate::state::ActiveTurn;
use crate::state::CompletedTurnForReview;
use crate::state::CompletedTurnReviewRound;
use crate::state::PendingContinuation;
use crate::state::PendingContinuationTarget;
use crate::state::PreviousTurnSettings;
use crate::state::SessionServices;
use crate::state::SessionState;
use crate::state::TaskKind;
use crate::state_db;
use crate::tasks::GhostSnapshotTask;
use crate::tasks::PostTurnCompletionReviewTask;
use crate::tasks::ReviewDelegateConfigParams;
use crate::tasks::ReviewDelegateInstructionProfile;
use crate::tasks::ReviewTask;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;
use crate::tasks::configure_review_delegate_config;
use crate::tools::ToolRouter;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::parallel::ToolCallExecutionMode;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::sandboxing::ApprovalStore;
use crate::tools::spec::ToolsConfig;
use crate::tools::spec::ToolsConfigParams;
use crate::turn_diff_tracker::TurnDiffTracker;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::util::backoff;
use crate::windows_sandbox::WindowsSandboxLevelExt;
use codex_async_utils::OrCancelExt;
use codex_otel::OtelManager;
use codex_otel::TelemetryAuthMode;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::ContentItem;
use codex_protocol::models::DeveloperInstructions;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::NonSteerableTurnKind;
use codex_protocol::user_input::UserInput;
use codex_utils_readiness::Readiness;
use codex_utils_readiness::ReadinessFlag;
use tokio::sync::watch;

mod handlers;
mod mcp;
mod read_only_temp;
mod review;
mod rollout_reconstruction;
#[allow(clippy::module_inception)]
pub(crate) mod session;
pub(crate) mod turn;
pub(crate) mod turn_context;

use self::handlers::submission_loop;
use self::rollout_reconstruction::ReconstructedRollout;
use self::session::Session;
use self::session::SessionConfiguration;
pub(crate) use self::session::SessionSettingsUpdate;
use self::turn::completed_turn_for_review_from_history;
use self::turn::completed_turn_interaction_history_from_history;
#[cfg(test)]
use self::turn::filter_connectors_for_input;
use self::turn::history_needs_continuation;
use self::turn::is_user_turn_boundary_response_item;
use self::turn::remove_trailing_turn_aborted_marker;
use self::turn::trim_incomplete_continuation_tail;
use self::turn_context::TurnContext;

#[derive(Debug, PartialEq)]
pub enum SteerInputError {
    NoActiveTurn(Vec<UserInput>),
    ExpectedTurnMismatch { expected: String, actual: String },
    ActiveTurnNotSteerable { turn_kind: NonSteerableTurnKind },
    EmptyInput,
}

impl SteerInputError {
    fn to_error_event(&self) -> ErrorEvent {
        match self {
            Self::NoActiveTurn(_) => ErrorEvent {
                message: "no active turn to steer".to_string(),
                codex_error_info: Some(CodexErrorInfo::NoActiveTurnToSteer),
            },
            Self::ExpectedTurnMismatch { expected, actual } => ErrorEvent {
                message: format!("expected active turn id `{expected}` but found `{actual}`"),
                codex_error_info: Some(CodexErrorInfo::ExpectedTurnMismatch {
                    expected: expected.clone(),
                    actual: actual.clone(),
                }),
            },
            Self::ActiveTurnNotSteerable { turn_kind } => {
                let turn_kind_label = match turn_kind {
                    NonSteerableTurnKind::Review => "review",
                    NonSteerableTurnKind::Compact => "compact",
                    NonSteerableTurnKind::UserShell => "user-shell",
                };
                ErrorEvent {
                    message: format!("cannot steer a {turn_kind_label} turn"),
                    codex_error_info: Some(CodexErrorInfo::ActiveTurnNotSteerable {
                        turn_kind: *turn_kind,
                    }),
                }
            }
            Self::EmptyInput => ErrorEvent {
                message: "input must not be empty".to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            },
        }
    }
}

const CYBER_VERIFY_URL: &str = "https://chatgpt.com/cyber";
const CYBER_SAFETY_URL: &str = "https://developers.openai.com/codex/concepts/cyber-safety";

fn server_model_mismatch_warning(requested_model: &str, server_model: &str) -> String {
    format!(
        "The server used a different model than requested (requested: {requested_model}; used: {server_model}). This can happen for several reasons, including automated routing for potentially high-risk cybersecurity activity. The turn has been paused; use `/continue` to resume with {requested_model}. For trusted security work, apply for access: {CYBER_VERIFY_URL}. Learn more: {CYBER_SAFETY_URL}"
    )
}

/// The high-level interface to the Codex system.
/// It operates as a queue pair where you send submissions and receive events.
pub struct Codex {
    pub(crate) next_id: AtomicU64,
    pub(crate) tx_sub: Sender<Submission>,
    pub(crate) rx_event: Receiver<Event>,
    // Last known status of the agent.
    pub(crate) agent_status: watch::Receiver<AgentStatus>,
    pub(crate) session: Arc<Session>,
}

/// Wrapper returned by [`Codex::spawn`] containing the spawned [`Codex`],
/// the submission id for the initial `ConfigureSession` request and the
/// unique session id.
pub struct CodexSpawnOk {
    pub codex: Codex,
    pub thread_id: ThreadId,
    #[deprecated(note = "use thread_id")]
    pub conversation_id: ThreadId,
}

pub(crate) const INITIAL_SUBMIT_ID: &str = "";
pub(crate) const SUBMISSION_CHANNEL_CAPACITY: usize = 64;

impl Codex {
    /// Spawn a new [`Codex`] and initialize the session.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn spawn(
        mut config: Config,
        auth_manager: Arc<AuthManager>,
        models_manager: Arc<ModelsManager>,
        skills_manager: Arc<SkillsManager>,
        file_watcher: Arc<FileWatcher>,
        conversation_history: InitialHistory,
        session_source: SessionSource,
        agent_control: AgentControl,
        dynamic_tools: Vec<DynamicToolSpec>,
    ) -> CodexResult<CodexSpawnOk> {
        let (tx_sub, rx_sub) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);
        let (tx_event, rx_event) = async_channel::unbounded();

        let loaded_skills = skills_manager.skills_for_config(&config);

        for err in &loaded_skills.errors {
            error!(
                "failed to load skill {}: {}",
                err.path.display(),
                err.message
            );
        }

        if let SessionSource::SubAgent(SubAgentSource::ThreadSpawn { depth, .. }) = session_source
            && depth >= MAX_THREAD_SPAWN_DEPTH
        {
            config.features.disable(Feature::Collab);
        }

        let enabled_skills = loaded_skills.enabled_skills();
        let user_instructions = get_user_instructions(&config, Some(&enabled_skills)).await;

        let exec_policy = ExecPolicyManager::load(&config.features, &config.config_layer_stack)
            .await
            .map_err(|err| CodexErr::Fatal(format!("failed to load rules: {err}")))?;

        let config = Arc::new(config);
        let _ = models_manager
            .list_models(
                &config,
                crate::models_manager::manager::RefreshStrategy::OnlineIfUncached,
            )
            .await;
        let model = models_manager
            .get_default_model(
                &config.model,
                &config,
                crate::models_manager::manager::RefreshStrategy::OnlineIfUncached,
            )
            .await;

        let (model_provider_id, model_provider) =
            config.resolve_model_provider_for_model(model.as_str())?;

        let model_info = models_manager.get_model_info(model.as_str(), &config).await;
        let final_instruction_override = models_manager
            .get_final_instruction_override(model.as_str(), &config)
            .await;
        let base_instructions = resolve_session_base_instructions(
            &config,
            &model_info,
            &conversation_history,
            final_instruction_override.as_deref(),
        );

        // Respect thread-start tools. When missing (resumed/forked threads), read from the db
        // first, then fall back to rollout-file tools.
        let persisted_tools = if dynamic_tools.is_empty()
            && config.features.enabled(Feature::Sqlite)
        {
            let thread_id = match &conversation_history {
                InitialHistory::Resumed(resumed) => Some(resumed.conversation_id),
                InitialHistory::Forked(_) => conversation_history.forked_from_id(),
                InitialHistory::New => None,
            };
            match thread_id {
                Some(thread_id) => {
                    let state_db_ctx = state_db::open_if_present(
                        config.codex_home.as_path(),
                        config.model_provider_id.as_str(),
                    )
                    .await;
                    state_db::get_dynamic_tools(state_db_ctx.as_deref(), thread_id, "codex_spawn")
                        .await
                }
                None => None,
            }
        } else {
            None
        };
        let dynamic_tools = if dynamic_tools.is_empty() {
            persisted_tools
                .or_else(|| conversation_history.get_dynamic_tools())
                .unwrap_or_default()
        } else {
            dynamic_tools
        };

        // TODO (aibrahim): Consolidate config.model and config.model_reasoning_effort into config.collaboration_mode
        // to avoid extracting these fields separately and constructing CollaborationMode here.
        let collaboration_mode = CollaborationMode {
            mode: ModeKind::Default,
            settings: Settings {
                model: model.clone(),
                reasoning_effort: config.model_reasoning_effort,
                developer_instructions: None,
            },
        };
        let session_configuration = SessionConfiguration {
            provider_id: model_provider_id,
            provider: model_provider,
            collaboration_mode,
            model_reasoning_summary: config.model_reasoning_summary,
            service_tier: config.service_tier.clone(),
            developer_instructions: config.developer_instructions.clone(),
            user_instructions,
            personality: config.personality,
            base_instructions,
            compact_prompt: config.compact_prompt.clone(),
            approval_policy: config.approval_policy.clone(),
            sandbox_policy: config.sandbox_policy.clone(),
            windows_sandbox_level: WindowsSandboxLevel::from_config(&config),
            cwd: config.cwd.clone(),
            codex_home: config.codex_home.clone(),
            thread_name: None,
            original_config_do_not_use: Arc::clone(&config),
            session_source,
            dynamic_tools,
        };

        // Generate a unique ID for the lifetime of this Codex session.
        let session_source_clone = session_configuration.session_source.clone();
        let (agent_status_tx, agent_status_rx) = watch::channel(AgentStatus::PendingInit);

        let session_init_span = info_span!("session_init");
        let session = Session::new(
            session_configuration,
            config.clone(),
            auth_manager.clone(),
            models_manager.clone(),
            exec_policy,
            tx_sub.clone(),
            tx_event.clone(),
            agent_status_tx.clone(),
            conversation_history,
            session_source_clone,
            skills_manager,
            file_watcher,
            agent_control,
        )
        .instrument(session_init_span)
        .await
        .map_err(|e| {
            error!("Failed to create session: {e:#}");
            map_session_init_error(&e, &config.codex_home)
        })?;
        let thread_id = session.conversation_id;

        // This task will run until Op::Shutdown is received.
        let session_loop_span = info_span!("session_loop", thread_id = %thread_id);
        tokio::spawn(
            submission_loop(Arc::clone(&session), config, rx_sub).instrument(session_loop_span),
        );
        let codex = Codex {
            next_id: AtomicU64::new(0),
            tx_sub,
            rx_event,
            agent_status: agent_status_rx,
            session,
        };

        #[allow(deprecated)]
        Ok(CodexSpawnOk {
            codex,
            thread_id,
            conversation_id: thread_id,
        })
    }

    /// Submit the `op` wrapped in a `Submission` with a unique ID.
    pub async fn submit(&self, op: Op) -> CodexResult<String> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .to_string();
        let sub = Submission { id: id.clone(), op };
        self.submit_with_id(sub).await?;
        Ok(id)
    }

    /// Use sparingly: prefer `submit()` so Codex is responsible for generating
    /// unique IDs for each submission.
    pub async fn submit_with_id(&self, sub: Submission) -> CodexResult<()> {
        self.tx_sub
            .send(sub)
            .await
            .map_err(|_| CodexErr::InternalAgentDied)?;
        Ok(())
    }

    pub async fn next_event(&self) -> CodexResult<Event> {
        let event = self
            .rx_event
            .recv()
            .await
            .map_err(|_| CodexErr::InternalAgentDied)?;
        Ok(event)
    }

    pub(crate) async fn agent_status(&self) -> AgentStatus {
        self.agent_status.borrow().clone()
    }

    pub(crate) async fn active_turn_id(&self) -> Option<String> {
        let active = self.session.active_turn.lock().await;
        active
            .as_ref()
            .and_then(|turn| turn.tasks.first())
            .map(|(sub_id, _task)| sub_id.clone())
    }

    pub(crate) async fn thread_config_snapshot(&self) -> ThreadConfigSnapshot {
        let state = self.session.state.lock().await;
        state.session_configuration.thread_config_snapshot()
    }

    pub(crate) fn state_db(&self) -> Option<state_db::StateDbHandle> {
        self.session.state_db()
    }
}

fn resolve_session_base_instructions(
    config: &Config,
    model_info: &ModelInfo,
    conversation_history: &InitialHistory,
    final_instruction_override: Option<&str>,
) -> String {
    // Priority order:
    // 1. config.base_instructions override
    // 2. model_overlay final instruction override
    // 3. conversation history => session_meta.base_instructions
    // 4. base_instructions for current model
    config
        .base_instructions
        .clone()
        .or_else(|| final_instruction_override.map(ToOwned::to_owned))
        .or_else(|| conversation_history.get_base_instructions().map(|s| s.text))
        .unwrap_or_else(|| {
            ModelsManager::effective_model_instructions(model_info, config.personality, None)
        })
}

fn resolve_current_base_instructions(
    config: &Config,
    model_info: &ModelInfo,
    final_instruction_override: Option<&str>,
) -> String {
    config.base_instructions.clone().unwrap_or_else(|| {
        ModelsManager::effective_model_instructions(
            model_info,
            config.personality,
            final_instruction_override,
        )
    })
}

fn base_instruction_settings_changed(
    previous: &SessionConfiguration,
    next: &SessionConfiguration,
) -> bool {
    previous.collaboration_mode.model() != next.collaboration_mode.model()
        || previous.personality != next.personality
}

impl Session {
    /// Builds the `x-codex-beta-features` header value for this session.
    ///
    /// `ModelClient` is session-scoped and intentionally does not depend on the full `Config`, so
    /// we precompute the comma-separated list of enabled experimental feature keys at session
    /// creation time and thread it into the client.
    fn build_model_client_beta_features_header(config: &Config) -> Option<String> {
        let beta_features_header = FEATURES
            .iter()
            .filter_map(|spec| {
                if spec.stage.experimental_menu_description().is_some()
                    && config.features.enabled(spec.id)
                {
                    Some(spec.key)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(",");

        if beta_features_header.is_empty() {
            None
        } else {
            Some(beta_features_header)
        }
    }

    pub(crate) async fn codex_home(&self) -> PathBuf {
        let state = self.state.lock().await;
        state.session_configuration.codex_home().clone()
    }

    fn start_file_watcher_listener(self: &Arc<Self>) {
        let mut rx = self.services.file_watcher.subscribe();
        let weak_sess = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(FileWatcherEvent::SkillsChanged { .. }) => {
                        let Some(sess) = weak_sess.upgrade() else {
                            break;
                        };
                        let event = Event {
                            id: sess.next_internal_sub_id(),
                            msg: EventMsg::SkillsUpdateAvailable,
                        };
                        sess.send_event_raw(event).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });
    }
    pub(crate) fn get_tx_event(&self) -> Sender<Event> {
        self.tx_event.clone()
    }

    pub(crate) fn state_db(&self) -> Option<state_db::StateDbHandle> {
        self.services.state_db.clone()
    }

    /// Ensure all rollout writes are durably flushed.
    pub(crate) async fn flush_rollout(&self) {
        let recorder = {
            let guard = self.services.rollout.lock().await;
            guard.clone()
        };
        if let Some(rec) = recorder
            && let Err(e) = rec.flush().await
        {
            warn!("failed to flush rollout recorder: {e}");
        }
    }

    pub(crate) async fn load_current_rollout_items(&self) -> Option<Vec<RolloutItem>> {
        self.flush_rollout().await;
        let rollout_path = {
            let guard = self.services.rollout.lock().await;
            guard
                .as_ref()
                .map(|recorder| recorder.rollout_path().to_path_buf())
        }?;

        match RolloutRecorder::load_rollout_items(&rollout_path).await {
            Ok((items, _thread_id, _parse_errors)) => Some(items),
            Err(err) => {
                warn!(
                    "failed to load rollout for reconstruction from {}: {err}",
                    rollout_path.display()
                );
                None
            }
        }
    }

    pub(crate) async fn reconstruct_for_thread_rollback(
        &self,
        turn_context: &TurnContext,
        rollback: ThreadRolledBackEvent,
    ) -> bool {
        let Some(mut rollout_items) = self.load_current_rollout_items().await else {
            return false;
        };
        rollout_items.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)));
        let reconstructed = self
            .reconstruct_history_from_rollout(turn_context, &rollout_items)
            .await;
        self.replace_with_reconstructed_rollout(reconstructed).await;
        true
    }

    pub(crate) async fn set_thread_name(&self, name: String) -> std::io::Result<()> {
        let Some(name) = crate::util::normalize_thread_name(&name) else {
            return Err(std::io::Error::other("thread name cannot be empty"));
        };

        let persistence_enabled = {
            let rollout = self.services.rollout.lock().await;
            rollout.is_some()
        };
        if !persistence_enabled {
            return Err(std::io::Error::other(
                "session persistence is disabled; cannot rename thread",
            ));
        }

        let codex_home = self.codex_home().await;
        session_index::append_thread_name(&codex_home, self.conversation_id, &name).await?;

        let mut state = self.state.lock().await;
        state.session_configuration.thread_name = Some(name);
        Ok(())
    }

    pub(crate) async fn prepare_auto_rename(&self) -> bool {
        let mut state = self.state.lock().await;
        if state.is_forked_session()
            || state.session_configuration.thread_name.is_some()
            || state.auto_rename_attempted()
        {
            return false;
        }
        state.mark_auto_rename_attempted();
        true
    }

    fn next_internal_sub_id(&self) -> String {
        self.next_internal_sub_id_with_prefix("auto-compact")
    }

    pub(crate) fn next_internal_sub_id_with_prefix(&self, prefix: &str) -> String {
        let id = self
            .next_internal_sub_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("{prefix}-{id}")
    }

    async fn get_total_token_usage(&self) -> i64 {
        let state = self.state.lock().await;
        state.get_total_token_usage(state.server_reasoning_included())
    }

    async fn get_estimated_token_count(&self, turn_context: &TurnContext) -> Option<i64> {
        let state = self.state.lock().await;
        state.history.estimate_token_count(turn_context)
    }

    pub(crate) async fn get_base_instructions(&self) -> BaseInstructions {
        let state = self.state.lock().await;
        BaseInstructions {
            text: state.session_configuration.base_instructions.clone(),
        }
    }

    async fn record_initial_history(&self, conversation_history: InitialHistory) {
        let turn_context = self.new_default_turn().await;
        match conversation_history {
            InitialHistory::New => {
                // Initial context is recorded with the first real user turn so the
                // persisted baseline matches a turn boundary.
                self.flush_rollout().await;
            }
            InitialHistory::Resumed(resumed_history) => {
                let rollout_items = resumed_history.history;
                let reconstructed = self
                    .reconstruct_history_from_rollout(&turn_context, &rollout_items)
                    .await;

                // If resuming, warn when the last recorded model differs from the current one.
                let curr = turn_context.model_info.slug.as_str();
                if let Some(prev) = reconstructed
                    .previous_turn_settings
                    .as_ref()
                    .map(|settings| settings.model.as_str())
                    .filter(|prev| *prev != curr)
                {
                    warn!("resuming session with different model: previous={prev}, current={curr}");
                    self.send_event(
                        &turn_context,
                        EventMsg::Warning(WarningEvent {
                            message: format!(
                                "This session was recorded with model `{prev}` but is resuming with `{curr}`. \
                         Consider switching back to `{prev}` as it may affect Codex performance."
                            ),
                        }),
                    )
                    .await;
                }

                self.replace_with_reconstructed_rollout(reconstructed).await;

                // Seed usage info from the recorded rollout so UIs can show token counts
                // immediately on resume/fork.
                if let Some(info) = Self::last_token_info_from_rollout(&rollout_items) {
                    let mut state = self.state.lock().await;
                    state.set_token_info(Some(info));
                }

                // Defer seeding the session's initial context until the first turn starts so
                // turn/start overrides can be merged before we write to the rollout.
                self.flush_rollout().await;
            }
            InitialHistory::Forked(rollout_items) => {
                let reconstructed = self
                    .reconstruct_history_from_rollout(&turn_context, &rollout_items)
                    .await;
                self.replace_with_reconstructed_rollout(reconstructed).await;

                // Seed usage info from the recorded rollout so UIs can show token counts
                // immediately on resume/fork.
                if let Some(info) = Self::last_token_info_from_rollout(&rollout_items) {
                    let mut state = self.state.lock().await;
                    state.set_token_info(Some(info));
                }

                // If persisting, persist all rollout items as-is (recorder filters)
                if !rollout_items.is_empty() {
                    self.persist_rollout_items(&rollout_items).await;
                }

                // Flush after seeding history and any persisted rollout copy.
                self.flush_rollout().await;
            }
        }
    }

    async fn replace_with_reconstructed_rollout(&self, reconstructed: ReconstructedRollout) {
        let mut state = self.state.lock().await;
        state.replace_history(reconstructed.history);
        state
            .history
            .set_reference_context_item(reconstructed.reference_context_item.clone());
        state.previous_turn_settings = reconstructed.previous_turn_settings;
        state.pending_continuation = reconstructed.pending_continuation;
        state.initial_context_seeded = reconstructed.reference_context_item.is_some();
    }

    fn last_token_info_from_rollout(rollout_items: &[RolloutItem]) -> Option<TokenUsageInfo> {
        rollout_items.iter().rev().find_map(|item| match item {
            RolloutItem::EventMsg(EventMsg::TokenCount(ev)) => ev.info.clone(),
            _ => None,
        })
    }

    #[cfg(test)]
    fn pending_continuation_from_rollout(
        rollout_items: &[RolloutItem],
        reconstructed_history: &[ResponseItem],
    ) -> Option<PendingContinuation> {
        let mut pending_event: Option<PendingContinuation> = None;

        for item in rollout_items {
            match item {
                RolloutItem::EventMsg(EventMsg::TurnAborted(ev))
                    if ev.reason == TurnAbortReason::Interrupted =>
                {
                    pending_event = Some(PendingContinuation {
                        source: TurnContinuationSource::Interrupted,
                        continued_from_turn_id: None,
                        model: None,
                        pause_reason: None,
                        target: PendingContinuationTarget::Regular,
                    });
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_))
                | RolloutItem::EventMsg(EventMsg::UserMessage(_)) => {
                    pending_event = None;
                }
                RolloutItem::ResponseItem(response_item)
                    if is_user_turn_boundary_response_item(response_item) =>
                {
                    pending_event = None;
                }
                _ => {}
            }
        }

        pending_event.or_else(|| {
            history_needs_continuation(reconstructed_history).then_some(PendingContinuation {
                source: TurnContinuationSource::Interrupted,
                continued_from_turn_id: None,
                model: None,
                pause_reason: None,
                target: PendingContinuationTarget::Regular,
            })
        })
    }

    pub(crate) async fn update_settings(
        &self,
        updates: SessionSettingsUpdate,
    ) -> ConstraintResult<()> {
        self.apply_settings_update(updates).await.map(|_| ())
    }

    pub(crate) async fn apply_settings_update(
        &self,
        updates: SessionSettingsUpdate,
    ) -> ConstraintResult<(SessionConfiguration, bool)> {
        let (mut updated, sandbox_policy_changed, refresh_base_instructions) = {
            let state = self.state.lock().await;
            let current = state.session_configuration.clone();
            let updated = match current.apply(&updates) {
                Ok(updated) => updated,
                Err(err) => {
                    warn!("rejected session settings update: {err}");
                    return Err(err);
                }
            };
            let sandbox_policy_changed = current.sandbox_policy != updated.sandbox_policy;
            let refresh_base_instructions = base_instruction_settings_changed(&current, &updated);
            (updated, sandbox_policy_changed, refresh_base_instructions)
        };

        if refresh_base_instructions {
            let per_turn_config = Self::build_per_turn_config(&updated);
            let model = updated.collaboration_mode.model().to_string();
            let model_info = self
                .services
                .models_manager
                .get_model_info(model.as_str(), &per_turn_config)
                .await;
            let final_instruction_override = self
                .services
                .models_manager
                .get_final_instruction_override(model.as_str(), &per_turn_config)
                .await;
            updated.base_instructions = resolve_current_base_instructions(
                &per_turn_config,
                &model_info,
                final_instruction_override.as_deref(),
            );
        }

        let mut state = self.state.lock().await;
        state.session_configuration = updated.clone();
        Ok((updated, sandbox_policy_changed))
    }

    async fn get_config(&self) -> std::sync::Arc<Config> {
        let state = self.state.lock().await;
        state
            .session_configuration
            .original_config_do_not_use
            .clone()
    }
    fn build_environment_update_item(
        &self,
        previous: &TurnContextItem,
        next: &TurnContext,
    ) -> Option<ResponseItem> {
        if previous.cwd == next.cwd {
            return None;
        }

        Some(ResponseItem::from(EnvironmentContext::new(
            Some(next.cwd.clone()),
            self.user_shell().as_ref().clone(),
        )))
    }

    fn build_permissions_update_item(
        &self,
        previous: &TurnContextItem,
        next: &TurnContext,
    ) -> Option<ResponseItem> {
        if previous.sandbox_policy == next.sandbox_policy
            && previous.approval_policy == next.approval_policy
        {
            return None;
        }

        Some(
            DeveloperInstructions::from_policy(
                &next.sandbox_policy,
                next.approval_policy,
                self.services.exec_policy.current().as_ref(),
                self.features.enabled(Feature::RequestRule),
                &next.cwd,
            )
            .into(),
        )
    }

    fn build_developer_instructions_update_item(
        previous: &TurnContextItem,
        next: &TurnContext,
    ) -> Option<ResponseItem> {
        if previous.developer_instructions == next.developer_instructions {
            return None;
        }

        next.developer_instructions
            .as_ref()
            .map(|instructions| DeveloperInstructions::new(instructions.clone()).into())
    }

    fn build_user_instructions_update_item(
        previous: &TurnContextItem,
        next: &TurnContext,
    ) -> Option<ResponseItem> {
        if previous.user_instructions == next.user_instructions {
            return None;
        }

        next.user_instructions.as_ref().map(|instructions| {
            UserInstructions {
                text: instructions.clone(),
                directory: next.cwd.to_string_lossy().into_owned(),
            }
            .into()
        })
    }

    fn build_personality_update_item(
        &self,
        previous: &TurnContextItem,
        previous_turn_settings: Option<&PreviousTurnSettings>,
        next: &TurnContext,
    ) -> Option<ResponseItem> {
        if !self.features.enabled(Feature::Personality) {
            return None;
        }
        if next.final_instruction_override.is_some() {
            return None;
        }
        let previous_model = previous_turn_settings
            .map(|settings| settings.model.as_str())
            .unwrap_or(previous.model.as_str());
        if next.model_info.slug != previous_model {
            return None;
        }

        if let Some(personality) = next.personality
            && next.personality != previous.personality
        {
            let model_info = &next.model_info;
            let personality_message = Self::personality_message_for(model_info, personality);
            personality_message.map(|personality_message| {
                DeveloperInstructions::personality_spec_message(personality_message).into()
            })
        } else {
            None
        }
    }

    fn personality_message_for(model_info: &ModelInfo, personality: Personality) -> Option<String> {
        model_info
            .model_messages
            .as_ref()
            .and_then(|spec| spec.get_personality_message(Some(personality)))
            .filter(|message| !message.is_empty())
    }

    fn build_collaboration_mode_update_item(
        &self,
        previous: &TurnContextItem,
        next: &TurnContext,
    ) -> Option<ResponseItem> {
        let previous_collaboration_mode = previous.collaboration_mode.as_ref()?;
        if previous_collaboration_mode != &next.collaboration_mode {
            // If the next mode has empty developer instructions, this returns None and we emit no
            // update, so prior collaboration instructions remain in the prompt history.
            Some(DeveloperInstructions::from_collaboration_mode(&next.collaboration_mode)?.into())
        } else {
            None
        }
    }

    fn build_model_instructions_update_item(
        &self,
        previous: &TurnContextItem,
        previous_turn_settings: Option<&PreviousTurnSettings>,
        next: &TurnContext,
    ) -> Option<ResponseItem> {
        let previous_model = previous_turn_settings
            .map(|settings| settings.model.as_str())
            .unwrap_or(previous.model.as_str());
        if previous_model == next.model_info.slug {
            return None;
        }

        let model_instructions = next.effective_model_instructions();
        if model_instructions.is_empty() {
            return None;
        }

        Some(DeveloperInstructions::model_switch_message(model_instructions).into())
    }

    fn build_settings_update_items(
        &self,
        previous_context: &TurnContextItem,
        previous_turn_settings: Option<&PreviousTurnSettings>,
        current_context: &TurnContext,
    ) -> Vec<ResponseItem> {
        let mut update_items = Vec::new();
        if let Some(env_item) =
            self.build_environment_update_item(previous_context, current_context)
        {
            update_items.push(env_item);
        }
        if let Some(model_instructions_item) = self.build_model_instructions_update_item(
            previous_context,
            previous_turn_settings,
            current_context,
        ) {
            update_items.push(model_instructions_item);
        }
        if let Some(permissions_item) =
            self.build_permissions_update_item(previous_context, current_context)
        {
            update_items.push(permissions_item);
        }
        if let Some(developer_instructions_item) =
            Self::build_developer_instructions_update_item(previous_context, current_context)
        {
            update_items.push(developer_instructions_item);
        }
        if let Some(collaboration_mode_item) =
            self.build_collaboration_mode_update_item(previous_context, current_context)
        {
            update_items.push(collaboration_mode_item);
        }
        if let Some(personality_item) = self.build_personality_update_item(
            previous_context,
            previous_turn_settings,
            current_context,
        ) {
            update_items.push(personality_item);
        }
        if let Some(user_instructions_item) =
            Self::build_user_instructions_update_item(previous_context, current_context)
        {
            update_items.push(user_instructions_item);
        }
        update_items
    }

    async fn build_full_context_with_model_switch_if_needed(
        &self,
        previous_context: &TurnContextItem,
        previous_turn_settings: Option<&PreviousTurnSettings>,
        current_context: &TurnContext,
    ) -> Vec<ResponseItem> {
        let mut update_items = self.build_initial_context(current_context).await;
        if let Some(model_instructions_item) = self.build_model_instructions_update_item(
            previous_context,
            previous_turn_settings,
            current_context,
        ) {
            update_items.insert(0, model_instructions_item);
        }
        update_items
    }

    /// Persist the event to rollout and send it to clients.
    pub(crate) async fn send_event(&self, turn_context: &TurnContext, msg: EventMsg) {
        let show_raw_agent_reasoning = self.show_raw_agent_reasoning();
        let legacy_events = msg.as_legacy_events(show_raw_agent_reasoning);
        let event = Event {
            id: turn_context.sub_id.clone(),
            msg,
        };
        self.send_event_raw(event).await;

        for legacy in legacy_events {
            let legacy_event = Event {
                id: turn_context.sub_id.clone(),
                msg: legacy,
            };
            self.send_event_raw(legacy_event).await;
        }
    }

    /// Send an event to clients without recording it in this session's rollout.
    ///
    /// Use this for events whose canonical persistence belongs to another
    /// session, such as forwarded delegate events.
    pub(crate) async fn send_event_transient(&self, turn_context: &TurnContext, msg: EventMsg) {
        self.send_event_transient_with_id(turn_context.sub_id.clone(), msg)
            .await;
    }

    /// Send an event to clients without recording it in this session's rollout,
    /// preserving the caller-provided event id.
    pub(crate) async fn send_event_transient_with_id(&self, id: String, msg: EventMsg) {
        let show_raw_agent_reasoning = self.show_raw_agent_reasoning();
        let legacy_events = msg.as_legacy_events(show_raw_agent_reasoning);
        let event = Event {
            id: id.clone(),
            msg,
        };
        self.send_event_raw_transient(event).await;

        for legacy in legacy_events {
            let legacy_event = Event {
                id: id.clone(),
                msg: legacy,
            };
            self.send_event_raw_transient(legacy_event).await;
        }
    }

    pub(crate) async fn send_event_raw(&self, event: Event) {
        // Persist the event into rollout (recorder filters as needed)
        let rollout_items = vec![RolloutItem::EventMsg(event.msg.clone())];
        self.persist_rollout_items(&rollout_items).await;
        self.dispatch_event_raw(event).await;
    }

    /// Persist the event to the rollout file, flush it, and only then deliver it to clients.
    ///
    /// Most events can be delivered immediately after queueing the rollout write, but some
    /// clients (e.g. app-server thread/rollback) re-read the rollout file synchronously on
    /// receipt of the event and depend on the marker already being visible on disk.
    pub(crate) async fn send_event_raw_flushed(&self, event: Event) {
        self.persist_rollout_items(&[RolloutItem::EventMsg(event.msg.clone())])
            .await;
        self.flush_rollout().await;
        self.dispatch_event_raw(event).await;
    }

    async fn send_event_raw_transient(&self, event: Event) {
        self.dispatch_event_raw(event).await;
    }

    async fn dispatch_event_raw(&self, event: Event) {
        // Record the last known agent status.
        if let Some(status) = agent_status_from_event(&event.msg) {
            self.agent_status.send_replace(status);
        }
        if let Err(e) = self.tx_event.send(event).await {
            debug!("dropping event because channel is closed: {e}");
        }
    }

    pub(crate) async fn emit_turn_item_started(&self, turn_context: &TurnContext, item: &TurnItem) {
        self.send_event(
            turn_context,
            EventMsg::ItemStarted(ItemStartedEvent {
                thread_id: self.conversation_id,
                turn_id: turn_context.sub_id.clone(),
                item: item.clone(),
            }),
        )
        .await;
    }

    pub(crate) async fn emit_turn_item_completed(
        &self,
        turn_context: &TurnContext,
        item: TurnItem,
    ) {
        self.send_event(
            turn_context,
            EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: self.conversation_id,
                turn_id: turn_context.sub_id.clone(),
                item,
            }),
        )
        .await;
    }

    pub(crate) async fn emit_progress_trace(
        &self,
        turn_context: &TurnContext,
        category: ProgressTraceCategory,
        state: ProgressTraceState,
        label: Option<String>,
        source: Option<&str>,
    ) {
        self.send_event(
            turn_context,
            EventMsg::ProgressTrace(ProgressTraceEvent {
                thread_id: self.conversation_id,
                turn_id: turn_context.sub_id.clone(),
                category,
                state,
                label,
                source: source.map(str::to_string),
            }),
        )
        .await;
    }

    /// Adds an execpolicy amendment to both the in-memory and on-disk policies so future
    /// commands can use the newly approved prefix.
    pub(crate) async fn persist_execpolicy_amendment(
        &self,
        amendment: &ExecPolicyAmendment,
    ) -> Result<(), ExecPolicyUpdateError> {
        let features = self.features.clone();
        let codex_home = self
            .state
            .lock()
            .await
            .session_configuration
            .codex_home()
            .clone();

        if !features.enabled(Feature::ExecPolicy) {
            error!("attempted to append execpolicy rule while execpolicy feature is disabled");
            return Err(ExecPolicyUpdateError::FeatureDisabled);
        }

        self.services
            .exec_policy
            .append_amendment_and_update(&codex_home, amendment)
            .await?;

        Ok(())
    }

    async fn turn_context_for_sub_id(&self, sub_id: &str) -> Option<Arc<TurnContext>> {
        let active = self.active_turn.lock().await;
        active
            .as_ref()
            .and_then(|turn| turn.tasks.get(sub_id))
            .map(|task| Arc::clone(&task.turn_context))
    }

    async fn active_turn_context_and_cancellation_token(
        &self,
    ) -> Option<(Arc<TurnContext>, CancellationToken)> {
        let active = self.active_turn.lock().await;
        let (_, task) = active.as_ref()?.tasks.first()?;
        Some((
            Arc::clone(&task.turn_context),
            task.cancellation_token.child_token(),
        ))
    }

    pub(crate) async fn record_execpolicy_amendment_message(
        &self,
        sub_id: &str,
        amendment: &ExecPolicyAmendment,
    ) {
        let Some(prefixes) = format_allow_prefixes(vec![amendment.command.clone()]) else {
            warn!("execpolicy amendment for {sub_id} had no command prefix");
            return;
        };
        let text = format!("Approved command prefix saved:\n{prefixes}");
        let message: ResponseItem = DeveloperInstructions::new(text.clone()).into();

        if let Some(turn_context) = self.turn_context_for_sub_id(sub_id).await {
            self.record_conversation_items(&turn_context, std::slice::from_ref(&message))
                .await;
            return;
        }

        if self
            .inject_response_items(vec![ResponseInputItem::Message {
                role: "developer".to_string(),
                content: vec![ContentItem::InputText { text }],
            }])
            .await
            .is_err()
        {
            warn!("no active turn found to record execpolicy amendment message for {sub_id}");
        }
    }

    /// Emit an exec approval request event and await the user's decision.
    ///
    /// The request is keyed by `sub_id`/`call_id` so matching responses are delivered
    /// to the correct in-flight turn. If the task is aborted, this returns the
    /// default `ReviewDecision` (`Denied`).
    #[allow(clippy::too_many_arguments)]
    pub async fn request_command_approval(
        &self,
        turn_context: &TurnContext,
        call_id: String,
        command: Vec<String>,
        cwd: PathBuf,
        reason: Option<String>,
        proposed_execpolicy_amendment: Option<ExecPolicyAmendment>,
    ) -> ReviewDecision {
        let sub_id = turn_context.sub_id.clone();
        // Add the tx_approve callback to the map before sending the request.
        let (tx_approve, rx_approve) = oneshot::channel();
        let event_id = sub_id.clone();
        let prev_entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_approval(sub_id, tx_approve)
                }
                None => None,
            }
        };
        if prev_entry.is_some() {
            warn!("Overwriting existing pending approval for sub_id: {event_id}");
        }

        let parsed_cmd = parse_command(&command);
        let event = EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
            call_id,
            turn_id: turn_context.sub_id.clone(),
            command,
            cwd,
            reason,
            proposed_execpolicy_amendment,
            parsed_cmd,
        });
        self.send_event(turn_context, event).await;
        rx_approve.await.unwrap_or_default()
    }

    pub async fn request_patch_approval(
        &self,
        turn_context: &TurnContext,
        call_id: String,
        changes: HashMap<PathBuf, FileChange>,
        reason: Option<String>,
        grant_root: Option<PathBuf>,
    ) -> oneshot::Receiver<ReviewDecision> {
        let sub_id = turn_context.sub_id.clone();
        // Add the tx_approve callback to the map before sending the request.
        let (tx_approve, rx_approve) = oneshot::channel();
        let event_id = sub_id.clone();
        let prev_entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_approval(sub_id, tx_approve)
                }
                None => None,
            }
        };
        if prev_entry.is_some() {
            warn!("Overwriting existing pending approval for sub_id: {event_id}");
        }

        let event = EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
            call_id,
            turn_id: turn_context.sub_id.clone(),
            changes,
            reason,
            grant_root,
        });
        self.send_event(turn_context, event).await;
        rx_approve
    }

    pub async fn request_user_input(
        &self,
        turn_context: &TurnContext,
        call_id: String,
        args: RequestUserInputArgs,
    ) -> Option<RequestUserInputResponse> {
        let sub_id = turn_context.sub_id.clone();
        let (tx_response, rx_response) = oneshot::channel();
        let event_id = sub_id.clone();
        let prev_entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_user_input(sub_id, tx_response)
                }
                None => None,
            }
        };
        if prev_entry.is_some() {
            warn!("Overwriting existing pending user input for sub_id: {event_id}");
        }

        let event = EventMsg::RequestUserInput(RequestUserInputEvent {
            call_id,
            turn_id: turn_context.sub_id.clone(),
            questions: args.questions,
        });
        self.emit_progress_trace(
            turn_context,
            ProgressTraceCategory::Waiting,
            ProgressTraceState::Started,
            Some("Waiting for user input".to_string()),
            Some("request_user_input"),
        )
        .await;
        self.send_event(turn_context, event).await;
        let response = rx_response.await.ok();
        self.emit_progress_trace(
            turn_context,
            ProgressTraceCategory::Waiting,
            ProgressTraceState::Completed,
            None,
            Some("request_user_input"),
        )
        .await;
        response
    }

    pub async fn notify_user_input_response(
        &self,
        sub_id: &str,
        response: RequestUserInputResponse,
    ) {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_user_input(sub_id)
                }
                None => None,
            }
        };
        match entry {
            Some(tx_response) => {
                tx_response.send(response).ok();
            }
            None => {
                warn!("No pending user input found for sub_id: {sub_id}");
            }
        }
    }

    pub async fn notify_dynamic_tool_response(&self, call_id: &str, response: DynamicToolResponse) {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_dynamic_tool(call_id)
                }
                None => None,
            }
        };
        match entry {
            Some(tx_response) => {
                tx_response.send(response).ok();
            }
            None => {
                warn!("No pending dynamic tool call found for call_id: {call_id}");
            }
        }
    }

    pub async fn notify_approval(&self, sub_id: &str, decision: ReviewDecision) {
        let entry = {
            let mut active = self.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.remove_pending_approval(sub_id)
                }
                None => None,
            }
        };
        match entry {
            Some(tx_approve) => {
                tx_approve.send(decision).ok();
            }
            None => {
                warn!("No pending approval found for sub_id: {sub_id}");
            }
        }
    }

    /// Records input items: always append to conversation history and
    /// persist these response items to rollout.
    pub(crate) async fn record_conversation_items(
        &self,
        turn_context: &TurnContext,
        items: &[ResponseItem],
    ) {
        self.record_into_history(items, turn_context).await;
        self.persist_rollout_response_items(items).await;
        self.send_raw_response_items(turn_context, items).await;
    }

    /// Append ResponseItems to the in-memory conversation history only.
    pub(crate) async fn record_into_history(
        &self,
        items: &[ResponseItem],
        turn_context: &TurnContext,
    ) {
        let mut state = self.state.lock().await;
        state.record_items(items.iter(), turn_context.truncation_policy);
    }

    pub(crate) async fn maybe_pause_on_server_model_mismatch(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        server_model: String,
    ) -> bool {
        let requested_model = turn_context.model_info.slug.clone();
        if server_model.eq_ignore_ascii_case(&requested_model) {
            info!("server reported model {server_model} (matches requested model)");
            return false;
        }

        warn!("server reported model {server_model} while requested model was {requested_model}");

        self.send_event(
            turn_context,
            EventMsg::ModelReroute(ModelRerouteEvent {
                from_model: requested_model.clone(),
                to_model: server_model.clone(),
                reason: ModelRerouteReason::ServerSelectedDifferentModel,
            }),
        )
        .await;

        self.send_event(
            turn_context,
            EventMsg::Warning(WarningEvent {
                message: server_model_mismatch_warning(&requested_model, &server_model),
            }),
        )
        .await;
        self.pause_current_task_from_self(
            turn_context,
            TurnPauseReason::ServerSelectedDifferentModel,
            Some(requested_model),
        )
        .await;
        true
    }

    pub(crate) async fn emit_model_verification(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        verifications: Vec<ModelVerification>,
    ) {
        self.send_event(
            turn_context,
            EventMsg::ModelVerification(ModelVerificationEvent { verifications }),
        )
        .await;
    }

    pub(crate) async fn record_model_warning(&self, message: impl Into<String>, ctx: &TurnContext) {
        self.services
            .otel_manager
            .counter("codex.model_warning", 1, &[]);
        let item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!("Warning: {}", message.into()),
            }],
            end_turn: None,
            phase: None,
        };

        self.record_conversation_items(ctx, &[item]).await;
    }

    pub(crate) async fn replace_history(&self, items: Vec<ResponseItem>) {
        let mut state = self.state.lock().await;
        state.replace_history(items);
    }

    pub(crate) async fn replace_compacted_history(
        &self,
        turn_context: &TurnContext,
        items: Vec<ResponseItem>,
        reference_context_item: Option<TurnContextItem>,
        compacted_item: CompactedItem,
    ) {
        {
            let mut state = self.state.lock().await;
            state.replace_history(items);
            state
                .history
                .set_reference_context_item(reference_context_item.clone());
            state.previous_turn_settings =
                reference_context_item
                    .as_ref()
                    .map(|item| PreviousTurnSettings {
                        model: item.model.clone(),
                    });
            state.initial_context_seeded = reference_context_item.is_some();
        }

        let mut rollout_items = vec![RolloutItem::Compacted(compacted_item)];
        if let Some(reference_context_item) = reference_context_item {
            rollout_items.push(RolloutItem::TurnContext(reference_context_item));
        }
        self.persist_rollout_items(&rollout_items).await;
        self.recompute_token_usage(turn_context).await;
    }

    pub(crate) async fn reference_context_item(&self) -> Option<TurnContextItem> {
        let state = self.state.lock().await;
        state.history.reference_context_item()
    }

    pub(crate) async fn previous_turn_settings(&self) -> Option<PreviousTurnSettings> {
        let state = self.state.lock().await;
        state.previous_turn_settings.clone()
    }

    pub(crate) async fn clear_turn_context_baseline(&self) {
        let mut state = self.state.lock().await;
        state.history.set_reference_context_item(None);
        state.previous_turn_settings = None;
        state.initial_context_seeded = false;
    }

    pub(crate) async fn record_context_updates_and_set_reference_context_item(
        &self,
        turn_context: &TurnContext,
    ) {
        let reference_context_item = self.reference_context_item().await;
        let previous_turn_settings = self.previous_turn_settings().await;

        let update_items = if let Some(reference_context_item) = reference_context_item.as_ref() {
            if reference_context_item.collaboration_mode.is_some() {
                self.build_settings_update_items(
                    reference_context_item,
                    previous_turn_settings.as_ref(),
                    turn_context,
                )
            } else {
                self.build_full_context_with_model_switch_if_needed(
                    reference_context_item,
                    previous_turn_settings.as_ref(),
                    turn_context,
                )
                .await
            }
        } else {
            self.build_initial_context(turn_context).await
        };

        if !update_items.is_empty() {
            self.record_conversation_items(turn_context, &update_items)
                .await;
        }

        let current_context_item = turn_context.to_turn_context_item();
        self.persist_rollout_items(&[RolloutItem::TurnContext(current_context_item.clone())])
            .await;

        let mut state = self.state.lock().await;
        state.initial_context_seeded = true;
        state
            .history
            .set_reference_context_item(Some(current_context_item.clone()));
        state.previous_turn_settings = Some(PreviousTurnSettings {
            model: current_context_item.model,
        });
    }

    pub(crate) async fn seed_initial_context_if_needed(&self, turn_context: &TurnContext) {
        {
            let state = self.state.lock().await;
            if state.initial_context_seeded {
                return;
            }
        }

        self.record_context_updates_and_set_reference_context_item(turn_context)
            .await;
        self.flush_rollout().await;
    }

    async fn persist_rollout_response_items(&self, items: &[ResponseItem]) {
        let rollout_items: Vec<RolloutItem> = items
            .iter()
            .cloned()
            .map(RolloutItem::ResponseItem)
            .collect();
        self.persist_rollout_items(&rollout_items).await;
    }

    pub fn enabled(&self, feature: Feature) -> bool {
        self.features.enabled(feature)
    }

    pub(crate) fn features(&self) -> Features {
        self.features.clone()
    }

    pub(crate) async fn collaboration_mode(&self) -> CollaborationMode {
        let state = self.state.lock().await;
        state.session_configuration.collaboration_mode.clone()
    }

    async fn send_raw_response_items(&self, turn_context: &TurnContext, items: &[ResponseItem]) {
        for item in items {
            self.send_event(
                turn_context,
                EventMsg::RawResponseItem(RawResponseItemEvent { item: item.clone() }),
            )
            .await;
        }
    }

    pub(crate) async fn build_initial_context(
        &self,
        turn_context: &TurnContext,
    ) -> Vec<ResponseItem> {
        let mut items = Vec::<ResponseItem>::with_capacity(4);
        let shell = self.user_shell();
        items.push(
            DeveloperInstructions::from_policy(
                &turn_context.sandbox_policy,
                turn_context.approval_policy,
                self.services.exec_policy.current().as_ref(),
                self.features.enabled(Feature::RequestRule),
                &turn_context.cwd,
            )
            .into(),
        );
        if let Some(developer_instructions) = turn_context.developer_instructions.as_deref() {
            items.push(DeveloperInstructions::new(developer_instructions.to_string()).into());
        }
        // Add developer instructions from collaboration_mode if they exist and are non-empty
        let (collaboration_mode, base_instructions) = {
            let state = self.state.lock().await;
            (
                state.session_configuration.collaboration_mode.clone(),
                state.session_configuration.base_instructions.clone(),
            )
        };
        if let Some(collab_instructions) =
            DeveloperInstructions::from_collaboration_mode(&collaboration_mode)
        {
            items.push(collab_instructions.into());
        }
        if self.features.enabled(Feature::Personality)
            && let Some(personality) = turn_context.personality
            && turn_context.final_instruction_override.is_none()
        {
            let model_info = turn_context.model_info.clone();
            let has_baked_personality = model_info.supports_personality()
                && base_instructions == model_info.get_model_instructions(Some(personality));
            if !has_baked_personality
                && let Some(personality_message) =
                    Self::personality_message_for(&model_info, personality)
            {
                items.push(
                    DeveloperInstructions::personality_spec_message(personality_message).into(),
                );
            }
        }
        if let Some(user_instructions) = turn_context.user_instructions.as_deref() {
            items.push(
                UserInstructions {
                    text: user_instructions.to_string(),
                    directory: turn_context.cwd.to_string_lossy().into_owned(),
                }
                .into(),
            );
        }
        items.push(ResponseItem::from(EnvironmentContext::new(
            Some(turn_context.cwd.clone()),
            shell.as_ref().clone(),
        )));
        items
    }

    pub(crate) async fn persist_rollout_items(&self, items: &[RolloutItem]) {
        let recorder = {
            let guard = self.services.rollout.lock().await;
            guard.clone()
        };
        if let Some(rec) = recorder
            && let Err(e) = rec.record_items(items).await
        {
            error!("failed to record rollout items: {e:#}");
        }
    }

    pub(crate) async fn clone_history(&self) -> ContextManager {
        let state = self.state.lock().await;
        state.clone_history()
    }

    pub(crate) async fn prompt_history(&self, turn_context: &TurnContext) -> Vec<ResponseItem> {
        let items = {
            let state = self.state.lock().await;
            state.history.raw_items().to_vec()
        };
        ContextManager::prepare_items_for_prompt_with_modalities(
            items,
            &turn_context.model_info.input_modalities,
        )
    }

    pub(crate) async fn update_token_usage_info(
        &self,
        turn_context: &TurnContext,
        token_usage: Option<&TokenUsage>,
    ) {
        {
            let mut state = self.state.lock().await;
            if let Some(token_usage) = token_usage {
                state
                    .update_token_info_from_usage(token_usage, turn_context.model_context_window());
            }
        }
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn recompute_token_usage(&self, turn_context: &TurnContext) {
        let base_instructions = self.get_base_instructions().await;
        let estimated_total_tokens = {
            let state = self.state.lock().await;
            state
                .history
                .estimate_token_count_with_base_instructions(&base_instructions)
        };
        let Some(estimated_total_tokens) = estimated_total_tokens else {
            return;
        };
        {
            let mut state = self.state.lock().await;
            let mut info = state.token_info().unwrap_or(TokenUsageInfo {
                total_token_usage: TokenUsage::default(),
                last_token_usage: TokenUsage::default(),
                model_context_window: None,
            });

            info.last_token_usage = TokenUsage {
                input_tokens: 0,
                cached_input_tokens: 0,
                output_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: estimated_total_tokens.max(0),
            };

            if info.model_context_window.is_none() {
                info.model_context_window = turn_context.model_context_window();
            }

            state.set_token_info(Some(info));
        }
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn update_rate_limits(
        &self,
        turn_context: &TurnContext,
        new_rate_limits: RateLimitSnapshot,
    ) {
        {
            let mut state = self.state.lock().await;
            state.set_rate_limits(new_rate_limits);
        }
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn mcp_dependency_prompted(&self) -> HashSet<String> {
        let state = self.state.lock().await;
        state.mcp_dependency_prompted()
    }

    pub(crate) async fn record_mcp_dependency_prompted<I>(&self, names: I)
    where
        I: IntoIterator<Item = String>,
    {
        let mut state = self.state.lock().await;
        state.record_mcp_dependency_prompted(names);
    }

    pub async fn dependency_env(&self) -> HashMap<String, String> {
        let state = self.state.lock().await;
        state.dependency_env()
    }

    pub async fn set_dependency_env(&self, values: HashMap<String, String>) {
        let mut state = self.state.lock().await;
        state.set_dependency_env(values);
    }

    pub(crate) async fn set_server_reasoning_included(&self, included: bool) {
        let mut state = self.state.lock().await;
        state.set_server_reasoning_included(included);
    }

    async fn send_token_count_event(&self, turn_context: &TurnContext) {
        let (info, rate_limits) = {
            let state = self.state.lock().await;
            state.token_info_and_rate_limits()
        };
        let event = EventMsg::TokenCount(TokenCountEvent { info, rate_limits });
        self.send_event(turn_context, event).await;
    }

    pub(crate) async fn set_total_tokens_full(&self, turn_context: &TurnContext) {
        if let Some(context_window) = turn_context.model_context_window() {
            let mut state = self.state.lock().await;
            state.set_token_usage_full(context_window);
        }
        self.send_token_count_event(turn_context).await;
    }

    pub(crate) async fn record_response_item_and_emit_turn_item(
        &self,
        turn_context: &TurnContext,
        response_item: ResponseItem,
    ) {
        // Add to conversation history and persist response item to rollout.
        self.record_conversation_items(turn_context, std::slice::from_ref(&response_item))
            .await;

        // Derive a turn item and emit lifecycle events if applicable.
        if let Some(item) = parse_turn_item(&response_item) {
            self.emit_turn_item_started(turn_context, &item).await;
            self.emit_turn_item_completed(turn_context, item).await;
        }
    }

    pub(crate) async fn record_user_prompt_and_emit_turn_item(
        &self,
        turn_context: &TurnContext,
        input: &[UserInput],
        response_item: ResponseItem,
    ) {
        // Persist the user message to history, but emit the turn item from `UserInput` so
        // UI-only `text_elements` are preserved. `ResponseItem::Message` does not carry
        // those spans, and `record_response_item_and_emit_turn_item` would drop them.
        self.record_conversation_items(turn_context, std::slice::from_ref(&response_item))
            .await;
        let turn_item = TurnItem::UserMessage(UserMessageItem::new(input));
        self.emit_turn_item_started(turn_context, &turn_item).await;
        self.emit_turn_item_completed(turn_context, turn_item).await;
    }

    pub(crate) async fn record_pending_input(
        &self,
        turn_context: &TurnContext,
        pending_input: TurnInput,
    ) {
        match pending_input {
            TurnInput::UserInput {
                content,
                client_id: _,
            } => {
                let response_item: ResponseItem = ResponseInputItem::from(content.clone()).into();
                self.record_user_prompt_and_emit_turn_item(
                    turn_context,
                    content.as_slice(),
                    response_item,
                )
                .await;
            }
            TurnInput::ResponseItem(item) => {
                let response_item: ResponseItem = item.into();
                if let Some(TurnItem::UserMessage(user_message)) = parse_turn_item(&response_item) {
                    self.record_user_prompt_and_emit_turn_item(
                        turn_context,
                        user_message.content.as_slice(),
                        response_item,
                    )
                    .await;
                } else {
                    self.record_conversation_items(
                        turn_context,
                        std::slice::from_ref(&response_item),
                    )
                    .await;
                }
            }
        }
    }

    pub(crate) async fn notify_background_event(
        &self,
        turn_context: &TurnContext,
        message: impl Into<String>,
    ) {
        let event = EventMsg::BackgroundEvent(BackgroundEventEvent {
            message: message.into(),
        });
        self.send_event(turn_context, event).await;
    }

    pub(crate) async fn notify_stream_error(
        &self,
        turn_context: &TurnContext,
        message: impl Into<String>,
        codex_error: CodexErr,
    ) {
        let additional_details = codex_error.to_string();
        let codex_error_info = CodexErrorInfo::ResponseStreamDisconnected {
            http_status_code: codex_error.http_status_code_value(),
        };
        let event = EventMsg::StreamError(StreamErrorEvent {
            message: message.into(),
            codex_error_info: Some(codex_error_info),
            additional_details: Some(additional_details),
        });
        self.send_event(turn_context, event).await;
    }

    async fn maybe_start_ghost_snapshot(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        cancellation_token: CancellationToken,
    ) {
        if !self.enabled(Feature::GhostCommit) {
            return;
        }
        let token = match turn_context.tool_call_gate.subscribe().await {
            Ok(token) => token,
            Err(err) => {
                warn!("failed to subscribe to ghost snapshot readiness: {err}");
                return;
            }
        };

        info!("spawning ghost snapshot task");
        let task = GhostSnapshotTask::new(token);
        Arc::new(task)
            .run(
                Arc::new(SessionTaskContext::new(self.clone())),
                turn_context.clone(),
                Vec::new(),
                cancellation_token,
            )
            .await;
    }

    /// Inject additional user input into the currently active regular turn.
    pub async fn steer_input(&self, input: Vec<UserInput>) -> Result<(), SteerInputError> {
        self.steer_input_for_turn(input, None, None)
            .await
            .map(|_| ())
    }

    /// Inject additional user input into the currently active regular turn observed by the caller.
    pub async fn steer_input_for_turn(
        &self,
        input: Vec<UserInput>,
        expected_turn_id: Option<&str>,
        client_user_message_id: Option<String>,
    ) -> Result<String, SteerInputError> {
        if input.is_empty() {
            return Err(SteerInputError::EmptyInput);
        }

        let mut active = self.active_turn.lock().await;
        let Some(active_turn) = active.as_mut() else {
            return Err(SteerInputError::NoActiveTurn(input));
        };

        let Some((_, active_task)) = active_turn.tasks.first() else {
            return Err(SteerInputError::NoActiveTurn(input));
        };
        let active_turn_id = active_task.turn_context.sub_id.clone();

        if let Some(expected_turn_id) = expected_turn_id
            && expected_turn_id != active_turn_id.as_str()
        {
            return Err(SteerInputError::ExpectedTurnMismatch {
                expected: expected_turn_id.to_string(),
                actual: active_turn_id,
            });
        }

        match active_task.kind {
            crate::state::TaskKind::Regular => {}
            crate::state::TaskKind::Review | crate::state::TaskKind::PostTurnCompletionReview => {
                return Err(SteerInputError::ActiveTurnNotSteerable {
                    turn_kind: NonSteerableTurnKind::Review,
                });
            }
            crate::state::TaskKind::Compact => {
                return Err(SteerInputError::ActiveTurnNotSteerable {
                    turn_kind: NonSteerableTurnKind::Compact,
                });
            }
            crate::state::TaskKind::UserShell => {
                return Err(SteerInputError::ActiveTurnNotSteerable {
                    turn_kind: NonSteerableTurnKind::UserShell,
                });
            }
        }

        let mut ts = active_turn.turn_state.lock().await;
        ts.push_pending_input(TurnInput::UserInput {
            content: input,
            client_id: client_user_message_id,
        });
        Ok(active_turn_id)
    }

    /// Returns the input if there was no task running to inject into
    pub async fn inject_response_items(
        &self,
        input: Vec<ResponseInputItem>,
    ) -> Result<(), Vec<ResponseInputItem>> {
        let mut active = self.active_turn.lock().await;
        match active.as_mut() {
            Some(at) => {
                let mut ts = at.turn_state.lock().await;
                for item in input {
                    ts.push_pending_input(TurnInput::ResponseItem(item));
                }
                Ok(())
            }
            None => Err(input),
        }
    }

    pub async fn get_pending_input(&self) -> Vec<TurnInput> {
        let mut active = self.active_turn.lock().await;
        match active.as_mut() {
            Some(at) => {
                let mut ts = at.turn_state.lock().await;
                ts.take_pending_input()
            }
            None => Vec::with_capacity(0),
        }
    }

    pub async fn has_pending_input(&self) -> bool {
        let active = self.active_turn.lock().await;
        match active.as_ref() {
            Some(at) => {
                let ts = at.turn_state.lock().await;
                ts.has_pending_input()
            }
            None => false,
        }
    }

    pub async fn interrupt_task(self: &Arc<Self>) {
        info!("interrupt received: abort current task, if any");
        let has_active_turn = { self.active_turn.lock().await.is_some() };
        if has_active_turn {
            self.abort_all_tasks(TurnAbortReason::Interrupted).await;
        } else {
            self.cancel_mcp_startup().await;
        }
    }

    pub async fn pause_task(self: &Arc<Self>) {
        info!("pause received: pause current task, if any");
        let active_task = {
            let active = self.active_turn.lock().await;
            active.as_ref().and_then(|active_turn| {
                active_turn
                    .tasks
                    .first()
                    .map(|(sub_id, task)| (sub_id.clone(), task.kind))
            })
        };
        match active_task {
            Some((_, TaskKind::Regular | TaskKind::PostTurnCompletionReview)) => {
                self.pause_all_tasks(crate::protocol::TurnPauseReason::UserRequested)
                    .await;
            }
            Some((sub_id, TaskKind::Review | TaskKind::Compact | TaskKind::UserShell)) => {
                self.send_event_raw(Event {
                    id: sub_id,
                    msg: EventMsg::Warning(WarningEvent {
                        message: "Pause is not available for this task.".to_string(),
                    }),
                })
                .await;
            }
            None => {
                self.cancel_mcp_startup().await;
            }
        }
    }

    async fn take_pending_continuation(&self) -> Option<PendingContinuation> {
        let mut state = self.state.lock().await;
        state.pending_continuation.take()
    }

    pub(crate) async fn set_pending_continuation(
        &self,
        pending_continuation: Option<PendingContinuation>,
    ) {
        let mut state = self.state.lock().await;
        state.pending_continuation = pending_continuation;
    }

    pub(crate) async fn clear_pending_continuation(&self) {
        self.set_pending_continuation(None).await;
    }

    pub(crate) async fn pending_pause_reason_for_turn(
        &self,
        turn_id: &str,
    ) -> Option<TurnPauseReason> {
        let state = self.state.lock().await;
        match state.pending_continuation.as_ref() {
            Some(PendingContinuation {
                source: TurnContinuationSource::Paused,
                continued_from_turn_id: Some(paused_turn_id),
                pause_reason,
                ..
            }) if paused_turn_id == turn_id => Some(
                pause_reason
                    .clone()
                    .unwrap_or(TurnPauseReason::UserRequested),
            ),
            Some(_) | None => None,
        }
    }

    pub(crate) async fn submit_internal_op(&self, id: String, op: Op) {
        if self.tx_sub.send(Submission { id, op }).await.is_err() {
            warn!("failed to submit internal session operation");
        }
    }

    pub(crate) async fn set_pending_post_turn_completion_review_continuation(
        &self,
        pending_continuation: Option<PendingContinuation>,
    ) {
        let mut state = self.state.lock().await;
        state.pending_post_turn_completion_review_continuation = pending_continuation;
    }

    pub(crate) async fn take_pending_post_turn_completion_review_continuation(
        &self,
    ) -> Option<PendingContinuation> {
        let mut state = self.state.lock().await;
        state
            .pending_post_turn_completion_review_continuation
            .take()
    }

    pub(crate) async fn last_completed_regular_turn_for_review(
        &self,
    ) -> Option<CompletedTurnForReview> {
        let state = self.state.lock().await;
        state.last_completed_regular_turn_for_review.clone()
    }

    pub(crate) async fn completed_turn_for_review(&self) -> Option<CompletedTurnForReview> {
        let state = self.state.lock().await;
        state
            .last_completed_regular_turn_for_review
            .clone()
            .or_else(|| {
                completed_turn_for_review_from_history(
                    state.history.raw_items(),
                    state.session_configuration.cwd.clone(),
                )
            })
    }

    pub(crate) async fn capture_completed_regular_turn_for_review(
        &self,
        turn_context: &TurnContext,
        mut user_messages: Vec<String>,
        final_agent_message: Option<&str>,
    ) {
        let Some(final_agent_message) = final_agent_message else {
            return;
        };
        if final_agent_message.trim().is_empty() {
            return;
        }

        let mut state = self.state.lock().await;
        let mut interaction_history =
            completed_turn_interaction_history_from_history(state.history.raw_items())
                .map(|(rounds, _, _)| rounds)
                .unwrap_or_default();

        if !user_messages.is_empty()
            && interaction_history
                .last()
                .is_none_or(|round| round.final_agent_message != final_agent_message)
        {
            if let Some(round) = interaction_history
                .last_mut()
                .filter(|round| round.user_messages == user_messages)
            {
                round.final_agent_message = final_agent_message.to_string();
            } else {
                interaction_history.push(CompletedTurnReviewRound {
                    user_messages: user_messages.clone(),
                    final_agent_message: final_agent_message.to_string(),
                });
            }
        }

        let Some(latest_round) = interaction_history.last().cloned() else {
            return;
        };
        if user_messages.is_empty() {
            user_messages = latest_round.user_messages;
        }

        let completed_turn = CompletedTurnForReview {
            turn_id: turn_context.sub_id.clone(),
            cwd: turn_context.cwd.clone(),
            user_messages,
            final_agent_message: final_agent_message.to_string(),
            interaction_history,
        };
        state.last_completed_regular_turn_for_review = Some(completed_turn);
    }

    async fn prepare_history_for_continuation(&self, remove_interrupted_abort: bool) {
        let mut state = self.state.lock().await;
        let mut items = state.history.raw_items().to_vec();
        let mut changed = false;
        if remove_interrupted_abort {
            changed |= remove_trailing_turn_aborted_marker(&mut items);
        }
        changed |= trim_incomplete_continuation_tail(&mut items);
        if changed {
            state.replace_history(items);
        }
    }

    async fn clean_rollout_for_continuation(&self, remove_interrupted_abort: bool) {
        self.flush_rollout().await;
        let rollout_path = {
            let guard = self.services.rollout.lock().await;
            guard
                .as_ref()
                .map(|recorder| recorder.rollout_path().to_path_buf())
        };
        let Some(rollout_path) = rollout_path else {
            return;
        };
        if let Err(err) =
            RolloutRecorder::clean_for_continue(&rollout_path, remove_interrupted_abort).await
        {
            warn!("failed to clean rollout before continuation: {err}");
        }
    }

    pub(crate) fn hooks(&self) -> &Hooks {
        &self.services.hooks
    }

    pub(crate) fn user_shell(&self) -> Arc<shell::Shell> {
        Arc::clone(&self.services.user_shell)
    }

    fn show_raw_agent_reasoning(&self) -> bool {
        self.services.show_raw_agent_reasoning
    }
}

fn skills_to_info(
    skills: &[SkillMetadata],
    disabled_paths: &HashSet<PathBuf>,
) -> Vec<ProtocolSkillMetadata> {
    skills
        .iter()
        .map(|skill| ProtocolSkillMetadata {
            name: skill.name.clone(),
            description: skill.description.clone(),
            short_description: skill.short_description.clone(),
            interface: skill
                .interface
                .clone()
                .map(|interface| ProtocolSkillInterface {
                    display_name: interface.display_name,
                    short_description: interface.short_description,
                    icon_small: interface.icon_small,
                    icon_large: interface.icon_large,
                    brand_color: interface.brand_color,
                    default_prompt: interface.default_prompt,
                }),
            dependencies: skill.dependencies.clone().map(|dependencies| {
                ProtocolSkillDependencies {
                    tools: dependencies
                        .tools
                        .into_iter()
                        .map(|tool| ProtocolSkillToolDependency {
                            r#type: tool.r#type,
                            value: tool.value,
                            description: tool.description,
                            transport: tool.transport,
                            command: tool.command,
                            url: tool.url,
                        })
                        .collect(),
                }
            }),
            path: skill.path.clone(),
            scope: skill.scope,
            enabled: !disabled_paths.contains(&skill.path),
        })
        .collect()
}

fn errors_to_info(errors: &[SkillError]) -> Vec<SkillErrorInfo> {
    errors
        .iter()
        .map(|err| SkillErrorInfo {
            path: err.path.clone(),
            message: err.message.clone(),
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests;
