use crate::render::line_utils::line_to_static;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_line;
use pulldown_cmark::BrokenLink;
use pulldown_cmark::CodeBlockKind;
use pulldown_cmark::CowStr;
use pulldown_cmark::Event;
use pulldown_cmark::HeadingLevel;
use pulldown_cmark::Options;
use pulldown_cmark::Parser;
use pulldown_cmark::Tag;
use pulldown_cmark::TagEnd;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use std::cell::Cell;
use std::ops::Range;
use std::sync::OnceLock;
use std::sync::RwLock;

use crate::render::syntect::DEFAULT_SYNTAX_THEME;
use crate::render::syntect::SyntectHighlighter;

mod fences;
mod table;

use table::Table;
use table::TableBlock;

const MARKDOWN_OPTIONS: Options = Options::ENABLE_STRIKETHROUGH.union(Options::ENABLE_TABLES);

/// Prose can stream one line at a time. A table stays together and is laid out by
/// the eventual display rather than by the viewport at the start of the reply.
#[derive(Debug)]
pub(crate) enum MarkdownBlock {
    Line(Line<'static>),
    Table(Box<TableBlock>),
}

pub(crate) struct SourceBlock {
    pub(crate) start: usize,
    pub(crate) block: MarkdownBlock,
}

pub(crate) struct MarkdownRender {
    pub(crate) blocks: Vec<SourceBlock>,
    pub(crate) mutable_from: Option<usize>,
}

impl MarkdownBlock {
    pub(crate) fn into_lines(self, width: Option<usize>) -> Vec<Line<'static>> {
        match self {
            Self::Line(line) => vec![line],
            Self::Table(table) => table.render(width),
        }
    }

    pub(crate) fn to_lines(&self, width: Option<usize>) -> Vec<Line<'static>> {
        match self {
            Self::Line(line) => vec![line.clone()],
            Self::Table(table) => table.render(width),
        }
    }
}

struct MarkdownStyles {
    h1: Style,
    h2: Style,
    h3: Style,
    h4: Style,
    h5: Style,
    h6: Style,
    code: Style,
    emphasis: Style,
    strong: Style,
    strikethrough: Style,
    ordered_list_marker: Style,
    unordered_list_marker: Style,
    link: Style,
    blockquote: Style,
    code_block_frame: Style,
}

impl Default for MarkdownStyles {
    fn default() -> Self {
        use ratatui::style::Stylize;

        Self {
            h1: Style::new().bold().underlined(),
            h2: Style::new().bold(),
            h3: Style::new().bold().italic(),
            h4: Style::new().italic(),
            h5: Style::new().italic(),
            h6: Style::new().italic(),
            code: Style::new().cyan(),
            emphasis: Style::new().italic(),
            strong: Style::new().bold(),
            strikethrough: Style::new().crossed_out(),
            ordered_list_marker: Style::new().light_blue(),
            unordered_list_marker: Style::new(),
            link: Style::new().cyan().underlined(),
            blockquote: Style::new().green(),
            code_block_frame: Style::new().dim(),
        }
    }
}

#[derive(Clone, Debug)]
struct IndentContext {
    prefix: Vec<Span<'static>>,
    marker: Option<Vec<Span<'static>>>,
    is_list: bool,
}

impl IndentContext {
    fn new(prefix: Vec<Span<'static>>, marker: Option<Vec<Span<'static>>>, is_list: bool) -> Self {
        Self {
            prefix,
            marker,
            is_list,
        }
    }
}

pub fn render_markdown_text(input: &str) -> Text<'static> {
    render_markdown_text_with_width(input, None)
}

pub(crate) fn set_syntax_highlight_theme(theme_name: String) {
    let lock = syntax_highlight_theme_lock();
    if let Ok(mut value) = lock.write() {
        *value = theme_name;
    }
}

pub(crate) fn render_markdown_text_with_width(input: &str, width: Option<usize>) -> Text<'static> {
    Text::from(
        render_markdown_blocks(
            input, width, /*streaming*/ false, /*unwrap_fences*/ false,
        )
        .blocks
        .into_iter()
        .flat_map(|entry| entry.block.into_lines(width))
        .collect::<Vec<_>>(),
    )
}

pub(crate) fn render_completed_markdown(input: &str, width: Option<usize>) -> Vec<MarkdownBlock> {
    render_markdown_blocks(
        input, width, /*streaming*/ false, /*unwrap_fences*/ true,
    )
    .blocks
    .into_iter()
    .map(|entry| entry.block)
    .collect()
}

/// Source offsets refer to the normalized input. Fence normalization does not change
/// an already-rendered prefix: open Markdown fences are withheld until classified.
pub(crate) fn render_markdown_blocks(
    input: &str,
    width: Option<usize>,
    streaming: bool,
    unwrap_fences: bool,
) -> MarkdownRender {
    if unwrap_fences {
        let (source, mutable_from) = fences::unwrap_table_fences(input, streaming);
        Writer::new(width).run(&source, streaming, mutable_from)
    } else {
        Writer::new(width).run(input, streaming, None)
    }
}

fn syntax_highlight_theme_lock() -> &'static RwLock<String> {
    static THEME: OnceLock<RwLock<String>> = OnceLock::new();
    THEME.get_or_init(|| RwLock::new(DEFAULT_SYNTAX_THEME.to_string()))
}

fn syntax_highlight_theme() -> String {
    syntax_highlight_theme_lock()
        .read()
        .map(|value| value.clone())
        .unwrap_or_else(|_| DEFAULT_SYNTAX_THEME.to_string())
}

fn line_start(source: &str, offset: usize) -> usize {
    source[..offset].rfind('\n').map_or(0, |index| index + 1)
}

struct Writer {
    blocks: Vec<SourceBlock>,
    event_start: usize,
    current_line_start: usize,
    table_start: usize,
    styles: MarkdownStyles,
    inline_styles: Vec<Style>,
    indent_stack: Vec<IndentContext>,
    list_indices: Vec<Option<u64>>,
    link: Option<String>,
    needs_newline: bool,
    pending_marker_line: bool,
    in_paragraph: bool,
    in_code_block: bool,
    in_fenced_code_block: bool,
    wrap_width: Option<usize>,
    current_line_content: Option<Line<'static>>,
    current_initial_indent: Vec<Span<'static>>,
    current_subsequent_indent: Vec<Span<'static>>,
    current_line_style: Style,
    current_line_in_code_block: bool,
    syntax_theme: String,
    code_highlighter: Option<SyntectHighlighter>,
    table: Option<Table>,
}

impl Writer {
    fn new(wrap_width: Option<usize>) -> Self {
        Self {
            blocks: Vec::new(),
            event_start: 0,
            current_line_start: 0,
            table_start: 0,
            styles: MarkdownStyles::default(),
            inline_styles: Vec::new(),
            indent_stack: Vec::new(),
            list_indices: Vec::new(),
            link: None,
            needs_newline: false,
            pending_marker_line: false,
            in_paragraph: false,
            in_code_block: false,
            in_fenced_code_block: false,
            wrap_width,
            current_line_content: None,
            current_initial_indent: Vec::new(),
            current_subsequent_indent: Vec::new(),
            current_line_style: Style::default(),
            current_line_in_code_block: false,
            syntax_theme: syntax_highlight_theme(),
            code_highlighter: None,
            table: None,
        }
    }

    fn run(
        mut self,
        input: &str,
        streaming: bool,
        mut mutable_from: Option<usize>,
    ) -> MarkdownRender {
        let tail = input.strip_suffix('\n').unwrap_or(input);
        let last_line_start = line_start(tail, tail.len());
        let pending_header =
            (streaming && tail[last_line_start..].contains('|')).then_some(last_line_start);
        let unresolved_reference = Cell::new(None::<usize>);
        let parser = Parser::new_with_broken_link_callback(
            input,
            MARKDOWN_OPTIONS,
            streaming.then_some(|link: BrokenLink<'_>| {
                let start = line_start(input, link.span.start);
                unresolved_reference.set(Some(
                    unresolved_reference
                        .get()
                        .map_or(start, |first| start.min(first)),
                ));
                None
            }),
        );
        let mut open_inline = Vec::new();
        for (event, range) in parser.into_offset_iter() {
            // A delimiter can promote the last line to a header, and another row can change
            // every column's width. Neither belongs in append-only history yet.
            let starts_table = matches!(event, Event::Start(Tag::Table(_)));
            let unfinished_table = streaming && starts_table && range.end == input.len();
            if unfinished_table {
                let table_start = line_start(input, range.start);
                mutable_from = Some(table_start.min(mutable_from.unwrap_or(table_start)));
            } else if self.table.is_none()
                && !self.in_code_block
                && let Some(start) = pending_header
                && (range.start >= start
                    || (range.end == start && matches!(event, Event::SoftBreak | Event::HardBreak)))
            {
                // Keep the possible header visible, but hold it and any separator
                // with the eventual table out of append-only history.
                mutable_from = Some(start.min(mutable_from.unwrap_or(start)));
            }
            self.event_start = match event {
                Event::SoftBreak | Event::HardBreak => range.end,
                _ => line_start(input, range.start),
            };
            if starts_table {
                self.table_start = self.event_start;
            }
            let source = &input[range.clone()];
            if streaming {
                match event {
                    Event::Start(Tag::Paragraph | Tag::Item) => {
                        open_inline.push(range.end == input.len());
                    }
                    Event::End(TagEnd::Paragraph | TagEnd::Item) => {
                        open_inline.pop();
                    }
                    // The parser leaves unmatched backtick runs as text. A later line
                    // can close one and collapse several previously rendered lines.
                    Event::Text(_)
                        if open_inline.last() == Some(&true)
                            && !self.in_code_block
                            && self.table.is_none()
                            && !source.is_empty()
                            && source.bytes().all(|b| b == b'`') =>
                    {
                        let start = self.event_start;
                        mutable_from = Some(mutable_from.map_or(start, |first| first.min(start)));
                    }
                    _ => {}
                }
                if let Some(start) = unresolved_reference.take() {
                    let start = if self.table.is_some() {
                        self.table_start
                    } else {
                        start
                    };
                    mutable_from = Some(mutable_from.map_or(start, |first| first.min(start)));
                }
            }
            self.handle_event(event, input, range);
        }
        self.flush_current_line();
        MarkdownRender {
            blocks: self.blocks,
            mutable_from,
        }
    }

    fn handle_event(&mut self, event: Event<'_>, input: &str, range: Range<usize>) {
        let source = &input[range.clone()];
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.text(text, source, range.start),
            Event::Code(code) => self.code(code),
            Event::SoftBreak => self.soft_break(),
            Event::HardBreak => self.hard_break(),
            Event::Rule => {
                self.flush_current_line();
                if !self.blocks.is_empty() {
                    self.push_blank_line();
                }
                self.push_line(Line::from("———"));
                self.needs_newline = true;
            }
            Event::Html(html) => self.html(html, source, range.start, false),
            Event::InlineHtml(html) => self.html(html, source, range.start, true),
            Event::FootnoteReference(_) => {}
            Event::TaskListMarker(_) => {}
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.start_paragraph(),
            Tag::Heading { level, .. } => self.start_heading(level),
            Tag::BlockQuote => self.start_blockquote(),
            Tag::CodeBlock(kind) => {
                let indent = match kind {
                    CodeBlockKind::Fenced(_) => None,
                    CodeBlockKind::Indented => Some(Span::from(" ".repeat(4))),
                };
                match kind {
                    CodeBlockKind::Fenced(lang) => {
                        self.start_codeblock(Some(lang.to_string()), indent, true)
                    }
                    CodeBlockKind::Indented => self.start_codeblock(None, indent, false),
                }
            }
            Tag::List(start) => self.start_list(start),
            Tag::Item => self.start_item(),
            Tag::Emphasis => self.push_inline_style(self.styles.emphasis),
            Tag::Strong => self.push_inline_style(self.styles.strong),
            Tag::Strikethrough => self.push_inline_style(self.styles.strikethrough),
            Tag::Link { dest_url, .. } => self.push_link(dest_url.to_string()),
            Tag::Table(alignments) => {
                self.flush_current_line();
                self.table = Some(Table::new(alignments));
            }
            Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::Image { .. }
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.end_paragraph(),
            TagEnd::Heading(_) => self.end_heading(),
            TagEnd::BlockQuote => self.end_blockquote(),
            TagEnd::CodeBlock => self.end_codeblock(),
            TagEnd::List(_) => self.end_list(),
            TagEnd::Item => {
                self.indent_stack.pop();
                self.pending_marker_line = false;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_inline_style(),
            TagEnd::Link => self.pop_link(),
            TagEnd::Table => self.end_table(),
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.finish_row(matches!(tag, TagEnd::TableHead));
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.finish_cell();
                }
            }
            TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::Image
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn start_paragraph(&mut self) {
        if self.needs_newline {
            self.push_blank_line();
        }
        self.push_line(Line::default());
        self.needs_newline = false;
        self.in_paragraph = true;
    }

    fn end_paragraph(&mut self) {
        self.needs_newline = true;
        self.in_paragraph = false;
        self.pending_marker_line = false;
    }

    fn start_heading(&mut self, level: HeadingLevel) {
        if self.needs_newline {
            self.push_line(Line::default());
            self.needs_newline = false;
        }
        let heading_style = match level {
            HeadingLevel::H1 => self.styles.h1,
            HeadingLevel::H2 => self.styles.h2,
            HeadingLevel::H3 => self.styles.h3,
            HeadingLevel::H4 => self.styles.h4,
            HeadingLevel::H5 => self.styles.h5,
            HeadingLevel::H6 => self.styles.h6,
        };
        let content = format!("{} ", "#".repeat(level as usize));
        self.push_line(Line::from(vec![Span::styled(content, heading_style)]));
        self.push_inline_style(heading_style);
        self.needs_newline = false;
    }

    fn end_heading(&mut self) {
        self.needs_newline = true;
        self.pop_inline_style();
    }

    fn start_blockquote(&mut self) {
        if self.needs_newline {
            self.push_blank_line();
            self.needs_newline = false;
        }
        self.indent_stack
            .push(IndentContext::new(vec![Span::from("> ")], None, false));
    }

    fn end_blockquote(&mut self) {
        self.indent_stack.pop();
        self.needs_newline = true;
    }

    fn text(&mut self, text: CowStr<'_>, source: &str, start: usize) {
        if let Some(table) = self.table.as_mut() {
            let style = self.inline_styles.last().copied().unwrap_or_default();
            table.push_span(Span::styled(text.into_string(), style));
            return;
        }
        if self.pending_marker_line {
            self.push_line(Line::default());
        }
        if self.in_code_block && !self.needs_newline {
            let has_content = self
                .current_line_content
                .as_ref()
                .map(|line| !line.spans.is_empty())
                .unwrap_or_else(|| {
                    matches!(self.blocks.last().map(|entry| &entry.block), Some(MarkdownBlock::Line(line)) if !line.spans.is_empty())
                });
            if has_content {
                self.push_line(Line::default());
            }
        }
        let mut source_lines = source.match_indices('\n');
        for (i, line) in text.lines().enumerate() {
            if i > 0
                && let Some((newline, _)) = source_lines.next()
            {
                // A code text event may span several lines. Anchor each line to its
                // own source line rather than to the start of the whole code event.
                self.event_start = start + newline + 1;
            }
            if self.needs_newline {
                self.push_line(Line::default());
                self.needs_newline = false;
            }
            if i > 0 {
                self.push_line(Line::default());
            }
            if let Some(highlighter) = self.code_highlighter.as_mut() {
                for span in highlighter.highlight_line(line) {
                    self.push_span(span);
                }
            } else {
                let content = line.to_string();
                let span = Span::styled(
                    content,
                    self.inline_styles.last().copied().unwrap_or_default(),
                );
                self.push_span(span);
            }
        }
        self.needs_newline = false;
    }

    fn code(&mut self, code: CowStr<'_>) {
        if self.pending_marker_line && self.table.is_none() {
            self.push_line(Line::default());
        }
        let span = Span::from(code.into_string()).style(self.styles.code);
        self.push_span(span);
    }

    fn html(&mut self, html: CowStr<'_>, source: &str, start: usize, inline: bool) {
        if self.table.is_some() {
            self.text(html, source, start);
            return;
        }
        self.pending_marker_line = false;
        let mut source_lines = source.match_indices('\n');
        for (i, line) in html.lines().enumerate() {
            if i > 0
                && let Some((newline, _)) = source_lines.next()
            {
                self.event_start = start + newline + 1;
            }
            if self.needs_newline {
                self.push_line(Line::default());
                self.needs_newline = false;
            }
            if i > 0 {
                self.push_line(Line::default());
            }
            let style = self.inline_styles.last().copied().unwrap_or_default();
            self.push_span(Span::styled(line.to_string(), style));
        }
        self.needs_newline = !inline;
    }

    fn hard_break(&mut self) {
        self.push_line(Line::default());
    }

    fn soft_break(&mut self) {
        self.push_line(Line::default());
    }

    fn start_list(&mut self, index: Option<u64>) {
        if self.list_indices.is_empty() && self.needs_newline {
            self.push_line(Line::default());
        }
        self.list_indices.push(index);
    }

    fn end_list(&mut self) {
        self.list_indices.pop();
        self.needs_newline = true;
    }

    fn start_item(&mut self) {
        self.pending_marker_line = true;
        let depth = self.list_indices.len();
        let Some(index) = self.list_indices.last_mut() else {
            return;
        };
        let width = depth * 4 - 3;
        let marker = match index {
            None => Span::styled(
                " ".repeat(width - 1) + "- ",
                self.styles.unordered_list_marker,
            ),
            Some(index) => {
                let marker =
                    Span::styled(format!("{index:width$}. "), self.styles.ordered_list_marker);
                *index += 1;
                marker
            }
        };
        let indent_prefix = vec![Span::from(" ".repeat(marker.width()))];
        self.indent_stack
            .push(IndentContext::new(indent_prefix, Some(vec![marker]), true));
        self.needs_newline = false;
    }

    fn start_codeblock(
        &mut self,
        lang: Option<String>,
        indent: Option<Span<'static>>,
        is_fenced: bool,
    ) {
        self.flush_current_line();
        if !self.blocks.is_empty() {
            self.push_blank_line();
        }

        if is_fenced {
            let language = lang
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string);
            let heading = match language.clone() {
                Some(language) if !language.is_empty() => format!("╭ code: {language}"),
                _ => "╭ code".to_string(),
            };
            let mut spans = self.prefix_spans(false);
            spans.push(Span::styled(heading, self.styles.code_block_frame));
            self.blocks.push(SourceBlock {
                start: self.event_start,
                block: MarkdownBlock::Line(Line::from(spans)),
            });
            self.code_highlighter =
                SyntectHighlighter::from_language(language.as_deref(), &self.syntax_theme);
        }

        self.in_code_block = true;
        self.in_fenced_code_block = is_fenced;
        let mut prefix = Vec::new();
        if let Some(indent) = indent {
            prefix.push(indent);
        }
        if is_fenced {
            prefix.push(Span::styled("│ ".to_string(), self.styles.code_block_frame));
        }
        self.indent_stack
            .push(IndentContext::new(prefix, None, false));
        self.needs_newline = true;
    }

    fn end_codeblock(&mut self) {
        self.flush_current_line();
        self.needs_newline = true;
        self.in_code_block = false;
        self.in_fenced_code_block = false;
        self.code_highlighter = None;
        self.indent_stack.pop();
    }

    fn end_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        self.event_start = self.table_start;
        if self.needs_newline {
            self.push_blank_line();
        }
        let block = TableBlock {
            table,
            initial_indent: self.prefix_spans(self.pending_marker_line),
            subsequent_indent: self.prefix_spans(false),
            style: self.line_style(Style::default()),
        };
        self.blocks.push(SourceBlock {
            start: self.table_start,
            block: MarkdownBlock::Table(Box::new(block)),
        });
        self.pending_marker_line = false;
        self.needs_newline = true;
    }

    fn push_inline_style(&mut self, style: Style) {
        let current = self.inline_styles.last().copied().unwrap_or_default();
        let merged = current.patch(style);
        self.inline_styles.push(merged);
    }

    fn pop_inline_style(&mut self) {
        self.inline_styles.pop();
    }

    fn push_link(&mut self, dest_url: String) {
        self.link = Some(dest_url);
    }

    fn pop_link(&mut self) {
        if let Some(link) = self.link.take() {
            self.push_span(" (".into());
            self.push_span(Span::styled(link, self.styles.link));
            self.push_span(")".into());
        }
    }

    fn flush_current_line(&mut self) {
        if let Some(line) = self.current_line_content.take() {
            let style = self.current_line_style;
            // Preserve copyable code whitespace.
            if !self.current_line_in_code_block
                && let Some(width) = self.wrap_width
            {
                let opts = RtOptions::new(width)
                    .initial_indent(self.current_initial_indent.clone().into())
                    .subsequent_indent(self.current_subsequent_indent.clone().into());
                for wrapped in word_wrap_line(&line, opts) {
                    let owned = line_to_static(&wrapped).style(style);
                    self.blocks.push(SourceBlock {
                        start: self.current_line_start,
                        block: MarkdownBlock::Line(owned),
                    });
                }
            } else {
                let mut spans = self.current_initial_indent.clone();
                let mut line = line;
                spans.append(&mut line.spans);
                let line = Line::from_iter(spans).style(style);
                self.blocks.push(SourceBlock {
                    start: self.current_line_start,
                    block: MarkdownBlock::Line(line),
                });
            }
            self.current_initial_indent.clear();
            self.current_subsequent_indent.clear();
            self.current_line_in_code_block = false;
        }
    }

    fn push_line(&mut self, line: Line<'static>) {
        self.flush_current_line();
        self.current_initial_indent = self.prefix_spans(self.pending_marker_line);
        self.current_subsequent_indent = self.prefix_spans(false);
        self.current_line_style = self.line_style(line.style);
        self.current_line_start = self.event_start;
        self.current_line_content = Some(line);
        self.current_line_in_code_block = self.in_code_block;

        self.pending_marker_line = false;
    }

    fn line_style(&self, style: Style) -> Style {
        if self
            .indent_stack
            .iter()
            .any(|ctx| ctx.prefix.iter().any(|s| s.content.contains('>')))
        {
            self.styles.blockquote
        } else {
            style
        }
    }

    fn push_span(&mut self, span: Span<'static>) {
        if let Some(table) = self.table.as_mut() {
            table.push_span(span);
        } else if let Some(line) = self.current_line_content.as_mut() {
            line.push_span(span);
        } else {
            self.push_line(Line::from(vec![span]));
        }
    }

    fn push_blank_line(&mut self) {
        self.flush_current_line();
        if self.indent_stack.iter().all(|ctx| ctx.is_list) {
            self.blocks.push(SourceBlock {
                start: self.event_start,
                block: MarkdownBlock::Line(Line::default()),
            });
        } else {
            self.push_line(Line::default());
            self.flush_current_line();
        }
    }

    fn prefix_spans(&self, pending_marker_line: bool) -> Vec<Span<'static>> {
        let mut prefix: Vec<Span<'static>> = Vec::new();
        let last_marker_index = if pending_marker_line {
            self.indent_stack
                .iter()
                .enumerate()
                .rev()
                .find_map(|(i, ctx)| if ctx.marker.is_some() { Some(i) } else { None })
        } else {
            None
        };
        let last_list_index = self.indent_stack.iter().rposition(|ctx| ctx.is_list);

        for (i, ctx) in self.indent_stack.iter().enumerate() {
            if pending_marker_line {
                if Some(i) == last_marker_index
                    && let Some(marker) = &ctx.marker
                {
                    prefix.extend(marker.iter().cloned());
                    continue;
                }
                if ctx.is_list && last_marker_index.is_some_and(|idx| idx > i) {
                    continue;
                }
            } else if ctx.is_list && Some(i) != last_list_index {
                continue;
            }
            prefix.extend(ctx.prefix.iter().cloned());
        }

        prefix
    }
}

#[cfg(test)]
mod markdown_render_tests {
    include!("markdown_render_tests.rs");
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::text::Text;

    fn lines_to_strings(text: &Text<'_>) -> Vec<String> {
        text.lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn aligns_unpadded_table_by_display_width_and_preserves_inline_styles() {
        use ratatui::style::Stylize;

        let source = "|Item|Qty|\n|:---|---:|\n|**火** \\||`2`|\n|bird|120|\n";
        let table = render_markdown_text_with_width(source, Some(20));
        assert_eq!(
            lines_to_strings(&table),
            [
                " Item    Qty",
                "━━━━━━  ━━━━━",
                " 火 |      2",
                "──────  ─────",
                " bird    120",
            ]
        );
        let styled_cells: Vec<_> = table.lines[2]
            .spans
            .iter()
            .filter(|span| matches!(span.content.as_ref(), "火" | "2"))
            .cloned()
            .collect();
        assert_eq!(styled_cells, ["火".bold(), "2".cyan()]);
    }

    #[test]
    fn wraps_table_cells_or_uses_records_when_columns_cannot_fit() {
        let source = "|Name|Description|\n|---|---|\n|alpha|four small words|\n";
        assert_eq!(
            lines_to_strings(&render_markdown_text_with_width(source, Some(24))),
            [
                " Name     Description",
                "━━━━━━━  ━━━━━━━━━━━━━━━",
                " alpha    four small",
                "          words",
            ]
        );
        assert_eq!(
            lines_to_strings(&render_markdown_text_with_width(source, Some(12))),
            ["Name: alpha", "Description:", "four small", "words"]
        );
    }

    #[test]
    fn preserves_sparse_table_rows_in_record_layout() {
        let input = "Name|Status\n---|---\nAlice|OK\nHTML:\n";
        assert_eq!(
            lines_to_strings(&render_markdown_text_with_width(input, Some(12))),
            ["Name: Alice", "Status: OK", "", "Name: HTML:", "Status:"]
        );
    }

    #[test]
    fn table_reserves_a_compact_date_before_wrapping_prose() {
        let input = "Date|Notes\n---|---\n2026-05-01|one two three four\n";
        assert_eq!(
            lines_to_strings(&render_markdown_text_with_width(input, Some(24))),
            [
                " Date          Notes",
                "━━━━━━━━━━━━  ━━━━━━━━━━",
                " 2026-05-01    one two",
                "               three",
                "               four",
            ]
        );
    }

    #[test]
    fn table_in_double_digit_ordered_item_aligns_header_and_body() {
        let input = "10. Key|Val\n    ---|---\n    abc|xyz\n";
        assert_eq!(
            lines_to_strings(&render_markdown_text(input)),
            ["10.  Key    Val", "    ━━━━━  ━━━━━", "     abc    xyz"]
        );
    }

    #[test]
    fn table_measures_and_wraps_halfwidth_kana_in_terminal_cells() {
        let input = "Kana|Value\n---|---\nｶﾞﾊﾟ|Y\nｶﾞﾊﾟ ｶﾞﾊﾟ|Z\n";
        assert_eq!(
            lines_to_strings(&render_markdown_text_with_width(input, Some(17))),
            [
                " Kana      Value",
                "━━━━━━━━  ━━━━━━━",
                " ｶﾞﾊﾟ      Y",
                "────────  ───────",
                " ｶﾞﾊﾟ      Z",
                " ｶﾞﾊﾟ      ",
            ]
        );
    }

    #[test]
    fn narrow_table_keeps_repeated_dates_intact() {
        let input = "|Start|End|\n|---|---|\n|2026-05-01|2026-05-02|\n|2026-05-03|2026-05-04|\n";
        let rendered = render_markdown_text_with_width(input, Some(20));
        assert_eq!(
            lines_to_strings(&rendered),
            [
                "Start: 2026-05-01",
                "End: 2026-05-02",
                "",
                "Start: 2026-05-03",
                "End: 2026-05-04",
            ]
        );
    }

    #[test]
    fn narrow_header_only_table_keeps_its_pipe_syntax() {
        let input = "|A\\|B|Cc|Dd|Ee|\n|:---|---:|---|---|";
        let rendered = render_markdown_text_with_width(input, Some(16));
        assert_eq!(
            lines_to_strings(&rendered),
            ["| A\\|B | Cc | Dd", "| Ee |", "|:---|---:|---|-", "--|"]
        );
    }

    #[test]
    fn long_path_keeps_grid_when_columns_are_readable() {
        let source = "|Path|Notes|\n|---|---|\n|src/platform/terminal/renderer/configuration.rs|Contains the terminal rendering defaults and layout for tables.|\n";
        let grid = render_markdown_text_with_width(source, Some(78));
        assert!(lines_to_strings(&grid)[1].starts_with('━'));
        assert!(grid.lines.iter().all(|line| line.width() <= 78));

        let records = render_markdown_text_with_width(source, Some(28));
        assert!(lines_to_strings(&records)[0].starts_with("Path:"));
    }

    #[test]
    fn cramped_path_and_prose_columns_render_as_records() {
        // Adapt the upstream 42-column session table fixture without local paths.
        let input = r#"| Session | Why useful | Detected table blocks |
| --- | --- | --- |
| [2026-05-25 current gallery](/var/lib/codex/sessions/2026/05/25/rollout-2026-05-25T18-13-09-00000000-0000-0000-0000-000000000001.jsonl) | The large gallery from this thread: emojis, links, emphasis, code, alignment, paragraphs, and a 30+ row table | 7 |
| [2026-05-14 renderer testing](/var/lib/codex/sessions/2026/05/14/rollout-2026-05-14T12-57-18-00000000-0000-0000-0000-000000000002.jsonl) | Explicit "markdown tables for testing" session with several successive assistant samples | 16 |
| [2026-05-14 five-table test](/var/lib/codex/sessions/2026/05/14/rollout-2026-05-14T12-27-57-00000000-0000-0000-0000-000000000003.jsonl) | Explicit request for five tables containing emojis, code, italics, and varied cell content | 10 |
"#;
        let narrow = render_markdown_text_with_width(input, Some(42));
        let records = lines_to_strings(&narrow);
        let counts: Vec<_> = records
            .iter()
            .filter(|line| line.starts_with("Detected table blocks:"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            counts,
            [
                "Detected table blocks: 7",
                "Detected table blocks: 16",
                "Detected table blocks: 10"
            ]
        );
        assert!(narrow.lines.iter().all(|line| line.width() <= 42));
        assert!(
            lines_to_strings(&render_markdown_text_with_width(input, Some(160)))
                .iter()
                .any(|line| line.contains('━'))
        );
    }

    #[test]
    fn wraps_plain_text_when_width_provided() {
        let markdown = "This is a simple sentence that should wrap.";
        let rendered = render_markdown_text_with_width(markdown, Some(16));
        let lines = lines_to_strings(&rendered);
        assert_eq!(
            lines,
            vec![
                "This is a simple".to_string(),
                "sentence that".to_string(),
                "should wrap.".to_string(),
            ]
        );
    }

    #[test]
    fn wraps_list_items_preserving_indent() {
        let markdown = "- first second third fourth";
        let rendered = render_markdown_text_with_width(markdown, Some(14));
        let lines = lines_to_strings(&rendered);
        assert_eq!(
            lines,
            vec!["- first second".to_string(), "  third fourth".to_string(),]
        );
    }

    #[test]
    fn wraps_nested_lists() {
        let markdown =
            "- outer item with several words to wrap\n  - inner item that also needs wrapping";
        let rendered = render_markdown_text_with_width(markdown, Some(20));
        let lines = lines_to_strings(&rendered);
        assert_eq!(
            lines,
            vec![
                "- outer item with".to_string(),
                "  several words to".to_string(),
                "  wrap".to_string(),
                "    - inner item".to_string(),
                "      that also".to_string(),
                "      needs wrapping".to_string(),
            ]
        );
    }

    #[test]
    fn wraps_ordered_lists() {
        let markdown = "1. ordered item contains many words for wrapping";
        let rendered = render_markdown_text_with_width(markdown, Some(18));
        let lines = lines_to_strings(&rendered);
        assert_eq!(
            lines,
            vec![
                "1. ordered item".to_string(),
                "   contains many".to_string(),
                "   words for".to_string(),
                "   wrapping".to_string(),
            ]
        );
    }

    #[test]
    fn wraps_blockquotes() {
        let markdown = "> block quote with content that should wrap nicely";
        let rendered = render_markdown_text_with_width(markdown, Some(22));
        let lines = lines_to_strings(&rendered);
        assert_eq!(
            lines,
            vec![
                "> block quote with".to_string(),
                "> content that should".to_string(),
                "> wrap nicely".to_string(),
            ]
        );
    }

    #[test]
    fn wraps_blockquotes_inside_lists() {
        let markdown = "- list item\n  > block quote inside list that wraps";
        let rendered = render_markdown_text_with_width(markdown, Some(24));
        let lines = lines_to_strings(&rendered);
        assert_eq!(
            lines,
            vec![
                "- list item".to_string(),
                "  > block quote inside".to_string(),
                "  > list that wraps".to_string(),
            ]
        );
    }

    #[test]
    fn wraps_list_items_containing_blockquotes() {
        let markdown = "1. item with quote\n   > quoted text that should wrap";
        let rendered = render_markdown_text_with_width(markdown, Some(24));
        let lines = lines_to_strings(&rendered);
        assert_eq!(
            lines,
            vec![
                "1. item with quote".to_string(),
                "   > quoted text that".to_string(),
                "   > should wrap".to_string(),
            ]
        );
    }

    #[test]
    fn does_not_wrap_code_blocks() {
        let markdown = "````\nfn main() { println!(\"hi from a long line\"); }\n````";
        let rendered = render_markdown_text_with_width(markdown, Some(10));
        let lines = lines_to_strings(&rendered);
        assert_eq!(
            lines,
            vec![
                "╭ code".to_string(),
                "│ fn main() { println!(\"hi from a long line\"); }".to_string(),
            ]
        );
    }
}
