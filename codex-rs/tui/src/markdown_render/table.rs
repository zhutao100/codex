use std::borrow::Cow;
use std::mem;

use pulldown_cmark::Alignment;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::render::line_utils::display_width;
use crate::render::line_utils::line_to_static;
use crate::render::line_utils::line_width;
use crate::wrapping::word_wrap_table_cell;

const CELL_PADDING: usize = 1;
const COLUMN_GAP: &str = "  ";
const MIN_READABLE_CELL_WIDTH: usize = 6;
const MIN_SCANNABLE_TOKEN_WIDTH: usize = 12;
const LONG_TOKEN_WIDTH: usize = 20;

/// Keep column geometry independent of the width at which the Markdown was parsed.
#[derive(Debug)]
pub(crate) struct TableBlock {
    pub(super) table: Table,
    pub(super) initial_indent: Vec<Span<'static>>,
    pub(super) subsequent_indent: Vec<Span<'static>>,
    pub(super) style: Style,
}

impl TableBlock {
    pub(crate) fn render(&self, width: Option<usize>) -> Vec<Line<'static>> {
        let width = width.map(|width| {
            let indent_width = [&self.initial_indent, &self.subsequent_indent]
                .into_iter()
                .map(|prefix| {
                    prefix
                        .iter()
                        .map(|span| display_width(&span.content))
                        .sum::<usize>()
                })
                .max()
                .unwrap_or_default();
            width.saturating_sub(indent_width)
        });
        self.table
            .render(width)
            .into_iter()
            .enumerate()
            .map(|(index, line)| {
                let indent = if index == 0 {
                    &self.initial_indent
                } else {
                    &self.subsequent_indent
                };
                Line::from_iter(indent.iter().cloned().chain(line.spans)).style(self.style)
            })
            .collect()
    }
}

#[derive(Debug)]
pub(super) struct Table {
    alignments: Vec<Alignment>,
    header: Vec<TableCell>,
    rows: Vec<Vec<TableCell>>,
    row: Vec<TableCell>,
    cell: TableCell,
}

#[derive(Debug, Default)]
struct TableCell {
    lines: Vec<Line<'static>>,
    after_cr: bool,
}

impl TableCell {
    fn push_span(&mut self, span: Span<'static>) {
        if span.content.is_empty() {
            return;
        }
        if !span.content.contains(['\r', '\n']) {
            self.after_cr = false;
            self.push_inline(span);
            return;
        }
        // Separate numeric entities can deliver CR and LF in different parser events.
        for part in span.content.split_inclusive(['\r', '\n']) {
            let follows_cr = mem::replace(&mut self.after_cr, part.ends_with('\r'));
            if follows_cr && part == "\n" {
                continue;
            }
            let text = part.trim_end_matches(['\r', '\n']);
            self.push_inline(Span::styled(text.to_owned(), span.style));
            if text.len() < part.len() {
                self.lines.push(Line::default());
            }
        }
    }

    fn push_inline(&mut self, span: Span<'static>) {
        let Some(line) = self.lines.last_mut() else {
            self.lines.push(span.into());
            return;
        };
        // Entity decoding can split a grapheme across events with the same style.
        if let Some(last) = line.spans.last_mut()
            && last.style == span.style
        {
            last.content.to_mut().push_str(&span.content);
        } else {
            line.push_span(span);
        }
    }

    fn width(&self) -> usize {
        self.lines.iter().map(line_width).max().unwrap_or_default()
    }

    fn label_spans(&self) -> impl Iterator<Item = Span<'static>> + '_ {
        self.lines.iter().enumerate().flat_map(|(index, line)| {
            (index > 0)
                .then(|| Span::from(" "))
                .into_iter()
                .chain(line.spans.iter().cloned())
        })
    }
}

impl Table {
    pub(super) fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            header: Vec::new(),
            rows: Vec::new(),
            row: Vec::new(),
            cell: TableCell::default(),
        }
    }

    pub(super) fn push_span(&mut self, span: Span<'static>) {
        self.cell.push_span(span);
    }

    pub(super) fn finish_cell(&mut self) {
        self.row.push(mem::take(&mut self.cell));
    }

    pub(super) fn finish_row(&mut self, header: bool) {
        let row = mem::take(&mut self.row);
        if header {
            self.header = row;
        } else {
            self.rows.push(row);
        }
    }

    fn render(&self, width: Option<usize>) -> Vec<Line<'static>> {
        let mut widths = vec![1; self.alignments.len()];
        for row in std::iter::once(&self.header).chain(&self.rows) {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(cell.width());
            }
        }
        if let Some(width) = width {
            let overhead =
                widths.len() * (2 * CELL_PADDING) + (widths.len() - 1) * COLUMN_GAP.len();
            let available = width.saturating_sub(overhead);
            if widths.iter().sum::<usize>() > available
                && (!fit_columns(&mut widths, available, &self.rows)
                    || should_render_records(&self.rows, &widths))
            {
                if self.rows.is_empty() {
                    return self.render_pipe_header(width.max(1));
                }
                return render_records(&self.header, &self.rows, width.max(1));
            }
        }

        let mut lines = render_row(
            &self.header,
            &widths,
            &self.alignments,
            /*header*/ true,
        );
        lines.push(rule(&widths, '━'));
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 {
                lines.push(rule(&widths, '─'));
            }
            lines.extend(render_row(
                row,
                &widths,
                &self.alignments,
                /*header*/ false,
            ));
        }
        lines
    }

    fn render_pipe_header(&self, width: usize) -> Vec<Line<'static>> {
        let mut label = Line::from("|");
        let mut rule = String::from("|");
        for (cell, alignment) in self.header.iter().zip(&self.alignments) {
            label.push_span(" ");
            label.spans.extend(
                cell.label_spans()
                    .map(|span| Span::styled(span.content.replace('|', "\\|"), span.style)),
            );
            label.push_span(" |");
            rule.push_str(match alignment {
                Alignment::None => "---|",
                Alignment::Left => ":---|",
                Alignment::Center => ":---:|",
                Alignment::Right => "---:|",
            });
        }
        let mut lines = wrap_owned(&label, width);
        lines.extend(wrap_owned(&Line::from(rule), width));
        lines
    }
}

fn should_render_records(rows: &[Vec<TableCell>], widths: &[usize]) -> bool {
    // Prefer records for systemic fragmentation, not one long outlier in an
    // otherwise readable grid. Display widths give a cheap lower bound on height.
    let threshold = if rows.len() == 1 {
        1
    } else {
        rows.len().div_ceil(3).max(2)
    };
    let mut affected_rows = 0;
    for row in rows {
        let mut tall_cells = 0;
        let cramped = row.iter().zip(widths).any(|(cell, &width)| {
            let min_height = cell.width().div_ceil(width);
            tall_cells += usize::from(min_height >= 4);
            tall_cells >= 2
                || (width < MIN_SCANNABLE_TOKEN_WIDTH && min_height >= 7)
                || (min_height > 1
                    && cell
                        .lines
                        .iter()
                        .filter_map(short_line_token_width)
                        .any(|token| {
                            token > width
                                && (token < LONG_TOKEN_WIDTH || width < MIN_SCANNABLE_TOKEN_WIDTH)
                        }))
        });
        affected_rows += usize::from(cramped);
        if affected_rows >= threshold {
            return true;
        }
    }
    false
}

fn short_line_token_width(line: &Line<'_>) -> Option<usize> {
    let text = match line.spans.as_slice() {
        [span] => Cow::Borrowed(span.content.as_ref()),
        _ => Cow::Owned(line.to_string()),
    };
    let words = text.split_whitespace();
    (words.clone().nth(3).is_none()).then(|| words.map(display_width).max().unwrap_or(0))
}

fn fit_columns(widths: &mut [usize], available: usize, rows: &[Vec<TableCell>]) -> bool {
    let mut floors = widths
        .iter()
        .map(|&width| width.min(MIN_READABLE_CELL_WIDTH))
        .collect::<Vec<_>>();
    let minimum = floors.iter().sum::<usize>();
    if minimum > available {
        return false;
    }
    let mut extra = available - minimum;
    let mut order: Vec<_> = (0..widths.len()).collect();
    order.sort_unstable_by_key(|&column| (widths[column], column));
    // Reserve intact compact values (dates, IDs) before distributing space to prose.
    for &column in &order {
        if extra == 0 {
            break;
        }
        if widths[column] == floors[column] {
            continue;
        }
        let compact = rows
            .iter()
            .flat_map(|row| &row[column].lines)
            .try_fold(0, |max, line| {
                short_line_token_width(line)
                    .filter(|&w| w < LONG_TOKEN_WIDTH)
                    .map(|w| max.max(w))
            });
        let reserve = compact.unwrap_or(0).saturating_sub(floors[column]);
        if reserve <= extra {
            floors[column] += reserve;
            extra -= reserve;
        }
    }
    order.sort_unstable_by_key(|&column| (widths[column] - floors[column], column));
    for (index, column) in order.into_iter().enumerate() {
        let share = (widths[column] - floors[column]).min(extra / (widths.len() - index));
        widths[column] = floors[column] + share;
        extra -= share;
    }
    true
}

fn rule(widths: &[usize], glyph: char) -> Line<'static> {
    widths
        .iter()
        .map(|&width| glyph.to_string().repeat(width + 2 * CELL_PADDING))
        .collect::<Vec<_>>()
        .join(COLUMN_GAP)
        .dim()
        .into()
}

fn render_row(
    row: &[TableCell],
    widths: &[usize],
    alignments: &[Alignment],
    header: bool,
) -> Vec<Line<'static>> {
    let cells: Vec<_> = row
        .iter()
        .zip(widths)
        .map(|(cell, &width)| {
            if cell.width() <= width {
                Cow::Borrowed(cell.lines.as_slice())
            } else {
                Cow::Owned(
                    cell.lines
                        .iter()
                        .flat_map(|line| {
                            word_wrap_table_cell(
                                line,
                                width,
                                textwrap::WordSplitter::HyphenSplitter,
                            )
                        })
                        .collect(),
                )
            }
        })
        .collect();
    let height = cells
        .iter()
        .map(|cell| cell.len())
        .max()
        .unwrap_or(0)
        .max(1);
    (0..height)
        .map(|line_index| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (column, (&width, alignment)) in widths.iter().zip(alignments).enumerate() {
                let contents = cells[column].get(line_index);
                let slack = width.saturating_sub(contents.map_or(0, line_width));
                let left = match alignment {
                    Alignment::None | Alignment::Left => 0,
                    Alignment::Center => slack / 2,
                    Alignment::Right => slack,
                };
                spans.push(" ".repeat(CELL_PADDING + left).into());
                if let Some(line) = contents {
                    spans.extend(
                        line_to_static(line)
                            .spans
                            .into_iter()
                            .map(|span| if header { span.bold() } else { span }),
                    );
                }
                if column + 1 < widths.len() {
                    spans.push(" ".repeat(CELL_PADDING + slack - left).into());
                    spans.push(COLUMN_GAP.into());
                }
            }
            spans.into()
        })
        .collect()
}

fn render_records(
    header: &[TableCell],
    rows: &[Vec<TableCell>],
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            lines.push(Line::default());
        }
        for (column, (label, value)) in header.iter().zip(row).enumerate() {
            let mut line = if label.width() == 0 {
                let number = column + 1;
                Line::from(format!("Column {number}").bold())
            } else {
                Line::from_iter(label.label_spans().map(Stylize::bold))
            };
            line.push_span(": ");
            let mut value_lines = value.lines.iter();
            if let Some(first) = value_lines.next() {
                line.spans.extend(first.spans.iter().cloned());
            }
            lines.extend(wrap_owned(&line, width));
            lines.extend(value_lines.flat_map(|line| wrap_owned(line, width)));
        }
    }
    lines
}

fn wrap_owned(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    word_wrap_table_cell(line, width, textwrap::WordSplitter::NoHyphenation)
        .iter()
        .map(line_to_static)
        .collect()
}
