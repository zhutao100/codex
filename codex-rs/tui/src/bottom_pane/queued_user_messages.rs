use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::key_hint;
use crate::render::renderable::Renderable;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;

/// Widget that displays a list of user messages queued while a turn is in progress.
pub(crate) struct QueuedUserMessages {
    pub messages: Vec<String>,
    pub pending_steers: Vec<String>,
    pub rejected_steers: Vec<String>,
    pub show_send_next_hint: bool,
}

impl QueuedUserMessages {
    pub(crate) fn new() -> Self {
        Self {
            messages: Vec::new(),
            pending_steers: Vec::new(),
            rejected_steers: Vec::new(),
            show_send_next_hint: false,
        }
    }

    fn as_renderable(&self, width: u16) -> Box<dyn Renderable> {
        if (self.messages.is_empty()
            && self.pending_steers.is_empty()
            && self.rejected_steers.is_empty())
            || width < 4
        {
            return Box::new(());
        }

        let mut lines = vec![];

        self.push_section(
            &mut lines,
            "Messages to be submitted after next tool call",
            &self.pending_steers,
            "  ↪ ",
            width,
        );
        self.push_section(
            &mut lines,
            "Messages to be submitted at end of turn",
            &self.rejected_steers,
            "  ↳ ",
            width,
        );
        let show_queued_label = !self.pending_steers.is_empty() || !self.rejected_steers.is_empty();
        self.push_section(
            &mut lines,
            if show_queued_label {
                "Queued messages"
            } else {
                ""
            },
            &self.messages,
            "  ↳ ",
            width,
        );

        if !self.messages.is_empty() {
            let mut hint = vec![
                "    ".into(),
                key_hint::alt(KeyCode::Up).into(),
                " edit".into(),
            ];

            if self.show_send_next_hint {
                hint.push(" · ".into());
                hint.push(key_hint::ctrl(KeyCode::Char('y')).into());
                hint.push(" send next".into());
            }

            hint.push(" · ".into());
            hint.push(key_hint::ctrl(KeyCode::Char('o')).into());
            hint.push(" queue".into());

            hint.push(" · /queue".into());
            lines.push(Line::from(hint).dim());
        }

        Paragraph::new(lines).into()
    }

    fn push_section(
        &self,
        lines: &mut Vec<Line<'static>>,
        label: &str,
        messages: &[String],
        prefix: &str,
        width: u16,
    ) {
        if messages.is_empty() {
            return;
        }

        if !lines.is_empty() {
            lines.push("".into());
        }
        if !label.is_empty() {
            lines.push(label.to_string().dim().into());
        }

        for message in messages {
            let wrapped = word_wrap_lines(
                message.lines().map(|line| line.dim().italic()),
                RtOptions::new(width as usize)
                    .initial_indent(Line::from(prefix.to_string().dim()))
                    .subsequent_indent(Line::from("    ")),
            );
            let len = wrapped.len();
            for line in wrapped.into_iter().take(3) {
                lines.push(line);
            }
            if len > 3 {
                lines.push(Line::from("    …".dim().italic()));
            }
        }
    }
}

impl Renderable for QueuedUserMessages {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        self.as_renderable(area.width).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.as_renderable(width).desired_height(width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;

    #[test]
    fn desired_height_empty() {
        let queue = QueuedUserMessages::new();
        assert_eq!(queue.desired_height(40), 0);
    }

    #[test]
    fn desired_height_one_message() {
        let mut queue = QueuedUserMessages::new();
        queue.messages.push("Hello, world!".to_string());
        assert_eq!(queue.desired_height(40), 2);
    }

    #[test]
    fn render_one_message() {
        let mut queue = QueuedUserMessages::new();
        queue.messages.push("Hello, world!".to_string());
        let width = 40;
        let height = queue.desired_height(width);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        queue.render(Rect::new(0, 0, width, height), &mut buf);
        assert_snapshot!("render_one_message", format!("{buf:?}"));
    }

    #[test]
    fn render_two_messages() {
        let mut queue = QueuedUserMessages::new();
        queue.messages.push("Hello, world!".to_string());
        queue.messages.push("This is another message".to_string());
        let width = 40;
        let height = queue.desired_height(width);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        queue.render(Rect::new(0, 0, width, height), &mut buf);
        assert_snapshot!("render_two_messages", format!("{buf:?}"));
    }

    #[test]
    fn render_more_than_three_messages() {
        let mut queue = QueuedUserMessages::new();
        queue.messages.push("Hello, world!".to_string());
        queue.messages.push("This is another message".to_string());
        queue.messages.push("This is a third message".to_string());
        queue.messages.push("This is a fourth message".to_string());
        let width = 40;
        let height = queue.desired_height(width);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        queue.render(Rect::new(0, 0, width, height), &mut buf);
        assert_snapshot!("render_more_than_three_messages", format!("{buf:?}"));
    }

    #[test]
    fn render_wrapped_message() {
        let mut queue = QueuedUserMessages::new();
        queue
            .messages
            .push("This is a longer message that should be wrapped".to_string());
        queue.messages.push("This is another message".to_string());
        let width = 40;
        let height = queue.desired_height(width);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        queue.render(Rect::new(0, 0, width, height), &mut buf);
        assert_snapshot!("render_wrapped_message", format!("{buf:?}"));
    }

    #[test]
    fn render_many_line_message() {
        let mut queue = QueuedUserMessages::new();
        queue
            .messages
            .push("This is\na message\nwith many\nlines".to_string());
        let width = 40;
        let height = queue.desired_height(width);
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        queue.render(Rect::new(0, 0, width, height), &mut buf);
        assert_snapshot!("render_many_line_message", format!("{buf:?}"));
    }
}
