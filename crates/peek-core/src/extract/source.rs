//! Source text and position mapping.
//!
//! # Why this exists
//!
//! Tree-sitter reports columns as **byte** offsets within a line. Cortex persisted those numbers
//! and labelled them as columns, so any span into a line containing non-ASCII text — a Rust `//!`
//! doc comment, a TypeScript string literal, a Python docstring — pointed at the wrong place.
//! Every editor and every LSP client that consumed those spans was off by however many
//! multi-byte characters preceded the token.
//!
//! [`SourceText`] keeps a line index and converts byte offsets into true **character** columns,
//! while the span still carries byte offsets for slicing. Both are needed: bytes to read the
//! source, characters to place a caret.

/// A source file with a precomputed line index.
#[derive(Debug, Clone)]
pub struct SourceText<'a> {
    text: &'a str,
    /// Byte offset at which each line begins. Always starts with `0`.
    line_starts: Vec<usize>,
}

impl<'a> SourceText<'a> {
    /// Index a source file.
    ///
    /// Handles `\n`, `\r\n` and a lone `\r` as line breaks, because a repository being indexed
    /// will contain all three and a span computed against the wrong convention is a span that
    /// points at the wrong line.
    pub fn new(text: &'a str) -> Self {
        let mut line_starts = vec![0usize];
        let bytes = text.as_bytes();
        let mut index = 0usize;
        while index < bytes.len() {
            match bytes[index] {
                b'\n' => {
                    line_starts.push(index + 1);
                    index += 1;
                }
                b'\r' => {
                    // Treat CRLF as a single break so the next line starts after both bytes.
                    let next = if index + 1 < bytes.len() && bytes[index + 1] == b'\n' {
                        index + 2
                    } else {
                        index + 1
                    };
                    line_starts.push(next);
                    index = next;
                }
                _ => index += 1,
            }
        }
        Self { text, line_starts }
    }

    /// The full source text.
    pub fn text(&self) -> &'a str {
        self.text
    }

    /// Number of lines. A file always has at least one.
    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    /// The text of a 1-based line, without its terminator.
    pub fn line_text(&self, line: usize) -> Option<&'a str> {
        if line == 0 || line > self.line_starts.len() {
            return None;
        }
        let start = self.line_starts[line - 1];
        let end = self
            .line_starts
            .get(line)
            .copied()
            .unwrap_or(self.text.len());
        self.text
            .get(start..end)
            .map(|line| line.trim_end_matches(['\n', '\r']))
    }

    /// The 0-based index of the line containing `offset`.
    fn line_index(&self, offset: usize) -> usize {
        match self.line_starts.binary_search(&offset) {
            Ok(exact) => exact,
            // `Err(i)` is the insertion point, so the containing line is one before it.
            Err(insertion) => insertion.saturating_sub(1),
        }
    }

    /// Build a span for a byte range, with true character columns.
    ///
    /// Returns `None` when the range is inverted, out of bounds, or does not fall on character
    /// boundaries. Tree-sitter always produces well-formed ranges, but a caller that hand-builds
    /// one should not be able to produce a span that silently slices through a multi-byte
    /// character.
    pub fn span(&self, start_byte: usize, end_byte: usize) -> Option<crate::model::Span> {
        if end_byte < start_byte || end_byte > self.text.len() {
            return None;
        }
        // Confirm both ends are character boundaries before trusting the range.
        self.text.get(start_byte..end_byte)?;

        let start_line = self.line_index(start_byte);
        let end_line = self.line_index(end_byte);
        let start_line_start = self.line_starts[start_line];
        let end_line_start = self.line_starts[end_line];

        crate::model::Span::new(
            u32::try_from(start_byte).ok()?,
            u32::try_from(end_byte).ok()?,
            u32::try_from(start_line + 1).ok()?,
            self.char_column(start_byte, start_line_start),
            u32::try_from(end_line + 1).ok()?,
            self.char_column(end_byte, end_line_start),
        )
    }

    /// 1-based character column of `offset`, counting from the start of `line_start`.
    fn char_column(&self, offset: usize, line_start: usize) -> u32 {
        let prefix = self.text.get(line_start..offset).unwrap_or_default();
        u32::try_from(prefix.chars().count())
            .unwrap_or(u32::MAX)
            .saturating_add(1)
    }
}

#[cfg(test)]
mod tests {
    use super::SourceText;

    #[test]
    fn indexes_lines_for_lf_crlf_and_lone_cr() {
        let source = SourceText::new("a\nb\r\nc\rd");
        assert_eq!(source.line_count(), 4);
        assert_eq!(source.line_text(1), Some("a"));
        assert_eq!(source.line_text(2), Some("b"));
        assert_eq!(source.line_text(3), Some("c"));
        assert_eq!(source.line_text(4), Some("d"));
    }

    #[test]
    fn rejects_out_of_range_lines() {
        let source = SourceText::new("one\ntwo");
        assert_eq!(source.line_text(0), None);
        assert_eq!(source.line_text(99), None);
    }

    #[test]
    fn empty_source_has_one_line() {
        let source = SourceText::new("");
        assert_eq!(source.line_count(), 1);
        assert_eq!(source.line_text(1), Some(""));
    }

    #[test]
    fn columns_are_characters_not_bytes() {
        // "é" is two bytes and "日本" is six. A byte-column implementation would report 3 here;
        // an editor shows 2.
        let source = SourceText::new("é日本");
        let span = source.span(0, "é".len()).expect("valid span");
        assert_eq!(span.start_line, 1);
        assert_eq!(span.start_column, 1);
        // The end of "é" is character 2, byte 2.
        assert_eq!(span.end_column, 2);
        // Byte offsets are still exact, which is what slicing needs.
        assert_eq!(span.end_byte as usize, "é".len());
    }

    #[test]
    fn column_resets_on_each_line() {
        let source = SourceText::new("héllo\nwörld");
        let second_line_start = "héllo\n".len();
        let span = source
            .span(second_line_start, second_line_start + "w".len())
            .expect("valid span");
        assert_eq!(span.start_line, 2);
        assert_eq!(span.start_column, 1);
        assert_eq!(span.end_line, 2);
        assert_eq!(span.end_column, 2);
    }

    #[test]
    fn spans_carry_one_based_lines() {
        let source = SourceText::new("fn a() {}\nfn b() {}\n");
        let span = source.span(10, 17).expect("valid span");
        assert_eq!(span.start_line, 2);
        assert_eq!(span.end_line, 2);
    }

    #[test]
    fn slices_exactly_the_covered_text() {
        let source = SourceText::new("fn main() { println!(\"héllo\"); }");
        let start = "fn main() { println!(\"".len();
        let end = start + "héllo".len();
        let span = source.span(start, end).expect("valid span");
        assert_eq!(span.slice(source.text()), Some("héllo"));
    }

    #[test]
    fn rejects_inverted_and_out_of_bounds_ranges() {
        let source = SourceText::new("hello");
        assert!(source.span(4, 2).is_none());
        assert!(source.span(0, 999).is_none());
    }

    #[test]
    fn rejects_a_range_that_splits_a_character() {
        let source = SourceText::new("é");
        assert!(source.span(0, 1).is_none(), "split a two-byte character");
    }
}
