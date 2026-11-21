//! Renderable implementations for ChatWidget.

use super::*;
use crate::render::renderable::HistoryParagraph;

struct BorrowedHistoryCellRenderable<'a> {
    cell: &'a dyn HistoryCell,
}

pub(super) enum StreamPreview<'a> {
    Answer(&'a StreamController),
    Plan(&'a PlanStreamController),
}

impl StreamPreview<'_> {
    pub(super) fn lines(&self, width: u16) -> Vec<Line<'static>> {
        match self {
            Self::Answer(stream) => stream.preview_lines(width),
            Self::Plan(stream) => stream.preview_lines(width),
        }
    }

    pub(super) fn is_continuation(&self) -> bool {
        match self {
            Self::Answer(stream) => stream.preview_is_continuation(),
            Self::Plan(stream) => stream.preview_is_continuation(),
        }
    }
}

impl Renderable for StreamPreview<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let lines = self.lines(area.width);
        let overflow = lines.len().saturating_sub(usize::from(area.height));
        HistoryParagraph::new(
            lines,
            Style::default(),
            u16::try_from(overflow).unwrap_or(u16::MAX),
        )
        .render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        u16::try_from(self.lines(width).len()).unwrap_or(u16::MAX)
    }
}

impl Renderable for BorrowedHistoryCellRenderable<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.cell.render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        HistoryCell::desired_height(self.cell, width)
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

impl ChatWidget {
    pub(super) fn as_renderable(&self) -> RenderableItem<'_> {
        let active_cell_renderable = match &self.active_cell {
            Some(cell) => RenderableItem::Borrowed(cell).inset(Insets::tlbr(1, 0, 0, 0)),
            None => RenderableItem::Owned(Box::new(())),
        };
        let mut flex = FlexRenderable::new();
        flex.push(1, active_cell_renderable);
        for preview in self.stream_previews() {
            flex.push(1, RenderableItem::Owned(Box::new(preview)));
        }
        if let Some(cell) = self.pending_token_activity_output() {
            flex.push(
                1,
                RenderableItem::Owned(Box::new(BorrowedHistoryCellRenderable { cell }))
                    .inset(Insets::tlbr(1, 0, 0, 0)),
            );
        }
        flex.push(
            0,
            RenderableItem::Borrowed(&self.bottom_pane).inset(Insets::tlbr(1, 0, 0, 0)),
        );
        RenderableItem::Owned(Box::new(flex))
    }
}
