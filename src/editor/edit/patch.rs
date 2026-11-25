//! Disjoint replacements expressed in the coordinates of one base text.
//!
//! Sorting by start and end makes normalization independent of input order.
//! A patch is accepted exactly when each sorted pair satisfies
//! `previous.end < next.start`: closed-interval contact, including duplicate
//! empty spans, is rejected. Each gap therefore contains untouched bytes.
//!
//! Forward reconstruction copies each gap and replacement in order. Applying
//! the same splices from right to left gives the same concatenation, since an
//! edit to the right cannot change coordinates to its left. In the result,
//! inverse spans cover the inserted bytes and retain the original nonempty gaps;
//! they are consequently strictly separated even when replacements are empty.
//! Replacing those inserted bytes with the saved original slices restores every
//! byte. The empty patch copies the input unchanged. Every application checks
//! both endpoints against the supplied text before any slicing.

use crate::editor::edit::splice::{Splice, SpliceError};
use crate::editor::text::span::ByteSpan;
use crate::editor::text::utf8::Text;
use std::fmt;

/// Splices in canonical ascending base-coordinate order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Patch {
    splices: Vec<Splice>,
}

/// A patch contains contacting spans or an endpoint invalid for its target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatchError {
    /// Two spans intersect as closed intervals.
    Contact { previous: ByteSpan, next: ByteSpan },
    /// An endpoint is invalid for the supplied text.
    InvalidSplice(SpliceError),
}

impl fmt::Display for PatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Contact { previous, next } => {
                write!(f, "contacting spans {previous:?} and {next:?}")
            }
            Self::InvalidSplice(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for PatchError {}

impl From<SpliceError> for PatchError {
    fn from(error: SpliceError) -> Self {
        Self::InvalidSplice(error)
    }
}

impl Patch {
    /// Sorts splices and rejects all closed-interval contact.
    pub fn new(mut splices: Vec<Splice>) -> Result<Self, PatchError> {
        splices.sort_by_key(|splice| (splice.span().start(), splice.span().end()));
        for pair in splices.windows(2) {
            let previous = pair[0].span();
            let next = pair[1].span();
            if previous.end() >= next.start() {
                return Err(PatchError::Contact { previous, next });
            }
        }
        Ok(Self { splices })
    }

    /// Returns the canonical splices.
    pub fn splices(&self) -> &[Splice] {
        &self.splices
    }

    /// Reconstructs the target from its unchanged gaps and replacements.
    pub fn apply(&self, text: &Text) -> Result<Text, PatchError> {
        self.apply_with_inverse(text).map(|(next, _)| next)
    }

    /// Returns the patch that restores this specific input after application.
    pub fn invert(&self, text: &Text) -> Result<Self, PatchError> {
        self.apply_with_inverse(text).map(|(_, inverse)| inverse)
    }

    /// Applies the patch and returns its exact inverse in result coordinates.
    pub fn apply_with_inverse(&self, text: &Text) -> Result<(Text, Self), PatchError> {
        let mut output = String::new();
        let mut removed = Vec::with_capacity(self.splices.len());
        let mut cursor = 0;
        for splice in &self.splices {
            let span = splice.span();
            let original = span.slice_in(text).map_err(SpliceError::from)?;
            let start = span.start().as_usize();
            output.push_str(&text.as_str()[cursor..start]);
            let inverse_start = output.len();
            output.push_str(splice.replacement().as_str());
            removed.push((inverse_start, output.len(), Text::from_str(original)));
            cursor = span.end().as_usize();
        }
        output.push_str(&text.as_str()[cursor..]);
        let next = Text::from_str(&output);
        let mut inverse = Vec::with_capacity(removed.len());
        for (start, end, original) in removed {
            let span = ByteSpan::validate_in(&next, start, end).map_err(SpliceError::from)?;
            inverse.push(Splice::new(span, original));
        }
        Ok((next, Self::new(inverse)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permutations(items: &mut [Splice], at: usize, expected: &[Splice]) {
        if at == items.len() {
            assert_eq!(Patch::new(items.to_vec()).unwrap().splices(), expected);
        } else {
            for i in at..items.len() {
                items.swap(at, i);
                permutations(items, at + 1, expected);
                items.swap(at, i);
            }
        }
    }

    fn verify_sets(text: &Text, spans: &[ByteSpan], selected: &mut Vec<Splice>, first: usize) {
        let patch = Patch::new(selected.clone()).unwrap();
        permutations(&mut selected.clone(), 0, selected);
        let (next, inverse) = patch.apply_with_inverse(text).unwrap();
        let mut sequential = text.clone();
        for splice in selected.iter().rev() {
            sequential = splice.apply(&sequential).unwrap();
        }
        assert_eq!(next, sequential);
        let mut cursor = 0;
        let mut inverse_cursor = 0;
        let mut expected_inverse = Vec::new();
        for splice in selected.iter() {
            let span = splice.span();
            inverse_cursor += span.start().as_usize() - cursor;
            let end = inverse_cursor + splice.replacement().len_bytes();
            let inverse_span = ByteSpan::validate_in(&next, inverse_cursor, end).unwrap();
            expected_inverse.push(Splice::new(
                inverse_span,
                Text::from_str(span.slice_in(text).unwrap()),
            ));
            inverse_cursor = end;
            cursor = span.end().as_usize();
        }
        assert_eq!(inverse.splices(), expected_inverse);
        assert_eq!(patch.apply(text), Ok(next.clone()));
        assert_eq!(patch.invert(text), Ok(inverse.clone()));
        assert_eq!(inverse.apply(&next), Ok(text.clone()));
        for i in first..spans.len() {
            if selected
                .last()
                .is_some_and(|last| last.span().end() >= spans[i].start())
            {
                continue;
            }
            for replacement in ["", "X", "界😀"] {
                selected.push(Splice::new(spans[i], Text::from_str(replacement)));
                verify_sets(text, spans, selected, i + 1);
                selected.pop();
            }
        }
    }

    #[test]
    fn exhaustive_small_utf8_corpus() {
        for source in ["", "abc", "\n\r", "界a", "e\u{301}x", "😀a", "a😀\r"] {
            let text = Text::from_str(source);
            let boundaries = text.char_boundaries();
            let mut spans = Vec::new();
            for (i, &start) in boundaries.iter().enumerate() {
                for &end in &boundaries[i..] {
                    spans.push(ByteSpan::new(start, end).unwrap());
                }
            }
            for &a in &spans {
                for &b in &spans {
                    let result = Patch::new(vec![Splice::delete(a), Splice::delete(b)]);
                    assert_eq!(
                        result.is_err(),
                        a.contacts_closed(b),
                        "{source:?}: {a:?}, {b:?}"
                    );
                    if a.contacts_closed(b) {
                        assert!(matches!(result, Err(PatchError::Contact { .. })));
                    }
                }
            }
            verify_sets(&text, &spans, &mut Vec::new(), 0);
        }
    }

    #[test]
    fn revalidates_each_endpoint_against_target() {
        for source in ["", "abc", "\n\r", "界a", "e\u{301}x", "😀a", "a😀\r"] {
            let target = Text::from_str(source);
            let ascii = Text::from_str(&"a".repeat(target.len_bytes() + 1));
            for start in 0..=ascii.len_bytes() {
                for end in start..=ascii.len_bytes() {
                    let span = ByteSpan::validate_in(&ascii, start, end).unwrap();
                    let patch = Patch::new(vec![Splice::delete(span)]).unwrap();
                    if let Err(error) = ByteSpan::validate_in(&target, start, end) {
                        let error = PatchError::InvalidSplice(error.into());
                        assert_eq!(patch.apply(&target), Err(error.clone()));
                        assert_eq!(patch.invert(&target), Err(error.clone()));
                        assert_eq!(patch.apply_with_inverse(&target), Err(error));
                    } else {
                        assert!(patch.apply_with_inverse(&target).is_ok());
                    }
                }
            }
        }
    }

    #[test]
    fn rejects_invalid_later_splices() {
        let ascii = Text::from_str("abc");
        let zero = ByteSpan::validate_in(&ascii, 0, 0).unwrap();
        let one = ByteSpan::validate_in(&ascii, 1, 1).unwrap();
        assert_eq!(
            Patch::new(vec![
                Splice::delete(zero),
                Splice::delete(one),
                Splice::delete(one)
            ]),
            Err(PatchError::Contact {
                previous: one,
                next: one
            })
        );
        let foreign = ByteSpan::validate_in(&ascii, 1, 2).unwrap();
        let patch = Patch::new(vec![Splice::delete(zero), Splice::delete(foreign)]).unwrap();
        let target = Text::from_str("a😀\r");
        assert_eq!(
            patch.apply_with_inverse(&target),
            Err(PatchError::InvalidSplice(SpliceError::NotCharBoundary {
                offset: 2
            }))
        );
    }
}
