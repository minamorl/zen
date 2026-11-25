//! Lossless conversion between UTF-8 byte offsets and Neovim coordinates.
//!
//! `LineColByte` uses a zero-based line and byte column for buffer text APIs;
//! `OneBasedLineColByte` uses a one-based line and zero-based byte column for
//! marks. A line ends immediately before LF or at EOF; CR is line content.
//!
//! # Laws and proofs
//!
//! * **Bijection:** LF splits the text into disjoint line content ranges.
//!   Each boundary belongs to exactly one range, including its end: the boundary
//!   immediately before LF (at the LF's offset) is the preceding line's end,
//!   while the boundary after LF is the next line's start. Subtracting the start
//!   gives the unique column, and
//!   adding it back recovers the boundary. Adding one to the line and then
//!   subtracting one gives the same bijection for marks, including empty text
//!   and the empty final line after a trailing LF.
//! * **Order:** within a line, adding its fixed start preserves column order.
//!   Every boundary on an earlier line precedes every boundary on a later
//!   line, so byte order is exactly lexicographic line/column order for both
//!   coordinate origins.
//! * **Rejection:** offsets are revalidated against the supplied text. Lines
//!   and columns outside its line ranges and `ByteOffset` or `BytePos` beyond
//!   the text produce `OutOfRange`; a byte inside a UTF-8 scalar produces
//!   `InvalidBoundary`. These checks precede use of
//!   the resulting offset, including for offsets validated against other text.

use super::coordinates::{BytePos, LineColByte, OneBasedLineColByte};
use super::error::{BoundaryError, ErrorCode};
use crate::editor::text::lines::{LineError, LineIndex, LinePosition};
use crate::editor::text::utf8::{ByteOffset, Text, Utf8Error};

fn line_error(error: LineError) -> BoundaryError {
    let code = match error {
        LineError::NotCharBoundary { .. } => ErrorCode::InvalidBoundary,
        _ => ErrorCode::OutOfRange,
    };
    BoundaryError::new(code, error.to_string())
}

fn validate_byte(text: &Text, raw: usize) -> Result<ByteOffset, BoundaryError> {
    text.validate_offset(raw).map_err(|error| {
        let code = match error {
            Utf8Error::NotCharBoundary { .. } => ErrorCode::InvalidBoundary,
            Utf8Error::OutOfBounds { .. } => ErrorCode::OutOfRange,
            _ => unreachable!("offset validation only checks range and boundaries"),
        };
        BoundaryError::new(code, error.to_string())
    })
}

/// Converts a byte offset to a zero-based line and byte column.
pub fn byte_to_line_col(text: &Text, offset: ByteOffset) -> Result<LineColByte, BoundaryError> {
    let pos = LineIndex::from_text(text)
        .offset_to_line_col(text, offset)
        .map_err(line_error)?;
    Ok(LineColByte::new(pos.line, pos.col_byte))
}

/// Converts a zero-based line and byte column to a byte offset.
pub fn line_col_to_byte(text: &Text, pos: LineColByte) -> Result<ByteOffset, BoundaryError> {
    LineIndex::from_text(text)
        .line_col_to_offset(
            text,
            LinePosition {
                line: pos.line(),
                col_byte: pos.col_byte(),
            },
        )
        .map_err(line_error)
}

/// Converts a raw byte position to a zero-based line and byte column.
pub fn byte_pos_to_line_col(text: &Text, pos: BytePos) -> Result<LineColByte, BoundaryError> {
    byte_to_line_col(text, validate_byte(text, pos.raw())?)
}

/// Converts a zero-based line and byte column to a raw byte position.
pub fn line_col_to_byte_pos(text: &Text, pos: LineColByte) -> Result<BytePos, BoundaryError> {
    line_col_to_byte(text, pos).map(|offset| BytePos::new(offset.as_usize()))
}

/// Converts a byte offset to a one-based mark line and zero-based byte column.
pub fn byte_to_mark(text: &Text, offset: ByteOffset) -> Result<OneBasedLineColByte, BoundaryError> {
    let pos = byte_to_line_col(text, offset)?;
    OneBasedLineColByte::new(pos.line() + 1, pos.col_byte())
}

/// Converts a mark line and byte column to a byte offset.
pub fn mark_to_byte(text: &Text, pos: OneBasedLineColByte) -> Result<ByteOffset, BoundaryError> {
    line_col_to_byte(
        text,
        LineColByte::new(pos.zero_based_line(), pos.col_byte()),
    )
}

/// Converts a raw byte position to a mark position.
pub fn byte_pos_to_mark(text: &Text, pos: BytePos) -> Result<OneBasedLineColByte, BoundaryError> {
    byte_to_mark(text, validate_byte(text, pos.raw())?)
}

/// Converts a mark position to a raw byte position.
pub fn mark_to_byte_pos(text: &Text, pos: OneBasedLineColByte) -> Result<BytePos, BoundaryError> {
    mark_to_byte(text, pos).map(|offset| BytePos::new(offset.as_usize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};

    #[test]
    fn corpus_bijection_order_and_rejection() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus.iter().map(String::as_str).chain([
                "",
                "\n",
                "\n\n",
                "abc",
                "a\rb\r\nc\n",
                "界e\u{301}😀\n",
            ]) {
                let text = Text::from_str(source);
                let foreign = Text::from_str(&"a".repeat(source.len()));
                let mut positions = Vec::new();
                let mut start = 0;
                for (line, content) in source.split('\n').enumerate() {
                    for col in 0..=content.len() {
                        let raw = start + col;
                        let pos = LineColByte::new(line, col);
                        let mark = OneBasedLineColByte::new(line + 1, col).unwrap();
                        if source.is_char_boundary(raw) {
                            let offset = text.validate_offset(raw).unwrap();
                            assert_eq!(byte_to_line_col(&text, offset), Ok(pos));
                            assert_eq!(line_col_to_byte(&text, pos), Ok(offset));
                            assert_eq!(byte_to_mark(&text, offset), Ok(mark));
                            assert_eq!(mark_to_byte(&text, mark), Ok(offset));
                            assert_eq!(byte_pos_to_line_col(&text, BytePos::new(raw)), Ok(pos));
                            assert_eq!(line_col_to_byte_pos(&text, pos), Ok(BytePos::new(raw)));
                            assert_eq!(byte_pos_to_mark(&text, BytePos::new(raw)), Ok(mark));
                            assert_eq!(mark_to_byte_pos(&text, mark), Ok(BytePos::new(raw)));
                            positions.push((raw, pos, mark));
                        } else {
                            let foreign_offset = foreign.validate_offset(raw).unwrap();
                            assert_eq!(
                                byte_to_line_col(&text, foreign_offset).unwrap_err().code(),
                                ErrorCode::InvalidBoundary
                            );
                            assert_eq!(
                                byte_to_mark(&text, foreign_offset).unwrap_err().code(),
                                ErrorCode::InvalidBoundary
                            );
                            assert_eq!(
                                line_col_to_byte(&text, pos).unwrap_err().code(),
                                ErrorCode::InvalidBoundary
                            );
                            assert_eq!(
                                mark_to_byte(&text, mark).unwrap_err().code(),
                                ErrorCode::InvalidBoundary
                            );
                            assert_eq!(
                                byte_pos_to_line_col(&text, BytePos::new(raw))
                                    .unwrap_err()
                                    .code(),
                                ErrorCode::InvalidBoundary
                            );
                            assert_eq!(
                                byte_pos_to_mark(&text, BytePos::new(raw))
                                    .unwrap_err()
                                    .code(),
                                ErrorCode::InvalidBoundary
                            );
                        }
                    }
                    for col in [content.len() + 1, usize::MAX] {
                        assert_eq!(
                            line_col_to_byte(&text, LineColByte::new(line, col))
                                .unwrap_err()
                                .code(),
                            ErrorCode::OutOfRange
                        );
                        assert_eq!(
                            mark_to_byte(&text, OneBasedLineColByte::new(line + 1, col).unwrap())
                                .unwrap_err()
                                .code(),
                            ErrorCode::OutOfRange
                        );
                    }
                    start += content.len() + 1;
                }
                assert_eq!(positions.len(), text.char_boundaries().len());
                for &(a, pos_a, mark_a) in &positions {
                    for &(b, pos_b, mark_b) in &positions {
                        assert_eq!(a < b, pos_a < pos_b);
                        assert_eq!(a < b, mark_a < mark_b);
                    }
                }
                for line in [source.split('\n').count(), usize::MAX] {
                    assert_eq!(
                        line_col_to_byte(&text, LineColByte::new(line, 0))
                            .unwrap_err()
                            .code(),
                        ErrorCode::OutOfRange
                    );
                }
                for line in [source.split('\n').count() + 1, usize::MAX] {
                    assert_eq!(
                        mark_to_byte(&text, OneBasedLineColByte::new(line, 0).unwrap())
                            .unwrap_err()
                            .code(),
                        ErrorCode::OutOfRange
                    );
                }
                for raw in [source.len() + 1, usize::MAX] {
                    assert_eq!(
                        byte_pos_to_line_col(&text, BytePos::new(raw))
                            .unwrap_err()
                            .code(),
                        ErrorCode::OutOfRange
                    );
                    assert_eq!(
                        byte_pos_to_mark(&text, BytePos::new(raw))
                            .unwrap_err()
                            .code(),
                        ErrorCode::OutOfRange
                    );
                }
            }
        });
    }

    #[test]
    fn foreign_offsets_are_revalidated() {
        let foreign = Text::from_str("abcd");
        let text = Text::from_str("界");
        for (raw, code) in [(1, ErrorCode::InvalidBoundary), (4, ErrorCode::OutOfRange)] {
            let offset = foreign.validate_offset(raw).unwrap();
            assert_eq!(byte_to_line_col(&text, offset).unwrap_err().code(), code);
            assert_eq!(byte_to_mark(&text, offset).unwrap_err().code(), code);
        }
    }
}
