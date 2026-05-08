//! The main Codex TUI chat surface.
//!
//! `ChatWidget` consumes protocol events, builds and updates history cells, and drives rendering
//! for both the main viewport and overlay UIs.
//!
//! The UI has both committed transcript cells (finalized `HistoryCell`s) and an in-flight active
//! cell (`ChatWidget.active_cell`) that can mutate in place while streaming (often representing a
//! coalesced exec/tool group). The transcript overlay (`Ctrl+T`) renders committed cells plus a
//! cached, render-only live tail derived from the current active cell so in-flight tool calls are
//! visible immediately.
//!
//! The transcript overlay is kept in sync by `App::overlay_forward_event`, which syncs a live tail
//! during draws using `active_cell_transcript_key()` and `active_cell_transcript_lines()`. The
//! cache key is designed to change when the active cell mutates in place or when its transcript
//! output is time-dependent so the overlay can refresh its cached tail without rebuilding it on
//! every draw.
//!
//! The bottom pane exposes a single "task running" indicator that drives the spinner and interrupt
//! hints. This module treats that indicator as derived UI-busy state: it is set while an agent turn
//! is in progress and while MCP server startup is in progress. Those lifecycles are tracked
//! independently (`agent_turn_running` and `mcp_startup_status`) and synchronized via
//! `update_task_running_state`.
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crate::bottom_pane::StatusLineItem;
use crate::bottom_pane::StatusLineSetupView;
use crate::status::RateLimitWindowDisplay;
use crate::status::format_directory_display;
use crate::status::format_tokens_compact;
use crate::text_formatting::proper_join;
use crate::version::CODEX_CLI_VERSION;
use codex_app_server_protocol::ConfigLayerSource;
use codex_backend_client::Client as BackendClient;
use codex_chatgpt::connectors;
use codex_core::config::Config;
use codex_core::config::ConstraintResult;
use codex_core::config::types::Notifications;
use codex_core::config_loader::ConfigLayerStackOrdering;
use codex_core::features::FEATURES;
use codex_core::features::Feature;
use codex_core::find_thread_name_by_id;
use codex_core::git_info::current_branch_name;
use codex_core::git_info::get_git_repo_root;
use codex_core::git_info::local_git_branches;
use codex_core::models_manager::manager::ModelsManager;
use codex_core::project_doc::DEFAULT_PROJECT_DOC_FILENAME;
use codex_core::protocol::AgentMessageDeltaEvent;
use codex_core::protocol::AgentMessageEvent;
use codex_core::protocol::AgentReasoningDeltaEvent;
use codex_core::protocol::AgentReasoningEvent;
use codex_core::protocol::AgentReasoningRawContentDeltaEvent;
use codex_core::protocol::AgentReasoningRawContentEvent;
use codex_core::protocol::ApplyPatchApprovalRequestEvent;
use codex_core::protocol::BackgroundEventEvent;
use codex_core::protocol::CodexErrorInfo;
use codex_core::protocol::CreditsSnapshot;
use codex_core::protocol::DeprecationNoticeEvent;
use codex_core::protocol::ErrorEvent;
use codex_core::protocol::Event;
use codex_core::protocol::EventMsg;
use codex_core::protocol::ExecApprovalRequestEvent;
use codex_core::protocol::ExecCommandBeginEvent;
use codex_core::protocol::ExecCommandEndEvent;
use codex_core::protocol::ExecCommandOutputDeltaEvent;
use codex_core::protocol::ExecCommandSource;
use codex_core::protocol::ExitedReviewModeEvent;
use codex_core::protocol::ListCustomPromptsResponseEvent;
use codex_core::protocol::ListSkillsResponseEvent;
use codex_core::protocol::McpListToolsResponseEvent;
use codex_core::protocol::McpStartupCompleteEvent;
use codex_core::protocol::McpStartupStatus;
use codex_core::protocol::McpStartupUpdateEvent;
use codex_core::protocol::McpToolCallBeginEvent;
use codex_core::protocol::McpToolCallEndEvent;
use codex_core::protocol::Op;
use codex_core::protocol::PatchApplyBeginEvent;
use codex_core::protocol::ProgressTraceCategory;
use codex_core::protocol::ProgressTraceEvent;
use codex_core::protocol::RateLimitSnapshot;
use codex_core::protocol::ReviewRequest;
use codex_core::protocol::ReviewTarget;
use codex_core::protocol::RuntimeContextDeactivatedEvent;
use codex_core::protocol::RuntimeContextSnapshot;
use codex_core::protocol::SkillMetadata as ProtocolSkillMetadata;
use codex_core::protocol::StreamErrorEvent;
use codex_core::protocol::TerminalInteractionEvent;
use codex_core::protocol::TokenUsage;
use codex_core::protocol::TokenUsageInfo;
use codex_core::protocol::TurnAbortReason;
use codex_core::protocol::TurnCompleteEvent;
use codex_core::protocol::TurnDiffEvent;
use codex_core::protocol::UndoCompletedEvent;
use codex_core::protocol::UndoStartedEvent;
use codex_core::protocol::UserMessageEvent;
use codex_core::protocol::ViewImageToolCallEvent;
use codex_core::protocol::WarningEvent;
use codex_core::protocol::WebSearchBeginEvent;
use codex_core::protocol::WebSearchEndEvent;
use codex_core::skills::model::SkillMetadata;
#[cfg(target_os = "windows")]
use codex_core::windows_sandbox::WindowsSandboxLevelExt;
use codex_otel::OtelManager;
use codex_protocol::ThreadId;
use codex_protocol::account::PlanType;
use codex_protocol::approvals::ElicitationRequestEvent;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Personality;
use codex_protocol::config_types::Settings;
#[cfg(target_os = "windows")]
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::local_image_label_text;
use codex_protocol::parse_command::ParsedCommand;
use codex_protocol::request_user_input::RequestUserInputEvent;
use codex_protocol::user_input::TextElement;
use codex_protocol::user_input::UserInput;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use rand::Rng;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tracing::debug;

const DEFAULT_MODEL_DISPLAY_NAME: &str = "loading";
const PLAN_IMPLEMENTATION_TITLE: &str = "Implement this plan?";
const PLAN_IMPLEMENTATION_YES: &str = "Yes, implement this plan";
const PLAN_IMPLEMENTATION_NO: &str = "No, stay in Plan mode";
const PLAN_IMPLEMENTATION_CODING_MESSAGE: &str = "Implement the plan.";

use crate::app_event::AppEvent;
use crate::app_event::ChatExportFormat;
use crate::app_event::ConnectorsSnapshot;
use crate::app_event::CopyCodeBlockScope;
use crate::app_event::CopyMessageFilter;
use crate::app_event::ExitMode;
use crate::app_event::ExportOverrides;
#[cfg(target_os = "windows")]
use crate::app_event::WindowsSandboxEnableMode;
use crate::app_event::WindowsSandboxFallbackReason;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::ApprovalRequest;
use crate::bottom_pane::BottomPane;
use crate::bottom_pane::BottomPaneParams;
use crate::bottom_pane::CancellationEvent;
use crate::bottom_pane::CollaborationModeIndicator;
use crate::bottom_pane::ColumnWidthMode;
use crate::bottom_pane::DOUBLE_PRESS_QUIT_SHORTCUT_ENABLED;
use crate::bottom_pane::ExperimentalFeatureItem;
use crate::bottom_pane::ExperimentalFeaturesView;
use crate::bottom_pane::FeedbackAudience;
use crate::bottom_pane::InputResult;
use crate::bottom_pane::LocalImageAttachment;
use crate::bottom_pane::QUIT_SHORTCUT_TIMEOUT;
use crate::bottom_pane::QueuePopup;
use crate::bottom_pane::QueuePopupItem;
use crate::bottom_pane::SelectionAction;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::custom_prompt_view::CustomPromptView;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use crate::clipboard_paste::PasteImageError;
use crate::clipboard_paste::copy_text_to_clipboard;
use crate::clipboard_paste::paste_image_to_temp_png;
use crate::clipboard_paste::paste_text_from_clipboard;
use crate::clipboard_text;
use crate::collab;
use crate::collaboration_modes;
use crate::diff_render::display_path_for;
use crate::exec_cell::CommandOutput;
use crate::exec_cell::ExecCell;
use crate::exec_cell::new_active_exec_command;
use crate::exec_command::strip_bash_lc_and_escape;
use crate::export_markdown;
use crate::get_git_diff::GitDiffResult;
use crate::get_git_diff::get_git_diff;
use crate::history_cell;
use crate::history_cell::AgentMessageCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::McpToolCallCell;
use crate::history_cell::PlainHistoryCell;
use crate::history_cell::WebSearchCell;
use crate::key_hint;
use crate::key_hint::KeyBinding;
use crate::keybindings::Keybindings;
use crate::markdown::append_markdown;
use crate::render::Insets;
use crate::render::renderable::ColumnRenderable;
use crate::render::renderable::FlexRenderable;
use crate::render::renderable::Renderable;
use crate::render::renderable::RenderableExt;
use crate::render::renderable::RenderableItem;
use crate::slash_command::SlashCommand;
use crate::status::RateLimitSnapshotDisplay;
use crate::text_formatting::truncate_text;
use crate::tui::FrameRequester;
mod interrupts;
use self::interrupts::InterruptManager;
mod agent;
use self::agent::spawn_agent;
use self::agent::spawn_agent_from_existing;
pub(crate) use self::agent::spawn_op_forwarder;
mod session_header;
use self::session_header::SessionHeader;
mod skills;
use self::skills::collect_tool_mentions;
use self::skills::find_app_mentions;
use self::skills::find_skill_mentions_with_tool_mentions;
use crate::progress_trace_style::ProgressTraceStyles;
use crate::progress_trace_style::progress_trace_category_label;
use crate::progress_trace_style::progress_trace_style_description;
use crate::progress_trace_style::progress_trace_style_for_category;
use crate::progress_trace_style::resolve_progress_trace_styles;
use crate::streaming::chunking::AdaptiveChunkingPolicy;
use crate::streaming::commit_tick::CommitTickScope;
use crate::streaming::commit_tick::run_commit_tick;
use crate::streaming::controller::PlanStreamController;
use crate::streaming::controller::StreamController;

use chrono::Local;
use codex_common::approval_presets::ApprovalPreset;
use codex_common::approval_presets::builtin_approval_presets;
use codex_core::AuthManager;
use codex_core::CodexAuth;
use codex_core::ThreadManager;
use codex_core::config::types::CopyUiMode;
use codex_core::config::types::DiffView;
use codex_core::config::types::ProgressLegendMode;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::SandboxPolicy;
use codex_file_search::FileMatch;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::plan_tool::UpdatePlanArgs;
use strum::IntoEnumIterator;

const USER_SHELL_COMMAND_HELP_TITLE: &str = "Prefix a command with ! to run it locally";
const USER_SHELL_COMMAND_HELP_HINT: &str = "Example: !ls";
const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
// Track information about an in-flight exec command.
struct RunningCommand {
    command: Vec<String>,
    parsed_cmd: Vec<ParsedCommand>,
    source: ExecCommandSource,
}

struct UnifiedExecProcessSummary {
    key: String,
    call_id: String,
    command_display: String,
    recent_chunks: Vec<String>,
}

struct UnifiedExecWaitState {
    command_display: String,
}

impl UnifiedExecWaitState {
    fn new(command_display: String) -> Self {
        Self { command_display }
    }

    fn is_duplicate(&self, command_display: &str) -> bool {
        self.command_display == command_display
    }
}

#[derive(Clone, Debug)]
struct UnifiedExecWaitStreak {
    process_id: String,
    command_display: Option<String>,
}

impl UnifiedExecWaitStreak {
    fn new(process_id: String, command_display: Option<String>) -> Self {
        Self {
            process_id,
            command_display: command_display.filter(|display| !display.is_empty()),
        }
    }

    fn update_command_display(&mut self, command_display: Option<String>) {
        if self.command_display.is_some() {
            return;
        }
        self.command_display = command_display.filter(|display| !display.is_empty());
    }
}

fn is_unified_exec_source(source: ExecCommandSource) -> bool {
    matches!(
        source,
        ExecCommandSource::UnifiedExecStartup | ExecCommandSource::UnifiedExecInteraction
    )
}

fn is_standard_tool_call(parsed_cmd: &[ParsedCommand]) -> bool {
    !parsed_cmd.is_empty()
        && parsed_cmd
            .iter()
            .all(|parsed| !matches!(parsed, ParsedCommand::Unknown { .. }))
}

const RATE_LIMIT_WARNING_THRESHOLDS: [f64; 3] = [75.0, 90.0, 95.0];
const NUDGE_MODEL_SLUG: &str = "gpt-5.4-mini";
const RATE_LIMIT_SWITCH_PROMPT_THRESHOLD: f64 = 90.0;

#[derive(Default)]
struct RateLimitWarningState {
    secondary_index: usize,
    primary_index: usize,
}

impl RateLimitWarningState {
    fn take_warnings(
        &mut self,
        secondary_used_percent: Option<f64>,
        secondary_window_minutes: Option<i64>,
        primary_used_percent: Option<f64>,
        primary_window_minutes: Option<i64>,
    ) -> Vec<String> {
        let reached_secondary_cap =
            matches!(secondary_used_percent, Some(percent) if percent == 100.0);
        let reached_primary_cap = matches!(primary_used_percent, Some(percent) if percent == 100.0);
        if reached_secondary_cap || reached_primary_cap {
            return Vec::new();
        }

        let mut warnings = Vec::new();

        if let Some(secondary_used_percent) = secondary_used_percent {
            let mut highest_secondary: Option<f64> = None;
            while self.secondary_index < RATE_LIMIT_WARNING_THRESHOLDS.len()
                && secondary_used_percent >= RATE_LIMIT_WARNING_THRESHOLDS[self.secondary_index]
            {
                highest_secondary = Some(RATE_LIMIT_WARNING_THRESHOLDS[self.secondary_index]);
                self.secondary_index += 1;
            }
            if let Some(threshold) = highest_secondary {
                let limit_label = secondary_window_minutes
                    .map(get_limits_duration)
                    .unwrap_or_else(|| "weekly".to_string());
                let remaining_percent = 100.0 - threshold;
                warnings.push(format!(
                    "Heads up, you have less than {remaining_percent:.0}% of your {limit_label} limit left. Run /status for a breakdown."
                ));
            }
        }

        if let Some(primary_used_percent) = primary_used_percent {
            let mut highest_primary: Option<f64> = None;
            while self.primary_index < RATE_LIMIT_WARNING_THRESHOLDS.len()
                && primary_used_percent >= RATE_LIMIT_WARNING_THRESHOLDS[self.primary_index]
            {
                highest_primary = Some(RATE_LIMIT_WARNING_THRESHOLDS[self.primary_index]);
                self.primary_index += 1;
            }
            if let Some(threshold) = highest_primary {
                let limit_label = primary_window_minutes
                    .map(get_limits_duration)
                    .unwrap_or_else(|| "5h".to_string());
                let remaining_percent = 100.0 - threshold;
                warnings.push(format!(
                    "Heads up, you have less than {remaining_percent:.0}% of your {limit_label} limit left. Run /status for a breakdown."
                ));
            }
        }

        warnings
    }
}

pub(crate) fn get_limits_duration(windows_minutes: i64) -> String {
    const MINUTES_PER_HOUR: i64 = 60;
    const MINUTES_PER_DAY: i64 = 24 * MINUTES_PER_HOUR;
    const MINUTES_PER_WEEK: i64 = 7 * MINUTES_PER_DAY;
    const MINUTES_PER_MONTH: i64 = 30 * MINUTES_PER_DAY;
    const ROUNDING_BIAS_MINUTES: i64 = 3;

    let windows_minutes = windows_minutes.max(0);

    if windows_minutes <= MINUTES_PER_DAY.saturating_add(ROUNDING_BIAS_MINUTES) {
        let adjusted = windows_minutes.saturating_add(ROUNDING_BIAS_MINUTES);
        let hours = std::cmp::max(1, adjusted / MINUTES_PER_HOUR);
        format!("{hours}h")
    } else if windows_minutes <= MINUTES_PER_WEEK.saturating_add(ROUNDING_BIAS_MINUTES) {
        "weekly".to_string()
    } else if windows_minutes <= MINUTES_PER_MONTH.saturating_add(ROUNDING_BIAS_MINUTES) {
        "monthly".to_string()
    } else {
        "annual".to_string()
    }
}

/// Common initialization parameters shared by all `ChatWidget` constructors.
pub(crate) struct ChatWidgetInit {
    pub(crate) config: Config,
    pub(crate) frame_requester: FrameRequester,
    pub(crate) app_event_tx: AppEventSender,
    pub(crate) initial_user_message: Option<UserMessage>,
    pub(crate) enhanced_keys_supported: bool,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) models_manager: Arc<ModelsManager>,
    pub(crate) feedback: codex_feedback::CodexFeedback,
    pub(crate) is_first_run: bool,
    pub(crate) feedback_audience: FeedbackAudience,
    pub(crate) model: Option<String>,
    // Shared latch so we only warn once about invalid status-line item IDs.
    pub(crate) status_line_invalid_items_warned: Arc<AtomicBool>,
    pub(crate) otel_manager: OtelManager,
}

#[derive(Default)]
enum RateLimitSwitchPromptState {
    #[default]
    Idle,
    Pending,
    Shown,
}

#[derive(Debug, Clone, Default)]
enum ConnectorsCacheState {
    #[default]
    Uninitialized,
    Loading,
    Ready(ConnectorsSnapshot),
    Failed(String),
}

#[derive(Debug)]
enum RateLimitErrorKind {
    ModelCap {
        model: String,
        reset_after_seconds: Option<u64>,
    },
    UsageLimit,
    Generic,
}

fn rate_limit_error_kind(info: &CodexErrorInfo) -> Option<RateLimitErrorKind> {
    match info {
        CodexErrorInfo::ModelCap {
            model,
            reset_after_seconds,
        } => Some(RateLimitErrorKind::ModelCap {
            model: model.clone(),
            reset_after_seconds: *reset_after_seconds,
        }),
        CodexErrorInfo::UsageLimitExceeded => Some(RateLimitErrorKind::UsageLimit),
        CodexErrorInfo::ResponseTooManyFailedAttempts {
            http_status_code: Some(429),
        } => Some(RateLimitErrorKind::Generic),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ExternalEditorState {
    #[default]
    Closed,
    Requested,
    Active,
}

/// Maintains the per-session UI state and interaction state machines for the chat screen.
///
/// `ChatWidget` owns the state derived from the protocol event stream (history cells, streaming
/// buffers, bottom-pane overlays, and transient status text) and turns key presses into user
/// intent (`Op` submissions and `AppEvent` requests).
///
/// It is not responsible for running the agent itself; it reflects progress by updating UI state
/// and by sending requests back to codex-core.
///
/// Quit/interrupt behavior intentionally spans layers: the bottom pane owns local input routing
/// (which view gets Ctrl+C), while `ChatWidget` owns process-level decisions such as interrupting
/// active work, arming the double-press quit shortcut, and requesting shutdown-first exit.
pub(crate) struct ChatWidget {
    app_event_tx: AppEventSender,
    codex_op_tx: UnboundedSender<Op>,
    bottom_pane: BottomPane,
    active_cell: Option<Box<dyn HistoryCell>>,
    /// Monotonic-ish counter used to invalidate transcript overlay caching.
    ///
    /// The transcript overlay appends a cached "live tail" for the current active cell. Most
    /// active-cell updates are mutations of the *existing* cell (not a replacement), so pointer
    /// identity alone is not a good cache key.
    ///
    /// Callers bump this whenever the active cell's transcript output could change without
    /// flushing. It is intentionally allowed to wrap, which implies a rare one-time cache collision
    /// where the overlay may briefly treat new tail content as already cached.
    active_cell_revision: u64,
    config: Config,
    keybindings: Keybindings,
    /// The unmasked collaboration mode settings (always Default mode).
    ///
    /// Masks are applied on top of this base mode to derive the effective mode.
    current_collaboration_mode: CollaborationMode,
    /// The currently active collaboration mask, if any.
    active_collaboration_mask: Option<CollaborationModeMask>,
    auth_manager: Arc<AuthManager>,
    models_manager: Arc<ModelsManager>,
    otel_manager: OtelManager,
    session_header: SessionHeader,
    initial_user_message: Option<UserMessage>,
    token_info: Option<TokenUsageInfo>,
    rate_limit_snapshot: Option<RateLimitSnapshotDisplay>,
    plan_type: Option<PlanType>,
    rate_limit_warnings: RateLimitWarningState,
    rate_limit_switch_prompt: RateLimitSwitchPromptState,
    rate_limit_poller: Option<JoinHandle<()>>,
    adaptive_chunking: AdaptiveChunkingPolicy,
    // Stream lifecycle controller
    stream_controller: Option<StreamController>,
    // Stream lifecycle controller for proposed plan output.
    plan_stream_controller: Option<PlanStreamController>,
    // Latest completed user-visible Codex output that `/copy` should place on the clipboard.
    last_copyable_output: Option<String>,
    running_commands: HashMap<String, RunningCommand>,
    suppressed_exec_calls: HashSet<String>,
    skills_all: Vec<ProtocolSkillMetadata>,
    skills_initial_state: Option<HashMap<PathBuf, bool>>,
    last_unified_wait: Option<UnifiedExecWaitState>,
    unified_exec_wait_streak: Option<UnifiedExecWaitStreak>,
    task_complete_pending: bool,
    unified_exec_processes: Vec<UnifiedExecProcessSummary>,
    /// Tracks whether codex-core currently considers an agent turn to be in progress.
    ///
    /// This is kept separate from `mcp_startup_status` so that MCP startup progress (or completion)
    /// can update the status header without accidentally clearing the spinner for an active turn.
    agent_turn_running: bool,
    /// Tracks per-server MCP startup state while startup is in progress.
    ///
    /// The map is `Some(_)` from the first `McpStartupUpdate` until `McpStartupComplete`, and the
    /// bottom pane is treated as "running" while this is populated, even if no agent turn is
    /// currently executing.
    mcp_startup_status: Option<HashMap<String, McpStartupStatus>>,
    connectors_cache: ConnectorsCacheState,
    // Queue of interruptive UI events deferred during an active write cycle
    interrupts: InterruptManager,
    // Accumulates the current reasoning block text to extract a header
    reasoning_buffer: String,
    // Accumulates full reasoning content for transcript-only recording
    full_reasoning_buffer: String,
    // Current status header shown in the status indicator.
    current_status_header: String,
    // Previous status header to restore after a transient stream retry.
    retry_status_header: Option<String>,
    // Runtime context currently surfaced as the active status subject.
    active_runtime_context: Option<RuntimeContextSnapshot>,
    // Model used by the currently running turn.
    running_turn_model: Option<String>,
    // Reasoning effort used by the currently running turn.
    running_turn_reasoning_effort: Option<ReasoningEffortConfig>,
    thread_id: Option<ThreadId>,
    thread_name: Option<String>,
    has_completed_assistant_message: bool,
    last_assistant_output_markdown: Option<String>,
    copyable_messages: Vec<CopyableMessage>,
    copy_code_ui_state: Option<CopyCodeUiState>,
    copy_message_ui_state: Option<CopyMessageUiState>,
    forked_from: Option<ThreadId>,
    frame_requester: FrameRequester,
    // Whether to include the initial welcome banner on session configured
    show_welcome_banner: bool,
    // When resuming an existing session (selected via resume picker), avoid an
    // immediate redraw on SessionConfigured to prevent a gratuitous UI flicker.
    suppress_session_configured_redraw: bool,
    // User messages queued while a turn is in progress
    queued_user_messages: VecDeque<QueuedUserMessage>,
    next_queued_user_message_id: u64,
    queued_edit_state: Option<QueuedEditState>,
    // Pending notification to show when unfocused on next Draw
    pending_notification: Option<Notification>,
    /// When `Some`, the user has pressed a quit shortcut and the second press
    /// must occur before `quit_shortcut_expires_at`.
    quit_shortcut_expires_at: Option<Instant>,
    /// Tracks which quit shortcut key was pressed first.
    ///
    /// We require the second press to match this key so `Ctrl+C` followed by
    /// `Ctrl+D` (or vice versa) doesn't quit accidentally.
    quit_shortcut_key: Option<KeyBinding>,
    // Simple review mode flag; used to adjust layout and banners.
    is_review_mode: bool,
    // Snapshot of token usage to restore after review mode exits.
    pre_review_token_info: Option<Option<TokenUsageInfo>>,
    // Whether the next streamed assistant content should be preceded by a final message separator.
    //
    // This is set whenever we insert a visible history cell that conceptually belongs to a turn.
    // The separator itself is only rendered if the turn recorded "work" activity (see
    // `had_work_activity`).
    needs_final_message_separator: bool,
    // Whether the current turn performed "work" (exec commands, MCP tool calls, patch applications).
    //
    // This gates rendering of the "Worked for …" separator so purely conversational turns don't
    // show an empty divider. It is reset when the separator is emitted.
    had_work_activity: bool,
    // Progress trace categories collected during the active turn.
    turn_progress_trace: Vec<ProgressTraceCategory>,
    // Completed turn trace held until the next separator emission.
    pending_separator_progress_trace: Option<Vec<ProgressTraceCategory>>,
    // Configured legend visibility mode for the status indicator.
    progress_legend_mode: ProgressLegendMode,
    // Resolved per-category timeline styles.
    progress_trace_styles: ProgressTraceStyles,
    // Whether the current turn emitted a plan update.
    saw_plan_update_this_turn: bool,
    // Whether the current turn emitted a proposed plan item.
    saw_plan_item_this_turn: bool,
    // Incremental buffer for streamed plan content.
    plan_delta_buffer: String,
    // True while a plan item is streaming.
    plan_item_active: bool,
    // Status-indicator elapsed seconds captured at the last emitted final-message separator.
    //
    // This lets the separator show per-chunk work time (since the previous separator) rather than
    // the total task-running time reported by the status indicator.
    last_separator_elapsed_secs: Option<u64>,

    last_rendered_width: std::cell::Cell<Option<usize>>,
    // Feedback sink for /feedback
    feedback: codex_feedback::CodexFeedback,
    feedback_audience: FeedbackAudience,
    // Current session rollout path (if known)
    current_rollout_path: Option<PathBuf>,
    // Current working directory (if known)
    current_cwd: Option<PathBuf>,
    // Shared latch so we only warn once about invalid status-line item IDs.
    status_line_invalid_items_warned: Arc<AtomicBool>,
    // Cached git branch name for the status line (None if unknown).
    status_line_branch: Option<String>,
    // CWD used to resolve the cached branch; change resets branch state.
    status_line_branch_cwd: Option<PathBuf>,
    // True while an async branch lookup is in flight.
    status_line_branch_pending: bool,
    // True once we've attempted a branch lookup for the current CWD.
    status_line_branch_lookup_complete: bool,
    external_editor_state: ExternalEditorState,
}

/// Snapshot of active-cell state that affects transcript overlay rendering.
///
/// The overlay keeps a cached "live tail" for the in-flight cell; this key lets
/// it cheaply decide when to recompute that tail as the active cell evolves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ActiveCellTranscriptKey {
    /// Cache-busting revision for in-place updates.
    ///
    /// Many active cells are updated incrementally while streaming (for example when exec groups
    /// add output or change status), and the transcript overlay caches its live tail, so this
    /// revision gives a cheap way to say "same active cell, but its transcript output is different
    /// now". Callers bump it on any mutation that can affect `HistoryCell::transcript_lines`.
    pub(crate) revision: u64,
    /// Whether the active cell continues the prior stream, which affects
    /// spacing between transcript blocks.
    pub(crate) is_stream_continuation: bool,
    /// Optional animation tick for time-dependent transcript output.
    ///
    /// When this changes, the overlay recomputes the cached tail even if the revision and width
    /// are unchanged, which is how shimmer/spinner visuals can animate in the overlay without any
    /// underlying data change.
    pub(crate) animation_tick: Option<u64>,
}

pub(crate) struct UserMessage {
    text: String,
    local_images: Vec<LocalImageAttachment>,
    text_elements: Vec<TextElement>,
    mention_paths: HashMap<String, String>,
}

#[derive(Clone, Debug)]
struct QueuedUserMessage {
    id: u64,
    text: String,
    local_images: Vec<LocalImageAttachment>,
    text_elements: Vec<TextElement>,
    mention_paths: HashMap<String, String>,
    model_override: Option<String>,
    effort_override: Option<Option<ReasoningEffortConfig>>,
}

#[derive(Clone)]
struct QueuedComposerSnapshot {
    text: String,
    text_elements: Vec<TextElement>,
    local_images: Vec<LocalImageAttachment>,
    mention_paths: HashMap<String, String>,
}

#[derive(Clone)]
struct QueuedUserMessageDraft {
    text: String,
    text_elements: Vec<TextElement>,
    local_images: Vec<LocalImageAttachment>,
    mention_paths: HashMap<String, String>,
    model_override: Option<String>,
    effort_override: Option<Option<ReasoningEffortConfig>>,
}

struct QueuedEditState {
    selected_id: u64,
    composer_before_edit: QueuedComposerSnapshot,
    drafts: HashMap<u64, QueuedUserMessageDraft>,
}

impl From<String> for UserMessage {
    fn from(text: String) -> Self {
        Self {
            text,
            local_images: Vec::new(),
            // Plain text conversion has no UI element ranges.
            text_elements: Vec::new(),
            mention_paths: HashMap::new(),
        }
    }
}

impl From<&str> for UserMessage {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_string(),
            local_images: Vec::new(),
            // Plain text conversion has no UI element ranges.
            text_elements: Vec::new(),
            mention_paths: HashMap::new(),
        }
    }
}

pub(crate) fn create_initial_user_message(
    text: Option<String>,
    local_image_paths: Vec<PathBuf>,
    text_elements: Vec<TextElement>,
) -> Option<UserMessage> {
    let text = text.unwrap_or_default();
    if text.is_empty() && local_image_paths.is_empty() {
        None
    } else {
        let local_images = local_image_paths
            .into_iter()
            .enumerate()
            .map(|(idx, path)| LocalImageAttachment {
                placeholder: local_image_label_text(idx + 1),
                path,
            })
            .collect();
        Some(UserMessage {
            text,
            local_images,
            text_elements,
            mention_paths: HashMap::new(),
        })
    }
}

impl ChatWidget {
    /// Synchronize the bottom-pane "task running" indicator with the current lifecycles.
    ///
    /// The bottom pane only has one running flag, but this module treats it as a derived state of
    /// both the agent turn lifecycle and MCP startup lifecycle.
    fn update_task_running_state(&mut self) {
        self.bottom_pane
            .set_task_running(self.agent_turn_running || self.mcp_startup_status.is_some());
        if self.agent_turn_running {
            let active_model = self
                .active_runtime_context
                .as_ref()
                .and_then(|snapshot| {
                    (!snapshot.model.trim().is_empty()).then(|| snapshot.model.clone())
                })
                .or_else(|| self.running_turn_model.clone());
            let active_reasoning_effort = self
                .active_runtime_context
                .as_ref()
                .map(|snapshot| snapshot.reasoning_effort)
                .unwrap_or(self.running_turn_reasoning_effort);
            self.bottom_pane.set_active_model(active_model);
            self.bottom_pane
                .set_active_reasoning_effort(active_reasoning_effort);
        } else {
            self.bottom_pane.set_active_model(None);
            self.bottom_pane.set_active_reasoning_effort(None);
        }
    }

    fn restore_reasoning_status_header(&mut self) {
        if let Some(header) = extract_first_bold(&self.reasoning_buffer) {
            self.set_status_header(header);
        } else if self.bottom_pane.is_task_running() {
            self.set_status_header(String::from("Working"));
        }
    }

    fn flush_unified_exec_wait_streak(&mut self) {
        let Some(wait) = self.unified_exec_wait_streak.take() else {
            return;
        };
        self.needs_final_message_separator = true;
        let cell = history_cell::new_unified_exec_interaction(wait.command_display, String::new());
        self.app_event_tx
            .send(AppEvent::InsertHistoryCell(Box::new(cell)));
        self.restore_reasoning_status_header();
    }

    fn flush_answer_stream_with_separator(&mut self) {
        if let Some(mut controller) = self.stream_controller.take()
            && let Some(cell) = controller.finalize()
        {
            self.add_boxed_history(cell);
        }
        self.adaptive_chunking.reset();
    }

    /// Update the status indicator header and details.
    ///
    /// Passing `None` clears any existing details.
    fn set_status(&mut self, header: String, details: Option<String>) {
        self.current_status_header = header.clone();
        self.bottom_pane.update_status(header, details);
    }

    /// Convenience wrapper around [`Self::set_status`];
    /// updates the status indicator header and clears any existing details.
    fn set_status_header(&mut self, header: String) {
        self.set_status(header, None);
    }

    /// Sets the currently rendered footer status-line value and schedules a redraw.
    pub(crate) fn set_status_line(&mut self, status_line: Option<Line<'static>>) {
        self.bottom_pane.set_status_line(status_line);
        self.request_redraw();
    }

    /// Recomputes footer status-line content from config and current runtime state.
    ///
    /// This method is the status-line orchestrator: it parses configured item identifiers,
    /// warns once per session about invalid items, updates whether status-line mode is enabled,
    /// schedules async git-branch lookup when needed, and renders only values that are currently
    /// available.
    ///
    /// The omission behavior is intentional. If selected items are unavailable (for example before
    /// a session id exists or before branch lookup completes), those items are skipped without
    /// placeholders so the line remains compact and stable.
    pub(crate) fn refresh_status_line(&mut self) {
        let (items, invalid_items) = self.status_line_items_with_invalids();
        if self.thread_id.is_some()
            && !invalid_items.is_empty()
            && self
                .status_line_invalid_items_warned
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let label = if invalid_items.len() == 1 {
                "item"
            } else {
                "items"
            };
            let message = format!(
                "Ignored invalid status line {label}: {}.",
                proper_join(invalid_items.as_slice())
            );
            self.on_warning(message);
        }
        if !items.contains(&StatusLineItem::GitBranch) {
            self.status_line_branch = None;
            self.status_line_branch_pending = false;
            self.status_line_branch_lookup_complete = false;
        }
        let enabled = !items.is_empty();
        self.bottom_pane.set_status_line_enabled(enabled);
        if !enabled {
            self.set_status_line(None);
            return;
        }

        let cwd = self.status_line_cwd().to_path_buf();
        self.sync_status_line_branch_state(&cwd);

        if items.contains(&StatusLineItem::GitBranch) && !self.status_line_branch_lookup_complete {
            self.request_status_line_branch(cwd);
        }

        let mut parts = Vec::new();
        for item in items {
            if let Some(value) = self.status_line_value_for_item(&item) {
                parts.push(value);
            }
        }

        let line = if parts.is_empty() {
            None
        } else {
            Some(Line::from(parts.join(" · ")))
        };
        self.set_status_line(line);
    }

    /// Records that status-line setup was canceled.
    ///
    /// Cancellation is intentionally side-effect free for config state; the existing configuration
    /// remains active and no persistence is attempted.
    pub(crate) fn cancel_status_line_setup(&self) {
        tracing::info!("Status line setup canceled by user");
    }

    /// Applies status-line item selection from the setup view to in-memory config.
    ///
    /// We persist an explicit empty list when all items are deselected so "unset"
    /// (`None`) can retain the built-in default footer status line configuration.
    pub(crate) fn setup_status_line(&mut self, items: Vec<StatusLineItem>) {
        tracing::info!("status line setup confirmed with items: {items:#?}");
        let ids = items.iter().map(ToString::to_string).collect::<Vec<_>>();
        self.config.tui_status_line = Some(ids);
        self.refresh_status_line();
    }

    pub(crate) fn set_progress_legend_mode(&mut self, mode: ProgressLegendMode) {
        self.progress_legend_mode = mode;
        self.config.tui_progress_legend_mode = mode;
        self.bottom_pane.set_progress_legend_mode(mode);
    }

    /// Stores async git-branch lookup results for the current status-line cwd.
    ///
    /// Results are dropped when they target an out-of-date cwd to avoid rendering stale branch
    /// names after directory changes.
    pub(crate) fn set_status_line_branch(&mut self, cwd: PathBuf, branch: Option<String>) {
        if self.status_line_branch_cwd.as_ref() != Some(&cwd) {
            self.status_line_branch_pending = false;
            return;
        }
        self.status_line_branch = branch;
        self.status_line_branch_pending = false;
        self.status_line_branch_lookup_complete = true;
    }

    /// Forces a new git-branch lookup when `GitBranch` is part of the configured status line.
    fn request_status_line_branch_refresh(&mut self) {
        let (items, _) = self.status_line_items_with_invalids();
        if items.is_empty() || !items.contains(&StatusLineItem::GitBranch) {
            return;
        }
        let cwd = self.status_line_cwd().to_path_buf();
        self.sync_status_line_branch_state(&cwd);
        self.request_status_line_branch(cwd);
    }

    fn restore_retry_status_header_if_present(&mut self) {
        if let Some(header) = self.retry_status_header.take() {
            self.set_status_header(header);
        }
    }

    // --- Small event handlers ---
    fn on_session_configured(&mut self, event: codex_core::protocol::SessionConfiguredEvent) {
        self.bottom_pane
            .set_history_metadata(event.history_log_id, event.history_entry_count);
        self.set_skills(None);
        self.bottom_pane.set_connectors_snapshot(None);
        self.thread_id = Some(event.session_id);
        self.thread_name = event.thread_name.clone();
        self.forked_from = event.forked_from_id;
        self.current_rollout_path = event.rollout_path.clone();
        self.current_cwd = Some(event.cwd.clone());
        self.copyable_messages.clear();
        self.active_runtime_context = None;
        self.running_turn_model = None;
        self.running_turn_reasoning_effort = None;
        let initial_messages = event.initial_messages.clone();
        let forked_from_id = event.forked_from_id;
        self.last_copyable_output = None;
        let model_for_header = event.model.clone();
        self.session_header.set_model(&model_for_header);
        self.current_collaboration_mode = self.current_collaboration_mode.with_updates(
            Some(model_for_header.clone()),
            Some(event.reasoning_effort),
            None,
        );
        self.refresh_model_display();
        self.sync_personality_command_enabled();
        let session_info_cell = history_cell::new_session_info(
            &self.config,
            &model_for_header,
            event,
            self.show_welcome_banner,
            self.auth_manager
                .auth_cached()
                .and_then(|auth| auth.account_plan_type()),
        );
        self.apply_session_info_cell(session_info_cell);

        if let Some(messages) = initial_messages {
            self.replay_initial_messages(messages);
        }
        // Ask codex-core to enumerate custom prompts for this session.
        self.submit_op(Op::ListCustomPrompts);
        self.submit_op(Op::ListSkills {
            cwds: Vec::new(),
            force_reload: true,
        });
        if self.connectors_enabled() {
            self.prefetch_connectors();
        }
        if let Some(user_message) = self.initial_user_message.take() {
            self.submit_user_message(user_message);
        }
        if let Some(forked_from_id) = forked_from_id {
            self.emit_forked_thread_event(forked_from_id);
        }
        if !self.suppress_session_configured_redraw {
            self.request_redraw();
        }
    }

    fn emit_forked_thread_event(&self, forked_from_id: ThreadId) {
        let app_event_tx = self.app_event_tx.clone();
        let codex_home = self.config.codex_home.clone();
        tokio::spawn(async move {
            let forked_from_id_text = forked_from_id.to_string();
            let send_name_and_id = |name: String| {
                let line: Line<'static> = vec![
                    "• ".dim(),
                    "Thread forked from ".into(),
                    name.cyan(),
                    " (".into(),
                    forked_from_id_text.clone().cyan(),
                    ")".into(),
                ]
                .into();
                app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
                    PlainHistoryCell::new(vec![line]),
                )));
            };
            let send_id_only = || {
                let line: Line<'static> = vec![
                    "• ".dim(),
                    "Thread forked from ".into(),
                    forked_from_id_text.clone().cyan(),
                ]
                .into();
                app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
                    PlainHistoryCell::new(vec![line]),
                )));
            };

            match find_thread_name_by_id(&codex_home, &forked_from_id).await {
                Ok(Some(name)) if !name.trim().is_empty() => {
                    send_name_and_id(name);
                }
                Ok(_) => send_id_only(),
                Err(err) => {
                    tracing::warn!("Failed to read forked thread name: {err}");
                    send_id_only();
                }
            }
        });
    }

    fn on_thread_name_updated(&mut self, event: codex_core::protocol::ThreadNameUpdatedEvent) {
        if self.thread_id == Some(event.thread_id) {
            self.thread_name = event.thread_name.clone();
            let message = match event.thread_name.as_deref() {
                Some(thread_name) => format!("Renamed thread name to \"{thread_name}\"."),
                None => "Cleared thread name.".to_string(),
            };
            self.add_info_message(message, None);
            self.request_redraw();
        }
    }

    fn on_runtime_context_activated(&mut self, snapshot: RuntimeContextSnapshot) {
        self.active_runtime_context = Some(snapshot);
        self.refresh_status_subject();
    }

    fn on_runtime_context_updated(&mut self, snapshot: RuntimeContextSnapshot) {
        self.active_runtime_context = Some(snapshot);
        self.refresh_status_subject();
    }

    fn on_runtime_context_deactivated(&mut self, event: RuntimeContextDeactivatedEvent) {
        if self
            .active_runtime_context
            .as_ref()
            .is_some_and(|snapshot| snapshot.scope_id == event.scope_id)
        {
            self.active_runtime_context = None;
            self.refresh_status_subject();
        }
    }

    fn refresh_status_subject(&mut self) {
        self.sync_context_window_indicator();
        self.refresh_status_line();
        self.update_task_running_state();
        self.request_redraw();
    }

    fn set_skills(&mut self, skills: Option<Vec<SkillMetadata>>) {
        self.bottom_pane.set_skills(skills);
    }

    pub(crate) fn open_feedback_note(
        &mut self,
        category: crate::app_event::FeedbackCategory,
        include_logs: bool,
    ) {
        // Build a fresh snapshot at the time of opening the note overlay.
        let snapshot = self.feedback.snapshot(self.thread_id);
        let rollout = if include_logs {
            self.current_rollout_path.clone()
        } else {
            None
        };
        let view = crate::bottom_pane::FeedbackNoteView::new(
            category,
            snapshot,
            rollout,
            self.app_event_tx.clone(),
            include_logs,
            self.feedback_audience,
        );
        self.bottom_pane.show_view(Box::new(view));
        self.request_redraw();
    }

    pub(crate) fn open_app_link_view(
        &mut self,
        title: String,
        description: Option<String>,
        instructions: String,
        url: String,
        is_installed: bool,
    ) {
        let view = crate::bottom_pane::AppLinkView::new(
            title,
            description,
            instructions,
            url,
            is_installed,
        );
        self.bottom_pane.show_view(Box::new(view));
        self.request_redraw();
    }

    pub(crate) fn open_feedback_consent(&mut self, category: crate::app_event::FeedbackCategory) {
        let params = crate::bottom_pane::feedback_upload_consent_params(
            self.app_event_tx.clone(),
            category,
            self.current_rollout_path.clone(),
        );
        self.bottom_pane.show_selection_view(params);
        self.request_redraw();
    }

    fn on_agent_message(&mut self, message: String, from_replay: bool) {
        if !message.trim().is_empty() {
            self.has_completed_assistant_message = true;
            self.last_assistant_output_markdown = Some(message.clone());
            if from_replay {
                self.push_copyable_message(CopyableRole::Response, &message);
            }
        }

        // If we have a stream_controller, then the final agent message is redundant and will be a
        // duplicate of what has already been streamed.
        if self.stream_controller.is_none() && !message.is_empty() {
            self.handle_streaming_delta(message);
        }
        self.flush_answer_stream_with_separator();
        self.handle_stream_finished();
        self.request_redraw();
    }

    fn on_agent_message_delta(&mut self, delta: String) {
        self.handle_streaming_delta(delta);
    }

    fn on_plan_delta(&mut self, delta: String) {
        if self.active_mode_kind() != ModeKind::Plan {
            return;
        }
        if !self.plan_item_active {
            self.plan_item_active = true;
            self.plan_delta_buffer.clear();
        }
        self.plan_delta_buffer.push_str(&delta);
        // Before streaming plan content, flush any active exec cell group.
        self.flush_unified_exec_wait_streak();
        self.flush_active_cell();

        if self.plan_stream_controller.is_none() {
            self.plan_stream_controller = Some(PlanStreamController::new(
                self.last_rendered_width.get().map(|w| w.saturating_sub(4)),
            ));
        }
        if let Some(controller) = self.plan_stream_controller.as_mut()
            && controller.push(&delta)
        {
            self.app_event_tx.send(AppEvent::StartCommitAnimation);
            self.run_catch_up_commit_tick();
        }
        self.request_redraw();
    }

    fn on_plan_item_completed(&mut self, text: String) {
        let streamed_plan = self.plan_delta_buffer.trim().to_string();
        let plan_text = if text.trim().is_empty() {
            streamed_plan
        } else {
            text
        };
        if !plan_text.trim().is_empty() {
            self.last_copyable_output = Some(plan_text.clone());
        }
        self.plan_delta_buffer.clear();
        self.plan_item_active = false;
        self.saw_plan_item_this_turn = true;
        if let Some(mut controller) = self.plan_stream_controller.take()
            && let Some(cell) = controller.finalize()
        {
            self.add_boxed_history(cell);
            // TODO: Replace streamed output with the final plan item text if plan streaming is
            // removed or if we need to reconcile mismatches between streamed and final content.
            return;
        }
        if plan_text.is_empty() {
            return;
        }
        self.add_to_history(history_cell::new_proposed_plan(plan_text));
    }

    fn on_agent_reasoning_delta(&mut self, delta: String) {
        // For reasoning deltas, do not stream to history. Accumulate the
        // current reasoning block and extract the first bold element
        // (between **/**) as the chunk header. Show this header as status.
        self.reasoning_buffer.push_str(&delta);

        if self.unified_exec_wait_streak.is_some() {
            // Unified exec waiting should take precedence over reasoning-derived status headers.
            self.request_redraw();
            return;
        }

        if let Some(header) = extract_first_bold(&self.reasoning_buffer) {
            // Update the shimmer header to the extracted reasoning chunk header.
            self.set_status_header(header);
        } else {
            // Fallback while we don't yet have a bold header: leave existing header as-is.
        }
        self.request_redraw();
    }

    fn on_agent_reasoning_final(&mut self) {
        // At the end of a reasoning block, record transcript-only content.
        self.full_reasoning_buffer.push_str(&self.reasoning_buffer);
        if !self.full_reasoning_buffer.is_empty() {
            let cell =
                history_cell::new_reasoning_summary_block(self.full_reasoning_buffer.clone());
            self.add_boxed_history(cell);
        }
        self.reasoning_buffer.clear();
        self.full_reasoning_buffer.clear();
        self.request_redraw();
    }

    fn on_reasoning_section_break(&mut self) {
        // Start a new reasoning block for header extraction and accumulate transcript.
        self.full_reasoning_buffer.push_str(&self.reasoning_buffer);
        self.full_reasoning_buffer.push_str("\n\n");
        self.reasoning_buffer.clear();
    }

    // Raw reasoning uses the same flow as summarized reasoning

    fn on_task_started(&mut self) {
        if self.running_turn_model.is_none() {
            if let Some(snapshot) = self
                .active_runtime_context
                .as_ref()
                .filter(|snapshot| !snapshot.model.trim().is_empty())
            {
                self.running_turn_model = Some(snapshot.model.clone());
                self.running_turn_reasoning_effort = snapshot.reasoning_effort;
            } else {
                self.running_turn_model = Some(self.current_model().to_string());
                self.running_turn_reasoning_effort = self.effective_reasoning_effort();
            }
        }
        self.agent_turn_running = true;
        self.saw_plan_update_this_turn = false;
        self.saw_plan_item_this_turn = false;
        self.plan_delta_buffer.clear();
        self.plan_item_active = false;
        self.adaptive_chunking.reset();
        self.plan_stream_controller = None;
        self.otel_manager.reset_runtime_metrics();
        self.bottom_pane.clear_quit_shortcut_hint();
        self.quit_shortcut_expires_at = None;
        self.quit_shortcut_key = None;
        self.update_task_running_state();
        self.retry_status_header = None;
        self.bottom_pane.set_interrupt_hint_visible(true);
        self.bottom_pane.clear_progress_trace();
        self.turn_progress_trace.clear();
        self.pending_separator_progress_trace = None;
        self.set_status_header(String::from("Working"));
        self.full_reasoning_buffer.clear();
        self.reasoning_buffer.clear();
        self.refresh_status_line();
        self.request_redraw();
    }

    fn on_task_complete(&mut self, last_agent_message: Option<String>, from_replay: bool) {
        if let Some(last_message) = last_agent_message.as_ref()
            && !last_message.trim().is_empty()
        {
            self.last_assistant_output_markdown = Some(last_message.clone());
            if !from_replay || !self.last_copyable_response_is(last_message) {
                self.push_copyable_message(CopyableRole::Response, last_message);
            }
            self.last_copyable_output = Some(last_message.clone());
        }
        // If a stream is currently active, finalize it.
        self.flush_answer_stream_with_separator();
        if let Some(mut controller) = self.plan_stream_controller.take()
            && let Some(cell) = controller.finalize()
        {
            self.add_boxed_history(cell);
        }
        self.flush_unified_exec_wait_streak();
        if !from_replay {
            let runtime_metrics = self.otel_manager.runtime_metrics_summary();
            let separator_trace = if self.turn_progress_trace.is_empty() {
                self.pending_separator_progress_trace.take()
            } else {
                Some(std::mem::take(&mut self.turn_progress_trace))
            };
            if runtime_metrics.is_some() {
                let elapsed_seconds = self
                    .bottom_pane
                    .status_widget()
                    .map(super::status_indicator_widget::StatusIndicatorWidget::elapsed_seconds);
                self.add_to_history(history_cell::FinalMessageSeparator::new(
                    elapsed_seconds,
                    runtime_metrics,
                    separator_trace,
                    self.progress_trace_styles,
                ));
            }
            self.needs_final_message_separator = false;
            self.had_work_activity = false;
            self.request_status_line_branch_refresh();
        }
        // Mark task stopped and request redraw now that all content is in history.
        self.agent_turn_running = false;
        self.running_turn_model = None;
        self.running_turn_reasoning_effort = None;
        self.update_task_running_state();
        self.bottom_pane.clear_progress_trace();
        self.turn_progress_trace.clear();
        self.pending_separator_progress_trace = None;
        self.running_commands.clear();
        self.suppressed_exec_calls.clear();
        self.last_unified_wait = None;
        self.unified_exec_wait_streak = None;
        self.clear_unified_exec_processes();
        self.refresh_status_line();
        self.request_redraw();

        if !from_replay && self.queued_user_messages.is_empty() {
            self.maybe_prompt_plan_implementation();
        }
        // Keep this flag for replayed completion events so a subsequent live TurnComplete can
        // still show the prompt once after thread switch replay.
        if !from_replay {
            self.saw_plan_item_this_turn = false;
        }
        // If there is a queued user message, send exactly one now to begin the next turn.
        self.maybe_send_next_queued_input();
        // Emit a notification when the turn completes (suppressed if focused).
        self.notify(Notification::AgentTurnComplete {
            response: last_agent_message.unwrap_or_default(),
        });

        self.maybe_show_pending_rate_limit_prompt();
    }

    fn maybe_prompt_plan_implementation(&mut self) {
        if !self.collaboration_modes_enabled() {
            return;
        }
        if !self.queued_user_messages.is_empty() {
            return;
        }
        if self.active_mode_kind() != ModeKind::Plan {
            return;
        }
        if !self.saw_plan_item_this_turn {
            return;
        }
        if !self.bottom_pane.no_modal_or_popup_active() {
            return;
        }

        if matches!(
            self.rate_limit_switch_prompt,
            RateLimitSwitchPromptState::Pending
        ) {
            return;
        }

        self.open_plan_implementation_prompt();
    }

    fn open_plan_implementation_prompt(&mut self) {
        let default_mask = collaboration_modes::default_mode_mask(self.models_manager.as_ref());
        let (implement_actions, implement_disabled_reason) = match default_mask {
            Some(mask) => {
                let user_text = PLAN_IMPLEMENTATION_CODING_MESSAGE.to_string();
                let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
                    tx.send(AppEvent::SubmitUserMessageWithMode {
                        text: user_text.clone(),
                        collaboration_mode: mask.clone(),
                    });
                })];
                (actions, None)
            }
            None => (Vec::new(), Some("Default mode unavailable".to_string())),
        };

        let items = vec![
            SelectionItem {
                name: PLAN_IMPLEMENTATION_YES.to_string(),
                description: Some("Switch to Default and start coding.".to_string()),
                selected_description: None,
                is_current: false,
                actions: implement_actions,
                disabled_reason: implement_disabled_reason,
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: PLAN_IMPLEMENTATION_NO.to_string(),
                description: Some("Continue planning with the model.".to_string()),
                selected_description: None,
                is_current: false,
                actions: Vec::new(),
                dismiss_on_select: true,
                ..Default::default()
            },
        ];

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some(PLAN_IMPLEMENTATION_TITLE.to_string()),
            subtitle: None,
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }

    pub(crate) fn set_token_info(&mut self, info: Option<TokenUsageInfo>) {
        self.token_info = info;
        self.sync_context_window_indicator();
    }

    fn context_remaining_percent(&self, info: &TokenUsageInfo) -> Option<i64> {
        info.model_context_window.map(|window| {
            info.last_token_usage
                .percent_of_context_window_remaining(window)
        })
    }

    fn context_used_tokens(&self, info: &TokenUsageInfo, percent_known: bool) -> Option<i64> {
        if percent_known {
            return None;
        }

        Some(info.total_token_usage.tokens_in_context_window())
    }

    fn sync_context_window_indicator(&mut self) {
        let Some(info) = self.status_subject_token_info() else {
            self.bottom_pane.set_context_window(None, None);
            return;
        };
        let percent = self.context_remaining_percent(info);
        let used_tokens = self.context_used_tokens(info, percent.is_some());
        self.bottom_pane.set_context_window(percent, used_tokens);
    }

    fn restore_pre_review_token_info(&mut self) {
        if let Some(saved) = self.pre_review_token_info.take() {
            self.token_info = saved;
            self.sync_context_window_indicator();
        }
    }

    pub(crate) fn on_rate_limit_snapshot(&mut self, snapshot: Option<RateLimitSnapshot>) {
        if let Some(mut snapshot) = snapshot {
            if snapshot.credits.is_none() {
                snapshot.credits = self
                    .rate_limit_snapshot
                    .as_ref()
                    .and_then(|display| display.credits.as_ref())
                    .map(|credits| CreditsSnapshot {
                        has_credits: credits.has_credits,
                        unlimited: credits.unlimited,
                        balance: credits.balance.clone(),
                    });
            }

            self.plan_type = snapshot.plan_type.or(self.plan_type);

            let warnings = self.rate_limit_warnings.take_warnings(
                snapshot
                    .secondary
                    .as_ref()
                    .map(|window| window.used_percent),
                snapshot
                    .secondary
                    .as_ref()
                    .and_then(|window| window.window_minutes),
                snapshot.primary.as_ref().map(|window| window.used_percent),
                snapshot
                    .primary
                    .as_ref()
                    .and_then(|window| window.window_minutes),
            );

            let high_usage = snapshot
                .secondary
                .as_ref()
                .map(|w| w.used_percent >= RATE_LIMIT_SWITCH_PROMPT_THRESHOLD)
                .unwrap_or(false)
                || snapshot
                    .primary
                    .as_ref()
                    .map(|w| w.used_percent >= RATE_LIMIT_SWITCH_PROMPT_THRESHOLD)
                    .unwrap_or(false);

            if high_usage
                && !self.rate_limit_switch_prompt_hidden()
                && self.current_model() != NUDGE_MODEL_SLUG
                && !matches!(
                    self.rate_limit_switch_prompt,
                    RateLimitSwitchPromptState::Shown
                )
            {
                self.rate_limit_switch_prompt = RateLimitSwitchPromptState::Pending;
            }

            let display = crate::status::rate_limit_snapshot_display(&snapshot, Local::now());
            self.rate_limit_snapshot = Some(display);

            if !warnings.is_empty() {
                for warning in warnings {
                    self.add_to_history(history_cell::new_warning_event(warning));
                }
                self.request_redraw();
            }
        } else {
            self.rate_limit_snapshot = None;
        }
        self.refresh_status_line();
    }
    /// Finalize any active exec as failed and stop/clear agent-turn UI state.
    ///
    /// This does not clear MCP startup tracking, because MCP startup can overlap with turn cleanup
    /// and should continue to drive the bottom-pane running indicator while it is in progress.
    fn finalize_turn(&mut self) {
        // Ensure any spinner is replaced by a red ✗ and flushed into history.
        self.finalize_active_cell_as_failed();
        // Reset running state and clear streaming buffers.
        self.agent_turn_running = false;
        self.running_turn_model = None;
        self.running_turn_reasoning_effort = None;
        self.update_task_running_state();
        self.bottom_pane.clear_progress_trace();
        self.running_commands.clear();
        self.suppressed_exec_calls.clear();
        self.last_unified_wait = None;
        self.unified_exec_wait_streak = None;
        self.clear_unified_exec_processes();
        self.adaptive_chunking.reset();
        self.stream_controller = None;
        self.plan_stream_controller = None;
        self.request_status_line_branch_refresh();
        self.refresh_status_line();
        self.maybe_show_pending_rate_limit_prompt();
    }

    fn on_model_cap_error(&mut self, model: String, reset_after_seconds: Option<u64>) {
        self.finalize_turn();

        let mut message = format!("Model {model} is at capacity. Please try a different model.");
        if let Some(seconds) = reset_after_seconds {
            message.push_str(&format!(
                " Try again in {}.",
                format_duration_short(seconds)
            ));
        } else {
            message.push_str(" Try again later.");
        }

        self.add_to_history(history_cell::new_warning_event(message));
        self.request_redraw();
        self.maybe_send_next_queued_input();
    }

    fn on_error(&mut self, message: String) {
        self.finalize_turn();
        self.add_to_history(history_cell::new_error_event(message));
        self.request_redraw();

        // After an error ends the turn, try sending the next queued input.
        self.maybe_send_next_queued_input();
    }

    fn on_warning(&mut self, message: impl Into<String>) {
        self.add_to_history(history_cell::new_warning_event(message.into()));
        self.request_redraw();
    }

    fn on_mcp_startup_update(&mut self, ev: McpStartupUpdateEvent) {
        let mut status = self.mcp_startup_status.take().unwrap_or_default();
        if let McpStartupStatus::Failed { error } = &ev.status {
            self.on_warning(error);
        }
        status.insert(ev.server, ev.status);
        self.mcp_startup_status = Some(status);
        self.update_task_running_state();
        if let Some(current) = &self.mcp_startup_status {
            let total = current.len();
            let mut starting: Vec<_> = current
                .iter()
                .filter_map(|(name, state)| {
                    if matches!(state, McpStartupStatus::Starting) {
                        Some(name)
                    } else {
                        None
                    }
                })
                .collect();
            starting.sort();
            if let Some(first) = starting.first() {
                let completed = total.saturating_sub(starting.len());
                let max_to_show = 3;
                let mut to_show: Vec<String> = starting
                    .iter()
                    .take(max_to_show)
                    .map(ToString::to_string)
                    .collect();
                if starting.len() > max_to_show {
                    to_show.push("…".to_string());
                }
                let header = if total > 1 {
                    format!(
                        "Starting MCP servers ({completed}/{total}): {}",
                        to_show.join(", ")
                    )
                } else {
                    format!("Booting MCP server: {first}")
                };
                self.set_status_header(header);
            }
        }
        self.request_redraw();
    }

    fn on_mcp_startup_complete(&mut self, ev: McpStartupCompleteEvent) {
        let mut parts = Vec::new();
        if !ev.failed.is_empty() {
            let failed_servers: Vec<_> = ev.failed.iter().map(|f| f.server.clone()).collect();
            parts.push(format!("failed: {}", failed_servers.join(", ")));
        }
        if !ev.cancelled.is_empty() {
            self.on_warning(format!(
                "MCP startup interrupted. The following servers were not initialized: {}",
                ev.cancelled.join(", ")
            ));
        }
        if !parts.is_empty() {
            self.on_warning(format!("MCP startup incomplete ({})", parts.join("; ")));
        }

        self.mcp_startup_status = None;
        self.update_task_running_state();
        self.maybe_send_next_queued_input();
        self.request_redraw();
    }

    /// Handle a turn aborted due to user interrupt (Esc).
    /// Keep queued messages in the queue for later.
    fn on_interrupted_turn(&mut self, reason: TurnAbortReason) {
        // Finalize, log a gentle prompt, and clear running state.
        self.finalize_turn();

        if reason != TurnAbortReason::ReviewEnded {
            self.add_to_history(history_cell::new_error_event(
                "Conversation interrupted - tell the model what to do differently. Something went wrong? Hit `/feedback` to report the issue.".to_owned(),
            ));
        }

        self.request_redraw();
    }

    fn on_paused_turn(&mut self) {
        self.finalize_turn();
        self.add_info_message(
            "Conversation paused.".to_string(),
            Some("Use `/continue` to resume this turn.".to_string()),
        );
        self.request_redraw();
    }

    fn on_plan_update(&mut self, update: UpdatePlanArgs) {
        self.saw_plan_update_this_turn = true;
        self.add_to_history(history_cell::new_plan_update(update));
    }

    fn on_exec_approval_request(&mut self, id: String, ev: ExecApprovalRequestEvent) {
        let id2 = id.clone();
        let ev2 = ev.clone();
        self.defer_or_handle(
            |q| q.push_exec_approval(id, ev),
            |s| s.handle_exec_approval_now(id2, ev2),
        );
    }

    fn on_apply_patch_approval_request(&mut self, id: String, ev: ApplyPatchApprovalRequestEvent) {
        let id2 = id.clone();
        let ev2 = ev.clone();
        self.defer_or_handle(
            |q| q.push_apply_patch_approval(id, ev),
            |s| s.handle_apply_patch_approval_now(id2, ev2),
        );
    }

    fn on_elicitation_request(&mut self, ev: ElicitationRequestEvent) {
        let ev2 = ev.clone();
        self.defer_or_handle(
            |q| q.push_elicitation(ev),
            |s| s.handle_elicitation_request_now(ev2),
        );
    }

    fn on_request_user_input(&mut self, ev: RequestUserInputEvent) {
        let ev2 = ev.clone();
        self.defer_or_handle(
            |q| q.push_user_input(ev),
            |s| s.handle_request_user_input_now(ev2),
        );
    }

    fn on_progress_trace(&mut self, ev: ProgressTraceEvent) {
        if matches!(ev.state, codex_core::protocol::ProgressTraceState::Started) {
            self.turn_progress_trace.push(ev.category);
            const MAX_TURN_TRACE_SEGMENTS: usize = 128;
            if self.turn_progress_trace.len() > MAX_TURN_TRACE_SEGMENTS {
                let remove_count = self.turn_progress_trace.len() - MAX_TURN_TRACE_SEGMENTS;
                self.turn_progress_trace.drain(0..remove_count);
            }
        }
        self.bottom_pane
            .record_progress_trace(ev.category, ev.state, ev.label);
    }

    fn on_exec_command_begin(&mut self, ev: ExecCommandBeginEvent) {
        self.flush_answer_stream_with_separator();
        if is_unified_exec_source(ev.source) {
            // Unified exec may be parsed as Unknown; keep the working indicator visible regardless.
            self.bottom_pane.ensure_status_indicator();
            self.track_unified_exec_process_begin(&ev);
            if !is_standard_tool_call(&ev.parsed_cmd) {
                return;
            }
        }
        let ev2 = ev.clone();
        self.defer_or_handle(|q| q.push_exec_begin(ev), |s| s.handle_exec_begin_now(ev2));
    }

    fn on_exec_command_output_delta(&mut self, ev: ExecCommandOutputDeltaEvent) {
        self.track_unified_exec_output_chunk(&ev.call_id, &ev.chunk);

        let Some(cell) = self
            .active_cell
            .as_mut()
            .and_then(|c| c.as_any_mut().downcast_mut::<ExecCell>())
        else {
            return;
        };

        if cell.append_output(&ev.call_id, std::str::from_utf8(&ev.chunk).unwrap_or("")) {
            self.bump_active_cell_revision();
            self.request_redraw();
        }
    }

    fn on_terminal_interaction(&mut self, ev: TerminalInteractionEvent) {
        if !self.bottom_pane.is_task_running() {
            return;
        }
        self.flush_answer_stream_with_separator();
        let command_display = self
            .unified_exec_processes
            .iter()
            .find(|process| process.key == ev.process_id)
            .map(|process| process.command_display.clone());
        if ev.stdin.is_empty() {
            // Empty stdin means we are polling for background output.
            // Surface this in the status header (single "waiting" surface) instead of the transcript.
            self.bottom_pane.ensure_status_indicator();
            self.bottom_pane.set_interrupt_hint_visible(true);
            let header = if let Some(command) = &command_display {
                format!("Waiting for background terminal · {command}")
            } else {
                "Waiting for background terminal".to_string()
            };
            self.set_status_header(header);
            match &mut self.unified_exec_wait_streak {
                Some(wait) if wait.process_id == ev.process_id => {
                    wait.update_command_display(command_display);
                }
                Some(_) => {
                    self.flush_unified_exec_wait_streak();
                    self.unified_exec_wait_streak =
                        Some(UnifiedExecWaitStreak::new(ev.process_id, command_display));
                }
                None => {
                    self.unified_exec_wait_streak =
                        Some(UnifiedExecWaitStreak::new(ev.process_id, command_display));
                }
            }
            self.request_redraw();
        } else {
            if self
                .unified_exec_wait_streak
                .as_ref()
                .is_some_and(|wait| wait.process_id == ev.process_id)
            {
                self.flush_unified_exec_wait_streak();
            }
            self.add_to_history(history_cell::new_unified_exec_interaction(
                command_display,
                ev.stdin,
            ));
        }
    }

    fn on_patch_apply_begin(&mut self, event: PatchApplyBeginEvent) {
        self.add_to_history(history_cell::new_patch_event(
            event.changes,
            &self.config.cwd,
            self.config.diff_view,
        ));
    }

    fn on_view_image_tool_call(&mut self, event: ViewImageToolCallEvent) {
        self.flush_answer_stream_with_separator();
        self.add_to_history(history_cell::new_view_image_tool_call(
            event.path,
            &self.config.cwd,
        ));
        self.request_redraw();
    }

    fn on_patch_apply_end(&mut self, event: codex_core::protocol::PatchApplyEndEvent) {
        let ev2 = event.clone();
        self.defer_or_handle(
            |q| q.push_patch_end(event),
            |s| s.handle_patch_apply_end_now(ev2),
        );
    }

    fn on_exec_command_end(&mut self, ev: ExecCommandEndEvent) {
        if is_unified_exec_source(ev.source) {
            if let Some(process_id) = ev.process_id.as_deref()
                && self
                    .unified_exec_wait_streak
                    .as_ref()
                    .is_some_and(|wait| wait.process_id == process_id)
            {
                self.flush_unified_exec_wait_streak();
            }
            self.track_unified_exec_process_end(&ev);
            if !self.bottom_pane.is_task_running() {
                return;
            }
        }
        let ev2 = ev.clone();
        self.defer_or_handle(|q| q.push_exec_end(ev), |s| s.handle_exec_end_now(ev2));
    }

    fn track_unified_exec_process_begin(&mut self, ev: &ExecCommandBeginEvent) {
        if ev.source != ExecCommandSource::UnifiedExecStartup {
            return;
        }
        let key = ev.process_id.clone().unwrap_or(ev.call_id.to_string());
        let command_display = strip_bash_lc_and_escape(&ev.command);
        if let Some(existing) = self
            .unified_exec_processes
            .iter_mut()
            .find(|process| process.key == key)
        {
            existing.call_id = ev.call_id.clone();
            existing.command_display = command_display;
            existing.recent_chunks.clear();
        } else {
            self.unified_exec_processes.push(UnifiedExecProcessSummary {
                key,
                call_id: ev.call_id.clone(),
                command_display,
                recent_chunks: Vec::new(),
            });
        }
        self.sync_unified_exec_footer();
    }

    fn track_unified_exec_process_end(&mut self, ev: &ExecCommandEndEvent) {
        let key = ev.process_id.clone().unwrap_or(ev.call_id.to_string());
        let before = self.unified_exec_processes.len();
        self.unified_exec_processes
            .retain(|process| process.key != key);
        if self.unified_exec_processes.len() != before {
            self.sync_unified_exec_footer();
        }
    }

    fn sync_unified_exec_footer(&mut self) {
        let processes = self
            .unified_exec_processes
            .iter()
            .map(|process| process.command_display.clone())
            .collect();
        self.bottom_pane.set_unified_exec_processes(processes);
    }

    /// Record recent stdout/stderr lines for the unified exec footer.
    fn track_unified_exec_output_chunk(&mut self, call_id: &str, chunk: &[u8]) {
        let Some(process) = self
            .unified_exec_processes
            .iter_mut()
            .find(|process| process.call_id == call_id)
        else {
            return;
        };

        let text = String::from_utf8_lossy(chunk);
        for line in text
            .lines()
            .map(str::trim_end)
            .filter(|line| !line.is_empty())
        {
            process.recent_chunks.push(line.to_string());
        }

        const MAX_RECENT_CHUNKS: usize = 3;
        if process.recent_chunks.len() > MAX_RECENT_CHUNKS {
            let drop_count = process.recent_chunks.len() - MAX_RECENT_CHUNKS;
            process.recent_chunks.drain(0..drop_count);
        }
    }

    fn clear_unified_exec_processes(&mut self) {
        if self.unified_exec_processes.is_empty() {
            return;
        }
        self.unified_exec_processes.clear();
        self.sync_unified_exec_footer();
    }

    fn on_mcp_tool_call_begin(&mut self, ev: McpToolCallBeginEvent) {
        let ev2 = ev.clone();
        self.defer_or_handle(|q| q.push_mcp_begin(ev), |s| s.handle_mcp_begin_now(ev2));
    }

    fn on_mcp_tool_call_end(&mut self, ev: McpToolCallEndEvent) {
        let ev2 = ev.clone();
        self.defer_or_handle(|q| q.push_mcp_end(ev), |s| s.handle_mcp_end_now(ev2));
    }

    fn on_web_search_begin(&mut self, ev: WebSearchBeginEvent) {
        self.flush_answer_stream_with_separator();
        self.flush_active_cell();
        self.active_cell = Some(Box::new(history_cell::new_active_web_search_call(
            ev.call_id,
            String::new(),
            self.config.animations,
        )));
        self.bump_active_cell_revision();
        self.request_redraw();
    }

    fn on_web_search_end(&mut self, ev: WebSearchEndEvent) {
        self.flush_answer_stream_with_separator();
        let WebSearchEndEvent {
            call_id,
            query,
            action,
        } = ev;
        let mut handled = false;
        if let Some(cell) = self
            .active_cell
            .as_mut()
            .and_then(|cell| cell.as_any_mut().downcast_mut::<WebSearchCell>())
            && cell.call_id() == call_id
        {
            cell.update(action.clone(), query.clone());
            cell.complete();
            self.bump_active_cell_revision();
            self.flush_active_cell();
            handled = true;
        }

        if !handled {
            self.add_to_history(history_cell::new_web_search_call(call_id, query, action));
        }
        self.had_work_activity = true;
    }

    fn on_collab_event(&mut self, cell: PlainHistoryCell) {
        self.flush_answer_stream_with_separator();
        self.add_to_history(cell);
        self.request_redraw();
    }

    fn on_get_history_entry_response(
        &mut self,
        event: codex_core::protocol::GetHistoryEntryResponseEvent,
    ) {
        let codex_core::protocol::GetHistoryEntryResponseEvent {
            offset,
            log_id,
            entry,
        } = event;
        self.bottom_pane
            .on_history_entry_response(log_id, offset, entry.map(|e| e.text));
    }

    fn on_shutdown_complete(&mut self) {
        self.request_immediate_exit();
    }

    fn on_turn_diff(&mut self, unified_diff: String) {
        debug!("TurnDiffEvent: {unified_diff}");
        self.refresh_status_line();
    }

    fn on_deprecation_notice(&mut self, event: DeprecationNoticeEvent) {
        let DeprecationNoticeEvent { summary, details } = event;
        self.add_to_history(history_cell::new_deprecation_notice(summary, details));
        self.request_redraw();
    }

    fn on_background_event(&mut self, message: String) {
        debug!("BackgroundEvent: {message}");
        self.bottom_pane.ensure_status_indicator();
        self.bottom_pane.set_interrupt_hint_visible(true);
        self.set_status_header(message);
    }

    fn on_undo_started(&mut self, event: UndoStartedEvent) {
        self.bottom_pane.ensure_status_indicator();
        self.bottom_pane.set_interrupt_hint_visible(false);
        let message = event
            .message
            .unwrap_or_else(|| "Undo in progress...".to_string());
        self.set_status_header(message);
    }

    fn on_undo_completed(&mut self, event: UndoCompletedEvent) {
        let UndoCompletedEvent { success, message } = event;
        self.bottom_pane.hide_status_indicator();
        let message = message.unwrap_or_else(|| {
            if success {
                "Undo completed successfully.".to_string()
            } else {
                "Undo failed.".to_string()
            }
        });
        if success {
            self.add_info_message(message, None);
        } else {
            self.add_error_message(message);
        }
    }

    fn on_stream_error(&mut self, message: String, additional_details: Option<String>) {
        if self.retry_status_header.is_none() {
            self.retry_status_header = Some(self.current_status_header.clone());
        }
        self.set_status(message, additional_details);
    }

    /// Periodic tick for stream commits. In smooth mode this preserves one-line pacing, while
    /// catch-up mode drains larger batches to reduce queue lag.
    pub(crate) fn on_commit_tick(&mut self) {
        self.run_commit_tick();
    }

    /// Runs a regular periodic commit tick.
    fn run_commit_tick(&mut self) {
        self.run_commit_tick_with_scope(CommitTickScope::AnyMode);
    }

    /// Runs an opportunistic commit tick only if catch-up mode is active.
    fn run_catch_up_commit_tick(&mut self) {
        self.run_commit_tick_with_scope(CommitTickScope::CatchUpOnly);
    }

    /// Runs a commit tick for the current stream queue snapshot.
    ///
    /// `scope` controls whether this call may commit in smooth mode or only when catch-up
    /// is currently active. While lines are actively streaming we hide the status row to avoid
    /// duplicate "in progress" affordances, but once all stream controllers go idle for this
    /// turn we restore the status row if the task is still running so users keep a live
    /// spinner/shimmer signal between preamble output and subsequent tool activity.
    fn run_commit_tick_with_scope(&mut self, scope: CommitTickScope) {
        let now = Instant::now();
        let outcome = run_commit_tick(
            &mut self.adaptive_chunking,
            self.stream_controller.as_mut(),
            self.plan_stream_controller.as_mut(),
            scope,
            now,
        );
        for cell in outcome.cells {
            self.bottom_pane.hide_status_indicator();
            self.add_boxed_history(cell);
        }

        if outcome.has_controller && outcome.all_idle {
            if self.bottom_pane.is_task_running() {
                self.bottom_pane.ensure_status_indicator();
                self.set_status_header(self.current_status_header.clone());
            }
            self.app_event_tx.send(AppEvent::StopCommitAnimation);
        }
    }

    fn flush_interrupt_queue(&mut self) {
        let mut mgr = std::mem::take(&mut self.interrupts);
        mgr.flush_all(self);
        self.interrupts = mgr;
    }

    #[inline]
    fn defer_or_handle(
        &mut self,
        push: impl FnOnce(&mut InterruptManager),
        handle: impl FnOnce(&mut Self),
    ) {
        // Preserve deterministic FIFO across queued interrupts: once anything
        // is queued due to an active write cycle, continue queueing until the
        // queue is flushed to avoid reordering (e.g., ExecEnd before ExecBegin).
        if self.stream_controller.is_some() || !self.interrupts.is_empty() {
            push(&mut self.interrupts);
        } else {
            handle(self);
        }
    }

    fn handle_stream_finished(&mut self) {
        if self.task_complete_pending {
            self.bottom_pane.hide_status_indicator();
            self.task_complete_pending = false;
        }
        // A completed stream indicates non-exec content was just inserted.
        self.flush_interrupt_queue();
    }

    #[inline]
    fn handle_streaming_delta(&mut self, delta: String) {
        // Before streaming agent content, flush any active exec cell group.
        self.flush_unified_exec_wait_streak();
        self.flush_active_cell();

        if self.stream_controller.is_none() {
            // If the previous turn inserted non-stream history (exec output, patch status, MCP
            // calls), render a separator before starting the next streamed assistant message.
            if self.needs_final_message_separator && self.had_work_activity {
                let elapsed_seconds = self
                    .bottom_pane
                    .status_widget()
                    .map(super::status_indicator_widget::StatusIndicatorWidget::elapsed_seconds)
                    .map(|current| self.worked_elapsed_from(current));
                let separator_trace = if self.turn_progress_trace.is_empty() {
                    self.pending_separator_progress_trace.take()
                } else {
                    Some(std::mem::take(&mut self.turn_progress_trace))
                };
                self.add_to_history(history_cell::FinalMessageSeparator::new(
                    elapsed_seconds,
                    None,
                    separator_trace,
                    self.progress_trace_styles,
                ));
                self.needs_final_message_separator = false;
                self.had_work_activity = false;
            } else if self.needs_final_message_separator {
                // Reset the flag even if we don't show separator (no work was done)
                self.needs_final_message_separator = false;
            }
            self.stream_controller = Some(StreamController::new(
                self.last_rendered_width.get().map(|w| w.saturating_sub(2)),
            ));
        }
        if let Some(controller) = self.stream_controller.as_mut()
            && controller.push(&delta)
        {
            self.app_event_tx.send(AppEvent::StartCommitAnimation);
            self.run_catch_up_commit_tick();
        }
        self.request_redraw();
    }

    fn worked_elapsed_from(&mut self, current_elapsed: u64) -> u64 {
        let baseline = match self.last_separator_elapsed_secs {
            Some(last) if current_elapsed < last => 0,
            Some(last) => last,
            None => 0,
        };
        let elapsed = current_elapsed.saturating_sub(baseline);
        self.last_separator_elapsed_secs = Some(current_elapsed);
        elapsed
    }

    pub(crate) fn handle_exec_end_now(&mut self, ev: ExecCommandEndEvent) {
        let running = self.running_commands.remove(&ev.call_id);
        if self.suppressed_exec_calls.remove(&ev.call_id) {
            return;
        }
        let (command, parsed, source) = match running {
            Some(rc) => (rc.command, rc.parsed_cmd, rc.source),
            None => (ev.command.clone(), ev.parsed_cmd.clone(), ev.source),
        };
        let is_unified_exec_interaction =
            matches!(source, ExecCommandSource::UnifiedExecInteraction);

        let needs_new = self
            .active_cell
            .as_ref()
            .map(|cell| cell.as_any().downcast_ref::<ExecCell>().is_none())
            .unwrap_or(true);
        if needs_new {
            self.flush_active_cell();
            self.active_cell = Some(Box::new(new_active_exec_command(
                ev.call_id.clone(),
                command,
                parsed,
                source,
                ev.interaction_input.clone(),
                self.config.animations,
            )));
        }

        if let Some(cell) = self
            .active_cell
            .as_mut()
            .and_then(|c| c.as_any_mut().downcast_mut::<ExecCell>())
        {
            let output = if is_unified_exec_interaction {
                CommandOutput {
                    exit_code: ev.exit_code,
                    formatted_output: String::new(),
                    aggregated_output: String::new(),
                }
            } else {
                CommandOutput {
                    exit_code: ev.exit_code,
                    formatted_output: ev.formatted_output.clone(),
                    aggregated_output: ev.aggregated_output.clone(),
                }
            };
            cell.complete_call(&ev.call_id, output, ev.duration);
            if cell.should_flush() {
                self.flush_active_cell();
            } else {
                self.bump_active_cell_revision();
                self.request_redraw();
            }
        }
        // Mark that actual work was done (command executed)
        self.had_work_activity = true;
    }

    pub(crate) fn handle_patch_apply_end_now(
        &mut self,
        event: codex_core::protocol::PatchApplyEndEvent,
    ) {
        // If the patch was successful, just let the "Edited" block stand.
        // Otherwise, add a failure block.
        if !event.success {
            self.add_to_history(history_cell::new_patch_apply_failure(event.stderr));
        }
        // Mark that actual work was done (patch applied)
        self.had_work_activity = true;
    }

    pub(crate) fn handle_exec_approval_now(&mut self, id: String, ev: ExecApprovalRequestEvent) {
        self.flush_answer_stream_with_separator();
        let command = shlex::try_join(ev.command.iter().map(String::as_str))
            .unwrap_or_else(|_| ev.command.join(" "));
        self.notify(Notification::ExecApprovalRequested { command });

        let request = ApprovalRequest::Exec {
            id,
            command: ev.command,
            reason: ev.reason,
            proposed_execpolicy_amendment: ev.proposed_execpolicy_amendment,
        };
        self.bottom_pane
            .push_approval_request(request, &self.config.features);
        self.request_redraw();
    }

    pub(crate) fn handle_apply_patch_approval_now(
        &mut self,
        id: String,
        ev: ApplyPatchApprovalRequestEvent,
    ) {
        self.flush_answer_stream_with_separator();

        let request = ApprovalRequest::ApplyPatch {
            id,
            reason: ev.reason,
            changes: ev.changes.clone(),
            cwd: self.config.cwd.clone(),
            diff_view: self.config.diff_view,
        };
        self.bottom_pane
            .push_approval_request(request, &self.config.features);
        self.request_redraw();
        self.notify(Notification::EditApprovalRequested {
            cwd: self.config.cwd.clone(),
            changes: ev.changes.keys().cloned().collect(),
        });
    }

    pub(crate) fn handle_elicitation_request_now(&mut self, ev: ElicitationRequestEvent) {
        self.flush_answer_stream_with_separator();

        self.notify(Notification::ElicitationRequested {
            server_name: ev.server_name.clone(),
        });

        let request = ApprovalRequest::McpElicitation {
            server_name: ev.server_name,
            request_id: ev.id,
            message: ev.message,
        };
        self.bottom_pane
            .push_approval_request(request, &self.config.features);
        self.request_redraw();
    }

    pub(crate) fn handle_request_user_input_now(&mut self, ev: RequestUserInputEvent) {
        self.flush_answer_stream_with_separator();
        self.bottom_pane.push_user_input_request(ev);
        self.request_redraw();
    }

    pub(crate) fn handle_exec_begin_now(&mut self, ev: ExecCommandBeginEvent) {
        // Ensure the status indicator is visible while the command runs.
        self.bottom_pane.ensure_status_indicator();
        self.running_commands.insert(
            ev.call_id.clone(),
            RunningCommand {
                command: ev.command.clone(),
                parsed_cmd: ev.parsed_cmd.clone(),
                source: ev.source,
            },
        );
        let is_wait_interaction = matches!(ev.source, ExecCommandSource::UnifiedExecInteraction)
            && ev
                .interaction_input
                .as_deref()
                .map(str::is_empty)
                .unwrap_or(true);
        let command_display = ev.command.join(" ");
        let should_suppress_unified_wait = is_wait_interaction
            && self
                .last_unified_wait
                .as_ref()
                .is_some_and(|wait| wait.is_duplicate(&command_display));
        if is_wait_interaction {
            self.last_unified_wait = Some(UnifiedExecWaitState::new(command_display));
        } else {
            self.last_unified_wait = None;
        }
        if should_suppress_unified_wait {
            self.suppressed_exec_calls.insert(ev.call_id);
            return;
        }
        let interaction_input = ev.interaction_input.clone();
        if let Some(cell) = self
            .active_cell
            .as_mut()
            .and_then(|c| c.as_any_mut().downcast_mut::<ExecCell>())
            && let Some(new_exec) = cell.with_added_call(
                ev.call_id.clone(),
                ev.command.clone(),
                ev.parsed_cmd.clone(),
                ev.source,
                interaction_input.clone(),
            )
        {
            *cell = new_exec;
            self.bump_active_cell_revision();
        } else {
            self.flush_active_cell();

            self.active_cell = Some(Box::new(new_active_exec_command(
                ev.call_id.clone(),
                ev.command.clone(),
                ev.parsed_cmd,
                ev.source,
                interaction_input,
                self.config.animations,
            )));
            self.bump_active_cell_revision();
        }

        self.request_redraw();
    }

    pub(crate) fn handle_mcp_begin_now(&mut self, ev: McpToolCallBeginEvent) {
        self.flush_answer_stream_with_separator();
        self.flush_active_cell();
        self.active_cell = Some(Box::new(history_cell::new_active_mcp_tool_call(
            ev.call_id,
            ev.invocation,
            self.config.animations,
        )));
        self.bump_active_cell_revision();
        self.request_redraw();
    }
    pub(crate) fn handle_mcp_end_now(&mut self, ev: McpToolCallEndEvent) {
        self.flush_answer_stream_with_separator();

        let McpToolCallEndEvent {
            call_id,
            invocation,
            duration,
            result,
        } = ev;

        let extra_cell = match self
            .active_cell
            .as_mut()
            .and_then(|cell| cell.as_any_mut().downcast_mut::<McpToolCallCell>())
        {
            Some(cell) if cell.call_id() == call_id => cell.complete(duration, result),
            _ => {
                self.flush_active_cell();
                let mut cell = history_cell::new_active_mcp_tool_call(
                    call_id,
                    invocation,
                    self.config.animations,
                );
                let extra_cell = cell.complete(duration, result);
                self.active_cell = Some(Box::new(cell));
                extra_cell
            }
        };

        self.flush_active_cell();
        if let Some(extra) = extra_cell {
            self.add_boxed_history(extra);
        }
        // Mark that actual work was done (MCP tool call)
        self.had_work_activity = true;
    }

    fn resolve_keybindings(config: &Config, enhanced_keys_supported: bool) -> Keybindings {
        #[cfg(target_os = "linux")]
        let is_wsl = crate::clipboard_paste::is_probably_wsl();
        #[cfg(not(target_os = "linux"))]
        let is_wsl = false;

        Keybindings::from_config(&config.keybindings, enhanced_keys_supported, is_wsl)
    }

    pub(crate) fn new(common: ChatWidgetInit, thread_manager: Arc<ThreadManager>) -> Self {
        let ChatWidgetInit {
            config,
            frame_requester,
            app_event_tx,
            initial_user_message,
            enhanced_keys_supported,
            auth_manager,
            models_manager,
            feedback,
            is_first_run,
            feedback_audience,
            model,
            status_line_invalid_items_warned,
            otel_manager,
        } = common;
        let model = model.filter(|m| !m.trim().is_empty());
        let mut config = config;
        config.model = model.clone();
        let keybindings = Self::resolve_keybindings(&config, enhanced_keys_supported);
        let progress_legend_mode = config.tui_progress_legend_mode;
        let (progress_trace_styles, progress_trace_style_warnings) =
            resolve_progress_trace_styles(config.tui_progress_trace_style.as_ref());
        crate::markdown_render::set_syntax_highlight_theme(
            config.tui_syntax_highlight_theme.clone(),
        );
        let mut rng = rand::rng();
        let placeholder = PLACEHOLDERS[rng.random_range(0..PLACEHOLDERS.len())].to_string();
        let codex_op_tx = spawn_agent(config.clone(), app_event_tx.clone(), thread_manager);

        let model_override = model.as_deref();
        let model_for_header = model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL_DISPLAY_NAME.to_string());
        let active_collaboration_mask =
            Self::initial_collaboration_mask(&config, models_manager.as_ref(), model_override);
        let header_model = active_collaboration_mask
            .as_ref()
            .and_then(|mask| mask.model.clone())
            .unwrap_or_else(|| model_for_header.clone());
        let fallback_default = Settings {
            model: header_model.clone(),
            reasoning_effort: None,
            developer_instructions: None,
        };
        // Collaboration modes start in Default mode.
        let current_collaboration_mode = CollaborationMode {
            mode: ModeKind::Default,
            settings: fallback_default,
        };

        let active_cell = Some(Self::placeholder_session_header_cell(&config));

        let current_cwd = Some(config.cwd.clone());
        let mut widget = Self {
            app_event_tx: app_event_tx.clone(),
            frame_requester: frame_requester.clone(),
            codex_op_tx,
            bottom_pane: BottomPane::new(BottomPaneParams {
                frame_requester,
                app_event_tx,
                has_input_focus: true,
                enhanced_keys_supported,
                placeholder_text: placeholder,
                disable_paste_burst: config.disable_paste_burst,
                animations_enabled: config.animations,
                progress_legend_mode,
                progress_trace_styles,
                skills: None,
            }),
            active_cell,
            active_cell_revision: 0,
            config,
            keybindings,
            skills_all: Vec::new(),
            skills_initial_state: None,
            current_collaboration_mode,
            active_collaboration_mask,
            auth_manager,
            models_manager,
            otel_manager,
            session_header: SessionHeader::new(header_model),
            initial_user_message,
            token_info: None,
            rate_limit_snapshot: None,
            plan_type: None,
            rate_limit_warnings: RateLimitWarningState::default(),
            rate_limit_switch_prompt: RateLimitSwitchPromptState::default(),
            rate_limit_poller: None,
            adaptive_chunking: AdaptiveChunkingPolicy::default(),
            stream_controller: None,
            plan_stream_controller: None,
            last_copyable_output: None,
            running_commands: HashMap::new(),
            suppressed_exec_calls: HashSet::new(),
            last_unified_wait: None,
            unified_exec_wait_streak: None,
            task_complete_pending: false,
            unified_exec_processes: Vec::new(),
            agent_turn_running: false,
            mcp_startup_status: None,
            connectors_cache: ConnectorsCacheState::default(),
            interrupts: InterruptManager::new(),
            reasoning_buffer: String::new(),
            full_reasoning_buffer: String::new(),
            current_status_header: String::from("Working"),
            retry_status_header: None,
            active_runtime_context: None,
            running_turn_model: None,
            running_turn_reasoning_effort: None,
            thread_id: None,
            thread_name: None,
            has_completed_assistant_message: false,
            last_assistant_output_markdown: None,
            copyable_messages: Vec::new(),
            copy_code_ui_state: None,
            copy_message_ui_state: None,
            forked_from: None,
            queued_user_messages: VecDeque::new(),
            next_queued_user_message_id: 1,
            queued_edit_state: None,
            show_welcome_banner: is_first_run,
            suppress_session_configured_redraw: false,
            pending_notification: None,
            quit_shortcut_expires_at: None,
            quit_shortcut_key: None,
            is_review_mode: false,
            pre_review_token_info: None,
            needs_final_message_separator: false,
            had_work_activity: false,
            turn_progress_trace: Vec::new(),
            pending_separator_progress_trace: None,
            progress_legend_mode,
            progress_trace_styles,
            saw_plan_update_this_turn: false,
            saw_plan_item_this_turn: false,
            plan_delta_buffer: String::new(),
            plan_item_active: false,
            last_separator_elapsed_secs: None,
            last_rendered_width: std::cell::Cell::new(None),
            feedback,
            feedback_audience,
            current_rollout_path: None,
            current_cwd,
            status_line_invalid_items_warned,
            status_line_branch: None,
            status_line_branch_cwd: None,
            status_line_branch_pending: false,
            status_line_branch_lookup_complete: false,
            external_editor_state: ExternalEditorState::Closed,
        };

        for warning in progress_trace_style_warnings {
            widget.add_to_history(history_cell::new_warning_event(warning));
        }

        widget
            .bottom_pane
            .set_keybindings(widget.keybindings.clone());
        widget.prefetch_rate_limits();
        widget
            .bottom_pane
            .set_steer_enabled(widget.config.features.enabled(Feature::Steer));
        let status_line_enabled = !widget.status_line_items_with_invalids().0.is_empty();
        widget
            .bottom_pane
            .set_status_line_enabled(status_line_enabled);
        widget.bottom_pane.set_collaboration_modes_enabled(
            widget.config.features.enabled(Feature::CollaborationModes),
        );
        widget.sync_personality_command_enabled();
        #[cfg(target_os = "windows")]
        widget.bottom_pane.set_windows_degraded_sandbox_active(
            codex_core::windows_sandbox::ELEVATED_SANDBOX_NUX_ENABLED
                && matches!(
                    WindowsSandboxLevel::from_config(&widget.config),
                    WindowsSandboxLevel::RestrictedToken
                ),
        );
        widget.update_collaboration_mode_indicator();
        widget.refresh_model_display();
        widget.refresh_status_line();

        widget
            .bottom_pane
            .set_connectors_enabled(widget.config.features.enabled(Feature::Apps));

        widget
    }

    pub(crate) fn new_with_op_sender(
        common: ChatWidgetInit,
        codex_op_tx: UnboundedSender<Op>,
    ) -> Self {
        let ChatWidgetInit {
            config,
            frame_requester,
            app_event_tx,
            initial_user_message,
            enhanced_keys_supported,
            auth_manager,
            models_manager,
            feedback,
            is_first_run,
            feedback_audience,
            model,
            status_line_invalid_items_warned,
            otel_manager,
        } = common;
        let model = model.filter(|m| !m.trim().is_empty());
        let mut config = config;
        config.model = model.clone();
        let keybindings = Self::resolve_keybindings(&config, enhanced_keys_supported);
        let progress_legend_mode = config.tui_progress_legend_mode;
        let (progress_trace_styles, progress_trace_style_warnings) =
            resolve_progress_trace_styles(config.tui_progress_trace_style.as_ref());
        crate::markdown_render::set_syntax_highlight_theme(
            config.tui_syntax_highlight_theme.clone(),
        );
        let mut rng = rand::rng();
        let placeholder = PLACEHOLDERS[rng.random_range(0..PLACEHOLDERS.len())].to_string();

        let model_override = model.as_deref();
        let model_for_header = model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL_DISPLAY_NAME.to_string());
        let active_collaboration_mask =
            Self::initial_collaboration_mask(&config, models_manager.as_ref(), model_override);
        let header_model = active_collaboration_mask
            .as_ref()
            .and_then(|mask| mask.model.clone())
            .unwrap_or_else(|| model_for_header.clone());
        let fallback_default = Settings {
            model: header_model.clone(),
            reasoning_effort: None,
            developer_instructions: None,
        };
        // Collaboration modes start in Default mode.
        let current_collaboration_mode = CollaborationMode {
            mode: ModeKind::Default,
            settings: fallback_default,
        };

        let active_cell = Some(Self::placeholder_session_header_cell(&config));
        let current_cwd = Some(config.cwd.clone());

        let mut widget = Self {
            app_event_tx: app_event_tx.clone(),
            frame_requester: frame_requester.clone(),
            codex_op_tx,
            bottom_pane: BottomPane::new(BottomPaneParams {
                frame_requester,
                app_event_tx,
                has_input_focus: true,
                enhanced_keys_supported,
                placeholder_text: placeholder,
                disable_paste_burst: config.disable_paste_burst,
                animations_enabled: config.animations,
                progress_legend_mode,
                progress_trace_styles,
                skills: None,
            }),
            active_cell,
            active_cell_revision: 0,
            config,
            keybindings,
            skills_all: Vec::new(),
            skills_initial_state: None,
            current_collaboration_mode,
            active_collaboration_mask,
            auth_manager,
            models_manager,
            otel_manager,
            session_header: SessionHeader::new(header_model),
            initial_user_message,
            token_info: None,
            rate_limit_snapshot: None,
            plan_type: None,
            rate_limit_warnings: RateLimitWarningState::default(),
            rate_limit_switch_prompt: RateLimitSwitchPromptState::default(),
            rate_limit_poller: None,
            adaptive_chunking: AdaptiveChunkingPolicy::default(),
            stream_controller: None,
            plan_stream_controller: None,
            last_copyable_output: None,
            running_commands: HashMap::new(),
            suppressed_exec_calls: HashSet::new(),
            last_unified_wait: None,
            unified_exec_wait_streak: None,
            task_complete_pending: false,
            unified_exec_processes: Vec::new(),
            agent_turn_running: false,
            mcp_startup_status: None,
            connectors_cache: ConnectorsCacheState::default(),
            interrupts: InterruptManager::new(),
            reasoning_buffer: String::new(),
            full_reasoning_buffer: String::new(),
            current_status_header: String::from("Working"),
            retry_status_header: None,
            active_runtime_context: None,
            running_turn_model: None,
            running_turn_reasoning_effort: None,
            thread_id: None,
            thread_name: None,
            has_completed_assistant_message: false,
            last_assistant_output_markdown: None,
            copyable_messages: Vec::new(),
            copy_code_ui_state: None,
            copy_message_ui_state: None,
            forked_from: None,
            saw_plan_update_this_turn: false,
            saw_plan_item_this_turn: false,
            plan_delta_buffer: String::new(),
            plan_item_active: false,
            queued_user_messages: VecDeque::new(),
            next_queued_user_message_id: 1,
            queued_edit_state: None,
            show_welcome_banner: is_first_run,
            suppress_session_configured_redraw: false,
            pending_notification: None,
            quit_shortcut_expires_at: None,
            quit_shortcut_key: None,
            is_review_mode: false,
            pre_review_token_info: None,
            needs_final_message_separator: false,
            had_work_activity: false,
            turn_progress_trace: Vec::new(),
            pending_separator_progress_trace: None,
            progress_legend_mode,
            progress_trace_styles,
            last_separator_elapsed_secs: None,
            last_rendered_width: std::cell::Cell::new(None),
            feedback,
            feedback_audience,
            current_rollout_path: None,
            current_cwd,
            status_line_invalid_items_warned,
            status_line_branch: None,
            status_line_branch_cwd: None,
            status_line_branch_pending: false,
            status_line_branch_lookup_complete: false,
            external_editor_state: ExternalEditorState::Closed,
        };

        for warning in progress_trace_style_warnings {
            widget.add_to_history(history_cell::new_warning_event(warning));
        }

        widget
            .bottom_pane
            .set_keybindings(widget.keybindings.clone());
        widget.prefetch_rate_limits();
        widget
            .bottom_pane
            .set_steer_enabled(widget.config.features.enabled(Feature::Steer));
        let status_line_enabled = !widget.status_line_items_with_invalids().0.is_empty();
        widget
            .bottom_pane
            .set_status_line_enabled(status_line_enabled);
        widget.bottom_pane.set_collaboration_modes_enabled(
            widget.config.features.enabled(Feature::CollaborationModes),
        );
        widget.sync_personality_command_enabled();
        widget.refresh_status_line();

        widget
    }

    /// Create a ChatWidget attached to an existing conversation (e.g., a fork).
    pub(crate) fn new_from_existing(
        common: ChatWidgetInit,
        conversation: std::sync::Arc<codex_core::CodexThread>,
        session_configured: codex_core::protocol::SessionConfiguredEvent,
    ) -> Self {
        let ChatWidgetInit {
            config,
            frame_requester,
            app_event_tx,
            initial_user_message,
            enhanced_keys_supported,
            auth_manager,
            models_manager,
            feedback,
            is_first_run: _,
            feedback_audience,
            model,
            status_line_invalid_items_warned,
            otel_manager,
        } = common;
        let model = model.filter(|m| !m.trim().is_empty());
        let keybindings = Self::resolve_keybindings(&config, enhanced_keys_supported);
        let progress_legend_mode = config.tui_progress_legend_mode;
        let (progress_trace_styles, progress_trace_style_warnings) =
            resolve_progress_trace_styles(config.tui_progress_trace_style.as_ref());
        crate::markdown_render::set_syntax_highlight_theme(
            config.tui_syntax_highlight_theme.clone(),
        );
        let mut rng = rand::rng();
        let placeholder = PLACEHOLDERS[rng.random_range(0..PLACEHOLDERS.len())].to_string();

        let model_override = model.as_deref();
        let header_model = model
            .clone()
            .unwrap_or_else(|| session_configured.model.clone());
        let active_collaboration_mask =
            Self::initial_collaboration_mask(&config, models_manager.as_ref(), model_override);
        let header_model = active_collaboration_mask
            .as_ref()
            .and_then(|mask| mask.model.clone())
            .unwrap_or(header_model);

        let current_cwd = Some(session_configured.cwd.clone());
        let codex_op_tx =
            spawn_agent_from_existing(conversation, session_configured, app_event_tx.clone());

        let fallback_default = Settings {
            model: header_model.clone(),
            reasoning_effort: None,
            developer_instructions: None,
        };
        // Collaboration modes start in Default mode.
        let current_collaboration_mode = CollaborationMode {
            mode: ModeKind::Default,
            settings: fallback_default,
        };

        let mut widget = Self {
            app_event_tx: app_event_tx.clone(),
            frame_requester: frame_requester.clone(),
            codex_op_tx,
            bottom_pane: BottomPane::new(BottomPaneParams {
                frame_requester,
                app_event_tx,
                has_input_focus: true,
                enhanced_keys_supported,
                placeholder_text: placeholder,
                disable_paste_burst: config.disable_paste_burst,
                animations_enabled: config.animations,
                progress_legend_mode,
                progress_trace_styles,
                skills: None,
            }),
            active_cell: None,
            active_cell_revision: 0,
            config,
            keybindings,
            skills_all: Vec::new(),
            skills_initial_state: None,
            current_collaboration_mode,
            active_collaboration_mask,
            auth_manager,
            models_manager,
            otel_manager,
            session_header: SessionHeader::new(header_model),
            initial_user_message,
            token_info: None,
            rate_limit_snapshot: None,
            plan_type: None,
            rate_limit_warnings: RateLimitWarningState::default(),
            rate_limit_switch_prompt: RateLimitSwitchPromptState::default(),
            rate_limit_poller: None,
            adaptive_chunking: AdaptiveChunkingPolicy::default(),
            stream_controller: None,
            plan_stream_controller: None,
            last_copyable_output: None,
            running_commands: HashMap::new(),
            suppressed_exec_calls: HashSet::new(),
            last_unified_wait: None,
            unified_exec_wait_streak: None,
            task_complete_pending: false,
            unified_exec_processes: Vec::new(),
            agent_turn_running: false,
            mcp_startup_status: None,
            connectors_cache: ConnectorsCacheState::default(),
            interrupts: InterruptManager::new(),
            reasoning_buffer: String::new(),
            full_reasoning_buffer: String::new(),
            current_status_header: String::from("Working"),
            retry_status_header: None,
            active_runtime_context: None,
            running_turn_model: None,
            running_turn_reasoning_effort: None,
            thread_id: None,
            thread_name: None,
            has_completed_assistant_message: false,
            last_assistant_output_markdown: None,
            copyable_messages: Vec::new(),
            copy_code_ui_state: None,
            copy_message_ui_state: None,
            forked_from: None,
            queued_user_messages: VecDeque::new(),
            next_queued_user_message_id: 1,
            queued_edit_state: None,
            show_welcome_banner: false,
            suppress_session_configured_redraw: true,
            pending_notification: None,
            quit_shortcut_expires_at: None,
            quit_shortcut_key: None,
            is_review_mode: false,
            pre_review_token_info: None,
            needs_final_message_separator: false,
            had_work_activity: false,
            turn_progress_trace: Vec::new(),
            pending_separator_progress_trace: None,
            progress_legend_mode,
            progress_trace_styles,
            saw_plan_update_this_turn: false,
            saw_plan_item_this_turn: false,
            plan_delta_buffer: String::new(),
            plan_item_active: false,
            last_separator_elapsed_secs: None,
            last_rendered_width: std::cell::Cell::new(None),
            feedback,
            feedback_audience,
            current_rollout_path: None,
            current_cwd,
            status_line_invalid_items_warned,
            status_line_branch: None,
            status_line_branch_cwd: None,
            status_line_branch_pending: false,
            status_line_branch_lookup_complete: false,
            external_editor_state: ExternalEditorState::Closed,
        };

        for warning in progress_trace_style_warnings {
            widget.add_to_history(history_cell::new_warning_event(warning));
        }

        widget
            .bottom_pane
            .set_keybindings(widget.keybindings.clone());
        widget.prefetch_rate_limits();
        widget
            .bottom_pane
            .set_steer_enabled(widget.config.features.enabled(Feature::Steer));
        let status_line_enabled = !widget.status_line_items_with_invalids().0.is_empty();
        widget
            .bottom_pane
            .set_status_line_enabled(status_line_enabled);
        widget.bottom_pane.set_collaboration_modes_enabled(
            widget.config.features.enabled(Feature::CollaborationModes),
        );
        widget.sync_personality_command_enabled();
        #[cfg(target_os = "windows")]
        widget.bottom_pane.set_windows_degraded_sandbox_active(
            codex_core::windows_sandbox::ELEVATED_SANDBOX_NUX_ENABLED
                && matches!(
                    WindowsSandboxLevel::from_config(&widget.config),
                    WindowsSandboxLevel::RestrictedToken
                ),
        );
        widget.update_collaboration_mode_indicator();
        widget.refresh_model_display();
        widget.refresh_status_line();

        widget
    }

    pub(crate) fn handle_key_event(&mut self, key_event: KeyEvent) {
        match key_event {
            KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                kind: KeyEventKind::Press,
                ..
            } if modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'c') => {
                self.on_ctrl_c();
                return;
            }
            KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                kind: KeyEventKind::Press,
                ..
            } if modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'d') => {
                if self.on_ctrl_d() {
                    return;
                }
                self.bottom_pane.clear_quit_shortcut_hint();
                self.quit_shortcut_expires_at = None;
                self.quit_shortcut_key = None;
            }
            key_event
                if key_event.kind == KeyEventKind::Press
                    && self
                        .keybindings
                        .paste
                        .iter()
                        .any(|binding| binding.matches(&key_event)) =>
            {
                self.paste_from_clipboard();
                return;
            }
            key_event
                if key_event.kind == KeyEventKind::Press
                    && self
                        .keybindings
                        .copy_prompt
                        .iter()
                        .any(|binding| binding.matches(&key_event)) =>
            {
                self.copy_prompt_to_clipboard();
                return;
            }
            key_event
                if key_event.kind == KeyEventKind::Press
                    && self
                        .keybindings
                        .copy_last_output
                        .iter()
                        .any(|binding| binding.matches(&key_event)) =>
            {
                self.copy_last_output_to_clipboard();
                return;
            }
            key_event
                if key_event.kind == KeyEventKind::Press
                    && self
                        .keybindings
                        .copy_code_block
                        .iter()
                        .any(|binding| binding.matches(&key_event)) =>
            {
                self.open_copy_code_block_picker();
                return;
            }
            other if other.kind == KeyEventKind::Press => {
                self.bottom_pane.clear_quit_shortcut_hint();
                self.quit_shortcut_expires_at = None;
                self.quit_shortcut_key = None;
            }
            _ => {}
        }

        if self.queued_edit_state.is_some()
            && self.bottom_pane.no_modal_or_popup_active()
            && self.handle_queue_edit_key_event(key_event)
        {
            return;
        }

        match key_event {
            KeyEvent {
                code: KeyCode::Char('o' | 'O'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('\u{000f}'),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                ..
            } if self.bottom_pane.no_modal_or_popup_active()
                && !self.queued_user_messages.is_empty()
                && self.queued_edit_state.is_none() =>
            {
                self.open_queue_popup();
            }
            KeyEvent {
                code: KeyCode::Char('y' | 'Y'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('\u{0019}'),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                ..
            } if self.bottom_pane.no_modal_or_popup_active()
                && !self.bottom_pane.is_task_running()
                && !self.queued_user_messages.is_empty()
                && self.queued_edit_state.is_none() =>
            {
                self.send_next_queued_user_message();
            }
            KeyEvent {
                code: KeyCode::Right,
                modifiers,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } if modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
                && self.bottom_pane.no_modal_or_popup_active() =>
            {
                self.cycle_model_shortcut(1);
            }
            KeyEvent {
                code: KeyCode::Left,
                modifiers,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } if modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
                && self.bottom_pane.no_modal_or_popup_active() =>
            {
                self.cycle_model_shortcut(-1);
            }
            KeyEvent {
                code: KeyCode::Down,
                modifiers,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } if modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
                && self.bottom_pane.no_modal_or_popup_active() =>
            {
                self.cycle_reasoning_effort_shortcut(-1);
            }
            KeyEvent {
                code: KeyCode::Up,
                modifiers,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } if modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
                && self.bottom_pane.no_modal_or_popup_active() =>
            {
                self.cycle_reasoning_effort_shortcut(1);
            }
            KeyEvent {
                code: KeyCode::BackTab,
                kind: KeyEventKind::Press,
                ..
            } if self.collaboration_modes_enabled()
                && !self.bottom_pane.is_task_running()
                && self.bottom_pane.no_modal_or_popup_active() =>
            {
                self.cycle_collaboration_mode();
            }
            KeyEvent {
                code: KeyCode::Char('q' | 'Q'),
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press,
                ..
            } if !self.queued_user_messages.is_empty() && self.queued_edit_state.is_none() => {
                self.open_queue_popup();
            }
            KeyEvent {
                code: KeyCode::Up,
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press,
                ..
            } if !self.queued_user_messages.is_empty() => {
                self.begin_queue_edit_most_recent();
            }
            _ => match self.bottom_pane.handle_key_event(key_event) {
                InputResult::Submitted {
                    text,
                    text_elements,
                } => {
                    let user_message = UserMessage {
                        text,
                        local_images: self
                            .bottom_pane
                            .take_recent_submission_images_with_placeholders(),
                        text_elements,
                        mention_paths: self.bottom_pane.take_mention_paths(),
                    };
                    if self.is_session_configured() {
                        // Submitted is only emitted when steer is enabled (Enter sends immediately).
                        // Reset any reasoning header only when we are actually submitting a turn.
                        self.reasoning_buffer.clear();
                        self.full_reasoning_buffer.clear();
                        self.set_status_header(String::from("Working"));
                        self.submit_user_message(user_message);
                    } else {
                        self.queue_user_message(user_message);
                    }
                }
                InputResult::Queued {
                    text,
                    text_elements,
                } => {
                    let user_message = UserMessage {
                        text,
                        local_images: self
                            .bottom_pane
                            .take_recent_submission_images_with_placeholders(),
                        text_elements,
                        mention_paths: self.bottom_pane.take_mention_paths(),
                    };
                    self.queue_user_message(user_message);
                }
                InputResult::Command(cmd) => {
                    self.dispatch_command(cmd);
                }
                InputResult::CommandWithArgs(cmd, args, text_elements) => {
                    self.dispatch_command_with_args(cmd, args, text_elements);
                }
                InputResult::None => {}
            },
        }
    }

    /// Attach a local image to the composer when the active model supports image inputs.
    ///
    /// When the model does not advertise image support, we keep the draft unchanged and surface a
    /// warning event so users can switch models or remove attachments.
    pub(crate) fn attach_image(&mut self, path: PathBuf) {
        if !self.current_model_supports_images() {
            self.add_to_history(history_cell::new_warning_event(
                self.image_inputs_not_supported_message(),
            ));
            self.request_redraw();
            return;
        }
        tracing::info!("attach_image path={path:?}");
        self.bottom_pane.attach_image(path);
        self.request_redraw();
    }

    pub(crate) fn composer_text_with_pending(&self) -> String {
        self.bottom_pane.composer_text_with_pending()
    }

    pub(crate) fn apply_external_edit(&mut self, text: String) {
        self.bottom_pane.apply_external_edit(text);
        self.request_redraw();
    }

    pub(crate) fn external_editor_state(&self) -> ExternalEditorState {
        self.external_editor_state
    }

    pub(crate) fn set_external_editor_state(&mut self, state: ExternalEditorState) {
        self.external_editor_state = state;
    }

    pub(crate) fn set_footer_hint_override(&mut self, items: Option<Vec<(String, String)>>) {
        self.bottom_pane.set_footer_hint_override(items);
    }

    pub(crate) fn show_selection_view(&mut self, params: SelectionViewParams) {
        self.bottom_pane.show_selection_view(params);
        self.request_redraw();
    }

    pub(crate) fn can_launch_external_editor(&self) -> bool {
        self.bottom_pane.can_launch_external_editor()
    }

    fn dispatch_command(&mut self, cmd: SlashCommand) {
        if !cmd.available_during_task() && self.bottom_pane.is_task_running() {
            let message = format!(
                "'/{}' is disabled while a task is in progress.",
                cmd.command()
            );
            self.add_to_history(history_cell::new_error_event(message));
            self.bottom_pane.drain_pending_submission_state();
            self.request_redraw();
            return;
        }
        match cmd {
            SlashCommand::Feedback => {
                if !self.config.feedback_enabled {
                    let params = crate::bottom_pane::feedback_disabled_params();
                    self.bottom_pane.show_selection_view(params);
                    self.request_redraw();
                    return;
                }
                // Step 1: pick a category (UI built in feedback_view)
                let params =
                    crate::bottom_pane::feedback_selection_params(self.app_event_tx.clone());
                self.bottom_pane.show_selection_view(params);
                self.request_redraw();
            }
            SlashCommand::New => {
                self.app_event_tx.send(AppEvent::NewSession);
            }
            SlashCommand::Resume => {
                self.app_event_tx.send(AppEvent::OpenResumePicker);
            }
            SlashCommand::Session => {
                self.app_event_tx.send(AppEvent::OpenSessionsPicker {
                    view: crate::sessions_picker::SessionView::Active,
                });
            }
            SlashCommand::Archived => {
                self.app_event_tx.send(AppEvent::OpenSessionsPicker {
                    view: crate::sessions_picker::SessionView::Archived,
                });
            }
            SlashCommand::Fork => {
                self.app_event_tx.send(AppEvent::ForkCurrentSession);
            }
            SlashCommand::Init => {
                let init_target = self.config.cwd.join(DEFAULT_PROJECT_DOC_FILENAME);
                if init_target.exists() {
                    let message = format!(
                        "{DEFAULT_PROJECT_DOC_FILENAME} already exists here. Skipping /init to avoid overwriting it."
                    );
                    self.add_info_message(message, None);
                    return;
                }
                const INIT_PROMPT: &str = include_str!("../prompt_for_init_command.md");
                self.submit_user_message(INIT_PROMPT.to_string().into());
            }
            SlashCommand::Compact => {
                self.clear_token_usage();
                self.app_event_tx.send(AppEvent::CodexOp(Op::Compact));
            }
            SlashCommand::Pause => {
                if self.bottom_pane.is_task_running() {
                    self.submit_op(Op::Pause);
                } else {
                    self.add_info_message("No running turn to pause.".to_string(), None);
                }
            }
            SlashCommand::Continue => {
                if self.bottom_pane.is_task_running() {
                    self.add_info_message(
                        "A turn is already running.".to_string(),
                        Some("Pause or interrupt it before continuing another turn.".to_string()),
                    );
                } else {
                    self.submit_op(Op::Continue);
                }
            }
            SlashCommand::Review => {
                self.open_review_popup();
            }
            SlashCommand::ReviewCompletedTurn => {
                self.submit_op(Op::ReviewCompletedTurn);
            }
            SlashCommand::Rename => {
                self.open_rename_thread_view();
            }
            SlashCommand::Export => {
                self.open_export_picker();
            }
            SlashCommand::Model => {
                self.open_model_popup();
            }
            SlashCommand::Personality => {
                self.open_personality_popup();
            }
            SlashCommand::Plan => {
                if !self.collaboration_modes_enabled() {
                    self.add_info_message(
                        "Collaboration modes are disabled.".to_string(),
                        Some("Enable collaboration modes to use /plan.".to_string()),
                    );
                    return;
                }
                if let Some(mask) = collaboration_modes::plan_mask(self.models_manager.as_ref()) {
                    self.set_collaboration_mask(mask);
                } else {
                    self.add_info_message("Plan mode unavailable right now.".to_string(), None);
                }
            }
            SlashCommand::Collab => {
                if !self.collaboration_modes_enabled() {
                    self.add_info_message(
                        "Collaboration modes are disabled.".to_string(),
                        Some("Enable collaboration modes to use /collab.".to_string()),
                    );
                    return;
                }
                self.open_collaboration_modes_popup();
            }
            SlashCommand::Agent => {
                self.app_event_tx.send(AppEvent::OpenAgentPicker);
            }
            SlashCommand::Approvals => {
                self.open_approvals_popup();
            }
            SlashCommand::Permissions => {
                self.open_permissions_popup();
            }
            SlashCommand::ElevateSandbox => {
                #[cfg(target_os = "windows")]
                {
                    let windows_sandbox_level = WindowsSandboxLevel::from_config(&self.config);
                    let windows_degraded_sandbox_enabled =
                        matches!(windows_sandbox_level, WindowsSandboxLevel::RestrictedToken);
                    if !windows_degraded_sandbox_enabled
                        || !codex_core::windows_sandbox::ELEVATED_SANDBOX_NUX_ENABLED
                    {
                        // This command should not be visible/recognized outside degraded mode,
                        // but guard anyway in case something dispatches it directly.
                        return;
                    }

                    let Some(preset) = builtin_approval_presets()
                        .into_iter()
                        .find(|preset| preset.id == "auto")
                    else {
                        // Avoid panicking in interactive UI; treat this as a recoverable
                        // internal error.
                        self.add_error_message(
                            "Internal error: missing the 'auto' approval preset.".to_string(),
                        );
                        return;
                    };

                    if let Err(err) = self.config.approval_policy.can_set(&preset.approval) {
                        self.add_error_message(err.to_string());
                        return;
                    }

                    self.otel_manager.counter(
                        "codex.windows_sandbox.setup_elevated_sandbox_command",
                        1,
                        &[],
                    );
                    self.app_event_tx
                        .send(AppEvent::BeginWindowsSandboxElevatedSetup { preset });
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = &self.otel_manager;
                    // Not supported; on non-Windows this command should never be reachable.
                };
            }
            SlashCommand::Experimental => {
                self.open_experimental_popup();
            }
            SlashCommand::Quit | SlashCommand::Exit => {
                self.request_quit_without_confirmation();
            }
            SlashCommand::Logout => {
                if let Err(e) = codex_core::auth::logout(
                    &self.config.codex_home,
                    self.config.cli_auth_credentials_store_mode,
                ) {
                    tracing::error!("failed to logout: {e}");
                }
                self.request_quit_without_confirmation();
            }
            // SlashCommand::Undo => {
            //     self.app_event_tx.send(AppEvent::CodexOp(Op::Undo));
            // }
            SlashCommand::Diff => {
                self.add_diff_in_progress();
                let tx = self.app_event_tx.clone();
                let cwd = self.config.cwd.clone();
                let diff_view = self.config.diff_view;
                let syntax_theme = self.config.tui_syntax_highlight_theme.clone();
                let width = self.last_rendered_width.get().unwrap_or(80);
                tokio::spawn(async move {
                    let result = match get_git_diff(&cwd, diff_view, width, &syntax_theme).await {
                        Ok(result) => result,
                        Err(e) => GitDiffResult::Error(format!("Failed to compute diff: {e}")),
                    };
                    tx.send(AppEvent::DiffResult(result));
                });
            }
            SlashCommand::Copy => {
                let Some(text) = self.last_copyable_output.as_deref() else {
                    self.add_info_message(
                        "`/copy` is unavailable before the first Codex output or right after a rollback."
                            .to_string(),
                        None,
                    );
                    return;
                };

                match clipboard_text::copy_text_to_clipboard(text) {
                    Ok(()) => {
                        let hint = self.agent_turn_running.then_some(
                            "Current turn is still running; copied the latest completed output (not the in-progress response)."
                                .to_string(),
                        );
                        self.add_info_message(
                            "Copied latest Codex output to clipboard.".to_string(),
                            hint,
                        );
                    }
                    Err(err) => {
                        self.add_error_message(format!("Failed to copy to clipboard: {err}"))
                    }
                }
            }
            SlashCommand::Mention => {
                self.insert_str("@");
            }
            SlashCommand::CopyCodeBlock => {
                self.open_copy_code_block_picker();
            }
            SlashCommand::CopyMessage => {
                self.open_copy_message_picker(CopyMessageFilter::Responses);
            }
            SlashCommand::Skills => {
                self.open_skills_menu();
            }
            SlashCommand::Status => {
                self.add_status_output();
            }
            SlashCommand::DebugConfig => {
                self.add_debug_config_output();
            }
            SlashCommand::Statusline => {
                self.open_status_line_setup();
            }
            SlashCommand::Legend => {
                self.open_progress_legend_popup();
            }
            SlashCommand::LegendMode => {
                self.add_info_message(
                    format!("Progress legend mode is '{}'.", self.progress_legend_mode),
                    Some("Use /legend-mode off|auto|always to change it.".to_string()),
                );
            }
            SlashCommand::Ps => {
                self.add_ps_output();
            }
            SlashCommand::Mcp => {
                self.add_mcp_output();
            }
            SlashCommand::Apps => {
                self.add_connectors_output();
            }
            SlashCommand::Queue => {
                if self.queued_user_messages.is_empty() {
                    self.add_info_message("Queue is empty.".to_string(), None);
                } else {
                    self.open_queue_popup();
                }
            }
            SlashCommand::Rollout => {
                if let Some(path) = self.rollout_path() {
                    self.add_info_message(
                        format!("Current rollout path: {}", path.display()),
                        None,
                    );
                } else {
                    self.add_info_message("Rollout path is not available yet.".to_string(), None);
                }
            }
            SlashCommand::TestApproval => {
                use codex_core::protocol::EventMsg;
                use std::collections::HashMap;

                use codex_core::protocol::ApplyPatchApprovalRequestEvent;
                use codex_core::protocol::FileChange;

                self.app_event_tx.send(AppEvent::CodexEvent(Event {
                    id: "1".to_string(),
                    // msg: EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
                    //     call_id: "1".to_string(),
                    //     command: vec!["git".into(), "apply".into()],
                    //     cwd: self.config.cwd.clone(),
                    //     reason: Some("test".to_string()),
                    // }),
                    msg: EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
                        call_id: "1".to_string(),
                        turn_id: "turn-1".to_string(),
                        changes: HashMap::from([
                            (
                                PathBuf::from("/tmp/test.txt"),
                                FileChange::Add {
                                    content: "test".to_string(),
                                },
                            ),
                            (
                                PathBuf::from("/tmp/test2.txt"),
                                FileChange::Update {
                                    unified_diff: "+test\n-test2".to_string(),
                                    move_path: None,
                                },
                            ),
                        ]),
                        reason: None,
                        grant_root: Some(PathBuf::from("/tmp")),
                    }),
                }));
            }
        }
    }

    fn dispatch_command_with_args(
        &mut self,
        cmd: SlashCommand,
        args: String,
        _text_elements: Vec<TextElement>,
    ) {
        if !cmd.supports_inline_args() {
            self.dispatch_command(cmd);
            return;
        }
        if !cmd.available_during_task() && self.bottom_pane.is_task_running() {
            let message = format!(
                "'/{}' is disabled while a task is in progress.",
                cmd.command()
            );
            self.add_to_history(history_cell::new_error_event(message));
            self.request_redraw();
            return;
        }

        let trimmed = args.trim();
        match cmd {
            SlashCommand::Export if !trimmed.is_empty() => {
                match parse_export_args(trimmed, &self.config.cwd) {
                    Ok(parsed) => {
                        self.start_export(parsed.format, parsed.overrides);
                        self.bottom_pane.drain_pending_submission_state();
                    }
                    Err(message) => {
                        self.add_error_message(message);
                    }
                }
            }
            SlashCommand::Rename if !trimmed.is_empty() => {
                let Some((prepared_args, _prepared_elements)) =
                    self.bottom_pane.prepare_inline_args_submission(false)
                else {
                    return;
                };
                let Some(name) = codex_core::util::normalize_thread_name(&prepared_args) else {
                    self.add_error_message("Thread name cannot be empty.".to_string());
                    return;
                };
                self.app_event_tx
                    .send(AppEvent::CodexOp(Op::SetThreadName { name }));
                self.bottom_pane.drain_pending_submission_state();
            }
            SlashCommand::Plan if !trimmed.is_empty() => {
                self.dispatch_command(cmd);
                if self.active_mode_kind() != ModeKind::Plan {
                    return;
                }
                let Some((prepared_args, prepared_elements)) =
                    self.bottom_pane.prepare_inline_args_submission(true)
                else {
                    return;
                };
                let user_message = UserMessage {
                    text: prepared_args,
                    local_images: self
                        .bottom_pane
                        .take_recent_submission_images_with_placeholders(),
                    text_elements: prepared_elements,
                    mention_paths: self.bottom_pane.take_mention_paths(),
                };
                if self.is_session_configured() {
                    self.reasoning_buffer.clear();
                    self.full_reasoning_buffer.clear();
                    self.set_status_header(String::from("Working"));
                    self.submit_user_message(user_message);
                } else {
                    self.queue_user_message(user_message);
                }
            }
            SlashCommand::Review if !trimmed.is_empty() => {
                let Some((prepared_args, _prepared_elements)) =
                    self.bottom_pane.prepare_inline_args_submission(false)
                else {
                    return;
                };
                self.submit_op(Op::Review {
                    review_request: ReviewRequest {
                        target: ReviewTarget::Custom {
                            instructions: prepared_args,
                        },
                        user_facing_hint: None,
                    },
                });
                self.bottom_pane.drain_pending_submission_state();
            }
            SlashCommand::Diff => {
                let diff_view = match diff_view_override_from_args(trimmed, self.config.diff_view) {
                    Ok(view) => view,
                    Err(message) => {
                        self.add_error_message(message);
                        return;
                    }
                };
                self.add_diff_in_progress();
                let tx = self.app_event_tx.clone();
                let cwd = self.config.cwd.clone();
                let syntax_theme = self.config.tui_syntax_highlight_theme.clone();
                let width = self.last_rendered_width.get().unwrap_or(80);
                tokio::spawn(async move {
                    let result = match get_git_diff(&cwd, diff_view, width, &syntax_theme).await {
                        Ok(result) => result,
                        Err(e) => GitDiffResult::Error(format!("Failed to compute diff: {e}")),
                    };
                    tx.send(AppEvent::DiffResult(result));
                });
            }
            SlashCommand::LegendMode => match parse_progress_legend_mode(trimmed) {
                Ok(mode) => {
                    self.app_event_tx
                        .send(AppEvent::SetProgressLegendMode { mode });
                    self.bottom_pane.drain_pending_submission_state();
                }
                Err(err) => {
                    self.add_error_message(err);
                }
            },
            _ => self.dispatch_command(cmd),
        }
    }

    fn open_rename_thread_view(&mut self) {
        let mut items = Vec::new();
        items.push(SelectionItem {
            name: "Rename manually".to_string(),
            description: Some("Set a custom name.".to_string()),
            actions: vec![Box::new(|tx| {
                tx.send(AppEvent::OpenRenameThreadPrompt);
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        if self.has_completed_assistant_message {
            items.push(SelectionItem {
                name: "Generate thread name".to_string(),
                description: Some("Generate a name automatically.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::CodexOp(Op::AutoRenameThread));
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Rename thread".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
        self.request_redraw();
    }

    pub(crate) fn show_rename_prompt(&mut self) {
        let tx = self.app_event_tx.clone();
        let has_name = self
            .thread_name
            .as_ref()
            .is_some_and(|name| !name.is_empty());
        let title = if has_name {
            "Rename thread"
        } else {
            "Name thread"
        };
        let view = CustomPromptView::new(
            title.to_string(),
            "Type a name and press Enter".to_string(),
            None,
            Box::new(move |name: String| {
                let Some(name) = codex_core::util::normalize_thread_name(&name) else {
                    tx.send(AppEvent::InsertHistoryCell(Box::new(
                        history_cell::new_error_event("Thread name cannot be empty.".to_string()),
                    )));
                    return;
                };
                tx.send(AppEvent::CodexOp(Op::SetThreadName { name }));
            }),
        );

        self.bottom_pane.show_view(Box::new(view));
    }

    fn open_export_picker(&mut self) {
        if self.current_rollout_path.is_none() {
            self.add_info_message("Export is not available yet.".to_string(), None);
            return;
        }

        let items = vec![
            SelectionItem {
                name: "Markdown (.md)".to_string(),
                description: Some("Readable transcript for sharing.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::ExportChat {
                        format: Some(ChatExportFormat::Markdown),
                        overrides: ExportOverrides::default(),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Markdown (.md) in current dir".to_string(),
                description: Some("Creates a file in the current directory.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::ExportChat {
                        format: Some(ChatExportFormat::Markdown),
                        overrides: ExportOverrides {
                            output_dir: Some(PathBuf::from(".")),
                            ..Default::default()
                        },
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Markdown (custom path...)".to_string(),
                description: Some("Choose a destination path.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::OpenExportPathPrompt {
                        format: ChatExportFormat::Markdown,
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "JSON (.json)".to_string(),
                description: Some("Structured messages for tooling.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::ExportChat {
                        format: Some(ChatExportFormat::Json),
                        overrides: ExportOverrides::default(),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "JSON (.json) in current dir".to_string(),
                description: Some("Creates a file in the current directory.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::ExportChat {
                        format: Some(ChatExportFormat::Json),
                        overrides: ExportOverrides {
                            output_dir: Some(PathBuf::from(".")),
                            ..Default::default()
                        },
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "JSON (custom path...)".to_string(),
                description: Some("Choose a destination path.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::OpenExportPathPrompt {
                        format: ChatExportFormat::Json,
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
        ];

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Export Chat".to_string()),
            subtitle: Some("Creates a file next to the rollout (.jsonl).".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
        self.request_redraw();
    }

    pub(crate) fn open_export_path_prompt(&mut self, format: ChatExportFormat) {
        let Some(rollout_path) = self.current_rollout_path.clone() else {
            self.add_info_message("Export is not available yet.".to_string(), None);
            return;
        };

        let default_path = match resolve_export_destination(
            &rollout_path,
            Some(format),
            &ExportOverrides::default(),
        ) {
            Ok(destination) => destination.path,
            Err(message) => {
                self.add_error_message(message);
                return;
            }
        };

        let placeholder = format!("Path (default: {})", default_path.display());
        let context_label = Some(format!("Format: {}", format.label()));
        let tx = self.app_event_tx.clone();
        let cwd = self.config.cwd.clone();
        let view = CustomPromptView::new(
            "Export path".to_string(),
            placeholder,
            context_label,
            Box::new(move |input: String| {
                let trimmed = input.trim();
                let overrides = if trimmed.is_empty() {
                    ExportOverrides::default()
                } else {
                    export_overrides_from_path_input(trimmed, &cwd)
                };
                tx.send(AppEvent::ExportChat {
                    format: Some(format),
                    overrides,
                });
            }),
        );
        self.bottom_pane.show_view(Box::new(view));
        self.request_redraw();
    }

    pub(crate) fn start_export(
        &mut self,
        format: Option<ChatExportFormat>,
        overrides: ExportOverrides,
    ) {
        let Some(rollout_path) = self.current_rollout_path.clone() else {
            self.add_info_message("Export is not available yet.".to_string(), None);
            return;
        };

        let destination = match resolve_export_destination(&rollout_path, format, &overrides) {
            Ok(destination) => destination,
            Err(message) => {
                self.add_error_message(message);
                return;
            }
        };

        let out_path = destination.path;
        let format = destination.format;
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = async {
                if let Some(parent) = out_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                match format {
                    ChatExportFormat::Markdown => {
                        export_markdown::export_rollout_as_markdown(&rollout_path, &out_path).await
                    }
                    ChatExportFormat::Json => {
                        export_markdown::export_rollout_as_json(&rollout_path, &out_path).await
                    }
                }
            }
            .await;

            match result {
                Ok(messages) => tx.send(AppEvent::ExportResult {
                    path: out_path,
                    messages,
                    error: None,
                    format,
                }),
                Err(error) => tx.send(AppEvent::ExportResult {
                    path: out_path,
                    messages: 0,
                    error: Some(error.to_string()),
                    format,
                }),
            };
        });
    }

    pub(crate) fn handle_paste(&mut self, text: String) {
        if text.is_empty() {
            // Some terminals (like VS Code) route Cmd+V through terminal paste, which can
            // yield an empty payload for images. Fall back to reading the clipboard.
            self.paste_from_clipboard();
            return;
        }
        self.bottom_pane.handle_paste(text);
    }

    // Returns true if caller should skip rendering this frame (a future frame is scheduled).
    pub(crate) fn handle_paste_burst_tick(&mut self, frame_requester: FrameRequester) -> bool {
        if self.bottom_pane.flush_paste_burst_if_due() {
            // A paste just flushed; request an immediate redraw and skip this frame.
            self.request_redraw();
            true
        } else if self.bottom_pane.is_in_paste_burst() {
            // While capturing a burst, schedule a follow-up tick and skip this frame
            // to avoid redundant renders between ticks.
            frame_requester.schedule_frame_in(
                crate::bottom_pane::ChatComposer::recommended_paste_flush_delay(),
            );
            true
        } else {
            false
        }
    }

    fn flush_active_cell(&mut self) {
        if let Some(active) = self.active_cell.take() {
            self.needs_final_message_separator = true;
            self.app_event_tx.send(AppEvent::InsertHistoryCell(active));
        }
    }

    pub(crate) fn add_to_history(&mut self, cell: impl HistoryCell + 'static) {
        self.add_boxed_history(Box::new(cell));
    }

    fn add_boxed_history(&mut self, cell: Box<dyn HistoryCell>) {
        // Keep the placeholder session header as the active cell until real session info arrives,
        // so we can merge headers instead of committing a duplicate box to history.
        let keep_placeholder_header_active = !self.is_session_configured()
            && self
                .active_cell
                .as_ref()
                .is_some_and(|c| c.as_any().is::<history_cell::SessionHeaderHistoryCell>());

        if !keep_placeholder_header_active && !cell.display_lines(u16::MAX).is_empty() {
            // Only break exec grouping if the cell renders visible lines.
            self.flush_active_cell();
            self.needs_final_message_separator = true;
        }
        self.app_event_tx.send(AppEvent::InsertHistoryCell(cell));
    }

    fn queue_user_message(&mut self, user_message: UserMessage) {
        if !self.is_session_configured()
            || self.bottom_pane.is_task_running()
            || self.is_review_mode
        {
            let id = self.next_queued_user_message_id;
            self.next_queued_user_message_id = self.next_queued_user_message_id.saturating_add(1);
            let queued = QueuedUserMessage {
                id,
                text: user_message.text,
                local_images: user_message.local_images,
                text_elements: user_message.text_elements,
                mention_paths: user_message.mention_paths,
                model_override: None,
                effort_override: None,
            };
            self.queued_user_messages.push_back(queued);
            self.refresh_queued_user_messages();
        } else {
            self.submit_user_message(user_message);
        }
    }

    fn submit_queued_user_message(&mut self, queued: QueuedUserMessage) {
        let user_message = UserMessage {
            text: queued.text,
            local_images: queued.local_images,
            text_elements: queued.text_elements,
            mention_paths: queued.mention_paths,
        };
        self.submit_user_message_with_overrides(
            user_message,
            queued.model_override,
            queued.effort_override,
        );
    }

    fn submit_user_message(&mut self, user_message: UserMessage) {
        self.submit_user_message_with_overrides(user_message, None, None);
    }

    fn submit_user_message_with_overrides(
        &mut self,
        user_message: UserMessage,
        model_override: Option<String>,
        effort_override: Option<Option<ReasoningEffortConfig>>,
    ) {
        if !self.is_session_configured() {
            tracing::warn!("cannot submit user message before session is configured; queueing");
            let id = self.next_queued_user_message_id;
            self.next_queued_user_message_id = self.next_queued_user_message_id.saturating_add(1);
            let queued = QueuedUserMessage {
                id,
                text: user_message.text,
                local_images: user_message.local_images,
                text_elements: user_message.text_elements,
                mention_paths: user_message.mention_paths,
                model_override,
                effort_override,
            };
            self.queued_user_messages.push_front(queued);
            self.refresh_queued_user_messages();
            return;
        }

        let UserMessage {
            text,
            local_images,
            text_elements,
            mention_paths,
        } = user_message;
        if text.is_empty() && local_images.is_empty() {
            return;
        }
        if !local_images.is_empty() && !self.current_model_supports_images() {
            self.restore_blocked_image_submission(text, text_elements, local_images, mention_paths);
            return;
        }

        let mut items: Vec<UserInput> = Vec::new();

        // Special-case: "!cmd" executes a local shell command instead of sending to the model.
        if let Some(stripped) = text.strip_prefix('!') {
            let cmd = stripped.trim();
            if cmd.is_empty() {
                self.app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
                    history_cell::new_info_event(
                        USER_SHELL_COMMAND_HELP_TITLE.to_string(),
                        Some(USER_SHELL_COMMAND_HELP_HINT.to_string()),
                    ),
                )));
                return;
            }
            self.submit_op(Op::RunUserShellCommand {
                command: cmd.to_string(),
            });
            return;
        }

        for image in &local_images {
            items.push(UserInput::LocalImage {
                path: image.path.clone(),
            });
        }

        if !text.is_empty() {
            items.push(UserInput::Text {
                text: text.clone(),
                text_elements: text_elements.clone(),
            });
        }

        let mentions = collect_tool_mentions(&text, &mention_paths);
        let mut skill_names_lower: HashSet<String> = HashSet::new();

        if let Some(skills) = self.bottom_pane.skills() {
            skill_names_lower = skills
                .iter()
                .map(|skill| skill.name.to_ascii_lowercase())
                .collect();
            let skill_mentions = find_skill_mentions_with_tool_mentions(&mentions, skills);
            for skill in skill_mentions {
                items.push(UserInput::Skill {
                    name: skill.name.clone(),
                    path: skill.path.clone(),
                });
            }
        }

        if let Some(apps) = self.connectors_for_mentions() {
            let app_mentions = find_app_mentions(&mentions, apps, &skill_names_lower);
            for app in app_mentions {
                let app_id = app.id.as_str();
                items.push(UserInput::Mention {
                    name: app.name.clone(),
                    path: format!("app://{app_id}"),
                });
            }
        }

        let effective_mode = self.effective_collaboration_mode();
        let running_model = model_override
            .or_else(|| {
                self.agent_turn_running
                    .then(|| self.running_turn_model.clone())
                    .flatten()
            })
            .unwrap_or_else(|| effective_mode.model().to_string());
        let running_effort = effort_override.unwrap_or_else(|| {
            if self.agent_turn_running {
                self.running_turn_reasoning_effort
            } else {
                effective_mode.reasoning_effort()
            }
        });
        let collaboration_mode = if self.collaboration_modes_enabled() {
            self.active_collaboration_mask
                .as_ref()
                .map(|_| effective_mode.clone())
        } else {
            None
        };
        let personality = self
            .config
            .personality
            .filter(|_| self.config.features.enabled(Feature::Personality))
            .filter(|_| self.current_model_supports_personality());
        let op = Op::UserTurn {
            items,
            cwd: self.config.cwd.clone(),
            approval_policy: self.config.approval_policy.value(),
            sandbox_policy: self.config.sandbox_policy.get().clone(),
            model: running_model.clone(),
            effort: running_effort,
            summary: self.config.model_reasoning_summary,
            final_output_json_schema: None,
            collaboration_mode,
            personality,
            service_tier: self.config.service_tier,
        };

        if !self.agent_turn_running {
            self.running_turn_model = Some(running_model);
            self.running_turn_reasoning_effort = running_effort;
        }

        self.codex_op_tx.send(op).unwrap_or_else(|e| {
            tracing::error!("failed to send message: {e}");
        });

        // Persist the text to cross-session message history.
        if !text.is_empty() {
            self.codex_op_tx
                .send(Op::AddToHistory { text: text.clone() })
                .unwrap_or_else(|e| {
                    tracing::error!("failed to send AddHistory op: {e}");
                });
        }

        // Only show the text portion in conversation history.
        if !text.is_empty() {
            let local_image_paths = local_images.into_iter().map(|img| img.path).collect();
            self.add_to_history(history_cell::new_user_prompt(
                text.clone(),
                text_elements,
                local_image_paths,
            ));
            self.push_copyable_message(CopyableRole::User, &text);
        }

        self.needs_final_message_separator = false;
        self.refresh_status_line();
    }

    /// Restore the blocked submission draft without losing mention resolution state.
    ///
    /// The blocked-image path intentionally keeps the draft in the composer so
    /// users can remove attachments and retry. We must restore
    /// `mention_paths` alongside visible text; restoring only `$name` tokens
    /// makes the draft look correct while degrading mention resolution to
    /// name-only heuristics on retry.
    fn restore_blocked_image_submission(
        &mut self,
        text: String,
        text_elements: Vec<TextElement>,
        local_images: Vec<LocalImageAttachment>,
        mention_paths: HashMap<String, String>,
    ) {
        // Preserve the user's composed payload so they can retry after changing models.
        let local_image_paths = local_images.iter().map(|img| img.path.clone()).collect();
        self.bottom_pane.set_composer_text_with_mention_paths(
            text,
            text_elements,
            local_image_paths,
            mention_paths,
        );
        self.add_to_history(history_cell::new_warning_event(
            self.image_inputs_not_supported_message(),
        ));
        self.request_redraw();
    }

    /// Replay a subset of initial events into the UI to seed the transcript when
    /// resuming an existing session. This approximates the live event flow and
    /// is intentionally conservative: only safe-to-replay items are rendered to
    /// avoid triggering side effects. Event ids are passed as `None` to
    /// distinguish replayed events from live ones.
    fn replay_initial_messages(&mut self, events: Vec<EventMsg>) {
        for msg in events {
            if matches!(
                msg,
                EventMsg::SessionConfigured(_) | EventMsg::ThreadNameUpdated(_)
            ) {
                continue;
            }
            // `id: None` indicates a synthetic/fake id coming from replay.
            self.dispatch_event_msg(None, msg, true);
        }
    }

    pub(crate) fn handle_codex_event(&mut self, event: Event) {
        let Event { id, msg } = event;
        self.dispatch_event_msg(Some(id), msg, false);
    }

    pub(crate) fn handle_codex_event_replay(&mut self, event: Event) {
        let Event { msg, .. } = event;
        if matches!(msg, EventMsg::ShutdownComplete) {
            return;
        }
        self.dispatch_event_msg(None, msg, true);
    }

    /// Dispatch a protocol `EventMsg` to the appropriate handler.
    ///
    /// `id` is `Some` for live events and `None` for replayed events from
    /// `replay_initial_messages()`. Callers should treat `None` as a "fake" id
    /// that must not be used to correlate follow-up actions.
    fn dispatch_event_msg(&mut self, id: Option<String>, msg: EventMsg, from_replay: bool) {
        let is_stream_error = matches!(&msg, EventMsg::StreamError(_));
        if !is_stream_error {
            self.restore_retry_status_header_if_present();
        }

        match msg {
            EventMsg::AgentMessageDelta(_)
            | EventMsg::PlanDelta(_)
            | EventMsg::AgentReasoningDelta(_)
            | EventMsg::TerminalInteraction(_)
            | EventMsg::ExecCommandOutputDelta(_)
            | EventMsg::ProgressTrace(_) => {}
            _ => {
                tracing::trace!("handle_codex_event: {:?}", msg);
            }
        }

        match msg {
            EventMsg::SessionConfigured(e) => self.on_session_configured(e),
            EventMsg::ThreadNameUpdated(e) => self.on_thread_name_updated(e),
            EventMsg::RuntimeContextActivated(event) => {
                self.on_runtime_context_activated(event.snapshot)
            }
            EventMsg::RuntimeContextUpdated(event) => {
                self.on_runtime_context_updated(event.snapshot)
            }
            EventMsg::RuntimeContextDeactivated(event) => {
                self.on_runtime_context_deactivated(event)
            }
            EventMsg::AgentMessage(AgentMessageEvent { message }) => {
                self.on_agent_message(message, from_replay)
            }
            EventMsg::AgentMessageDelta(AgentMessageDeltaEvent { delta }) => {
                self.on_agent_message_delta(delta)
            }
            EventMsg::PlanDelta(event) => self.on_plan_delta(event.delta),
            EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent { delta })
            | EventMsg::AgentReasoningRawContentDelta(AgentReasoningRawContentDeltaEvent {
                delta,
            }) => self.on_agent_reasoning_delta(delta),
            EventMsg::AgentReasoning(AgentReasoningEvent { .. }) => self.on_agent_reasoning_final(),
            EventMsg::AgentReasoningRawContent(AgentReasoningRawContentEvent { text }) => {
                self.on_agent_reasoning_delta(text);
                self.on_agent_reasoning_final();
            }
            EventMsg::AgentReasoningSectionBreak(_) => self.on_reasoning_section_break(),
            EventMsg::TurnStarted(_) => self.on_task_started(),
            EventMsg::TurnComplete(TurnCompleteEvent { last_agent_message }) => {
                self.on_task_complete(last_agent_message, from_replay)
            }
            EventMsg::TokenCount(ev) => {
                self.set_token_info(ev.info);
                self.on_rate_limit_snapshot(ev.rate_limits);
            }
            EventMsg::Warning(WarningEvent { message }) => self.on_warning(message),
            EventMsg::Error(ErrorEvent {
                message,
                codex_error_info,
            }) => {
                if let Some(info) = codex_error_info
                    && let Some(kind) = rate_limit_error_kind(&info)
                {
                    match kind {
                        RateLimitErrorKind::ModelCap {
                            model,
                            reset_after_seconds,
                        } => self.on_model_cap_error(model, reset_after_seconds),
                        RateLimitErrorKind::UsageLimit | RateLimitErrorKind::Generic => {
                            self.on_error(message)
                        }
                    }
                } else {
                    self.on_error(message);
                }
            }
            EventMsg::McpStartupUpdate(ev) => self.on_mcp_startup_update(ev),
            EventMsg::McpStartupComplete(ev) => self.on_mcp_startup_complete(ev),
            EventMsg::TurnAborted(ev) => match ev.reason {
                TurnAbortReason::Interrupted => {
                    self.on_interrupted_turn(ev.reason);
                }
                TurnAbortReason::Replaced => {
                    self.on_error("Turn aborted: replaced by a new task".to_owned())
                }
                TurnAbortReason::ReviewEnded => {
                    self.on_interrupted_turn(ev.reason);
                }
            },
            EventMsg::TurnPaused(_) => self.on_paused_turn(),
            EventMsg::TurnContinued(_) => {}
            EventMsg::PlanUpdate(update) => self.on_plan_update(update),
            EventMsg::ExecApprovalRequest(ev) => {
                // For replayed events, synthesize an empty id (these should not occur).
                self.on_exec_approval_request(id.unwrap_or_default(), ev)
            }
            EventMsg::ApplyPatchApprovalRequest(ev) => {
                self.on_apply_patch_approval_request(id.unwrap_or_default(), ev)
            }
            EventMsg::ElicitationRequest(ev) => {
                self.on_elicitation_request(ev);
            }
            EventMsg::RequestUserInput(ev) => {
                self.on_request_user_input(ev);
            }
            EventMsg::ProgressTrace(ev) => self.on_progress_trace(ev),
            EventMsg::ExecCommandBegin(ev) => self.on_exec_command_begin(ev),
            EventMsg::TerminalInteraction(delta) => self.on_terminal_interaction(delta),
            EventMsg::ExecCommandOutputDelta(delta) => self.on_exec_command_output_delta(delta),
            EventMsg::PatchApplyBegin(ev) => self.on_patch_apply_begin(ev),
            EventMsg::PatchApplyEnd(ev) => self.on_patch_apply_end(ev),
            EventMsg::ExecCommandEnd(ev) => self.on_exec_command_end(ev),
            EventMsg::ViewImageToolCall(ev) => self.on_view_image_tool_call(ev),
            EventMsg::McpToolCallBegin(ev) => self.on_mcp_tool_call_begin(ev),
            EventMsg::McpToolCallEnd(ev) => self.on_mcp_tool_call_end(ev),
            EventMsg::WebSearchBegin(ev) => self.on_web_search_begin(ev),
            EventMsg::WebSearchEnd(ev) => self.on_web_search_end(ev),
            EventMsg::GetHistoryEntryResponse(ev) => self.on_get_history_entry_response(ev),
            EventMsg::McpListToolsResponse(ev) => self.on_list_mcp_tools(ev),
            EventMsg::ListCustomPromptsResponse(ev) => self.on_list_custom_prompts(ev),
            EventMsg::ListSkillsResponse(ev) => self.on_list_skills(ev),
            EventMsg::ListRemoteSkillsResponse(_) | EventMsg::RemoteSkillDownloaded(_) => {}
            EventMsg::SkillsUpdateAvailable => {
                self.submit_op(Op::ListSkills {
                    cwds: Vec::new(),
                    force_reload: true,
                });
            }
            EventMsg::ShutdownComplete => self.on_shutdown_complete(),
            EventMsg::TurnDiff(TurnDiffEvent { unified_diff }) => self.on_turn_diff(unified_diff),
            EventMsg::DeprecationNotice(ev) => self.on_deprecation_notice(ev),
            EventMsg::BackgroundEvent(BackgroundEventEvent { message }) => {
                self.on_background_event(message)
            }
            EventMsg::UndoStarted(ev) => self.on_undo_started(ev),
            EventMsg::UndoCompleted(ev) => self.on_undo_completed(ev),
            EventMsg::StreamError(StreamErrorEvent {
                message,
                additional_details,
                ..
            }) => self.on_stream_error(message, additional_details),
            EventMsg::UserMessage(ev) => {
                if from_replay {
                    self.on_user_message_event(ev);
                }
            }
            EventMsg::EnteredReviewMode(review_request) => {
                self.on_entered_review_mode(review_request, from_replay)
            }
            EventMsg::ExitedReviewMode(review) => self.on_exited_review_mode(review),
            EventMsg::ContextCompacted(_) => {
                self.on_agent_message("Context compacted".to_owned(), from_replay)
            }
            EventMsg::CollabAgentSpawnBegin(_) => {}
            EventMsg::CollabAgentSpawnEnd(ev) => self.on_collab_event(collab::spawn_end(ev)),
            EventMsg::CollabAgentInteractionBegin(_) => {}
            EventMsg::CollabAgentInteractionEnd(ev) => {
                self.on_collab_event(collab::interaction_end(ev))
            }
            EventMsg::CollabWaitingBegin(ev) => self.on_collab_event(collab::waiting_begin(ev)),
            EventMsg::CollabWaitingEnd(ev) => self.on_collab_event(collab::waiting_end(ev)),
            EventMsg::CollabCloseBegin(_) => {}
            EventMsg::CollabCloseEnd(ev) => self.on_collab_event(collab::close_end(ev)),
            EventMsg::ThreadRolledBack(_) => {
                // Conservatively clear `/copy` state on rollback. The app layer trims visible
                // transcript cells, but we do not maintain rollback-aware raw-markdown history yet,
                // so keeping the previous cache can return content that was just removed.
                self.last_copyable_output = None;
            }
            EventMsg::RawResponseItem(_)
            | EventMsg::ItemStarted(_)
            | EventMsg::AgentMessageContentDelta(_)
            | EventMsg::ReasoningContentDelta(_)
            | EventMsg::ReasoningRawContentDelta(_)
            | EventMsg::DynamicToolCallRequest(_) => {}
            EventMsg::ItemCompleted(event) => {
                if let codex_protocol::items::TurnItem::Plan(plan_item) = event.item {
                    self.on_plan_item_completed(plan_item.text);
                }
            }
        }
    }

    fn on_entered_review_mode(&mut self, review: ReviewRequest, from_replay: bool) {
        // Enter review mode and emit a concise banner
        if self.pre_review_token_info.is_none() {
            self.pre_review_token_info = Some(self.token_info.clone());
        }
        // Avoid toggling running state for replayed history events on resume.
        if !from_replay && !self.bottom_pane.is_task_running() {
            self.bottom_pane.set_task_running(true);
        }
        self.is_review_mode = true;
        let hint = review
            .user_facing_hint
            .unwrap_or_else(|| codex_core::review_prompts::user_facing_hint(&review.target));
        let banner = format!(">> Code review started: {hint} <<");
        self.add_to_history(history_cell::new_review_status_line(banner));
        self.request_redraw();
    }

    fn on_exited_review_mode(&mut self, review: ExitedReviewModeEvent) {
        // Leave review mode; if output is present, flush pending stream + show results.
        if let Some(output) = review.review_output {
            self.flush_answer_stream_with_separator();
            self.flush_interrupt_queue();
            self.flush_active_cell();

            if output.findings.is_empty() {
                let explanation = output.overall_explanation.trim().to_string();
                if explanation.is_empty() {
                    tracing::error!("Reviewer failed to output a response.");
                    self.add_to_history(history_cell::new_error_event(
                        "Reviewer failed to output a response.".to_owned(),
                    ));
                } else {
                    // Show explanation when there are no structured findings.
                    let mut rendered: Vec<ratatui::text::Line<'static>> = vec!["".into()];
                    append_markdown(&explanation, None, &mut rendered);
                    let body_cell = AgentMessageCell::new(rendered, false);
                    self.app_event_tx
                        .send(AppEvent::InsertHistoryCell(Box::new(body_cell)));
                }
            }
            // Final message is rendered as part of the AgentMessage.
        }
        if let Some(output) = review.post_turn_completion_review_output {
            self.flush_answer_stream_with_separator();
            self.flush_interrupt_queue();
            self.flush_active_cell();

            let mut rendered: Vec<ratatui::text::Line<'static>> = vec!["".into()];
            append_markdown(output.evaluation.trim(), None, &mut rendered);
            rendered.push("".into());
            rendered.push(
                format!(
                    "Fix actions advised: {}",
                    if output.fix_actions_advised {
                        "yes"
                    } else {
                        "no"
                    }
                )
                .into(),
            );
            let body_cell = AgentMessageCell::new(rendered, false);
            self.app_event_tx
                .send(AppEvent::InsertHistoryCell(Box::new(body_cell)));
        }

        self.is_review_mode = false;
        self.restore_pre_review_token_info();
        // Append a finishing banner at the end of this turn.
        self.add_to_history(history_cell::new_review_status_line(
            "<< Code review finished >>".to_string(),
        ));
        self.request_redraw();
    }

    fn on_user_message_event(&mut self, event: UserMessageEvent) {
        if !event.message.trim().is_empty() {
            self.push_copyable_message(CopyableRole::User, &event.message);
            self.add_to_history(history_cell::new_user_prompt(
                event.message,
                event.text_elements,
                event.local_images,
            ));
        }

        // User messages reset separator state so the next agent response doesn't add a stray break.
        self.needs_final_message_separator = false;
    }

    /// Exit the UI immediately without waiting for shutdown.
    ///
    /// Prefer [`Self::request_quit_without_confirmation`] for user-initiated exits;
    /// this is mainly a fallback for shutdown completion or emergency exits.
    fn request_immediate_exit(&self) {
        self.app_event_tx.send(AppEvent::Exit(ExitMode::Immediate));
    }

    /// Request a shutdown-first quit.
    ///
    /// This is used for explicit quit commands (`/quit`, `/exit`, `/logout`) and for
    /// the double-press Ctrl+C/Ctrl+D quit shortcut.
    fn request_quit_without_confirmation(&self) {
        self.app_event_tx
            .send(AppEvent::Exit(ExitMode::ShutdownFirst));
    }

    fn request_redraw(&mut self) {
        self.frame_requester.schedule_frame();
    }

    fn bump_active_cell_revision(&mut self) {
        // Wrapping avoids overflow; wraparound would require 2^64 bumps and at
        // worst causes a one-time cache-key collision.
        self.active_cell_revision = self.active_cell_revision.wrapping_add(1);
    }

    fn notify(&mut self, notification: Notification) {
        if !notification.allowed_for(&self.config.tui_notifications) {
            return;
        }
        self.pending_notification = Some(notification);
        self.request_redraw();
    }

    pub(crate) fn maybe_post_pending_notification(&mut self, tui: &mut crate::tui::Tui) {
        if let Some(notif) = self.pending_notification.take() {
            tui.notify(notif.display());
        }
    }

    /// Mark the active cell as failed (✗) and flush it into history.
    fn finalize_active_cell_as_failed(&mut self) {
        if let Some(mut cell) = self.active_cell.take() {
            // Insert finalized cell into history and keep grouping consistent.
            if let Some(exec) = cell.as_any_mut().downcast_mut::<ExecCell>() {
                exec.mark_failed();
            } else if let Some(tool) = cell.as_any_mut().downcast_mut::<McpToolCallCell>() {
                tool.mark_failed();
            }
            self.add_boxed_history(cell);
        }
    }

    // If idle and there are queued inputs, submit exactly one to start the next turn.
    fn maybe_send_next_queued_input(&mut self) {
        if self.bottom_pane.is_task_running()
            || self.queued_edit_state.is_some()
            || !self.bottom_pane.no_modal_or_popup_active()
        {
            return;
        }
        if let Some(queued) = self.queued_user_messages.pop_front() {
            self.submit_queued_user_message(queued);
        }
        // Update the list to reflect the remaining queued messages (if any).
        self.refresh_queued_user_messages();
    }

    fn paste_from_clipboard(&mut self) {
        let active_view = !self.bottom_pane.no_modal_or_popup_active();

        let mut image_error: Option<PasteImageError> = None;
        if !active_view {
            match paste_image_to_temp_png() {
                Ok((path, info)) => {
                    tracing::debug!(
                        "pasted image size={}x{} format={}",
                        info.width,
                        info.height,
                        info.encoded_format.label()
                    );
                    self.attach_image(path);
                    return;
                }
                Err(err) => {
                    image_error = Some(err);
                }
            }
        }

        match paste_text_from_clipboard() {
            Ok(text) => {
                self.bottom_pane.handle_paste(text);
            }
            Err(err) => {
                let message = if let Some(img_err) = image_error {
                    format!("Failed to paste from clipboard: {img_err}; {err}")
                } else {
                    format!("Failed to paste from clipboard: {err}")
                };
                self.add_to_history(history_cell::new_error_event(message));
                self.request_redraw();
            }
        }
    }

    fn copy_prompt_to_clipboard(&mut self) {
        if !self.bottom_pane.no_modal_or_popup_active() {
            return;
        }

        let text = self.bottom_pane.composer_text();
        if text.trim().is_empty() {
            return;
        }

        if let Err(err) = copy_text_to_clipboard(&text) {
            self.add_to_history(history_cell::new_error_event(format!(
                "Failed to copy prompt to clipboard: {err}",
            )));
            self.request_redraw();
        }
    }

    fn copy_last_output_to_clipboard(&mut self) {
        if !self.bottom_pane.no_modal_or_popup_active() {
            return;
        }

        let Some(markdown) = self
            .last_assistant_output_markdown
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        else {
            self.add_to_history(history_cell::new_info_event(
                "No output to copy.".to_string(),
                None,
            ));
            self.request_redraw();
            return;
        };

        if let Err(err) = copy_text_to_clipboard(markdown) {
            self.add_to_history(history_cell::new_error_event(format!(
                "Failed to copy last output to clipboard: {err}",
            )));
            self.request_redraw();
            return;
        }

        self.add_to_history(history_cell::new_info_event(
            "Copied last output to clipboard.".to_string(),
            None,
        ));
        self.request_redraw();
    }

    fn open_copy_code_block_picker(&mut self) {
        if !self.bottom_pane.no_modal_or_popup_active() {
            return;
        }
        self.open_copy_code_block_picker_with_scope(CopyCodeBlockScope::LastResponse);
    }

    pub(crate) fn open_copy_code_block_picker_with_scope(&mut self, scope: CopyCodeBlockScope) {
        let scope = scope.into();
        self.copy_code_ui_state = Some(CopyCodeUiState::new(
            scope,
            self.config.tui_copy_code_ui_mode,
        ));
        self.show_copy_code_block_view();
    }

    pub(crate) fn set_copy_code_block_scope(&mut self, scope: CopyCodeBlockScope) {
        let scope = scope.into();
        if let Some(state) = self.copy_code_ui_state.as_mut() {
            state.scope = scope;
            state.selected_id = None;
            state.selected_ids.clear();
        } else {
            self.copy_code_ui_state = Some(CopyCodeUiState::new(
                scope,
                self.config.tui_copy_code_ui_mode,
            ));
        }
        self.show_copy_code_block_view();
    }

    pub(crate) fn toggle_copy_code_block_ui_mode(&mut self) {
        if let Some(state) = self.copy_code_ui_state.as_mut() {
            state.ui_mode = match state.ui_mode {
                CopyUiMode::Picker => CopyUiMode::Navigator,
                CopyUiMode::Navigator => CopyUiMode::Picker,
            };
        }
        self.show_copy_code_block_view();
    }

    pub(crate) fn toggle_copy_code_block_multi_select_mode(&mut self) {
        if let Some(state) = self.copy_code_ui_state.as_mut() {
            state.multi_select = !state.multi_select;
            if !state.multi_select {
                state.selected_ids.clear();
            }
        }
        self.show_copy_code_block_view();
    }

    pub(crate) fn toggle_copy_code_block_selection(&mut self, id: String) {
        if let Some(state) = self.copy_code_ui_state.as_mut() {
            state.selected_id = Some(id.clone());
            if !state.selected_ids.insert(id.clone()) {
                state.selected_ids.remove(&id);
            }
        }
        self.show_copy_code_block_view();
    }

    pub(crate) fn copy_selected_code_blocks(&mut self) {
        let Some(state) = self.copy_code_ui_state.as_ref() else {
            return;
        };
        if state.selected_ids.is_empty() {
            self.add_to_history(history_cell::new_info_event(
                "No code blocks selected.".to_string(),
                None,
            ));
            self.request_redraw();
            return;
        }
        let selected = state.selected_ids.clone();
        let contents = self
            .code_block_candidates_for_scope(state.scope)
            .into_iter()
            .filter(|candidate| selected.contains(&candidate.id))
            .map(|candidate| candidate.content)
            .collect::<Vec<_>>();
        if contents.is_empty() {
            self.add_to_history(history_cell::new_info_event(
                "No code blocks selected.".to_string(),
                None,
            ));
            self.request_redraw();
            return;
        }
        self.copy_joined_items_to_clipboard(contents, "code block", "code blocks");
    }

    fn show_copy_code_block_view(&mut self) {
        let Some(state) = self.copy_code_ui_state.clone() else {
            return;
        };
        let has_any_response = self
            .copyable_messages
            .iter()
            .any(|message| message.role == CopyableRole::Response);
        let candidates = self.code_block_candidates_for_scope(state.scope);
        if candidates.is_empty() && !has_any_response {
            let message = match state.scope {
                CodeBlockScope::LastResponse => {
                    if self.last_response_markdown().is_some() {
                        "No code blocks to copy."
                    } else {
                        "No output to scan for code blocks."
                    }
                }
                CodeBlockScope::AllResponses => "No response output to scan for code blocks.",
            };
            self.add_to_history(history_cell::new_info_event(message.to_string(), None));
            self.request_redraw();
            return;
        }

        let mut items = vec![
            SelectionItem {
                name: format!("View: {}", copy_ui_mode_label(state.ui_mode)),
                description: Some(
                    "Switch between picker and navigator (session only).".to_string(),
                ),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::ToggleCopyCodeBlockUiMode);
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: format!("Scope: {}", state.scope.label()),
                description: Some(state.scope.description().to_string()),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::SetCopyCodeBlockScope {
                        scope: state.scope.toggle().into(),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: format!(
                    "Selection: {}",
                    if state.multi_select {
                        "multi"
                    } else {
                        "single"
                    }
                ),
                description: Some("Toggle multi-select mode.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::ToggleCopyCodeBlockMultiSelect);
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
        ];

        if state.multi_select {
            let selected_count = state.selected_ids.len();
            items.push(SelectionItem {
                name: format!("Copy selected ({selected_count})"),
                description: Some("Copy all selected code blocks.".to_string()),
                is_disabled: selected_count == 0,
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::CopySelectedCodeBlocks);
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        let top_rows = items.len();
        let has_candidates = !candidates.is_empty();
        if !has_candidates {
            items.push(SelectionItem {
                name: "No code blocks in this scope".to_string(),
                is_disabled: true,
                ..Default::default()
            });
        }

        for candidate in &candidates {
            let id = candidate.id.clone();
            let content = candidate.content.clone();
            let selected = state.selected_ids.contains(&id);
            let label = if state.multi_select {
                format!("[{}] {}", if selected { "x" } else { " " }, candidate.label)
            } else {
                candidate.label.clone()
            };
            items.push(SelectionItem {
                name: label,
                description: Some(candidate.preview.clone()),
                search_value: Some(candidate.search_value.clone()),
                actions: if state.multi_select {
                    vec![Box::new(move |tx| {
                        tx.send(AppEvent::ToggleCopyCodeBlockSelection { id: id.clone() });
                    })]
                } else {
                    vec![Box::new(move |tx| match copy_text_to_clipboard(&content) {
                        Ok(()) => tx.send(AppEvent::InsertHistoryCell(Box::new(
                            history_cell::new_info_event(
                                "Copied code block to clipboard.".to_string(),
                                None,
                            ),
                        ))),
                        Err(err) => tx.send(AppEvent::InsertHistoryCell(Box::new(
                            history_cell::new_error_event(format!(
                                "Failed to copy code block to clipboard: {err}",
                            )),
                        ))),
                    })]
                },
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        let initial_selected_idx =
            selected_index_for_candidates(top_rows, &candidates, state.selected_id.as_deref())
                .or_else(|| has_candidates.then_some(top_rows))
                .or(Some(0));
        let (subtitle, searchable, search_placeholder) = match state.ui_mode {
            CopyUiMode::Picker => (
                format!("Choose a block to copy · {}", state.scope.label()),
                true,
                Some("Type to search code blocks".to_string()),
            ),
            CopyUiMode::Navigator => (
                format!("Navigate blocks in chat order · {}", state.scope.label()),
                false,
                None,
            ),
        };

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Copy code block".to_string()),
            subtitle: Some(subtitle),
            footer_hint: Some(standard_popup_hint_line()),
            is_searchable: searchable,
            search_placeholder,
            items,
            initial_selected_idx,
            ..Default::default()
        });
        self.request_redraw();
    }

    pub(crate) fn open_copy_message_picker(&mut self, filter: CopyMessageFilter) {
        let filter = filter.into();
        self.copy_message_ui_state = Some(CopyMessageUiState::new(
            filter,
            self.config.tui_copy_message_ui_mode,
        ));
        self.show_copy_message_view();
    }

    pub(crate) fn set_copy_message_filter(&mut self, filter: CopyMessageFilter) {
        let filter = filter.into();
        if let Some(state) = self.copy_message_ui_state.as_mut() {
            state.filter = filter;
            state.selected_id = None;
            state.selected_ids.clear();
        } else {
            self.copy_message_ui_state = Some(CopyMessageUiState::new(
                filter,
                self.config.tui_copy_message_ui_mode,
            ));
        }
        self.show_copy_message_view();
    }

    pub(crate) fn toggle_copy_message_ui_mode(&mut self) {
        if let Some(state) = self.copy_message_ui_state.as_mut() {
            state.ui_mode = match state.ui_mode {
                CopyUiMode::Picker => CopyUiMode::Navigator,
                CopyUiMode::Navigator => CopyUiMode::Picker,
            };
        }
        self.show_copy_message_view();
    }

    pub(crate) fn toggle_copy_message_multi_select_mode(&mut self) {
        if let Some(state) = self.copy_message_ui_state.as_mut() {
            state.multi_select = !state.multi_select;
            if !state.multi_select {
                state.selected_ids.clear();
            }
        }
        self.show_copy_message_view();
    }

    pub(crate) fn toggle_copy_message_selection(&mut self, id: String) {
        if let Some(state) = self.copy_message_ui_state.as_mut() {
            state.selected_id = Some(id.clone());
            if !state.selected_ids.insert(id.clone()) {
                state.selected_ids.remove(&id);
            }
        }
        self.show_copy_message_view();
    }

    pub(crate) fn copy_selected_messages(&mut self) {
        let Some(state) = self.copy_message_ui_state.as_ref() else {
            return;
        };
        if state.selected_ids.is_empty() {
            self.add_to_history(history_cell::new_info_event(
                "No messages selected.".to_string(),
                None,
            ));
            self.request_redraw();
            return;
        }
        let selected = state.selected_ids.clone();
        let contents = self
            .message_candidates_for_filter(state.filter)
            .into_iter()
            .filter(|candidate| selected.contains(&candidate.id))
            .map(|candidate| candidate.content)
            .collect::<Vec<_>>();
        if contents.is_empty() {
            self.add_to_history(history_cell::new_info_event(
                "No messages selected.".to_string(),
                None,
            ));
            self.request_redraw();
            return;
        }
        self.copy_joined_items_to_clipboard(contents, "message", "messages");
    }

    fn show_copy_message_view(&mut self) {
        if self.copyable_messages.is_empty() {
            self.add_to_history(history_cell::new_info_event(
                "No messages to copy.".to_string(),
                None,
            ));
            self.request_redraw();
            return;
        }
        let Some(state) = self.copy_message_ui_state.clone() else {
            return;
        };
        let candidates = self.message_candidates_for_filter(state.filter);
        let mut items = vec![
            SelectionItem {
                name: format!("View: {}", copy_ui_mode_label(state.ui_mode)),
                description: Some(
                    "Switch between picker and navigator (session only).".to_string(),
                ),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::ToggleCopyMessageUiMode);
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: format!("Filter: {}", state.filter.label()),
                description: Some(state.filter.description().to_string()),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::SetCopyMessageFilter {
                        filter: state.filter.next().into(),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: format!(
                    "Selection: {}",
                    if state.multi_select {
                        "multi"
                    } else {
                        "single"
                    }
                ),
                description: Some("Toggle multi-select mode.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::ToggleCopyMessageMultiSelect);
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
        ];
        if state.multi_select {
            let selected_count = state.selected_ids.len();
            items.push(SelectionItem {
                name: format!("Copy selected ({selected_count})"),
                description: Some("Copy all selected messages.".to_string()),
                is_disabled: selected_count == 0,
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::CopySelectedMessages);
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        let top_rows = items.len();
        let has_candidates = !candidates.is_empty();
        if !has_candidates {
            items.push(SelectionItem {
                name: "No messages in this filter".to_string(),
                is_disabled: true,
                ..Default::default()
            });
        }
        for candidate in &candidates {
            let id = candidate.id.clone();
            let content = candidate.content.clone();
            let selected = state.selected_ids.contains(&id);
            let label = if state.multi_select {
                format!("[{}] {}", if selected { "x" } else { " " }, candidate.label)
            } else {
                candidate.label.clone()
            };
            items.push(SelectionItem {
                name: label,
                description: Some(candidate.preview.clone()),
                search_value: Some(candidate.search_value.clone()),
                actions: if state.multi_select {
                    vec![Box::new(move |tx| {
                        tx.send(AppEvent::ToggleCopyMessageSelection { id: id.clone() });
                    })]
                } else {
                    vec![Box::new(move |tx| match copy_text_to_clipboard(&content) {
                        Ok(()) => tx.send(AppEvent::InsertHistoryCell(Box::new(
                            history_cell::new_info_event(
                                "Copied message to clipboard.".to_string(),
                                None,
                            ),
                        ))),
                        Err(err) => tx.send(AppEvent::InsertHistoryCell(Box::new(
                            history_cell::new_error_event(format!(
                                "Failed to copy message to clipboard: {err}",
                            )),
                        ))),
                    })]
                },
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        let initial_selected_idx =
            selected_index_for_candidates(top_rows, &candidates, state.selected_id.as_deref())
                .or_else(|| has_candidates.then_some(top_rows))
                .or(Some(0));
        let (subtitle, searchable, search_placeholder) = match state.ui_mode {
            CopyUiMode::Picker => (
                format!("Choose a message to copy · {}", state.filter.label()),
                true,
                Some("Type to search messages".to_string()),
            ),
            CopyUiMode::Navigator => (
                format!("Navigate messages in chat order · {}", state.filter.label()),
                false,
                None,
            ),
        };
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Copy message".to_string()),
            subtitle: Some(subtitle),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            is_searchable: searchable,
            search_placeholder,
            initial_selected_idx,
            ..Default::default()
        });
        self.request_redraw();
    }

    fn copy_joined_items_to_clipboard(
        &mut self,
        contents: Vec<String>,
        singular: &str,
        plural: &str,
    ) {
        let text = contents.join("\n\n");
        match copy_text_to_clipboard(&text) {
            Ok(()) => self.add_to_history(history_cell::new_info_event(
                format!(
                    "Copied {} {} to clipboard.",
                    contents.len(),
                    if contents.len() == 1 {
                        singular
                    } else {
                        plural
                    }
                ),
                None,
            )),
            Err(err) => self.add_to_history(history_cell::new_error_event(format!(
                "Failed to copy selected {plural} to clipboard: {err}",
            ))),
        }
        self.request_redraw();
    }

    fn message_candidates_for_filter(&self, filter: MessageFilter) -> Vec<CopyMessageCandidate> {
        self.copyable_messages
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, message)| filter.includes(message.role))
            .enumerate()
            .map(|(display_idx, (message_idx, message))| {
                let snippet = message
                    .text
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("")
                    .trim();
                let preview = if snippet.is_empty() {
                    "(empty message)".to_string()
                } else {
                    truncate_text(snippet, 80)
                };
                let role_label = message.role.label();
                let content = message.text.clone();
                CopyMessageCandidate {
                    id: format!("msg-{message_idx}"),
                    label: format!("#{} · {}", display_idx + 1, role_label),
                    preview,
                    search_value: format!("{role_label} {content}"),
                    content,
                }
            })
            .collect()
    }

    fn code_block_candidates_for_scope(
        &self,
        scope: CodeBlockScope,
    ) -> Vec<CopyCodeBlockCandidate> {
        let mut candidates = Vec::new();
        match scope {
            CodeBlockScope::LastResponse => {
                let Some(markdown) = self.last_response_markdown() else {
                    return candidates;
                };
                let extracted = extract_fenced_code_blocks(markdown);
                for (idx, candidate) in extracted.into_iter().enumerate() {
                    let label = match candidate.language {
                        Some(language) => format!("#{} · {}", idx + 1, language),
                        None => format!("#{} · plain text", idx + 1),
                    };
                    let preview = first_non_empty_preview(&candidate.content);
                    let search_value = format!("{label} {preview}");
                    candidates.push(CopyCodeBlockCandidate {
                        id: format!("last-{}", idx + 1),
                        label,
                        preview,
                        search_value,
                        content: candidate.content,
                    });
                }
            }
            CodeBlockScope::AllResponses => {
                for (response_idx, message) in self
                    .copyable_messages
                    .iter()
                    .rev()
                    .filter(|message| message.role == CopyableRole::Response)
                    .enumerate()
                {
                    let extracted = extract_fenced_code_blocks(&message.text);
                    for (block_idx, candidate) in extracted.into_iter().enumerate() {
                        let lang = candidate
                            .language
                            .unwrap_or_else(|| "plain text".to_string());
                        let label = format!("R{} #{} · {lang}", response_idx + 1, block_idx + 1);
                        let preview = first_non_empty_preview(&candidate.content);
                        let search_value = format!("{label} {preview}");
                        candidates.push(CopyCodeBlockCandidate {
                            id: format!("all-{}-{}", response_idx + 1, block_idx + 1),
                            label,
                            preview,
                            search_value,
                            content: candidate.content,
                        });
                    }
                }
            }
        }
        candidates
    }

    fn last_response_markdown(&self) -> Option<&str> {
        self.last_assistant_output_markdown
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
    }

    fn push_copyable_message(&mut self, role: CopyableRole, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        self.copyable_messages.push(CopyableMessage {
            role,
            text: trimmed.to_string(),
        });
    }

    fn last_copyable_response_is(&self, text: &str) -> bool {
        let text = text.trim();
        self.copyable_messages
            .last()
            .is_some_and(|message| message.role == CopyableRole::Response && message.text == text)
    }

    fn send_next_queued_user_message(&mut self) {
        if self.bottom_pane.is_task_running()
            || self.queued_edit_state.is_some()
            || !self.bottom_pane.no_modal_or_popup_active()
        {
            return;
        }
        if let Some(queued) = self.queued_user_messages.pop_front() {
            self.submit_queued_user_message(queued);
        }
        self.refresh_queued_user_messages();
    }

    fn handle_queue_edit_key_event(&mut self, key_event: KeyEvent) -> bool {
        match key_event {
            KeyEvent {
                code: KeyCode::Esc,
                kind: KeyEventKind::Press,
                ..
            } => {
                self.exit_queue_edit(false);
                true
            }
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                ..
            } => {
                self.exit_queue_edit(true);
                true
            }
            KeyEvent {
                code: KeyCode::Up,
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press,
                ..
            } => {
                self.switch_queue_edit(-1);
                true
            }
            KeyEvent {
                code: KeyCode::Down,
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press,
                ..
            } => {
                self.switch_queue_edit(1);
                true
            }
            KeyEvent {
                code: KeyCode::Char('m' | 'M'),
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press,
                ..
            } => {
                if let Some(id) = self
                    .queued_edit_state
                    .as_ref()
                    .map(|state| state.selected_id)
                {
                    self.open_queue_model_picker(id);
                }
                true
            }
            KeyEvent {
                code: KeyCode::Char('t' | 'T'),
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press,
                ..
            } => {
                if let Some(id) = self
                    .queued_edit_state
                    .as_ref()
                    .map(|state| state.selected_id)
                {
                    self.open_queue_thinking_picker(id);
                }
                true
            }
            _ => false,
        }
    }

    fn queue_popup_items(&self) -> Vec<QueuePopupItem> {
        let session_model = self.current_model();
        let session_effort = self.effective_reasoning_effort();

        self.queued_user_messages
            .iter()
            .map(|message| {
                let mut preview = message
                    .text
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if preview.is_empty() && !message.local_images.is_empty() {
                    preview = "[image]".to_string();
                }

                let effective_model = message.model_override.as_deref().unwrap_or(session_model);
                let effective_effort = message.effort_override.unwrap_or(session_effort);
                let mut meta_parts: Vec<String> = Vec::new();
                if !message.local_images.is_empty() {
                    meta_parts.push("img".to_string());
                }
                meta_parts.push(format!("model: {effective_model}"));
                meta_parts.push(format!(
                    "thinking: {}",
                    Self::status_line_reasoning_effort_label(effective_effort)
                ));

                QueuePopupItem {
                    id: message.id,
                    preview,
                    meta: Some(meta_parts.join(" · ")),
                }
            })
            .collect()
    }

    fn open_queue_popup(&mut self) {
        if !self.bottom_pane.no_modal_or_popup_active()
            || self.queued_user_messages.is_empty()
            || self.queued_edit_state.is_some()
        {
            return;
        }
        let items = self.queue_popup_items();
        self.bottom_pane
            .show_view(Box::new(QueuePopup::new(items, self.app_event_tx.clone())));
        self.request_redraw();
    }

    fn begin_queue_edit_most_recent(&mut self) {
        let Some(selected_id) = self.queued_user_messages.back().map(|message| message.id) else {
            return;
        };
        self.start_queue_edit(selected_id);
    }

    pub(crate) fn start_queue_edit(&mut self, id: u64) {
        if self.queued_edit_state.is_some()
            || self.queued_user_messages.is_empty()
            || !self.bottom_pane.no_modal_or_popup_active()
        {
            return;
        }

        if !self
            .queued_user_messages
            .iter()
            .any(|message| message.id == id)
        {
            return;
        }

        let composer_before_edit = QueuedComposerSnapshot {
            text: self.bottom_pane.composer_text(),
            text_elements: self.bottom_pane.composer_text_elements(),
            local_images: self.bottom_pane.composer_local_images(),
            mention_paths: self.bottom_pane.composer_mention_paths(),
        };

        self.queued_edit_state = Some(QueuedEditState {
            selected_id: id,
            composer_before_edit,
            drafts: HashMap::new(),
        });

        self.load_queue_edit_draft(id);
        self.update_queue_edit_footer_hint();
        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    pub(crate) fn delete_queued_user_message(&mut self, id: u64) {
        let Some(idx) = self
            .queued_user_messages
            .iter()
            .position(|message| message.id == id)
        else {
            return;
        };

        self.queued_user_messages.remove(idx);
        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    pub(crate) fn move_queued_user_message_up(&mut self, id: u64) {
        let Some(idx) = self
            .queued_user_messages
            .iter()
            .position(|message| message.id == id)
        else {
            return;
        };
        if idx == 0 {
            return;
        }

        self.queued_user_messages.swap(idx, idx - 1);
        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    pub(crate) fn move_queued_user_message_down(&mut self, id: u64) {
        let len = self.queued_user_messages.len();
        let Some(idx) = self
            .queued_user_messages
            .iter()
            .position(|message| message.id == id)
        else {
            return;
        };
        if idx + 1 >= len {
            return;
        }

        self.queued_user_messages.swap(idx, idx + 1);
        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    pub(crate) fn move_queued_user_message_to_front(&mut self, id: u64) {
        let Some(idx) = self
            .queued_user_messages
            .iter()
            .position(|message| message.id == id)
        else {
            return;
        };
        if idx == 0 {
            return;
        }

        let Some(message) = self.queued_user_messages.remove(idx) else {
            return;
        };
        self.queued_user_messages.push_front(message);
        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    pub(crate) fn open_queue_model_picker(&mut self, id: u64) {
        let current_override = self
            .queued_edit_state
            .as_ref()
            .and_then(|state| state.drafts.get(&id))
            .map(|draft| draft.model_override.clone())
            .unwrap_or_else(|| {
                self.queued_user_messages
                    .iter()
                    .find(|message| message.id == id)
                    .and_then(|message| message.model_override.clone())
            });

        let session_model = self.current_model().to_string();
        let mut items: Vec<SelectionItem> = vec![SelectionItem {
            name: "Use session model".to_string(),
            description: Some(format!("Current session model: {session_model}")),
            is_current: current_override.is_none(),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::QueueSetModelOverride { id, model: None });
            })],
            dismiss_on_select: true,
            ..Default::default()
        }];

        let mut presets = match self.models_manager.try_list_models(&self.config) {
            Ok(presets) => presets,
            Err(_) => {
                self.add_info_message(
                    "Models are being updated; please try again in a moment.".to_string(),
                    None,
                );
                return;
            }
        };
        presets.sort_by(|a, b| a.display_name.cmp(&b.display_name));

        for preset in presets {
            let model_slug = preset.model.clone();
            let is_current = current_override.as_deref() == Some(model_slug.as_str());
            let description = (!preset.description.is_empty()).then_some(preset.description);
            let model_for_action = model_slug.clone();
            items.push(SelectionItem {
                name: preset.display_name,
                description,
                is_current,
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::QueueSetModelOverride {
                        id,
                        model: Some(model_for_action.clone()),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Select Model for Queued Message".to_string()),
            subtitle: Some("Applies only when this message is sent.".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }

    pub(crate) fn open_queue_thinking_picker(&mut self, id: u64) {
        let model_override = self
            .queued_edit_state
            .as_ref()
            .and_then(|state| state.drafts.get(&id))
            .map(|draft| draft.model_override.clone())
            .unwrap_or_else(|| {
                self.queued_user_messages
                    .iter()
                    .find(|message| message.id == id)
                    .and_then(|message| message.model_override.clone())
            });
        let model_slug = model_override.unwrap_or_else(|| self.current_model().to_string());

        let current_override = self
            .queued_edit_state
            .as_ref()
            .and_then(|state| state.drafts.get(&id))
            .map(|draft| draft.effort_override)
            .unwrap_or_else(|| {
                self.queued_user_messages
                    .iter()
                    .find(|message| message.id == id)
                    .and_then(|message| message.effort_override)
            });

        let presets = match self.models_manager.try_list_models(&self.config) {
            Ok(presets) => presets,
            Err(_) => {
                self.add_info_message(
                    "Models are being updated; please try again in a moment.".to_string(),
                    None,
                );
                return;
            }
        };
        let Some(preset) = presets
            .into_iter()
            .find(|preset| preset.model == model_slug)
        else {
            self.add_info_message(
                format!("Model '{model_slug}' is not available right now."),
                None,
            );
            return;
        };

        let default_effort = preset.default_reasoning_effort;
        let mut items: Vec<SelectionItem> = vec![
            SelectionItem {
                name: "Use session thinking".to_string(),
                description: Some("Inherit the current session reasoning level.".to_string()),
                is_current: current_override.is_none(),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::QueueSetThinkingOverride { id, effort: None });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: format!("Default ({})", Self::reasoning_effort_label(default_effort)),
                description: Some("Use the model's default reasoning level.".to_string()),
                is_current: matches!(current_override, Some(None)),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::QueueSetThinkingOverride {
                        id,
                        effort: Some(None),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
        ];

        for option in preset.supported_reasoning_efforts {
            let effort = option.effort;
            let mut label = Self::reasoning_effort_label(effort).to_string();
            if effort == default_effort {
                label.push_str(" (default)");
            }
            let description = (!option.description.is_empty()).then_some(option.description);
            let is_current = current_override == Some(Some(effort));
            items.push(SelectionItem {
                name: label,
                description,
                is_current,
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::QueueSetThinkingOverride {
                        id,
                        effort: Some(Some(effort)),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Select Thinking for Queued Message".to_string()),
            subtitle: Some(format!("Model: {model_slug}")),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }

    pub(crate) fn set_queued_user_message_model_override(
        &mut self,
        id: u64,
        model: Option<String>,
    ) {
        if self.queued_edit_state.is_some() {
            let selected_id = self
                .queued_edit_state
                .as_ref()
                .map(|state| state.selected_id);
            let base = self
                .queued_edit_state
                .as_ref()
                .and_then(|state| state.drafts.get(&id).cloned())
                .or_else(|| {
                    if selected_id == Some(id) {
                        self.capture_queue_edit_draft(id)
                    } else {
                        self.queued_user_message_draft(id)
                    }
                });
            if let Some(mut draft) = base {
                draft.model_override = model;
                if let Some(state) = self.queued_edit_state.as_mut() {
                    state.drafts.insert(id, draft);
                }
            }
            self.refresh_queued_user_messages();
            self.request_redraw();
            return;
        }

        let Some(message) = self
            .queued_user_messages
            .iter_mut()
            .find(|message| message.id == id)
        else {
            return;
        };
        message.model_override = model;
        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    pub(crate) fn set_queued_user_message_thinking_override(
        &mut self,
        id: u64,
        effort: Option<Option<ReasoningEffortConfig>>,
    ) {
        if self.queued_edit_state.is_some() {
            let selected_id = self
                .queued_edit_state
                .as_ref()
                .map(|state| state.selected_id);
            let base = self
                .queued_edit_state
                .as_ref()
                .and_then(|state| state.drafts.get(&id).cloned())
                .or_else(|| {
                    if selected_id == Some(id) {
                        self.capture_queue_edit_draft(id)
                    } else {
                        self.queued_user_message_draft(id)
                    }
                });
            if let Some(mut draft) = base {
                draft.effort_override = effort;
                if let Some(state) = self.queued_edit_state.as_mut() {
                    state.drafts.insert(id, draft);
                }
            }
            self.refresh_queued_user_messages();
            self.request_redraw();
            return;
        }

        let Some(message) = self
            .queued_user_messages
            .iter_mut()
            .find(|message| message.id == id)
        else {
            return;
        };
        message.effort_override = effort;
        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    fn exit_queue_edit(&mut self, save: bool) {
        let Some(mut state) = self.queued_edit_state.take() else {
            return;
        };

        if save {
            if let Some(draft) = self.capture_queue_edit_draft(state.selected_id) {
                state.drafts.insert(state.selected_id, draft);
            }

            for message in &mut self.queued_user_messages {
                if let Some(draft) = state.drafts.get(&message.id) {
                    message.text = draft.text.clone();
                    message.text_elements = draft.text_elements.clone();
                    message.local_images = draft.local_images.clone();
                    message.mention_paths = draft.mention_paths.clone();
                    message.model_override = draft.model_override.clone();
                    message.effort_override = draft.effort_override;
                }
            }
        }

        let local_image_paths: Vec<PathBuf> = state
            .composer_before_edit
            .local_images
            .iter()
            .map(|img| img.path.clone())
            .collect();
        self.bottom_pane.set_footer_hint_override(None);
        self.bottom_pane.set_composer_text_with_mention_paths(
            state.composer_before_edit.text,
            state.composer_before_edit.text_elements,
            local_image_paths,
            state.composer_before_edit.mention_paths,
        );

        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    fn switch_queue_edit(&mut self, direction: isize) {
        let Some(selected_id) = self
            .queued_edit_state
            .as_ref()
            .map(|state| state.selected_id)
        else {
            return;
        };

        if let Some(draft) = self.capture_queue_edit_draft(selected_id)
            && let Some(state) = self.queued_edit_state.as_mut()
        {
            state.drafts.insert(selected_id, draft);
        }

        let Some(current_idx) = self
            .queued_user_messages
            .iter()
            .position(|message| message.id == selected_id)
        else {
            self.exit_queue_edit(false);
            return;
        };

        let len = self.queued_user_messages.len();
        if len == 0 {
            self.exit_queue_edit(false);
            return;
        }

        let next_idx = ((current_idx as isize + direction).rem_euclid(len as isize)) as usize;
        let Some(next_id) = self
            .queued_user_messages
            .get(next_idx)
            .map(|message| message.id)
        else {
            return;
        };

        let draft = self
            .queued_edit_state
            .as_ref()
            .and_then(|state| state.drafts.get(&next_id).cloned())
            .or_else(|| self.queued_user_message_draft(next_id));
        let Some(draft) = draft else {
            return;
        };

        if let Some(state) = self.queued_edit_state.as_mut() {
            state.selected_id = next_id;
        }

        let local_image_paths: Vec<PathBuf> = draft
            .local_images
            .iter()
            .map(|img| img.path.clone())
            .collect();
        self.bottom_pane.set_composer_text_with_mention_paths(
            draft.text,
            draft.text_elements,
            local_image_paths,
            draft.mention_paths,
        );
        self.update_queue_edit_footer_hint();
        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    fn capture_queue_edit_draft(&self, id: u64) -> Option<QueuedUserMessageDraft> {
        let message = self
            .queued_user_messages
            .iter()
            .find(|message| message.id == id)?;

        let (model_override, effort_override) = self
            .queued_edit_state
            .as_ref()
            .and_then(|state| state.drafts.get(&id))
            .map(|draft| (draft.model_override.clone(), draft.effort_override))
            .unwrap_or_else(|| (message.model_override.clone(), message.effort_override));

        Some(QueuedUserMessageDraft {
            text: self.bottom_pane.composer_text(),
            text_elements: self.bottom_pane.composer_text_elements(),
            local_images: self.bottom_pane.composer_local_images(),
            mention_paths: self.bottom_pane.composer_mention_paths(),
            model_override,
            effort_override,
        })
    }

    fn queued_user_message_draft(&self, id: u64) -> Option<QueuedUserMessageDraft> {
        let message = self
            .queued_user_messages
            .iter()
            .find(|message| message.id == id)?;

        Some(QueuedUserMessageDraft {
            text: message.text.clone(),
            text_elements: message.text_elements.clone(),
            local_images: message.local_images.clone(),
            mention_paths: message.mention_paths.clone(),
            model_override: message.model_override.clone(),
            effort_override: message.effort_override,
        })
    }

    fn load_queue_edit_draft(&mut self, id: u64) {
        let draft = self
            .queued_edit_state
            .as_ref()
            .and_then(|state| state.drafts.get(&id).cloned())
            .or_else(|| self.queued_user_message_draft(id));

        let Some(draft) = draft else {
            return;
        };

        let local_image_paths: Vec<PathBuf> = draft
            .local_images
            .iter()
            .map(|img| img.path.clone())
            .collect();
        self.bottom_pane.set_composer_text_with_mention_paths(
            draft.text,
            draft.text_elements,
            local_image_paths,
            draft.mention_paths,
        );
    }

    fn update_queue_edit_footer_hint(&mut self) {
        let Some(state) = self.queued_edit_state.as_ref() else {
            return;
        };

        let total = self.queued_user_messages.len();
        let position = self
            .queued_user_messages
            .iter()
            .position(|message| message.id == state.selected_id)
            .map(|idx| idx + 1)
            .unwrap_or_default();

        self.bottom_pane.set_footer_hint_override(Some(vec![
            ("Editing".to_string(), format!("{position}/{total}")),
            ("Enter".to_string(), "save".to_string()),
            ("Esc".to_string(), "cancel".to_string()),
            ("Alt+↑/↓".to_string(), "switch".to_string()),
            ("Alt+M".to_string(), "model".to_string()),
            ("Alt+T".to_string(), "thinking".to_string()),
        ]));
    }

    /// Rebuild and update the queued user messages from the current queue.
    fn refresh_queued_user_messages(&mut self) {
        let session_model = self.current_model();
        let session_effort = self.effective_reasoning_effort();
        let editing_id = self
            .queued_edit_state
            .as_ref()
            .map(|state| state.selected_id);
        let messages: Vec<String> = self
            .queued_user_messages
            .iter()
            .map(|message| {
                let effective_model = message.model_override.as_deref().unwrap_or(session_model);
                let effective_effort = message.effort_override.unwrap_or(session_effort);
                let mut tag = String::new();
                if message.model_override.is_some() || message.effort_override.is_some() {
                    tag = format!(
                        "[{effective_model} · reasoning {}] ",
                        Self::status_line_reasoning_effort_label(effective_effort)
                    );
                }

                if Some(message.id) == editing_id {
                    format!("✎ {tag}{}", message.text)
                } else {
                    format!("{tag}{}", message.text)
                }
            })
            .collect();
        self.bottom_pane.set_queued_user_messages(messages);
    }

    pub(crate) fn add_diff_in_progress(&mut self) {
        self.request_redraw();
    }

    pub(crate) fn on_diff_complete(&mut self) {
        self.request_redraw();
    }

    pub(crate) fn add_status_output(&mut self) {
        if let Some(runtime_context) = self.active_runtime_context.as_ref() {
            let snapshot =
                crate::status::StatusOutputSnapshot::from_runtime_context(runtime_context);
            self.add_to_history(crate::status::new_status_output_from_snapshot(
                snapshot,
                self.auth_manager.as_ref(),
                self.rate_limit_snapshot.as_ref(),
                self.plan_type,
                Local::now(),
            ));
            return;
        }

        let default_usage = TokenUsage::default();
        let token_info = self.token_info.as_ref();
        let total_usage = token_info
            .map(|ti| &ti.total_token_usage)
            .unwrap_or(&default_usage);
        let collaboration_mode = self.collaboration_mode_label();
        let reasoning_effort_override = Some(self.effective_reasoning_effort());
        self.add_to_history(crate::status::new_status_output(
            &self.config,
            self.auth_manager.as_ref(),
            token_info,
            total_usage,
            &self.thread_id,
            self.thread_name.clone(),
            self.forked_from,
            self.rate_limit_snapshot.as_ref(),
            self.plan_type,
            Local::now(),
            self.model_display_name(),
            collaboration_mode,
            reasoning_effort_override,
        ));
    }

    pub(crate) fn add_debug_config_output(&mut self) {
        self.add_to_history(crate::debug_config::new_debug_config_output(&self.config));
    }

    fn open_status_line_setup(&mut self) {
        let selected_items = self
            .config
            .tui_status_line
            .clone()
            .unwrap_or_else(Self::default_status_line_item_ids);
        let view =
            StatusLineSetupView::new(Some(selected_items.as_slice()), self.app_event_tx.clone());
        self.bottom_pane.show_view(Box::new(view));
    }

    fn open_progress_legend_popup(&mut self) {
        let mut items = vec![SelectionItem {
            name: format!("Mode: {}", self.progress_legend_mode),
            description: Some("Choose when the progress legend is shown.".to_string()),
            actions: vec![Box::new(|tx| {
                tx.send(AppEvent::OpenProgressLegendModePicker);
            })],
            dismiss_on_select: false,
            ..Default::default()
        }];
        for category in [
            ProgressTraceCategory::Tool,
            ProgressTraceCategory::Edit,
            ProgressTraceCategory::Waiting,
            ProgressTraceCategory::Network,
            ProgressTraceCategory::Prefill,
            ProgressTraceCategory::Reasoning,
            ProgressTraceCategory::Gen,
        ] {
            let swatch_style =
                progress_trace_style_for_category(&self.progress_trace_styles, category).to_style();
            items.push(SelectionItem {
                name: progress_trace_category_label(category).to_string(),
                name_prefix: Some(Span::styled("▮ ", swatch_style)),
                description: Some(progress_trace_style_description(
                    category,
                    &self.progress_trace_styles,
                )),
                ..Default::default()
            });
        }
        self.show_selection_view(SelectionViewParams {
            title: Some("Progress Legend".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            initial_selected_idx: Some(1),
            ..Default::default()
        });
    }

    pub(crate) fn open_progress_legend_mode_picker(&mut self) {
        let mut items = Vec::new();
        for mode in [
            ProgressLegendMode::Off,
            ProgressLegendMode::Auto,
            ProgressLegendMode::Always,
        ] {
            items.push(SelectionItem {
                name: mode.to_string(),
                description: Some(format!("Set progress legend mode to '{mode}'.")),
                is_current: self.progress_legend_mode == mode,
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::SetProgressLegendMode { mode });
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        self.show_selection_view(SelectionViewParams {
            title: Some("Progress legend mode".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }

    fn default_status_line_items() -> Vec<StatusLineItem> {
        vec![
            StatusLineItem::ModelWithReasoning,
            StatusLineItem::ContextRemaining,
            StatusLineItem::CurrentDir,
            StatusLineItem::GitBranch,
        ]
    }

    fn default_status_line_item_ids() -> Vec<String> {
        Self::default_status_line_items()
            .into_iter()
            .map(|item| item.to_string())
            .collect()
    }

    /// Parses configured status-line ids into known items and collects unknown ids.
    ///
    /// Unknown ids are deduplicated in insertion order for warning messages.
    fn status_line_items_with_invalids(&self) -> (Vec<StatusLineItem>, Vec<String>) {
        let mut invalid = Vec::new();
        let mut invalid_seen = HashSet::new();
        let mut items = Vec::new();
        let Some(config_items) = self.config.tui_status_line.as_ref() else {
            return (Self::default_status_line_items(), invalid);
        };
        for id in config_items {
            match id.parse::<StatusLineItem>() {
                Ok(item) => items.push(item),
                Err(_) => {
                    if invalid_seen.insert(id.clone()) {
                        invalid.push(format!(r#""{id}""#));
                    }
                }
            }
        }
        (items, invalid)
    }

    fn status_line_cwd(&self) -> &Path {
        if let Some(snapshot) = self.active_runtime_context.as_ref() {
            return &snapshot.cwd;
        }
        self.current_cwd.as_ref().unwrap_or(&self.config.cwd)
    }

    fn status_line_project_root(&self) -> Option<PathBuf> {
        let cwd = self.status_line_cwd();
        if let Some(repo_root) = get_git_repo_root(cwd) {
            return Some(repo_root);
        }

        self.config
            .config_layer_stack
            .get_layers(ConfigLayerStackOrdering::LowestPrecedenceFirst, true)
            .iter()
            .find_map(|layer| match &layer.name {
                ConfigLayerSource::Project { dot_codex_folder } => {
                    dot_codex_folder.as_path().parent().map(Path::to_path_buf)
                }
                _ => None,
            })
    }

    fn status_line_project_root_name(&self) -> Option<String> {
        self.status_line_project_root().map(|root| {
            root.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| format_directory_display(&root, None))
        })
    }

    /// Resets git-branch cache state when the status-line cwd changes.
    ///
    /// The branch cache is keyed by cwd because branch lookup is performed relative to that path.
    /// Keeping stale branch values across cwd changes would surface incorrect repository context.
    fn sync_status_line_branch_state(&mut self, cwd: &Path) {
        if self
            .status_line_branch_cwd
            .as_ref()
            .is_some_and(|path| path == cwd)
        {
            return;
        }
        self.status_line_branch_cwd = Some(cwd.to_path_buf());
        self.status_line_branch = None;
        self.status_line_branch_pending = false;
        self.status_line_branch_lookup_complete = false;
    }

    /// Starts an async git-branch lookup unless one is already running.
    ///
    /// The resulting `StatusLineBranchUpdated` event carries the lookup cwd so callers can reject
    /// stale completions after directory changes.
    fn request_status_line_branch(&mut self, cwd: PathBuf) {
        if self.status_line_branch_pending {
            return;
        }
        self.status_line_branch_pending = true;
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let branch = current_branch_name(&cwd).await;
            tx.send(AppEvent::StatusLineBranchUpdated { cwd, branch });
        });
    }

    fn status_line_model_display_name(&self) -> &str {
        if let Some(snapshot) = self.active_runtime_context.as_ref()
            && !snapshot.model.trim().is_empty()
        {
            return snapshot.model.as_str();
        }
        self.model_display_name()
    }

    fn status_line_reasoning_effort(&self) -> Option<ReasoningEffortConfig> {
        if let Some(snapshot) = self.active_runtime_context.as_ref() {
            return snapshot.reasoning_effort;
        }
        self.effective_reasoning_effort()
    }

    /// Resolves a display string for one configured status-line item.
    ///
    /// Returning `None` means "omit this item for now", not "configuration error". Callers rely on
    /// this to keep partially available status lines readable while waiting for session, token, or
    /// git metadata.
    fn status_line_value_for_item(&self, item: &StatusLineItem) -> Option<String> {
        match item {
            StatusLineItem::ModelName => Some(self.status_line_model_display_name().to_string()),
            StatusLineItem::ModelWithReasoning => {
                let label =
                    Self::status_line_reasoning_effort_label(self.status_line_reasoning_effort());
                Some(format!("{} {label}", self.status_line_model_display_name()))
            }
            StatusLineItem::CurrentDir => {
                Some(format_directory_display(self.status_line_cwd(), None))
            }
            StatusLineItem::ProjectRoot => self.status_line_project_root_name(),
            StatusLineItem::GitBranch => self.status_line_branch.clone(),
            StatusLineItem::UsedTokens => {
                let usage = self.status_line_total_usage();
                let total = usage.tokens_in_context_window();
                if total <= 0 {
                    None
                } else {
                    Some(format!("{} used", format_tokens_compact(total)))
                }
            }
            StatusLineItem::ContextRemaining => self
                .status_line_context_remaining_percent()
                .map(|remaining| format!("{remaining}% left")),
            StatusLineItem::ContextUsed => self
                .status_line_context_used_percent()
                .map(|used| format!("{used}% used")),
            StatusLineItem::FiveHourLimit => {
                let window = self
                    .rate_limit_snapshot
                    .as_ref()
                    .and_then(|s| s.primary.as_ref());
                let label = window
                    .and_then(|window| window.window_minutes)
                    .map(get_limits_duration)
                    .unwrap_or_else(|| "5h".to_string());
                self.status_line_limit_display(window, &label)
            }
            StatusLineItem::WeeklyLimit => {
                let window = self
                    .rate_limit_snapshot
                    .as_ref()
                    .and_then(|s| s.secondary.as_ref());
                let label = window
                    .and_then(|window| window.window_minutes)
                    .map(get_limits_duration)
                    .unwrap_or_else(|| "weekly".to_string());
                self.status_line_limit_display(window, &label)
            }
            StatusLineItem::CodexVersion => Some(CODEX_CLI_VERSION.to_string()),
            StatusLineItem::ContextWindowSize => self
                .status_line_context_window_size()
                .map(|cws| format!("{} window", format_tokens_compact(cws))),
            StatusLineItem::TotalInputTokens => Some(format!(
                "{} in",
                format_tokens_compact(self.status_line_total_usage().input_tokens)
            )),
            StatusLineItem::TotalOutputTokens => Some(format!(
                "{} out",
                format_tokens_compact(self.status_line_total_usage().output_tokens)
            )),
            StatusLineItem::SessionId => self
                .active_runtime_context
                .as_ref()
                .map(|snapshot| snapshot.session_id.to_string())
                .or_else(|| self.thread_id.map(|id| id.to_string())),
        }
    }

    fn status_line_context_window_size(&self) -> Option<i64> {
        self.status_subject_context_window_size()
    }

    fn status_line_context_remaining_percent(&self) -> Option<i64> {
        let Some(context_window) = self.status_line_context_window_size() else {
            return Some(100);
        };
        let default_usage = TokenUsage::default();
        let usage = self
            .status_subject_token_info()
            .map(|info| &info.last_token_usage)
            .unwrap_or(&default_usage);
        Some(
            usage
                .percent_of_context_window_remaining(context_window)
                .clamp(0, 100),
        )
    }

    fn status_line_context_used_percent(&self) -> Option<i64> {
        let remaining = self.status_line_context_remaining_percent().unwrap_or(100);
        Some((100 - remaining).clamp(0, 100))
    }

    fn status_line_total_usage(&self) -> TokenUsage {
        self.status_subject_token_info()
            .map(|info| info.total_token_usage.clone())
            .unwrap_or_default()
    }

    fn status_subject_token_info(&self) -> Option<&TokenUsageInfo> {
        if let Some(snapshot) = self.active_runtime_context.as_ref() {
            return snapshot.token_info.as_ref();
        }

        self.token_info.as_ref()
    }

    fn status_subject_model_context_window(&self) -> Option<i64> {
        if let Some(snapshot) = self.active_runtime_context.as_ref() {
            return snapshot.model_context_window;
        }

        self.config.model_context_window
    }

    fn status_subject_context_window_size(&self) -> Option<i64> {
        self.status_subject_token_info()
            .and_then(|info| info.model_context_window)
            .or_else(|| self.status_subject_model_context_window())
    }

    fn status_line_limit_display(
        &self,
        window: Option<&RateLimitWindowDisplay>,
        label: &str,
    ) -> Option<String> {
        let window = window?;
        let remaining = (100.0f64 - window.used_percent).clamp(0.0f64, 100.0f64);
        Some(format!("{label} {remaining:.0}%"))
    }

    fn status_line_reasoning_effort_label(effort: Option<ReasoningEffortConfig>) -> &'static str {
        match effort {
            Some(ReasoningEffortConfig::Minimal) => "minimal",
            Some(ReasoningEffortConfig::Low) => "low",
            Some(ReasoningEffortConfig::Medium) => "medium",
            Some(ReasoningEffortConfig::High) => "high",
            Some(ReasoningEffortConfig::XHigh) => "xhigh",
            None | Some(ReasoningEffortConfig::None) => "default",
        }
    }

    pub(crate) fn add_ps_output(&mut self) {
        let processes = self
            .unified_exec_processes
            .iter()
            .map(|process| history_cell::UnifiedExecProcessDetails {
                command_display: process.command_display.clone(),
                recent_chunks: process.recent_chunks.clone(),
            })
            .collect();
        self.add_to_history(history_cell::new_unified_exec_processes_output(processes));
    }

    fn stop_rate_limit_poller(&mut self) {
        if let Some(handle) = self.rate_limit_poller.take() {
            handle.abort();
        }
    }

    fn prefetch_connectors(&mut self) {
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
                let connectors = connectors::list_connectors(&config).await?;
                Ok(ConnectorsSnapshot { connectors })
            }
            .await;
            let result = result.map_err(|err| format!("Failed to load apps: {err}"));
            app_event_tx.send(AppEvent::ConnectorsLoaded(result));
        });
    }

    fn prefetch_rate_limits(&mut self) {
        self.stop_rate_limit_poller();

        if !self
            .auth_manager
            .auth_cached()
            .as_ref()
            .is_some_and(CodexAuth::is_chatgpt_auth)
        {
            return;
        }

        let base_url = self.config.chatgpt_base_url.clone();
        let app_event_tx = self.app_event_tx.clone();
        let auth_manager = Arc::clone(&self.auth_manager);

        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));

            loop {
                if let Some(auth) = auth_manager.auth().await
                    && auth.is_chatgpt_auth()
                    && let Some(snapshot) = fetch_rate_limits(base_url.clone(), auth).await
                {
                    app_event_tx.send(AppEvent::RateLimitSnapshotFetched(snapshot));
                }
                interval.tick().await;
            }
        });

        self.rate_limit_poller = Some(handle);
    }

    fn lower_cost_preset(&self) -> Option<ModelPreset> {
        let models = self.models_manager.try_list_models(&self.config).ok()?;
        models
            .iter()
            .find(|preset| preset.show_in_picker && preset.model == NUDGE_MODEL_SLUG)
            .cloned()
    }

    fn rate_limit_switch_prompt_hidden(&self) -> bool {
        self.config
            .notices
            .hide_rate_limit_model_nudge
            .unwrap_or(false)
    }

    fn maybe_show_pending_rate_limit_prompt(&mut self) {
        if self.rate_limit_switch_prompt_hidden() {
            self.rate_limit_switch_prompt = RateLimitSwitchPromptState::Idle;
            return;
        }
        if !matches!(
            self.rate_limit_switch_prompt,
            RateLimitSwitchPromptState::Pending
        ) {
            return;
        }
        if let Some(preset) = self.lower_cost_preset() {
            self.open_rate_limit_switch_prompt(preset);
            self.rate_limit_switch_prompt = RateLimitSwitchPromptState::Shown;
        } else {
            self.rate_limit_switch_prompt = RateLimitSwitchPromptState::Idle;
        }
    }

    fn open_rate_limit_switch_prompt(&mut self, preset: ModelPreset) {
        let switch_model = preset.model.to_string();
        let display_name = preset.display_name.to_string();
        let default_effort: ReasoningEffortConfig = preset.default_reasoning_effort;

        let switch_actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
            tx.send(AppEvent::CodexOp(Op::OverrideTurnContext {
                cwd: None,
                approval_policy: None,
                sandbox_policy: None,
                windows_sandbox_level: None,
                model: Some(switch_model.clone()),
                effort: Some(Some(default_effort)),
                summary: None,
                collaboration_mode: None,
                personality: None,
                service_tier: None,
            }));
            tx.send(AppEvent::UpdateModel(switch_model.clone()));
            tx.send(AppEvent::UpdateReasoningEffort(Some(default_effort)));
        })];

        let keep_actions: Vec<SelectionAction> = Vec::new();
        let never_actions: Vec<SelectionAction> = vec![Box::new(|tx| {
            tx.send(AppEvent::UpdateRateLimitSwitchPromptHidden(true));
            tx.send(AppEvent::PersistRateLimitSwitchPromptHidden);
        })];
        let description = if preset.description.is_empty() {
            Some("Uses fewer credits for upcoming turns.".to_string())
        } else {
            Some(preset.description)
        };

        let items = vec![
            SelectionItem {
                name: format!("Switch to {display_name}"),
                description,
                selected_description: None,
                is_current: false,
                actions: switch_actions,
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Keep current model".to_string(),
                description: None,
                selected_description: None,
                is_current: false,
                actions: keep_actions,
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Keep current model (never show again)".to_string(),
                description: Some(
                    "Hide future rate limit reminders about switching models.".to_string(),
                ),
                selected_description: None,
                is_current: false,
                actions: never_actions,
                dismiss_on_select: true,
                ..Default::default()
            },
        ];

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Approaching rate limits".to_string()),
            subtitle: Some(format!("Switch to {display_name} for lower credit usage?")),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }

    /// Open a popup to choose a quick auto model. Selecting "All models"
    /// opens the full picker with every available preset.
    pub(crate) fn open_model_popup(&mut self) {
        if !self.is_session_configured() {
            self.add_info_message(
                "Model selection is disabled until startup completes.".to_string(),
                None,
            );
            return;
        }

        let presets: Vec<ModelPreset> = match self.models_manager.try_list_models(&self.config) {
            Ok(models) => models,
            Err(_) => {
                self.add_info_message(
                    "Models are being updated; please try /model again in a moment.".to_string(),
                    None,
                );
                return;
            }
        };
        self.open_model_popup_with_presets(presets);
    }

    pub(crate) fn open_personality_popup(&mut self) {
        if !self.is_session_configured() {
            self.add_info_message(
                "Personality selection is disabled until startup completes.".to_string(),
                None,
            );
            return;
        }
        if !self.current_model_supports_personality() {
            let current_model = self.current_model();
            self.add_error_message(format!(
                "Current model ({current_model}) doesn't support personalities. Try /model to pick a different model."
            ));
            return;
        }
        self.open_personality_popup_for_current_model();
    }

    fn open_personality_popup_for_current_model(&mut self) {
        let current_personality = self.config.personality.unwrap_or(Personality::Friendly);
        let personalities = [Personality::Friendly, Personality::Pragmatic];
        let supports_personality = self.current_model_supports_personality();

        let items: Vec<SelectionItem> = personalities
            .into_iter()
            .map(|personality| {
                let name = Self::personality_label(personality).to_string();
                let description = Some(Self::personality_description(personality).to_string());
                let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
                    tx.send(AppEvent::CodexOp(Op::OverrideTurnContext {
                        cwd: None,
                        approval_policy: None,
                        sandbox_policy: None,
                        model: None,
                        effort: None,
                        summary: None,
                        collaboration_mode: None,
                        windows_sandbox_level: None,
                        personality: Some(personality),
                        service_tier: None,
                    }));
                    tx.send(AppEvent::UpdatePersonality(personality));
                    tx.send(AppEvent::PersistPersonalitySelection { personality });
                })];
                SelectionItem {
                    name,
                    description,
                    is_current: current_personality == personality,
                    is_disabled: !supports_personality,
                    actions,
                    dismiss_on_select: true,
                    ..Default::default()
                }
            })
            .collect();

        let mut header = ColumnRenderable::new();
        header.push(Line::from("Select Personality".bold()));
        header.push(Line::from(
            "Choose a communication style for Codex. Disable in /experimental.".dim(),
        ));

        self.bottom_pane.show_selection_view(SelectionViewParams {
            header: Box::new(header),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }

    fn model_menu_header(&self, title: &str, subtitle: &str) -> Box<dyn Renderable> {
        let title = title.to_string();
        let subtitle = subtitle.to_string();
        let mut header = ColumnRenderable::new();
        header.push(Line::from(title.bold()));
        header.push(Line::from(subtitle.dim()));
        if let Some(warning) = self.model_menu_warning_line() {
            header.push(warning);
        }
        Box::new(header)
    }

    fn model_menu_warning_line(&self) -> Option<Line<'static>> {
        let base_url = self.custom_openai_base_url()?;
        let warning = format!(
            "Warning: OPENAI_BASE_URL is set to {base_url}. Selecting models may not be supported or work properly."
        );
        Some(Line::from(warning.red()))
    }

    fn custom_openai_base_url(&self) -> Option<String> {
        if !self.config.model_provider.is_openai() {
            return None;
        }

        let base_url = self.config.model_provider.base_url.as_ref()?;
        let trimmed = base_url.trim();
        if trimmed.is_empty() {
            return None;
        }

        let normalized = trimmed.trim_end_matches('/');
        if normalized == DEFAULT_OPENAI_BASE_URL {
            return None;
        }

        Some(trimmed.to_string())
    }

    pub(crate) fn open_model_popup_with_presets(&mut self, presets: Vec<ModelPreset>) {
        let presets = Self::picker_visible_model_presets(presets);

        let current_model = self.current_model();
        let current_label = presets
            .iter()
            .find(|preset| preset.model.as_str() == current_model)
            .map(|preset| preset.display_name.to_string())
            .unwrap_or_else(|| self.model_display_name().to_string());

        let (mut auto_presets, other_presets): (Vec<ModelPreset>, Vec<ModelPreset>) = presets
            .into_iter()
            .partition(|preset| Self::is_auto_model(&preset.model));

        if auto_presets.is_empty() {
            self.open_all_models_popup(other_presets);
            return;
        }

        auto_presets.sort_by_key(|preset| Self::auto_model_order(&preset.model));

        let mut items: Vec<SelectionItem> = auto_presets
            .into_iter()
            .map(|preset| {
                let description =
                    (!preset.description.is_empty()).then_some(preset.description.clone());
                let model = preset.model.clone();
                let actions = Self::model_selection_actions(
                    model.clone(),
                    Some(preset.default_reasoning_effort),
                );
                SelectionItem {
                    name: preset.display_name.clone(),
                    description,
                    is_current: model.as_str() == current_model,
                    is_default: preset.is_default,
                    actions,
                    dismiss_on_select: true,
                    ..Default::default()
                }
            })
            .collect();

        if !other_presets.is_empty() {
            let all_models = other_presets;
            let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
                tx.send(AppEvent::OpenAllModelsPopup {
                    models: all_models.clone(),
                });
            })];

            let is_current = !items.iter().any(|item| item.is_current);
            let description = Some(format!(
                "Choose a specific model and reasoning level (current: {current_label})"
            ));

            items.push(SelectionItem {
                name: "All models".to_string(),
                description,
                is_current,
                actions,
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        let header = self.model_menu_header(
            "Select Model",
            "Pick a quick auto mode or browse all models.",
        );
        self.bottom_pane.show_selection_view(SelectionViewParams {
            footer_hint: Some(standard_popup_hint_line()),
            items,
            header,
            ..Default::default()
        });
    }

    fn is_auto_model(model: &str) -> bool {
        model.starts_with("codex-auto-")
    }

    fn picker_visible_model_presets(presets: Vec<ModelPreset>) -> Vec<ModelPreset> {
        presets
            .into_iter()
            .filter(|preset| preset.show_in_picker)
            .collect()
    }

    fn model_shortcut_choices(current_model: &str, presets: Vec<ModelPreset>) -> Vec<ModelPreset> {
        let (mut auto_presets, other_presets): (Vec<ModelPreset>, Vec<ModelPreset>) =
            Self::picker_visible_model_presets(presets)
                .into_iter()
                .partition(|preset| Self::is_auto_model(&preset.model));

        auto_presets.sort_by_key(|preset| Self::auto_model_order(&preset.model));
        if auto_presets.is_empty() {
            return other_presets;
        }

        if auto_presets
            .iter()
            .any(|preset| preset.model.as_str() == current_model)
        {
            return auto_presets;
        }

        let current_preset = other_presets
            .iter()
            .find(|preset| preset.model.as_str() == current_model)
            .cloned();
        if let Some(current_preset) = current_preset {
            auto_presets.insert(0, current_preset);
        }

        auto_presets
    }

    fn auto_model_order(model: &str) -> usize {
        match model {
            "codex-auto-fast" => 0,
            "codex-auto-balanced" => 1,
            "codex-auto-thorough" => 2,
            _ => 3,
        }
    }

    pub(crate) fn open_all_models_popup(&mut self, presets: Vec<ModelPreset>) {
        if presets.is_empty() {
            self.add_info_message(
                "No additional models are available right now.".to_string(),
                None,
            );
            return;
        }

        let mut items: Vec<SelectionItem> = Vec::new();
        for preset in presets.into_iter() {
            let description =
                (!preset.description.is_empty()).then_some(preset.description.to_string());
            let is_current = preset.model.as_str() == self.current_model();
            let single_supported_effort = preset.supported_reasoning_efforts.len() == 1;
            let preset_for_action = preset.clone();
            let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
                let preset_for_event = preset_for_action.clone();
                tx.send(AppEvent::OpenReasoningPopup {
                    model: preset_for_event,
                });
            })];
            items.push(SelectionItem {
                name: preset.display_name.clone(),
                description,
                is_current,
                is_default: preset.is_default,
                actions,
                dismiss_on_select: single_supported_effort,
                ..Default::default()
            });
        }

        let header = self.model_menu_header(
            "Select Model and Effort",
            "Access legacy models by running codex -m <model_name> or in your config.toml",
        );
        self.bottom_pane.show_selection_view(SelectionViewParams {
            footer_hint: Some("Press enter to select reasoning effort, or esc to dismiss.".into()),
            items,
            header,
            ..Default::default()
        });
    }

    pub(crate) fn open_collaboration_modes_popup(&mut self) {
        let presets = collaboration_modes::presets_for_tui(self.models_manager.as_ref());
        if presets.is_empty() {
            self.add_info_message(
                "No collaboration modes are available right now.".to_string(),
                None,
            );
            return;
        }

        let current_kind = self
            .active_collaboration_mask
            .as_ref()
            .and_then(|mask| mask.mode)
            .or_else(|| {
                collaboration_modes::default_mask(self.models_manager.as_ref())
                    .and_then(|mask| mask.mode)
            });
        let items: Vec<SelectionItem> = presets
            .into_iter()
            .map(|mask| {
                let name = mask.name.clone();
                let is_current = current_kind == mask.mode;
                let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
                    tx.send(AppEvent::UpdateCollaborationMode(mask.clone()));
                })];
                SelectionItem {
                    name,
                    is_current,
                    actions,
                    dismiss_on_select: true,
                    ..Default::default()
                }
            })
            .collect();

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Select Collaboration Mode".to_string()),
            subtitle: Some("Pick a collaboration preset.".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }

    fn model_selection_actions(
        model_for_action: String,
        effort_for_action: Option<ReasoningEffortConfig>,
    ) -> Vec<SelectionAction> {
        vec![Box::new(move |tx| {
            let effort_label = effort_for_action
                .map(|effort| effort.to_string())
                .unwrap_or_else(|| "default".to_string());
            tx.send(AppEvent::CodexOp(Op::OverrideTurnContext {
                cwd: None,
                approval_policy: None,
                sandbox_policy: None,
                windows_sandbox_level: None,
                model: Some(model_for_action.clone()),
                effort: Some(effort_for_action),
                summary: None,
                collaboration_mode: None,
                personality: None,
                service_tier: None,
            }));
            tx.send(AppEvent::UpdateModel(model_for_action.clone()));
            tx.send(AppEvent::UpdateReasoningEffort(effort_for_action));
            tx.send(AppEvent::PersistModelSelection {
                model: model_for_action.clone(),
                effort: effort_for_action,
            });
            tracing::info!(
                "Selected model: {}, Selected effort: {}",
                model_for_action,
                effort_label
            );
        })]
    }

    /// Open a popup to choose the reasoning effort (stage 2) for the given model.
    pub(crate) fn open_reasoning_popup(&mut self, preset: ModelPreset) {
        let default_effort: ReasoningEffortConfig = preset.default_reasoning_effort;
        let supported = preset.supported_reasoning_efforts;

        let warn_effort = if supported
            .iter()
            .any(|option| option.effort == ReasoningEffortConfig::XHigh)
        {
            Some(ReasoningEffortConfig::XHigh)
        } else if supported
            .iter()
            .any(|option| option.effort == ReasoningEffortConfig::High)
        {
            Some(ReasoningEffortConfig::High)
        } else {
            None
        };
        let warning_text = warn_effort.map(|effort| {
            let effort_label = Self::reasoning_effort_label(effort);
            format!("⚠ {effort_label} reasoning effort can quickly consume Plus plan rate limits.")
        });
        let warn_for_model = preset.model.starts_with("gpt-5.2");

        struct EffortChoice {
            stored: Option<ReasoningEffortConfig>,
            display: ReasoningEffortConfig,
        }
        let mut choices: Vec<EffortChoice> = Vec::new();
        for effort in ReasoningEffortConfig::iter() {
            if supported.iter().any(|option| option.effort == effort) {
                choices.push(EffortChoice {
                    stored: Some(effort),
                    display: effort,
                });
            }
        }
        if choices.is_empty() {
            choices.push(EffortChoice {
                stored: Some(default_effort),
                display: default_effort,
            });
        }

        if choices.len() == 1 {
            if let Some(effort) = choices.first().and_then(|c| c.stored) {
                self.apply_model_and_effort(preset.model, Some(effort));
            } else {
                self.apply_model_and_effort(preset.model, None);
            }
            return;
        }

        let default_choice: Option<ReasoningEffortConfig> = choices
            .iter()
            .any(|choice| choice.stored == Some(default_effort))
            .then_some(Some(default_effort))
            .flatten()
            .or_else(|| choices.iter().find_map(|choice| choice.stored))
            .or(Some(default_effort));

        let model_slug = preset.model.to_string();
        let is_current_model = self.current_model() == preset.model.as_str();
        let highlight_choice = if is_current_model {
            self.effective_reasoning_effort()
        } else {
            default_choice
        };
        let selection_choice = highlight_choice.or(default_choice);
        let initial_selected_idx = choices
            .iter()
            .position(|choice| choice.stored == selection_choice)
            .or_else(|| {
                selection_choice
                    .and_then(|effort| choices.iter().position(|choice| choice.display == effort))
            });
        let mut items: Vec<SelectionItem> = Vec::new();
        for choice in choices.iter() {
            let effort = choice.display;
            let mut effort_label = Self::reasoning_effort_label(effort).to_string();
            if choice.stored == default_choice {
                effort_label.push_str(" (default)");
            }

            let description = choice
                .stored
                .and_then(|effort| {
                    supported
                        .iter()
                        .find(|option| option.effort == effort)
                        .map(|option| option.description.to_string())
                })
                .filter(|text| !text.is_empty());

            let show_warning = warn_for_model && warn_effort == Some(effort);
            let selected_description = if show_warning {
                warning_text.as_ref().map(|warning_message| {
                    description.as_ref().map_or_else(
                        || warning_message.clone(),
                        |d| format!("{d}\n{warning_message}"),
                    )
                })
            } else {
                None
            };

            let model_for_action = model_slug.clone();
            let actions = Self::model_selection_actions(model_for_action, choice.stored);

            items.push(SelectionItem {
                name: effort_label,
                description,
                selected_description,
                is_current: is_current_model && choice.stored == highlight_choice,
                actions,
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        let mut header = ColumnRenderable::new();
        header.push(Line::from(
            format!("Select Reasoning Level for {model_slug}").bold(),
        ));

        self.bottom_pane.show_selection_view(SelectionViewParams {
            header: Box::new(header),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            initial_selected_idx,
            ..Default::default()
        });
    }

    fn reasoning_effort_label(effort: ReasoningEffortConfig) -> &'static str {
        match effort {
            ReasoningEffortConfig::None => "None",
            ReasoningEffortConfig::Minimal => "Minimal",
            ReasoningEffortConfig::Low => "Low",
            ReasoningEffortConfig::Medium => "Medium",
            ReasoningEffortConfig::High => "High",
            ReasoningEffortConfig::XHigh => "Extra high",
        }
    }

    fn apply_model_and_effort(&self, model: String, effort: Option<ReasoningEffortConfig>) {
        self.app_event_tx
            .send(AppEvent::CodexOp(Op::OverrideTurnContext {
                cwd: None,
                approval_policy: None,
                sandbox_policy: None,
                windows_sandbox_level: None,
                model: Some(model.clone()),
                effort: Some(effort),
                summary: None,
                collaboration_mode: None,
                personality: None,
                service_tier: None,
            }));
        self.app_event_tx.send(AppEvent::UpdateModel(model.clone()));
        self.app_event_tx
            .send(AppEvent::UpdateReasoningEffort(effort));
        self.app_event_tx.send(AppEvent::PersistModelSelection {
            model: model.clone(),
            effort,
        });
        tracing::info!(
            "Selected model: {}, Selected effort: {}",
            model,
            effort
                .map(|e| e.to_string())
                .unwrap_or_else(|| "default".to_string())
        );
    }

    /// Open a popup to choose the approvals mode (ask for approval policy + sandbox policy).
    pub(crate) fn open_approvals_popup(&mut self) {
        self.open_approval_mode_popup(true);
    }

    /// Open a popup to choose the permissions mode (approval policy + sandbox policy).
    pub(crate) fn open_permissions_popup(&mut self) {
        let include_read_only = cfg!(target_os = "windows");
        self.open_approval_mode_popup(include_read_only);
    }

    fn open_approval_mode_popup(&mut self, include_read_only: bool) {
        let current_approval = self.config.approval_policy.value();
        let current_sandbox = self.config.sandbox_policy.get();
        let mut items: Vec<SelectionItem> = Vec::new();
        let presets: Vec<ApprovalPreset> = builtin_approval_presets();

        #[cfg(target_os = "windows")]
        let windows_sandbox_level = WindowsSandboxLevel::from_config(&self.config);
        #[cfg(target_os = "windows")]
        let windows_degraded_sandbox_enabled =
            matches!(windows_sandbox_level, WindowsSandboxLevel::RestrictedToken);
        #[cfg(not(target_os = "windows"))]
        let windows_degraded_sandbox_enabled = false;

        let show_elevate_sandbox_hint = codex_core::windows_sandbox::ELEVATED_SANDBOX_NUX_ENABLED
            && windows_degraded_sandbox_enabled
            && presets.iter().any(|preset| preset.id == "auto");

        for preset in presets.into_iter() {
            if !include_read_only && preset.id == "read-only" {
                continue;
            }
            let is_current =
                Self::preset_matches_current(current_approval, current_sandbox, &preset);
            let name = if preset.id == "auto" && windows_degraded_sandbox_enabled {
                "Default (non-elevated sandbox)".to_string()
            } else {
                preset.label.to_string()
            };
            let description = Some(preset.description.to_string());
            let disabled_reason = match self.config.approval_policy.can_set(&preset.approval) {
                Ok(()) => None,
                Err(err) => Some(err.to_string()),
            };
            let requires_confirmation = preset.id == "full-access"
                && !self
                    .config
                    .notices
                    .hide_full_access_warning
                    .unwrap_or(false);
            let actions: Vec<SelectionAction> = if requires_confirmation {
                let preset_clone = preset.clone();
                vec![Box::new(move |tx| {
                    tx.send(AppEvent::OpenFullAccessConfirmation {
                        preset: preset_clone.clone(),
                        return_to_permissions: !include_read_only,
                    });
                })]
            } else if preset.id == "auto" {
                #[cfg(target_os = "windows")]
                {
                    if WindowsSandboxLevel::from_config(&self.config)
                        == WindowsSandboxLevel::Disabled
                    {
                        let preset_clone = preset.clone();
                        if codex_core::windows_sandbox::ELEVATED_SANDBOX_NUX_ENABLED
                            && codex_core::windows_sandbox::sandbox_setup_is_complete(
                                self.config.codex_home.as_path(),
                            )
                        {
                            vec![Box::new(move |tx| {
                                tx.send(AppEvent::EnableWindowsSandboxForAgentMode {
                                    preset: preset_clone.clone(),
                                    mode: WindowsSandboxEnableMode::Elevated,
                                });
                            })]
                        } else {
                            vec![Box::new(move |tx| {
                                tx.send(AppEvent::OpenWindowsSandboxEnablePrompt {
                                    preset: preset_clone.clone(),
                                });
                            })]
                        }
                    } else if let Some((sample_paths, extra_count, failed_scan)) =
                        self.world_writable_warning_details()
                    {
                        let preset_clone = preset.clone();
                        vec![Box::new(move |tx| {
                            tx.send(AppEvent::OpenWorldWritableWarningConfirmation {
                                preset: Some(preset_clone.clone()),
                                sample_paths: sample_paths.clone(),
                                extra_count,
                                failed_scan,
                            });
                        })]
                    } else {
                        Self::approval_preset_actions(preset.approval, preset.sandbox.clone())
                    }
                }
                #[cfg(not(target_os = "windows"))]
                {
                    Self::approval_preset_actions(preset.approval, preset.sandbox.clone())
                }
            } else {
                Self::approval_preset_actions(preset.approval, preset.sandbox.clone())
            };
            items.push(SelectionItem {
                name,
                description,
                is_current,
                actions,
                dismiss_on_select: true,
                disabled_reason,
                ..Default::default()
            });
        }

        let footer_note = show_elevate_sandbox_hint.then(|| {
            vec![
                "The non-elevated sandbox protects your files and prevents network access under most circumstances. However, it carries greater risk if prompt injected. To upgrade to the elevated sandbox, run ".dim(),
                "/setup-elevated-sandbox".cyan(),
                ".".dim(),
            ]
            .into()
        });

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Update Model Permissions".to_string()),
            footer_note,
            footer_hint: Some(standard_popup_hint_line()),
            items,
            header: Box::new(()),
            ..Default::default()
        });
    }

    pub(crate) fn open_experimental_popup(&mut self) {
        let features: Vec<ExperimentalFeatureItem> = FEATURES
            .iter()
            .filter_map(|spec| {
                let name = spec.stage.experimental_menu_name()?;
                let description = spec.stage.experimental_menu_description()?;
                Some(ExperimentalFeatureItem {
                    feature: spec.id,
                    name: name.to_string(),
                    description: description.to_string(),
                    enabled: self.config.features.enabled(spec.id),
                })
            })
            .collect();

        let view = ExperimentalFeaturesView::new(features, self.app_event_tx.clone());
        self.bottom_pane.show_view(Box::new(view));
    }

    fn approval_preset_actions(
        approval: AskForApproval,
        sandbox: SandboxPolicy,
    ) -> Vec<SelectionAction> {
        vec![Box::new(move |tx| {
            let sandbox_clone = sandbox.clone();
            tx.send(AppEvent::CodexOp(Op::OverrideTurnContext {
                cwd: None,
                approval_policy: Some(approval),
                sandbox_policy: Some(sandbox_clone.clone()),
                windows_sandbox_level: None,
                model: None,
                effort: None,
                summary: None,
                collaboration_mode: None,
                personality: None,
                service_tier: None,
            }));
            tx.send(AppEvent::UpdateAskForApprovalPolicy(approval));
            tx.send(AppEvent::UpdateSandboxPolicy(sandbox_clone));
        })]
    }

    fn preset_matches_current(
        current_approval: AskForApproval,
        current_sandbox: &SandboxPolicy,
        preset: &ApprovalPreset,
    ) -> bool {
        if current_approval != preset.approval {
            return false;
        }
        matches!(
            (&preset.sandbox, current_sandbox),
            (SandboxPolicy::ReadOnly, SandboxPolicy::ReadOnly)
                | (
                    SandboxPolicy::DangerFullAccess,
                    SandboxPolicy::DangerFullAccess
                )
                | (
                    SandboxPolicy::WorkspaceWrite { .. },
                    SandboxPolicy::WorkspaceWrite { .. }
                )
        )
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn world_writable_warning_details(&self) -> Option<(Vec<String>, usize, bool)> {
        if self
            .config
            .notices
            .hide_world_writable_warning
            .unwrap_or(false)
        {
            return None;
        }
        let cwd = self.config.cwd.clone();
        let env_map: std::collections::HashMap<String, String> = std::env::vars().collect();
        match codex_windows_sandbox::apply_world_writable_scan_and_denies(
            self.config.codex_home.as_path(),
            cwd.as_path(),
            &env_map,
            self.config.sandbox_policy.get(),
            Some(self.config.codex_home.as_path()),
        ) {
            Ok(_) => None,
            Err(_) => Some((Vec::new(), 0, true)),
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[allow(dead_code)]
    pub(crate) fn world_writable_warning_details(&self) -> Option<(Vec<String>, usize, bool)> {
        None
    }

    pub(crate) fn open_full_access_confirmation(
        &mut self,
        preset: ApprovalPreset,
        return_to_permissions: bool,
    ) {
        let approval = preset.approval;
        let sandbox = preset.sandbox;
        let mut header_children: Vec<Box<dyn Renderable>> = Vec::new();
        let title_line = Line::from("Enable full access?").bold();
        let info_line = Line::from(vec![
            "When Codex runs with full access, it can edit any file on your computer and run commands with network, without your approval. "
                .into(),
            "Exercise caution when enabling full access. This significantly increases the risk of data loss, leaks, or unexpected behavior."
                .fg(Color::Red),
        ]);
        header_children.push(Box::new(title_line));
        header_children.push(Box::new(
            Paragraph::new(vec![info_line]).wrap(Wrap { trim: false }),
        ));
        let header = ColumnRenderable::with(header_children);

        let mut accept_actions = Self::approval_preset_actions(approval, sandbox.clone());
        accept_actions.push(Box::new(|tx| {
            tx.send(AppEvent::UpdateFullAccessWarningAcknowledged(true));
        }));

        let mut accept_and_remember_actions = Self::approval_preset_actions(approval, sandbox);
        accept_and_remember_actions.push(Box::new(|tx| {
            tx.send(AppEvent::UpdateFullAccessWarningAcknowledged(true));
            tx.send(AppEvent::PersistFullAccessWarningAcknowledged);
        }));

        let deny_actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
            if return_to_permissions {
                tx.send(AppEvent::OpenPermissionsPopup);
            } else {
                tx.send(AppEvent::OpenApprovalsPopup);
            }
        })];

        let items = vec![
            SelectionItem {
                name: "Yes, continue anyway".to_string(),
                description: Some("Apply full access for this session".to_string()),
                actions: accept_actions,
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Yes, and don't ask again".to_string(),
                description: Some("Enable full access and remember this choice".to_string()),
                actions: accept_and_remember_actions,
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Cancel".to_string(),
                description: Some("Go back without enabling full access".to_string()),
                actions: deny_actions,
                dismiss_on_select: true,
                ..Default::default()
            },
        ];

        self.bottom_pane.show_selection_view(SelectionViewParams {
            footer_hint: Some(standard_popup_hint_line()),
            items,
            header: Box::new(header),
            ..Default::default()
        });
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn open_world_writable_warning_confirmation(
        &mut self,
        preset: Option<ApprovalPreset>,
        sample_paths: Vec<String>,
        extra_count: usize,
        failed_scan: bool,
    ) {
        let (approval, sandbox) = match &preset {
            Some(p) => (Some(p.approval), Some(p.sandbox.clone())),
            None => (None, None),
        };
        let mut header_children: Vec<Box<dyn Renderable>> = Vec::new();
        let describe_policy = |policy: &SandboxPolicy| match policy {
            SandboxPolicy::WorkspaceWrite { .. } => "Agent mode",
            SandboxPolicy::ReadOnly => "Read-Only mode",
            _ => "Agent mode",
        };
        let mode_label = preset
            .as_ref()
            .map(|p| describe_policy(&p.sandbox))
            .unwrap_or_else(|| describe_policy(self.config.sandbox_policy.get()));
        let info_line = if failed_scan {
            Line::from(vec![
                "We couldn't complete the world-writable scan, so protections cannot be verified. "
                    .into(),
                format!("The Windows sandbox cannot guarantee protection in {mode_label}.")
                    .fg(Color::Red),
            ])
        } else {
            Line::from(vec![
                "The Windows sandbox cannot protect writes to folders that are writable by Everyone.".into(),
                " Consider removing write access for Everyone from the following folders:".into(),
            ])
        };
        header_children.push(Box::new(
            Paragraph::new(vec![info_line]).wrap(Wrap { trim: false }),
        ));

        if !sample_paths.is_empty() {
            // Show up to three examples and optionally an "and X more" line.
            let mut lines: Vec<Line> = Vec::new();
            lines.push(Line::from(""));
            for p in &sample_paths {
                lines.push(Line::from(format!("  - {p}")));
            }
            if extra_count > 0 {
                lines.push(Line::from(format!("and {extra_count} more")));
            }
            header_children.push(Box::new(Paragraph::new(lines).wrap(Wrap { trim: false })));
        }
        let header = ColumnRenderable::with(header_children);

        // Build actions ensuring acknowledgement happens before applying the new sandbox policy,
        // so downstream policy-change hooks don't re-trigger the warning.
        let mut accept_actions: Vec<SelectionAction> = Vec::new();
        // Suppress the immediate re-scan only when a preset will be applied (i.e., via /approvals),
        // to avoid duplicate warnings from the ensuing policy change.
        if preset.is_some() {
            accept_actions.push(Box::new(|tx| {
                tx.send(AppEvent::SkipNextWorldWritableScan);
            }));
        }
        if let (Some(approval), Some(sandbox)) = (approval, sandbox.clone()) {
            accept_actions.extend(Self::approval_preset_actions(approval, sandbox));
        }

        let mut accept_and_remember_actions: Vec<SelectionAction> = Vec::new();
        accept_and_remember_actions.push(Box::new(|tx| {
            tx.send(AppEvent::UpdateWorldWritableWarningAcknowledged(true));
            tx.send(AppEvent::PersistWorldWritableWarningAcknowledged);
        }));
        if let (Some(approval), Some(sandbox)) = (approval, sandbox) {
            accept_and_remember_actions.extend(Self::approval_preset_actions(approval, sandbox));
        }

        let items = vec![
            SelectionItem {
                name: "Continue".to_string(),
                description: Some(format!("Apply {mode_label} for this session")),
                actions: accept_actions,
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Continue and don't warn again".to_string(),
                description: Some(format!("Enable {mode_label} and remember this choice")),
                actions: accept_and_remember_actions,
                dismiss_on_select: true,
                ..Default::default()
            },
        ];

        self.bottom_pane.show_selection_view(SelectionViewParams {
            footer_hint: Some(standard_popup_hint_line()),
            items,
            header: Box::new(header),
            ..Default::default()
        });
    }

    #[cfg(not(target_os = "windows"))]
    pub(crate) fn open_world_writable_warning_confirmation(
        &mut self,
        _preset: Option<ApprovalPreset>,
        _sample_paths: Vec<String>,
        _extra_count: usize,
        _failed_scan: bool,
    ) {
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn open_windows_sandbox_enable_prompt(&mut self, preset: ApprovalPreset) {
        use ratatui_macros::line;

        if !codex_core::windows_sandbox::ELEVATED_SANDBOX_NUX_ENABLED {
            // Legacy flow (pre-NUX): explain the experimental sandbox and let the user enable it
            // directly (no elevation prompts).
            let mut header = ColumnRenderable::new();
            header.push(*Box::new(
                Paragraph::new(vec![
                    line!["Agent mode on Windows uses an experimental sandbox to limit network and filesystem access.".bold()],
                    line!["Learn more: https://developers.openai.com/codex/windows"],
                ])
                .wrap(Wrap { trim: false }),
            ));

            let preset_clone = preset;
            let items = vec![
                SelectionItem {
                    name: "Enable experimental sandbox".to_string(),
                    description: None,
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::EnableWindowsSandboxForAgentMode {
                            preset: preset_clone.clone(),
                            mode: WindowsSandboxEnableMode::Legacy,
                        });
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Go back".to_string(),
                    description: None,
                    actions: vec![Box::new(|tx| {
                        tx.send(AppEvent::OpenApprovalsPopup);
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
            ];

            self.bottom_pane.show_selection_view(SelectionViewParams {
                title: None,
                footer_hint: Some(standard_popup_hint_line()),
                items,
                header: Box::new(header),
                ..Default::default()
            });
            return;
        }

        let current_approval = self.config.approval_policy.value();
        let current_sandbox = self.config.sandbox_policy.get();
        let presets = builtin_approval_presets();
        let stay_full_access = presets
            .iter()
            .find(|preset| preset.id == "full-access")
            .is_some_and(|preset| {
                Self::preset_matches_current(current_approval, current_sandbox, preset)
            });
        self.otel_manager
            .counter("codex.windows_sandbox.elevated_prompt_shown", 1, &[]);

        let mut header = ColumnRenderable::new();
        header.push(*Box::new(
            Paragraph::new(vec![
                line!["Set Up Agent Sandbox".bold()],
                line![""],
                line!["Agent mode uses an experimental Windows sandbox that protects your files and prevents network access by default."],
                line!["Learn more: https://developers.openai.com/codex/windows"],
            ])
            .wrap(Wrap { trim: false }),
        ));

        let stay_label = if stay_full_access {
            "Stay in Agent Full Access".to_string()
        } else {
            "Stay in Read-Only".to_string()
        };
        let mut stay_actions = if stay_full_access {
            Vec::new()
        } else {
            presets
                .iter()
                .find(|preset| preset.id == "read-only")
                .map(|preset| {
                    Self::approval_preset_actions(preset.approval, preset.sandbox.clone())
                })
                .unwrap_or_default()
        };
        stay_actions.insert(
            0,
            Box::new({
                let otel = self.otel_manager.clone();
                move |_tx| {
                    otel.counter("codex.windows_sandbox.elevated_prompt_decline", 1, &[]);
                }
            }),
        );

        let accept_otel = self.otel_manager.clone();
        let items = vec![
            SelectionItem {
                name: "Set up agent sandbox (requires elevation)".to_string(),
                description: None,
                actions: vec![Box::new(move |tx| {
                    accept_otel.counter("codex.windows_sandbox.elevated_prompt_accept", 1, &[]);
                    tx.send(AppEvent::BeginWindowsSandboxElevatedSetup {
                        preset: preset.clone(),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: stay_label,
                description: None,
                actions: stay_actions,
                dismiss_on_select: true,
                ..Default::default()
            },
        ];

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: None,
            footer_hint: Some(standard_popup_hint_line()),
            items,
            header: Box::new(header),
            ..Default::default()
        });
    }

    #[cfg(not(target_os = "windows"))]
    pub(crate) fn open_windows_sandbox_enable_prompt(&mut self, _preset: ApprovalPreset) {}

    #[cfg(target_os = "windows")]
    pub(crate) fn open_windows_sandbox_fallback_prompt(
        &mut self,
        preset: ApprovalPreset,
        reason: WindowsSandboxFallbackReason,
    ) {
        use ratatui_macros::line;

        let _ = reason;

        let current_approval = self.config.approval_policy.value();
        let current_sandbox = self.config.sandbox_policy.get();
        let presets = builtin_approval_presets();
        let stay_full_access = presets
            .iter()
            .find(|preset| preset.id == "full-access")
            .is_some_and(|preset| {
                Self::preset_matches_current(current_approval, current_sandbox, preset)
            });
        let mut lines = Vec::new();
        lines.push(line!["Use Non-Elevated Sandbox?".bold()]);
        lines.push(line![""]);
        lines.push(line![
            "Elevation failed. You can also use a non-elevated sandbox, which protects your files and prevents network access under most circumstances. However, it carries greater risk if prompt injected."
        ]);
        lines.push(line![
            "Learn more: https://developers.openai.com/codex/windows"
        ]);

        let mut header = ColumnRenderable::new();
        header.push(*Box::new(Paragraph::new(lines).wrap(Wrap { trim: false })));

        let elevated_preset = preset.clone();
        let legacy_preset = preset;
        let stay_label = if stay_full_access {
            "Stay in Agent Full Access".to_string()
        } else {
            "Stay in Read-Only".to_string()
        };
        let mut stay_actions = if stay_full_access {
            Vec::new()
        } else {
            presets
                .iter()
                .find(|preset| preset.id == "read-only")
                .map(|preset| {
                    Self::approval_preset_actions(preset.approval, preset.sandbox.clone())
                })
                .unwrap_or_default()
        };
        stay_actions.insert(
            0,
            Box::new({
                let otel = self.otel_manager.clone();
                move |_tx| {
                    otel.counter("codex.windows_sandbox.fallback_stay_current", 1, &[]);
                }
            }),
        );
        let items = vec![
            SelectionItem {
                name: "Try elevated agent sandbox setup again".to_string(),
                description: None,
                actions: vec![Box::new({
                    let otel = self.otel_manager.clone();
                    let preset = elevated_preset;
                    move |tx| {
                        otel.counter("codex.windows_sandbox.fallback_retry_elevated", 1, &[]);
                        tx.send(AppEvent::BeginWindowsSandboxElevatedSetup {
                            preset: preset.clone(),
                        });
                    }
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Use non-elevated agent sandbox".to_string(),
                description: None,
                actions: vec![Box::new({
                    let otel = self.otel_manager.clone();
                    let preset = legacy_preset;
                    move |tx| {
                        otel.counter("codex.windows_sandbox.fallback_use_legacy", 1, &[]);
                        tx.send(AppEvent::EnableWindowsSandboxForAgentMode {
                            preset: preset.clone(),
                            mode: WindowsSandboxEnableMode::Legacy,
                        });
                    }
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: stay_label,
                description: None,
                actions: stay_actions,
                dismiss_on_select: true,
                ..Default::default()
            },
        ];

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: None,
            footer_hint: Some(standard_popup_hint_line()),
            items,
            header: Box::new(header),
            ..Default::default()
        });
    }

    #[cfg(not(target_os = "windows"))]
    pub(crate) fn open_windows_sandbox_fallback_prompt(
        &mut self,
        _preset: ApprovalPreset,
        _reason: WindowsSandboxFallbackReason,
    ) {
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn maybe_prompt_windows_sandbox_enable(&mut self) {
        if self.config.forced_auto_mode_downgraded_on_windows
            && WindowsSandboxLevel::from_config(&self.config) == WindowsSandboxLevel::Disabled
            && let Some(preset) = builtin_approval_presets()
                .into_iter()
                .find(|preset| preset.id == "auto")
        {
            self.open_windows_sandbox_enable_prompt(preset);
        }
    }

    #[cfg(not(target_os = "windows"))]
    pub(crate) fn maybe_prompt_windows_sandbox_enable(&mut self) {}

    #[cfg(target_os = "windows")]
    pub(crate) fn show_windows_sandbox_setup_status(&mut self) {
        // While elevated sandbox setup runs, prevent typing so the user doesn't
        // accidentally queue messages that will run under an unexpected mode.
        self.bottom_pane.set_composer_input_enabled(
            false,
            Some("Input disabled until setup completes.".to_string()),
        );
        self.bottom_pane.ensure_status_indicator();
        self.bottom_pane.set_interrupt_hint_visible(false);
        self.set_status_header("Setting up agent sandbox. This can take a minute.".to_string());
        self.request_redraw();
    }

    #[cfg(not(target_os = "windows"))]
    #[allow(dead_code)]
    pub(crate) fn show_windows_sandbox_setup_status(&mut self) {}

    #[cfg(target_os = "windows")]
    pub(crate) fn clear_windows_sandbox_setup_status(&mut self) {
        self.bottom_pane.set_composer_input_enabled(true, None);
        self.bottom_pane.hide_status_indicator();
        self.request_redraw();
    }

    #[cfg(not(target_os = "windows"))]
    pub(crate) fn clear_windows_sandbox_setup_status(&mut self) {}

    #[cfg(target_os = "windows")]
    pub(crate) fn clear_forced_auto_mode_downgrade(&mut self) {
        self.config.forced_auto_mode_downgraded_on_windows = false;
    }

    #[cfg(not(target_os = "windows"))]
    #[allow(dead_code)]
    pub(crate) fn clear_forced_auto_mode_downgrade(&mut self) {}

    /// Set the approval policy in the widget's config copy.
    pub(crate) fn set_approval_policy(&mut self, policy: AskForApproval) {
        if let Err(err) = self.config.approval_policy.set(policy) {
            tracing::warn!(%err, "failed to set approval_policy on chat config");
        }
    }

    /// Set the sandbox policy in the widget's config copy.
    pub(crate) fn set_sandbox_policy(&mut self, policy: SandboxPolicy) -> ConstraintResult<()> {
        #[cfg(target_os = "windows")]
        let should_clear_downgrade = !matches!(&policy, SandboxPolicy::ReadOnly)
            || WindowsSandboxLevel::from_config(&self.config) != WindowsSandboxLevel::Disabled;

        self.config.sandbox_policy.set(policy)?;

        #[cfg(target_os = "windows")]
        if should_clear_downgrade {
            self.config.forced_auto_mode_downgraded_on_windows = false;
        }

        Ok(())
    }

    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn set_feature_enabled(&mut self, feature: Feature, enabled: bool) {
        if enabled {
            self.config.features.enable(feature);
        } else {
            self.config.features.disable(feature);
        }
        if feature == Feature::Steer {
            self.bottom_pane.set_steer_enabled(enabled);
        }
        if feature == Feature::CollaborationModes {
            self.bottom_pane.set_collaboration_modes_enabled(enabled);
            let settings = self.current_collaboration_mode.settings.clone();
            self.current_collaboration_mode = CollaborationMode {
                mode: ModeKind::Default,
                settings,
            };
            self.active_collaboration_mask = if enabled {
                collaboration_modes::default_mask(self.models_manager.as_ref())
            } else {
                None
            };
            self.update_collaboration_mode_indicator();
            self.refresh_model_display();
            self.request_redraw();
        }
        if feature == Feature::Personality {
            self.sync_personality_command_enabled();
        }
        #[cfg(target_os = "windows")]
        if matches!(
            feature,
            Feature::WindowsSandbox | Feature::WindowsSandboxElevated
        ) {
            self.bottom_pane.set_windows_degraded_sandbox_active(
                codex_core::windows_sandbox::ELEVATED_SANDBOX_NUX_ENABLED
                    && matches!(
                        WindowsSandboxLevel::from_config(&self.config),
                        WindowsSandboxLevel::RestrictedToken
                    ),
            );
        }
    }

    pub(crate) fn set_full_access_warning_acknowledged(&mut self, acknowledged: bool) {
        self.config.notices.hide_full_access_warning = Some(acknowledged);
    }

    pub(crate) fn set_world_writable_warning_acknowledged(&mut self, acknowledged: bool) {
        self.config.notices.hide_world_writable_warning = Some(acknowledged);
    }

    pub(crate) fn set_rate_limit_switch_prompt_hidden(&mut self, hidden: bool) {
        self.config.notices.hide_rate_limit_model_nudge = Some(hidden);
        if hidden {
            self.rate_limit_switch_prompt = RateLimitSwitchPromptState::Idle;
        }
    }

    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn world_writable_warning_hidden(&self) -> bool {
        self.config
            .notices
            .hide_world_writable_warning
            .unwrap_or(false)
    }

    /// Set the reasoning effort in the stored collaboration mode.
    pub(crate) fn set_reasoning_effort(&mut self, effort: Option<ReasoningEffortConfig>) {
        self.current_collaboration_mode =
            self.current_collaboration_mode
                .with_updates(None, Some(effort), None);
        if self.collaboration_modes_enabled()
            && let Some(mask) = self.active_collaboration_mask.as_mut()
        {
            mask.reasoning_effort = Some(effort);
        }
    }

    /// Set the personality in the widget's config copy.
    pub(crate) fn set_personality(&mut self, personality: Personality) {
        self.config.personality = Some(personality);
    }

    /// Set the model in the widget's config copy and stored collaboration mode.
    pub(crate) fn set_model(&mut self, model: &str) {
        self.current_collaboration_mode =
            self.current_collaboration_mode
                .with_updates(Some(model.to_string()), None, None);
        if self.collaboration_modes_enabled()
            && let Some(mask) = self.active_collaboration_mask.as_mut()
        {
            mask.model = Some(model.to_string());
        }
        self.refresh_model_display();
    }

    pub(crate) fn current_model(&self) -> &str {
        if !self.collaboration_modes_enabled() {
            return self.current_collaboration_mode.model();
        }
        self.active_collaboration_mask
            .as_ref()
            .and_then(|mask| mask.model.as_deref())
            .unwrap_or_else(|| self.current_collaboration_mode.model())
    }

    fn sync_personality_command_enabled(&mut self) {
        self.bottom_pane
            .set_personality_command_enabled(self.config.features.enabled(Feature::Personality));
    }

    fn current_model_supports_personality(&self) -> bool {
        let model = self.current_model();
        self.models_manager
            .try_list_models(&self.config)
            .ok()
            .and_then(|models| {
                models
                    .into_iter()
                    .find(|preset| preset.model == model)
                    .map(|preset| preset.supports_personality)
            })
            .unwrap_or(false)
    }

    /// Return whether the effective model currently advertises image-input support.
    ///
    /// We intentionally default to `true` when model metadata cannot be read so transient catalog
    /// failures do not hard-block user input in the UI.
    fn current_model_supports_images(&self) -> bool {
        let model = self.current_model();
        self.models_manager
            .try_list_models(&self.config)
            .ok()
            .and_then(|models| {
                models
                    .into_iter()
                    .find(|preset| preset.model == model)
                    .map(|preset| preset.input_modalities.contains(&InputModality::Image))
            })
            .unwrap_or(true)
    }

    fn sync_image_paste_enabled(&mut self) {
        let enabled = self.current_model_supports_images();
        self.bottom_pane.set_image_paste_enabled(enabled);
    }

    fn image_inputs_not_supported_message(&self) -> String {
        format!(
            "Model {} does not support image inputs. Remove images or switch models.",
            self.current_model()
        )
    }

    #[allow(dead_code)] // Used in tests
    pub(crate) fn current_collaboration_mode(&self) -> &CollaborationMode {
        &self.current_collaboration_mode
    }

    #[cfg(test)]
    pub(crate) fn current_reasoning_effort(&self) -> Option<ReasoningEffortConfig> {
        self.effective_reasoning_effort()
    }

    #[cfg(test)]
    pub(crate) fn active_collaboration_mode_kind(&self) -> ModeKind {
        self.active_mode_kind()
    }

    fn is_session_configured(&self) -> bool {
        self.thread_id.is_some()
    }

    fn collaboration_modes_enabled(&self) -> bool {
        self.config.features.enabled(Feature::CollaborationModes)
    }

    fn initial_collaboration_mask(
        config: &Config,
        models_manager: &ModelsManager,
        model_override: Option<&str>,
    ) -> Option<CollaborationModeMask> {
        if !config.features.enabled(Feature::CollaborationModes) {
            return None;
        }
        let mut mask = match config.experimental_mode {
            Some(kind) => collaboration_modes::mask_for_kind(models_manager, kind)?,
            None => collaboration_modes::default_mask(models_manager)?,
        };
        if let Some(model_override) = model_override {
            mask.model = Some(model_override.to_string());
        }
        Some(mask)
    }

    fn active_mode_kind(&self) -> ModeKind {
        self.active_collaboration_mask
            .as_ref()
            .and_then(|mask| mask.mode)
            .unwrap_or(ModeKind::Default)
    }

    fn effective_reasoning_effort(&self) -> Option<ReasoningEffortConfig> {
        if !self.collaboration_modes_enabled() {
            return self.current_collaboration_mode.reasoning_effort();
        }
        let current_effort = self.current_collaboration_mode.reasoning_effort();
        self.active_collaboration_mask
            .as_ref()
            .and_then(|mask| mask.reasoning_effort)
            .unwrap_or(current_effort)
    }

    fn effective_collaboration_mode(&self) -> CollaborationMode {
        if !self.collaboration_modes_enabled() {
            return self.current_collaboration_mode.clone();
        }
        self.active_collaboration_mask.as_ref().map_or_else(
            || self.current_collaboration_mode.clone(),
            |mask| self.current_collaboration_mode.apply_mask(mask),
        )
    }

    fn refresh_model_display(&mut self) {
        let effective = self.effective_collaboration_mode();
        self.session_header.set_model(effective.model());
        self.bottom_pane
            .set_session_model(effective.model().to_string());
        self.bottom_pane
            .set_session_reasoning_effort(self.effective_reasoning_effort());
        // Keep composer paste affordances aligned with the currently effective model.
        self.sync_image_paste_enabled();
    }

    fn model_display_name(&self) -> &str {
        let model = self.current_model();
        if model.is_empty() {
            DEFAULT_MODEL_DISPLAY_NAME
        } else {
            model
        }
    }

    /// Get the label for the current collaboration mode.
    fn collaboration_mode_label(&self) -> Option<&'static str> {
        if !self.collaboration_modes_enabled() {
            return None;
        }
        let active_mode = self.active_mode_kind();
        active_mode
            .is_tui_visible()
            .then_some(active_mode.display_name())
    }

    fn collaboration_mode_indicator(&self) -> Option<CollaborationModeIndicator> {
        if !self.collaboration_modes_enabled() {
            return None;
        }
        match self.active_mode_kind() {
            ModeKind::Plan => Some(CollaborationModeIndicator::Plan),
            ModeKind::Default | ModeKind::PairProgramming | ModeKind::Execute => None,
        }
    }

    fn update_collaboration_mode_indicator(&mut self) {
        let indicator = self.collaboration_mode_indicator();
        self.bottom_pane.set_collaboration_mode_indicator(indicator);
    }

    fn personality_label(personality: Personality) -> &'static str {
        match personality {
            Personality::None => "None",
            Personality::Friendly => "Friendly",
            Personality::Pragmatic => "Pragmatic",
        }
    }

    fn personality_description(personality: Personality) -> &'static str {
        match personality {
            Personality::None => "No personality instructions.",
            Personality::Friendly => "Warm, collaborative, and helpful.",
            Personality::Pragmatic => "Concise, task-focused, and direct.",
        }
    }

    /// Cycle to the next collaboration mode variant (Plan -> Default -> Plan).
    fn cycle_collaboration_mode(&mut self) {
        if !self.collaboration_modes_enabled() {
            return;
        }

        if let Some(next_mask) = collaboration_modes::next_mask(
            self.models_manager.as_ref(),
            self.active_collaboration_mask.as_ref(),
        ) {
            self.set_collaboration_mask(next_mask);
        }
    }

    fn cycle_model_shortcut(&mut self, direction: isize) {
        let current_model = self.current_model().to_string();
        let presets: Vec<ModelPreset> = match self.models_manager.try_list_models(&self.config) {
            Ok(models) => models,
            Err(_) => {
                self.add_info_message(
                    "Models are being updated; please try again in a moment.".to_string(),
                    None,
                );
                return;
            }
        };

        let choices = Self::model_shortcut_choices(&current_model, presets);

        if choices.len() <= 1 {
            return;
        }

        let next_idx = if let Some(current_idx) = choices
            .iter()
            .position(|preset| preset.model == current_model)
        {
            let len = choices.len() as isize;
            (current_idx as isize + direction).rem_euclid(len) as usize
        } else if direction >= 0 {
            0
        } else {
            choices.len() - 1
        };

        let next = choices[next_idx].clone();
        self.apply_model_and_effort(next.model.to_string(), Some(next.default_reasoning_effort));
    }

    fn cycle_reasoning_effort_shortcut(&mut self, direction: isize) {
        let model_slug = self.current_model().to_string();
        let current_effort = self.effective_reasoning_effort();

        let presets = match self.models_manager.try_list_models(&self.config) {
            Ok(presets) => presets,
            Err(_) => {
                self.add_info_message(
                    "Models are being updated; please try again in a moment.".to_string(),
                    None,
                );
                return;
            }
        };

        let Some(preset) = Self::picker_visible_model_presets(presets)
            .into_iter()
            .find(|preset| preset.model == model_slug)
        else {
            self.add_info_message(
                format!("Model '{model_slug}' is not available right now."),
                None,
            );
            return;
        };

        let default_effort = preset.default_reasoning_effort;
        let mut supported: HashSet<ReasoningEffortConfig> = preset
            .supported_reasoning_efforts
            .into_iter()
            .map(|option| option.effort)
            .collect();
        supported.insert(default_effort);

        let choices: Vec<ReasoningEffortConfig> = ReasoningEffortConfig::iter()
            .filter(|effort| *effort != ReasoningEffortConfig::None && supported.contains(effort))
            .collect();

        if choices.len() <= 1 {
            return;
        }

        let current_idx = current_effort
            .and_then(|effort| choices.iter().position(|choice| *choice == effort))
            .or_else(|| choices.iter().position(|choice| *choice == default_effort))
            .unwrap_or(0);

        let len = choices.len() as isize;
        let next_idx = (current_idx as isize + direction).rem_euclid(len) as usize;
        let next_effort = Some(choices[next_idx]);
        self.apply_model_and_effort(model_slug, next_effort);
    }

    /// Update the active collaboration mask.
    ///
    /// When collaboration modes are enabled and a preset is selected,
    /// the current mode is attached to submissions as `Op::UserTurn { collaboration_mode: Some(...) }`.
    pub(crate) fn set_collaboration_mask(&mut self, mask: CollaborationModeMask) {
        if !self.collaboration_modes_enabled() {
            return;
        }
        self.active_collaboration_mask = Some(mask);
        self.update_collaboration_mode_indicator();
        self.refresh_model_display();
        self.request_redraw();
    }

    fn connectors_enabled(&self) -> bool {
        self.config.features.enabled(Feature::Apps)
    }

    fn connectors_for_mentions(&self) -> Option<&[connectors::AppInfo]> {
        if !self.connectors_enabled() {
            return None;
        }

        match &self.connectors_cache {
            ConnectorsCacheState::Ready(snapshot) => Some(snapshot.connectors.as_slice()),
            _ => None,
        }
    }

    /// Build a placeholder header cell while the session is configuring.
    fn placeholder_session_header_cell(config: &Config) -> Box<dyn HistoryCell> {
        let placeholder_style = Style::default().add_modifier(Modifier::DIM | Modifier::ITALIC);
        Box::new(history_cell::SessionHeaderHistoryCell::new_with_style(
            DEFAULT_MODEL_DISPLAY_NAME.to_string(),
            placeholder_style,
            None,
            config.cwd.clone(),
            CODEX_CLI_VERSION,
        ))
    }

    /// Merge the real session info cell with any placeholder header to avoid double boxes.
    fn apply_session_info_cell(&mut self, cell: history_cell::SessionInfoCell) {
        let mut session_info_cell = Some(Box::new(cell) as Box<dyn HistoryCell>);
        let merged_header = if let Some(active) = self.active_cell.take() {
            if active
                .as_any()
                .is::<history_cell::SessionHeaderHistoryCell>()
            {
                // Reuse the existing placeholder header to avoid rendering two boxes.
                if let Some(cell) = session_info_cell.take() {
                    self.active_cell = Some(cell);
                }
                true
            } else {
                self.active_cell = Some(active);
                false
            }
        } else {
            false
        };

        self.flush_active_cell();

        if !merged_header && let Some(cell) = session_info_cell {
            self.add_boxed_history(cell);
        }
    }

    pub(crate) fn add_info_message(&mut self, message: String, hint: Option<String>) {
        self.add_to_history(history_cell::new_info_event(message, hint));
        self.request_redraw();
    }

    pub(crate) fn add_plain_history_lines(&mut self, lines: Vec<Line<'static>>) {
        self.add_boxed_history(Box::new(PlainHistoryCell::new(lines)));
        self.request_redraw();
    }

    pub(crate) fn add_error_message(&mut self, message: String) {
        self.add_to_history(history_cell::new_error_event(message));
        self.request_redraw();
    }

    pub(crate) fn add_mcp_output(&mut self) {
        if self.config.mcp_servers.is_empty() {
            self.add_to_history(history_cell::empty_mcp_output());
        } else {
            self.submit_op(Op::ListMcpTools);
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

    fn open_connectors_popup(&mut self, connectors: &[connectors::AppInfo]) {
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
            let connector_label = connectors::connector_display_label(connector);
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

    fn connectors_popup_hint_line() -> Line<'static> {
        Line::from(vec![
            "Press ".into(),
            key_hint::plain(KeyCode::Esc).into(),
            " to close.".into(),
        ])
    }

    fn connector_brief_description(connector: &connectors::AppInfo) -> String {
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

    fn connector_description(connector: &connectors::AppInfo) -> Option<String> {
        connector
            .description
            .as_deref()
            .map(str::trim)
            .filter(|description| !description.is_empty())
            .map(str::to_string)
    }

    /// Forward file-search results to the bottom pane.
    pub(crate) fn apply_file_search_result(&mut self, query: String, matches: Vec<FileMatch>) {
        self.bottom_pane.on_file_search_result(query, matches);
    }

    /// Handles a Ctrl+C press at the chat-widget layer.
    ///
    /// The first press arms a time-bounded quit shortcut and shows a footer hint via the bottom
    /// pane. If cancellable work is active, Ctrl+C also submits `Op::Interrupt` after the shortcut
    /// is armed.
    ///
    /// If the same quit shortcut is pressed again before expiry, this requests a shutdown-first
    /// quit.
    fn on_ctrl_c(&mut self) {
        let key = key_hint::ctrl(KeyCode::Char('c'));
        let modal_or_popup_active = !self.bottom_pane.no_modal_or_popup_active();
        if self.bottom_pane.on_ctrl_c() == CancellationEvent::Handled {
            if DOUBLE_PRESS_QUIT_SHORTCUT_ENABLED {
                if modal_or_popup_active {
                    self.quit_shortcut_expires_at = None;
                    self.quit_shortcut_key = None;
                    self.bottom_pane.clear_quit_shortcut_hint();
                } else {
                    self.arm_quit_shortcut(key);
                }
            }
            return;
        }

        if !DOUBLE_PRESS_QUIT_SHORTCUT_ENABLED {
            if self.is_cancellable_work_active() {
                self.submit_op(Op::Interrupt);
            } else {
                self.request_quit_without_confirmation();
            }
            return;
        }

        if self.quit_shortcut_active_for(key) {
            self.quit_shortcut_expires_at = None;
            self.quit_shortcut_key = None;
            self.request_quit_without_confirmation();
            return;
        }

        self.arm_quit_shortcut(key);

        if self.is_cancellable_work_active() {
            self.submit_op(Op::Interrupt);
        }
    }

    /// Handles a Ctrl+D press at the chat-widget layer.
    ///
    /// Ctrl-D only participates in quit when the composer is empty and no modal/popup is active.
    /// Otherwise it should be routed to the active view and not attempt to quit.
    fn on_ctrl_d(&mut self) -> bool {
        let key = key_hint::ctrl(KeyCode::Char('d'));
        if !DOUBLE_PRESS_QUIT_SHORTCUT_ENABLED {
            if !self.bottom_pane.composer_is_empty() || !self.bottom_pane.no_modal_or_popup_active()
            {
                return false;
            }

            self.request_quit_without_confirmation();
            return true;
        }

        if self.quit_shortcut_active_for(key) {
            self.quit_shortcut_expires_at = None;
            self.quit_shortcut_key = None;
            self.request_quit_without_confirmation();
            return true;
        }

        if !self.bottom_pane.composer_is_empty() || !self.bottom_pane.no_modal_or_popup_active() {
            return false;
        }

        self.arm_quit_shortcut(key);
        true
    }

    /// True if `key` matches the armed quit shortcut and the window has not expired.
    fn quit_shortcut_active_for(&self, key: KeyBinding) -> bool {
        self.quit_shortcut_key == Some(key)
            && self
                .quit_shortcut_expires_at
                .is_some_and(|expires_at| Instant::now() < expires_at)
    }

    /// Arm the double-press quit shortcut and show the footer hint.
    ///
    /// This keeps the state machine (`quit_shortcut_*`) in `ChatWidget`, since
    /// it is the component that interprets Ctrl+C vs Ctrl+D and decides whether
    /// quitting is currently allowed, while delegating rendering to `BottomPane`.
    fn arm_quit_shortcut(&mut self, key: KeyBinding) {
        self.quit_shortcut_expires_at = Instant::now()
            .checked_add(QUIT_SHORTCUT_TIMEOUT)
            .or_else(|| Some(Instant::now()));
        self.quit_shortcut_key = Some(key);
        self.bottom_pane.show_quit_shortcut_hint(key);
    }

    // Review mode counts as cancellable work so Ctrl+C interrupts instead of quitting.
    fn is_cancellable_work_active(&self) -> bool {
        self.bottom_pane.is_task_running() || self.is_review_mode
    }

    pub(crate) fn composer_is_empty(&self) -> bool {
        self.bottom_pane.composer_is_empty()
    }

    pub(crate) fn submit_user_message_with_mode(
        &mut self,
        text: String,
        collaboration_mode: CollaborationModeMask,
    ) {
        self.set_collaboration_mask(collaboration_mode);
        self.submit_user_message(text.into());
    }

    /// True when the UI is in the regular composer state with no running task,
    /// no modal overlay (e.g. approvals or status indicator), and no composer popups.
    /// In this state Esc-Esc backtracking is enabled.
    pub(crate) fn is_normal_backtrack_mode(&self) -> bool {
        self.bottom_pane.is_normal_backtrack_mode()
    }

    pub(crate) fn insert_str(&mut self, text: &str) {
        self.bottom_pane.insert_str(text);
    }

    /// Replace the composer content with the provided text and reset cursor.
    pub(crate) fn set_composer_text(
        &mut self,
        text: String,
        text_elements: Vec<TextElement>,
        local_image_paths: Vec<PathBuf>,
    ) {
        self.bottom_pane
            .set_composer_text(text, text_elements, local_image_paths);
    }

    pub(crate) fn show_esc_backtrack_hint(&mut self) {
        self.bottom_pane.show_esc_backtrack_hint();
    }

    pub(crate) fn clear_esc_backtrack_hint(&mut self) {
        self.bottom_pane.clear_esc_backtrack_hint();
    }
    /// Forward an `Op` directly to codex.
    pub(crate) fn submit_op(&mut self, op: Op) {
        // Record outbound operation for session replay fidelity.
        crate::session_log::log_outbound_op(&op);
        if matches!(&op, Op::Review { .. } | Op::ReviewCompletedTurn)
            && !self.bottom_pane.is_task_running()
        {
            self.bottom_pane.set_task_running(true);
        }
        if let Err(e) = self.codex_op_tx.send(op) {
            tracing::error!("failed to submit op: {e}");
        }
    }

    fn on_list_mcp_tools(&mut self, ev: McpListToolsResponseEvent) {
        self.add_to_history(history_cell::new_mcp_tools_output(
            &self.config,
            ev.tools,
            ev.resources,
            ev.resource_templates,
            &ev.auth_statuses,
        ));
    }

    fn on_list_custom_prompts(&mut self, ev: ListCustomPromptsResponseEvent) {
        let len = ev.custom_prompts.len();
        debug!("received {len} custom prompts");
        // Forward to bottom pane so the slash popup can show them now.
        self.bottom_pane.set_custom_prompts(ev.custom_prompts);
    }

    fn on_list_skills(&mut self, ev: ListSkillsResponseEvent) {
        self.set_skills_from_response(&ev);
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

    pub(crate) fn open_review_popup(&mut self) {
        let mut items: Vec<SelectionItem> = Vec::new();

        items.push(SelectionItem {
            name: "Review against a base branch".to_string(),
            description: Some("(PR Style)".into()),
            actions: vec![Box::new({
                let cwd = self.config.cwd.clone();
                move |tx| {
                    tx.send(AppEvent::OpenReviewBranchPicker(cwd.clone()));
                }
            })],
            dismiss_on_select: false,
            ..Default::default()
        });

        items.push(SelectionItem {
            name: "Review uncommitted changes".to_string(),
            actions: vec![Box::new(move |tx: &AppEventSender| {
                tx.send(AppEvent::CodexOp(Op::Review {
                    review_request: ReviewRequest {
                        target: ReviewTarget::UncommittedChanges,
                        user_facing_hint: None,
                    },
                }));
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        // New: Review a specific commit (opens commit picker)
        items.push(SelectionItem {
            name: "Review a commit".to_string(),
            actions: vec![Box::new({
                let cwd = self.config.cwd.clone();
                move |tx| {
                    tx.send(AppEvent::OpenReviewCommitPicker(cwd.clone()));
                }
            })],
            dismiss_on_select: false,
            ..Default::default()
        });

        items.push(SelectionItem {
            name: "Custom review instructions".to_string(),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::OpenReviewCustomPrompt);
            })],
            dismiss_on_select: false,
            ..Default::default()
        });

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Select a review preset".into()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }

    pub(crate) async fn show_review_branch_picker(&mut self, cwd: &Path) {
        let branches = local_git_branches(cwd).await;
        let current_branch = current_branch_name(cwd)
            .await
            .unwrap_or_else(|| "(detached HEAD)".to_string());
        let mut items: Vec<SelectionItem> = Vec::with_capacity(branches.len());

        for option in branches {
            let branch = option.clone();
            items.push(SelectionItem {
                name: format!("{current_branch} -> {branch}"),
                actions: vec![Box::new(move |tx3: &AppEventSender| {
                    tx3.send(AppEvent::CodexOp(Op::Review {
                        review_request: ReviewRequest {
                            target: ReviewTarget::BaseBranch {
                                branch: branch.clone(),
                            },
                            user_facing_hint: None,
                        },
                    }));
                })],
                dismiss_on_select: true,
                search_value: Some(option),
                ..Default::default()
            });
        }

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Select a base branch".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            is_searchable: true,
            search_placeholder: Some("Type to search branches".to_string()),
            ..Default::default()
        });
    }

    pub(crate) async fn show_review_commit_picker(&mut self, cwd: &Path) {
        let commits = codex_core::git_info::recent_commits(cwd, 100).await;

        let mut items: Vec<SelectionItem> = Vec::with_capacity(commits.len());
        for entry in commits {
            let subject = entry.subject.clone();
            let sha = entry.sha.clone();
            let search_val = format!("{subject} {sha}");

            items.push(SelectionItem {
                name: subject.clone(),
                actions: vec![Box::new(move |tx3: &AppEventSender| {
                    tx3.send(AppEvent::CodexOp(Op::Review {
                        review_request: ReviewRequest {
                            target: ReviewTarget::Commit {
                                sha: sha.clone(),
                                title: Some(subject.clone()),
                            },
                            user_facing_hint: None,
                        },
                    }));
                })],
                dismiss_on_select: true,
                search_value: Some(search_val),
                ..Default::default()
            });
        }

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Select a commit to review".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            is_searchable: true,
            search_placeholder: Some("Type to search commits".to_string()),
            ..Default::default()
        });
    }

    pub(crate) fn show_review_custom_prompt(&mut self) {
        let tx = self.app_event_tx.clone();
        let view = CustomPromptView::new(
            "Custom review instructions".to_string(),
            "Type instructions and press Enter".to_string(),
            None,
            Box::new(move |prompt: String| {
                let trimmed = prompt.trim().to_string();
                if trimmed.is_empty() {
                    return;
                }
                tx.send(AppEvent::CodexOp(Op::Review {
                    review_request: ReviewRequest {
                        target: ReviewTarget::Custom {
                            instructions: trimmed,
                        },
                        user_facing_hint: None,
                    },
                }));
            }),
        );
        self.bottom_pane.show_view(Box::new(view));
    }

    pub(crate) fn token_usage(&self) -> TokenUsage {
        self.token_info
            .as_ref()
            .map(|ti| ti.total_token_usage.clone())
            .unwrap_or_default()
    }

    pub(crate) fn thread_id(&self) -> Option<ThreadId> {
        self.thread_id
    }

    pub(crate) fn thread_name(&self) -> Option<String> {
        self.thread_name.clone()
    }
    pub(crate) fn rollout_path(&self) -> Option<PathBuf> {
        self.current_rollout_path.clone()
    }

    /// Returns a cache key describing the current in-flight active cell for the transcript overlay.
    ///
    /// `Ctrl+T` renders committed transcript cells plus a render-only live tail derived from the
    /// current active cell, and the overlay caches that tail; this key is what it uses to decide
    /// whether it must recompute. When there is no active cell, this returns `None` so the overlay
    /// can drop the tail entirely.
    ///
    /// If callers mutate the active cell's transcript output without bumping the revision (or
    /// providing an appropriate animation tick), the overlay will keep showing a stale tail while
    /// the main viewport updates.
    pub(crate) fn active_cell_transcript_key(&self) -> Option<ActiveCellTranscriptKey> {
        let cell = self.active_cell.as_ref()?;
        Some(ActiveCellTranscriptKey {
            revision: self.active_cell_revision,
            is_stream_continuation: cell.is_stream_continuation(),
            animation_tick: cell.transcript_animation_tick(),
        })
    }

    /// Returns the active cell's transcript lines for a given terminal width.
    ///
    /// This is a convenience for the transcript overlay live-tail path, and it intentionally
    /// filters out empty results so the overlay can treat "nothing to render" as "no tail". Callers
    /// should pass the same width the overlay uses; using a different width will cause wrapping
    /// mismatches between the main viewport and the transcript overlay.
    pub(crate) fn active_cell_transcript_lines(&self, width: u16) -> Option<Vec<Line<'static>>> {
        let cell = self.active_cell.as_ref()?;
        let lines = cell.transcript_lines(width);
        (!lines.is_empty()).then_some(lines)
    }

    /// Return a reference to the widget's current config (includes any
    /// runtime overrides applied via TUI, e.g., model or approval policy).
    pub(crate) fn config_ref(&self) -> &Config {
        &self.config
    }

    pub(crate) fn clear_token_usage(&mut self) {
        self.token_info = None;
    }

    fn as_renderable(&self) -> RenderableItem<'_> {
        let active_cell_renderable = match &self.active_cell {
            Some(cell) => RenderableItem::Borrowed(cell).inset(Insets::tlbr(1, 0, 0, 0)),
            None => RenderableItem::Owned(Box::new(())),
        };
        let mut flex = FlexRenderable::new();
        flex.push(1, active_cell_renderable);
        flex.push(
            0,
            RenderableItem::Borrowed(&self.bottom_pane).inset(Insets::tlbr(1, 0, 0, 0)),
        );
        RenderableItem::Owned(Box::new(flex))
    }
}

impl Drop for ChatWidget {
    fn drop(&mut self) {
        self.stop_rate_limit_poller();
    }
}

impl Renderable for ChatWidget {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.as_renderable().render(area, buf);
        self.last_rendered_width.set(Some(area.width as usize));
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.as_renderable().desired_height(width)
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.as_renderable().cursor_pos(area)
    }
}

enum Notification {
    AgentTurnComplete { response: String },
    ExecApprovalRequested { command: String },
    EditApprovalRequested { cwd: PathBuf, changes: Vec<PathBuf> },
    ElicitationRequested { server_name: String },
}

impl Notification {
    fn display(&self) -> String {
        match self {
            Notification::AgentTurnComplete { response } => {
                Notification::agent_turn_preview(response)
                    .unwrap_or_else(|| "Agent turn complete".to_string())
            }
            Notification::ExecApprovalRequested { command } => {
                format!("Approval requested: {}", truncate_text(command, 30))
            }
            Notification::EditApprovalRequested { cwd, changes } => {
                format!(
                    "Codex wants to edit {}",
                    if changes.len() == 1 {
                        #[allow(clippy::unwrap_used)]
                        display_path_for(changes.first().unwrap(), cwd)
                    } else {
                        format!("{} files", changes.len())
                    }
                )
            }
            Notification::ElicitationRequested { server_name } => {
                format!("Approval requested by {server_name}")
            }
        }
    }

    fn type_name(&self) -> &str {
        match self {
            Notification::AgentTurnComplete { .. } => "agent-turn-complete",
            Notification::ExecApprovalRequested { .. }
            | Notification::EditApprovalRequested { .. }
            | Notification::ElicitationRequested { .. } => "approval-requested",
        }
    }

    fn allowed_for(&self, settings: &Notifications) -> bool {
        match settings {
            Notifications::Enabled(enabled) => *enabled,
            Notifications::Custom(allowed) => allowed.iter().any(|a| a == self.type_name()),
        }
    }

    fn agent_turn_preview(response: &str) -> Option<String> {
        let mut normalized = String::new();
        for part in response.split_whitespace() {
            if !normalized.is_empty() {
                normalized.push(' ');
            }
            normalized.push_str(part);
        }
        let trimmed = normalized.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(truncate_text(trimmed, AGENT_NOTIFICATION_PREVIEW_GRAPHEMES))
        }
    }
}

const AGENT_NOTIFICATION_PREVIEW_GRAPHEMES: usize = 200;

const PLACEHOLDERS: [&str; 8] = [
    "Explain this codebase",
    "Summarize recent commits",
    "Implement {feature}",
    "Find and fix a bug in @filename",
    "Write tests for @filename",
    "Improve documentation in @filename",
    "Run /review on my current changes",
    "Use /skills to list available skills",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyableRole {
    Response,
    User,
}

impl CopyableRole {
    fn label(self) -> &'static str {
        match self {
            CopyableRole::Response => "Response",
            CopyableRole::User => "User",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CopyableMessage {
    role: CopyableRole,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeBlockScope {
    LastResponse,
    AllResponses,
}

impl CodeBlockScope {
    fn label(self) -> &'static str {
        match self {
            CodeBlockScope::LastResponse => "Last response",
            CodeBlockScope::AllResponses => "All responses",
        }
    }

    fn description(self) -> &'static str {
        match self {
            CodeBlockScope::LastResponse => "Show blocks from the latest response only.",
            CodeBlockScope::AllResponses => "Show blocks from all responses in this chat.",
        }
    }

    fn toggle(self) -> Self {
        match self {
            CodeBlockScope::LastResponse => CodeBlockScope::AllResponses,
            CodeBlockScope::AllResponses => CodeBlockScope::LastResponse,
        }
    }
}

impl From<CopyCodeBlockScope> for CodeBlockScope {
    fn from(value: CopyCodeBlockScope) -> Self {
        match value {
            CopyCodeBlockScope::LastResponse => CodeBlockScope::LastResponse,
            CopyCodeBlockScope::AllResponses => CodeBlockScope::AllResponses,
        }
    }
}

impl From<CodeBlockScope> for CopyCodeBlockScope {
    fn from(value: CodeBlockScope) -> Self {
        match value {
            CodeBlockScope::LastResponse => CopyCodeBlockScope::LastResponse,
            CodeBlockScope::AllResponses => CopyCodeBlockScope::AllResponses,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageFilter {
    Responses,
    User,
    Both,
}

impl MessageFilter {
    fn label(self) -> &'static str {
        match self {
            MessageFilter::Responses => "Responses",
            MessageFilter::User => "User messages",
            MessageFilter::Both => "Responses + user messages",
        }
    }

    fn description(self) -> &'static str {
        match self {
            MessageFilter::Responses => "Only assistant responses",
            MessageFilter::User => "Only your messages",
            MessageFilter::Both => "Both response and user messages",
        }
    }

    fn next(self) -> Self {
        match self {
            MessageFilter::Responses => MessageFilter::User,
            MessageFilter::User => MessageFilter::Both,
            MessageFilter::Both => MessageFilter::Responses,
        }
    }

    fn includes(self, role: CopyableRole) -> bool {
        match self {
            MessageFilter::Responses => role == CopyableRole::Response,
            MessageFilter::User => role == CopyableRole::User,
            MessageFilter::Both => true,
        }
    }
}

impl From<CopyMessageFilter> for MessageFilter {
    fn from(value: CopyMessageFilter) -> Self {
        match value {
            CopyMessageFilter::Responses => MessageFilter::Responses,
            CopyMessageFilter::User => MessageFilter::User,
            CopyMessageFilter::Both => MessageFilter::Both,
        }
    }
}

impl From<MessageFilter> for CopyMessageFilter {
    fn from(value: MessageFilter) -> Self {
        match value {
            MessageFilter::Responses => CopyMessageFilter::Responses,
            MessageFilter::User => CopyMessageFilter::User,
            MessageFilter::Both => CopyMessageFilter::Both,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CopyCodeBlockCandidate {
    id: String,
    label: String,
    preview: String,
    search_value: String,
    content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CopyMessageCandidate {
    id: String,
    label: String,
    preview: String,
    search_value: String,
    content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CopyCodeUiState {
    scope: CodeBlockScope,
    ui_mode: CopyUiMode,
    multi_select: bool,
    selected_id: Option<String>,
    selected_ids: BTreeSet<String>,
}

impl CopyCodeUiState {
    fn new(scope: CodeBlockScope, ui_mode: CopyUiMode) -> Self {
        Self {
            scope,
            ui_mode,
            multi_select: false,
            selected_id: None,
            selected_ids: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CopyMessageUiState {
    filter: MessageFilter,
    ui_mode: CopyUiMode,
    multi_select: bool,
    selected_id: Option<String>,
    selected_ids: BTreeSet<String>,
}

impl CopyMessageUiState {
    fn new(filter: MessageFilter, ui_mode: CopyUiMode) -> Self {
        Self {
            filter,
            ui_mode,
            multi_select: false,
            selected_id: None,
            selected_ids: BTreeSet::new(),
        }
    }
}

fn copy_ui_mode_label(mode: CopyUiMode) -> &'static str {
    match mode {
        CopyUiMode::Picker => "picker",
        CopyUiMode::Navigator => "navigator",
    }
}

fn selected_index_for_candidates<T>(
    top_rows: usize,
    candidates: &[T],
    selected_id: Option<&str>,
) -> Option<usize>
where
    T: CopyCandidateId,
{
    selected_id.and_then(|id| {
        candidates
            .iter()
            .position(|candidate| candidate.candidate_id() == id)
            .map(|idx| top_rows + idx)
    })
}

trait CopyCandidateId {
    fn candidate_id(&self) -> &str;
}

impl CopyCandidateId for CopyCodeBlockCandidate {
    fn candidate_id(&self) -> &str {
        &self.id
    }
}

impl CopyCandidateId for CopyMessageCandidate {
    fn candidate_id(&self) -> &str {
        &self.id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FencedCodeBlock {
    language: Option<String>,
    content: String,
}

fn first_non_empty_preview(content: &str) -> String {
    let snippet = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    if snippet.is_empty() {
        "(empty)".to_string()
    } else {
        truncate_text(snippet, 80)
    }
}

fn extract_fenced_code_blocks(markdown: &str) -> Vec<FencedCodeBlock> {
    #[derive(Debug)]
    struct OpenFence {
        fence_char: char,
        fence_len: usize,
        language: Option<String>,
        content_lines: Vec<String>,
    }

    fn parse_fence(line: &str) -> Option<(char, usize, &str)> {
        let trimmed = line.trim_start();
        let fence_char = match trimmed.chars().next() {
            Some('`') => '`',
            Some('~') => '~',
            _ => return None,
        };
        let fence_len = trimmed.chars().take_while(|ch| *ch == fence_char).count();
        if fence_len < 3 {
            return None;
        }
        Some((fence_char, fence_len, trimmed[fence_len..].trim()))
    }

    let mut blocks = Vec::new();
    let mut open: Option<OpenFence> = None;

    for line in markdown.lines() {
        if let Some(state) = open.as_mut() {
            let trimmed = line.trim_start();
            let close_len = trimmed
                .chars()
                .take_while(|ch| *ch == state.fence_char)
                .count();
            if close_len >= state.fence_len && trimmed[close_len..].trim().is_empty() {
                if let Some(state) = open.take() {
                    blocks.push(FencedCodeBlock {
                        language: state.language,
                        content: state.content_lines.join("\n"),
                    });
                }
                continue;
            }
            state.content_lines.push(line.to_string());
            continue;
        }

        let Some((fence_char, fence_len, rest)) = parse_fence(line) else {
            continue;
        };
        let language = rest.split_whitespace().next().map(ToString::to_string);
        open = Some(OpenFence {
            fence_char,
            fence_len,
            language,
            content_lines: Vec::new(),
        });
    }

    blocks
}

// Extract the first bold (Markdown) element in the form **...** from `s`.
// Returns the inner text if found; otherwise `None`.
fn extract_first_bold(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b'*' && bytes[i + 1] == b'*' {
            let start = i + 2;
            let mut j = start;
            while j + 1 < bytes.len() {
                if bytes[j] == b'*' && bytes[j + 1] == b'*' {
                    // Found closing **
                    let inner = &s[start..j];
                    let trimmed = inner.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    } else {
                        return None;
                    }
                }
                j += 1;
            }
            // No closing; stop searching (wait for more deltas)
            return None;
        }
        i += 1;
    }
    None
}

#[derive(Debug, Default)]
struct ParsedExportArgs {
    format: Option<ChatExportFormat>,
    overrides: ExportOverrides,
}

#[derive(Debug)]
struct ExportDestination {
    path: PathBuf,
    format: ChatExportFormat,
}

#[derive(Debug)]
enum ExportPathSpec {
    File(PathBuf),
    Dir(PathBuf),
}

fn parse_export_args(args: &str, cwd: &Path) -> Result<ParsedExportArgs, String> {
    let mut parsed = ParsedExportArgs::default();
    if args.trim().is_empty() {
        return Ok(parsed);
    }

    let tokens =
        shlex::split(args).ok_or_else(|| "Could not parse /export arguments.".to_string())?;
    let mut positional: Option<String> = None;
    let mut idx = 0usize;

    while idx < tokens.len() {
        let token = &tokens[idx];
        if token == "--" {
            if idx + 1 >= tokens.len() {
                return Err("Expected a path after --.".to_string());
            }
            if tokens.len() - idx > 2 {
                return Err(format!(
                    "Unexpected /export arguments: {}",
                    tokens[idx + 2..].join(" ")
                ));
            }
            positional = Some(tokens[idx + 1].clone());
            break;
        }

        let next_value =
            |idx: &mut usize, tokens: &[String], flag: &str| -> Result<String, String> {
                *idx += 1;
                if *idx >= tokens.len() {
                    return Err(format!("Expected a value after {flag}."));
                }
                Ok(tokens[*idx].clone())
            };

        match token.as_str() {
            "-f" | "--format" => {
                let value = next_value(&mut idx, &tokens, token)?;
                parsed.format = Some(parse_export_format(&value)?);
            }
            "--json" => {
                parsed.format = Some(ChatExportFormat::Json);
            }
            "--markdown" | "--md" => {
                parsed.format = Some(ChatExportFormat::Markdown);
            }
            "-o" | "--output" => {
                let value = next_value(&mut idx, &tokens, token)?;
                parsed.overrides.output_path = Some(resolve_input_path(cwd, &value));
            }
            "-C" | "--dir" => {
                let value = next_value(&mut idx, &tokens, token)?;
                parsed.overrides.output_dir = Some(resolve_input_path(cwd, &value));
            }
            "--name" => {
                let value = next_value(&mut idx, &tokens, token)?;
                parsed.overrides.name = Some(value);
            }
            _ if token.starts_with('-') => {
                return Err(format!("Unknown /export flag: {token}"));
            }
            _ => {
                if positional.is_some() {
                    return Err("Provide only one export path.".to_string());
                }
                positional = Some(token.clone());
            }
        }

        idx += 1;
    }

    if parsed.overrides.output_path.is_some() && parsed.overrides.output_dir.is_some() {
        return Err("Use either --output or --dir, not both.".to_string());
    }

    if let Some(positional) = positional {
        if parsed.overrides.output_path.is_some() || parsed.overrides.output_dir.is_some() {
            return Err("Provide only one export path (flag or positional).".to_string());
        }
        match classify_export_path(&positional, cwd) {
            ExportPathSpec::File(path) => parsed.overrides.output_path = Some(path),
            ExportPathSpec::Dir(path) => parsed.overrides.output_dir = Some(path),
        }
    }

    Ok(parsed)
}

fn parse_export_format(value: &str) -> Result<ChatExportFormat, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "md" | "markdown" => Ok(ChatExportFormat::Markdown),
        "json" => Ok(ChatExportFormat::Json),
        _ => Err(format!(
            "Unknown export format: {value} (expected md or json)."
        )),
    }
}

fn export_overrides_from_path_input(value: &str, cwd: &Path) -> ExportOverrides {
    match classify_export_path(value, cwd) {
        ExportPathSpec::File(path) => ExportOverrides {
            output_path: Some(path),
            ..Default::default()
        },
        ExportPathSpec::Dir(path) => ExportOverrides {
            output_dir: Some(path),
            ..Default::default()
        },
    }
}

fn classify_export_path(value: &str, cwd: &Path) -> ExportPathSpec {
    let path = resolve_input_path(cwd, value);
    let trailing_separator = value.ends_with(std::path::MAIN_SEPARATOR)
        || (std::path::MAIN_SEPARATOR != '/' && value.ends_with('/'))
        || (std::path::MAIN_SEPARATOR != '\\' && value.ends_with('\\'));
    if trailing_separator || path.is_dir() {
        ExportPathSpec::Dir(path)
    } else {
        ExportPathSpec::File(path)
    }
}

fn resolve_input_path(cwd: &Path, value: &str) -> PathBuf {
    let trimmed = value.trim();
    let path = if let Some(rest) = trimmed.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(rest)
        } else {
            PathBuf::from(trimmed)
        }
    } else {
        PathBuf::from(trimmed)
    };
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn resolve_export_destination(
    rollout_path: &Path,
    format_override: Option<ChatExportFormat>,
    overrides: &ExportOverrides,
) -> Result<ExportDestination, String> {
    let mut format = format_override
        .or_else(|| {
            overrides
                .output_path
                .as_deref()
                .and_then(format_from_extension)
        })
        .unwrap_or(ChatExportFormat::Markdown);

    if let Some(mut path) = overrides.output_path.clone() {
        if path.is_dir() {
            return Err(format!("Export path is a directory: {}", path.display()));
        }
        if path.extension().is_none() {
            path.set_extension(format.extension());
        } else if let Some(from_ext) = format_from_extension(&path) {
            format = from_ext;
        }
        return Ok(ExportDestination { path, format });
    }

    let output_dir = overrides
        .output_dir
        .clone()
        .or_else(|| rollout_path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    if output_dir.is_file() {
        return Err(format!(
            "Export directory is a file: {}",
            output_dir.display()
        ));
    }

    let export_name = if let Some(name) = overrides.name.as_deref() {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err("Export name cannot be empty.".to_string());
        }
        trimmed.to_string()
    } else {
        default_export_name(rollout_path)?
    };

    if export_name.contains(std::path::MAIN_SEPARATOR) || export_name.contains('/') {
        return Err("Export name must not contain path separators.".to_string());
    }

    let mut path = output_dir.join(export_name);
    path.set_extension(format.extension());

    Ok(ExportDestination { path, format })
}

fn default_export_name(rollout_path: &Path) -> Result<String, String> {
    let stem = rollout_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::trim)
        .filter(|stem| !stem.is_empty())
        .ok_or_else(|| "Failed to derive export name from rollout path.".to_string())?;
    Ok(stem.to_string())
}

fn format_from_extension(path: &Path) -> Option<ChatExportFormat> {
    let ext = path.extension()?.to_str()?;
    match ext.to_ascii_lowercase().as_str() {
        "md" | "markdown" => Some(ChatExportFormat::Markdown),
        "json" => Some(ChatExportFormat::Json),
        _ => None,
    }
}

fn diff_view_override_from_args(args: &str, default_view: DiffView) -> Result<DiffView, String> {
    if args.trim().is_empty() {
        return Ok(default_view);
    }

    let Some(tokens) = shlex::split(args) else {
        return Err("Failed to parse /diff arguments.".to_string());
    };

    if tokens.is_empty() {
        return Ok(default_view);
    }

    let mut view_override = None;
    let mut args_iter = tokens.iter();
    while let Some(arg) = args_iter.next() {
        if let Some(value) = arg.strip_prefix("--view=") {
            view_override = Some(parse_diff_view_value(value)?);
            continue;
        }

        match arg.as_str() {
            "--pretty" => view_override = Some(DiffView::Pretty),
            "--line" => view_override = Some(DiffView::Line),
            "--inline" => view_override = Some(DiffView::Inline),
            "--side-by-side" => view_override = Some(DiffView::SideBySide),
            "--view" => {
                let Some(value) = args_iter.next() else {
                    return Err(
                        "Expected a value after --view (pretty, line, inline, or side-by-side)."
                            .to_string(),
                    );
                };
                view_override = Some(parse_diff_view_value(value)?);
            }
            _ if arg.starts_with('-') => {
                return Err(format!("Unknown /diff flag: {arg}"));
            }
            _ => {
                return Err(format!("Unexpected /diff argument: {arg}"));
            }
        }
    }

    Ok(view_override.unwrap_or(default_view))
}

fn parse_diff_view_value(value: &str) -> Result<DiffView, String> {
    match value {
        "pretty" => Ok(DiffView::Pretty),
        "line" => Ok(DiffView::Line),
        "inline" => Ok(DiffView::Inline),
        "side-by-side" | "side_by_side" | "side" => Ok(DiffView::SideBySide),
        _ => Err(format!(
            "Invalid /diff view '{value}'. Use 'pretty', 'line', 'inline', or 'side-by-side'."
        )),
    }
}

fn parse_progress_legend_mode(value: &str) -> Result<ProgressLegendMode, String> {
    match value.trim() {
        "off" => Ok(ProgressLegendMode::Off),
        "auto" => Ok(ProgressLegendMode::Auto),
        "always" => Ok(ProgressLegendMode::Always),
        _ => Err("Invalid /legend-mode value. Use 'off', 'auto', or 'always'.".to_string()),
    }
}

async fn fetch_rate_limits(base_url: String, auth: CodexAuth) -> Option<RateLimitSnapshot> {
    match BackendClient::from_auth(base_url, &auth) {
        Ok(client) => match client.get_rate_limits().await {
            Ok(snapshot) => Some(snapshot),
            Err(err) => {
                debug!(error = ?err, "failed to fetch rate limits from /usage");
                None
            }
        },
        Err(err) => {
            debug!(error = ?err, "failed to construct backend client for rate limits");
            None
        }
    }
}

#[cfg(test)]
pub(crate) fn show_review_commit_picker_with_entries(
    chat: &mut ChatWidget,
    entries: Vec<codex_core::git_info::CommitLogEntry>,
) {
    let mut items: Vec<SelectionItem> = Vec::with_capacity(entries.len());
    for entry in entries {
        let subject = entry.subject.clone();
        let sha = entry.sha.clone();
        let search_val = format!("{subject} {sha}");

        items.push(SelectionItem {
            name: subject.clone(),
            actions: vec![Box::new(move |tx3: &AppEventSender| {
                tx3.send(AppEvent::CodexOp(Op::Review {
                    review_request: ReviewRequest {
                        target: ReviewTarget::Commit {
                            sha: sha.clone(),
                            title: Some(subject.clone()),
                        },
                        user_facing_hint: None,
                    },
                }));
            })],
            dismiss_on_select: true,
            search_value: Some(search_val),
            ..Default::default()
        });
    }

    chat.bottom_pane.show_selection_view(SelectionViewParams {
        title: Some("Select a commit to review".to_string()),
        footer_hint: Some(standard_popup_hint_line()),
        items,
        is_searchable: true,
        search_placeholder: Some("Type to search commits".to_string()),
        ..Default::default()
    });
}

fn format_duration_short(seconds: u64) -> String {
    if seconds < 60 {
        "less than a minute".to_string()
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

#[cfg(test)]
pub(crate) mod tests;
