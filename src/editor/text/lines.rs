//! Line indexing over UTF-8 bytes. LF separates lines; CR is ordinary content.
//!
//! An index must be used with the text from which it was built. Every line has
//! a start and a content end, and an LF (if present) lies at that end. Starts
//! partition the valid boundaries: a boundary at LF belongs to the preceding
//! line, while the boundary after it is the next start. Subtracting the start
//! gives its column, and adding it back recovers the boundary. Conversely,
//! every valid column is within exactly that line's content, so the two
//! conversions are a bijection on valid boundaries and positions.
//!
//! Scanning bytes for LF yields the same starts and ends as an independent
//! linear scan: each LF terminates exactly one line, and the final suffix is
//! always another line, even when empty. This includes CR, multibyte scalars,
//! combining marks, and emoji, since none of their UTF-8 bytes is LF. Invalid
//! line numbers and columns beyond the content end are rejected before
//! conversion; text validation rejects offsets inside UTF-8 scalars or beyond
//! the text.

use super::span::ByteSpan;
use super::utf8::{ByteOffset, Text, Utf8Error};
use std::fmt;

/// A zero-based line and byte column within its content, excluding LF.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LinePosition {
    /// Zero-based line number.
    pub line: usize,
    /// Zero-based byte column.
    pub col_byte: usize,
}

/// A failure to address a line or a UTF-8 boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineError {
    /// The line does not exist.
    LineOutOfRange { line: usize, line_count: usize },
    /// The column exceeds the line's content length.
    ColumnOutOfRange {
        line: usize,
        col_byte: usize,
        line_len: usize,
    },
    /// The offset lies inside a UTF-8 scalar.
    NotCharBoundary { offset: usize },
    /// The offset exceeds the text length.
    OutOfBounds { offset: usize, len: usize },
}

impl fmt::Display for LineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::LineOutOfRange { line, line_count } => {
                write!(f, "line {line} out of range for {line_count} lines")
            }
            Self::ColumnOutOfRange {
                line,
                col_byte,
                line_len,
            } => write!(f, "column {col_byte} exceeds line {line} length {line_len}"),
            Self::NotCharBoundary { offset } => {
                write!(f, "offset {offset} is not a character boundary")
            }
            Self::OutOfBounds { offset, len } => write!(f, "offset {offset} exceeds length {len}"),
        }
    }
}

impl std::error::Error for LineError {}

fn validate(text: &Text, offset: usize) -> Result<ByteOffset, LineError> {
    text.validate_offset(offset).map_err(|error| match error {
        Utf8Error::OutOfBounds { offset, len } => LineError::OutOfBounds { offset, len },
        Utf8Error::NotCharBoundary { offset } => LineError::NotCharBoundary { offset },
        _ => unreachable!("offset validation only returns offset errors"),
    })
}

/// Immutable line ranges for the text supplied to [`LineIndex::from_text`].
#[derive(Clone, Debug)]
pub struct LineIndex {
    lines: Vec<(ByteSpan, ByteSpan)>,
}

impl LineIndex {
    /// Builds an index, including the final empty line after a trailing LF.
    pub fn from_text(text: &Text) -> Self {
        let mut lines = Vec::new();
        let mut start = 0;
        for (end, byte) in text.as_bytes().iter().enumerate() {
            if *byte == b'\n' {
                let span =
                    ByteSpan::validate_in(text, start, end).expect("LF boundaries are valid");
                let with_lf =
                    ByteSpan::validate_in(text, start, end + 1).expect("LF boundaries are valid");
                lines.push((span, with_lf));
                start = end + 1;
            }
        }
        let final_span =
            ByteSpan::validate_in(text, start, text.len_bytes()).expect("text endpoints are valid");
        lines.push((final_span, final_span));
        Self { lines }
    }

    /// Returns the number of lines, always at least one.
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn line(&self, line: usize) -> Result<(ByteSpan, ByteSpan), LineError> {
        self.lines
            .get(line)
            .copied()
            .ok_or(LineError::LineOutOfRange {
                line,
                line_count: self.line_count(),
            })
    }

    /// Returns the content span, excluding a trailing LF.
    pub fn line_span(&self, line: usize) -> Result<ByteSpan, LineError> {
        Ok(self.line(line)?.0)
    }

    /// Returns the content span including its trailing LF, if present.
    pub fn line_span_with_lf(&self, line: usize) -> Result<ByteSpan, LineError> {
        Ok(self.line(line)?.1)
    }

    /// Maps a text boundary to its line and byte column. The LF belongs to its preceding line.
    pub fn offset_to_line_col(
        &self,
        text: &Text,
        offset: ByteOffset,
    ) -> Result<LinePosition, LineError> {
        let offset = validate(text, offset.as_usize())?.as_usize();
        let line = self
            .lines
            .partition_point(|(span, _)| span.start().as_usize() <= offset)
            - 1;
        Ok(LinePosition {
            line,
            col_byte: offset - self.lines[line].0.start().as_usize(),
        })
    }

    /// Maps a valid line and byte column to a text boundary.
    pub fn line_col_to_offset(
        &self,
        text: &Text,
        pos: LinePosition,
    ) -> Result<ByteOffset, LineError> {
        let span = self.line_span(pos.line)?;
        let line_len = span.len_bytes();
        if pos.col_byte > line_len {
            return Err(LineError::ColumnOutOfRange {
                line: pos.line,
                col_byte: pos.col_byte,
                line_len,
            });
        }
        validate(text, span.start().as_usize() + pos.col_byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};

    // Independent byte-by-byte scan, including the final suffix.
    fn scan(source: &str) -> Vec<(usize, usize, usize)> {
        let mut result = Vec::new();
        let mut start = 0;
        for (i, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                result.push((start, i, i + 1));
                start = i + 1;
            }
        }
        result.push((start, source.len(), source.len()));
        result
    }

    #[test]
    fn reference_scan_and_bijection() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus
                .iter()
                .map(String::as_str)
                .chain(["\n", "\n\n", "a\n\nb\n"])
            {
                let text = Text::from_str(source);
                let index = LineIndex::from_text(&text);
                let expected = scan(source);
                assert_eq!(index.line_count(), expected.len());
                for (line, &(start, end, with_lf)) in expected.iter().enumerate() {
                    let span = index.line_span(line).unwrap();
                    let full = index.line_span_with_lf(line).unwrap();
                    assert_eq!(
                        (span.start().as_usize(), span.end().as_usize()),
                        (start, end)
                    );
                    assert_eq!(
                        (full.start().as_usize(), full.end().as_usize()),
                        (start, with_lf)
                    );
                    for col_byte in 0..=end - start {
                        let pos = LinePosition { line, col_byte };
                        let offset = start + col_byte;
                        if source.is_char_boundary(offset) {
                            let actual = index.line_col_to_offset(&text, pos).unwrap();
                            assert_eq!(actual.as_usize(), offset);
                            assert_eq!(index.offset_to_line_col(&text, actual), Ok(pos));
                        } else {
                            assert_eq!(
                                index.line_col_to_offset(&text, pos),
                                Err(LineError::NotCharBoundary { offset })
                            );
                        }
                    }
                    assert_eq!(
                        index.line_col_to_offset(
                            &text,
                            LinePosition {
                                line,
                                col_byte: end - start + 1
                            }
                        ),
                        Err(LineError::ColumnOutOfRange {
                            line,
                            col_byte: end - start + 1,
                            line_len: end - start
                        })
                    );
                }
                for offset in text.char_boundaries() {
                    let n = offset.as_usize();
                    let line = expected
                        .iter()
                        .position(|&(start, end, _)| start <= n && n <= end)
                        .unwrap();
                    let pos = LinePosition {
                        line,
                        col_byte: n - expected[line].0,
                    };
                    assert_eq!(index.offset_to_line_col(&text, offset), Ok(pos));
                    assert_eq!(index.line_col_to_offset(&text, pos), Ok(offset));
                }
                let bad = expected.len();
                let error = Err(LineError::LineOutOfRange {
                    line: bad,
                    line_count: bad,
                });
                assert_eq!(index.line_span(bad), error);
                assert_eq!(index.line_span_with_lf(bad), error);
                assert_eq!(
                    index.line_col_to_offset(
                        &text,
                        LinePosition {
                            line: bad,
                            col_byte: 0
                        }
                    ),
                    Err(LineError::LineOutOfRange {
                        line: bad,
                        line_count: bad
                    })
                );
            }
        });
    }

    #[test]
    fn foreign_offsets_are_revalidated() {
        let text = Text::from_str("界");
        let index = LineIndex::from_text(&text);
        let ascii = Text::from_str("abcd");
        assert_eq!(
            index.offset_to_line_col(&text, ascii.validate_offset(1).unwrap()),
            Err(LineError::NotCharBoundary { offset: 1 })
        );
        assert_eq!(
            index.offset_to_line_col(&text, ascii.validate_offset(4).unwrap()),
            Err(LineError::OutOfBounds { offset: 4, len: 3 })
        );
    }
}
