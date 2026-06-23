//! Visual line layout for the plain-text body editor.
//!
//! The editor owns its own hard-wrapping so that cursor positioning and
//! rendering use the *same* algorithm and can never disagree — the previous
//! renderer derived the cursor row from newline counts while the text itself
//! was not wrapped at all, so long lines clipped and the cursor drifted.
//!
//! Wrapping is by character count (one column per `char`), matching the
//! editor's existing column convention. This is exact for prose; it does not
//! account for wide (CJK) glyphs or tab expansion, which the editor already
//! ignored.

/// One visual row produced by wrapping: a byte range `[start, end)` into the
/// source buffer. A trailing `'\n'` is never included in the range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualRow {
    pub start: usize,
    pub end: usize,
    /// `true` when this row ends a logical line (a hard `'\n'` or end of
    /// buffer), `false` when it ends at a soft-wrap boundary.
    pub hard: bool,
}

/// Wrap `text` into visual rows of at most `width` columns. `width` is clamped
/// to at least 1, and the result always contains at least one row (an empty
/// buffer yields a single empty row).
pub fn wrap(text: &str, width: usize) -> Vec<VisualRow> {
    let width = width.max(1);
    let mut rows = Vec::new();

    // Split into logical lines on '\n', tracking byte offsets. A trailing
    // newline (or empty input) yields a final empty logical line so the cursor
    // can rest on the blank line after it.
    let mut logical: Vec<(usize, usize)> = Vec::new();
    let mut line_start = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            logical.push((line_start, i));
            line_start = i + 1;
        }
    }
    logical.push((line_start, text.len()));

    for (ls, le) in logical {
        let mut row_start = ls;
        let mut col = 0usize;
        for (off, _ch) in text[ls..le].char_indices() {
            if col == width {
                let abs = ls + off;
                rows.push(VisualRow {
                    start: row_start,
                    end: abs,
                    hard: false,
                });
                row_start = abs;
                col = 0;
            }
            col += 1;
        }
        rows.push(VisualRow {
            start: row_start,
            end: le,
            hard: true,
        });
    }

    rows
}

/// Map a byte cursor position to its `(row, col)` in the wrapped layout. `col`
/// is a character count from the row start and may equal `width` when the
/// cursor sits at the end of a row that was filled exactly.
pub fn cursor_row_col(rows: &[VisualRow], text: &str, cursor: usize) -> (usize, usize) {
    for (ri, row) in rows.iter().enumerate() {
        let last = ri + 1 == rows.len();
        // The cursor belongs to this row if it falls inside the range, or sits
        // exactly at the end of a hard line / the final row. A cursor at a
        // soft-wrap boundary falls through to the next row, landing at column 0.
        if cursor < row.end || (cursor == row.end && (row.hard || last)) {
            let from = row.start.min(cursor);
            let col = text[from..cursor].chars().count();
            return (ri, col);
        }
    }
    (rows.len().saturating_sub(1), 0)
}

/// Clamp a vertical scroll offset so a viewport never starts past the end of
/// the rendered content. Empty content and zero-height viewports both clamp to
/// the top.
pub fn clamp_scroll(scroll: usize, content_rows: usize, viewport_rows: usize) -> usize {
    if content_rows == 0 || viewport_rows == 0 {
        return 0;
    }
    scroll.min(content_rows.saturating_sub(viewport_rows))
}

/// Move `scroll` only as much as needed to keep `cursor_row` visible in the
/// viewport, then clamp it to the current content height.
pub fn scroll_to_cursor(
    scroll: usize,
    cursor_row: usize,
    content_rows: usize,
    viewport_rows: usize,
) -> usize {
    if viewport_rows == 0 {
        return 0;
    }

    let mut next = scroll;
    if cursor_row < next {
        next = cursor_row;
    } else if cursor_row >= next + viewport_rows {
        next = cursor_row + 1 - viewport_rows;
    }

    clamp_scroll(next, content_rows, viewport_rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(text: &str, width: usize) -> Vec<String> {
        wrap(text, width)
            .iter()
            .map(|r| text[r.start..r.end].to_string())
            .collect()
    }

    #[test]
    fn empty_buffer_is_one_empty_row() {
        let rows = wrap("", 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(cursor_row_col(&rows, "", 0), (0, 0));
    }

    #[test]
    fn short_lines_pass_through() {
        assert_eq!(texts("a\nbb\nccc", 10), vec!["a", "bb", "ccc"]);
    }

    #[test]
    fn trailing_newline_yields_blank_row() {
        let text = "a\n";
        assert_eq!(texts(text, 10), vec!["a", ""]);
        // Cursor after the newline rests on the blank row.
        assert_eq!(cursor_row_col(&wrap(text, 10), text, text.len()), (1, 0));
    }

    #[test]
    fn hard_wraps_long_line_by_width() {
        assert_eq!(texts("abcdef", 3), vec!["abc", "def"]);
    }

    #[test]
    fn cursor_crosses_soft_wrap_boundary() {
        let text = "abcdef";
        let rows = wrap(text, 3);
        // Within the first row.
        assert_eq!(cursor_row_col(&rows, text, 1), (0, 1));
        // At the soft-wrap boundary -> start of the second row.
        assert_eq!(cursor_row_col(&rows, text, 3), (1, 0));
        // End of the wrapped line.
        assert_eq!(cursor_row_col(&rows, text, 6), (1, 3));
    }

    #[test]
    fn cursor_at_end_of_exactly_full_line() {
        // A logical line exactly `width` long is a single hard row; the cursor
        // at its end reports col == width (the renderer clamps for display).
        let text = "abc";
        let rows = wrap(text, 3);
        assert_eq!(rows.len(), 1);
        assert_eq!(cursor_row_col(&rows, text, 3), (0, 3));
    }

    #[test]
    fn multibyte_chars_count_as_one_column() {
        let text = "áéíóú"; // 5 chars, 10 bytes
        let rows = wrap(text, 3);
        // First row holds 3 chars, second holds 2.
        assert_eq!(rows.len(), 2);
        assert_eq!(texts(text, 3), vec!["áéí", "óú"]);
        assert_eq!(text[rows[0].start..rows[0].end].chars().count(), 3);
        // Cursor after the 4th char is on row 1, col 1.
        let pos: usize = text.chars().take(4).map(|c| c.len_utf8()).sum();
        assert_eq!(cursor_row_col(&rows, text, pos), (1, 1));
    }

    #[test]
    fn clamp_scroll_never_starts_past_content() {
        assert_eq!(clamp_scroll(10, 12, 5), 7);
        assert_eq!(clamp_scroll(10, 3, 5), 0);
        assert_eq!(clamp_scroll(10, 0, 5), 0);
        assert_eq!(clamp_scroll(10, 12, 0), 0);
    }

    #[test]
    fn scroll_to_cursor_only_moves_when_needed() {
        assert_eq!(scroll_to_cursor(4, 5, 20, 5), 4);
        assert_eq!(scroll_to_cursor(4, 3, 20, 5), 3);
        assert_eq!(scroll_to_cursor(4, 9, 20, 5), 5);
    }

    #[test]
    fn scroll_to_cursor_clamps_after_content_shrinks() {
        assert_eq!(scroll_to_cursor(10, 11, 12, 5), 7);
        assert_eq!(scroll_to_cursor(10, 2, 3, 5), 0);
    }
}
