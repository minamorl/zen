//! Predicate-driven runs of Unicode scalars and their byte boundaries.
//!
//! Offsets and spans are value types, not tied to the text that created them.
//! Every operation revalidates them against its target text. Runs are maximal
//! only within the supplied half-open range. Predicates are called once per
//! scalar, in ascending order, even for navigation queries.

use super::span::{ByteSpan, SpanError};
use super::utf8::{ByteOffset, Text};
use std::fmt;

/// A rejected range or navigation position.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WordError {
    /// A range or offset is invalid for the target text.
    Span(SpanError),
    /// A valid offset satisfies `offset < start || offset > end`; endpoints are accepted.
    OutsideRange {
        offset: usize,
        start: usize,
        end: usize,
    },
}

impl fmt::Display for WordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Span(error) => error.fmt(f),
            Self::OutsideRange { offset, start, end } => {
                write!(f, "offset {offset} is outside range {start}..={end}")
            }
        }
    }
}
impl std::error::Error for WordError {}
impl From<SpanError> for WordError {
    fn from(error: SpanError) -> Self {
        Self::Span(error)
    }
}

/// Unicode alphanumeric scalars and underscore; this is not a language identifier grammar.
pub fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}
/// Alias for the simple identifier character class.
pub fn is_identifier_char(ch: char) -> bool {
    is_word_char(ch)
}
/// Unicode whitespace as defined by `char::is_whitespace`.
pub fn is_whitespace(ch: char) -> bool {
    ch.is_whitespace()
}
/// Scalars outside the word and whitespace classes, including symbols and marks.
pub fn is_punctuation(ch: char) -> bool {
    !is_word_char(ch) && !is_whitespace(ch)
}

/// Returns ascending, nonempty maximal runs matching `predicate` within `range`.
pub fn segments<F: FnMut(char) -> bool>(
    text: &Text,
    range: ByteSpan,
    mut predicate: F,
) -> Result<Vec<ByteSpan>, SpanError> {
    let slice = range.slice_in(text)?;
    let base = range.start().as_usize();
    let mut result = Vec::new();
    let mut start = None;
    for (index, ch) in slice.char_indices() {
        if predicate(ch) {
            start.get_or_insert(base + index);
        } else if let Some(begin) = start.take() {
            result.push(ByteSpan::validate_in(text, begin, base + index)?);
        }
    }
    if let Some(begin) = start {
        result.push(ByteSpan::validate_in(text, begin, range.end().as_usize())?);
    }
    Ok(result)
}

/// Segments the entire text using the same rules as [`segments`].
pub fn segments_all<F: FnMut(char) -> bool>(text: &Text, predicate: F) -> Vec<ByteSpan> {
    segments(text, ByteSpan::full_of(text), predicate).expect("whole text is valid")
}

fn checked_runs<F: FnMut(char) -> bool>(
    text: &Text,
    range: ByteSpan,
    offset: ByteOffset,
    predicate: F,
) -> Result<Vec<ByteSpan>, WordError> {
    // Validate the range first, then the offset, before checking membership.
    range.slice_in(text)?;
    ByteSpan::validate_in(text, offset.as_usize(), offset.as_usize())?;
    if !range.contains_offset(offset) {
        return Err(WordError::OutsideRange {
            offset: offset.as_usize(),
            start: range.start().as_usize(),
            end: range.end().as_usize(),
        });
    }
    Ok(segments(text, range, predicate)?)
}

/// Returns the run containing the scalar at `offset`, or `None` at a gap or range end.
pub fn word_at_offset<F: FnMut(char) -> bool>(
    text: &Text,
    range: ByteSpan,
    offset: ByteOffset,
    predicate: F,
) -> Result<Option<ByteSpan>, WordError> {
    Ok(checked_runs(text, range, offset, predicate)?
        .into_iter()
        .find(|run| run.start() <= offset && offset < run.end()))
}

/// Returns the first run start strictly greater than `offset`.
pub fn next_word_start<F: FnMut(char) -> bool>(
    text: &Text,
    range: ByteSpan,
    offset: ByteOffset,
    predicate: F,
) -> Result<Option<ByteOffset>, WordError> {
    Ok(checked_runs(text, range, offset, predicate)?
        .into_iter()
        .map(ByteSpan::start)
        .find(|&point| point > offset))
}
/// Returns the first run end strictly greater than `offset`.
pub fn next_word_end<F: FnMut(char) -> bool>(
    text: &Text,
    range: ByteSpan,
    offset: ByteOffset,
    predicate: F,
) -> Result<Option<ByteOffset>, WordError> {
    Ok(checked_runs(text, range, offset, predicate)?
        .into_iter()
        .map(ByteSpan::end)
        .find(|&point| point > offset))
}
/// Returns the last run start strictly less than `offset`.
pub fn previous_word_start<F: FnMut(char) -> bool>(
    text: &Text,
    range: ByteSpan,
    offset: ByteOffset,
    predicate: F,
) -> Result<Option<ByteOffset>, WordError> {
    Ok(checked_runs(text, range, offset, predicate)?
        .into_iter()
        .rev()
        .map(ByteSpan::start)
        .find(|&point| point < offset))
}
/// Returns the last run end strictly less than `offset`.
pub fn previous_word_end<F: FnMut(char) -> bool>(
    text: &Text,
    range: ByteSpan,
    offset: ByteOffset,
    predicate: F,
) -> Result<Option<ByteOffset>, WordError> {
    Ok(checked_runs(text, range, offset, predicate)?
        .into_iter()
        .rev()
        .map(ByteSpan::end)
        .find(|&point| point < offset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};

    // Independently classify each scalar, then find the extent of each matched
    // scalar by walking its neighbors. Deduplicate the resulting intervals.
    fn oracle(
        source: &str,
        start: usize,
        end: usize,
        predicate: fn(char) -> bool,
    ) -> Vec<(usize, usize)> {
        let chars: Vec<_> = source
            .char_indices()
            .filter(|(i, _)| *i >= start && *i < end)
            .collect();
        let mut result = Vec::new();
        for i in 0..chars.len() {
            if !predicate(chars[i].1) {
                continue;
            }
            let mut left = i;
            let mut right = i + 1;
            while left > 0 && predicate(chars[left - 1].1) {
                left -= 1;
            }
            while right < chars.len() && predicate(chars[right].1) {
                right += 1;
            }
            let pair = (
                chars[left].0,
                if right == chars.len() {
                    end
                } else {
                    chars[right].0
                },
            );
            if result.last() != Some(&pair) {
                result.push(pair);
            }
        }
        result
    }

    // Derive navigation directly from the matching state on each side.
    fn navigation_oracle(
        source: &str,
        start: usize,
        end: usize,
        pos: usize,
        predicate: fn(char) -> bool,
    ) -> [Option<usize>; 4] {
        let mut answer = [None; 4];
        let mut previous = false;
        for (relative, ch) in source[start..end]
            .char_indices()
            .chain(std::iter::once((end - start, '\0')))
        {
            let boundary = start + relative;
            let current = boundary < end && predicate(ch);
            if !previous && current {
                if boundary > pos && answer[0].is_none() {
                    answer[0] = Some(boundary);
                }
                if boundary < pos {
                    answer[2] = Some(boundary);
                }
            }
            if previous && !current {
                if boundary > pos && answer[1].is_none() {
                    answer[1] = Some(boundary);
                }
                if boundary < pos {
                    answer[3] = Some(boundary);
                }
            }
            previous = current;
        }
        answer
    }

    #[test]
    fn exhaustive_ranges_and_navigation_against_oracle() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus
                .iter()
                .chain(["a_b !界😀\te\u{301}".to_owned(), "...  12".to_owned()].iter())
            {
                let source: String = source.chars().take(8).collect();
                let text = Text::from_str(&source);
                let boundaries = text.char_boundaries();
                for (i, &start) in boundaries.iter().enumerate() {
                    for &end in &boundaries[i..] {
                        let range = ByteSpan::new(start, end).unwrap();
                        for predicate in [
                            is_word_char as fn(char) -> bool,
                            is_whitespace,
                            is_punctuation,
                            |_| true,
                            |_| false,
                            |c| c == '界',
                        ] {
                            let expected =
                                oracle(&source, start.as_usize(), end.as_usize(), predicate);
                            let got = segments(&text, range, predicate).unwrap();
                            assert_eq!(
                                got.iter()
                                    .map(|s| (s.start().as_usize(), s.end().as_usize()))
                                    .collect::<Vec<_>>(),
                                expected
                            );
                            if range == ByteSpan::full_of(&text) {
                                assert_eq!(segments_all(&text, predicate), got);
                            }
                            for &offset in boundaries.iter().filter(|&&p| range.contains_offset(p))
                            {
                                let p = offset.as_usize();
                                let at = expected.iter().find(|&&(a, b)| a <= p && p < b).copied();
                                assert_eq!(
                                    word_at_offset(&text, range, offset, predicate)
                                        .unwrap()
                                        .map(|s| (s.start().as_usize(), s.end().as_usize())),
                                    at
                                );
                                let check =
                                    |result: Result<Option<ByteOffset>, WordError>,
                                     want: Option<usize>| {
                                        assert_eq!(result.unwrap().map(ByteOffset::as_usize), want)
                                    };
                                let nav = navigation_oracle(
                                    &source,
                                    start.as_usize(),
                                    end.as_usize(),
                                    p,
                                    predicate,
                                );
                                check(next_word_start(&text, range, offset, predicate), nav[0]);
                                check(next_word_end(&text, range, offset, predicate), nav[1]);
                                check(previous_word_start(&text, range, offset, predicate), nav[2]);
                                check(previous_word_end(&text, range, offset, predicate), nav[3]);
                            }
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn default_character_classes() {
        for (ch, word, whitespace, punctuation) in [
            ('a', true, false, false),
            ('9', true, false, false),
            ('_', true, false, false),
            ('界', true, false, false),
            ('!', false, false, true),
            ('😀', false, false, true),
            ('\u{0301}', false, false, true),
            (' ', false, true, false),
            ('\t', false, true, false),
            ('\u{3000}', false, true, false),
        ] {
            assert_eq!(is_word_char(ch), word, "{ch:?}");
            assert_eq!(is_identifier_char(ch), word, "{ch:?}");
            assert_eq!(is_whitespace(ch), whitespace, "{ch:?}");
            assert_eq!(is_punctuation(ch), punctuation, "{ch:?}");
        }
    }

    #[test]
    fn revalidates_inputs_and_calls_predicate_in_order() {
        let text = Text::from_str("界a");
        let foreign = Text::from_str("abcde");
        let point = |n| foreign.validate_offset(n).unwrap();
        let bad = ByteSpan::new(point(0), point(1)).unwrap();
        assert_eq!(
            segments(&text, bad, |_| true),
            Err(SpanError::NotCharBoundary { offset: 1 })
        );
        let full = ByteSpan::full_of(&text);
        assert_eq!(
            word_at_offset(&text, full, point(1), |_| true),
            Err(WordError::Span(SpanError::NotCharBoundary { offset: 1 }))
        );
        assert_eq!(
            next_word_end(&text, full, point(5), |_| true),
            Err(WordError::Span(SpanError::OutOfBounds {
                offset: 5,
                len: 4
            }))
        );
        let small = ByteSpan::validate_in(&text, 0, 3).unwrap();
        assert_eq!(
            previous_word_start(&text, small, point(4), |_| true),
            Err(WordError::OutsideRange {
                offset: 4,
                start: 0,
                end: 3
            })
        );
        let mut seen = String::new();
        let _ = next_word_start(&text, full, point(0), |c| {
            seen.push(c);
            true
        })
        .unwrap();
        assert_eq!(seen, "界a");
        assert!(is_identifier_char('_'));
    }
}
