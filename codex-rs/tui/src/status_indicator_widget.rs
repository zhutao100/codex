//! A live status indicator that shows the *latest* log line emitted by the
//! application while the agent is processing a long‑running task.

use std::time::Duration;
use std::time::Instant;

use codex_core::config::types::ProgressLegendMode;
use codex_core::protocol::Op;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::ProgressTraceCategory;
use codex_protocol::protocol::ProgressTraceState;
use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use ratatui::widgets::WidgetRef;
use unicode_width::UnicodeWidthStr;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::exec_cell::spinner;
use crate::key_hint;
use crate::progress_trace_style::ProgressTraceStyles;
use crate::progress_trace_style::progress_trace_category_label;
use crate::progress_trace_style::progress_trace_span;
use crate::render::renderable::Renderable;
use crate::shimmer::shimmer_spans;
use crate::text_formatting::capitalize_first;
use crate::tui::FrameRequester;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;

const DETAILS_MAX_LINES: usize = 3;
const DETAILS_PREFIX: &str = "  └ ";

pub(crate) struct StatusIndicatorWidget {
    /// Animated header text (defaults to "Working").
    header: String,
    details: Option<String>,
    show_interrupt_hint: bool,
    active_model: Option<String>,
    active_reasoning_effort: Option<ReasoningEffort>,
    progress_trace: Vec<ProgressTraceCategory>,
    legend_mode: ProgressLegendMode,
    progress_trace_styles: ProgressTraceStyles,
    task_running: bool,

    elapsed_running: Duration,
    last_resume_at: Instant,
    is_paused: bool,
    app_event_tx: AppEventSender,
    frame_requester: FrameRequester,
    animations_enabled: bool,
}

// Format elapsed seconds into a compact human-friendly form used by the status line.
// Examples: 0s, 59s, 1m 00s, 59m 59s, 1h 00m 00s, 2h 03m 09s
pub fn fmt_elapsed_compact(elapsed_secs: u64) -> String {
    if elapsed_secs < 60 {
        return format!("{elapsed_secs}s");
    }
    if elapsed_secs < 3600 {
        let minutes = elapsed_secs / 60;
        let seconds = elapsed_secs % 60;
        return format!("{minutes}m {seconds:02}s");
    }
    let hours = elapsed_secs / 3600;
    let minutes = (elapsed_secs % 3600) / 60;
    let seconds = elapsed_secs % 60;
    format!("{hours}h {minutes:02}m {seconds:02}s")
}

impl StatusIndicatorWidget {
    pub(crate) fn new(
        app_event_tx: AppEventSender,
        frame_requester: FrameRequester,
        animations_enabled: bool,
        legend_mode: ProgressLegendMode,
        progress_trace_styles: ProgressTraceStyles,
    ) -> Self {
        Self {
            header: String::from("Working"),
            details: None,
            show_interrupt_hint: true,
            active_model: None,
            active_reasoning_effort: None,
            progress_trace: Vec::new(),
            legend_mode,
            progress_trace_styles,
            task_running: false,
            elapsed_running: Duration::ZERO,
            last_resume_at: Instant::now(),
            is_paused: false,

            app_event_tx,
            frame_requester,
            animations_enabled,
        }
    }

    pub(crate) fn interrupt(&self) {
        self.app_event_tx.send(AppEvent::CodexOp(Op::Interrupt));
    }

    /// Update the animated header label (left of the brackets).
    pub(crate) fn update_header(&mut self, header: String) {
        self.header = header;
    }

    /// Update the details text shown below the header.
    pub(crate) fn update_details(&mut self, details: Option<String>) {
        self.details = details
            .filter(|details| !details.is_empty())
            .map(|details| capitalize_first(details.trim_start()));
    }

    #[cfg(test)]
    pub(crate) fn header(&self) -> &str {
        &self.header
    }

    #[cfg(test)]
    pub(crate) fn details(&self) -> Option<&str> {
        self.details.as_deref()
    }

    pub(crate) fn set_interrupt_hint_visible(&mut self, visible: bool) {
        self.show_interrupt_hint = visible;
    }

    pub(crate) fn set_active_model(&mut self, model: Option<String>) {
        self.active_model = model;
    }

    pub(crate) fn set_active_reasoning_effort(&mut self, effort: Option<ReasoningEffort>) {
        self.active_reasoning_effort = effort;
    }

    pub(crate) fn set_legend_mode(&mut self, mode: ProgressLegendMode) {
        self.legend_mode = mode;
    }

    pub(crate) fn set_task_running(&mut self, running: bool) {
        self.task_running = running;
    }

    pub(crate) fn record_progress_trace(
        &mut self,
        category: ProgressTraceCategory,
        state: ProgressTraceState,
        label: Option<String>,
    ) {
        if let ProgressTraceState::Started = state {
            self.progress_trace.push(category);
            const MAX_TRACE_SEGMENTS: usize = 96;
            if self.progress_trace.len() > MAX_TRACE_SEGMENTS {
                let remove_count = self.progress_trace.len() - MAX_TRACE_SEGMENTS;
                self.progress_trace.drain(0..remove_count);
            }
        }
        if label.is_some() {
            self.update_details(label);
        }
    }

    pub(crate) fn clear_progress_trace(&mut self) {
        self.progress_trace.clear();
    }

    #[cfg(test)]
    pub(crate) fn interrupt_hint_visible(&self) -> bool {
        self.show_interrupt_hint
    }

    pub(crate) fn pause_timer(&mut self) {
        self.pause_timer_at(Instant::now());
    }

    pub(crate) fn resume_timer(&mut self) {
        self.resume_timer_at(Instant::now());
    }

    pub(crate) fn pause_timer_at(&mut self, now: Instant) {
        if self.is_paused {
            return;
        }
        self.elapsed_running += now.saturating_duration_since(self.last_resume_at);
        self.is_paused = true;
    }

    pub(crate) fn resume_timer_at(&mut self, now: Instant) {
        if !self.is_paused {
            return;
        }
        self.last_resume_at = now;
        self.is_paused = false;
        self.frame_requester.schedule_frame();
    }

    fn elapsed_duration_at(&self, now: Instant) -> Duration {
        let mut elapsed = self.elapsed_running;
        if !self.is_paused {
            elapsed += now.saturating_duration_since(self.last_resume_at);
        }
        elapsed
    }

    fn elapsed_seconds_at(&self, now: Instant) -> u64 {
        self.elapsed_duration_at(now).as_secs()
    }

    pub fn elapsed_seconds(&self) -> u64 {
        self.elapsed_seconds_at(Instant::now())
    }

    /// Wrap the details text into a fixed width and return the lines, truncating if necessary.
    fn wrapped_details_lines(&self, width: u16) -> Vec<Line<'static>> {
        let Some(details) = self.details.as_deref() else {
            return Vec::new();
        };
        if width == 0 {
            return Vec::new();
        }

        let prefix_width = UnicodeWidthStr::width(DETAILS_PREFIX);
        let opts = RtOptions::new(usize::from(width))
            .initial_indent(Line::from(DETAILS_PREFIX.dim()))
            .subsequent_indent(Line::from(Span::from(" ".repeat(prefix_width)).dim()))
            .break_words(true);

        let mut out = word_wrap_lines(details.lines().map(|line| vec![line.dim()]), opts);

        if out.len() > DETAILS_MAX_LINES {
            out.truncate(DETAILS_MAX_LINES);
            let content_width = usize::from(width).saturating_sub(prefix_width).max(1);
            let max_base_len = content_width.saturating_sub(1);
            if let Some(last) = out.last_mut()
                && let Some(span) = last.spans.last_mut()
            {
                let trimmed: String = span.content.as_ref().chars().take(max_base_len).collect();
                *span = format!("{trimmed}…").dim();
            }
        }

        out
    }
}

impl Renderable for StatusIndicatorWidget {
    fn desired_height(&self, width: u16) -> u16 {
        1 + u16::try_from(self.wrapped_details_lines(width).len()).unwrap_or(0)
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        // Schedule next animation frame.
        self.frame_requester
            .schedule_frame_in(Duration::from_millis(32));
        let now = Instant::now();
        let elapsed_duration = self.elapsed_duration_at(now);
        let pretty_elapsed = fmt_elapsed_compact(elapsed_duration.as_secs());

        let mut spans = Vec::with_capacity(9);
        spans.push(spinner(Some(self.last_resume_at), self.animations_enabled));
        spans.push(" ".into());
        if self.progress_trace.is_empty() {
            if self.animations_enabled {
                spans.extend(shimmer_spans(&self.header));
            } else if !self.header.is_empty() {
                spans.push(self.header.clone().into());
            }
        } else {
            spans.push("[".dim());
            let start = self.progress_trace.len().saturating_sub(20);
            for category in &self.progress_trace[start..] {
                spans.push(progress_trace_span(*category, &self.progress_trace_styles));
            }
            spans.push("]".dim());

            if self.should_show_legend() {
                spans.push(" ".into());
                spans.push("(".dim());
                for category in [
                    ProgressTraceCategory::Tool,
                    ProgressTraceCategory::Edit,
                    ProgressTraceCategory::Waiting,
                    ProgressTraceCategory::Network,
                    ProgressTraceCategory::Prefill,
                    ProgressTraceCategory::Reasoning,
                    ProgressTraceCategory::Gen,
                ] {
                    spans.push(progress_trace_span(category, &self.progress_trace_styles));
                    spans.push(format!(" {}", progress_trace_category_label(category)).dim());
                    spans.push(" ".dim());
                }
                spans.push(")".dim());
            }
        }

        if let Some(model) = self.active_model.as_deref()
            && !model.trim().is_empty()
        {
            spans.push(" · ".dim());
            spans.push(model.to_string().into());
            if let Some(label) =
                thinking_label_for(model, self.active_reasoning_effort).or_else(|| {
                    (!model.starts_with("codex-auto-") && self.active_reasoning_effort.is_none())
                        .then_some("default")
                })
            {
                spans.push(format!(" (reasoning {label})").dim());
            }
            spans.push(" · ".dim());
        } else {
            spans.push(" ".into());
        }

        if self.show_interrupt_hint {
            spans.extend(vec![
                format!("({pretty_elapsed} • ").dim(),
                key_hint::plain(KeyCode::Esc).into(),
                " to interrupt)".dim(),
            ]);
        } else {
            spans.push(format!("({pretty_elapsed})").dim());
        }

        let mut lines = Vec::new();
        lines.push(Line::from(spans));
        if area.height > 1 {
            // If there is enough space, add the details lines below the header.
            let details = self.wrapped_details_lines(area.width);
            let max_details = usize::from(area.height.saturating_sub(1));
            lines.extend(details.into_iter().take(max_details));
        }

        Paragraph::new(Text::from(lines)).render_ref(area, buf);
    }
}

impl StatusIndicatorWidget {
    fn should_show_legend(&self) -> bool {
        match self.legend_mode {
            ProgressLegendMode::Off => false,
            ProgressLegendMode::Auto => self.task_running,
            ProgressLegendMode::Always => true,
        }
    }
}

fn thinking_label_for(model: &str, effort: Option<ReasoningEffort>) -> Option<&'static str> {
    if model.starts_with("codex-auto-") {
        return None;
    }

    effort.map(thinking_label)
}

fn thinking_label(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::XHigh => "xhigh",
        ReasoningEffort::None => "none",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_event::AppEvent;
    use crate::app_event_sender::AppEventSender;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;
    use std::time::Instant;
    use tokio::sync::mpsc::unbounded_channel;

    use pretty_assertions::assert_eq;

    #[test]
    fn fmt_elapsed_compact_formats_seconds_minutes_hours() {
        assert_eq!(fmt_elapsed_compact(0), "0s");
        assert_eq!(fmt_elapsed_compact(1), "1s");
        assert_eq!(fmt_elapsed_compact(59), "59s");
        assert_eq!(fmt_elapsed_compact(60), "1m 00s");
        assert_eq!(fmt_elapsed_compact(61), "1m 01s");
        assert_eq!(fmt_elapsed_compact(3 * 60 + 5), "3m 05s");
        assert_eq!(fmt_elapsed_compact(59 * 60 + 59), "59m 59s");
        assert_eq!(fmt_elapsed_compact(3600), "1h 00m 00s");
        assert_eq!(fmt_elapsed_compact(3600 + 60 + 1), "1h 01m 01s");
        assert_eq!(fmt_elapsed_compact(25 * 3600 + 2 * 60 + 3), "25h 02m 03s");
    }

    #[test]
    fn renders_with_working_header() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);
        let w = StatusIndicatorWidget::new(
            tx,
            crate::tui::FrameRequester::test_dummy(),
            true,
            ProgressLegendMode::Off,
            ProgressTraceStyles::default(),
        );

        // Render into a fixed-size test terminal and snapshot the backend.
        let mut terminal = Terminal::new(TestBackend::new(80, 2)).expect("terminal");
        terminal
            .draw(|f| w.render(f.area(), f.buffer_mut()))
            .expect("draw");
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn renders_truncated() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);
        let w = StatusIndicatorWidget::new(
            tx,
            crate::tui::FrameRequester::test_dummy(),
            true,
            ProgressLegendMode::Off,
            ProgressTraceStyles::default(),
        );

        // Render into a fixed-size test terminal and snapshot the backend.
        let mut terminal = Terminal::new(TestBackend::new(20, 2)).expect("terminal");
        terminal
            .draw(|f| w.render(f.area(), f.buffer_mut()))
            .expect("draw");
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn renders_wrapped_details_panama_two_lines() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);
        let mut w = StatusIndicatorWidget::new(
            tx,
            crate::tui::FrameRequester::test_dummy(),
            false,
            ProgressLegendMode::Off,
            ProgressTraceStyles::default(),
        );
        w.update_details(Some("A man a plan a canal panama".to_string()));
        w.set_interrupt_hint_visible(false);

        // Freeze time-dependent rendering (elapsed + spinner) to keep the snapshot stable.
        w.is_paused = true;
        w.elapsed_running = Duration::ZERO;

        // Prefix is 4 columns, so a width of 30 yields a content width of 26: one column
        // short of fitting the whole phrase (27 cols), forcing exactly one wrap without ellipsis.
        let mut terminal = Terminal::new(TestBackend::new(30, 3)).expect("terminal");
        terminal
            .draw(|f| w.render(f.area(), f.buffer_mut()))
            .expect("draw");
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn timer_pauses_when_requested() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);
        let mut widget = StatusIndicatorWidget::new(
            tx,
            crate::tui::FrameRequester::test_dummy(),
            true,
            ProgressLegendMode::Off,
            ProgressTraceStyles::default(),
        );

        let baseline = Instant::now();
        widget.last_resume_at = baseline;

        let before_pause = widget.elapsed_seconds_at(baseline + Duration::from_secs(5));
        assert_eq!(before_pause, 5);

        widget.pause_timer_at(baseline + Duration::from_secs(5));
        let paused_elapsed = widget.elapsed_seconds_at(baseline + Duration::from_secs(10));
        assert_eq!(paused_elapsed, before_pause);

        widget.resume_timer_at(baseline + Duration::from_secs(10));
        let after_resume = widget.elapsed_seconds_at(baseline + Duration::from_secs(13));
        assert_eq!(after_resume, before_pause + 3);
    }

    #[test]
    fn thinking_label_hidden_for_auto_models() {
        assert_eq!(
            thinking_label_for("codex-auto-fast", Some(ReasoningEffort::High)),
            None
        );
    }

    #[test]
    fn thinking_label_used_for_non_auto_models() {
        assert_eq!(
            thinking_label_for("gpt-5.2-codex", Some(ReasoningEffort::Medium)),
            Some("medium")
        );
        assert_eq!(thinking_label_for("gpt-5.2-codex", None), None);
    }

    #[test]
    fn details_overflow_adds_ellipsis() {
        let (tx_raw, _rx) = unbounded_channel::<AppEvent>();
        let tx = AppEventSender::new(tx_raw);
        let mut w = StatusIndicatorWidget::new(
            tx,
            crate::tui::FrameRequester::test_dummy(),
            true,
            ProgressLegendMode::Off,
            ProgressTraceStyles::default(),
        );
        w.update_details(Some("abcd abcd abcd abcd".to_string()));

        let lines = w.wrapped_details_lines(6);
        assert_eq!(lines.len(), DETAILS_MAX_LINES);
        let last = lines.last().expect("expected last details line");
        assert!(
            last.spans[1].content.as_ref().ends_with("…"),
            "expected ellipsis in last line: {last:?}"
        );
    }
}
