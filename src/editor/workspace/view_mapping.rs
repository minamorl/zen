//! Transport views and oriented selections, deferring clipping until the end.
//!
//! For each endpoint, `PositionMap::compose` evaluates exactly the same fixed
//! affinity through each constituent map. Therefore mapping an unclipped
//! selection through a composition equals sequential mapping, provided each
//! intermediate text and endpoint is valid. Applying the same final clamp to
//! equal endpoints proves deferred-clip functoriality. Clipping earlier is not
//! equivalent in general: it can discard positions that later return to view.
//!
//! Clamping to validated view boundaries preserves endpoint ordering whenever
//! the results differ. The final selection is checked against the live view
//! and text, proving containment and UTF-8 boundary validity. Maps describe
//! byte geometry only: callers supply the source length, and are responsible
//! for ensuring that the map describes the actual edit history.
//! A different buffer ID is rejected, catalog lookup distinguishes unknown
//! and closed buffers, and source and target lengths are compared to the map.
//! Every mapped endpoint is checked against the target text before clipping;
//! invalid UTF-8 boundaries and out-of-domain positions return typed errors.

use std::fmt;

use crate::editor::edit::ids::BufferId;
use crate::editor::text::anchors::Affinity;
use crate::editor::text::position_map::{PositionMap, PositionMapError};
use crate::editor::text::span::ByteSpan;
use crate::editor::text::utf8::{ByteOffset, Text, Utf8Error};
use crate::editor::workspace::catalog::BufferCatalog;
use crate::editor::workspace::selections::{Selection, SelectionError};
use crate::editor::workspace::view::{BufferView, ViewError};

/// A failure to transport or clip a view or selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewMappingError {
    /// The selection and target view refer to different buffers.
    BufferMismatch {
        expected: BufferId,
        actual: BufferId,
    },
    /// The supplied source length differs from the map domain.
    SourceLengthMismatch { expected: usize, actual: usize },
    /// The live target length differs from the map target.
    TargetLengthMismatch { expected: usize, actual: usize },
    /// The buffer is not registered.
    UnknownBuffer(BufferId),
    /// The buffer has been closed.
    BufferClosed(BufferId),
    /// Independent endpoint affinities inverted a visible range.
    InvertedView { start: usize, end: usize },
    /// A mapped endpoint is not valid in the live text.
    InvalidOffset(Utf8Error),
    /// An endpoint was outside the map domain.
    PositionMap(PositionMapError),
    /// A view failed validation.
    View(ViewError),
    /// A selection failed validation.
    Selection(SelectionError),
}

impl fmt::Display for ViewMappingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "view mapping error: {self:?}")
    }
}

impl std::error::Error for ViewMappingError {}

fn live(catalog: &BufferCatalog, buffer: BufferId) -> Result<&Text, ViewMappingError> {
    catalog.get(buffer).ok_or_else(|| {
        if catalog.is_tombstoned(buffer) {
            ViewMappingError::BufferClosed(buffer)
        } else {
            ViewMappingError::UnknownBuffer(buffer)
        }
    })
}

fn target<'a>(
    catalog: &'a BufferCatalog,
    buffer: BufferId,
    map: &PositionMap,
    source_len: usize,
) -> Result<&'a Text, ViewMappingError> {
    let text = live(catalog, buffer)?;
    if map.source_len() != source_len {
        return Err(ViewMappingError::SourceLengthMismatch {
            expected: map.source_len(),
            actual: source_len,
        });
    }
    if map.target_len() != text.len_bytes() {
        return Err(ViewMappingError::TargetLengthMismatch {
            expected: map.target_len(),
            actual: text.len_bytes(),
        });
    }
    Ok(text)
}

fn mapped(
    map: &PositionMap,
    text: &Text,
    offset: ByteOffset,
    affinity: Affinity,
) -> Result<ByteOffset, ViewMappingError> {
    let offset = map
        .map_offset(offset.as_usize(), affinity)
        .map_err(ViewMappingError::PositionMap)?;
    text.validate_offset(offset)
        .map_err(ViewMappingError::InvalidOffset)
}

/// Maps visible endpoints with independent affinities. Equal endpoints produce
/// an empty view; inverted endpoints are rejected rather than silently sorted.
pub fn map_view(
    catalog: &BufferCatalog,
    view: &BufferView,
    map: &PositionMap,
    source_len: usize,
    start_affinity: Affinity,
    end_affinity: Affinity,
) -> Result<BufferView, ViewMappingError> {
    let text = target(catalog, view.buffer(), map, source_len)?;
    let start = mapped(map, text, view.visible().start(), start_affinity)?;
    let end = mapped(map, text, view.visible().end(), end_affinity)?;
    if start > end {
        return Err(ViewMappingError::InvertedView {
            start: start.as_usize(),
            end: end.as_usize(),
        });
    }
    let span = ByteSpan::new(start, end).expect("ordered endpoints");
    BufferView::narrowed(catalog, view.buffer(), span).map_err(ViewMappingError::View)
}

/// Maps both selection endpoints without clipping, retaining their identities.
/// Pass the same affinities on every sequential transport.
pub fn map_selection(
    catalog: &BufferCatalog,
    selection: Selection,
    map: &PositionMap,
    source_len: usize,
    anchor_affinity: Affinity,
    head_affinity: Affinity,
) -> Result<Selection, ViewMappingError> {
    let text = target(catalog, selection.buffer(), map, source_len)?;
    let anchor = mapped(map, text, selection.anchor(), anchor_affinity)?;
    let head = mapped(map, text, selection.head(), head_affinity)?;
    let full = BufferView::full(catalog, selection.buffer()).map_err(ViewMappingError::View)?;
    Selection::new(catalog, &full, selection.buffer(), anchor, head)
        .map_err(ViewMappingError::Selection)
}

/// Clamps both endpoints to the inclusive visible boundaries after validating
/// the view and the original endpoints against the current live text.
pub fn clip_selection(
    catalog: &BufferCatalog,
    selection: Selection,
    view: &BufferView,
) -> Result<Selection, ViewMappingError> {
    if selection.buffer() != view.buffer() {
        return Err(ViewMappingError::BufferMismatch {
            expected: view.buffer(),
            actual: selection.buffer(),
        });
    }
    let text = live(catalog, view.buffer())?;
    let lo = text
        .validate_offset(view.visible().start().as_usize())
        .map_err(ViewMappingError::InvalidOffset)?;
    let hi = text
        .validate_offset(view.visible().end().as_usize())
        .map_err(ViewMappingError::InvalidOffset)?;
    let anchor = text
        .validate_offset(selection.anchor().as_usize())
        .map_err(ViewMappingError::InvalidOffset)?
        .clamp(lo, hi);
    let head = text
        .validate_offset(selection.head().as_usize())
        .map_err(ViewMappingError::InvalidOffset)?
        .clamp(lo, hi);
    Selection::new(catalog, view, view.buffer(), anchor, head).map_err(ViewMappingError::Selection)
}

/// Transports without intermediate clipping, then clips to the target view.
pub fn map_and_clip_selection(
    catalog: &BufferCatalog,
    selection: Selection,
    view: &BufferView,
    map: &PositionMap,
    source_len: usize,
    anchor_affinity: Affinity,
    head_affinity: Affinity,
) -> Result<Selection, ViewMappingError> {
    if selection.buffer() != view.buffer() {
        return Err(ViewMappingError::BufferMismatch {
            expected: view.buffer(),
            actual: selection.buffer(),
        });
    }
    clip_selection(
        catalog,
        map_selection(
            catalog,
            selection,
            map,
            source_len,
            anchor_affinity,
            head_affinity,
        )?,
        view,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::text::position_map::ByteReplacement;

    fn edits(source: &str) -> Vec<(String, PositionMap)> {
        let text = Text::from_str(source);
        let mut result = Vec::new();
        for start in text.char_boundaries() {
            for end in text
                .char_boundaries()
                .into_iter()
                .filter(|&end| end >= start)
            {
                for inserted in ["", "x", "界"] {
                    let (s, e) = (start.as_usize(), end.as_usize());
                    let value = format!("{}{}{}", &source[..s], inserted, &source[e..]);
                    let map = PositionMap::from_replacement(
                        source.len(),
                        ByteReplacement {
                            start: s,
                            end: e,
                            inserted_len: inserted.len(),
                        },
                    )
                    .unwrap();
                    result.push((value, map));
                }
            }
        }
        result
    }

    fn catalog(value: &str) -> BufferCatalog {
        let mut catalog = BufferCatalog::new();
        catalog
            .register(BufferId::new(1), Text::from_str(value))
            .unwrap();
        catalog
    }

    #[test]
    fn generated_deferred_clipping_and_containment() {
        let id = BufferId::new(1);
        for source in ["", "a", "界", "a界"] {
            let initial = catalog(source);
            let full = BufferView::full(&initial, id).unwrap();
            let points = initial.get(id).unwrap().char_boundaries();
            for (middle, first) in edits(source) {
                let intermediate = catalog(&middle);
                for (end, second) in edits(&middle) {
                    let final_catalog = catalog(&end);
                    let final_points = final_catalog.get(id).unwrap().char_boundaries();
                    let composed = first.compose(&second).unwrap();
                    for &anchor in &points {
                        for &head in &points {
                            let selection =
                                Selection::new(&initial, &full, id, anchor, head).unwrap();
                            for aa in [Affinity::Left, Affinity::Right] {
                                for ha in [Affinity::Left, Affinity::Right] {
                                    let once = map_selection(
                                        &final_catalog,
                                        selection,
                                        &composed,
                                        source.len(),
                                        aa,
                                        ha,
                                    )
                                    .unwrap();
                                    let middle_selection = map_selection(
                                        &intermediate,
                                        selection,
                                        &first,
                                        source.len(),
                                        aa,
                                        ha,
                                    )
                                    .unwrap();
                                    let twice = map_selection(
                                        &final_catalog,
                                        middle_selection,
                                        &second,
                                        middle.len(),
                                        aa,
                                        ha,
                                    )
                                    .unwrap();
                                    assert_eq!(once, twice);
                                    assert_eq!(
                                        once.anchor().as_usize(),
                                        composed.map_offset(anchor.as_usize(), aa).unwrap()
                                    );
                                    assert_eq!(
                                        once.head().as_usize(),
                                        composed.map_offset(head.as_usize(), ha).unwrap()
                                    );
                                    for &start in &final_points {
                                        for &end in final_points.iter().filter(|&&end| end >= start)
                                        {
                                            let view = BufferView::narrowed(
                                                &final_catalog,
                                                id,
                                                ByteSpan::new(start, end).unwrap(),
                                            )
                                            .unwrap();
                                            let clipped =
                                                clip_selection(&final_catalog, twice, &view)
                                                    .unwrap();
                                            assert_eq!(
                                                clipped.anchor(),
                                                twice.anchor().clamp(start, end)
                                            );
                                            assert_eq!(
                                                clipped.head(),
                                                twice.head().clamp(start, end)
                                            );
                                            assert_eq!(
                                                map_and_clip_selection(
                                                    &final_catalog,
                                                    selection,
                                                    &view,
                                                    &composed,
                                                    source.len(),
                                                    aa,
                                                    ha
                                                )
                                                .unwrap(),
                                                clipped
                                            );
                                            assert_eq!(
                                                clipped.validate(&final_catalog, &view),
                                                Ok(())
                                            );
                                            assert_eq!(
                                                view.check_target(id, clipped.range()),
                                                Ok(())
                                            );
                                            if clipped.anchor() != clipped.head() {
                                                assert_eq!(clipped.direction(), twice.direction());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn generated_rejections() {
        let id = BufferId::new(1);
        let source = catalog("abc");
        let full = BufferView::full(&source, id).unwrap();
        let points = source.get(id).unwrap().char_boundaries();
        let selection = Selection::new(&source, &full, id, points[0], points[1]).unwrap();
        let mut invalid_checked = false;
        for (middle, first) in edits("abc") {
            for (end, second) in edits(&middle) {
                let map = first.compose(&second).unwrap();
                let destination = catalog(&end);
                for &start in &points {
                    for &finish in points.iter().filter(|&&finish| finish >= start) {
                        let narrowed = BufferView::narrowed(
                            &source,
                            id,
                            ByteSpan::new(start, finish).unwrap(),
                        )
                        .unwrap();
                        for sa in [Affinity::Left, Affinity::Right] {
                            for ea in [Affinity::Left, Affinity::Right] {
                                let s = map.map_offset(start.as_usize(), sa).unwrap();
                                let e = map.map_offset(finish.as_usize(), ea).unwrap();
                                let result = map_view(&destination, &narrowed, &map, 3, sa, ea);
                                if s > e {
                                    assert_eq!(
                                        result,
                                        Err(ViewMappingError::InvertedView { start: s, end: e })
                                    );
                                } else {
                                    assert_eq!(
                                        result.unwrap().visible(),
                                        ByteSpan::validate_in(destination.get(id).unwrap(), s, e)
                                            .unwrap()
                                    );
                                }
                            }
                        }
                    }
                }
                assert!(matches!(
                    map_selection(
                        &destination,
                        selection,
                        &map,
                        4,
                        Affinity::Left,
                        Affinity::Right
                    ),
                    Err(ViewMappingError::SourceLengthMismatch { .. })
                ));
                assert!(matches!(
                    map_selection(
                        &catalog(&format!("{end}x")),
                        selection,
                        &map,
                        3,
                        Affinity::Left,
                        Affinity::Right
                    ),
                    Err(ViewMappingError::TargetLengthMismatch { .. })
                ));
                assert_eq!(
                    map_selection(
                        &BufferCatalog::new(),
                        selection,
                        &map,
                        3,
                        Affinity::Left,
                        Affinity::Right
                    ),
                    Err(ViewMappingError::UnknownBuffer(id))
                );
                let mut closed = catalog(&end);
                closed.remove(id).unwrap();
                assert_eq!(
                    map_selection(&closed, selection, &map, 3, Affinity::Left, Affinity::Right),
                    Err(ViewMappingError::BufferClosed(id))
                );
                let other = BufferId::new(2);
                let mut both = catalog(&end);
                both.register(other, Text::from_str(&end)).unwrap();
                let view = BufferView::full(&both, other).unwrap();
                assert_eq!(
                    map_and_clip_selection(
                        &both,
                        selection,
                        &view,
                        &map,
                        3,
                        Affinity::Left,
                        Affinity::Right
                    ),
                    Err(ViewMappingError::BufferMismatch {
                        expected: other,
                        actual: id
                    })
                );
                if end.len() >= 3 {
                    let offset = map.map_offset(1, Affinity::Right).unwrap();
                    if (offset == 1 || offset == 2)
                        && map.map_offset(0, Affinity::Left).unwrap() == 0
                    {
                        let unicode = catalog(&format!("界{}", "a".repeat(end.len() - 3)));
                        let source_view = BufferView::narrowed(
                            &source,
                            id,
                            ByteSpan::new(points[0], points[1]).unwrap(),
                        )
                        .unwrap();
                        assert_eq!(
                            map_view(
                                &unicode,
                                &source_view,
                                &map,
                                3,
                                Affinity::Left,
                                Affinity::Right
                            ),
                            Err(ViewMappingError::InvalidOffset(
                                Utf8Error::NotCharBoundary { offset }
                            ))
                        );
                        let zero = unicode.get(id).unwrap().validate_offset(0).unwrap();
                        let empty =
                            BufferView::narrowed(&unicode, id, ByteSpan::new(zero, zero).unwrap())
                                .unwrap();
                        assert_eq!(
                            map_and_clip_selection(
                                &unicode,
                                selection,
                                &empty,
                                &map,
                                3,
                                Affinity::Left,
                                Affinity::Right
                            ),
                            Err(ViewMappingError::InvalidOffset(
                                Utf8Error::NotCharBoundary { offset }
                            ))
                        );
                        invalid_checked = true;
                    }
                }
            }
        }
        assert!(invalid_checked);
    }

    #[test]
    fn strict_errors_and_view_endpoints() {
        let id = BufferId::new(1);
        let before = catalog("abc");
        let view = BufferView::full(&before, id).unwrap();
        let selection = Selection::new(
            &before,
            &view,
            id,
            before.get(id).unwrap().validate_offset(1).unwrap(),
            before.get(id).unwrap().validate_offset(2).unwrap(),
        )
        .unwrap();
        let unicode = catalog("界");
        let identity = PositionMap::identity(3);
        assert_eq!(
            map_selection(
                &unicode,
                selection,
                &identity,
                3,
                Affinity::Left,
                Affinity::Right
            ),
            Err(ViewMappingError::InvalidOffset(
                Utf8Error::NotCharBoundary { offset: 1 }
            ))
        );
        assert_eq!(
            map_selection(
                &before,
                selection,
                &identity,
                2,
                Affinity::Left,
                Affinity::Right
            ),
            Err(ViewMappingError::SourceLengthMismatch {
                expected: 3,
                actual: 2
            })
        );
        assert_eq!(
            map_selection(
                &catalog("a"),
                selection,
                &identity,
                3,
                Affinity::Left,
                Affinity::Right
            ),
            Err(ViewMappingError::TargetLengthMismatch {
                expected: 3,
                actual: 1
            })
        );
        assert_eq!(
            map_selection(
                &BufferCatalog::new(),
                selection,
                &identity,
                3,
                Affinity::Left,
                Affinity::Right
            ),
            Err(ViewMappingError::UnknownBuffer(id))
        );
        let mut closed = catalog("abc");
        closed.remove(id).unwrap();
        assert_eq!(
            map_selection(
                &closed,
                selection,
                &identity,
                3,
                Affinity::Left,
                Affinity::Right
            ),
            Err(ViewMappingError::BufferClosed(id))
        );
        let other = BufferId::new(2);
        let mut both = catalog("abc");
        both.register(other, Text::from_str("abc")).unwrap();
        let other_view = BufferView::full(&both, other).unwrap();
        assert_eq!(
            clip_selection(&both, selection, &other_view),
            Err(ViewMappingError::BufferMismatch {
                expected: other,
                actual: id
            })
        );
        let replacement = PositionMap::from_replacement(
            3,
            ByteReplacement {
                start: 0,
                end: 3,
                inserted_len: 1,
            },
        )
        .unwrap();
        let after = catalog("x");
        assert_eq!(
            map_view(
                &after,
                &view,
                &replacement,
                3,
                Affinity::Right,
                Affinity::Left
            ),
            Err(ViewMappingError::InvertedView { start: 1, end: 0 })
        );
        let zero = after.get(id).unwrap().validate_offset(0).unwrap();
        assert_eq!(
            map_view(
                &after,
                &view,
                &replacement,
                3,
                Affinity::Left,
                Affinity::Left
            )
            .unwrap()
            .visible(),
            ByteSpan::new(zero, zero).unwrap()
        );
        assert_eq!(
            map_view(
                &after,
                &view,
                &replacement,
                3,
                Affinity::Left,
                Affinity::Right
            )
            .unwrap()
            .visible(),
            ByteSpan::full_of(after.get(id).unwrap())
        );
    }
}
