use crate::history_cell::HistoryCell;
use crate::history_cell::ProposedPlanStreamCell;
use crate::history_cell::{self};
use crate::markdown_render::MarkdownBlock;
use ratatui::text::Line;
use std::time::Duration;
use std::time::Instant;

use super::StreamState;

/// Controller that manages newline-gated streaming, header emission, and
/// commit animation across streams.
pub(crate) struct StreamController {
    state: StreamState,
    finishing_after_drain: bool,
    header_emitted: bool,
}

impl StreamController {
    pub(crate) fn new(width: Option<usize>) -> Self {
        Self {
            state: StreamState::new(width, /*unwrap_fences*/ true),
            finishing_after_drain: false,
            header_emitted: false,
        }
    }

    /// Push a delta; if it contains a newline, commit completed lines and start animation.
    pub(crate) fn push(&mut self, delta: &str) -> bool {
        let state = &mut self.state;
        if !delta.is_empty() {
            state.has_seen_delta = true;
        }
        state.collector.push_delta(delta);
        if delta.contains('\n') {
            let newly_completed = state.collector.commit_complete_blocks();
            if !newly_completed.is_empty() {
                state.enqueue(newly_completed);
                return true;
            }
        }
        false
    }

    /// Finalize the active stream. Drain and emit now.
    pub(crate) fn finalize(&mut self) -> Option<Box<dyn HistoryCell>> {
        // Finalize collector first.
        let remaining = {
            let state = &mut self.state;
            state.collector.finalize_and_drain()
        };
        // Collect all output first to avoid emitting headers when there is no content.
        let mut out_lines = Vec::new();
        {
            let state = &mut self.state;
            if !remaining.is_empty() {
                state.enqueue(remaining);
            }
            let step = state.drain_all();
            out_lines.extend(step);
        }

        // Cleanup
        self.state.clear();
        self.finishing_after_drain = false;
        self.emit(out_lines)
    }

    /// Step animation: commit at most one queued line and handle end-of-drain cleanup.
    pub(crate) fn on_commit_tick(&mut self) -> (Option<Box<dyn HistoryCell>>, bool) {
        let step = self.state.step();
        (self.emit(step), self.state.is_idle())
    }

    /// Step animation: commit at most `max_lines` queued lines.
    ///
    /// This is intended for adaptive catch-up drains. Callers should keep `max_lines` bounded; a
    /// very large value can collapse perceived animation into a single jump.
    pub(crate) fn on_commit_tick_batch(
        &mut self,
        max_lines: usize,
    ) -> (Option<Box<dyn HistoryCell>>, bool) {
        let step = self.state.drain_n(max_lines.max(1));
        (self.emit(step), self.state.is_idle())
    }

    /// Returns the current number of queued lines waiting to be displayed.
    pub(crate) fn queued_lines(&self) -> usize {
        self.state.queued_len()
    }

    /// Returns the age of the oldest queued line.
    pub(crate) fn oldest_queued_age(&self, now: Instant) -> Option<Duration> {
        self.state.oldest_queued_age(now)
    }

    pub(crate) fn has_preview(&self) -> bool {
        self.state.has_preview()
    }

    pub(crate) fn preview_is_continuation(&self) -> bool {
        self.header_emitted
    }

    pub(crate) fn preview_lines(&self, width: u16) -> Vec<Line<'static>> {
        history_cell::AgentMessageCell::render_blocks(
            self.state.collector.preview(),
            !self.header_emitted,
            width,
        )
    }

    fn emit(&mut self, blocks: Vec<MarkdownBlock>) -> Option<Box<dyn HistoryCell>> {
        if blocks.is_empty() {
            return None;
        }
        let is_first_line = !self.header_emitted;
        self.header_emitted = true;
        Some(Box::new(history_cell::AgentMessageCell::from_markdown(
            blocks,
            is_first_line,
        )))
    }
}

/// Controller that streams proposed plan markdown into a styled plan block.
pub(crate) struct PlanStreamController {
    state: StreamState,
    header_emitted: bool,
}

impl PlanStreamController {
    pub(crate) fn new(width: Option<usize>) -> Self {
        Self {
            state: StreamState::new(width, /*unwrap_fences*/ true),
            header_emitted: false,
        }
    }

    /// Push a delta; if it contains a newline, commit completed lines and start animation.
    pub(crate) fn push(&mut self, delta: &str) -> bool {
        let state = &mut self.state;
        if !delta.is_empty() {
            state.has_seen_delta = true;
        }
        state.collector.push_delta(delta);
        if delta.contains('\n') {
            let newly_completed = state.collector.commit_complete_blocks();
            if !newly_completed.is_empty() {
                state.enqueue(newly_completed);
                return true;
            }
        }
        false
    }

    /// Finalize the active stream. Drain and emit now.
    pub(crate) fn finalize(&mut self) -> Option<Box<dyn HistoryCell>> {
        let remaining = {
            let state = &mut self.state;
            state.collector.finalize_and_drain()
        };
        let mut out_lines = Vec::new();
        {
            let state = &mut self.state;
            if !remaining.is_empty() {
                state.enqueue(remaining);
            }
            let step = state.drain_all();
            out_lines.extend(step);
        }

        self.state.clear();
        self.emit(out_lines, true)
    }

    /// Step animation: commit at most one queued line and handle end-of-drain cleanup.
    pub(crate) fn on_commit_tick(&mut self) -> (Option<Box<dyn HistoryCell>>, bool) {
        let step = self.state.step();
        (self.emit(step, false), self.state.is_idle())
    }

    /// Step animation: commit at most `max_lines` queued lines.
    ///
    /// This is intended for adaptive catch-up drains. Callers should keep `max_lines` bounded; a
    /// very large value can collapse perceived animation into a single jump.
    pub(crate) fn on_commit_tick_batch(
        &mut self,
        max_lines: usize,
    ) -> (Option<Box<dyn HistoryCell>>, bool) {
        let step = self.state.drain_n(max_lines.max(1));
        (self.emit(step, false), self.state.is_idle())
    }

    /// Returns the current number of queued plan lines waiting to be displayed.
    pub(crate) fn queued_lines(&self) -> usize {
        self.state.queued_len()
    }

    /// Returns the age of the oldest queued plan line.
    pub(crate) fn oldest_queued_age(&self, now: Instant) -> Option<Duration> {
        self.state.oldest_queued_age(now)
    }

    pub(crate) fn has_preview(&self) -> bool {
        self.state.has_preview()
    }

    pub(crate) fn preview_is_continuation(&self) -> bool {
        self.header_emitted
    }

    pub(crate) fn preview_lines(&self, width: u16) -> Vec<Line<'static>> {
        ProposedPlanStreamCell::render_blocks(
            self.state.collector.preview(),
            self.header_emitted,
            /*include_bottom_padding*/ false,
            width,
        )
    }

    fn emit(
        &mut self,
        blocks: Vec<MarkdownBlock>,
        include_bottom_padding: bool,
    ) -> Option<Box<dyn HistoryCell>> {
        if blocks.is_empty() && !include_bottom_padding {
            return None;
        }

        let is_stream_continuation = self.header_emitted;
        self.header_emitted = true;
        Some(Box::new(ProposedPlanStreamCell::new(
            blocks,
            is_stream_continuation,
            include_bottom_padding,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_to_plain_strings(lines: &[ratatui::text::Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect()
    }

    #[tokio::test]
    async fn controller_loose_vs_tight_with_commit_ticks_matches_full() {
        let mut ctrl = StreamController::new(None);
        let mut lines = Vec::new();

        // Exact deltas from the session log (section: Loose vs. tight list items)
        let deltas = vec![
            "\n\n",
            "Loose",
            " vs",
            ".",
            " tight",
            " list",
            " items",
            ":\n",
            "1",
            ".",
            " Tight",
            " item",
            "\n",
            "2",
            ".",
            " Another",
            " tight",
            " item",
            "\n\n",
            "1",
            ".",
            " Loose",
            " item",
            " with",
            " its",
            " own",
            " paragraph",
            ".\n\n",
            "  ",
            " This",
            " paragraph",
            " belongs",
            " to",
            " the",
            " same",
            " list",
            " item",
            ".\n\n",
            "2",
            ".",
            " Second",
            " loose",
            " item",
            " with",
            " a",
            " nested",
            " list",
            " after",
            " a",
            " blank",
            " line",
            ".\n\n",
            "  ",
            " -",
            " Nested",
            " bullet",
            " under",
            " a",
            " loose",
            " item",
            "\n",
            "  ",
            " -",
            " Another",
            " nested",
            " bullet",
            "\n\n",
        ];

        // Simulate streaming with a commit tick attempt after each delta.
        for d in deltas.iter() {
            ctrl.push(d);
            while let (Some(cell), idle) = ctrl.on_commit_tick() {
                lines.extend(cell.transcript_lines(u16::MAX));
                if idle {
                    break;
                }
            }
        }
        // Finalize and flush remaining lines now.
        if let Some(cell) = ctrl.finalize() {
            lines.extend(cell.transcript_lines(u16::MAX));
        }

        let streamed: Vec<_> = lines_to_plain_strings(&lines)
            .into_iter()
            // skip • and 2-space indentation
            .map(|s| s.chars().skip(2).collect::<String>())
            .collect();

        // Full render of the same source
        let source: String = deltas.iter().copied().collect();
        let mut rendered: Vec<ratatui::text::Line<'static>> = Vec::new();
        crate::markdown::append_markdown(&source, None, &mut rendered);
        let rendered_strs = lines_to_plain_strings(&rendered);

        assert_eq!(streamed, rendered_strs);

        // Also assert exact expected plain strings for clarity.
        let expected = vec![
            "Loose vs. tight list items:".to_string(),
            "".to_string(),
            "1. Tight item".to_string(),
            "2. Another tight item".to_string(),
            "3. Loose item with its own paragraph.".to_string(),
            "".to_string(),
            "   This paragraph belongs to the same list item.".to_string(),
            "4. Second loose item with a nested list after a blank line.".to_string(),
            "    - Nested bullet under a loose item".to_string(),
            "    - Another nested bullet".to_string(),
        ];
        assert_eq!(
            streamed, expected,
            "expected exact rendered lines for loose/tight section"
        );
    }
}
