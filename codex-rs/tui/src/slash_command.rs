use strum::IntoEnumIterator;
use strum_macros::AsRefStr;
use strum_macros::EnumIter;
use strum_macros::EnumString;
use strum_macros::IntoStaticStr;

/// Commands that can be invoked by starting a message with a leading slash.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, EnumIter, AsRefStr, IntoStaticStr,
)]
#[strum(serialize_all = "kebab-case")]
pub enum SlashCommand {
    // DO NOT ALPHA-SORT! Enum order is presentation order in the popup, so
    // more frequently used commands should be listed first.
    Model,
    Approvals,
    Permissions,
    #[strum(serialize = "setup-elevated-sandbox")]
    ElevateSandbox,
    Experimental,
    Skills,
    Review,
    Rename,
    Export,
    New,
    Resume,
    #[strum(serialize = "sessions", serialize = "session")]
    Session,
    Archived,
    Fork,
    Init,
    Compact,
    Plan,
    Collab,
    Agent,
    // Undo,
    Diff,
    Copy,
    Mention,
    #[strum(serialize = "copy-code")]
    CopyCodeBlock,
    #[strum(serialize = "copy-messages")]
    CopyMessage,
    Status,
    DebugConfig,
    Statusline,
    Legend,
    LegendMode,
    Mcp,
    Apps,
    Queue,
    Logout,
    Quit,
    Exit,
    Feedback,
    Rollout,
    Ps,
    Personality,
    TestApproval,
}

impl SlashCommand {
    /// User-visible description shown in the popup.
    pub fn description(self) -> &'static str {
        match self {
            SlashCommand::Feedback => "send logs to maintainers",
            SlashCommand::New => "start a new chat during a conversation",
            SlashCommand::Init => "create an AGENTS.md file with instructions for Codex",
            SlashCommand::Compact => "summarize conversation to prevent hitting the context limit",
            SlashCommand::Review => "review my current changes and find issues",
            SlashCommand::Rename => "rename the current thread",
            SlashCommand::Export => "export this chat",
            SlashCommand::Resume => "resume a saved chat",
            SlashCommand::Session => "manage saved chats",
            SlashCommand::Archived => "view archived chats",
            SlashCommand::Fork => "fork the current chat",
            // SlashCommand::Undo => "ask Codex to undo a turn",
            SlashCommand::Quit | SlashCommand::Exit => "exit Codex",
            SlashCommand::Diff => "show git diff (including untracked files)",
            SlashCommand::Copy => "copy the latest Codex output to your clipboard",
            SlashCommand::Mention => "mention a file",
            SlashCommand::CopyCodeBlock => {
                "choose and copy a code block from the last assistant output"
            }
            SlashCommand::CopyMessage => "copy a previous message from this chat",
            SlashCommand::Skills => "use skills to improve how Codex performs specific tasks",
            SlashCommand::Status => "show current session configuration and token usage",
            SlashCommand::DebugConfig => "show config layers and requirement sources for debugging",
            SlashCommand::Statusline => "configure which items appear in the status line",
            SlashCommand::Legend => "show progress timeline legend",
            SlashCommand::LegendMode => "set progress timeline legend mode",
            SlashCommand::Ps => "list background terminals",
            SlashCommand::Model => "choose what model and reasoning effort to use",
            SlashCommand::Personality => "choose a communication style for Codex",
            SlashCommand::Plan => "switch to Plan mode",
            SlashCommand::Collab => "change collaboration mode (experimental)",
            SlashCommand::Agent => "switch the active agent thread",
            SlashCommand::Approvals => "choose what Codex can do without approval",
            SlashCommand::Permissions => "choose what Codex is allowed to do",
            SlashCommand::ElevateSandbox => "set up elevated agent sandbox",
            SlashCommand::Experimental => "toggle experimental features",
            SlashCommand::Mcp => "list configured MCP tools",
            SlashCommand::Apps => "manage apps",
            SlashCommand::Queue => "view and edit queued messages",
            SlashCommand::Logout => "log out of Codex",
            SlashCommand::Rollout => "print the rollout file path",
            SlashCommand::TestApproval => "test approval request",
        }
    }

    /// Command string without the leading '/'. Provided for compatibility with
    /// existing code that expects a method named `command()`.
    pub fn command(self) -> &'static str {
        self.into()
    }

    /// Whether this command supports inline args (for example `/review ...`).
    pub fn supports_inline_args(self) -> bool {
        matches!(
            self,
            SlashCommand::Review
                | SlashCommand::Rename
                | SlashCommand::Plan
                | SlashCommand::Export
                | SlashCommand::Diff
                | SlashCommand::LegendMode
        )
    }

    /// Whether this command can be run while a task is in progress.
    pub fn available_during_task(self) -> bool {
        match self {
            SlashCommand::New
            | SlashCommand::Resume
            | SlashCommand::Session
            | SlashCommand::Archived
            | SlashCommand::Fork
            | SlashCommand::Export
            | SlashCommand::Init
            | SlashCommand::Compact
            // | SlashCommand::Undo
            | SlashCommand::Personality
            | SlashCommand::Approvals
            | SlashCommand::Permissions
            | SlashCommand::ElevateSandbox
            | SlashCommand::Experimental
            | SlashCommand::Review
            | SlashCommand::Plan
            | SlashCommand::Logout => false,
            SlashCommand::Diff
            | SlashCommand::Copy
            | SlashCommand::Rename
            | SlashCommand::Mention
            | SlashCommand::CopyCodeBlock
            | SlashCommand::CopyMessage
            | SlashCommand::Skills
            | SlashCommand::Status
            | SlashCommand::DebugConfig
            | SlashCommand::Legend
            | SlashCommand::LegendMode
            | SlashCommand::Ps
            | SlashCommand::Mcp
            | SlashCommand::Apps
            | SlashCommand::Queue
            | SlashCommand::Feedback
            | SlashCommand::Model
            | SlashCommand::Quit
            | SlashCommand::Exit => true,
            SlashCommand::Rollout => true,
            SlashCommand::TestApproval => true,
            SlashCommand::Collab => true,
            SlashCommand::Agent => true,
            SlashCommand::Statusline => false,
        }
    }

    fn is_visible(self) -> bool {
        match self {
            SlashCommand::Rollout | SlashCommand::TestApproval => cfg!(debug_assertions),
            SlashCommand::Copy | SlashCommand::CopyCodeBlock | SlashCommand::CopyMessage => {
                !cfg!(target_os = "android")
            }
            _ => true,
        }
    }
}

/// Return all built-in commands in a Vec paired with their command string.
pub fn built_in_slash_commands() -> Vec<(&'static str, SlashCommand)> {
    SlashCommand::iter()
        .filter(|command| command.is_visible())
        .map(|c| (c.command(), c))
        .collect()
}
