//! Agent position conversion.
//! Lossless conversion between agent positions and internal byte boundaries.
//!
//! LF separates lines, its boundary belongs to the preceding line, and CR is
//! content. The final line exists even after a trailing LF.
//!
//! # Laws and proofs
//!
//! * A byte position stores the validated offset unchanged. Line indexing
//!   partitions all boundaries into line content ranges, and UTF-16 prefix
//!   lengths strictly increase at scalar boundaries. Therefore both forms
//!   roundtrip at every boundary, including EOF and empty lines.
//! * Both forms recover the same offset for the same boundary. A span is built
//!   from these offsets alone, so either representation at either endpoint
//!   produces the same span.
//! * Range and UTF-8 boundary checks precede use of byte offsets, including
//!   offsets obtained from another text. UTF-16 conversion rejects surrogate
//!   interiors and out-of-range lines and columns; span construction rejects
//!   inverted endpoints.

use super::coordinates::{BytePos, CoordKind, LineColUtf16};
use super::error::{BoundaryError, ErrorCode};
use crate::editor::text::lines::{LineError, LineIndex};
use crate::editor::text::span::ByteSpan;
use crate::editor::text::utf16::{self, Utf16Error};
use crate::editor::text::utf8::{ByteOffset, Text, Utf8Error};

/// A byte position or a zero-based line and UTF-16 column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentPosition {
    /// A raw UTF-8 byte position.
    Byte(BytePos),
    /// A zero-based line and UTF-16 code-unit column.
    LineColUtf16(LineColUtf16),
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

fn utf16_error(error: Utf16Error) -> BoundaryError {
    let code = match error {
        Utf16Error::SurrogateInterior { .. } => ErrorCode::InvalidUtf16Boundary,
        Utf16Error::NotCharBoundary { .. }
        | Utf16Error::Line(LineError::NotCharBoundary { .. }) => ErrorCode::InvalidBoundary,
        _ => ErrorCode::OutOfRange,
    };
    BoundaryError::new(code, error.to_string())
}

/// Validates and converts an agent position to an internal byte boundary.
pub fn position_to_byte(text: &Text, position: AgentPosition) -> Result<ByteOffset, BoundaryError> {
    match position {
        AgentPosition::Byte(pos) => validate_byte(text, pos.raw()),
        AgentPosition::LineColUtf16(pos) => utf16::line_col_to_offset(
            &LineIndex::from_text(text),
            text,
            utf16::LineColUtf16 {
                line: pos.line(),
                col_utf16: pos.col_utf16(),
            },
        )
        .map_err(utf16_error),
    }
}

/// Converts an offset to a raw byte position after revalidation against `text`.
pub fn byte_to_byte_pos(text: &Text, offset: ByteOffset) -> Result<BytePos, BoundaryError> {
    Ok(BytePos::new(
        validate_byte(text, offset.as_usize())?.as_usize(),
    ))
}

/// Converts an offset to a line and UTF-16 column, revalidating against `text`.
pub fn byte_to_utf16(text: &Text, offset: ByteOffset) -> Result<LineColUtf16, BoundaryError> {
    let pos = utf16::offset_to_line_col(&LineIndex::from_text(text), text, offset)
        .map_err(utf16_error)?;
    Ok(LineColUtf16::new(pos.line, pos.col_utf16))
}

/// Converts an offset to the requested agent representation.
///
/// Only [`CoordKind::Byte`] and [`CoordKind::LineColUtf16`] are supported.
pub fn byte_to_agent_position(
    text: &Text,
    offset: ByteOffset,
    kind: CoordKind,
) -> Result<AgentPosition, BoundaryError> {
    match kind {
        CoordKind::Byte => byte_to_byte_pos(text, offset).map(AgentPosition::Byte),
        CoordKind::LineColUtf16 => byte_to_utf16(text, offset).map(AgentPosition::LineColUtf16),
        _ => Err(BoundaryError::new(
            ErrorCode::UnsupportedOperation,
            "unsupported agent coordinate kind",
        )),
    }
}

/// Converts endpoints independently and constructs their ordered half-open span.
pub fn span_from_positions(
    text: &Text,
    start: AgentPosition,
    end: AgentPosition,
) -> Result<ByteSpan, BoundaryError> {
    let start = position_to_byte(text, start)?;
    let end = position_to_byte(text, end)?;
    ByteSpan::new(start, end)
        .map_err(|error| BoundaryError::new(ErrorCode::InvalidSpan, error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CORPUS: &[&str] = &[
        "",
        "\n",
        "\n\n",
        "abc",
        "a\nb\n",
        "\r",
        "\r\n",
        "a\rb\r\nc",
        "é界",
        "e\u{301}",
        "😀𐀀",
        "é😀\n\r\n界e\u{301}\n",
    ];

    #[test]
    fn every_boundary_and_span_roundtrips_across_representations() {
        for &source in CORPUS {
            let text = Text::from_str(source);
            let mut positions = Vec::new();
            for offset in text.char_boundaries() {
                let raw = offset.as_usize();
                let prefix = &source[..raw];
                let line = prefix.bytes().filter(|&b| b == b'\n').count();
                let column = prefix.rsplit('\n').next().unwrap().encode_utf16().count();
                let forms = [
                    AgentPosition::Byte(BytePos::new(raw)),
                    AgentPosition::LineColUtf16(LineColUtf16::new(line, column)),
                ];
                assert_eq!(byte_to_byte_pos(&text, offset), Ok(BytePos::new(raw)));
                assert_eq!(
                    byte_to_utf16(&text, offset),
                    Ok(LineColUtf16::new(line, column))
                );
                for (form, kind) in forms
                    .into_iter()
                    .zip([CoordKind::Byte, CoordKind::LineColUtf16])
                {
                    assert_eq!(position_to_byte(&text, form), Ok(offset));
                    assert_eq!(byte_to_agent_position(&text, offset, kind), Ok(form));
                }
                positions.push((offset, forms));
            }
            for &(start, start_forms) in &positions {
                for &(end, end_forms) in &positions {
                    for start_form in start_forms {
                        for end_form in end_forms {
                            let actual = span_from_positions(&text, start_form, end_form);
                            if start <= end {
                                assert_eq!(actual, Ok(ByteSpan::new(start, end).unwrap()));
                            } else {
                                assert_eq!(actual.unwrap_err().code(), ErrorCode::InvalidSpan);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn invalid_coordinates_and_foreign_offsets_are_rejected() {
        for &source in CORPUS {
            let text = Text::from_str(source);
            for raw in 0..source.len() {
                if !source.is_char_boundary(raw) {
                    assert_eq!(
                        position_to_byte(&text, AgentPosition::Byte(BytePos::new(raw)))
                            .unwrap_err()
                            .code(),
                        ErrorCode::InvalidBoundary
                    );
                }
            }
            for raw in [source.len() + 1, usize::MAX] {
                assert_eq!(
                    position_to_byte(&text, AgentPosition::Byte(BytePos::new(raw)))
                        .unwrap_err()
                        .code(),
                    ErrorCode::OutOfRange
                );
            }
            for (line, content) in source.split('\n').enumerate() {
                let mut units = 0;
                for ch in content.chars() {
                    if ch.len_utf16() == 2 {
                        let pos = AgentPosition::LineColUtf16(LineColUtf16::new(line, units + 1));
                        assert_eq!(
                            position_to_byte(&text, pos).unwrap_err().code(),
                            ErrorCode::InvalidUtf16Boundary
                        );
                    }
                    units += ch.len_utf16();
                }
                for col in [units + 1, usize::MAX] {
                    let pos = AgentPosition::LineColUtf16(LineColUtf16::new(line, col));
                    assert_eq!(
                        position_to_byte(&text, pos).unwrap_err().code(),
                        ErrorCode::OutOfRange
                    );
                }
            }
            for line in [source.split('\n').count(), usize::MAX] {
                let pos = AgentPosition::LineColUtf16(LineColUtf16::new(line, 0));
                assert_eq!(
                    position_to_byte(&text, pos).unwrap_err().code(),
                    ErrorCode::OutOfRange
                );
            }
        }
        let foreign = Text::from_str("abcd");
        let target = Text::from_str("界");
        for (raw, code) in [(1, ErrorCode::InvalidBoundary), (4, ErrorCode::OutOfRange)] {
            let offset = foreign.validate_offset(raw).unwrap();
            assert_eq!(byte_to_byte_pos(&target, offset).unwrap_err().code(), code);
            assert_eq!(byte_to_utf16(&target, offset).unwrap_err().code(), code);
            for kind in [CoordKind::Byte, CoordKind::LineColUtf16] {
                assert_eq!(
                    byte_to_agent_position(&target, offset, kind)
                        .unwrap_err()
                        .code(),
                    code
                );
            }
        }
        let zero = target.validate_offset(0).unwrap();
        for kind in [
            CoordKind::EmacsChar,
            CoordKind::LineColByte,
            CoordKind::OneBasedLineColByte,
        ] {
            assert_eq!(
                byte_to_agent_position(&target, zero, kind)
                    .unwrap_err()
                    .code(),
                ErrorCode::UnsupportedOperation
            );
        }
    }
}
