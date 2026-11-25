//! Minimal atomic replacements of UTF-8 text.
//!
//! For a valid span, applying a splice yields `prefix ++ replacement ++ suffix`:
//! the endpoints partition the original text, and the implementation joins those
//! three pieces in order. The inverse replaces exactly the inserted bytes with
//! the removed slice, so applying it restores the original prefix, slice, and
//! suffix. Inserting empty text at any valid boundary is therefore an identity.
//! Both endpoints are revalidated against the supplied text before slicing;
//! out-of-bounds and non-character-boundary spans are rejected.

use crate::editor::text::span::{ByteSpan, SpanError};
use crate::editor::text::utf8::{ByteOffset, Text};
use std::fmt;

/// An atomic replacement of a half-open byte span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Splice {
    span: ByteSpan,
    replacement: Text,
}

/// A span cannot be used with the supplied text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpliceError {
    /// An offset exceeds the text's byte length.
    OutOfBounds { offset: usize, len: usize },
    /// An offset falls inside a UTF-8 scalar.
    NotCharBoundary { offset: usize },
    /// The range starts after its end.
    InvertedRange { start: usize, end: usize },
}

impl fmt::Display for SpliceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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

impl std::error::Error for SpliceError {}

impl From<SpanError> for SpliceError {
    fn from(error: SpanError) -> Self {
        match error {
            SpanError::OutOfBounds { offset, len } => Self::OutOfBounds { offset, len },
            SpanError::NotCharBoundary { offset } => Self::NotCharBoundary { offset },
            SpanError::InvertedRange { start, end } => Self::InvertedRange { start, end },
        }
    }
}

impl Splice {
    /// Creates a replacement of `span` with `replacement`.
    pub fn new(span: ByteSpan, replacement: Text) -> Self {
        Self { span, replacement }
    }

    /// Creates an insertion at `at`.
    pub fn insert(at: ByteOffset, text: Text) -> Self {
        Self::new(ByteSpan::empty_at(at), text)
    }

    /// Creates a deletion of `span`.
    pub fn delete(span: ByteSpan) -> Self {
        Self::new(span, Text::empty())
    }

    /// Returns the replaced span.
    pub fn span(&self) -> ByteSpan {
        self.span
    }

    /// Returns the inserted text.
    pub fn replacement(&self) -> &Text {
        &self.replacement
    }

    /// Returns whether the splice replaces an empty span with empty text.
    pub fn is_noop(&self) -> bool {
        self.span.is_empty() && self.replacement.is_empty()
    }

    /// Returns prefix, replacement, and suffix concatenated in that order.
    ///
    /// Rejects endpoints that are not valid character boundaries in `text`.
    pub fn apply(&self, text: &Text) -> Result<Text, SpliceError> {
        self.span.slice_in(text)?;
        let start = self.span.start().as_usize();
        let end = self.span.end().as_usize();
        let mut result = String::new();
        result.push_str(&text.as_str()[..start]);
        result.push_str(self.replacement.as_str());
        result.push_str(&text.as_str()[end..]);
        Ok(Text::from_str(&result))
    }

    /// Returns the splice that restores `text` after this splice is applied.
    pub fn invert(&self, text: &Text) -> Result<Self, SpliceError> {
        self.apply_with_inverse(text).map(|(_, inverse)| inverse)
    }

    /// Applies this splice and returns an inverse that restores the input.
    ///
    /// If this returns `(next, inverse)`, then `inverse.apply(&next)` equals
    /// `Ok(text.clone())`: the inverse removes the inserted bytes and puts back
    /// the original slice, leaving the prefix and suffix unchanged.
    pub fn apply_with_inverse(&self, text: &Text) -> Result<(Text, Self), SpliceError> {
        let removed = Text::from_str(self.span.slice_in(text)?);
        let next = self.apply(text)?;
        let start = self.span.start().as_usize();
        let inverse_span =
            ByteSpan::validate_in(&next, start, start + self.replacement.len_bytes())?;
        Ok((next, Self::new(inverse_span, removed)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};

    #[test]
    fn examples() {
        let text = Text::from_str("a界e\u{301}😀z");
        for at in text.char_boundaries() {
            let splice = Splice::insert(at, Text::empty());
            assert!(splice.is_noop());
            assert_eq!(splice.apply(&text), Ok(text.clone()));
        }
        let span = ByteSpan::validate_in(&text, 1, 4).unwrap();
        let deletion = Splice::delete(span);
        assert_eq!(deletion.span(), span);
        assert!(deletion.replacement().is_empty());
        assert!(!deletion.is_noop());
        assert_eq!(deletion.apply(&text).unwrap().as_str(), "ae\u{301}😀z");
        let replacement = Splice::new(ByteSpan::full_of(&text), Text::from_str("新😀"));
        assert_eq!(replacement.apply(&text).unwrap().as_str(), "新😀");
        let (next, inverse) = replacement.apply_with_inverse(&text).unwrap();
        assert_eq!(inverse.apply(&next), Ok(text.clone()));
        assert_eq!(replacement.invert(&text), Ok(inverse));
    }

    #[test]
    fn splice_laws_on_deterministic_corpus() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for (index, source) in corpus.iter().enumerate() {
                let text = Text::from_str(source);
                let replacement = Text::from_str(&corpus[(index + 1) % corpus.len()]);
                let boundaries = text.char_boundaries();
                for (i, &start) in boundaries.iter().enumerate() {
                    for &end in &boundaries[i..] {
                        let splice =
                            Splice::new(ByteSpan::new(start, end).unwrap(), replacement.clone());
                        let (next, inverse) = splice.apply_with_inverse(&text).unwrap();
                        let expected = format!(
                            "{}{}{}",
                            &source[..start.as_usize()],
                            replacement.as_str(),
                            &source[end.as_usize()..]
                        );
                        assert_eq!(next.as_str(), expected);
                        assert_eq!(splice.apply(&text), Ok(next.clone()));
                        assert_eq!(inverse.apply(&next), Ok(text.clone()));
                        assert_eq!(splice.invert(&text), Ok(inverse));
                    }
                }
            }
        });
    }

    #[test]
    fn rejects_foreign_boundaries() {
        let ascii = Text::from_str("abcd");
        let text = Text::from_str("界");
        for (span, error) in [
            (
                ByteSpan::empty_at(ascii.validate_offset(1).unwrap()),
                SpliceError::NotCharBoundary { offset: 1 },
            ),
            (
                ByteSpan::empty_at(ascii.validate_offset(4).unwrap()),
                SpliceError::OutOfBounds { offset: 4, len: 3 },
            ),
            (
                ByteSpan::new(
                    ascii.validate_offset(0).unwrap(),
                    ascii.validate_offset(1).unwrap(),
                )
                .unwrap(),
                SpliceError::NotCharBoundary { offset: 1 },
            ),
            (
                ByteSpan::new(
                    ascii.validate_offset(0).unwrap(),
                    ascii.validate_offset(4).unwrap(),
                )
                .unwrap(),
                SpliceError::OutOfBounds { offset: 4, len: 3 },
            ),
        ] {
            let splice = Splice::delete(span);
            assert_eq!(splice.apply(&text), Err(error));
            assert_eq!(splice.invert(&text), Err(error));
            assert_eq!(splice.apply_with_inverse(&text), Err(error));
        }
    }
}
