//! Closed-interval edit footprints and conflict detection.
//!
//! A pair conflicts exactly when it belongs to the same buffer and both
//! `left.start <= right.end` and `right.start <= left.end`. Swapping operands
//! exchanges these inequalities, proving symmetry. Disjoint buffers have no
//! eligible pair, and an empty footprint has no pair at all, including with
//! itself. Enumerating every eligible pair gives the direct quadratic reference
//! predicate; `first_conflict` and `conflicts` select the first such pair and
//! its existence, while `all_conflicts` collects the same pairs. Thus their
//! existence results agree. Endpoints are inclusive for this comparison, so
//! touching edits and insertions at either endpoint conflict.

use std::collections::BTreeMap;

use crate::editor::edit::{ids::BufferId, patch::Patch, splice::Splice};
use crate::editor::text::span::ByteSpan;

/// A conflicting pair of spans in a single buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpanConflict {
    /// Buffer containing both spans.
    pub buffer: BufferId,
    /// Span from the left footprint.
    pub left: ByteSpan,
    /// Span from the right footprint.
    pub right: ByteSpan,
}

/// Spans grouped by buffer and sorted by start and end within each buffer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Footprint {
    spans: BTreeMap<BufferId, Vec<ByteSpan>>,
}

impl Footprint {
    /// Extracts the replaced span of a splice, including an empty span.
    pub fn from_splice(buffer: BufferId, splice: &Splice) -> Self {
        Self {
            spans: BTreeMap::from([(buffer, vec![splice.span()])]),
        }
    }

    /// Extracts all replaced spans of a patch.
    pub fn from_patch(buffer: BufferId, patch: &Patch) -> Self {
        Self::from_patches([(buffer, patch)])
    }

    /// Extracts spans from buffer and patch entries; repeated buffers combine.
    pub fn from_patches<'a>(entries: impl IntoIterator<Item = (BufferId, &'a Patch)>) -> Self {
        let mut spans: BTreeMap<BufferId, Vec<ByteSpan>> = BTreeMap::new();
        for (buffer, patch) in entries {
            if !patch.splices().is_empty() {
                spans
                    .entry(buffer)
                    .or_default()
                    .extend(patch.splices().iter().map(Splice::span));
            }
        }
        for buffer_spans in spans.values_mut() {
            buffer_spans.sort_by_key(|span| (span.start(), span.end()));
        }
        Self { spans }
    }

    /// Returns the per-buffer spans in deterministic buffer and span order.
    pub fn spans(&self) -> &BTreeMap<BufferId, Vec<ByteSpan>> {
        &self.spans
    }

    /// Returns whether any span pair conflicts.
    pub fn conflicts(&self, other: &Self) -> bool {
        self.first_conflict(other).is_some()
    }

    /// Returns the first conflict in buffer, left-span, then right-span order.
    pub fn first_conflict(&self, other: &Self) -> Option<SpanConflict> {
        self.conflict_iter(other).next()
    }

    /// Returns every conflicting pair in deterministic order, retaining duplicates.
    pub fn all_conflicts(&self, other: &Self) -> Vec<SpanConflict> {
        self.conflict_iter(other).collect()
    }

    fn conflict_iter<'a>(&'a self, other: &'a Self) -> impl Iterator<Item = SpanConflict> + 'a {
        self.spans.iter().flat_map(move |(&buffer, left)| {
            other.spans.get(&buffer).into_iter().flat_map(move |right| {
                left.iter().flat_map(move |&left| {
                    right.iter().copied().filter_map(move |right| {
                        spans_conflict(left, right).then_some(SpanConflict {
                            buffer,
                            left,
                            right,
                        })
                    })
                })
            })
        })
    }
}

/// Tests closed-interval intersection, including endpoint contact.
pub fn spans_conflict(left: ByteSpan, right: ByteSpan) -> bool {
    left.start() <= right.end() && right.start() <= left.end()
}

/// Returns whether two footprints contain a conflicting pair.
pub fn conflicts(left: &Footprint, right: &Footprint) -> bool {
    left.conflicts(right)
}

/// Returns the first conflicting pair, if any.
pub fn first_conflict(left: &Footprint, right: &Footprint) -> Option<SpanConflict> {
    left.first_conflict(right)
}

/// Returns all conflicting pairs, retaining duplicates.
pub fn all_conflicts(left: &Footprint, right: &Footprint) -> Vec<SpanConflict> {
    left.all_conflicts(right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::Rng;
    use crate::editor::text::utf8::Text;

    fn span(start: usize, end: usize) -> ByteSpan {
        ByteSpan::validate_in(&Text::from_str("abcdefghijklmnop"), start, end).unwrap()
    }

    fn reference(left: &Footprint, right: &Footprint) -> Vec<SpanConflict> {
        let mut result = Vec::new();
        for (&buffer, left_spans) in left.spans() {
            for (&other_buffer, right_spans) in right.spans() {
                for &l in left_spans {
                    for &r in right_spans {
                        if buffer == other_buffer && l.start() <= r.end() && r.start() <= l.end() {
                            result.push(SpanConflict {
                                buffer,
                                left: l,
                                right: r,
                            });
                        }
                    }
                }
            }
        }
        result
    }

    fn generated(rng: &mut Rng) -> Footprint {
        let mut patches = Vec::new();
        for _ in 0..rng.gen_range(0..8) {
            let start = rng.gen_range(0..17);
            let end = rng.gen_range(start..17);
            let splice = if start == end {
                Splice::insert(span(start, end).start(), Text::from_str("x"))
            } else {
                Splice::delete(span(start, end))
            };
            patches.push((
                BufferId::new(rng.gen_range(0..4) as u64),
                Patch::new(vec![splice]).unwrap(),
            ));
        }
        Footprint::from_patches(patches.iter().map(|(id, patch)| (*id, patch)))
    }

    #[test]
    fn endpoint_and_construction_examples() {
        let id = BufferId::new(1);
        let other = BufferId::new(2);
        let replacement = Splice::delete(span(2, 5));
        let footprint = Footprint::from_splice(id, &replacement);
        for point in [2, 5] {
            assert!(conflicts(
                &footprint,
                &Footprint::from_splice(
                    id,
                    &Splice::insert(span(point, point).start(), Text::from_str("x"))
                )
            ));
        }
        let insertion =
            Footprint::from_splice(id, &Splice::insert(span(3, 3).start(), Text::from_str("x")));
        assert!(conflicts(&insertion, &insertion));
        assert!(!conflicts(
            &footprint,
            &Footprint::from_splice(other, &replacement)
        ));
        let patch = Patch::new(vec![Splice::delete(span(7, 8)), replacement]).unwrap();
        assert_eq!(
            Footprint::from_patch(id, &patch).spans()[&id],
            vec![span(2, 5), span(7, 8)]
        );
        assert_eq!(
            Footprint::from_patch(id, &Patch::default()),
            Footprint::default()
        );
        let first = Patch::new(vec![Splice::delete(span(7, 8))]).unwrap();
        let second = Patch::new(vec![Splice::delete(span(2, 5))]).unwrap();
        let merged = Footprint::from_patches([(id, &first), (id, &second), (id, &second)]);
        assert_eq!(
            merged.spans()[&id],
            vec![span(2, 5), span(2, 5), span(7, 8)]
        );
        assert_eq!(all_conflicts(&merged, &footprint).len(), 2);
        let multi = Footprint::from_patches([(other, &first), (id, &second)]);
        assert_eq!(
            multi.spans(),
            &BTreeMap::from([(id, vec![span(2, 5)]), (other, vec![span(7, 8)])])
        );
    }

    #[test]
    fn patches_with_insertions_preserve_spans_and_witnesses() {
        let buffer = BufferId::new(7);
        let point = span(3, 3);
        let left_patch =
            Patch::new(vec![Splice::insert(point.start(), Text::from_str("x"))]).unwrap();
        let right_patch =
            Patch::new(vec![Splice::insert(point.start(), Text::from_str("y"))]).unwrap();
        let left = Footprint::from_patch(buffer, &left_patch);
        let right = Footprint::from_patch(buffer, &right_patch);
        let expected = SpanConflict {
            buffer,
            left: point,
            right: point,
        };
        assert_eq!(left.spans(), &BTreeMap::from([(buffer, vec![point])]));
        assert_eq!(right.spans(), &BTreeMap::from([(buffer, vec![point])]));
        assert_eq!(first_conflict(&left, &right), Some(expected));
        assert_eq!(all_conflicts(&left, &right), vec![expected]);
    }

    #[test]
    fn deterministic_laws_and_witnesses() {
        for start in 0..=8 {
            for end in start..=8 {
                for other_start in 0..=8 {
                    for other_end in other_start..=8 {
                        let (a, b) = (span(start, end), span(other_start, other_end));
                        assert_eq!(spans_conflict(a, b), spans_conflict(b, a));
                        assert_eq!(
                            spans_conflict(a, b),
                            start <= other_end && other_start <= end
                        );
                    }
                }
            }
        }
        for seed in [0, 1, 42, u64::MAX] {
            let mut rng = Rng::new(seed);
            for _ in 0..500 {
                let point = rng.gen_range(0..17);
                let buffer = BufferId::new(rng.gen_range(0..4) as u64);
                let insertion = Splice::insert(span(point, point).start(), Text::from_str("x"));
                let a = Footprint::from_splice(buffer, &insertion);
                let b = Footprint::from_patch(buffer, &Patch::new(vec![insertion]).unwrap());
                let expected = reference(&a, &b);
                assert_eq!(expected.len(), 1);
                assert_eq!(all_conflicts(&a, &b), expected);
                assert_eq!(first_conflict(&a, &b), expected.first().copied());
                assert!(conflicts(&a, &b));
                let (a, b) = (generated(&mut rng), generated(&mut rng));
                let expected = reference(&a, &b);
                assert_eq!(all_conflicts(&a, &b), expected);
                assert_eq!(first_conflict(&a, &b), expected.first().copied());
                assert_eq!(conflicts(&a, &b), !expected.is_empty());
                assert_eq!(conflicts(&a, &b), conflicts(&b, &a));
                assert!(!conflicts(&a, &Footprint::default()));
                assert!(!conflicts(&Footprint::default(), &a));
            }
        }
        assert!(!conflicts(&Footprint::default(), &Footprint::default()));
    }
}
