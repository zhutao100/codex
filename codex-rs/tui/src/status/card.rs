use crate::history_cell::CompositeHistoryCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::PlainHistoryCell;
use crate::history_cell::with_border_with_inner_width;
use crate::version::CODEX_CLI_VERSION;
use chrono::DateTime;
use chrono::Local;
use codex_core::WireApi;
use codex_core::config::Config;
use codex_core::protocol::NetworkAccess;
use codex_core::protocol::RuntimeContextSnapshot;
use codex_core::protocol::SandboxPolicy;
use codex_core::protocol::TokenUsage;
use codex_core::protocol::TokenUsageInfo;
use codex_protocol::ThreadId;
use codex_protocol::account::PlanType;
use codex_protocol::openai_models::ReasoningEffort;
use ratatui::prelude::*;
use ratatui::style::Stylize;
use std::collections::BTreeSet;
use std::path::PathBuf;
use url::Url;

use super::account::StatusAccountDisplay;
use super::format::FieldFormatter;
use super::format::line_display_width;
use super::format::push_label;
use super::format::truncate_line_to_width;
use super::helpers::compose_account_display;
use super::helpers::compose_agents_summary;
use super::helpers::compose_model_display;
use super::helpers::format_directory_display;
use super::helpers::format_tokens_compact;
use super::rate_limits::RateLimitSnapshotDisplay;
use super::rate_limits::StatusRateLimitData;
use super::rate_limits::StatusRateLimitRow;
use super::rate_limits::StatusRateLimitValue;
use super::rate_limits::compose_rate_limit_data;
use super::rate_limits::format_status_limit_summary;
use super::rate_limits::render_status_limit_progress_bar;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;
use codex_core::AuthManager;

#[derive(Debug, Clone)]
struct StatusContextWindowData {
    percent_remaining: i64,
    tokens_in_context: i64,
    window: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct StatusTokenUsageData {
    total: i64,
    input: i64,
    output: i64,
    context_window: Option<StatusContextWindowData>,
}

#[derive(Debug)]
struct StatusHistoryCell {
    model_name: String,
    model_details: Vec<String>,
    directory: PathBuf,
    approval: String,
    sandbox: String,
    agents_summary: String,
    collaboration_mode: Option<String>,
    model_provider: Option<String>,
    account: Option<StatusAccountDisplay>,
    status_scope: Option<String>,
    task_kind: Option<String>,
    thread_name: Option<String>,
    session_id: Option<String>,
    forked_from: Option<String>,
    parent_session_id: Option<String>,
    parent_turn_id: Option<String>,
    token_usage: StatusTokenUsageData,
    rate_limits: StatusRateLimitData,
}

#[derive(Debug, Clone)]
pub(crate) struct StatusOutputSnapshot {
    model_name: String,
    directory: PathBuf,
    approval: String,
    sandbox: String,
    agents_summary: String,
    collaboration_mode: Option<String>,
    model_provider: Option<String>,
    status_scope: Option<String>,
    task_kind: Option<String>,
    thread_name: Option<String>,
    session_id: Option<String>,
    forked_from: Option<String>,
    parent_session_id: Option<String>,
    parent_turn_id: Option<String>,
    token_info: Option<TokenUsageInfo>,
    total_usage: TokenUsage,
    context_window: Option<i64>,
    reasoning_effort: Option<ReasoningEffort>,
    reasoning_summaries: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn new_status_output(
    config: &Config,
    auth_manager: &AuthManager,
    token_info: Option<&TokenUsageInfo>,
    total_usage: &TokenUsage,
    session_id: &Option<ThreadId>,
    thread_name: Option<String>,
    forked_from: Option<ThreadId>,
    rate_limits: Option<&RateLimitSnapshotDisplay>,
    plan_type: Option<PlanType>,
    now: DateTime<Local>,
    model_name: &str,
    collaboration_mode: Option<&str>,
    reasoning_effort_override: Option<Option<ReasoningEffort>>,
) -> CompositeHistoryCell {
    let snapshot = StatusOutputSnapshot::from_config(
        config,
        token_info,
        total_usage,
        session_id,
        thread_name,
        forked_from,
        model_name,
        collaboration_mode,
        reasoning_effort_override,
    );
    new_status_output_from_snapshot(snapshot, auth_manager, rate_limits, plan_type, now)
}

pub(crate) fn new_status_output_from_snapshot(
    snapshot: StatusOutputSnapshot,
    auth_manager: &AuthManager,
    rate_limits: Option<&RateLimitSnapshotDisplay>,
    plan_type: Option<PlanType>,
    now: DateTime<Local>,
) -> CompositeHistoryCell {
    let command = PlainHistoryCell::new(vec!["/status".magenta().into()]);
    let card = StatusHistoryCell::new(snapshot, auth_manager, rate_limits, plan_type, now);

    CompositeHistoryCell::new(vec![Box::new(command), Box::new(card)])
}

impl StatusOutputSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_config(
        config: &Config,
        token_info: Option<&TokenUsageInfo>,
        total_usage: &TokenUsage,
        session_id: &Option<ThreadId>,
        thread_name: Option<String>,
        forked_from: Option<ThreadId>,
        model_name: &str,
        collaboration_mode: Option<&str>,
        reasoning_effort_override: Option<Option<ReasoningEffort>>,
    ) -> Self {
        let reasoning_effort = (config.model_provider.wire_api == WireApi::Responses).then(|| {
            reasoning_effort_override
                .unwrap_or(None)
                .or(config.model_reasoning_effort)
                .unwrap_or(ReasoningEffort::None)
        });
        let reasoning_summaries = (config.model_provider.wire_api == WireApi::Responses)
            .then(|| config.model_reasoning_summary.to_string());

        Self {
            model_name: model_name.to_string(),
            directory: config.cwd.clone(),
            approval: config.approval_policy.value().to_string(),
            sandbox: sandbox_status_label(config.sandbox_policy.get()),
            agents_summary: compose_agents_summary(config),
            collaboration_mode: collaboration_mode.map(ToString::to_string),
            model_provider: format_model_provider(config),
            status_scope: None,
            task_kind: None,
            thread_name,
            session_id: session_id.as_ref().map(ToString::to_string),
            forked_from: forked_from.map(|id| id.to_string()),
            parent_session_id: None,
            parent_turn_id: None,
            token_info: token_info.cloned(),
            total_usage: total_usage.clone(),
            context_window: config.model_context_window,
            reasoning_effort,
            reasoning_summaries,
        }
    }

    pub(crate) fn from_runtime_context(snapshot: &RuntimeContextSnapshot) -> Self {
        let token_info = snapshot.token_info.clone();
        let total_usage = token_info
            .as_ref()
            .map(|info| info.total_token_usage.clone())
            .unwrap_or_default();
        let context_window = token_info
            .as_ref()
            .and_then(|info| info.model_context_window)
            .or(snapshot.model_context_window);

        Self {
            model_name: snapshot.model.clone(),
            directory: snapshot.cwd.clone(),
            approval: snapshot.approval_policy.to_string(),
            sandbox: sandbox_status_label(&snapshot.sandbox_policy),
            agents_summary: snapshot
                .agents_summary
                .clone()
                .unwrap_or_else(|| "<none>".to_string()),
            collaboration_mode: None,
            model_provider: Some(snapshot.model_provider_id.clone()),
            status_scope: Some(match snapshot.scope {
                codex_core::protocol::RuntimeContextScope::Primary => "primary".to_string(),
                codex_core::protocol::RuntimeContextScope::Delegate => "delegate".to_string(),
            }),
            task_kind: snapshot.task_kind.clone(),
            thread_name: snapshot.thread_name.clone(),
            session_id: Some(snapshot.session_id.to_string()),
            forked_from: None,
            parent_session_id: snapshot.parent_session_id.map(|id| id.to_string()),
            parent_turn_id: snapshot.parent_turn_id.clone(),
            token_info,
            total_usage,
            context_window,
            reasoning_effort: snapshot.reasoning_effort,
            reasoning_summaries: None,
        }
    }
}

impl StatusHistoryCell {
    fn new(
        snapshot: StatusOutputSnapshot,
        auth_manager: &AuthManager,
        rate_limits: Option<&RateLimitSnapshotDisplay>,
        plan_type: Option<PlanType>,
        now: DateTime<Local>,
    ) -> Self {
        let mut model_entries = Vec::new();
        if let Some(reasoning_effort) = snapshot.reasoning_effort {
            model_entries.push(("reasoning effort", reasoning_effort.to_string()));
        }
        if let Some(reasoning_summaries) = snapshot.reasoning_summaries.as_ref() {
            model_entries.push(("reasoning summaries", reasoning_summaries.clone()));
        }
        let (model_name, model_details) =
            compose_model_display(snapshot.model_name.as_str(), &model_entries);
        let account = compose_account_display(auth_manager, plan_type);
        let default_usage = TokenUsage::default();
        let (context_usage, context_window) = match snapshot.token_info.as_ref() {
            Some(info) => (&info.last_token_usage, info.model_context_window),
            None => (&default_usage, snapshot.context_window),
        };
        let context_window = context_window.map(|window| StatusContextWindowData {
            percent_remaining: context_usage.percent_of_context_window_remaining(window),
            tokens_in_context: context_usage.tokens_in_context_window(),
            window,
        });

        let token_usage = StatusTokenUsageData {
            total: snapshot.total_usage.blended_total(),
            input: snapshot.total_usage.non_cached_input(),
            output: snapshot.total_usage.output_tokens,
            context_window,
        };
        let rate_limits = compose_rate_limit_data(rate_limits, now);

        Self {
            model_name,
            model_details,
            directory: snapshot.directory,
            approval: snapshot.approval,
            sandbox: snapshot.sandbox,
            agents_summary: snapshot.agents_summary,
            collaboration_mode: snapshot.collaboration_mode,
            model_provider: snapshot.model_provider,
            account,
            status_scope: snapshot.status_scope,
            task_kind: snapshot.task_kind,
            thread_name: snapshot.thread_name,
            session_id: snapshot.session_id,
            forked_from: snapshot.forked_from,
            parent_session_id: snapshot.parent_session_id,
            parent_turn_id: snapshot.parent_turn_id,
            token_usage,
            rate_limits,
        }
    }

    fn token_usage_spans(&self) -> Vec<Span<'static>> {
        let total_fmt = format_tokens_compact(self.token_usage.total);
        let input_fmt = format_tokens_compact(self.token_usage.input);
        let output_fmt = format_tokens_compact(self.token_usage.output);

        vec![
            Span::from(total_fmt),
            Span::from(" total "),
            Span::from(" (").dim(),
            Span::from(input_fmt).dim(),
            Span::from(" input").dim(),
            Span::from(" + ").dim(),
            Span::from(output_fmt).dim(),
            Span::from(" output").dim(),
            Span::from(")").dim(),
        ]
    }

    fn context_window_spans(&self) -> Option<Vec<Span<'static>>> {
        let context = self.token_usage.context_window.as_ref()?;
        let percent = context.percent_remaining;
        let used_fmt = format_tokens_compact(context.tokens_in_context);
        let window_fmt = format_tokens_compact(context.window);

        Some(vec![
            Span::from(format!("{percent}% left")),
            Span::from(" (").dim(),
            Span::from(used_fmt).dim(),
            Span::from(" used / ").dim(),
            Span::from(window_fmt).dim(),
            Span::from(")").dim(),
        ])
    }

    fn rate_limit_lines(
        &self,
        available_inner_width: usize,
        formatter: &FieldFormatter,
    ) -> Vec<Line<'static>> {
        match &self.rate_limits {
            StatusRateLimitData::Available(rows_data) => {
                if rows_data.is_empty() {
                    return vec![
                        formatter.line("Limits", vec![Span::from("data not available yet").dim()]),
                    ];
                }

                self.rate_limit_row_lines(rows_data, available_inner_width, formatter)
            }
            StatusRateLimitData::Stale(rows_data) => {
                let mut lines =
                    self.rate_limit_row_lines(rows_data, available_inner_width, formatter);
                lines.push(formatter.line(
                    "Warning",
                    vec![Span::from("limits may be stale - start new turn to refresh.").dim()],
                ));
                lines
            }
            StatusRateLimitData::Missing => {
                vec![formatter.line("Limits", vec![Span::from("data not available yet").dim()])]
            }
        }
    }

    fn rate_limit_row_lines(
        &self,
        rows: &[StatusRateLimitRow],
        available_inner_width: usize,
        formatter: &FieldFormatter,
    ) -> Vec<Line<'static>> {
        let mut lines = Vec::with_capacity(rows.len().saturating_mul(2));

        for row in rows {
            match &row.value {
                StatusRateLimitValue::Window {
                    percent_used,
                    resets_at,
                } => {
                    let percent_remaining = (100.0 - percent_used).clamp(0.0, 100.0);
                    let value_spans = vec![
                        Span::from(render_status_limit_progress_bar(percent_remaining)),
                        Span::from(" "),
                        Span::from(format_status_limit_summary(percent_remaining)),
                    ];
                    let base_spans = formatter.full_spans(row.label.as_str(), value_spans);
                    let base_line = Line::from(base_spans.clone());

                    if let Some(resets_at) = resets_at.as_ref() {
                        let resets_span = Span::from(format!("(resets {resets_at})")).dim();
                        let mut inline_spans = base_spans.clone();
                        inline_spans.push(Span::from(" ").dim());
                        inline_spans.push(resets_span.clone());

                        if line_display_width(&Line::from(inline_spans.clone()))
                            <= available_inner_width
                        {
                            lines.push(Line::from(inline_spans));
                        } else {
                            lines.push(base_line);
                            lines.push(formatter.continuation(vec![resets_span]));
                        }
                    } else {
                        lines.push(base_line);
                    }
                }
                StatusRateLimitValue::Text(text) => {
                    let label = row.label.clone();
                    let spans =
                        formatter.full_spans(label.as_str(), vec![Span::from(text.clone())]);
                    lines.push(Line::from(spans));
                }
            }
        }

        lines
    }

    fn collect_rate_limit_labels(&self, seen: &mut BTreeSet<String>, labels: &mut Vec<String>) {
        match &self.rate_limits {
            StatusRateLimitData::Available(rows) => {
                if rows.is_empty() {
                    push_label(labels, seen, "Limits");
                } else {
                    for row in rows {
                        push_label(labels, seen, row.label.as_str());
                    }
                }
            }
            StatusRateLimitData::Stale(rows) => {
                for row in rows {
                    push_label(labels, seen, row.label.as_str());
                }
                push_label(labels, seen, "Warning");
            }
            StatusRateLimitData::Missing => push_label(labels, seen, "Limits"),
        }
    }
}

impl HistoryCell for StatusHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::from(vec![
            Span::from(format!("{}>_ ", FieldFormatter::INDENT)).dim(),
            Span::from("OpenAI Codex").bold(),
            Span::from(" ").dim(),
            Span::from(format!("(v{CODEX_CLI_VERSION})")).dim(),
        ]));
        lines.push(Line::from(Vec::<Span<'static>>::new()));

        let available_inner_width = usize::from(width.saturating_sub(4));
        if available_inner_width == 0 {
            return Vec::new();
        }

        let account_value = self.account.as_ref().map(|account| match account {
            StatusAccountDisplay::ChatGpt { email, plan } => match (email, plan) {
                (Some(email), Some(plan)) => format!("{email} ({plan})"),
                (Some(email), None) => email.clone(),
                (None, Some(plan)) => plan.clone(),
                (None, None) => "ChatGPT".to_string(),
            },
            StatusAccountDisplay::ApiKey => {
                "API key configured (run codex login to use ChatGPT)".to_string()
            }
        });

        let mut labels: Vec<String> =
            vec!["Model", "Directory", "Approval", "Sandbox", "Agents.md"]
                .into_iter()
                .map(str::to_string)
                .collect();
        let mut seen: BTreeSet<String> = labels.iter().cloned().collect();
        let thread_name = self
            .thread_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or("Untitled");

        if self.model_provider.is_some() {
            push_label(&mut labels, &mut seen, "Model provider");
        }
        if account_value.is_some() {
            push_label(&mut labels, &mut seen, "Account");
        }
        if self.status_scope.is_some() {
            push_label(&mut labels, &mut seen, "Status scope");
        }
        if self.task_kind.is_some() {
            push_label(&mut labels, &mut seen, "Task");
        }
        push_label(&mut labels, &mut seen, "Thread name");
        if self.session_id.is_some() {
            push_label(&mut labels, &mut seen, "Session");
        }
        if self.session_id.is_some() && self.forked_from.is_some() {
            push_label(&mut labels, &mut seen, "Forked from");
        }
        if self.parent_session_id.is_some() {
            push_label(&mut labels, &mut seen, "Parent session");
        }
        if self.parent_turn_id.is_some() {
            push_label(&mut labels, &mut seen, "Parent turn");
        }
        if self.collaboration_mode.is_some() {
            push_label(&mut labels, &mut seen, "Collaboration mode");
        }
        push_label(&mut labels, &mut seen, "Token usage");
        if self.token_usage.context_window.is_some() {
            push_label(&mut labels, &mut seen, "Context window");
        }

        self.collect_rate_limit_labels(&mut seen, &mut labels);

        let formatter = FieldFormatter::from_labels(labels.iter().map(String::as_str));
        let value_width = formatter.value_width(available_inner_width);

        let note_first_line = Line::from(vec![
            Span::from("Visit ").cyan(),
            "https://chatgpt.com/codex/settings/usage"
                .cyan()
                .underlined(),
            Span::from(" for up-to-date").cyan(),
        ]);
        let note_second_line = Line::from(vec![
            Span::from("information on rate limits and credits").cyan(),
        ]);
        let note_lines = word_wrap_lines(
            [note_first_line, note_second_line],
            RtOptions::new(available_inner_width),
        );
        lines.extend(note_lines);
        lines.push(Line::from(Vec::<Span<'static>>::new()));

        let mut model_spans = vec![Span::from(self.model_name.clone())];
        if !self.model_details.is_empty() {
            model_spans.push(Span::from(" (").dim());
            model_spans.push(Span::from(self.model_details.join(", ")).dim());
            model_spans.push(Span::from(")").dim());
        }

        let directory_value = format_directory_display(&self.directory, Some(value_width));

        lines.push(formatter.line("Model", model_spans));
        if let Some(model_provider) = self.model_provider.as_ref() {
            lines.push(formatter.line("Model provider", vec![Span::from(model_provider.clone())]));
        }
        lines.push(formatter.line("Directory", vec![Span::from(directory_value)]));
        lines.push(formatter.line("Approval", vec![Span::from(self.approval.clone())]));
        lines.push(formatter.line("Sandbox", vec![Span::from(self.sandbox.clone())]));
        lines.push(formatter.line("Agents.md", vec![Span::from(self.agents_summary.clone())]));

        if let Some(account_value) = account_value {
            lines.push(formatter.line("Account", vec![Span::from(account_value)]));
        }

        if let Some(scope) = self.status_scope.as_ref() {
            lines.push(formatter.line("Status scope", vec![Span::from(scope.clone())]));
        }
        if let Some(task_kind) = self.task_kind.as_ref() {
            lines.push(formatter.line("Task", vec![Span::from(task_kind.clone())]));
        }
        lines.push(formatter.line("Thread name", vec![Span::from(thread_name.to_string())]));
        if let Some(collab_mode) = self.collaboration_mode.as_ref() {
            lines.push(formatter.line("Collaboration mode", vec![Span::from(collab_mode.clone())]));
        }
        if let Some(session) = self.session_id.as_ref() {
            lines.push(formatter.line("Session", vec![Span::from(session.clone())]));
        }
        if self.session_id.is_some()
            && let Some(forked_from) = self.forked_from.as_ref()
        {
            lines.push(formatter.line("Forked from", vec![Span::from(forked_from.clone())]));
        }
        if let Some(parent_session_id) = self.parent_session_id.as_ref() {
            lines.push(formatter.line(
                "Parent session",
                vec![Span::from(parent_session_id.clone())],
            ));
        }
        if let Some(parent_turn_id) = self.parent_turn_id.as_ref() {
            lines.push(formatter.line("Parent turn", vec![Span::from(parent_turn_id.clone())]));
        }

        lines.push(Line::from(Vec::<Span<'static>>::new()));
        // Hide token usage only for ChatGPT subscribers
        if !matches!(self.account, Some(StatusAccountDisplay::ChatGpt { .. })) {
            lines.push(formatter.line("Token usage", self.token_usage_spans()));
        }

        if let Some(spans) = self.context_window_spans() {
            lines.push(formatter.line("Context window", spans));
        }

        lines.extend(self.rate_limit_lines(available_inner_width, &formatter));

        let content_width = lines.iter().map(line_display_width).max().unwrap_or(0);
        let inner_width = content_width.min(available_inner_width);
        let truncated_lines: Vec<Line<'static>> = lines
            .into_iter()
            .map(|line| truncate_line_to_width(line, inner_width))
            .collect();

        with_border_with_inner_width(truncated_lines, inner_width)
    }
}

fn sandbox_status_label(policy: &SandboxPolicy) -> String {
    match policy {
        SandboxPolicy::DangerFullAccess => "danger-full-access".to_string(),
        SandboxPolicy::ReadOnly { .. } => "read-only".to_string(),
        SandboxPolicy::WorkspaceWrite { .. } => "workspace-write".to_string(),
        SandboxPolicy::ExternalSandbox { network_access } => {
            if matches!(network_access, NetworkAccess::Enabled) {
                "external-sandbox (network access enabled)".to_string()
            } else {
                "external-sandbox".to_string()
            }
        }
    }
}

fn format_model_provider(config: &Config) -> Option<String> {
    let provider = &config.model_provider;
    let name = provider.name.trim();
    let provider_name = if name.is_empty() {
        config.model_provider_id.as_str()
    } else {
        name
    };
    let base_url = provider.base_url.as_deref().and_then(sanitize_base_url);
    let is_default_openai = provider.is_openai() && base_url.is_none();
    if is_default_openai {
        return None;
    }

    Some(match base_url {
        Some(base_url) => format!("{provider_name} - {base_url}"),
        None => provider_name.to_string(),
    })
}

fn sanitize_base_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let Ok(mut url) = Url::parse(trimmed) else {
        return None;
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string().trim_end_matches('/').to_string()).filter(|value| !value.is_empty())
}
