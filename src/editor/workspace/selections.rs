//! Oriented selections within visible buffer ranges.
//!
//! Swapping anchor and head twice restores both fields. The minimum and maximum
//! of the endpoints do not depend on their order, so flipping preserves the
//! range. Equal endpoints are precisely the fixed points; otherwise swapping
//! reverses their ordering and exchanges forward and backward directions.
//!
//! Validation compares the buffer and both ordered endpoints to the view,
//! then revalidates the endpoints against live text. Thus mismatches, endpoints
//! outside the inclusive visible bounds, and stale UTF-8 boundaries fail.
//! A view may itself have become stale independently of the selection.
//!
//! Actor operations touch only one map key. Therefore any pair of set or
//! clear operations on distinct actors commute and leave other keys intact.

use std::collections::BTreeMap;
use std::fmt;

use crate::editor::edit::ids::{ActorId, BufferId};
use crate::editor::text::span::{ByteSpan, SpanError};
use crate::editor::text::utf8::ByteOffset;
use crate::editor::workspace::catalog::BufferCatalog;
use crate::editor::workspace::view::{BufferView, ViewError};

/// The ordering of a selection's anchor and head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Anchor and head coincide.
    Empty,
    /// Anchor precedes head.
    Forward,
    /// Anchor follows head.
    Backward,
}

/// A selection failed validation against a view or live buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectionError {
    /// The selection and view identify different buffers.
    BufferMismatch {
        expected: BufferId,
        actual: BufferId,
    },
    /// At least one endpoint lies outside the visible range.
    SpanOutsideVisible {
        requested: ByteSpan,
        visible: ByteSpan,
    },
    /// The buffer has never been registered.
    UnknownBuffer(BufferId),
    /// The buffer has been closed.
    BufferClosed(BufferId),
    /// An endpoint is invalid in the current text.
    InvalidSpan(SpanError),
}

impl fmt::Display for SelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BufferMismatch { expected, actual } => {
                write!(f, "buffer mismatch: expected {expected}, got {actual}")
            }
            Self::SpanOutsideVisible { requested, visible } => {
                write!(f, "span {requested:?} is outside visible range {visible:?}")
            }
            Self::UnknownBuffer(id) => write!(f, "unknown buffer {id}"),
            Self::BufferClosed(id) => write!(f, "buffer {id} is closed"),
            Self::InvalidSpan(error) => write!(f, "invalid span: {error}"),
        }
    }
}

impl std::error::Error for SelectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidSpan(error) => Some(error),
            _ => None,
        }
    }
}

/// An oriented anchor and head in a buffer, analogous to mark/point or visual start/head.
///
/// Edits can invalidate offsets; call [`Selection::validate`] before using a
/// stored selection against the current buffer and view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    buffer: BufferId,
    anchor: ByteOffset,
    head: ByteOffset,
}

impl Selection {
    /// Constructs a selection after checking the view and current text.
    pub fn new(
        catalog: &BufferCatalog,
        view: &BufferView,
        buffer: BufferId,
        anchor: ByteOffset,
        head: ByteOffset,
    ) -> Result<Self, SelectionError> {
        let selection = Self {
            buffer,
            anchor,
            head,
        };
        selection.validate(catalog, view)?;
        Ok(selection)
    }

    /// Returns the buffer identifier.
    pub fn buffer(self) -> BufferId {
        self.buffer
    }
    /// Returns the fixed endpoint.
    pub fn anchor(self) -> ByteOffset {
        self.anchor
    }
    /// Returns the active endpoint.
    pub fn head(self) -> ByteOffset {
        self.head
    }

    /// Returns the ordered, half-open range regardless of orientation.
    pub fn range(self) -> ByteSpan {
        ByteSpan::new(self.anchor.min(self.head), self.anchor.max(self.head))
            .expect("ordered endpoints")
    }

    /// Returns the orientation of the endpoints.
    pub fn direction(self) -> Direction {
        match self.anchor.cmp(&self.head) {
            std::cmp::Ordering::Equal => Direction::Empty,
            std::cmp::Ordering::Less => Direction::Forward,
            std::cmp::Ordering::Greater => Direction::Backward,
        }
    }

    /// Swaps anchor and head, preserving the buffer and range.
    pub fn flip(self) -> Self {
        Self {
            buffer: self.buffer,
            anchor: self.head,
            head: self.anchor,
        }
    }

    /// Checks view containment and revalidates both endpoints in live text.
    pub fn validate(
        self,
        catalog: &BufferCatalog,
        view: &BufferView,
    ) -> Result<(), SelectionError> {
        view.check_target(self.buffer, self.range())
            .map_err(|error| match error {
                ViewError::BufferMismatch { expected, actual } => {
                    SelectionError::BufferMismatch { expected, actual }
                }
                ViewError::SpanOutsideVisible { requested, visible } => {
                    SelectionError::SpanOutsideVisible { requested, visible }
                }
                _ => unreachable!("target checks only compare buffer and range"),
            })?;
        let text = catalog.get(self.buffer).ok_or_else(|| {
            if catalog.is_tombstoned(self.buffer) {
                SelectionError::BufferClosed(self.buffer)
            } else {
                SelectionError::UnknownBuffer(self.buffer)
            }
        })?;
        ByteSpan::validate_in(
            text,
            self.range().start().as_usize(),
            self.range().end().as_usize(),
        )
        .map_err(SelectionError::InvalidSpan)?;
        Ok(())
    }
}

/// Independent selections indexed by actor. Stored selections may become stale.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActorSelections {
    selections: BTreeMap<ActorId, Selection>,
}

impl ActorSelections {
    /// Creates an empty selection map.
    pub fn new() -> Self {
        Self::default()
    }
    /// Returns an actor's selection, if present.
    pub fn get(&self, actor: ActorId) -> Option<Selection> {
        self.selections.get(&actor).copied()
    }
    /// Sets an actor's selection, returning the previous value.
    pub fn set(&mut self, actor: ActorId, selection: Selection) -> Option<Selection> {
        self.selections.insert(actor, selection)
    }
    /// Removes an actor's selection, returning the previous value.
    pub fn clear(&mut self, actor: ActorId) -> Option<Selection> {
        self.selections.remove(&actor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};
    use crate::editor::text::utf8::Text;

    #[test]
    fn orientation_and_view_laws_over_corpus() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let text = Text::from_str(source);
                let points = text.char_boundaries();
                let id = BufferId::new(1);
                let mut catalog = BufferCatalog::new();
                catalog.register(id, text).unwrap();
                let full = BufferView::full(&catalog, id).unwrap();
                for &anchor in &points {
                    for &head in &points {
                        let selection = Selection::new(&catalog, &full, id, anchor, head).unwrap();
                        let flipped = selection.flip();
                        assert_eq!(
                            (flipped.buffer(), flipped.anchor(), flipped.head()),
                            (id, head, anchor)
                        );
                        assert_eq!(
                            (selection.buffer(), selection.anchor(), selection.head()),
                            (id, anchor, head)
                        );
                        assert_eq!(flipped.flip(), selection);
                        assert_eq!(
                            selection.range(),
                            ByteSpan::new(anchor.min(head), anchor.max(head)).unwrap()
                        );
                        assert_eq!(selection.range(), flipped.range());
                        let expected = match anchor.cmp(&head) {
                            std::cmp::Ordering::Equal => Direction::Empty,
                            std::cmp::Ordering::Less => Direction::Forward,
                            std::cmp::Ordering::Greater => Direction::Backward,
                        };
                        assert_eq!(selection.direction(), expected);
                        assert_eq!(
                            flipped.direction(),
                            match expected {
                                Direction::Empty => Direction::Empty,
                                Direction::Forward => Direction::Backward,
                                Direction::Backward => Direction::Forward,
                            }
                        );
                        assert_eq!(flipped == selection, expected == Direction::Empty);
                        for &start in &points {
                            for &end in points.iter().filter(|&&end| end >= start) {
                                let visible = ByteSpan::new(start, end).unwrap();
                                let view = BufferView::narrowed(&catalog, id, visible).unwrap();
                                let expected = if visible.contains_offset(anchor)
                                    && visible.contains_offset(head)
                                {
                                    Ok(())
                                } else {
                                    Err(SelectionError::SpanOutsideVisible {
                                        requested: selection.range(),
                                        visible,
                                    })
                                };
                                assert_eq!(selection.validate(&catalog, &view), expected);
                            }
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn rejects_mismatch_and_stale_endpoints() {
        let id = BufferId::new(1);
        let other = BufferId::new(2);
        let ascii = Text::from_str("abcd");
        let p = |n| ascii.validate_offset(n).unwrap();
        let mut catalog = BufferCatalog::new();
        catalog.register(id, ascii.clone()).unwrap();
        let view = BufferView::full(&catalog, id).unwrap();
        assert_eq!(
            Selection::new(&catalog, &view, other, p(0), p(1)),
            Err(SelectionError::BufferMismatch {
                expected: id,
                actual: other
            })
        );
        let inside = Selection::new(&catalog, &view, id, p(1), p(0)).unwrap();
        let beyond = Selection::new(&catalog, &view, id, p(4), p(0)).unwrap();
        catalog.update(id, Text::from_str("界")).unwrap();
        assert_eq!(
            inside.validate(&catalog, &view),
            Err(SelectionError::InvalidSpan(SpanError::NotCharBoundary {
                offset: 1
            }))
        );
        assert_eq!(
            beyond.validate(&catalog, &view),
            Err(SelectionError::InvalidSpan(SpanError::OutOfBounds {
                offset: 4,
                len: 3
            }))
        );
        assert_eq!(
            Selection::new(&catalog, &view, id, p(0), p(1)),
            Err(SelectionError::InvalidSpan(SpanError::NotCharBoundary {
                offset: 1
            }))
        );
        catalog.remove(id).unwrap();
        assert_eq!(
            inside.validate(&catalog, &view),
            Err(SelectionError::BufferClosed(id))
        );
        assert_eq!(
            inside.validate(&BufferCatalog::new(), &view),
            Err(SelectionError::UnknownBuffer(id))
        );
    }

    #[test]
    fn stale_boundaries_on_either_side_over_corpus() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let live = Text::from_str(source);
                let original = Text::from_str(&"a".repeat(live.len_bytes() + 1));
                let id = BufferId::new(1);
                let mut catalog = BufferCatalog::new();
                catalog.register(id, original.clone()).unwrap();
                let view = BufferView::full(&catalog, id).unwrap();
                catalog.update(id, live.clone()).unwrap();
                for offset in 0..=live.len_bytes() + 1 {
                    if live.validate_offset(offset).is_ok() {
                        continue;
                    }
                    let invalid = original.validate_offset(offset).unwrap();
                    for valid in live.char_boundaries() {
                        for (anchor, head) in [(invalid, valid), (valid, invalid)] {
                            let expected = if offset > live.len_bytes() {
                                SpanError::OutOfBounds {
                                    offset,
                                    len: live.len_bytes(),
                                }
                            } else {
                                SpanError::NotCharBoundary { offset }
                            };
                            assert_eq!(
                                Selection::new(&catalog, &view, id, anchor, head),
                                Err(SelectionError::InvalidSpan(expected))
                            );
                        }
                    }
                }
                for anchor in original.char_boundaries() {
                    for head in original.char_boundaries() {
                        let start = anchor.min(head).as_usize();
                        let end = anchor.max(head).as_usize();
                        let expected = ByteSpan::validate_in(&live, start, end)
                            .map(|_| ())
                            .map_err(SelectionError::InvalidSpan);
                        assert_eq!(
                            Selection::new(&catalog, &view, id, anchor, head).map(|_| ()),
                            expected
                        );
                    }
                }
            }
        });
    }

    #[test]
    fn distinct_actor_operations_commute() {
        assert_law(
            &[0],
            |rng| texts(rng).into_iter().take(8).collect::<Vec<_>>(),
            |corpus| {
                for source in corpus {
                    let text = Text::from_str(source);
                    let id = BufferId::new(1);
                    let mut catalog = BufferCatalog::new();
                    catalog.register(id, text.clone()).unwrap();
                    let view = BufferView::full(&catalog, id).unwrap();
                    let points = text.char_boundaries();
                    let first_point = points[0];
                    let last_point = points[points.len() - 1];
                    let selections = [
                        Selection::new(&catalog, &view, id, first_point, last_point).unwrap(),
                        Selection::new(&catalog, &view, id, last_point, first_point).unwrap(),
                        Selection::new(&catalog, &view, id, last_point, last_point).unwrap(),
                    ];
                    let (a, b, c) = (ActorId::new(1), ActorId::new(2), ActorId::new(3));
                    let all: Vec<_> = points
                        .iter()
                        .flat_map(|&anchor| points.iter().map(move |&head| (anchor, head)))
                        .map(|(anchor, head)| {
                            Selection::new(&catalog, &view, id, anchor, head).unwrap()
                        })
                        .collect();
                    for &anchor in &points {
                        for &head in &points {
                            let value = Selection::new(&catalog, &view, id, anchor, head).unwrap();
                            for (index, other) in std::iter::once(None)
                                .chain(all.iter().copied().map(Some))
                                .enumerate()
                            {
                                let initial_b = all[(index + 1) % all.len()];
                                let value_b = other.unwrap_or(value);
                                for set in [false, true] {
                                    for set_b in [false, true] {
                                        let mut left = ActorSelections::new();
                                        if let Some(existing) = other {
                                            left.set(a, existing);
                                            left.set(c, existing);
                                        }
                                        left.set(b, initial_b);
                                        let mut right = left.clone();
                                        if set {
                                            left.set(a, value);
                                        } else {
                                            left.clear(a);
                                        }
                                        assert_eq!(
                                            (left.get(a), left.get(b), left.get(c)),
                                            (set.then_some(value), Some(initial_b), other)
                                        );
                                        if set_b {
                                            left.set(b, value_b);
                                        } else {
                                            left.clear(b);
                                        }
                                        assert_eq!(
                                            (left.get(a), left.get(b), left.get(c)),
                                            (set.then_some(value), set_b.then_some(value_b), other)
                                        );
                                        if set_b {
                                            right.set(b, value_b);
                                        } else {
                                            right.clear(b);
                                        }
                                        assert_eq!(
                                            (right.get(a), right.get(b), right.get(c)),
                                            (other, set_b.then_some(value_b), other)
                                        );
                                        if set {
                                            right.set(a, value);
                                        } else {
                                            right.clear(a);
                                        }
                                        assert_eq!(
                                            (right.get(a), right.get(b), right.get(c)),
                                            (set.then_some(value), set_b.then_some(value_b), other)
                                        );
                                        assert_eq!(left, right);
                                    }
                                }
                            }
                        }
                    }
                    let initial_values = [
                        None,
                        Some(selections[0]),
                        Some(selections[selections.len() - 1]),
                    ];
                    for &initial_a in &initial_values {
                        for &initial_b in &initial_values {
                            for &initial_c in &initial_values {
                                for &first in &selections {
                                    for &second in &selections {
                                        for set_a in [false, true] {
                                            for set_b in [false, true] {
                                                let mut left = ActorSelections::new();
                                                for (actor, value) in
                                                    [(a, initial_a), (b, initial_b), (c, initial_c)]
                                                {
                                                    if let Some(value) = value {
                                                        left.set(actor, value);
                                                    }
                                                }
                                                let mut right = left.clone();
                                                let op =
                                                |map: &mut ActorSelections, actor, set, value| {
                                                    if set {
                                                        map.set(actor, value)
                                                    } else {
                                                        map.clear(actor)
                                                    }
                                                };
                                                let la = op(&mut left, a, set_a, first);
                                                assert_eq!(
                                                    (left.get(b), left.get(c)),
                                                    (initial_b, initial_c)
                                                );
                                                let lb = op(&mut left, b, set_b, second);
                                                assert_eq!(
                                                    (left.get(a), left.get(c)),
                                                    (set_a.then_some(first), initial_c)
                                                );
                                                let rb = op(&mut right, b, set_b, second);
                                                assert_eq!(
                                                    (right.get(a), right.get(c)),
                                                    (initial_a, initial_c)
                                                );
                                                let ra = op(&mut right, a, set_a, first);
                                                assert_eq!(
                                                    (right.get(b), right.get(c)),
                                                    (set_b.then_some(second), initial_c)
                                                );
                                                assert_eq!((la, lb), (ra, rb));
                                                assert_eq!(left, right);
                                                assert_eq!(left.get(c), initial_c);
                                                assert_eq!(left.get(a), set_a.then_some(first));
                                                assert_eq!(left.get(b), set_b.then_some(second));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            },
        );
    }
}
