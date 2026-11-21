use std::borrow::Cow;
use std::ops::Range;

use pulldown_cmark::CodeBlockKind;
use pulldown_cmark::Event;
use pulldown_cmark::Parser;
use pulldown_cmark::Tag;
use pulldown_cmark::TagEnd;

use super::MARKDOWN_OPTIONS;
use super::line_start;

fn is_markdown(info: &str) -> bool {
    info.split_whitespace().next().is_some_and(|language| {
        language.eq_ignore_ascii_case("md") || language.eq_ignore_ascii_case("markdown")
    })
}

fn contains_table(content: &str) -> bool {
    // The outer fence's container has already been removed from its parsed text.
    // A blockquote inside that text is a literal quoted Markdown example.
    let mut quote_depth = 0;
    Parser::new_ext(content, MARKDOWN_OPTIONS).any(|event| match event {
        Event::Start(Tag::BlockQuote) => {
            quote_depth += 1;
            false
        }
        Event::End(TagEnd::BlockQuote) => {
            quote_depth -= 1;
            false
        }
        Event::Start(Tag::Table(_)) => quote_depth == 0,
        _ => false,
    })
}

fn list_continuation_indent(line: &str) -> usize {
    // The parser has already identified a list item. Use its marker's width
    // and CommonMark's one-to-four columns of post-marker indentation.
    let marker = line.trim_start_matches([' ', '\t']);
    let leading = line.len() - marker.len();
    let marker_len = marker.bytes().take_while(u8::is_ascii_digit).count() + 1;
    let padding = marker[marker_len..]
        .bytes()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .fold(0, |spaces, byte| {
            if byte == b'\t' {
                spaces + 4 - (leading + marker_len + spaces) % 4
            } else {
                spaces + 1
            }
        });
    leading
        + marker_len
        + if (1..=4).contains(&padding) {
            padding
        } else {
            1
        }
}

/// Unwrap only explicitly closed Markdown fences whose body parses as a table.
/// Use the parser's fence boundaries so a table-like example in a code block stays literal.
pub(super) fn unwrap_table_fences(source: &str, streaming: bool) -> (Cow<'_, str>, Option<usize>) {
    if !source.contains("```") && !source.contains("~~~") {
        return (Cow::Borrowed(source), None);
    }
    struct Fence {
        opening: Range<usize>,
        body_end: usize,
        content: String,
    }

    let mut fence: Option<Fence> = None;
    let mut rewritten = None;
    let mut copied_until = 0;
    let mut mutable_from = None;
    let mut container_prefix = String::new();
    let mut prefix_lengths = Vec::new();
    for (event, range) in Parser::new_ext(source, MARKDOWN_OPTIONS).into_offset_iter() {
        match event {
            Event::Start(Tag::BlockQuote) => {
                prefix_lengths.push(container_prefix.len());
                container_prefix.push_str("> ");
            }
            Event::Start(Tag::Item) => {
                prefix_lengths.push(container_prefix.len());
                let line = source[range].split('\n').next().unwrap_or_default();
                let continuation = list_continuation_indent(line);
                container_prefix.extend(std::iter::repeat_n(' ', continuation));
            }
            Event::End(TagEnd::BlockQuote | TagEnd::Item) => {
                if let Some(len) = prefix_lengths.pop() {
                    container_prefix.truncate(len);
                }
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) if is_markdown(&info) => {
                let body_start = source[range.clone()]
                    .find('\n')
                    .map_or(range.end, |offset| range.start + offset + 1);
                fence = Some(Fence {
                    opening: range.start..body_start,
                    body_end: body_start,
                    content: String::new(),
                });
            }
            Event::Text(text) => {
                if let Some(fence) = fence.as_mut() {
                    fence.content.push_str(&text);
                    fence.body_end = range.end;
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                let Some(fence) = fence.take() else {
                    continue;
                };
                let closing = &source[fence.body_end..range.end];
                let Some(marker) = closing.find(['`', '~']) else {
                    if streaming && range.end == source.len() {
                        // Preview the literal fence, but don't commit it before we know
                        // whether its closing marker will turn it into a table.
                        let start = line_start(source, fence.opening.start);
                        let rewritten_len = rewritten.as_ref().map_or(0, String::len);
                        mutable_from = Some(rewritten_len + (start - copied_until));
                    }
                    continue;
                };
                if contains_table(&fence.content) {
                    let output =
                        rewritten.get_or_insert_with(|| String::with_capacity(source.len()));
                    output.push_str(&source[copied_until..fence.opening.start]);
                    output.push('\n');
                    // The parser has removed the fence's own indent. Put back
                    // only the enclosing list and quote continuation prefixes.
                    for line in fence.content.split_inclusive('\n') {
                        output.push_str(&container_prefix);
                        output.push_str(line);
                    }
                    output.push_str(&closing[..marker]);
                    copied_until = range.end;
                }
            }
            _ => {}
        }
    }

    if let Some(mut output) = rewritten {
        output.push_str(&source[copied_until..]);
        (Cow::Owned(output), mutable_from)
    } else {
        (Cow::Borrowed(source), mutable_from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn unwraps_only_closed_markdown_fences_with_tables() {
        let body = "|A|B|\n|---|---|\n|a|b|\n";
        assert_eq!(
            unwrap_table_fences(&format!("```md additional-info\n{body}```\n"), false).0,
            format!("\n{body}\n")
        );
        let quoted_body = body
            .lines()
            .map(|line| format!("> {line}\n"))
            .collect::<String>();
        let quoted = format!("> ~~~md\n{quoted_body}> ~~~~\n");
        assert_eq!(
            unwrap_table_fences(&quoted, false).0,
            format!("> \n{quoted_body}> \n")
        );
        for source in [
            format!("```text\n{body}```\n"),
            format!("```markdown\n{quoted_body}```\n"),
            format!("```markdown\n{body}"),
            "```markdown\n# Example\n```\n".to_string(),
            format!("````markdown\n```text\n{body}```\n````\n"),
        ] {
            assert_eq!(unwrap_table_fences(&source, false).0, source);
        }
    }

    #[test]
    fn uses_the_parsed_indent_for_fenced_tables() {
        for prefix in ["", "> "] {
            let source: String = "  ```md\n    |K|V|\n    |---|---|\n    |1|2|\n  ```\n"
                .split_inclusive('\n')
                .map(|line| format!("{prefix}{line}"))
                .collect();
            let (normalized, _) = unwrap_table_fences(&source, false);
            assert!(
                Parser::new_ext(&normalized, MARKDOWN_OPTIONS)
                    .any(|event| matches!(event, Event::Start(Tag::Table(_)))),
                "{normalized:?}"
            );
        }
        for source in [
            "- Item\n\n    ```md\n      |K|V|\n      |---|---|\n      |1|2|\n    ```\n",
            "> - Item\n>\n>     ```md\n>       |K|V|\n>       |---|---|\n>       |1|2|\n>     ```\n",
        ] {
            let (normalized, _) = unwrap_table_fences(source, false);
            let events: Vec<_> = Parser::new_ext(&normalized, MARKDOWN_OPTIONS).collect();
            assert!(
                events
                    .iter()
                    .any(|ev| matches!(ev, Event::Start(Tag::Table(_))))
                    && !events
                        .iter()
                        .any(|ev| matches!(ev, Event::Start(Tag::CodeBlock(_)))),
                "{normalized:?}"
            );
        }
    }
}
