//! Execution state shared by command lifecycle handlers.

use std::time::Instant;

use super::*;

pub(super) struct RunningCommand {
    pub(super) command: Vec<String>,
    pub(super) parsed_cmd: Vec<ParsedCommand>,
    pub(super) source: ExecCommandSource,
    pub(super) interaction_input: Option<String>,
    pub(super) started_at: Instant,
    pub(super) aggregated_output: String,
}

impl RunningCommand {
    pub(super) fn new(
        command: Vec<String>,
        parsed_cmd: Vec<ParsedCommand>,
        source: ExecCommandSource,
        interaction_input: Option<String>,
    ) -> Self {
        Self {
            command,
            parsed_cmd,
            source,
            interaction_input,
            started_at: Instant::now(),
            aggregated_output: String::new(),
        }
    }

    pub(super) fn append_output(&mut self, chunk: &str) {
        self.aggregated_output.push_str(chunk);
    }
}

pub(super) struct UnifiedExecProcessSummary {
    pub(super) key: String,
    pub(super) call_id: String,
    pub(super) command_display: String,
    pub(super) recent_chunks: Vec<String>,
}

pub(super) struct UnifiedExecWaitState {
    pub(super) command_display: String,
}

impl UnifiedExecWaitState {
    pub(super) fn new(command_display: String) -> Self {
        Self { command_display }
    }

    pub(super) fn is_duplicate(&self, command_display: &str) -> bool {
        self.command_display == command_display
    }
}

#[derive(Clone, Debug)]
pub(super) struct UnifiedExecWaitStreak {
    pub(super) process_id: String,
    pub(super) command_display: Option<String>,
}

impl UnifiedExecWaitStreak {
    pub(super) fn new(process_id: String, command_display: Option<String>) -> Self {
        Self {
            process_id,
            command_display: command_display.filter(|display| !display.is_empty()),
        }
    }

    pub(super) fn update_command_display(&mut self, command_display: Option<String>) {
        if self.command_display.is_some() {
            return;
        }
        self.command_display = command_display.filter(|display| !display.is_empty());
    }
}

pub(super) fn is_unified_exec_source(source: ExecCommandSource) -> bool {
    matches!(
        source,
        ExecCommandSource::UnifiedExecStartup | ExecCommandSource::UnifiedExecInteraction
    )
}

pub(super) fn is_standard_tool_call(parsed_cmd: &[ParsedCommand]) -> bool {
    !parsed_cmd.is_empty()
        && parsed_cmd
            .iter()
            .all(|parsed| !matches!(parsed, ParsedCommand::Unknown { .. }))
}
