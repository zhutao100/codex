# TUI Markdown tables

Assistant replies, proposed plans, and review text render GitHub-flavored pipe tables as aligned columns. The source needs a header and a delimiter row, but no padding. Sparse rows retain their column labels; a blank line separates ordinary prose from table rows. Closed fences labeled `md` or `markdown` unwrap actual tables, not quoted Markdown examples inside the fence. Other code fences stay literal. Copy and export retain the original Markdown.

The [streaming collector](../../tui/src/markdown_stream.rs) tracks committed source positions rather than visual line counts. Possible headers, open tables, and their separators stay out of scrollback; completed table rows appear in the live viewport. Unresolved links and unclosed inline code also preview there, and open Markdown fences preview as code until closed.

Cells treat decoded CRLF as one line break, including across parser events. Width allocation, grapheme wrapping and padding use terminal cells: `ｶﾞﾊﾟ` occupies four. List continuations start under the first header cell, including after `10. `. Grid layout reserves compact values before wrapping prose: at 24 content columns a ten-cell date stays intact beside eight cells of notes. A long path can wrap in a readable grid; cramped layouts switch to labeled records. A cramped header-only table retains its pipe syntax.

Pending history retains its table structure until the inline draw that inserts it, whether or not the transcript overlay was open. Layout uses that draw's terminal width, and insertion does not wrap rows that already fit. Native scrollback does not redraw tables already inserted.

For layout and finalization regressions, run the `markdown` tests in `codex-tui`. The reply fixtures check the live widget buffer and the neighboring cell's physical column in VT100 scrollback; string widths alone do not check either display path.
