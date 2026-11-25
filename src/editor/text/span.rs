//! Validated half-open byte spans.
//!
//! Containment is a partial order: endpoint comparisons are reflexive and
//! transitive, and mutual containment forces both pairs of endpoints equal.
//! Both intersection operations are commutative because `max` and `min` are
//! commutative. Their returned endpoints lie within both inputs, so every
//! returned span is contained in each input. Closed intersection includes a
//! touching point; strict intersection requires a positive byte length.
//!
//! Inverted ranges are rejected at construction. Validation and slicing also
//! reject offsets beyond the text or inside a UTF-8 scalar. Offsets originating
//! in another text are revalidated before slicing.

use super::utf8::{ByteOffset, Text, Utf8Error};
use std::fmt;

/// A half-open byte range with ordered endpoints.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ByteSpan {
    start: ByteOffset,
    end: ByteOffset,
}

/// A failure to construct or use a byte span.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpanError {
    /// The range starts after it ends.
    InvertedRange { start: usize, end: usize },
    /// An offset exceeds the text's byte length.
    OutOfBounds { offset: usize, len: usize },
    /// An offset falls inside a multi-byte UTF-8 scalar.
    NotCharBoundary { offset: usize },
}

impl fmt::Display for SpanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvertedRange { start, end } => {
                write!(f, "range start {start} exceeds end {end}")
            }
            Self::OutOfBounds { offset, len } => write!(f, "offset {offset} exceeds length {len}"),
            Self::NotCharBoundary { offset } => {
                write!(f, "offset {offset} is not a character boundary")
            }
        }
    }
}

impl std::error::Error for SpanError {}

fn validate_offset(text: &Text, offset: usize) -> Result<ByteOffset, SpanError> {
    text.validate_offset(offset).map_err(|error| match error {
        Utf8Error::OutOfBounds { offset, len } => SpanError::OutOfBounds { offset, len },
        Utf8Error::NotCharBoundary { offset } => SpanError::NotCharBoundary { offset },
        _ => unreachable!("offset validation only returns offset errors"),
    })
}

impl ByteSpan {
    /// Constructs a span, rejecting inverted endpoints.
    pub fn new(start: ByteOffset, end: ByteOffset) -> Result<Self, SpanError> {
        if start > end {
            return Err(SpanError::InvertedRange {
                start: start.as_usize(),
                end: end.as_usize(),
            });
        }
        Ok(Self { start, end })
    }

    /// Validates both byte offsets against `text`, then checks their order.
    pub fn validate_in(text: &Text, start_byte: usize, end_byte: usize) -> Result<Self, SpanError> {
        let start = validate_offset(text, start_byte)?;
        let end = validate_offset(text, end_byte)?;
        Self::new(start, end)
    }

    /// Returns an empty span at an offset.
    pub fn empty_at(offset: ByteOffset) -> Self {
        Self {
            start: offset,
            end: offset,
        }
    }

    /// Returns the span covering the entire text.
    pub fn full_of(text: &Text) -> Self {
        Self::validate_in(text, 0, text.len_bytes()).expect("text endpoints are valid")
    }

    /// Returns the start offset.
    pub fn start(self) -> ByteOffset {
        self.start
    }
    /// Returns the end offset.
    pub fn end(self) -> ByteOffset {
        self.end
    }
    /// Returns the span length in bytes.
    pub fn len_bytes(self) -> usize {
        self.end.as_usize() - self.start.as_usize()
    }
    /// Returns whether the endpoints coincide.
    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
    /// Returns whether the offset lies between the endpoints, inclusive.
    pub fn contains_offset(self, offset: ByteOffset) -> bool {
        self.start <= offset && offset <= self.end
    }
    /// Returns whether both endpoints of `other` lie within this span.
    pub fn contains_span(self, other: Self) -> bool {
        self.start <= other.start && other.end <= self.end
    }
    /// Returns whether `self.start < other.end && other.start < self.end`.
    ///
    /// An empty span strictly inside a nonempty span satisfies this predicate.
    pub fn overlaps_open(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
    /// Returns whether the spans have closed-interval contact.
    pub fn contacts_closed(self, other: Self) -> bool {
        self.start <= other.end && other.start <= self.end
    }
    /// Returns the closed intersection, including an empty span at contact.
    pub fn intersect_closed(self, other: Self) -> Option<Self> {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        Self::new(start, end).ok()
    }
    /// Returns the intersection only if it has positive byte length.
    pub fn strict_intersect(self, other: Self) -> Option<Self> {
        self.intersect_closed(other).filter(|span| !span.is_empty())
    }
    /// Borrows this range after revalidating both endpoints against `text`.
    pub fn slice_in(self, text: &Text) -> Result<&str, SpanError> {
        let span = Self::validate_in(text, self.start.as_usize(), self.end.as_usize())?;
        Ok(&text.as_str()[span.start.as_usize()..span.end.as_usize()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};

    #[test]
    fn corpus_span_laws_and_slices() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let text = Text::from_str(source);
                let boundaries = text.char_boundaries();
                let mut spans = Vec::new();
                for (i, &start) in boundaries.iter().enumerate() {
                    for &end in &boundaries[i..] {
                        let span = ByteSpan::new(start, end).unwrap();
                        assert_eq!(
                            ByteSpan::validate_in(&text, start.as_usize(), end.as_usize()),
                            Ok(span)
                        );
                        assert_eq!(
                            span.slice_in(&text).unwrap(),
                            &source[start.as_usize()..end.as_usize()]
                        );
                        assert_eq!(span.len_bytes(), end.as_usize() - start.as_usize());
                        assert_eq!(span.is_empty(), start == end);
                        assert_eq!((span.start(), span.end()), (start, end));
                        assert!(span.contains_span(span));
                        assert!(span.contains_offset(start) && span.contains_offset(end));
                        spans.push(span);
                    }
                }
                assert_eq!(
                    ByteSpan::full_of(&text).slice_in(&text),
                    Ok(source.as_str())
                );
                for &a in &spans {
                    for &b in &spans {
                        if a.contains_span(b) && b.contains_span(a) {
                            assert_eq!(a, b);
                        }
                        assert_eq!(a.intersect_closed(b), b.intersect_closed(a));
                        assert_eq!(a.strict_intersect(b), b.strict_intersect(a));
                        assert_eq!(a.contacts_closed(b), a.intersect_closed(b).is_some());
                        assert_eq!(a.overlaps_open(b), a.start < b.end && b.start < a.end);
                        for c in [a.intersect_closed(b), a.strict_intersect(b)]
                            .into_iter()
                            .flatten()
                        {
                            assert!(a.contains_span(c) && b.contains_span(c));
                        }
                        // Check representative third spans from the deterministic corpus.
                        if a.contains_span(b) {
                            for &c in spans
                                .iter()
                                .step_by((spans.len() / 8).max(1))
                                .filter(|&&c| b.contains_span(c))
                            {
                                assert!(a.contains_span(c));
                            }
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn rejects_invalid_ranges_and_revalidates_foreign_offsets() {
        let ascii = Text::from_str("abcd");
        let multibyte = Text::from_str("界");
        let offset = |n| ascii.validate_offset(n).unwrap();
        assert_eq!(
            ByteSpan::new(offset(2), offset(1)),
            Err(SpanError::InvertedRange { start: 2, end: 1 })
        );
        assert_eq!(
            ByteSpan::validate_in(&ascii, 2, 1),
            Err(SpanError::InvertedRange { start: 2, end: 1 })
        );
        assert_eq!(
            ByteSpan::validate_in(&multibyte, 0, 4),
            Err(SpanError::OutOfBounds { offset: 4, len: 3 })
        );
        assert_eq!(
            ByteSpan::validate_in(&multibyte, 0, 1),
            Err(SpanError::NotCharBoundary { offset: 1 })
        );
        assert_eq!(
            ByteSpan::empty_at(offset(1)).slice_in(&multibyte),
            Err(SpanError::NotCharBoundary { offset: 1 })
        );
        assert_eq!(
            ByteSpan::empty_at(offset(4)).slice_in(&multibyte),
            Err(SpanError::OutOfBounds { offset: 4, len: 3 })
        );
        assert_eq!(
            ByteSpan::new(offset(0), offset(1))
                .unwrap()
                .slice_in(&multibyte),
            Err(SpanError::NotCharBoundary { offset: 1 })
        );
        assert_eq!(
            ByteSpan::new(offset(0), offset(4))
                .unwrap()
                .slice_in(&multibyte),
            Err(SpanError::OutOfBounds { offset: 4, len: 3 })
        );
        let a = ByteSpan::new(offset(0), offset(3)).unwrap();
        let b = ByteSpan::new(offset(1), offset(4)).unwrap();
        let intersection = Some(ByteSpan::new(offset(1), offset(3)).unwrap());
        assert_eq!(a.intersect_closed(b), intersection);
        assert_eq!(a.strict_intersect(b), intersection);
        let left = ByteSpan::new(offset(0), offset(1)).unwrap();
        let right = ByteSpan::new(offset(1), offset(2)).unwrap();
        assert_eq!(
            left.intersect_closed(right),
            Some(ByteSpan::empty_at(offset(1)))
        );
        assert_eq!(left.strict_intersect(right), None);
        assert!(!left.overlaps_open(right));
        assert!(left.contacts_closed(right));
    }
}
