//! Lossless conversion between UTF-8 byte boundaries and Emacs scalar positions.
//!
//! # Laws and proofs
//!
//! * **Definition:** for a valid boundary `b`, `byte_to_emacs(text, b)` is
//!   `Ok(EmacsCharPos::new(1 + prefix_scalars(b)).unwrap())`, where the prefix
//!   ends at `b`. The conversion counts exactly those prefix scalars.
//! * **Bijection:** every scalar contributes exactly one nonempty UTF-8 encoding,
//!   so the ordered boundaries correspond one-to-one to scalar counts from zero
//!   through the text's scalar length. Both conversions select the same boundary
//!   by that count; therefore each is the other's inverse, including point-min
//!   and point-max.
//! * **Monotonicity:** for valid boundaries `b1` and `b2`, `b1 < b2` if and
//!   only if their Emacs positions are ordered likewise. A nonempty interval
//!   between boundaries contains at least one scalar, and each scalar uses at
//!   least one byte.
//! * **Strict rejection:** byte inputs are checked for range and UTF-8 boundary
//!   membership before slicing. Emacs positions greater than point-max have no
//!   corresponding boundary and are rejected; zero cannot be constructed.

use super::coordinates::{BytePos, EmacsCharPos};
use super::error::{BoundaryError, ErrorCode};
use crate::editor::text::utf8::{ByteOffset, Text, Utf8Error};

fn validate_byte(text: &Text, offset: usize) -> Result<ByteOffset, BoundaryError> {
    text.validate_offset(offset).map_err(|error| {
        let code = match error {
            Utf8Error::NotCharBoundary { .. } => ErrorCode::InvalidBoundary,
            Utf8Error::OutOfBounds { .. } => ErrorCode::OutOfRange,
            _ => unreachable!("offset validation only checks range and boundaries"),
        };
        BoundaryError::new(code, error.to_string())
    })
}

/// Returns the first Emacs position, including for empty text.
pub fn point_min() -> EmacsCharPos {
    EmacsCharPos::new(1).expect("one is a valid position")
}

/// Returns the position immediately after the last scalar.
pub fn point_max(text: &Text) -> EmacsCharPos {
    EmacsCharPos::new(1 + text.len_scalars()).expect("scalar position is nonzero")
}

/// Converts a byte boundary after revalidating it against `text`.
pub fn byte_to_emacs(text: &Text, offset: ByteOffset) -> Result<EmacsCharPos, BoundaryError> {
    let offset = validate_byte(text, offset.as_usize())?.as_usize();
    Ok(
        EmacsCharPos::new(1 + text.as_str()[..offset].chars().count())
            .expect("scalar position is nonzero"),
    )
}

/// Converts an Emacs scalar position to its exact byte boundary.
pub fn emacs_to_byte(text: &Text, pos: EmacsCharPos) -> Result<ByteOffset, BoundaryError> {
    let scalar = pos.zero_based_scalar();
    let offset = text
        .as_str()
        .char_indices()
        .map(|(offset, _)| offset)
        .chain(std::iter::once(text.len_bytes()))
        .nth(scalar)
        .ok_or_else(|| BoundaryError::new(ErrorCode::OutOfRange, "position exceeds point-max"))?;
    Ok(text
        .validate_offset(offset)
        .expect("char_indices yields boundaries"))
}

/// Validates a raw byte position and converts it to an Emacs position.
pub fn byte_pos_to_emacs(text: &Text, pos: BytePos) -> Result<EmacsCharPos, BoundaryError> {
    byte_to_emacs(text, validate_byte(text, pos.raw())?)
}

/// Converts an Emacs position to a raw byte position.
pub fn emacs_to_byte_pos(text: &Text, pos: EmacsCharPos) -> Result<BytePos, BoundaryError> {
    emacs_to_byte(text, pos).map(|offset| BytePos::new(offset.as_usize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};

    #[test]
    fn known_positions_count_scalars_including_line_endings() {
        for (source, boundaries) in [
            ("", &[0][..]),
            ("abc", &[0, 1, 2, 3][..]),
            ("\n\r\r\n", &[0, 1, 2, 3, 4][..]),
            ("日本界", &[0, 3, 6, 9][..]),
            ("e\u{301}😀", &[0, 1, 3, 7][..]),
        ] {
            let text = Text::from_str(source);
            assert_eq!(point_min().raw(), 1);
            assert_eq!(point_max(&text).raw(), boundaries.len());
            for (index, &byte) in boundaries.iter().enumerate() {
                let pos = EmacsCharPos::new(index + 1).unwrap();
                assert_eq!(byte_pos_to_emacs(&text, BytePos::new(byte)), Ok(pos));
                assert_eq!(emacs_to_byte_pos(&text, pos), Ok(BytePos::new(byte)));
            }
        }
    }

    #[test]
    fn generated_boundaries_obey_definition_bijection_and_order() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let text = Text::from_str(source);
                let boundaries = text.char_boundaries();
                assert_eq!(point_max(&text).raw(), 1 + source.chars().count());
                for (i, &boundary) in boundaries.iter().enumerate() {
                    let pos = byte_to_emacs(&text, boundary).unwrap();
                    assert_eq!(pos.raw(), 1 + source[..boundary.as_usize()].chars().count());
                    assert_eq!(emacs_to_byte(&text, pos), Ok(boundary));
                    assert_eq!(
                        byte_pos_to_emacs(&text, BytePos::new(boundary.as_usize())),
                        Ok(pos)
                    );
                    assert_eq!(
                        emacs_to_byte_pos(&text, pos),
                        Ok(BytePos::new(boundary.as_usize()))
                    );
                    for &other in &boundaries {
                        assert_eq!(boundary < other, pos < byte_to_emacs(&text, other).unwrap());
                    }
                    assert_eq!(pos.raw(), i + 1);
                }
                for raw in 0..=source.len() {
                    if !source.is_char_boundary(raw) {
                        assert_eq!(
                            byte_pos_to_emacs(&text, BytePos::new(raw))
                                .unwrap_err()
                                .code(),
                            ErrorCode::InvalidBoundary
                        );
                    }
                }
                for raw in [source.len() + 1, usize::MAX] {
                    assert_eq!(
                        byte_pos_to_emacs(&text, BytePos::new(raw))
                            .unwrap_err()
                            .code(),
                        ErrorCode::OutOfRange
                    );
                }
                for raw in [point_max(&text).raw() + 1, usize::MAX] {
                    assert_eq!(
                        emacs_to_byte(&text, EmacsCharPos::new(raw).unwrap())
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
        let target = Text::from_str("界");
        assert_eq!(
            byte_to_emacs(&target, foreign.validate_offset(1).unwrap())
                .unwrap_err()
                .code(),
            ErrorCode::InvalidBoundary
        );
        assert_eq!(
            byte_to_emacs(&target, foreign.validate_offset(4).unwrap())
                .unwrap_err()
                .code(),
            ErrorCode::OutOfRange
        );
    }
}
