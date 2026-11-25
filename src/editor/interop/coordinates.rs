//! Strongly typed coordinates at editor boundaries.
//!
//! # Laws
//!
//! * **Integer roundtrips:** constructors store each valid integer unchanged,
//!   and accessors return those fields. Tagged coordinates retain their inner
//!   values. Thus constructing and reading a coordinate loses no information.
//! * **Origin separation:** the two one-based types reject zero with a
//!   [`BoundaryError`] carrying [`ErrorCode::InvalidCoordinate`]. Signed
//!   conversions reject negatives with the same code. Nonnegative integers
//!   that cannot fit in `usize` produce [`ErrorCode::OutOfRange`].
//! * **Kind bijection:** each kind has a distinct string, and the parser matches
//!   exactly that same mapping, so every kind roundtrips through its string.

use std::fmt;
use std::str::FromStr;

use super::error::{BoundaryError, ErrorCode};

/// The representation and origin of a coordinate.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CoordKind {
    /// Zero-based UTF-8 byte offset.
    Byte,
    /// One-based Unicode scalar position.
    EmacsChar,
    /// Zero-based line and byte column.
    LineColByte,
    /// Zero-based line and UTF-16 column.
    LineColUtf16,
    /// One-based line and zero-based byte column.
    OneBasedLineColByte,
}

impl CoordKind {
    /// All coordinate kinds in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Byte,
        Self::EmacsChar,
        Self::LineColByte,
        Self::LineColUtf16,
        Self::OneBasedLineColByte,
    ];

    /// Returns the stable lowercase snake-case tag.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Byte => "byte",
            Self::EmacsChar => "emacs_char",
            Self::LineColByte => "line_col_byte",
            Self::LineColUtf16 => "line_col_utf16",
            Self::OneBasedLineColByte => "one_based_line_col_byte",
        }
    }

    /// Parses an exact coordinate kind tag.
    pub fn parse_code(code: &str) -> Result<Self, UnknownCoordKind> {
        match code {
            "byte" => Ok(Self::Byte),
            "emacs_char" => Ok(Self::EmacsChar),
            "line_col_byte" => Ok(Self::LineColByte),
            "line_col_utf16" => Ok(Self::LineColUtf16),
            "one_based_line_col_byte" => Ok(Self::OneBasedLineColByte),
            _ => Err(UnknownCoordKind),
        }
    }
}

/// An unrecognized coordinate kind tag.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct UnknownCoordKind;

impl fmt::Display for UnknownCoordKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown coordinate kind")
    }
}
impl std::error::Error for UnknownCoordKind {}
impl fmt::Display for CoordKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl FromStr for CoordKind {
    type Err = UnknownCoordKind;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_code(s)
    }
}

fn signed(value: i64) -> Result<usize, BoundaryError> {
    if value < 0 {
        return Err(BoundaryError::new(
            ErrorCode::InvalidCoordinate,
            "negative coordinate",
        ));
    }
    usize::try_from(value)
        .map_err(|_| BoundaryError::new(ErrorCode::OutOfRange, "coordinate exceeds usize"))
}

/// A zero-based UTF-8 byte offset.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BytePos(usize);
impl BytePos {
    /// Creates a byte offset.
    pub const fn new(value: usize) -> Self {
        Self(value)
    }
    /// Converts a signed offset, rejecting negatives and overflow.
    pub fn try_from_i64(value: i64) -> Result<Self, BoundaryError> {
        signed(value).map(Self)
    }
    /// Converts an unsigned offset, rejecting overflow.
    pub fn try_from_u64(value: u64) -> Result<Self, BoundaryError> {
        usize::try_from(value)
            .map(Self)
            .map_err(|_| BoundaryError::new(ErrorCode::OutOfRange, "coordinate exceeds usize"))
    }
    /// Returns the offset.
    pub const fn raw(self) -> usize {
        self.0
    }
}

/// A one-based Unicode scalar position.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EmacsCharPos(usize);
impl EmacsCharPos {
    /// Creates a scalar position, rejecting zero.
    pub fn new(value: usize) -> Result<Self, BoundaryError> {
        if value == 0 {
            Err(BoundaryError::new(
                ErrorCode::InvalidCoordinate,
                "scalar position must be one-based",
            ))
        } else {
            Ok(Self(value))
        }
    }
    /// Converts a signed scalar position.
    pub fn try_from_i64(value: i64) -> Result<Self, BoundaryError> {
        Self::new(signed(value)?)
    }
    /// Returns the one-based position.
    pub const fn raw(self) -> usize {
        self.0
    }
    /// Returns the corresponding zero-based scalar offset.
    pub const fn zero_based_scalar(self) -> usize {
        self.0 - 1
    }
}

/// A zero-based line and zero-based byte column.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineColByte {
    line: usize,
    col_byte: usize,
}
impl LineColByte {
    /// Creates a line and byte column.
    pub const fn new(line: usize, col_byte: usize) -> Self {
        Self { line, col_byte }
    }
    /// Returns the zero-based line.
    pub const fn line(self) -> usize {
        self.line
    }
    /// Returns the zero-based byte column.
    pub const fn col_byte(self) -> usize {
        self.col_byte
    }
    /// Converts signed components.
    pub fn try_from_i64(line: i64, col_byte: i64) -> Result<Self, BoundaryError> {
        Ok(Self::new(signed(line)?, signed(col_byte)?))
    }
}

/// A zero-based line and zero-based UTF-16 code unit column.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LineColUtf16 {
    line: usize,
    col_utf16: usize,
}
impl LineColUtf16 {
    /// Creates a line and UTF-16 column.
    pub const fn new(line: usize, col_utf16: usize) -> Self {
        Self { line, col_utf16 }
    }
    /// Returns the zero-based line.
    pub const fn line(self) -> usize {
        self.line
    }
    /// Returns the zero-based UTF-16 column.
    pub const fn col_utf16(self) -> usize {
        self.col_utf16
    }
    /// Converts signed components.
    pub fn try_from_i64(line: i64, col_utf16: i64) -> Result<Self, BoundaryError> {
        Ok(Self::new(signed(line)?, signed(col_utf16)?))
    }
}

/// A one-based line and zero-based byte column.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OneBasedLineColByte {
    line: usize,
    col_byte: usize,
}
impl OneBasedLineColByte {
    /// Creates a mark position, rejecting line zero.
    pub fn new(line: usize, col_byte: usize) -> Result<Self, BoundaryError> {
        if line == 0 {
            Err(BoundaryError::new(
                ErrorCode::InvalidCoordinate,
                "line must be one-based",
            ))
        } else {
            Ok(Self { line, col_byte })
        }
    }
    /// Returns the one-based line.
    pub const fn line(self) -> usize {
        self.line
    }
    /// Returns the corresponding zero-based line.
    pub const fn zero_based_line(self) -> usize {
        self.line - 1
    }
    /// Returns the zero-based byte column.
    pub const fn col_byte(self) -> usize {
        self.col_byte
    }
}

/// A coordinate paired with its representation and origin.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TaggedCoord {
    /// A byte offset.
    Byte(BytePos),
    /// A scalar position.
    EmacsChar(EmacsCharPos),
    /// A zero-based line and byte column.
    LineColByte(LineColByte),
    /// A zero-based line and UTF-16 column.
    LineColUtf16(LineColUtf16),
    /// A one-based line and byte column.
    OneBasedLineColByte(OneBasedLineColByte),
}
impl TaggedCoord {
    /// Returns the tag corresponding to the wrapped coordinate.
    pub const fn kind(&self) -> CoordKind {
        match self {
            Self::Byte(_) => CoordKind::Byte,
            Self::EmacsChar(_) => CoordKind::EmacsChar,
            Self::LineColByte(_) => CoordKind::LineColByte,
            Self::LineColUtf16(_) => CoordKind::LineColUtf16,
            Self::OneBasedLineColByte(_) => CoordKind::OneBasedLineColByte,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, Rng};
    use std::collections::HashSet;

    #[test]
    fn kinds_roundtrip_and_are_unique() {
        let expected = [
            "byte",
            "emacs_char",
            "line_col_byte",
            "line_col_utf16",
            "one_based_line_col_byte",
        ];
        let mut seen = HashSet::new();
        assert_eq!(CoordKind::ALL.len(), expected.len());
        for (&kind, code) in CoordKind::ALL.iter().zip(expected) {
            assert!(seen.insert(code));
            assert_eq!(kind.as_str(), code);
            assert_eq!(kind.to_string(), code);
            assert_eq!(CoordKind::parse_code(code), Ok(kind));
            assert_eq!(code.parse::<CoordKind>(), Ok(kind));
            assert!(CoordKind::parse_code(&format!(" {code}")).is_err());
            assert!(CoordKind::parse_code(&code.to_uppercase()).is_err());
        }
        assert!(CoordKind::parse_code("").is_err());
    }

    #[test]
    fn origins_and_integer_boundaries() {
        assert_eq!(
            EmacsCharPos::new(0).unwrap_err().code(),
            ErrorCode::InvalidCoordinate
        );
        assert_eq!(
            OneBasedLineColByte::new(0, 0).unwrap_err().code(),
            ErrorCode::InvalidCoordinate
        );
        assert_eq!(
            EmacsCharPos::try_from_i64(0).unwrap_err().code(),
            ErrorCode::InvalidCoordinate
        );
        assert_eq!(
            BytePos::try_from_i64(-1).unwrap_err().code(),
            ErrorCode::InvalidCoordinate
        );
        assert_eq!(
            EmacsCharPos::try_from_i64(-1).unwrap_err().code(),
            ErrorCode::InvalidCoordinate
        );
        assert_eq!(
            LineColByte::try_from_i64(-1, 0).unwrap_err().code(),
            ErrorCode::InvalidCoordinate
        );
        assert_eq!(
            LineColByte::try_from_i64(0, -1).unwrap_err().code(),
            ErrorCode::InvalidCoordinate
        );
        assert_eq!(
            LineColUtf16::try_from_i64(-1, 0).unwrap_err().code(),
            ErrorCode::InvalidCoordinate
        );
        assert_eq!(
            LineColUtf16::try_from_i64(0, -1).unwrap_err().code(),
            ErrorCode::InvalidCoordinate
        );
        for value in [0, 1, u64::MAX] {
            match usize::try_from(value) {
                Ok(expected) => assert_eq!(BytePos::try_from_u64(value).unwrap().raw(), expected),
                Err(_) => assert_eq!(
                    BytePos::try_from_u64(value).unwrap_err().code(),
                    ErrorCode::OutOfRange
                ),
            }
        }
        match usize::try_from(i64::MAX) {
            Ok(expected) => assert_eq!(BytePos::try_from_i64(i64::MAX).unwrap().raw(), expected),
            Err(_) => assert_eq!(
                BytePos::try_from_i64(i64::MAX).unwrap_err().code(),
                ErrorCode::OutOfRange
            ),
        }
    }

    #[test]
    fn generated_coordinates_preserve_values_and_tags() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| {
                (0..128)
                    .map(|_| (rng.next_u64() as usize, rng.next_u64() as usize))
                    .collect::<Vec<_>>()
            },
            |values| {
                for &(a, b) in values {
                    let byte = BytePos::new(a);
                    let line_byte = LineColByte::new(a, b);
                    let utf16 = LineColUtf16::new(a, b);
                    assert_eq!(byte.raw(), a);
                    assert_eq!((line_byte.line(), line_byte.col_byte()), (a, b));
                    assert_eq!((utf16.line(), utf16.col_utf16()), (a, b));
                    let nonzero = a.max(1);
                    let scalar = EmacsCharPos::new(nonzero).unwrap();
                    let mark = OneBasedLineColByte::new(nonzero, b).unwrap();
                    assert_eq!(
                        (scalar.raw(), scalar.zero_based_scalar()),
                        (nonzero, nonzero - 1)
                    );
                    assert_eq!(
                        (mark.line(), mark.zero_based_line(), mark.col_byte()),
                        (nonzero, nonzero - 1, b)
                    );
                    let tagged = [
                        TaggedCoord::Byte(byte),
                        TaggedCoord::EmacsChar(scalar),
                        TaggedCoord::LineColByte(line_byte),
                        TaggedCoord::LineColUtf16(utf16),
                        TaggedCoord::OneBasedLineColByte(mark),
                    ];
                    assert_eq!(tagged.map(|c| c.kind()), CoordKind::ALL);
                    for coord in tagged {
                        match coord {
                            TaggedCoord::Byte(value) => assert_eq!(value.raw(), a),
                            TaggedCoord::EmacsChar(value) => assert_eq!(value.raw(), nonzero),
                            TaggedCoord::LineColByte(value) => {
                                assert_eq!((value.line(), value.col_byte()), (a, b));
                            }
                            TaggedCoord::LineColUtf16(value) => {
                                assert_eq!((value.line(), value.col_utf16()), (a, b));
                            }
                            TaggedCoord::OneBasedLineColByte(value) => {
                                assert_eq!((value.line(), value.col_byte()), (nonzero, b));
                            }
                        }
                    }
                    if let (Ok(x), Ok(y)) = (i64::try_from(a), i64::try_from(b)) {
                        assert_eq!(BytePos::try_from_i64(x).unwrap(), byte);
                        assert_eq!(LineColByte::try_from_i64(x, y).unwrap(), line_byte);
                        assert_eq!(LineColUtf16::try_from_i64(x, y).unwrap(), utf16);
                        if x > 0 {
                            assert_eq!(EmacsCharPos::try_from_i64(x).unwrap().raw(), a);
                        }
                    }
                }
            },
        );
    }
}
