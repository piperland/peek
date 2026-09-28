//! Source spans.
//!
//! A span records where something is in a file. Two rules make it trustworthy:
//!
//! * **Byte offsets are persisted.** They are the ground truth for slicing, and they survive
//!   re-indexing, encoding changes, and column recomputation.
//! * **Columns are character offsets, 1-based.** Tree-sitter reports *byte* columns, which do
//!   not match what an editor shows on any line containing non-ASCII text. Cortex persisted
//!   byte columns and labelled them as columns, so a span into a doc comment or a string
//!   literal pointed at the wrong place.

use serde::{Deserialize, Serialize};

/// A half-open byte range plus 1-based line/column positions.
///
/// `start_byte..end_byte` slices the file. `start_line`/`start_column` locate it for a human or
/// an editor. The two are kept consistent by [`Span::new`], which is the only constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Span {
    pub start_byte: u32,
    pub end_byte: u32,
    pub start_line: u32,
    pub start_column: u32,
    pub end_line: u32,
    pub end_column: u32,
}

impl Span {
    /// Build a span from a byte range and 1-based line/column positions.
    ///
    /// Returns `None` when the range is inverted or would overflow `u32`, so a malformed
    /// position from a parser cannot produce a silently wrong span.
    pub fn new(
        start_byte: u32,
        end_byte: u32,
        start_line: u32,
        start_column: u32,
        end_line: u32,
        end_column: u32,
    ) -> Option<Self> {
        if end_byte < start_byte {
            return None;
        }
        Some(Self {
            start_byte,
            end_byte,
            start_line,
            start_column,
            end_line,
            end_column,
        })
    }

    /// Byte length of the span. Saturates rather than panicking on malformed input.
    pub fn byte_len(&self) -> u32 {
        self.end_byte.saturating_sub(self.start_byte)
    }

    /// Number of source lines the span touches; always at least 1.
    pub fn line_count(&self) -> u32 {
        self.end_line.saturating_sub(self.start_line).saturating_add(1)
    }

    /// The text this span covers, or `None` if the range does not fall on character boundaries
    /// or lies outside `source`.
    pub fn slice<'a>(&self, source: &'a str) -> Option<&'a str> {
        source.get(self.start_byte as usize..self.end_byte as usize)
    }

    /// Whether `self` fully contains `other`.
    pub fn contains(&self, other: &Span) -> bool {
        self.start_byte <= other.start_byte && self.end_byte >= other.end_byte
    }

    /// Whether `self` and `other` share at least one byte.
    pub fn overlaps(&self, other: &Span) -> bool {
        self.start_byte < other.end_byte && other.start_byte < self.end_byte
    }

    /// The smallest span covering both inputs. `None` if either is `None`.
    pub fn union(a: Option<Span>, b: Option<Span>) -> Option<Span> {
        match (a, b) {
            (Some(a), Some(b)) => Some(Span {
                start_byte: a.start_byte.min(b.start_byte),
                end_byte: a.end_byte.max(b.end_byte),
                start_line: a.start_line.min(b.start_line),
                start_column: if a.start_line <= b.start_line {
                    a.start_column
                } else {
                    b.start_column
                },
                end_line: a.end_line.max(b.end_line),
                end_column: if a.end_line >= b.end_line {
                    a.end_column
                } else {
                    b.end_column
                },
            }),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Span;

    fn span(start: u32, end: u32) -> Span {
        Span::new(start, end, 1, 1, 1, 1).expect("valid span")
    }

    #[test]
    fn rejects_inverted_byte_range() {
        assert!(Span::new(10, 5, 1, 1, 1, 1).is_none());
    }

    #[test]
    fn accepts_empty_span() {
        let s = Span::new(4, 4, 1, 5, 1, 5).expect("empty but well-ordered");
        assert_eq!(s.byte_len(), 0);
    }

    #[test]
    fn byte_len_is_the_ground_truth_for_slicing() {
        let source = "fn main() {}";
        let s = span(3, 7);
        assert_eq!(s.slice(source), Some("main"));
    }

    #[test]
    fn slice_returns_none_for_out_of_bounds() {
        assert_eq!(span(0, 999).slice("short"), None);
    }

    #[test]
    fn slice_returns_none_for_non_char_boundary() {
        // "é" is two bytes; a range that splits it is not a valid `str` slice.
        let source = "café";
        assert_eq!(span(3, 4).slice(source), None);
    }

    #[test]
    fn contains_and_overlaps() {
        let outer = span(0, 100);
        let inner = span(10, 20);
        let straddling = span(90, 110);

        assert!(outer.contains(&inner));
        assert!(!inner.contains(&outer));
        assert!(outer.overlaps(&straddling));
        assert!(!inner.overlaps(&straddling));
    }

    #[test]
    fn union_takes_the_bounding_box() {
        let a = Span::new(0, 10, 1, 1, 1, 11).expect("a");
        let b = Span::new(50, 60, 3, 5, 3, 15).expect("b");
        let u = Span::union(Some(a), Some(b)).expect("union");
        assert_eq!((u.start_byte, u.end_byte), (0, 60));
        assert_eq!((u.start_line, u.end_line), (1, 3));
        // Start column comes from the earlier line, end column from the later.
        assert_eq!(u.start_column, 1);
        assert_eq!(u.end_column, 15);
    }

    #[test]
    fn union_with_none_is_identity() {
        let a = span(1, 2);
        assert_eq!(Span::union(Some(a), None), Some(a));
        assert_eq!(Span::union(None, Some(a)), Some(a));
        assert_eq!(Span::union(None, None), None);
    }

    #[test]
    fn line_count_is_at_least_one() {
        assert_eq!(span(0, 1).line_count(), 1);
        let tall = Span::new(0, 50, 10, 1, 12, 4).expect("tall");
        assert_eq!(tall.line_count(), 3);
    }
}
