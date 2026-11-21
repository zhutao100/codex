use crate::markdown_render::MarkdownBlock;
use crate::markdown_render::SourceBlock;
use crate::markdown_render::render_markdown_blocks;
use crate::render::line_utils::is_blank_line_spaces_only;

/// Newline-gated accumulator for Markdown destined for append-only history.
pub(crate) struct MarkdownStreamCollector {
    buffer: String,
    committed_through: Option<usize>,
    preview: Vec<MarkdownBlock>,
    width: Option<usize>,
    unwrap_fences: bool,
}

impl MarkdownStreamCollector {
    pub fn new(width: Option<usize>, unwrap_fences: bool) -> Self {
        Self {
            buffer: String::new(),
            committed_through: None,
            preview: Vec::new(),
            width,
            unwrap_fences,
        }
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.committed_through = None;
        self.preview.clear();
    }

    pub fn push_delta(&mut self, delta: &str) {
        tracing::trace!("push_delta: {delta:?}");
        self.buffer.push_str(delta);
    }

    /// Keep any unfinished table out of history until stable.
    pub fn commit_complete_blocks(&mut self) -> Vec<MarkdownBlock> {
        let Some(last_newline_idx) = self.buffer.rfind('\n') else {
            return Vec::new();
        };
        let mut rendered = render_markdown_blocks(
            &self.buffer[..=last_newline_idx],
            self.width,
            /*streaming*/ true,
            self.unwrap_fences,
        );
        if let Some(SourceBlock {
            block: MarkdownBlock::Line(line),
            ..
        }) = rendered.blocks.last()
            && is_blank_line_spaces_only(line)
        {
            rendered.blocks.pop();
        }
        self.preview.clear();
        let through = self.committed_through;
        let mut out = Vec::new();
        for entry in rendered.blocks {
            if through.is_some_and(|start| entry.start <= start) {
                continue;
            }
            if rendered
                .mutable_from
                .is_some_and(|start| entry.start >= start)
            {
                self.preview.push(entry.block);
            } else {
                self.committed_through = self.committed_through.max(Some(entry.start));
                out.push(entry.block);
            }
        }
        out
    }

    pub(crate) fn preview(&self) -> &[MarkdownBlock] {
        &self.preview
    }

    /// Finalize the stream: emit all remaining blocks beyond the last commit.
    /// If the buffer does not end with a newline, a temporary one is appended for rendering.
    pub fn finalize_and_drain(&mut self) -> Vec<MarkdownBlock> {
        let raw_len = self.buffer.len();
        let mut source = std::mem::take(&mut self.buffer);
        if !source.ends_with('\n') {
            source.push('\n');
        }
        tracing::debug!(raw_len, source_len = source.len(), "finalizing markdown");
        tracing::trace!("markdown finalize (raw source):\n---\n{source}\n---");

        let rendered = render_markdown_blocks(
            &source,
            self.width,
            /*streaming*/ false,
            self.unwrap_fences,
        );
        let out = rendered
            .blocks
            .into_iter()
            .filter(|entry| {
                self.committed_through
                    .is_none_or(|start| entry.start > start)
            })
            .map(|entry| entry.block)
            .collect();

        // Reset collector state for next stream.
        self.clear();
        out
    }
}

#[cfg(test)]
use ratatui::text::Line;

#[cfg(test)]
fn rendered_lines(blocks: Vec<MarkdownBlock>, width: Option<usize>) -> Vec<Line<'static>> {
    blocks
        .into_iter()
        .flat_map(|block| block.into_lines(width))
        .collect()
}

#[cfg(test)]
pub(crate) fn simulate_stream_markdown_for_tests(
    deltas: &[&str],
    finalize: bool,
) -> Vec<Line<'static>> {
    let mut collector = MarkdownStreamCollector::new(None, /*unwrap_fences*/ true);
    let mut out = Vec::new();
    for d in deltas {
        collector.push_delta(d);
        if d.contains('\n') {
            out.extend(rendered_lines(collector.commit_complete_blocks(), None));
        }
    }
    if finalize {
        out.extend(rendered_lines(collector.finalize_and_drain(), None));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::style::Color;

    #[test]
    fn table_is_committed_once_its_widths_are_stable() {
        let width = Some(24);
        let mut collector = MarkdownStreamCollector::new(width, /*unwrap_fences*/ true);
        let preamble = "Summary:\n";
        collector.push_delta(preamble);
        let mut lines = rendered_lines(collector.commit_complete_blocks(), width);
        assert_eq!(lines, [Line::from("Summary:")]);

        // The header immediately after prose is a table once its delimiter arrives.
        let table = [
            "|Item|Qty|\n",
            "|---|---:|\n",
            "|a|2|\n",
            "|longer label|120|\n",
        ];
        for delta in table {
            collector.push_delta(delta);
            assert!(collector.commit_complete_blocks().is_empty());
        }
        collector.push_delta("\n");
        let committed_table = collector.commit_complete_blocks();
        assert!(matches!(
            committed_table.as_slice(),
            [MarkdownBlock::Line(_), MarkdownBlock::Table(_)]
        ));
        lines.extend(rendered_lines(committed_table, width));
        collector.push_delta("Done.");
        lines.extend(rendered_lines(collector.finalize_and_drain(), width));
        let expected = crate::markdown_render::render_markdown_text_with_width(
            &format!("{preamble}{}\nDone.\n", table.concat()),
            width,
        );
        assert_eq!(lines, expected.lines);
    }

    #[test]
    fn possible_table_header_in_quote_does_not_drop_prose() {
        let deltas = ["> first\n> A | B\n", "> last\n"];
        let expected =
            crate::markdown_render::render_markdown_text_with_width(&deltas.concat(), Some(24));
        let mut collector = MarkdownStreamCollector::new(Some(24), /*unwrap_fences*/ true);
        collector.push_delta(deltas[0]);
        let mut lines = rendered_lines(collector.commit_complete_blocks(), Some(24));
        assert_eq!(lines, expected.lines[..1]);
        collector.push_delta(deltas[1]);
        lines.extend(rendered_lines(collector.commit_complete_blocks(), Some(24)));
        lines.extend(rendered_lines(collector.finalize_and_drain(), Some(24)));
        assert_eq!(lines, expected.lines);
    }

    #[test]
    fn multiline_inline_code_before_table_keeps_the_table() {
        let deltas = ["`a\nb\nc\n", "d`\n\n|K|V|\n|---|---|\n|1|2|\n"];
        let width = Some(38);
        let mut collector = MarkdownStreamCollector::new(width, /*unwrap_fences*/ true);
        let mut blocks = Vec::new();
        for delta in deltas {
            collector.push_delta(delta);
            blocks.extend(collector.commit_complete_blocks());
        }
        blocks.extend(collector.finalize_and_drain());
        let expected =
            crate::markdown_render::render_markdown_text_with_width(&deltas.concat(), width);
        assert_eq!(rendered_lines(blocks, width), expected.lines);
    }

    #[test]
    fn nested_quoted_table_is_not_skipped_after_its_separator() {
        let deltas = ["> Intro\n>\n> > |K|V|\n", "> > |---|---|\n> > |1|2|\n"];
        let source = deltas.concat();
        let expected = crate::markdown_render::render_markdown_text_with_width(&source, None);
        assert_eq!(
            simulate_stream_markdown_for_tests(&deltas, true),
            expected.lines
        );
        assert_eq!(
            simulate_stream_markdown_for_tests(&[&source], true),
            expected.lines
        );
    }

    #[test]
    fn closed_table_with_unresolved_reference_stays_out_of_history() {
        let table_and_prose = "Intro\n\n|Name|Status|\n|---|---|\n|[Alice][ref]|OK|\n\nAfter.\n\n";
        let definition = "[ref]: https://example.com";
        let expected = crate::markdown_render::render_markdown_text_with_width(
            &format!("{table_and_prose}{definition}\n"),
            None,
        );
        let mut collector = MarkdownStreamCollector::new(None, /*unwrap_fences*/ true);
        collector.push_delta(table_and_prose);
        let mut lines = rendered_lines(collector.commit_complete_blocks(), None);
        assert_eq!(lines, [Line::from("Intro")]);
        collector.push_delta(definition);
        lines.extend(rendered_lines(collector.finalize_and_drain(), None));
        assert_eq!(lines, expected.lines);
    }

    #[test]
    fn late_reference_before_table_does_not_duplicate_it() {
        let width = Some(20);
        for reference in ["[r]", "## [r]", "> [r]"] {
            let preamble = format!("Intro\n\n{reference}\n\n");
            let table = "|Key|Value|\n|---|---|\n|one|two|\n\n";
            let definition = "[r]: https://example.com/a/long/link/destination\n";
            let mut collector = MarkdownStreamCollector::new(width, /*unwrap_fences*/ true);
            collector.push_delta(&preamble);
            let mut lines = rendered_lines(collector.commit_complete_blocks(), width);
            collector.push_delta(table);
            lines.extend(rendered_lines(collector.commit_complete_blocks(), width));
            assert_eq!(
                lines
                    .iter()
                    .filter(|line| line.width() != 0)
                    .collect::<Vec<_>>(),
                [&Line::from("Intro")]
            );

            collector.push_delta(definition);
            let mut rest = collector.commit_complete_blocks();
            rest.extend(collector.finalize_and_drain());
            assert_eq!(
                rest.iter()
                    .filter(|block| matches!(block, MarkdownBlock::Table(_)))
                    .count(),
                1
            );
            lines.extend(rendered_lines(rest, width));
            let source = format!("{preamble}{table}{definition}");
            let expected = crate::markdown_render::render_markdown_text_with_width(&source, width);
            assert_eq!(lines, expected.lines);
        }
    }

    #[tokio::test]
    async fn no_commit_until_newline() {
        let mut c = MarkdownStreamCollector::new(None, /*unwrap_fences*/ true);
        c.push_delta("Hello, world");
        let out = c.commit_complete_blocks();
        assert!(out.is_empty(), "should not commit without newline");
        c.push_delta("!\n");
        let out2 = c.commit_complete_blocks();
        assert_eq!(out2.len(), 1, "one completed line after newline");
    }

    #[tokio::test]
    async fn finalize_commits_partial_line() {
        let mut c = MarkdownStreamCollector::new(None, /*unwrap_fences*/ true);
        c.push_delta("Line without newline");
        let out = c.finalize_and_drain();
        assert_eq!(out.len(), 1);
    }

    #[tokio::test]
    async fn e2e_stream_blockquote_simple_is_green() {
        let out = super::simulate_stream_markdown_for_tests(&["> Hello\n"], true);
        assert_eq!(out.len(), 1);
        let l = &out[0];
        assert_eq!(
            l.style.fg,
            Some(Color::Green),
            "expected blockquote line fg green, got {:?}",
            l.style.fg
        );
    }

    #[tokio::test]
    async fn e2e_stream_blockquote_nested_is_green() {
        let out = super::simulate_stream_markdown_for_tests(&["> Level 1\n>> Level 2\n"], true);
        // Filter out any blank lines that may be inserted at paragraph starts.
        let non_blank: Vec<_> = out
            .into_iter()
            .filter(|l| {
                let s = l
                    .spans
                    .iter()
                    .map(|sp| sp.content.clone())
                    .collect::<Vec<_>>()
                    .join("");
                let t = s.trim();
                // Ignore quote-only blank lines like ">" inserted at paragraph boundaries.
                !(t.is_empty() || t == ">")
            })
            .collect();
        assert_eq!(non_blank.len(), 2);
        assert_eq!(non_blank[0].style.fg, Some(Color::Green));
        assert_eq!(non_blank[1].style.fg, Some(Color::Green));
    }

    #[tokio::test]
    async fn e2e_stream_blockquote_with_list_items_is_green() {
        let out = super::simulate_stream_markdown_for_tests(&["> - item 1\n> - item 2\n"], true);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].style.fg, Some(Color::Green));
        assert_eq!(out[1].style.fg, Some(Color::Green));
    }

    #[tokio::test]
    async fn e2e_stream_nested_mixed_lists_ordered_marker_is_light_blue() {
        let md = [
            "1. First\n",
            "   - Second level\n",
            "     1. Third level (ordered)\n",
            "        - Fourth level (bullet)\n",
            "          - Fifth level to test indent consistency\n",
        ];
        let out = super::simulate_stream_markdown_for_tests(&md, true);
        // Find the line that contains the third-level ordered text
        let find_idx = out.iter().position(|l| {
            l.spans
                .iter()
                .map(|s| s.content.clone())
                .collect::<String>()
                .contains("Third level (ordered)")
        });
        let idx = find_idx.expect("expected third-level ordered line");
        let line = &out[idx];
        // Expect at least one span on this line to be styled light blue
        let has_light_blue = line
            .spans
            .iter()
            .any(|s| s.style.fg == Some(ratatui::style::Color::LightBlue));
        assert!(
            has_light_blue,
            "expected an ordered-list marker span with light blue fg on: {line:?}"
        );
    }

    #[tokio::test]
    async fn e2e_stream_blockquote_wrap_preserves_green_style() {
        let long = "> This is a very long quoted line that should wrap across multiple columns to verify style preservation.";
        let out = super::simulate_stream_markdown_for_tests(&[long, "\n"], true);
        // Wrap to a narrow width to force multiple output lines.
        let wrapped =
            crate::wrapping::word_wrap_lines(out.iter(), crate::wrapping::RtOptions::new(24));
        // Filter out purely blank lines
        let non_blank: Vec<_> = wrapped
            .into_iter()
            .filter(|l| {
                let s = l
                    .spans
                    .iter()
                    .map(|sp| sp.content.clone())
                    .collect::<Vec<_>>()
                    .join("");
                !s.trim().is_empty()
            })
            .collect();
        assert!(
            non_blank.len() >= 2,
            "expected wrapped blockquote to span multiple lines"
        );
        for (i, l) in non_blank.iter().enumerate() {
            assert_eq!(
                l.spans[0].style.fg,
                Some(Color::Green),
                "wrapped line {} should preserve green style, got {:?}",
                i,
                l.spans[0].style.fg
            );
        }
    }

    #[tokio::test]
    async fn heading_starts_on_new_line_when_following_paragraph() {
        // Stream a paragraph line, then a heading on the next line.
        // Expect two distinct rendered lines: "Hello." and "Heading".
        let mut c = MarkdownStreamCollector::new(None, /*unwrap_fences*/ true);
        c.push_delta("Hello.\n");
        let out1 = rendered_lines(c.commit_complete_blocks(), None);
        let s1: Vec<String> = out1
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect();
        assert_eq!(
            out1.len(),
            1,
            "first commit should contain only the paragraph line, got {}: {:?}",
            out1.len(),
            s1
        );

        c.push_delta("## Heading\n");
        let out2 = rendered_lines(c.commit_complete_blocks(), None);
        let s2: Vec<String> = out2
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect();
        assert_eq!(
            s2,
            vec!["", "## Heading"],
            "expected a blank separator then the heading line"
        );

        let line_to_string = |l: &ratatui::text::Line<'_>| -> String {
            l.spans
                .iter()
                .map(|s| s.content.clone())
                .collect::<Vec<_>>()
                .join("")
        };

        assert_eq!(line_to_string(&out1[0]), "Hello.");
        assert_eq!(line_to_string(&out2[1]), "## Heading");
    }

    #[tokio::test]
    async fn heading_not_inlined_when_split_across_chunks() {
        // Paragraph without trailing newline, then a chunk that starts with the newline
        // and the heading text, then a final newline. The collector should first commit
        // only the paragraph line, and later commit the heading as its own line.
        let mut c = MarkdownStreamCollector::new(None, /*unwrap_fences*/ true);
        c.push_delta("Sounds good!");
        // No commit yet
        assert!(c.commit_complete_blocks().is_empty());

        // Introduce the newline that completes the paragraph and the start of the heading.
        c.push_delta("\n## Adding Bird subcommand");
        let out1 = rendered_lines(c.commit_complete_blocks(), None);
        let s1: Vec<String> = out1
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect();
        assert_eq!(
            s1,
            vec!["Sounds good!"],
            "expected paragraph followed by blank separator before heading chunk"
        );

        // Now finish the heading line with the trailing newline.
        c.push_delta("\n");
        let out2 = rendered_lines(c.commit_complete_blocks(), None);
        let s2: Vec<String> = out2
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect();
        assert_eq!(
            s2,
            vec!["", "## Adding Bird subcommand"],
            "expected the heading line only on the final commit"
        );

        // Sanity check raw markdown rendering for a simple line does not produce spurious extras.
        let mut rendered: Vec<ratatui::text::Line<'static>> = Vec::new();
        crate::markdown::append_markdown("Hello.\n", None, &mut rendered);
        let rendered_strings: Vec<String> = rendered
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect();
        assert_eq!(
            rendered_strings,
            vec!["Hello."],
            "unexpected markdown lines: {rendered_strings:?}"
        );
    }

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
    async fn lists_and_fences_commit_without_duplication() {
        // List case
        assert_streamed_equals_full(&["- a\n- ", "b\n- c\n"]).await;

        // Fenced code case: stream in small chunks
        assert_streamed_equals_full(&["```", "\nco", "de 1\ncode 2\n", "```\n"]).await;
    }

    #[tokio::test]
    async fn utf8_boundary_safety_and_wide_chars() {
        // Emoji (wide), CJK, control char, digit + combining macron sequences
        let input = "🙂🙂🙂\n汉字漢字\nA\u{0003}0\u{0304}\n";
        let deltas = vec![
            "🙂",
            "🙂",
            "🙂\n汉",
            "字漢",
            "字\nA",
            "\u{0003}",
            "0",
            "\u{0304}",
            "\n",
        ];

        let streamed = simulate_stream_markdown_for_tests(&deltas, true);
        let streamed_str = lines_to_plain_strings(&streamed);

        let mut rendered_all: Vec<ratatui::text::Line<'static>> = Vec::new();
        crate::markdown::append_markdown(input, None, &mut rendered_all);
        let rendered_all_str = lines_to_plain_strings(&rendered_all);

        assert_eq!(
            streamed_str, rendered_all_str,
            "utf8/wide-char streaming should equal full render without duplication or truncation"
        );
    }

    #[tokio::test]
    async fn e2e_stream_deep_nested_third_level_marker_is_light_blue() {
        let md = "1. First\n   - Second level\n     1. Third level (ordered)\n        - Fourth level (bullet)\n          - Fifth level to test indent consistency\n";
        let streamed = super::simulate_stream_markdown_for_tests(&[md], true);
        let streamed_strs = lines_to_plain_strings(&streamed);

        // Locate the third-level line in the streamed output; avoid relying on exact indent.
        let target_suffix = "1. Third level (ordered)";
        let mut found = None;
        for line in &streamed {
            let s: String = line.spans.iter().map(|sp| sp.content.clone()).collect();
            if s.contains(target_suffix) {
                found = Some(line.clone());
                break;
            }
        }
        let line = found.unwrap_or_else(|| {
            panic!("expected to find the third-level ordered list line; got: {streamed_strs:?}")
        });

        // The marker (including indent and "1.") is expected to be in the first span
        // and colored LightBlue; following content should be default color.
        assert!(
            !line.spans.is_empty(),
            "expected non-empty spans for the third-level line"
        );
        let marker_span = &line.spans[0];
        assert_eq!(
            marker_span.style.fg,
            Some(Color::LightBlue),
            "expected LightBlue 3rd-level ordered marker, got {:?}",
            marker_span.style.fg
        );
        // Find the first non-empty non-space content span and verify it is default color.
        let mut content_fg = None;
        for sp in &line.spans[1..] {
            let t = sp.content.trim();
            if !t.is_empty() {
                content_fg = Some(sp.style.fg);
                break;
            }
        }
        assert_eq!(
            content_fg.flatten(),
            None,
            "expected default color for 3rd-level content, got {content_fg:?}"
        );
    }

    #[tokio::test]
    async fn empty_fenced_block_is_dropped_and_separator_preserved_before_heading() {
        // An empty fenced code block followed by a heading should not render the fence,
        // but should preserve a blank separator line so the heading starts on a new line.
        let deltas = vec!["```bash\n```\n", "## Heading\n"]; // empty block and close in same commit
        let streamed = simulate_stream_markdown_for_tests(&deltas, true);
        let texts = lines_to_plain_strings(&streamed);
        assert!(
            texts.iter().all(|s| !s.contains("```")),
            "no fence markers expected: {texts:?}"
        );
        // Expect the heading and no fence markers. A blank separator may or may not be rendered at start.
        assert!(
            texts.iter().any(|s| s == "## Heading"),
            "expected heading line: {texts:?}"
        );
    }

    #[tokio::test]
    async fn paragraph_then_empty_fence_then_heading_keeps_heading_on_new_line() {
        let deltas = vec!["Para.\n", "```\n```\n", "## Title\n"]; // empty fence block in one commit
        let streamed = simulate_stream_markdown_for_tests(&deltas, true);
        let texts = lines_to_plain_strings(&streamed);
        let para_idx = match texts.iter().position(|s| s == "Para.") {
            Some(i) => i,
            None => panic!("para present"),
        };
        let head_idx = match texts.iter().position(|s| s == "## Title") {
            Some(i) => i,
            None => panic!("heading present"),
        };
        assert!(
            head_idx > para_idx,
            "heading should not merge with paragraph: {texts:?}"
        );
    }

    #[tokio::test]
    async fn loose_list_with_split_dashes_matches_full_render() {
        // Minimized failing sequence discovered by the helper: two chunks
        // that still reproduce the mismatch.
        let deltas = vec!["- item.\n\n", "-"];

        let streamed = simulate_stream_markdown_for_tests(&deltas, true);
        let streamed_strs = lines_to_plain_strings(&streamed);

        let full: String = deltas.iter().copied().collect();
        let mut rendered_all: Vec<ratatui::text::Line<'static>> = Vec::new();
        crate::markdown::append_markdown(&full, None, &mut rendered_all);
        let rendered_all_strs = lines_to_plain_strings(&rendered_all);

        assert_eq!(
            streamed_strs, rendered_all_strs,
            "streamed output should match full render without dangling '-' lines"
        );
    }

    #[tokio::test]
    async fn loose_vs_tight_list_items_streaming_matches_full() {
        // Deltas extracted from the session log around 2025-08-27T00:33:18.216Z
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

        let streamed = simulate_stream_markdown_for_tests(&deltas, true);
        let streamed_strs = lines_to_plain_strings(&streamed);

        // Compute a full render for diagnostics only.
        let full: String = deltas.iter().copied().collect();
        let mut rendered_all: Vec<ratatui::text::Line<'static>> = Vec::new();
        crate::markdown::append_markdown(&full, None, &mut rendered_all);

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
            streamed_strs, expected,
            "expected exact rendered lines for loose/tight section"
        );
    }

    // Targeted tests derived from fuzz findings. Each asserts streamed == full render.
    async fn assert_streamed_equals_full(deltas: &[&str]) {
        let streamed = simulate_stream_markdown_for_tests(deltas, true);
        let streamed_strs = lines_to_plain_strings(&streamed);
        let full: String = deltas.iter().copied().collect();
        let mut rendered: Vec<ratatui::text::Line<'static>> = Vec::new();
        crate::markdown::append_markdown(&full, None, &mut rendered);
        let rendered_strs = lines_to_plain_strings(&rendered);
        assert_eq!(streamed_strs, rendered_strs, "full:\n---\n{full}\n---");
    }

    #[tokio::test]
    async fn fuzz_class_bullet_duplication_variant_1() {
        assert_streamed_equals_full(&[
            "aph.\n- let one\n- bull",
            "et two\n\n  second paragraph \n",
        ])
        .await;
    }

    #[tokio::test]
    async fn fuzz_class_bullet_duplication_variant_2() {
        assert_streamed_equals_full(&[
            "- e\n  c",
            "e\n- bullet two\n\n  second paragraph in bullet two\n",
        ])
        .await;
    }

    #[tokio::test]
    async fn streaming_html_block_then_text_matches_full() {
        assert_streamed_equals_full(&[
            "HTML block:\n",
            "<div>inline block</div>\n",
            "more stuff\n",
        ])
        .await;
    }
}
