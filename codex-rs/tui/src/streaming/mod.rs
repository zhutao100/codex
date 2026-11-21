//! Streaming primitives used by the TUI transcript pipeline.
//!
//! `StreamState` owns newline-gated Markdown collection and a FIFO queue of lines and tables.
//! Higher-level modules build on top of this state:
//! - `controller` adapts queued blocks into `HistoryCell` emission rules for message and plan streams.
//! - `chunking` computes adaptive drain plans from queue pressure.
//! - `commit_tick` binds policy decisions to concrete controller drains.
//!
//! The key invariant is queue ordering. All drains pop from the front, and enqueue records an
//! arrival timestamp so policy code can reason about oldest queued age without peeking into text.

use std::collections::VecDeque;
use std::time::Duration;
use std::time::Instant;

use crate::markdown_render::MarkdownBlock;
use crate::markdown_stream::MarkdownStreamCollector;
pub(crate) mod chunking;
pub(crate) mod commit_tick;
pub(crate) mod controller;

struct QueuedBlock {
    block: MarkdownBlock,
    enqueued_at: Instant,
}

/// Holds in-flight Markdown and queued blocks ready for display.
pub(crate) struct StreamState {
    pub(crate) collector: MarkdownStreamCollector,
    queue: VecDeque<QueuedBlock>,
    pub(crate) has_seen_delta: bool,
}

impl StreamState {
    /// Creates an empty stream state with an optional target wrap width.
    pub(crate) fn new(width: Option<usize>, unwrap_fences: bool) -> Self {
        Self {
            collector: MarkdownStreamCollector::new(width, unwrap_fences),
            queue: VecDeque::new(),
            has_seen_delta: false,
        }
    }
    /// Resets collector and queue state for the next stream lifecycle.
    pub(crate) fn clear(&mut self) {
        self.collector.clear();
        self.queue.clear();
        self.has_seen_delta = false;
    }
    /// Drains one line or table from the front of the queue.
    pub(crate) fn step(&mut self) -> Vec<MarkdownBlock> {
        self.queue
            .pop_front()
            .map(|queued| queued.block)
            .into_iter()
            .collect()
    }
    /// Drains up to `limit` queued blocks from the front of the queue.
    ///
    /// Callers that pass very large values still get bounded behavior because this method clamps to
    /// the currently available queue length.
    pub(crate) fn drain_n(&mut self, limit: usize) -> Vec<MarkdownBlock> {
        let end = limit.min(self.queue.len());
        self.queue.drain(..end).map(|queued| queued.block).collect()
    }
    /// Drains all queued blocks from the front of the queue.
    pub(crate) fn drain_all(&mut self) -> Vec<MarkdownBlock> {
        self.queue.drain(..).map(|queued| queued.block).collect()
    }
    /// Returns whether no blocks are queued for commit.
    pub(crate) fn is_idle(&self) -> bool {
        self.queue.is_empty()
    }

    pub(crate) fn has_preview(&self) -> bool {
        self.is_idle() && !self.collector.preview().is_empty()
    }
    /// Returns the current queue depth.
    pub(crate) fn queued_len(&self) -> usize {
        self.queue.len()
    }
    /// Returns the age of the oldest queued line.
    pub(crate) fn oldest_queued_age(&self, now: Instant) -> Option<Duration> {
        self.queue
            .front()
            .map(|queued| now.saturating_duration_since(queued.enqueued_at))
    }
    /// Appends committed blocks to the queue with a shared enqueue timestamp.
    pub(crate) fn enqueue(&mut self, blocks: Vec<MarkdownBlock>) {
        let now = Instant::now();
        self.queue
            .extend(blocks.into_iter().map(|block| QueuedBlock {
                block,
                enqueued_at: now,
            }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::text::Line;

    #[test]
    fn drain_n_clamps_to_available_lines() {
        let mut state = StreamState::new(None, /*unwrap_fences*/ false);
        state.enqueue(vec![MarkdownBlock::Line("one".into())]);

        let drained: Vec<_> = state
            .drain_n(8)
            .into_iter()
            .flat_map(|block| block.into_lines(None))
            .collect();
        assert_eq!(drained, vec![Line::from("one")]);
        assert!(state.is_idle());
    }
}
