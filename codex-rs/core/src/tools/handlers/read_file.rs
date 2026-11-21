use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use codex_protocol::models::FunctionCallOutputBody;
use codex_utils_string::take_bytes_at_char_boundary;
use serde::Deserialize;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;

pub struct ReadFileHandler;

const MAX_LINE_LENGTH: usize = 500;
const MAX_RETURN_LINES: usize = 2_000;
const MAX_INDENTATION_LINES: usize = 8_192;
const MAX_RESULT_BYTES: usize = 24 * 1024;
const METADATA_BUDGET: usize = 384;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const TAB_WIDTH: usize = 4;

// TODO(jif) add support for block comments
const COMMENT_PREFIXES: &[&str] = &["#", "//", "--"];

/// JSON arguments accepted by the `read_file` tool handler.
#[derive(Deserialize)]
struct ReadFileArgs {
    /// Absolute path to the file that will be read.
    file_path: String,
    /// 1-indexed line number to start reading from; defaults to 1.
    #[serde(default = "defaults::offset")]
    offset: usize,
    /// Maximum number of lines to return; defaults to 2000.
    #[serde(default = "defaults::limit")]
    limit: usize,
    /// Determines whether the handler reads a simple slice or indentation-aware block.
    #[serde(default)]
    mode: ReadMode,
    /// Optional indentation configuration used when `mode` is `Indentation`.
    #[serde(default)]
    indentation: Option<IndentationArgs>,
    /// Exact zero-based byte range; `limit` counts bytes in this mode.
    #[serde(default)]
    byte_offset: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum ReadMode {
    #[default]
    Slice,
    Indentation,
}
/// Additional configuration for indentation-aware reads.
#[derive(Deserialize, Clone)]
struct IndentationArgs {
    /// Optional explicit anchor line; defaults to `offset` when omitted.
    #[serde(default)]
    anchor_line: Option<usize>,
    /// Maximum indentation depth to collect; `0` means unlimited.
    #[serde(default = "defaults::max_levels")]
    max_levels: usize,
    /// Whether to include sibling blocks at the same indentation level.
    #[serde(default = "defaults::include_siblings")]
    include_siblings: bool,
    /// Whether to include header lines above the anchor block. This made on a best effort basis.
    #[serde(default = "defaults::include_header")]
    include_header: bool,
    /// Optional hard cap on returned lines; defaults to the global `limit`.
    #[serde(default)]
    max_lines: Option<usize>,
}

#[derive(Clone, Debug)]
struct LineRecord {
    number: usize,
    raw: String,
    display: String,
    indent: usize,
}

impl LineRecord {
    fn trimmed(&self) -> &str {
        self.raw.trim_start()
    }

    fn is_blank(&self) -> bool {
        self.trimmed().is_empty()
    }

    fn is_comment(&self) -> bool {
        COMMENT_PREFIXES
            .iter()
            .any(|prefix| self.raw.trim().starts_with(prefix))
    }
}

impl ToolHandler for ReadFileHandler {
    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError> {
        let ToolInvocation { payload, turn, .. } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "read_file handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: ReadFileArgs = parse_arguments(&arguments)?;

        let ReadFileArgs {
            file_path,
            offset,
            limit,
            mode,
            indentation,
            byte_offset,
        } = args;

        if !turn.sandbox_policy.has_full_disk_read_access() {
            return Err(FunctionCallError::RespondToModel(
                "read_file requires full read access; use a sandboxed tool instead".to_string(),
            ));
        }

        if offset == 0 {
            return Err(FunctionCallError::RespondToModel(
                "offset must be a 1-indexed line number".to_string(),
            ));
        }

        if limit == 0 {
            return Err(FunctionCallError::RespondToModel(
                "limit must be greater than zero".to_string(),
            ));
        }

        let path = PathBuf::from(&file_path);
        if !path.is_absolute() {
            return Err(FunctionCallError::RespondToModel(
                "file_path must be an absolute path".to_string(),
            ));
        }

        let mut budget = turn.truncation_policy.byte_budget().min(MAX_RESULT_BYTES);
        if turn.config.bounded_explorer {
            budget = budget.min(crate::agent::role::EXPLORER_RESULT_BYTES);
        }
        let payload_budget = budget.saturating_sub(METADATA_BUDGET);
        let before = file_revision(&path).await?;
        let (description, preview) = tokio::time::timeout(READ_TIMEOUT, async {
            if let Some(byte_offset) = byte_offset {
                return read_byte_range(&path, byte_offset, limit.min(payload_budget / 6)).await;
            }
            let line_limit = limit.min(MAX_RETURN_LINES);
            let (collected, cutoff) = match mode {
                ReadMode::Slice => {
                    let lines = slice::read(&path, offset, line_limit).await?;
                    let cutoff = (lines.len() == line_limit)
                        .then_some(" Line limit reached; subsequent lines not checked.");
                    (lines, cutoff)
                }
                ReadMode::Indentation => {
                    let indentation = indentation.unwrap_or_default();
                    let block =
                        indentation::read_block(&path, offset, line_limit, indentation).await?;
                    let cutoff = block.capped.then_some(
                        " Indentation line limit reached; additional adjacent lines not checked.",
                    );
                    (block.lines, cutoff)
                }
            };
            let mut preview = String::new();
            let mut returned = 0;
            for line in &collected {
                if preview.len() + line.len() + 1 > payload_budget {
                    break;
                }
                if !preview.is_empty() {
                    preview.push('\n');
                }
                preview.push_str(line);
                returned += 1;
            }
            let limited = if returned < collected.len() {
                " Further selected lines omitted (byte budget); narrow the read."
            } else {
                cutoff.unwrap_or_default()
            };
            Ok((
                format!("Source display: {returned} numbered lines.{limited}"),
                preview,
            ))
        })
        .await
        .map_err(|_| {
            FunctionCallError::RespondToModel(
                "read timed out; shorten the range or use byte_offset".to_string(),
            )
        })??;
        let after = file_revision(&path).await?;
        let header = if before == after {
            format!("{description}\nFile revision: {before}")
        } else {
            format!("{description}\nFile changed during read; revision unresolved.")
        };
        if turn.config.bounded_explorer {
            turn.exploration.observe(
                before != after
                    || header.len() > budget
                    || preview.contains("[partial/lossy line;")
                    || description.contains("omitted")
                    || description.contains("not checked"),
                (before == after).then_some(before.as_str()),
            );
        }
        Ok(ToolOutput::Function {
            body: FunctionCallOutputBody::Text(crate::truncate::fit_result(
                &header, &preview, budget,
            )),
            success: Some(before == after && header.len() <= budget),
        })
    }
}

fn read_error(err: io::Error) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!("failed to read file: {err}"))
}

async fn file_revision(path: &std::path::Path) -> Result<String, FunctionCallError> {
    let metadata = tokio::fs::metadata(path).await.map_err(read_error)?;
    if !metadata.is_file() {
        return Err(FunctionCallError::RespondToModel(
            "expected a regular file".to_string(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(format!(
            "{}:{}:{}:{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec()
        ))
    }
    #[cfg(not(unix))]
    {
        let modified = metadata.modified().map_err(read_error)?;
        Ok(format!("{}:{modified:?}", metadata.len()))
    }
}

async fn read_byte_range(
    path: &std::path::Path,
    offset: u64,
    limit: usize,
) -> Result<(String, String), FunctionCallError> {
    let mut file = tokio::fs::File::open(path).await.map_err(read_error)?;
    let length = file.metadata().await.map_err(read_error)?.len();
    if offset > length {
        return Err(FunctionCallError::RespondToModel(
            "byte offset exceeds file length".to_string(),
        ));
    }
    file.seek(io::SeekFrom::Start(offset))
        .await
        .map_err(read_error)?;
    let mut bytes = Vec::new();
    file.take(limit as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(read_error)?;
    let end = offset + bytes.len() as u64;
    let preview = super::encode_byte_range(bytes);
    Ok((
        format!("Source bytes {offset}..{end} of {length} (exclusive end)."),
        preview,
    ))
}

/// Consume a source line in fixed-size reads, retaining only a display prefix.
async fn read_bounded_line(
    reader: &mut (impl AsyncBufRead + Unpin),
    prefix: &mut Vec<u8>,
) -> io::Result<usize> {
    prefix.clear();
    let mut total = 0;
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Ok(total);
        }
        let newline = buf.iter().position(|&byte| byte == b'\n');
        let consumed = newline.map_or(buf.len(), |pos| pos + 1);
        let keep = consumed.min(MAX_LINE_LENGTH.saturating_sub(prefix.len()));
        prefix.extend_from_slice(&buf[..keep]);
        reader.consume(consumed);
        total += consumed;
        if newline.is_some() {
            return Ok(total);
        }
    }
}

mod slice {
    use crate::function_tool::FunctionCallError;
    use crate::tools::handlers::read_file::format_line;
    use crate::tools::handlers::read_file::read_bounded_line;
    use crate::tools::handlers::read_file::read_error;
    use std::path::Path;
    use tokio::fs::File;
    use tokio::io::BufReader;

    pub async fn read(
        path: &Path,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<String>, FunctionCallError> {
        let file = File::open(path).await.map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to read file: {err}"))
        })?;

        let mut reader = BufReader::new(file);
        let mut collected = Vec::new();
        let mut seen = 0usize;
        let mut buffer = Vec::new();
        let mut end = 0;

        loop {
            let bytes_read = read_bounded_line(&mut reader, &mut buffer)
                .await
                .map_err(read_error)?;

            if bytes_read == 0 {
                break;
            }

            let start = end;
            end += bytes_read;
            seen += 1;

            if seen < offset {
                continue;
            }

            let formatted = format_line(&buffer, start, bytes_read);
            collected.push(format!("L{seen}: {formatted}"));

            if collected.len() == limit {
                break;
            }
        }

        if seen < offset {
            return Err(FunctionCallError::RespondToModel(
                "offset exceeds file length".to_string(),
            ));
        }

        Ok(collected)
    }
}

mod indentation {
    use crate::function_tool::FunctionCallError;
    use crate::tools::handlers::read_file::IndentationArgs;
    use crate::tools::handlers::read_file::LineRecord;
    use crate::tools::handlers::read_file::MAX_INDENTATION_LINES;
    use crate::tools::handlers::read_file::TAB_WIDTH;
    use crate::tools::handlers::read_file::format_line;
    use crate::tools::handlers::read_file::read_bounded_line;
    use crate::tools::handlers::read_file::read_error;
    use crate::tools::handlers::read_file::trim_empty_lines;
    use std::collections::VecDeque;
    use std::path::Path;
    use tokio::fs::File;
    use tokio::io::BufReader;

    pub(super) struct Block {
        pub(super) lines: Vec<String>,
        pub(super) capped: bool,
    }

    pub async fn read_block(
        path: &Path,
        offset: usize,
        limit: usize,
        options: IndentationArgs,
    ) -> Result<Block, FunctionCallError> {
        let anchor_line = options.anchor_line.unwrap_or(offset);
        if anchor_line == 0 {
            return Err(FunctionCallError::RespondToModel(
                "anchor_line must be a 1-indexed line number".to_string(),
            ));
        }

        let guard_limit = options.max_lines.unwrap_or(limit);
        if guard_limit == 0 {
            return Err(FunctionCallError::RespondToModel(
                "max_lines must be greater than zero".to_string(),
            ));
        }

        let collected = collect_file_lines(path).await?;
        if collected.is_empty() || anchor_line > collected.len() {
            return Err(FunctionCallError::RespondToModel(
                "anchor_line exceeds file length".to_string(),
            ));
        }

        let anchor_index = anchor_line - 1;
        let effective_indents = compute_effective_indents(&collected);
        let anchor_indent = effective_indents[anchor_index];

        // Compute the min indent
        let min_indent = if options.max_levels == 0 {
            0
        } else {
            anchor_indent.saturating_sub(options.max_levels.saturating_mul(TAB_WIDTH))
        };

        // Cap requested lines by guard_limit and file length
        let final_limit = limit.min(guard_limit).min(collected.len());

        if final_limit == 1 {
            return Ok(Block {
                lines: vec![format!(
                    "L{}: {}",
                    collected[anchor_index].number, collected[anchor_index].display
                )],
                capped: collected.len() > final_limit,
            });
        }

        // Cursors
        let mut i: isize = anchor_index as isize - 1; // up (inclusive)
        let mut j: usize = anchor_index + 1; // down (inclusive)
        let mut i_counter_min_indent = 0;
        let mut j_counter_min_indent = 0;

        let mut out = VecDeque::with_capacity(final_limit);
        out.push_back(&collected[anchor_index]);

        while out.len() < final_limit {
            let mut progressed = 0;

            // Up.
            if i >= 0 {
                let iu = i as usize;
                if effective_indents[iu] >= min_indent {
                    out.push_front(&collected[iu]);
                    progressed += 1;
                    i -= 1;

                    // We do not include the siblings (not applied to comments).
                    if effective_indents[iu] == min_indent && !options.include_siblings {
                        let allow_header_comment =
                            options.include_header && collected[iu].is_comment();
                        let can_take_line = allow_header_comment || i_counter_min_indent == 0;

                        if can_take_line {
                            i_counter_min_indent += 1;
                        } else {
                            // This line shouldn't have been taken.
                            out.pop_front();
                            progressed -= 1;
                            i = -1; // consider using Option<usize> or a control flag instead of a sentinel
                        }
                    }

                    // Short-cut.
                    if out.len() >= final_limit {
                        break;
                    }
                } else {
                    // Stop moving up.
                    i = -1;
                }
            }

            // Down.
            if j < collected.len() {
                let ju = j;
                if effective_indents[ju] >= min_indent {
                    out.push_back(&collected[ju]);
                    progressed += 1;
                    j += 1;

                    // We do not include the siblings (applied to comments).
                    if effective_indents[ju] == min_indent && !options.include_siblings {
                        if j_counter_min_indent > 0 {
                            // This line shouldn't have been taken.
                            out.pop_back();
                            progressed -= 1;
                            j = collected.len();
                        }
                        j_counter_min_indent += 1;
                    }
                } else {
                    // Stop moving down.
                    j = collected.len();
                }
            }

            if progressed == 0 {
                break;
            }
        }

        // Record the boundary before trimming the selected blank lines.
        let capped = out.len() == final_limit && (i >= 0 || j < collected.len());
        trim_empty_lines(&mut out);

        let lines = out
            .into_iter()
            .map(|record| format!("L{}: {}", record.number, record.display))
            .collect();
        Ok(Block { lines, capped })
    }

    async fn collect_file_lines(path: &Path) -> Result<Vec<LineRecord>, FunctionCallError> {
        let file = File::open(path).await.map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to read file: {err}"))
        })?;

        let mut reader = BufReader::new(file);
        let mut buffer = Vec::new();
        let mut lines = Vec::new();
        let mut end = 0;

        loop {
            let bytes_read = read_bounded_line(&mut reader, &mut buffer)
                .await
                .map_err(read_error)?;

            if bytes_read == 0 {
                break;
            }
            if lines.len() == MAX_INDENTATION_LINES {
                return Err(FunctionCallError::RespondToModel(
                    "indentation scan exceeded its line limit; use slice or byte_offset"
                        .to_string(),
                ));
            }

            let display = format_line(&buffer, end, bytes_read);
            end += bytes_read;
            let raw = String::from_utf8_lossy(&buffer).into_owned();
            let indent = measure_indent(&raw);
            if bytes_read > buffer.len() && raw.trim().is_empty() {
                return Err(FunctionCallError::RespondToModel(
                    "indentation unresolved on an oversized line; use slice or byte_offset"
                        .to_string(),
                ));
            }
            lines.push(LineRecord {
                number: lines.len() + 1,
                raw,
                display,
                indent,
            });
        }

        Ok(lines)
    }

    fn compute_effective_indents(records: &[LineRecord]) -> Vec<usize> {
        let mut effective = Vec::with_capacity(records.len());
        let mut previous_indent = 0usize;
        for record in records {
            if record.is_blank() {
                effective.push(previous_indent);
            } else {
                previous_indent = record.indent;
                effective.push(previous_indent);
            }
        }
        effective
    }

    fn measure_indent(line: &str) -> usize {
        line.chars()
            .take_while(|c| matches!(c, ' ' | '\t'))
            .map(|c| if c == '\t' { TAB_WIDTH } else { 1 })
            .sum()
    }
}

fn format_line(bytes: &[u8], start: usize, source_len: usize) -> String {
    let display = bytes
        .strip_suffix(b"\r\n")
        .or_else(|| bytes.strip_suffix(b"\n"))
        .unwrap_or(bytes);
    let decoded = String::from_utf8_lossy(display);
    let prefix = take_bytes_at_char_boundary(&decoded, MAX_LINE_LENGTH);
    if source_len > bytes.len()
        || decoded.len() != prefix.len()
        || matches!(decoded, std::borrow::Cow::Owned(_))
    {
        let end = start + source_len;
        format!("{prefix} [partial/lossy line; exact source bytes {start}..{end} via byte_offset]")
    } else {
        prefix.to_string()
    }
}

fn trim_empty_lines(out: &mut VecDeque<&LineRecord>) {
    while matches!(out.front(), Some(line) if line.raw.trim().is_empty()) {
        out.pop_front();
    }
    while matches!(out.back(), Some(line) if line.raw.trim().is_empty()) {
        out.pop_back();
    }
}

mod defaults {
    use super::*;

    impl Default for IndentationArgs {
        fn default() -> Self {
            Self {
                anchor_line: None,
                max_levels: max_levels(),
                include_siblings: include_siblings(),
                include_header: include_header(),
                max_lines: None,
            }
        }
    }

    pub fn offset() -> usize {
        1
    }

    pub fn limit() -> usize {
        2000
    }

    pub fn max_levels() -> usize {
        0
    }

    pub fn include_siblings() -> bool {
        false
    }

    pub fn include_header() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::slice::read;
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::NamedTempFile;

    async fn read_block(
        path: &std::path::Path,
        offset: usize,
        limit: usize,
        options: IndentationArgs,
    ) -> Result<Vec<String>, FunctionCallError> {
        Ok(indentation::read_block(path, offset, limit, options)
            .await?
            .lines)
    }

    #[tokio::test]
    async fn reads_requested_range() -> anyhow::Result<()> {
        let mut temp = NamedTempFile::new()?;
        use std::io::Write as _;
        write!(
            temp,
            "alpha
beta
gamma
"
        )?;

        let lines = read(temp.path(), 2, 2).await?;
        assert_eq!(lines, vec!["L2: beta".to_string(), "L3: gamma".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn errors_when_offset_exceeds_length() -> anyhow::Result<()> {
        let mut temp = NamedTempFile::new()?;
        use std::io::Write as _;
        writeln!(temp, "only")?;

        let err = read(temp.path(), 3, 1)
            .await
            .expect_err("offset exceeds length");
        assert_eq!(
            err,
            FunctionCallError::RespondToModel("offset exceeds file length".to_string())
        );
        Ok(())
    }

    #[tokio::test]
    async fn reads_non_utf8_lines() -> anyhow::Result<()> {
        let mut temp = NamedTempFile::new()?;
        use std::io::Write as _;
        temp.as_file_mut().write_all(b"\xff\xfe\nplain\n")?;

        let lines = read(temp.path(), 1, 2).await?;
        let expected_first =
            "L1: \u{FFFD}\u{FFFD} [partial/lossy line; exact source bytes 0..3 via byte_offset]"
                .to_string();
        assert_eq!(lines, vec![expected_first, "L2: plain".to_string()]);
        assert_eq!(
            read_byte_range(temp.path(), 0, 3).await?,
            (
                "Source bytes 0..3 of 9 (exclusive end).".into(),
                r#"{"base64":"//4K"}"#.into(),
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn trims_crlf_endings() -> anyhow::Result<()> {
        let mut temp = NamedTempFile::new()?;
        use std::io::Write as _;
        write!(temp, "one\r\ntwo\r\n")?;

        let lines = read(temp.path(), 1, 2).await?;
        assert_eq!(lines, vec!["L1: one".to_string(), "L2: two".to_string()]);
        Ok(())
    }

    #[tokio::test]
    async fn respects_limit_even_with_more_lines() -> anyhow::Result<()> {
        let mut temp = NamedTempFile::new()?;
        use std::io::Write as _;
        write!(
            temp,
            "first
second
third
"
        )?;

        let lines = read(temp.path(), 1, 2).await?;
        assert_eq!(
            lines,
            vec!["L1: first".to_string(), "L2: second".to_string()]
        );
        Ok(())
    }

    #[tokio::test]
    async fn truncates_lines_longer_than_max_length() -> anyhow::Result<()> {
        let mut temp = NamedTempFile::new()?;
        use std::io::Write as _;
        let long_line = "x".repeat(MAX_LINE_LENGTH + 50);
        writeln!(temp, "{long_line}")?;

        let lines = read(temp.path(), 1, 1).await?;
        let expected = "x".repeat(MAX_LINE_LENGTH);
        assert_eq!(
            lines,
            vec![format!(
                "L1: {expected} [partial/lossy line; exact source bytes 0..551 via byte_offset]"
            )]
        );
        assert_eq!(
            read_byte_range(temp.path(), 545, 6).await?,
            (
                "Source bytes 545..551 of 551 (exclusive end).".into(),
                r#""xxxxx\n""#.into()
            )
        );
        Ok(())
    }

    #[tokio::test]
    async fn indentation_mode_captures_block() -> anyhow::Result<()> {
        let mut temp = NamedTempFile::new()?;
        use std::io::Write as _;
        write!(
            temp,
            "fn outer() {{
    if cond {{
        inner();
    }}
    tail();
}}
"
        )?;

        let options = IndentationArgs {
            anchor_line: Some(3),
            include_siblings: false,
            max_levels: 1,
            ..Default::default()
        };

        let lines = read_block(temp.path(), 3, 10, options).await?;

        assert_eq!(
            lines,
            vec![
                "L2:     if cond {".to_string(),
                "L3:         inner();".to_string(),
                "L4:     }".to_string()
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn indentation_mode_expands_parents() -> anyhow::Result<()> {
        let mut temp = NamedTempFile::new()?;
        use std::io::Write as _;
        write!(
            temp,
            "mod root {{
    fn outer() {{
        if cond {{
            inner();
        }}
    }}
}}
"
        )?;

        let mut options = IndentationArgs {
            anchor_line: Some(4),
            max_levels: 2,
            ..Default::default()
        };

        let lines = read_block(temp.path(), 4, 50, options.clone()).await?;
        assert_eq!(
            lines,
            vec![
                "L2:     fn outer() {".to_string(),
                "L3:         if cond {".to_string(),
                "L4:             inner();".to_string(),
                "L5:         }".to_string(),
                "L6:     }".to_string(),
            ]
        );

        options.max_levels = 3;
        let expanded = read_block(temp.path(), 4, 50, options).await?;
        assert_eq!(
            expanded,
            vec![
                "L1: mod root {".to_string(),
                "L2:     fn outer() {".to_string(),
                "L3:         if cond {".to_string(),
                "L4:             inner();".to_string(),
                "L5:         }".to_string(),
                "L6:     }".to_string(),
                "L7: }".to_string(),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn indentation_mode_respects_sibling_flag() -> anyhow::Result<()> {
        let mut temp = NamedTempFile::new()?;
        use std::io::Write as _;
        write!(
            temp,
            "fn wrapper() {{
    if first {{
        do_first();
    }}
    if second {{
        do_second();
    }}
}}
"
        )?;

        let mut options = IndentationArgs {
            anchor_line: Some(3),
            include_siblings: false,
            max_levels: 1,
            ..Default::default()
        };

        let lines = read_block(temp.path(), 3, 50, options.clone()).await?;
        assert_eq!(
            lines,
            vec![
                "L2:     if first {".to_string(),
                "L3:         do_first();".to_string(),
                "L4:     }".to_string(),
            ]
        );

        options.include_siblings = true;
        let with_siblings = read_block(temp.path(), 3, 50, options).await?;
        assert_eq!(
            with_siblings,
            vec![
                "L2:     if first {".to_string(),
                "L3:         do_first();".to_string(),
                "L4:     }".to_string(),
                "L5:     if second {".to_string(),
                "L6:         do_second();".to_string(),
                "L7:     }".to_string(),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn indentation_mode_handles_python_sample() -> anyhow::Result<()> {
        let mut temp = NamedTempFile::new()?;
        use std::io::Write as _;
        write!(
            temp,
            "class Foo:
    def __init__(self, size):
        self.size = size
    def double(self, value):
        if value is None:
            return 0
        result = value * self.size
        return result
class Bar:
    def compute(self):
        helper = Foo(2)
        return helper.double(5)
"
        )?;

        let options = IndentationArgs {
            anchor_line: Some(7),
            include_siblings: true,
            max_levels: 1,
            ..Default::default()
        };

        let lines = read_block(temp.path(), 1, 200, options).await?;
        assert_eq!(
            lines,
            vec![
                "L2:     def __init__(self, size):".to_string(),
                "L3:         self.size = size".to_string(),
                "L4:     def double(self, value):".to_string(),
                "L5:         if value is None:".to_string(),
                "L6:             return 0".to_string(),
                "L7:         result = value * self.size".to_string(),
                "L8:         return result".to_string(),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore]
    async fn indentation_mode_handles_javascript_sample() -> anyhow::Result<()> {
        let mut temp = NamedTempFile::new()?;
        use std::io::Write as _;
        write!(
            temp,
            "export function makeThing() {{
    const cache = new Map();
    function ensure(key) {{
        if (!cache.has(key)) {{
            cache.set(key, []);
        }}
        return cache.get(key);
    }}
    const handlers = {{
        init() {{
            console.log(\"init\");
        }},
        run() {{
            if (Math.random() > 0.5) {{
                return \"heads\";
            }}
            return \"tails\";
        }},
    }};
    return {{ cache, handlers }};
}}
export function other() {{
    return makeThing();
}}
"
        )?;

        let options = IndentationArgs {
            anchor_line: Some(15),
            max_levels: 1,
            ..Default::default()
        };

        let lines = read_block(temp.path(), 15, 200, options).await?;
        assert_eq!(
            lines,
            vec![
                "L10:         init() {".to_string(),
                "L11:             console.log(\"init\");".to_string(),
                "L12:         },".to_string(),
                "L13:         run() {".to_string(),
                "L14:             if (Math.random() > 0.5) {".to_string(),
                "L15:                 return \"heads\";".to_string(),
                "L16:             }".to_string(),
                "L17:             return \"tails\";".to_string(),
                "L18:         },".to_string(),
            ]
        );
        Ok(())
    }

    fn write_cpp_sample() -> anyhow::Result<NamedTempFile> {
        let mut temp = NamedTempFile::new()?;
        use std::io::Write as _;
        write!(
            temp,
            "#include <vector>
#include <string>

namespace sample {{
class Runner {{
public:
    void setup() {{
        if (enabled_) {{
            init();
        }}
    }}

    // Run the code
    int run() const {{
        switch (mode_) {{
            case Mode::Fast:
                return fast();
            case Mode::Slow:
                return slow();
            default:
                return fallback();
        }}
    }}

private:
    bool enabled_ = false;
    Mode mode_ = Mode::Fast;

    int fast() const {{
        return 1;
    }}
}};
}}  // namespace sample
"
        )?;
        Ok(temp)
    }

    #[tokio::test]
    async fn indentation_mode_handles_cpp_sample_shallow() -> anyhow::Result<()> {
        let temp = write_cpp_sample()?;

        let options = IndentationArgs {
            include_siblings: false,
            anchor_line: Some(18),
            max_levels: 1,
            ..Default::default()
        };

        let lines = read_block(temp.path(), 18, 200, options).await?;
        assert_eq!(
            lines,
            vec![
                "L15:         switch (mode_) {".to_string(),
                "L16:             case Mode::Fast:".to_string(),
                "L17:                 return fast();".to_string(),
                "L18:             case Mode::Slow:".to_string(),
                "L19:                 return slow();".to_string(),
                "L20:             default:".to_string(),
                "L21:                 return fallback();".to_string(),
                "L22:         }".to_string(),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn indentation_mode_handles_cpp_sample() -> anyhow::Result<()> {
        let temp = write_cpp_sample()?;

        let options = IndentationArgs {
            include_siblings: false,
            anchor_line: Some(18),
            max_levels: 2,
            ..Default::default()
        };

        let lines = read_block(temp.path(), 18, 200, options).await?;
        assert_eq!(
            lines,
            vec![
                "L13:     // Run the code".to_string(),
                "L14:     int run() const {".to_string(),
                "L15:         switch (mode_) {".to_string(),
                "L16:             case Mode::Fast:".to_string(),
                "L17:                 return fast();".to_string(),
                "L18:             case Mode::Slow:".to_string(),
                "L19:                 return slow();".to_string(),
                "L20:             default:".to_string(),
                "L21:                 return fallback();".to_string(),
                "L22:         }".to_string(),
                "L23:     }".to_string(),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn indentation_mode_handles_cpp_sample_no_headers() -> anyhow::Result<()> {
        let temp = write_cpp_sample()?;

        let options = IndentationArgs {
            include_siblings: false,
            include_header: false,
            anchor_line: Some(18),
            max_levels: 2,
            ..Default::default()
        };

        let lines = read_block(temp.path(), 18, 200, options).await?;
        assert_eq!(
            lines,
            vec![
                "L14:     int run() const {".to_string(),
                "L15:         switch (mode_) {".to_string(),
                "L16:             case Mode::Fast:".to_string(),
                "L17:                 return fast();".to_string(),
                "L18:             case Mode::Slow:".to_string(),
                "L19:                 return slow();".to_string(),
                "L20:             default:".to_string(),
                "L21:                 return fallback();".to_string(),
                "L22:         }".to_string(),
                "L23:     }".to_string(),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn indentation_mode_handles_cpp_sample_siblings() -> anyhow::Result<()> {
        let temp = write_cpp_sample()?;

        let options = IndentationArgs {
            include_siblings: true,
            include_header: false,
            anchor_line: Some(18),
            max_levels: 2,
            ..Default::default()
        };

        let lines = read_block(temp.path(), 18, 200, options).await?;
        assert_eq!(
            lines,
            vec![
                "L7:     void setup() {".to_string(),
                "L8:         if (enabled_) {".to_string(),
                "L9:             init();".to_string(),
                "L10:         }".to_string(),
                "L11:     }".to_string(),
                "L12: ".to_string(),
                "L13:     // Run the code".to_string(),
                "L14:     int run() const {".to_string(),
                "L15:         switch (mode_) {".to_string(),
                "L16:             case Mode::Fast:".to_string(),
                "L17:                 return fast();".to_string(),
                "L18:             case Mode::Slow:".to_string(),
                "L19:                 return slow();".to_string(),
                "L20:             default:".to_string(),
                "L21:                 return fallback();".to_string(),
                "L22:         }".to_string(),
                "L23:     }".to_string(),
            ]
        );
        Ok(())
    }
}
