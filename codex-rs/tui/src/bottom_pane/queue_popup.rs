use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Block;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::render::Insets;
use crate::render::RectExt as _;
use crate::render::renderable::Renderable;
use crate::style::user_message_style;

use super::CancellationEvent;
use super::bottom_pane_view::BottomPaneView;
use super::popup_consts::MAX_POPUP_ROWS;
use super::scroll_state::ScrollState;
use super::selection_popup_common::GenericDisplayRow;
use super::selection_popup_common::measure_rows_height;
use super::selection_popup_common::render_rows;

#[derive(Clone)]
pub(crate) struct QueuePopupItem {
    pub(crate) id: u64,
    pub(crate) preview: String,
    pub(crate) meta: Option<String>,
}

pub(crate) struct QueuePopup {
    items: Vec<QueuePopupItem>,
    state: ScrollState,
    complete: bool,
    delete_confirm_id: Option<u64>,
    app_event_tx: AppEventSender,
}

impl QueuePopup {
    pub(crate) fn new(items: Vec<QueuePopupItem>, app_event_tx: AppEventSender) -> Self {
        let mut state = ScrollState::new();
        state.selected_idx = (!items.is_empty()).then_some(0);
        Self {
            items,
            state,
            complete: false,
            delete_confirm_id: None,
            app_event_tx,
        }
    }

    fn selected_item(&self) -> Option<&QueuePopupItem> {
        self.state.selected_idx.and_then(|idx| self.items.get(idx))
    }

    fn move_selection_up(&mut self) {
        let len = self.items.len();
        self.state.move_up_wrap(len);
        self.state.ensure_visible(len, Self::max_visible_rows(len));
        self.delete_confirm_id = None;
    }

    fn move_selection_down(&mut self) {
        let len = self.items.len();
        self.state.move_down_wrap(len);
        self.state.ensure_visible(len, Self::max_visible_rows(len));
        self.delete_confirm_id = None;
    }

    fn max_visible_rows(len: usize) -> usize {
        MAX_POPUP_ROWS.min(len.max(1))
    }

    fn clamp_selection(&mut self) {
        let len = self.items.len();
        self.state.clamp_selection(len);
        self.state.ensure_visible(len, Self::max_visible_rows(len));
        if len == 0 {
            self.delete_confirm_id = None;
        }
    }

    fn reorder_selected(&mut self, direction: isize) {
        let Some(selected_idx) = self.state.selected_idx else {
            return;
        };

        let len = self.items.len();
        if len < 2 {
            return;
        }

        let new_idx = if direction < 0 {
            selected_idx.checked_sub(1)
        } else {
            selected_idx.checked_add(1).filter(|idx| *idx < len)
        };

        let Some(new_idx) = new_idx else {
            return;
        };

        let id = self.items[selected_idx].id;
        self.items.swap(selected_idx, new_idx);
        self.state.selected_idx = Some(new_idx);
        self.state.ensure_visible(len, Self::max_visible_rows(len));

        if direction < 0 {
            self.app_event_tx.send(AppEvent::QueueMoveUp { id });
        } else {
            self.app_event_tx.send(AppEvent::QueueMoveDown { id });
        }
        self.delete_confirm_id = None;
    }

    fn move_selected_to_front(&mut self) {
        let Some(selected_idx) = self.state.selected_idx else {
            return;
        };

        if selected_idx == 0 {
            return;
        }

        let Some(item) = self.items.get(selected_idx).cloned() else {
            return;
        };

        self.items.remove(selected_idx);
        self.items.insert(0, item.clone());
        self.state.selected_idx = Some(0);
        self.state
            .ensure_visible(self.items.len(), Self::max_visible_rows(self.items.len()));
        self.app_event_tx
            .send(AppEvent::QueueMoveToFront { id: item.id });
        self.delete_confirm_id = None;
    }

    fn request_delete_selected(&mut self) {
        let Some(selected) = self.selected_item() else {
            return;
        };

        if self.delete_confirm_id == Some(selected.id) {
            let id = selected.id;
            if let Some(idx) = self.state.selected_idx {
                self.items.remove(idx);
            }
            self.app_event_tx.send(AppEvent::QueueDelete { id });
            self.delete_confirm_id = None;
            self.clamp_selection();
        } else {
            self.delete_confirm_id = Some(selected.id);
        }
    }

    fn build_rows(&self, rows_width: u16) -> Vec<GenericDisplayRow> {
        let max_meta_width = self
            .items
            .iter()
            .filter_map(|item| item.meta.as_ref())
            .map(|meta| UnicodeWidthStr::width(meta.as_str()))
            .max()
            .unwrap_or(0);
        let max_name_width = rows_width.saturating_sub(max_meta_width as u16 + 2).max(1) as usize;

        self.items
            .iter()
            .enumerate()
            .map(|(idx, item)| {
                let is_selected = self.state.selected_idx == Some(idx);
                let prefix = if is_selected { '›' } else { ' ' };
                let n = idx + 1;
                let next_marker = if idx == 0 { "→ " } else { "  " };
                let prefix_text = format!("{prefix} {n}. {next_marker}");
                let prefix_width = UnicodeWidthStr::width(prefix_text.as_str());
                let available_preview_width = max_name_width.saturating_sub(prefix_width);
                let preview = truncate_to_width(item.preview.as_str(), available_preview_width);
                let name = format!("{prefix_text}{preview}");
                GenericDisplayRow {
                    name_prefix: None,
                    name,
                    display_shortcut: None,
                    match_indices: None,
                    description: item.meta.clone(),
                    is_disabled: false,
                    disabled_reason: None,
                    wrap_indent: None,
                }
            })
            .collect()
    }

    fn footer_hint(&self) -> Line<'static> {
        if self.delete_confirm_id.is_some() {
            "Press D again to delete · Esc cancel".into()
        } else {
            "Enter edit · M model · T thinking · D delete · Shift+J up · Shift+K down · S send next · Esc close"
                .into()
        }
    }

    fn rows_width(total_width: u16) -> u16 {
        total_width.saturating_sub(2)
    }
}

impl BottomPaneView for QueuePopup {
    fn handle_key_event(&mut self, key_event: KeyEvent) {
        match key_event {
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                self.on_ctrl_c();
            }
            KeyEvent {
                code: KeyCode::Up,
                modifiers: KeyModifiers::ALT,
                ..
            } => self.reorder_selected(-1),
            KeyEvent {
                code: KeyCode::Char('k'),
                modifiers: KeyModifiers::SHIFT,
                ..
            } => self.reorder_selected(1),
            KeyEvent {
                code: KeyCode::Char('K'),
                modifiers,
                ..
            } if modifiers == KeyModifiers::NONE || modifiers == KeyModifiers::SHIFT => {
                self.reorder_selected(1);
            }
            KeyEvent {
                code: KeyCode::Down,
                modifiers: KeyModifiers::ALT,
                ..
            } => self.reorder_selected(1),
            KeyEvent {
                code: KeyCode::Char('j'),
                modifiers: KeyModifiers::SHIFT,
                ..
            } => self.reorder_selected(-1),
            KeyEvent {
                code: KeyCode::Char('J'),
                modifiers,
                ..
            } if modifiers == KeyModifiers::NONE || modifiers == KeyModifiers::SHIFT => {
                self.reorder_selected(-1);
            }
            KeyEvent {
                code: KeyCode::Up, ..
            }
            | KeyEvent {
                code: KeyCode::Char('k'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('p'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('\u{0010}'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_selection_up(),
            KeyEvent {
                code: KeyCode::Down,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('j'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('\u{000e}'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_selection_down(),
            KeyEvent {
                code: KeyCode::Char('s'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('S'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.move_selected_to_front(),
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('D'),
                modifiers: KeyModifiers::NONE,
                ..
            } => self.request_delete_selected(),
            KeyEvent {
                code: KeyCode::Char('m'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('M'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                if let Some(selected) = self.selected_item() {
                    self.app_event_tx
                        .send(AppEvent::QueueOpenModelPicker { id: selected.id });
                }
                self.delete_confirm_id = None;
            }
            KeyEvent {
                code: KeyCode::Char('t'),
                modifiers: KeyModifiers::NONE,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('T'),
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                if let Some(selected) = self.selected_item() {
                    self.app_event_tx
                        .send(AppEvent::QueueOpenThinkingPicker { id: selected.id });
                }
                self.delete_confirm_id = None;
            }
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                if let Some(selected) = self.selected_item() {
                    self.app_event_tx
                        .send(AppEvent::QueueStartEdit { id: selected.id });
                }
                self.complete = true;
            }
            _ => {}
        }
    }

    fn is_complete(&self) -> bool {
        self.complete
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.complete = true;
        CancellationEvent::Handled
    }
}

impl Renderable for QueuePopup {
    fn desired_height(&self, width: u16) -> u16 {
        let rows_width = Self::rows_width(width);
        let rows = self.build_rows(rows_width);
        let rows_height = measure_rows_height(
            &rows,
            &self.state,
            MAX_POPUP_ROWS,
            rows_width.saturating_add(1),
        );

        let header_height: u16 = 2;
        header_height
            .saturating_add(rows_height)
            .saturating_add(3)
            .saturating_add(1)
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let [content_area, footer_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);

        Block::default()
            .style(user_message_style())
            .render(content_area, buf);

        let inner = content_area.inset(Insets::vh(1, 2));
        let [header_area, _, list_area] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .areas(inner);

        if header_area.height >= 1 {
            Line::from("Queue".bold()).render(
                Rect {
                    x: header_area.x,
                    y: header_area.y,
                    width: header_area.width,
                    height: 1,
                },
                buf,
            );
        }
        if header_area.height >= 2 {
            Line::from("Queued messages (oldest -> newest)".dim()).render(
                Rect {
                    x: header_area.x,
                    y: header_area.y + 1,
                    width: header_area.width,
                    height: 1,
                },
                buf,
            );
        }

        let rows = self.build_rows(Self::rows_width(content_area.width));
        if list_area.height > 0 {
            let render_area = Rect {
                x: list_area.x.saturating_sub(2),
                y: list_area.y,
                width: Self::rows_width(content_area.width).max(1),
                height: list_area.height,
            };
            render_rows(
                render_area,
                buf,
                &rows,
                &self.state,
                render_area.height as usize,
                "queue is empty",
            );
        }

        let hint_area = Rect {
            x: footer_area.x + 2,
            y: footer_area.y,
            width: footer_area.width.saturating_sub(2),
            height: footer_area.height,
        };
        self.footer_hint().dim().render(hint_area, buf);
    }
}

fn truncate_to_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }

    let width = UnicodeWidthStr::width(text);
    if width <= max_width {
        return text.to_string();
    }

    if max_width == 1 {
        return "…".to_string();
    }

    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used.saturating_add(ch_width) > max_width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used = used.saturating_add(ch_width);
    }
    out.push('…');
    out
}
