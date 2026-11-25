//! Literal UTF-8 substring search over validated byte spans.
//!
//! Each character boundary in the search span is visited once in ascending
//! order. A boundary is returned exactly when the remaining span starts with
//! the pattern, so this agrees with a brute-force boundary scan and includes
//! overlaps. The bounded slice prevents matches extending beyond the span.
//! Each match starts and ends at UTF-8 boundaries, has the pattern's byte
//! length, and slices to the pattern. Nonempty patterns ensure positive spans.
//! Empty patterns are rejected. Both span endpoints are revalidated before
//! slicing: out-of-bounds and non-character-boundary offsets are rejected,
//! including offsets from another text. `ByteSpan` stores no text identity,
//! so a span from another text with valid endpoints cannot be distinguished
//! from a span constructed for this text and is accepted.

use super::span::{ByteSpan, SpanError};
use super::utf8::Text;
use std::fmt;

/// A literal search could not be performed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SearchError {
    /// Empty patterns are not supported.
    EmptyPattern,
    /// The search span is invalid for the supplied text.
    InvalidSpan(SpanError),
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPattern => write!(f, "search pattern is empty"),
            Self::InvalidSpan(error) => write!(f, "invalid search span: {error}"),
        }
    }
}

impl std::error::Error for SearchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EmptyPattern => None,
            Self::InvalidSpan(error) => Some(error),
        }
    }
}

/// Finds all literal matches in the entire text, including overlaps.
pub fn find_all(text: &Text, pattern: &str) -> Result<Vec<ByteSpan>, SearchError> {
    find_in_span(text, ByteSpan::full_of(text), pattern)
}

/// Finds all literal matches contained in `span`, in ascending start order.
///
/// Empty patterns are rejected before span validation. Both endpoints are
/// revalidated against `text`, including for spans from another text.
pub fn find_in_span(
    text: &Text,
    span: ByteSpan,
    pattern: &str,
) -> Result<Vec<ByteSpan>, SearchError> {
    if pattern.is_empty() {
        return Err(SearchError::EmptyPattern);
    }
    let slice = span.slice_in(text).map_err(SearchError::InvalidSpan)?;
    let base = span.start().as_usize();
    Ok(slice
        .char_indices()
        .filter(|(start, _)| slice[*start..].starts_with(pattern))
        .map(|(start, _)| {
            ByteSpan::validate_in(text, base + start, base + start + pattern.len())
                .expect("a UTF-8 match has valid endpoints")
        })
        .collect())
}

/// Finds the first literal match within `span`.
pub fn find_first(
    text: &Text,
    span: ByteSpan,
    pattern: &str,
) -> Result<Option<ByteSpan>, SearchError> {
    Ok(find_in_span(text, span, pattern)?.into_iter().next())
}

/// Finds the last literal match within `span`.
pub fn find_last(
    text: &Text,
    span: ByteSpan,
    pattern: &str,
) -> Result<Option<ByteSpan>, SearchError> {
    Ok(find_in_span(text, span, pattern)?.into_iter().last())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhaustive_boundary_scan_laws() {
        let corpus = [
            "",
            "a",
            "aaa",
            "ababa",
            "界界a",
            "e\u{301}e\u{301}",
            "😀a😀",
        ];
        let patterns = [
            "a", "aa", "aba", "b", "界", "界界", "e\u{301}", "\u{301}", "😀", "😀a", "x", "aaaa",
        ];
        for source in corpus {
            let text = Text::from_str(source);
            let boundaries = text.char_boundaries();
            for (i, &start) in boundaries.iter().enumerate() {
                for &end in &boundaries[i..] {
                    let span = ByteSpan::new(start, end).unwrap();
                    assert_eq!(
                        find_in_span(&text, span, ""),
                        Err(SearchError::EmptyPattern)
                    );
                    assert_eq!(find_first(&text, span, ""), Err(SearchError::EmptyPattern));
                    assert_eq!(find_last(&text, span, ""), Err(SearchError::EmptyPattern));
                    for pattern in patterns {
                        let expected: Vec<_> = boundaries
                            .iter()
                            .copied()
                            .filter(|&offset| {
                                let position = offset.as_usize();
                                position >= start.as_usize()
                                    && position <= end.as_usize()
                                    && pattern.len() <= end.as_usize() - position
                                    && source[position..].starts_with(pattern)
                            })
                            .map(|offset| {
                                ByteSpan::validate_in(
                                    &text,
                                    offset.as_usize(),
                                    offset.as_usize() + pattern.len(),
                                )
                                .unwrap()
                            })
                            .collect();
                        let actual = find_in_span(&text, span, pattern).unwrap();
                        assert_eq!(actual, expected, "{source:?} {span:?} {pattern:?}");
                        assert!(actual
                            .windows(2)
                            .all(|pair| pair[0].start() < pair[1].start()));
                        for &found in &actual {
                            assert!(span.contains_span(found));
                            assert_eq!(found.len_bytes(), pattern.len());
                            assert_eq!(found.slice_in(&text), Ok(pattern));
                        }
                        assert_eq!(
                            find_first(&text, span, pattern).unwrap(),
                            actual.first().copied()
                        );
                        assert_eq!(
                            find_last(&text, span, pattern).unwrap(),
                            actual.last().copied()
                        );
                        if span == ByteSpan::full_of(&text) {
                            assert_eq!(find_all(&text, pattern).unwrap(), actual);
                        }
                    }
                }
            }
            assert_eq!(find_all(&text, ""), Err(SearchError::EmptyPattern));
            let foreign = Text::from_str(&"a".repeat(source.len() + 2));
            for start in 0..=foreign.len_bytes() {
                for end in start..=foreign.len_bytes() {
                    let span = ByteSpan::validate_in(&foreign, start, end).unwrap();
                    let invalid = [start, end].into_iter().find_map(|offset| {
                        if offset > source.len() {
                            Some(SpanError::OutOfBounds {
                                offset,
                                len: source.len(),
                            })
                        } else if !source.is_char_boundary(offset) {
                            Some(SpanError::NotCharBoundary { offset })
                        } else {
                            None
                        }
                    });
                    if let Some(error) = invalid {
                        let expected = Err(SearchError::InvalidSpan(error));
                        assert_eq!(find_in_span(&text, span, "a"), expected);
                        assert_eq!(find_first(&text, span, "a"), expected.clone().map(|_| None));
                        assert_eq!(find_last(&text, span, "a"), expected.map(|_| None));
                    }
                }
            }
        }
    }

    #[test]
    fn rejects_empty_patterns_and_invalid_foreign_spans() {
        let ascii = Text::from_str("abcd");
        let target = Text::from_str("界");
        let span = |start, end| ByteSpan::validate_in(&ascii, start, end).unwrap();
        assert_eq!(find_all(&target, ""), Err(SearchError::EmptyPattern));
        for search in [span(0, 1), span(1, 3)] {
            assert_eq!(
                find_in_span(&target, search, "a"),
                Err(SearchError::InvalidSpan(SpanError::NotCharBoundary {
                    offset: 1
                }))
            );
        }
        for search in [span(0, 4), span(4, 4)] {
            assert_eq!(
                find_in_span(&target, search, "a"),
                Err(SearchError::InvalidSpan(SpanError::OutOfBounds {
                    offset: 4,
                    len: 3
                }))
            );
        }
        assert_eq!(
            find_first(&target, span(0, 1), ""),
            Err(SearchError::EmptyPattern)
        );
        assert_eq!(
            find_last(&target, span(0, 4), "a"),
            Err(SearchError::InvalidSpan(SpanError::OutOfBounds {
                offset: 4,
                len: 3
            }))
        );
        assert_eq!(
            find_in_span(&target, span(0, 3), "界").unwrap(),
            vec![ByteSpan::full_of(&target)]
        );
    }
}
