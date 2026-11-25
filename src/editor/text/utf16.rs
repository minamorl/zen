//! Lossless conversion between UTF-8 byte and UTF-16 code-unit columns.
//!
//! Each scalar contributes a positive number of bytes and either one or two
//! UTF-16 units. Thus prefix lengths increase strictly in both encodings. The
//! prefix sums give an exact, monotone bijection between UTF-8 character
//! boundaries and UTF-16 scalar boundaries, and equal `encode_utf16().count()`
//! for the same prefix. The intermediate unit of a surrogate pair has no UTF-8
//! boundary and is rejected, as are columns outside the line and byte positions
//! inside a scalar. Text conversions inherit [`LineIndex`]'s line partition:
//! LF is excluded from content, its boundary belongs to the preceding line,
//! and CR is content. An index must be used with the text it was built from.

use super::lines::{LineError, LineIndex, LinePosition};
use super::utf8::{ByteOffset, Text};
use std::fmt;

/// Zero-based line and UTF-16 code-unit column, excluding LF.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineColUtf16 {
    /// Zero-based line number.
    pub line: usize,
    /// Zero-based UTF-16 code-unit column.
    pub col_utf16: usize,
}

/// An invalid column or text position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Utf16Error {
    /// A byte column exceeds the line length.
    ByteColumnOutOfRange { col_byte: usize, line_len: usize },
    /// A byte column lies inside a UTF-8 scalar.
    NotCharBoundary { col_byte: usize },
    /// A UTF-16 column exceeds the line length in code units.
    Utf16ColumnOutOfRange { col_utf16: usize, line_len: usize },
    /// A UTF-16 column lies between the two units of a surrogate pair.
    SurrogateInterior { col_utf16: usize },
    /// A line or text offset was rejected by the line index.
    Line(LineError),
}

impl fmt::Display for Utf16Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::ByteColumnOutOfRange { col_byte, line_len } => {
                write!(f, "byte column {col_byte} exceeds length {line_len}")
            }
            Self::NotCharBoundary { col_byte } => {
                write!(f, "byte column {col_byte} is not a character boundary")
            }
            Self::Utf16ColumnOutOfRange {
                col_utf16,
                line_len,
            } => write!(f, "UTF-16 column {col_utf16} exceeds length {line_len}"),
            Self::SurrogateInterior { col_utf16 } => {
                write!(f, "UTF-16 column {col_utf16} is inside a surrogate pair")
            }
            Self::Line(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Utf16Error {}

/// Converts a byte column in a line's content to UTF-16 code units.
pub fn byte_to_utf16(line: &str, col_byte: usize) -> Result<usize, Utf16Error> {
    if col_byte > line.len() {
        return Err(Utf16Error::ByteColumnOutOfRange {
            col_byte,
            line_len: line.len(),
        });
    }
    if !line.is_char_boundary(col_byte) {
        return Err(Utf16Error::NotCharBoundary { col_byte });
    }
    Ok(line[..col_byte].encode_utf16().count())
}

/// Converts a UTF-16 code-unit column in a line's content to bytes.
pub fn utf16_to_byte(line: &str, col_utf16: usize) -> Result<usize, Utf16Error> {
    let mut units = 0;
    for (byte, ch) in line.char_indices() {
        if units == col_utf16 {
            return Ok(byte);
        }
        let next = units + ch.len_utf16();
        if col_utf16 < next {
            return Err(Utf16Error::SurrogateInterior { col_utf16 });
        }
        units = next;
    }
    if col_utf16 == units {
        Ok(line.len())
    } else {
        Err(Utf16Error::Utf16ColumnOutOfRange {
            col_utf16,
            line_len: units,
        })
    }
}

/// Converts a text byte offset to a zero-based line and UTF-16 column.
pub fn offset_to_line_col(
    index: &LineIndex,
    text: &Text,
    offset: ByteOffset,
) -> Result<LineColUtf16, Utf16Error> {
    let pos = index
        .offset_to_line_col(text, offset)
        .map_err(Utf16Error::Line)?;
    let span = index.line_span(pos.line).map_err(Utf16Error::Line)?;
    let line = &text.as_str()[span.start().as_usize()..span.end().as_usize()];
    Ok(LineColUtf16 {
        line: pos.line,
        col_utf16: byte_to_utf16(line, pos.col_byte)?,
    })
}

/// Converts a zero-based line and UTF-16 column to a text byte offset.
pub fn line_col_to_offset(
    index: &LineIndex,
    text: &Text,
    pos: LineColUtf16,
) -> Result<ByteOffset, Utf16Error> {
    let span = index.line_span(pos.line).map_err(Utf16Error::Line)?;
    let line = &text.as_str()[span.start().as_usize()..span.end().as_usize()];
    let col_byte = utf16_to_byte(line, pos.col_utf16)?;
    index
        .line_col_to_offset(
            text,
            LinePosition {
                line: pos.line,
                col_byte,
            },
        )
        .map_err(Utf16Error::Line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};

    #[test]
    fn exhaustive_columns_and_text_roundtrips() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in
                corpus
                    .iter()
                    .map(String::as_str)
                    .chain(["", "abc", "\n\r\r\n", "é界e\u{301}😀𐀀\n"])
            {
                let text = Text::from_str(source);
                let index = LineIndex::from_text(&text);
                let mut start = 0;
                for (line_no, line) in source.split('\n').enumerate() {
                    let mut previous = None;
                    let mut expected = Vec::new();
                    for byte in 0..=line.len() {
                        if line.is_char_boundary(byte) {
                            let units = line[..byte].encode_utf16().count();
                            if let Some((prev_byte, prev_units)) = previous {
                                assert!(prev_byte < byte && prev_units < units);
                            }
                            previous = Some((byte, units));
                            expected.push((units, byte));
                            assert_eq!(byte_to_utf16(line, byte), Ok(units));
                            let offset = text.validate_offset(start + byte).unwrap();
                            let pos = LineColUtf16 {
                                line: line_no,
                                col_utf16: units,
                            };
                            assert_eq!(offset_to_line_col(&index, &text, offset), Ok(pos));
                            assert_eq!(line_col_to_offset(&index, &text, pos), Ok(offset));
                        } else {
                            assert_eq!(
                                byte_to_utf16(line, byte),
                                Err(Utf16Error::NotCharBoundary { col_byte: byte })
                            );
                        }
                    }
                    let len = line.encode_utf16().count();
                    for units in 0..=len {
                        match expected.iter().find(|&&(u, _)| u == units) {
                            Some(&(_, byte)) => assert_eq!(utf16_to_byte(line, units), Ok(byte)),
                            None => {
                                assert_eq!(
                                    utf16_to_byte(line, units),
                                    Err(Utf16Error::SurrogateInterior { col_utf16: units })
                                );
                                assert_eq!(
                                    line_col_to_offset(
                                        &index,
                                        &text,
                                        LineColUtf16 {
                                            line: line_no,
                                            col_utf16: units
                                        }
                                    ),
                                    Err(Utf16Error::SurrogateInterior { col_utf16: units })
                                );
                            }
                        }
                    }
                    assert_eq!(
                        utf16_to_byte(line, len + 1),
                        Err(Utf16Error::Utf16ColumnOutOfRange {
                            col_utf16: len + 1,
                            line_len: len
                        })
                    );
                    assert_eq!(
                        line_col_to_offset(
                            &index,
                            &text,
                            LineColUtf16 {
                                line: line_no,
                                col_utf16: len + 1,
                            },
                        ),
                        Err(Utf16Error::Utf16ColumnOutOfRange {
                            col_utf16: len + 1,
                            line_len: len,
                        })
                    );
                    assert_eq!(
                        byte_to_utf16(line, line.len() + 1),
                        Err(Utf16Error::ByteColumnOutOfRange {
                            col_byte: line.len() + 1,
                            line_len: line.len()
                        })
                    );
                    start += line.len() + 1;
                }
                let count = index.line_count();
                assert_eq!(
                    line_col_to_offset(
                        &index,
                        &text,
                        LineColUtf16 {
                            line: count,
                            col_utf16: 0
                        }
                    ),
                    Err(Utf16Error::Line(LineError::LineOutOfRange {
                        line: count,
                        line_count: count
                    }))
                );
                for offset in text.char_boundaries() {
                    let pos = offset_to_line_col(&index, &text, offset).unwrap();
                    assert_eq!(line_col_to_offset(&index, &text, pos), Ok(offset));
                }
            }
        });
    }

    #[test]
    fn foreign_offsets_are_revalidated() {
        let text = Text::from_str("界");
        let index = LineIndex::from_text(&text);
        let other = Text::from_str("abcd");
        assert_eq!(
            offset_to_line_col(&index, &text, other.validate_offset(1).unwrap()),
            Err(Utf16Error::Line(LineError::NotCharBoundary { offset: 1 }))
        );
        assert_eq!(
            offset_to_line_col(&index, &text, other.validate_offset(4).unwrap()),
            Err(Utf16Error::Line(LineError::OutOfBounds {
                offset: 4,
                len: 3
            }))
        );
    }
}
