//! Visible ranges over workspace buffers, without changing their text.
//!
//! For a live buffer with valid spans, closed intersection is associative:
//! `max` of starts and `min` of ends are associative. Thus successive
//! [`BufferView::narrow_intersect`] calls agree with one call using their
//! intersection whenever the successive intersections exist. Touching ranges
//! intersect at an empty span. [`BufferView::narrow`] instead rejects a span
//! that is not contained in the current view.
//!
//! [`BufferView::widen`] reconstructs the full span from the current live
//! text, independently of earlier narrowings. Constructors and text access
//! reject unknown or closed buffers and invalid spans; target checks reject a
//! different buffer ID or a span outside the visible range. Catalog lookup and
//! tombstone checks distinguish unknown from closed IDs; validating both span
//! endpoints against current text rejects invalid boundaries. Comparing the
//! target ID and endpoints enforces the remaining rejection rules.

use std::fmt;

use crate::editor::edit::ids::BufferId;
use crate::editor::text::span::{ByteSpan, SpanError};
use crate::editor::text::utf8::{ByteOffset, Text};
use crate::editor::workspace::catalog::BufferCatalog;

/// A visible range of a buffer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BufferView {
    buffer: BufferId,
    visible: ByteSpan,
}

/// An error addressing a buffer view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewError {
    /// The identifier has never been registered.
    UnknownBuffer(BufferId),
    /// The identifier has been removed.
    BufferClosed(BufferId),
    /// The target belongs to a different buffer.
    BufferMismatch {
        expected: BufferId,
        actual: BufferId,
    },
    /// The requested span is not contained in the visible range, or is disjoint.
    SpanOutsideVisible {
        requested: ByteSpan,
        visible: ByteSpan,
    },
    /// The span is invalid in the current text.
    InvalidSpan(SpanError),
}

impl fmt::Display for ViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownBuffer(id) => write!(f, "unknown buffer {id}"),
            Self::BufferClosed(id) => write!(f, "buffer {id} is closed"),
            Self::BufferMismatch { expected, actual } => {
                write!(f, "buffer mismatch: expected {expected}, got {actual}")
            }
            Self::SpanOutsideVisible { requested, visible } => {
                write!(f, "span {requested:?} is outside visible range {visible:?}")
            }
            Self::InvalidSpan(error) => write!(f, "invalid span: {error}"),
        }
    }
}

impl std::error::Error for ViewError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidSpan(error) => Some(error),
            _ => None,
        }
    }
}

fn live_text(catalog: &BufferCatalog, buffer: BufferId) -> Result<&Text, ViewError> {
    catalog.get(buffer).ok_or_else(|| {
        if catalog.is_tombstoned(buffer) {
            ViewError::BufferClosed(buffer)
        } else {
            ViewError::UnknownBuffer(buffer)
        }
    })
}

fn validate(text: &Text, span: ByteSpan) -> Result<ByteSpan, ViewError> {
    ByteSpan::validate_in(text, span.start().as_usize(), span.end().as_usize())
        .map_err(ViewError::InvalidSpan)
}

impl BufferView {
    /// Creates a view of the entire live buffer.
    pub fn full(catalog: &BufferCatalog, buffer: BufferId) -> Result<Self, ViewError> {
        Ok(Self {
            buffer,
            visible: ByteSpan::full_of(live_text(catalog, buffer)?),
        })
    }

    /// Creates a view of a valid span in a live buffer.
    pub fn narrowed(
        catalog: &BufferCatalog,
        buffer: BufferId,
        span: ByteSpan,
    ) -> Result<Self, ViewError> {
        Ok(Self {
            buffer,
            visible: validate(live_text(catalog, buffer)?, span)?,
        })
    }

    /// Narrows to a valid span contained in the current visible range.
    /// The current range itself need not still be valid in the live text.
    pub fn narrow(&self, catalog: &BufferCatalog, span: ByteSpan) -> Result<Self, ViewError> {
        let view = Self::narrowed(catalog, self.buffer, span)?;
        self.check_target(self.buffer, span)?;
        Ok(view)
    }

    /// Intersects the current range with a valid span, including endpoint contact.
    pub fn narrow_intersect(
        &self,
        catalog: &BufferCatalog,
        span: ByteSpan,
    ) -> Result<Self, ViewError> {
        let text = live_text(catalog, self.buffer)?;
        let span = validate(text, span)?;
        let visible = self
            .visible
            .intersect_closed(span)
            .ok_or(ViewError::SpanOutsideVisible {
                requested: span,
                visible: self.visible,
            })?;
        Ok(Self {
            buffer: self.buffer,
            visible: validate(text, visible)?,
        })
    }

    /// Restores the full range of the current live text.
    pub fn widen(&self, catalog: &BufferCatalog) -> Result<Self, ViewError> {
        Self::full(catalog, self.buffer)
    }

    /// Returns the buffer identifier.
    pub fn buffer(&self) -> BufferId {
        self.buffer
    }

    /// Returns the visible range.
    pub fn visible(&self) -> ByteSpan {
        self.visible
    }

    /// Borrows the visible text after revalidating it against the live buffer.
    pub fn visible_text(self, catalog: &BufferCatalog) -> Result<&str, ViewError> {
        self.visible
            .slice_in(live_text(catalog, self.buffer)?)
            .map_err(ViewError::InvalidSpan)
    }

    /// Tests inclusive endpoint containment.
    pub fn contains_offset(&self, offset: ByteOffset) -> bool {
        self.visible.contains_offset(offset)
    }

    /// Tests whether the visible range contains both span endpoints.
    pub fn contains_span(&self, span: ByteSpan) -> bool {
        self.visible.contains_span(span)
    }

    /// Checks the buffer ID and visible range of a target.
    pub fn check_target(&self, buffer: BufferId, span: ByteSpan) -> Result<(), ViewError> {
        if buffer != self.buffer {
            return Err(ViewError::BufferMismatch {
                expected: self.buffer,
                actual: buffer,
            });
        }
        if !self.contains_span(span) {
            return Err(ViewError::SpanOutsideVisible {
                requested: span,
                visible: self.visible,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts, Rng};

    #[test]
    fn narrowing_intersection_and_widen_laws() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            let mut rng = Rng::new(91);
            for source in corpus {
                let text = Text::from_str(source);
                let boundaries = text.char_boundaries();
                let id = BufferId::new(1);
                let mut catalog = BufferCatalog::new();
                catalog.register(id, text.clone()).unwrap();
                let full = BufferView::full(&catalog, id).unwrap();
                for _ in 0..100 {
                    let mut points = [0; 4];
                    for point in &mut points {
                        *point = rng.gen_range(0..boundaries.len());
                    }
                    let span = |a: usize, b: usize| {
                        ByteSpan::new(boundaries[a.min(b)], boundaries[a.max(b)]).unwrap()
                    };
                    let a = span(points[0], points[1]);
                    let b = span(points[2], points[3]);
                    let first = full.narrow_intersect(&catalog, a).unwrap();
                    assert_eq!(first.visible(), a);
                    assert_eq!(first.buffer(), id);
                    assert_eq!(
                        first.visible_text(&catalog).unwrap(),
                        &source[a.start().as_usize()..a.end().as_usize()]
                    );
                    assert_eq!(first.widen(&catalog).unwrap(), full);
                    assert!(first.contains_offset(a.start()) && first.contains_offset(a.end()));
                    match a.intersect_closed(b) {
                        Some(intersection) => assert_eq!(
                            first.narrow_intersect(&catalog, b).unwrap(),
                            full.narrow_intersect(&catalog, intersection).unwrap()
                        ),
                        None => assert_eq!(
                            first.narrow_intersect(&catalog, b),
                            Err(ViewError::SpanOutsideVisible {
                                requested: b,
                                visible: a
                            })
                        ),
                    }
                    assert_eq!(first.contains_span(b), a.contains_span(b));
                    if a.contains_span(b) {
                        assert_eq!(first.narrow(&catalog, b).unwrap().visible(), b);
                        assert_eq!(first.check_target(id, b), Ok(()));
                    } else {
                        assert_eq!(
                            first.narrow(&catalog, b),
                            Err(ViewError::SpanOutsideVisible {
                                requested: b,
                                visible: a
                            })
                        );
                    }
                }
            }
        });
    }

    #[test]
    fn rejects_missing_mismatched_and_stale_spans() {
        let id = BufferId::new(1);
        let other = BufferId::new(2);
        let mut catalog = BufferCatalog::new();
        assert_eq!(
            BufferView::full(&catalog, id),
            Err(ViewError::UnknownBuffer(id))
        );
        let ascii = Text::from_str("abcd");
        let span = ByteSpan::validate_in(&ascii, 1, 4).unwrap();
        catalog.register(id, ascii).unwrap();
        let view = BufferView::narrowed(&catalog, id, span).unwrap();
        assert_eq!(
            view.check_target(other, span),
            Err(ViewError::BufferMismatch {
                expected: id,
                actual: other
            })
        );
        catalog.update(id, Text::from_str("abc")).unwrap();
        let current = ByteSpan::validate_in(catalog.get(id).unwrap(), 2, 3).unwrap();
        assert_eq!(view.narrow(&catalog, current).unwrap().visible(), current);
        assert_eq!(
            view.narrow_intersect(&catalog, current).unwrap().visible(),
            current
        );
        catalog.update(id, Text::from_str("界")).unwrap();
        assert_eq!(
            view.visible_text(&catalog),
            Err(ViewError::InvalidSpan(SpanError::NotCharBoundary {
                offset: 1
            }))
        );
        assert_eq!(
            BufferView::narrowed(&catalog, id, span),
            Err(ViewError::InvalidSpan(SpanError::NotCharBoundary {
                offset: 1
            }))
        );
        assert_eq!(
            view.widen(&catalog).unwrap().visible(),
            ByteSpan::full_of(catalog.get(id).unwrap())
        );
        catalog.remove(id).unwrap();
        assert_eq!(
            view.visible_text(&catalog),
            Err(ViewError::BufferClosed(id))
        );
        assert_eq!(view.widen(&catalog), Err(ViewError::BufferClosed(id)));
        assert_eq!(
            BufferView::narrowed(&catalog, id, span),
            Err(ViewError::BufferClosed(id))
        );
    }
}
