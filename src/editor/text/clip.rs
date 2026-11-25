//! UTF-8 clips with byte coordinates in the original text.
//!
//! Nested clips use original coordinates, so restricting a clip to a contained
//! span has the same result as clipping the original text directly.

use super::lines::{LineError, LineIndex};
use super::span::{ByteSpan, SpanError};
use super::utf8::Text;
use std::fmt;
use std::ops::Range;

/// A failure to construct or restrict a clip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipError {
    /// A byte span is invalid for the original text.
    Span(SpanError),
    /// A requested span is not contained in its parent clip.
    OutsideParent {
        requested: ByteSpan,
        parent: ByteSpan,
    },
    /// A line range has inverted endpoints.
    InvertedLines { start: usize, end: usize },
    /// A line endpoint exceeds the number of lines.
    LineOutOfRange { line: usize, line_count: usize },
    /// The supplied index was not built for this text's line boundaries.
    IndexMismatch,
    /// A line lookup failed.
    Line(LineError),
}

impl fmt::Display for ClipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Span(error) => write!(f, "{error}"),
            Self::OutsideParent { requested, parent } => {
                write!(f, "span {requested:?} is outside parent {parent:?}")
            }
            Self::InvertedLines { start, end } => {
                write!(f, "line range start {start} exceeds end {end}")
            }
            Self::LineOutOfRange { line, line_count } => {
                write!(f, "line endpoint {line} exceeds line count {line_count}")
            }
            Self::IndexMismatch => write!(f, "line index does not match text"),
            Self::Line(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ClipError {}

impl From<SpanError> for ClipError {
    fn from(error: SpanError) -> Self {
        Self::Span(error)
    }
}

impl From<LineError> for ClipError {
    fn from(error: LineError) -> Self {
        Self::Line(error)
    }
}

/// An immutable slice and its span in the original text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Clip {
    source: Text,
    origin: ByteSpan,
}

impl Clip {
    /// Clips `text` to a span, revalidating offsets from other texts.
    pub fn new(text: &Text, span: ByteSpan) -> Result<Self, ClipError> {
        span.slice_in(text)?;
        Ok(Self {
            source: text.clone(),
            origin: span,
        })
    }

    /// Validates raw original byte coordinates and clips the text.
    pub fn from_range(text: &Text, range: Range<usize>) -> Result<Self, ClipError> {
        Self::new(text, ByteSpan::validate_in(text, range.start, range.end)?)
    }

    /// Returns the exact original byte span.
    pub fn origin(&self) -> ByteSpan {
        self.origin
    }

    /// Borrows the clipped content.
    pub fn as_str(&self) -> &str {
        &self.source.as_str()[self.origin.start().as_usize()..self.origin.end().as_usize()]
    }

    /// Restricts this clip using original, not relative, byte coordinates.
    pub fn clip(&self, span: ByteSpan) -> Result<Self, ClipError> {
        span.slice_in(&self.source)?;
        if !self.origin.contains_span(span) {
            return Err(ClipError::OutsideParent {
                requested: span,
                parent: self.origin,
            });
        }
        Self::new(&self.source, span)
    }

    /// Returns the longest character-boundary prefix within `max_bytes` and
    /// whether any bytes were omitted.
    pub fn truncate(&self, max_bytes: usize) -> (Self, bool) {
        let content = self.as_str();
        let mut end = max_bytes.min(content.len());
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        let end = self.origin.start().as_usize() + end;
        let span = ByteSpan::validate_in(&self.source, self.origin.start().as_usize(), end)
            .expect("clip and prefix boundaries are valid");
        (
            Self {
                source: self.source.clone(),
                origin: span,
            },
            end < self.origin.end().as_usize(),
        )
    }
}

/// Clips a document to original byte coordinates.
pub fn clip(text: &Text, span: ByteSpan) -> Result<Clip, ClipError> {
    Clip::new(text, span)
}

/// Clips a document and limits the result to a UTF-8-safe byte prefix.
pub fn clip_with_byte_limit(
    text: &Text,
    span: ByteSpan,
    max_bytes: usize,
) -> Result<(Clip, bool), ClipError> {
    Ok(Clip::new(text, span)?.truncate(max_bytes))
}

/// Clips a half-open line range, including LF after each selected line if present.
/// An empty range is anchored at the corresponding line start, or at EOF.
pub fn clip_lines(text: &Text, index: &LineIndex, lines: Range<usize>) -> Result<Clip, ClipError> {
    let expected = LineIndex::from_text(text);
    if index.line_count() != expected.line_count()
        || (0..index.line_count())
            .any(|line| index.line_span_with_lf(line) != expected.line_span_with_lf(line))
    {
        return Err(ClipError::IndexMismatch);
    }
    if lines.start > lines.end {
        return Err(ClipError::InvertedLines {
            start: lines.start,
            end: lines.end,
        });
    }
    let count = index.line_count();
    if lines.end > count {
        return Err(ClipError::LineOutOfRange {
            line: lines.end,
            line_count: count,
        });
    }
    let start = if lines.start == count {
        text.len_bytes()
    } else {
        index.line_span(lines.start)?.start().as_usize()
    };
    let end = if lines.start == lines.end {
        start
    } else {
        index.line_span_with_lf(lines.end - 1)?.end().as_usize()
    };
    Clip::from_range(text, start..end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};

    #[test]
    fn nested_clips_and_budgets_obey_laws() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let text = Text::from_str(source);
                let boundaries = text.char_boundaries();
                for (i, &start) in boundaries.iter().enumerate() {
                    for (j, &end) in boundaries.iter().enumerate().skip(i) {
                        let span = ByteSpan::new(start, end).unwrap();
                        let direct = clip(&text, span).unwrap();
                        assert_eq!(direct.as_str(), span.slice_in(&text).unwrap());
                        assert_eq!(direct.origin(), span);
                        let outer = clip(&text, ByteSpan::full_of(&text)).unwrap();
                        assert_eq!(outer.clip(span).unwrap(), direct);
                        for (left, &inner_start) in boundaries[i..=j].iter().enumerate() {
                            for &inner_end in &boundaries[i + left..=j] {
                                let inner = ByteSpan::new(inner_start, inner_end).unwrap();
                                assert_eq!(
                                    direct.clip(inner).unwrap(),
                                    clip(&text, inner).unwrap()
                                );
                            }
                        }
                        for budget in 0..=span.len_bytes() + 1 {
                            let (limited, truncated) =
                                clip_with_byte_limit(&text, span, budget).unwrap();
                            let prefix = limited.as_str();
                            assert!(prefix.len() <= budget && direct.as_str().starts_with(prefix));
                            assert_eq!(truncated, prefix.len() < direct.as_str().len());
                            assert_eq!(limited.origin().start(), start);
                            assert_eq!(limited.origin().slice_in(&text).unwrap(), prefix);
                            if truncated {
                                assert!(
                                    prefix.len()
                                        + direct.as_str()[prefix.len()..]
                                            .chars()
                                            .next()
                                            .unwrap()
                                            .len_utf8()
                                        > budget
                                );
                            }
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn line_ranges_match_direct_spans() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let text = Text::from_str(source);
                let index = LineIndex::from_text(&text);
                let starts: Vec<_> = std::iter::once(0)
                    .chain(source.match_indices('\n').map(|(i, _)| i + 1))
                    .chain(std::iter::once(source.len()))
                    .collect();
                for start in 0..=index.line_count() {
                    for end in start..=index.line_count() {
                        let actual = clip_lines(&text, &index, start..end).unwrap();
                        assert_eq!(
                            actual,
                            Clip::from_range(&text, starts[start]..starts[end]).unwrap()
                        );
                    }
                }
            }
        });
    }

    #[test]
    fn rejects_invalid_and_foreign_spans() {
        let text = Text::from_str("a界😀z");
        let ascii = Text::from_str("0123456789");
        let span = |a, b| {
            ByteSpan::new(
                ascii.validate_offset(a).unwrap(),
                ascii.validate_offset(b).unwrap(),
            )
            .unwrap()
        };
        assert!(matches!(
            Clip::from_range(&text, Range { start: 4, end: 1 }),
            Err(ClipError::Span(SpanError::InvertedRange { .. }))
        ));
        assert!(matches!(
            clip(&text, span(0, 10)),
            Err(ClipError::Span(SpanError::OutOfBounds { .. }))
        ));
        assert!(matches!(
            clip(&text, span(0, 2)),
            Err(ClipError::Span(SpanError::NotCharBoundary { .. }))
        ));
        let parent = Clip::from_range(&text, 1..8).unwrap();
        assert!(matches!(
            parent.clip(span(0, 1)),
            Err(ClipError::OutsideParent { .. })
        ));
        let index = LineIndex::from_text(&text);
        assert!(matches!(
            clip_lines(&text, &index, Range { start: 1, end: 0 }),
            Err(ClipError::InvertedLines { .. })
        ));
        assert!(matches!(
            clip_lines(&text, &index, 0..2),
            Err(ClipError::LineOutOfRange { .. })
        ));
        assert_eq!(
            clip_lines(&text, &LineIndex::from_text(&Text::from_str("x\ny")), 0..1),
            Err(ClipError::IndexMismatch)
        );
    }

    #[test]
    fn generated_invalid_ranges_are_rejected() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let text = Text::from_str(source);
                assert!(matches!(
                    Clip::from_range(&text, 0..source.len() + 1),
                    Err(ClipError::Span(SpanError::OutOfBounds { .. }))
                ));
                for offset in 0..source.len() {
                    if !source.is_char_boundary(offset) {
                        assert!(matches!(
                            Clip::from_range(&text, 0..offset),
                            Err(ClipError::Span(SpanError::NotCharBoundary { .. }))
                        ));
                    }
                }
                let boundaries = text.char_boundaries();
                for &offset in boundaries.iter().skip(1) {
                    let parent = Clip::new(&text, ByteSpan::empty_at(offset)).unwrap();
                    let outside = ByteSpan::empty_at(boundaries[0]);
                    assert!(matches!(
                        parent.clip(outside),
                        Err(ClipError::OutsideParent { .. })
                    ));
                }
            }
        });
    }
}
