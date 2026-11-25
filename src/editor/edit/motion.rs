//! Cursor and selection motion within a visible buffer range.
//!
//! `ByteOffset`, `ByteSpan`, and `Selection` are values without text identity.
//! Every operation revalidates the view and its inputs against the current
//! buffer text and visible range. Equal offsets from another text cannot be
//! distinguished. Lines use LF separators; CR is ordinary content.

use std::fmt;

use crate::editor::text::search::{self, SearchError};
use crate::editor::text::span::ByteSpan;
use crate::editor::text::utf8::{ByteOffset, Text};
use crate::editor::workspace::catalog::BufferCatalog;
use crate::editor::workspace::selections::{Selection, SelectionError};
use crate::editor::workspace::view::{BufferView, ViewError};

/// A motion could not be performed against the current buffer and view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MotionError {
    /// The view or point is invalid.
    View(ViewError),
    /// The selection is invalid.
    Selection(SelectionError),
    /// The search pattern or span is invalid.
    Search(SearchError),
}

impl fmt::Display for MotionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::View(error) => write!(f, "{error}"),
            Self::Selection(error) => write!(f, "{error}"),
            Self::Search(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for MotionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::View(error) => Some(error),
            Self::Selection(error) => Some(error),
            Self::Search(error) => Some(error),
        }
    }
}

fn checked<'a>(
    catalog: &'a BufferCatalog,
    view: &BufferView,
    point: ByteOffset,
) -> Result<&'a Text, MotionError> {
    view.visible_text(catalog).map_err(MotionError::View)?;
    let text = catalog
        .get(view.buffer())
        .expect("visible text requires a live buffer");
    ByteSpan::validate_in(text, point.as_usize(), point.as_usize())
        .map_err(|error| MotionError::View(ViewError::InvalidSpan(error)))?;
    view.check_target(view.buffer(), ByteSpan::empty_at(point))
        .map_err(MotionError::View)?;
    Ok(text)
}

/// Moves by Unicode scalar values, clipping at the inclusive visible endpoints.
/// Unclipped movement followed by its inverse restores the original point.
pub fn move_scalars(
    catalog: &BufferCatalog,
    view: &BufferView,
    point: ByteOffset,
    delta: isize,
) -> Result<ByteOffset, MotionError> {
    let text = checked(catalog, view, point)?;
    let p = point.as_usize();
    let visible = view.visible();
    let result = if delta >= 0 {
        text.as_str()[p..visible.end().as_usize()]
            .char_indices()
            .nth(delta as usize)
            .map_or(visible.end().as_usize(), |(i, _)| p + i)
    } else {
        text.as_str()[visible.start().as_usize()..p]
            .char_indices()
            .rev()
            .nth(delta.unsigned_abs() - 1)
            .map_or(visible.start().as_usize(), |(i, _)| {
                visible.start().as_usize() + i
            })
    };
    Ok(text.validate_offset(result).expect("scalar boundary"))
}

/// Moves to the start of the current visible line; applying it twice is unchanged.
pub fn line_start(
    catalog: &BufferCatalog,
    view: &BufferView,
    point: ByteOffset,
) -> Result<ByteOffset, MotionError> {
    let text = checked(catalog, view, point)?;
    let start = view.visible().start().as_usize();
    let prefix = &text.as_str()[start..point.as_usize()];
    let result = prefix.rfind('\n').map_or(start, |i| start + i + 1);
    Ok(text.validate_offset(result).expect("line boundary"))
}

/// Moves to the end of the current visible line, before LF, or the visible end.
pub fn line_end(
    catalog: &BufferCatalog,
    view: &BufferView,
    point: ByteOffset,
) -> Result<ByteOffset, MotionError> {
    let text = checked(catalog, view, point)?;
    let p = point.as_usize();
    let suffix = &text.as_str()[p..view.visible().end().as_usize()];
    let result = suffix
        .find('\n')
        .map_or(view.visible().end().as_usize(), |i| p + i);
    Ok(text.validate_offset(result).expect("line boundary"))
}

/// Search direction. Matches are compared by their start offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchDirection {
    /// The first match starting strictly after the point.
    Next,
    /// The last match starting strictly before the point.
    Previous,
}

/// Finds the next or previous literal match fully contained in the view.
/// Matches may overlap the point. There is no wraparound; empty patterns fail.
pub fn search_motion(
    catalog: &BufferCatalog,
    view: &BufferView,
    point: ByteOffset,
    pattern: &str,
    direction: SearchDirection,
) -> Result<Option<ByteSpan>, MotionError> {
    let text = checked(catalog, view, point)?;
    let matches =
        search::find_in_span(text, view.visible(), pattern).map_err(MotionError::Search)?;
    Ok(match direction {
        SearchDirection::Next => matches.into_iter().find(|span| span.start() > point),
        SearchDirection::Previous => matches.into_iter().rev().find(|span| span.start() < point),
    })
}

/// Moves the selection head to `head`, preserving its anchor.
pub fn extend_to(
    catalog: &BufferCatalog,
    view: &BufferView,
    selection: Selection,
    head: ByteOffset,
) -> Result<Selection, MotionError> {
    checked(catalog, view, head)?;
    selection
        .validate(catalog, view)
        .map_err(MotionError::Selection)?;
    Selection::new(catalog, view, selection.buffer(), selection.anchor(), head)
        .map_err(MotionError::Selection)
}

/// Collapses a valid selection to a caret at `point`.
pub fn collapse_to(
    catalog: &BufferCatalog,
    view: &BufferView,
    selection: Selection,
    point: ByteOffset,
) -> Result<Selection, MotionError> {
    checked(catalog, view, point)?;
    selection
        .validate(catalog, view)
        .map_err(MotionError::Selection)?;
    Selection::new(catalog, view, selection.buffer(), point, point).map_err(MotionError::Selection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::edit::ids::BufferId;
    use crate::editor::test_support::{assert_law, texts};
    use crate::editor::text::span::SpanError;

    fn setup(source: &str) -> (Text, BufferCatalog, BufferView) {
        let text = Text::from_str(source);
        let mut catalog = BufferCatalog::new();
        let id = BufferId::new(1);
        catalog.register(id, text.clone()).unwrap();
        let view = BufferView::full(&catalog, id).unwrap();
        (text, catalog, view)
    }

    #[test]
    fn scalar_and_line_laws_over_narrowed_views() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let (text, catalog, full) = setup(source);
                let points = text.char_boundaries();
                for (i, &start) in points.iter().enumerate() {
                    for (j, &end) in points.iter().enumerate().skip(i) {
                        let view = full
                            .narrow(&catalog, ByteSpan::new(start, end).unwrap())
                            .unwrap();
                        for (k, &point) in points.iter().enumerate().take(j + 1).skip(i) {
                            for delta in [-100, -3, -1, 0, 1, 3, 100, isize::MIN, isize::MAX] {
                                let moved = move_scalars(&catalog, &view, point, delta).unwrap();
                                let expected = k.saturating_add_signed(delta).clamp(i, j);
                                assert_eq!(moved, points[expected]);
                                if let Some(inverse) = delta.checked_neg() {
                                    if (i..=j).contains(&k.saturating_add_signed(delta))
                                        && k.checked_add_signed(delta).is_some()
                                    {
                                        assert_eq!(
                                            move_scalars(&catalog, &view, moved, inverse).unwrap(),
                                            point
                                        );
                                    }
                                }
                            }
                            let a = line_start(&catalog, &view, point).unwrap();
                            let b = line_end(&catalog, &view, point).unwrap();
                            assert_eq!(line_start(&catalog, &view, a).unwrap(), a);
                            assert_eq!(line_end(&catalog, &view, b).unwrap(), b);
                            let before = &source[start.as_usize()..point.as_usize()];
                            let after = &source[point.as_usize()..end.as_usize()];
                            assert_eq!(
                                a.as_usize(),
                                before
                                    .rfind('\n')
                                    .map_or(start.as_usize(), |n| start.as_usize() + n + 1)
                            );
                            assert_eq!(
                                b.as_usize(),
                                after
                                    .find('\n')
                                    .map_or(end.as_usize(), |n| point.as_usize() + n)
                            );
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn searches_overlap_and_narrowing() {
        let (text, catalog, full) = setup("xあああz");
        let p = |n| text.validate_offset(n).unwrap();
        let view = full
            .narrow(&catalog, ByteSpan::new(p(1), p(10)).unwrap())
            .unwrap();
        let find = |point, pattern, direction| {
            search_motion(&catalog, &view, p(point), pattern, direction)
        };
        assert_eq!(
            find(1, "ああ", SearchDirection::Next).unwrap(),
            Some(ByteSpan::new(p(4), p(10)).unwrap())
        );
        assert_eq!(
            find(10, "ああ", SearchDirection::Previous).unwrap(),
            Some(ByteSpan::new(p(4), p(10)).unwrap())
        );
        assert_eq!(
            find(7, "ああ", SearchDirection::Previous).unwrap(),
            Some(ByteSpan::new(p(4), p(10)).unwrap())
        );
        assert_eq!(find(1, "ああ", SearchDirection::Previous).unwrap(), None);
        assert_eq!(find(10, "あ", SearchDirection::Next).unwrap(), None);
        assert_eq!(
            find(1, "", SearchDirection::Next),
            Err(MotionError::Search(SearchError::EmptyPattern))
        );
        let narrow = full
            .narrow(&catalog, ByteSpan::new(p(4), p(7)).unwrap())
            .unwrap();
        let right_edge = full
            .narrow(&catalog, ByteSpan::new(p(1), p(7)).unwrap())
            .unwrap();
        assert_eq!(
            search_motion(&catalog, &right_edge, p(1), "ああ", SearchDirection::Next),
            Ok(None)
        );
        assert_eq!(
            search_motion(&catalog, &narrow, p(4), "ああ", SearchDirection::Next),
            Ok(None)
        );
        assert_eq!(
            search_motion(&catalog, &narrow, p(7), "ああ", SearchDirection::Previous),
            Ok(None)
        );
    }

    #[test]
    fn selections_and_stale_values_are_checked() {
        let (original, mut shortened_catalog, stale_view) = setup("abcdef");
        let caret = original.validate_offset(1).unwrap();
        let valid_selection = Selection::new(
            &shortened_catalog,
            &stale_view,
            stale_view.buffer(),
            caret,
            caret,
        )
        .unwrap();
        shortened_catalog
            .update(stale_view.buffer(), Text::from_str("abc"))
            .unwrap();
        assert_eq!(
            collapse_to(&shortened_catalog, &stale_view, valid_selection, caret),
            Err(MotionError::View(ViewError::InvalidSpan(
                SpanError::OutOfBounds { offset: 6, len: 3 }
            )))
        );
        let (text, mut catalog, full) = setup("abcd");
        let p = |n| text.validate_offset(n).unwrap();
        let view = full
            .narrow(&catalog, ByteSpan::new(p(1), p(3)).unwrap())
            .unwrap();
        let selection = Selection::new(&catalog, &view, view.buffer(), p(3), p(1)).unwrap();
        let extended = extend_to(&catalog, &view, selection, p(2)).unwrap();
        assert_eq!((extended.anchor(), extended.head()), (p(3), p(2)));
        let collapsed = collapse_to(&catalog, &view, selection, p(2)).unwrap();
        assert_eq!((collapsed.anchor(), collapsed.head()), (p(2), p(2)));
        assert!(matches!(
            extend_to(&catalog, &view, selection, p(0)),
            Err(MotionError::View(ViewError::SpanOutsideVisible { .. }))
        ));
        catalog
            .update(view.buffer(), Text::from_str("界d"))
            .unwrap();
        let fresh = BufferView::full(&catalog, view.buffer()).unwrap();
        assert_eq!(
            extend_to(&catalog, &fresh, selection, p(4)),
            Err(MotionError::Selection(SelectionError::InvalidSpan(
                SpanError::NotCharBoundary { offset: 1 }
            )))
        );
        assert_eq!(
            move_scalars(&catalog, &view, p(3), 0),
            Err(MotionError::View(ViewError::InvalidSpan(
                SpanError::NotCharBoundary { offset: 1 }
            )))
        );
        assert_eq!(
            line_end(&catalog, &fresh, p(1)),
            Err(MotionError::View(ViewError::InvalidSpan(
                SpanError::NotCharBoundary { offset: 1 }
            )))
        );
        assert!(matches!(
            collapse_to(&catalog, &fresh, selection, p(0)),
            Err(MotionError::Selection(SelectionError::InvalidSpan(_)))
        ));
        catalog.update(view.buffer(), Text::from_str("a")).unwrap();
        assert!(matches!(
            line_start(&catalog, &fresh, p(0)),
            Err(MotionError::View(ViewError::InvalidSpan(
                SpanError::OutOfBounds { .. }
            )))
        ));
        catalog.remove(view.buffer()).unwrap();
        assert!(matches!(
            line_start(&catalog, &fresh, p(0)),
            Err(MotionError::View(ViewError::BufferClosed(_)))
        ));
    }
}
