//! Immutable UTF-8 text and validated byte offsets.
//!
//! For every boundary `b` in `t`, splitting and concatenating yields `t`:
//! `let (l, r) = t.split_at(b)?; assert_eq!(l.concat(&r), t);`.
//! Splitting partitions the original bytes without changing them, so joining
//! the two pieces restores those bytes. Concatenation is associative and
//! [`Text::empty`] is its left and right identity: both follow from the same
//! laws for byte sequence concatenation. Valid UTF-8 remains valid when joined
//! at string boundaries.
//!
//! Construction rejects invalid UTF-8; offset validation rejects positions
//! outside the text and positions inside a multi-byte scalar. An offset must be
//! revalidated when used with a different text, which the operations do.
//! `str::from_utf8` checks the entire input before construction. Before any
//! slicing, a length comparison rejects out-of-bounds positions and
//! `str::is_char_boundary` rejects positions inside a scalar's encoding.

use std::fmt;
use std::sync::Arc;

/// Immutable, valid UTF-8 text.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct Text(Arc<str>);

/// A byte offset validated against a text's UTF-8 character boundaries.
///
/// An offset validated for one text is not necessarily valid for another.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ByteOffset(usize);

impl ByteOffset {
    /// Returns the zero-based byte offset.
    pub fn as_usize(self) -> usize {
        self.0
    }
}

/// A failure to construct or address UTF-8 text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Utf8Error {
    /// The supplied bytes are not valid UTF-8.
    InvalidUtf8,
    /// An offset exceeds the byte length of the text.
    OutOfBounds { offset: usize, len: usize },
    /// An offset falls inside a multi-byte UTF-8 scalar.
    NotCharBoundary { offset: usize },
    /// A range starts after it ends.
    InvertedRange { start: usize, end: usize },
}

impl fmt::Display for Utf8Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8 => write!(f, "invalid UTF-8"),
            Self::OutOfBounds { offset, len } => write!(f, "offset {offset} exceeds length {len}"),
            Self::NotCharBoundary { offset } => {
                write!(f, "offset {offset} is not a character boundary")
            }
            Self::InvertedRange { start, end } => {
                write!(f, "range start {start} exceeds end {end}")
            }
        }
    }
}

impl std::error::Error for Utf8Error {}

impl Text {
    /// Returns the identity element for concatenation.
    pub fn empty() -> Self {
        Self(Arc::from(""))
    }

    /// Copies valid UTF-8 from a string slice.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> Self {
        Self(Arc::from(text))
    }

    /// Copies bytes if and only if they form valid UTF-8.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Utf8Error> {
        std::str::from_utf8(bytes)
            .map(Self::from_str)
            .map_err(|_| Utf8Error::InvalidUtf8)
    }

    /// Borrows the text as a UTF-8 string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Borrows the UTF-8 bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.as_str().as_bytes()
    }

    /// Returns the length in bytes.
    pub fn len_bytes(&self) -> usize {
        self.0.len()
    }

    /// Returns the number of Unicode scalar values, not grapheme clusters.
    pub fn len_scalars(&self) -> usize {
        self.0.chars().count()
    }

    /// Returns whether the text contains no bytes.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Validates a byte offset in `0..=self.len_bytes()` at a character boundary.
    pub fn validate_offset(&self, byte_offset: usize) -> Result<ByteOffset, Utf8Error> {
        if byte_offset > self.len_bytes() {
            return Err(Utf8Error::OutOfBounds {
                offset: byte_offset,
                len: self.len_bytes(),
            });
        }
        if !self.0.is_char_boundary(byte_offset) {
            return Err(Utf8Error::NotCharBoundary {
                offset: byte_offset,
            });
        }
        Ok(ByteOffset(byte_offset))
    }

    /// Concatenates two texts. This operation is associative with empty identity.
    pub fn concat(&self, other: &Self) -> Self {
        let mut result = String::with_capacity(self.len_bytes() + other.len_bytes());
        result.push_str(self.as_str());
        result.push_str(other.as_str());
        Self::from_str(&result)
    }

    /// Splits at a boundary valid in this text; concatenating the results restores it.
    ///
    /// The supplied offset is revalidated against this text.
    pub fn split_at(&self, offset: ByteOffset) -> Result<(Self, Self), Utf8Error> {
        let offset = self.validate_offset(offset.0)?.0;
        let (left, right) = self.as_str().split_at(offset);
        Ok((Self::from_str(left), Self::from_str(right)))
    }

    /// Borrows the half-open range between two boundaries valid in this text.
    ///
    /// Both offsets are revalidated against this text and `start` must not exceed `end`.
    pub fn slice(&self, start: ByteOffset, end: ByteOffset) -> Result<&str, Utf8Error> {
        let start = self.validate_offset(start.0)?.0;
        let end = self.validate_offset(end.0)?.0;
        if start > end {
            return Err(Utf8Error::InvertedRange { start, end });
        }
        Ok(&self.as_str()[start..end])
    }

    /// Returns all character boundaries in order, including zero and the byte length.
    pub fn char_boundaries(&self) -> Vec<ByteOffset> {
        self.as_str()
            .char_indices()
            .map(|(i, _)| ByteOffset(i))
            .chain(std::iter::once(ByteOffset(self.len_bytes())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts, Rng};

    #[test]
    fn corpus_boundaries_and_roundtrips() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let text = Text::from_bytes(source.as_bytes()).unwrap();
                assert_eq!(text.as_str(), source);
                assert_eq!(text.as_bytes(), source.as_bytes());
                assert_eq!(text.len_bytes(), source.len());
                assert_eq!(text.len_scalars(), source.chars().count());
                assert_eq!(text.is_empty(), source.is_empty());
                let expected: Vec<_> = source
                    .char_indices()
                    .map(|(i, _)| i)
                    .chain(std::iter::once(source.len()))
                    .collect();
                let boundaries = text.char_boundaries();
                assert_eq!(
                    boundaries.iter().map(|b| b.as_usize()).collect::<Vec<_>>(),
                    expected
                );
                for offset in 0..=source.len() {
                    let result = text.validate_offset(offset);
                    if source.is_char_boundary(offset) {
                        let boundary = result.unwrap();
                        let (left, right) = text.split_at(boundary).unwrap();
                        assert_eq!(left.concat(&right), text);
                        assert_eq!(
                            text.slice(boundaries[0], boundary).unwrap(),
                            &source[..offset]
                        );
                    } else {
                        assert_eq!(result, Err(Utf8Error::NotCharBoundary { offset }));
                    }
                }
                assert_eq!(
                    text.validate_offset(source.len() + 1),
                    Err(Utf8Error::OutOfBounds {
                        offset: source.len() + 1,
                        len: source.len()
                    })
                );
            }
        });
    }

    #[test]
    fn concatenation_monoid() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| texts(rng),
            |corpus| {
                let empty = Text::empty();
                for chunk in corpus.chunks(3) {
                    let a = Text::from_str(&chunk[0]);
                    let b = Text::from_str(&chunk[1 % chunk.len()]);
                    let c = Text::from_str(&chunk[2 % chunk.len()]);
                    assert_eq!(empty.concat(&a), a);
                    assert_eq!(a.concat(&empty), a);
                    assert_eq!(a.concat(&b).concat(&c), a.concat(&b.concat(&c)));
                }
            },
        );
    }

    #[test]
    fn rejects_invalid_bytes_and_foreign_offsets() {
        for bytes in [
            &[0xff][..],
            &[0xc0, 0x80],
            &[0xe3, 0x81],
            &[0xed, 0xa0, 0x80],
            &[0xf4, 0x90, 0x80, 0x80],
        ] {
            assert_eq!(Text::from_bytes(bytes), Err(Utf8Error::InvalidUtf8));
        }
        let ascii = Text::from_str("abc");
        let multibyte = Text::from_str("界");
        let inside = ascii.validate_offset(1).unwrap();
        assert_eq!(
            multibyte.split_at(inside),
            Err(Utf8Error::NotCharBoundary { offset: 1 })
        );
        assert_eq!(
            multibyte.slice(inside, inside),
            Err(Utf8Error::NotCharBoundary { offset: 1 })
        );
        let long = Text::from_str("abcd");
        let beyond = long.validate_offset(4).unwrap();
        assert_eq!(
            multibyte.split_at(beyond),
            Err(Utf8Error::OutOfBounds { offset: 4, len: 3 })
        );
        assert_eq!(
            multibyte.slice(multibyte.validate_offset(0).unwrap(), beyond),
            Err(Utf8Error::OutOfBounds { offset: 4, len: 3 })
        );
        assert_eq!(
            ascii.slice(
                ascii.validate_offset(2).unwrap(),
                ascii.validate_offset(1).unwrap()
            ),
            Err(Utf8Error::InvertedRange { start: 2, end: 1 })
        );
    }
}
