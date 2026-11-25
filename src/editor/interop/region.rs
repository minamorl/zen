//! Oriented external regions over live buffer text.
//!
//! Coordinates are absolute in the buffer, even for a narrowed view. Exclusive
//! endpoints denote byte boundaries. Inclusive endpoints denote the scalar at
//! the upper coordinate, so decoding advances that boundary by one scalar,
//! saturating at EOF. The upper coordinate is the head in a forward region and
//! the anchor in a backward region; equal coordinates use the direction hint.
//!
//! # Laws
//!
//! * Exclusive conversion preserves each validated boundary independently;
//!   the position converters are bijections, so anchor, head, buffer, direction
//!   and range survive a roundtrip, including empty and backward selections.
//! * Inclusive decoding advances only the upper boundary by one scalar, or
//!   leaves EOF fixed. Encoding uses the previous scalar boundary as a canonical
//!   upper coordinate. Decoding it restores the selection when its orientation
//!   remains distinguishable. A direction hint disambiguates equal endpoints.
//!   Empty selections before EOF cannot be represented and are rejected.
//!   An inclusive coordinate at EOF and the preceding scalar can decode to the
//!   same end, so re-encoding chooses the preceding scalar canonically.
//! * Both conversions validate buffer identity, visible range and live text.
//!   Invalid coordinates retain the typed error from the position converter.
//!   Buffer mismatch maps to `InvalidCoordinate`, outside-view to `OutOfRange`,
//!   and closed and unknown buffers to their corresponding codes.

use super::agent_position::{self, AgentPosition};
use super::coordinates::{CoordKind, EmacsCharPos, LineColByte};
use super::emacs_position;
use super::error::{BoundaryError, ErrorCode};
use super::neovim_position;
use crate::editor::edit::ids::BufferId;
use crate::editor::text::span::SpanError;
use crate::editor::text::utf8::{ByteOffset, Text};
use crate::editor::workspace::catalog::BufferCatalog;
use crate::editor::workspace::selections::{Direction, Selection, SelectionError};
use crate::editor::workspace::view::BufferView;

/// Interpretation of the upper endpoint of an external region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointMode {
    /// Both endpoints are exact boundaries.
    Exclusive,
    /// The upper endpoint includes the scalar at that boundary, if any.
    Inclusive,
}

/// An external coordinate in one of the supported protocols.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionCoord {
    /// One-based Unicode scalar position.
    Emacs(EmacsCharPos),
    /// Zero-based line and UTF-8 byte column.
    Neovim(LineColByte),
    /// Agent byte or UTF-16 position.
    Agent(AgentPosition),
}

/// An oriented pair of absolute buffer coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrientedRegion {
    /// Fixed endpoint.
    pub anchor: RegionCoord,
    /// Active endpoint.
    pub head: RegionCoord,
    /// Direction used when inclusive endpoint coordinates coincide.
    pub direction: Direction,
    /// Endpoint interpretation.
    pub mode: EndpointMode,
}

fn selection_error(error: SelectionError) -> BoundaryError {
    let code = match &error {
        SelectionError::BufferMismatch { .. } => ErrorCode::InvalidCoordinate,
        SelectionError::SpanOutsideVisible { .. } => ErrorCode::OutOfRange,
        SelectionError::UnknownBuffer(_) => ErrorCode::UnknownBuffer,
        SelectionError::BufferClosed(_) => ErrorCode::BufferClosed,
        SelectionError::InvalidSpan(SpanError::NotCharBoundary { .. }) => {
            ErrorCode::InvalidBoundary
        }
        SelectionError::InvalidSpan(SpanError::OutOfBounds { .. }) => ErrorCode::OutOfRange,
        SelectionError::InvalidSpan(_) => ErrorCode::InvalidSpan,
    };
    BoundaryError::new(code, error.to_string())
}

fn live_text(catalog: &BufferCatalog, buffer: BufferId) -> Result<&Text, BoundaryError> {
    catalog.get(buffer).ok_or_else(|| {
        let code = if catalog.is_tombstoned(buffer) {
            ErrorCode::BufferClosed
        } else {
            ErrorCode::UnknownBuffer
        };
        BoundaryError::new(code, format!("buffer {buffer} is not live"))
    })
}

fn to_byte(text: &Text, coord: RegionCoord) -> Result<ByteOffset, BoundaryError> {
    match coord {
        RegionCoord::Emacs(pos) => emacs_position::emacs_to_byte(text, pos),
        RegionCoord::Neovim(pos) => neovim_position::line_col_to_byte(text, pos),
        RegionCoord::Agent(pos) => agent_position::position_to_byte(text, pos),
    }
}

fn from_byte(
    text: &Text,
    offset: ByteOffset,
    kind: CoordKind,
) -> Result<RegionCoord, BoundaryError> {
    match kind {
        CoordKind::EmacsChar => emacs_position::byte_to_emacs(text, offset).map(RegionCoord::Emacs),
        CoordKind::LineColByte => {
            neovim_position::byte_to_line_col(text, offset).map(RegionCoord::Neovim)
        }
        CoordKind::Byte | CoordKind::LineColUtf16 => {
            agent_position::byte_to_agent_position(text, offset, kind).map(RegionCoord::Agent)
        }
        _ => Err(BoundaryError::new(
            ErrorCode::UnsupportedOperation,
            "unsupported region coordinate kind",
        )),
    }
}

impl OrientedRegion {
    /// Decodes the region and validates it against the requested buffer and view.
    pub fn to_selection(
        self,
        catalog: &BufferCatalog,
        view: &BufferView,
        buffer: BufferId,
    ) -> Result<Selection, BoundaryError> {
        if buffer != view.buffer() {
            return Err(BoundaryError::new(
                ErrorCode::InvalidCoordinate,
                "buffer mismatch",
            ));
        }
        let text = live_text(catalog, buffer)?;
        let mut anchor = to_byte(text, self.anchor)?;
        let mut head = to_byte(text, self.head)?;
        if self.mode == EndpointMode::Inclusive {
            let upper =
                if anchor > head || (anchor == head && self.direction == Direction::Backward) {
                    &mut anchor
                } else {
                    &mut head
                };
            let raw = upper.as_usize();
            let next = text.as_str()[raw..]
                .chars()
                .next()
                .map_or(raw, |ch| raw + ch.len_utf8());
            *upper = text
                .validate_offset(next)
                .expect("scalar end is a boundary");
        }
        Selection::new(catalog, view, buffer, anchor, head).map_err(selection_error)
    }

    /// Encodes a validated selection in the requested coordinate kind.
    ///
    /// Inclusive selections that cannot retain their orientation are rejected
    /// with `UnsupportedOperation`; EOF is the only encodable empty selection.
    pub fn from_selection(
        catalog: &BufferCatalog,
        view: &BufferView,
        selection: Selection,
        kind: CoordKind,
        mode: EndpointMode,
    ) -> Result<Self, BoundaryError> {
        selection.validate(catalog, view).map_err(selection_error)?;
        let text = live_text(catalog, selection.buffer())?;
        let mut anchor = selection.anchor();
        let mut head = selection.head();
        if mode == EndpointMode::Inclusive {
            if anchor == head {
                if anchor.as_usize() != text.len_bytes() {
                    return Err(BoundaryError::new(
                        ErrorCode::UnsupportedOperation,
                        "inclusive empty selection before EOF is ambiguous",
                    ));
                }
            } else {
                let backward = anchor > head;
                let upper = if backward { &mut anchor } else { &mut head };
                let raw = upper.as_usize();
                let previous = text.as_str()[..raw]
                    .char_indices()
                    .next_back()
                    .expect("nonempty range has a preceding scalar")
                    .0;
                *upper = text
                    .validate_offset(previous)
                    .expect("scalar start is a boundary");
            }
        }
        Ok(Self {
            anchor: from_byte(text, anchor, kind)?,
            head: from_byte(text, head, kind)?,
            direction: selection.direction(),
            mode,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::interop::coordinates::{BytePos, LineColUtf16};
    use crate::editor::text::span::ByteSpan;

    const CORPUS: &[&str] = &["", "a", "界", "e\u{301}", "😀", "a界e\u{301}😀\n終\n"];
    const KINDS: &[CoordKind] = &[
        CoordKind::EmacsChar,
        CoordKind::LineColByte,
        CoordKind::Byte,
        CoordKind::LineColUtf16,
    ];

    #[test]
    fn exhaustive_selection_and_endpoint_laws() {
        for &source in CORPUS {
            let text = Text::from_str(source);
            let offsets = text.char_boundaries();
            let id = BufferId::new(1);
            let mut catalog = BufferCatalog::new();
            catalog.register(id, text.clone()).unwrap();
            for &start in &offsets {
                for &end in offsets.iter().filter(|&&end| end >= start) {
                    let view =
                        BufferView::narrowed(&catalog, id, ByteSpan::new(start, end).unwrap())
                            .unwrap();
                    for &anchor in offsets.iter().filter(|&&a| a >= start && a <= end) {
                        for &head in offsets.iter().filter(|&&h| h >= start && h <= end) {
                            let selection =
                                Selection::new(&catalog, &view, id, anchor, head).unwrap();
                            for &kind in KINDS {
                                let region = OrientedRegion::from_selection(
                                    &catalog,
                                    &view,
                                    selection,
                                    kind,
                                    EndpointMode::Exclusive,
                                )
                                .unwrap();
                                for coord in [region.anchor, region.head] {
                                    assert!(matches!(
                                        (kind, coord),
                                        (CoordKind::EmacsChar, RegionCoord::Emacs(_))
                                            | (CoordKind::LineColByte, RegionCoord::Neovim(_))
                                            | (
                                                CoordKind::Byte,
                                                RegionCoord::Agent(AgentPosition::Byte(_))
                                            )
                                            | (
                                                CoordKind::LineColUtf16,
                                                RegionCoord::Agent(AgentPosition::LineColUtf16(_))
                                            )
                                    ));
                                }
                                assert_eq!(
                                    region.to_selection(&catalog, &view, id).unwrap(),
                                    selection
                                );
                                assert_eq!(to_byte(&text, region.anchor).unwrap(), anchor);
                                assert_eq!(to_byte(&text, region.head).unwrap(), head);
                                let encoded = OrientedRegion::from_selection(
                                    &catalog,
                                    &view,
                                    selection,
                                    kind,
                                    EndpointMode::Inclusive,
                                );
                                let representable =
                                    anchor != head || head.as_usize() == text.len_bytes();
                                assert_eq!(encoded.is_ok(), representable);
                                if let Ok(region) = encoded {
                                    assert_eq!(region.mode, EndpointMode::Inclusive);
                                    assert_eq!(region.direction, selection.direction());
                                    let upper = anchor.max(head).as_usize();
                                    let expected = if anchor == head {
                                        upper
                                    } else {
                                        source[..upper].char_indices().next_back().unwrap().0
                                    };
                                    assert_eq!(
                                        to_byte(&text, region.anchor).unwrap().as_usize(),
                                        if anchor > head {
                                            expected
                                        } else {
                                            anchor.as_usize()
                                        }
                                    );
                                    assert_eq!(
                                        to_byte(&text, region.head).unwrap().as_usize(),
                                        if anchor < head {
                                            expected
                                        } else {
                                            head.as_usize()
                                        }
                                    );
                                    assert_eq!(
                                        region.to_selection(&catalog, &view, id).unwrap(),
                                        selection
                                    );
                                }
                            }
                        }
                    }
                }
            }
            let view = BufferView::full(&catalog, id).unwrap();
            for &anchor in &offsets {
                for &head in &offsets {
                    for &kind in KINDS {
                        let region = OrientedRegion {
                            anchor: from_byte(&text, anchor, kind).unwrap(),
                            head: from_byte(&text, head, kind).unwrap(),
                            direction: Direction::Forward,
                            mode: EndpointMode::Inclusive,
                        };
                        let selection = region.to_selection(&catalog, &view, id).unwrap();
                        let upper = anchor.max(head).as_usize();
                        let expected = source[upper..]
                            .chars()
                            .next()
                            .map_or(upper, |c| upper + c.len_utf8());
                        assert_eq!(selection.range().end().as_usize(), expected);
                        assert_eq!(selection.range().start(), anchor.min(head));
                        assert_eq!(
                            selection.anchor().as_usize(),
                            if anchor > head {
                                expected
                            } else {
                                anchor.as_usize()
                            }
                        );
                        assert_eq!(
                            selection.head().as_usize(),
                            if anchor > head {
                                head.as_usize()
                            } else {
                                expected
                            }
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn every_outside_selection_is_rejected_in_both_directions() {
        for &source in CORPUS {
            let text = Text::from_str(source);
            let offsets = text.char_boundaries();
            let id = BufferId::new(1);
            let mut catalog = BufferCatalog::new();
            catalog.register(id, text.clone()).unwrap();
            let full = BufferView::full(&catalog, id).unwrap();
            for &start in &offsets {
                for &end in offsets.iter().filter(|&&end| end >= start) {
                    let view =
                        BufferView::narrowed(&catalog, id, ByteSpan::new(start, end).unwrap())
                            .unwrap();
                    for &anchor in &offsets {
                        for &head in &offsets {
                            if anchor >= start && anchor <= end && head >= start && head <= end {
                                continue;
                            }
                            let selection =
                                Selection::new(&catalog, &full, id, anchor, head).unwrap();
                            for &kind in KINDS {
                                for mode in [EndpointMode::Exclusive, EndpointMode::Inclusive] {
                                    let region = OrientedRegion {
                                        anchor: from_byte(&text, anchor, kind).unwrap(),
                                        head: from_byte(&text, head, kind).unwrap(),
                                        direction: selection.direction(),
                                        mode,
                                    };
                                    assert_eq!(
                                        region.to_selection(&catalog, &view, id).unwrap_err().code(),
                                        ErrorCode::OutOfRange,
                                        "decoding {source:?} {start:?}..{end:?} {anchor:?} {head:?} {kind:?} {mode:?}"
                                    );
                                    assert_eq!(
                                        OrientedRegion::from_selection(
                                            &catalog, &view, selection, kind, mode
                                        )
                                        .unwrap_err()
                                        .code(),
                                        ErrorCode::OutOfRange,
                                        "encoding {source:?} {start:?}..{end:?} {anchor:?} {head:?} {kind:?} {mode:?}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn rejects_invalid_and_stale_inputs() {
        let id = BufferId::new(1);
        let text = Text::from_str("a😀z");
        let mut catalog = BufferCatalog::new();
        catalog.register(id, text.clone()).unwrap();
        let full = BufferView::full(&catalog, id).unwrap();
        let narrow =
            BufferView::narrowed(&catalog, id, ByteSpan::validate_in(&text, 1, 5).unwrap())
                .unwrap();
        let byte = |n| RegionCoord::Agent(AgentPosition::Byte(BytePos::new(n)));
        let region = |a, h, mode| OrientedRegion {
            anchor: byte(a),
            head: byte(h),
            direction: Direction::Forward,
            mode,
        };
        assert_eq!(
            region(1, 5, EndpointMode::Exclusive)
                .to_selection(&catalog, &full, BufferId::new(2))
                .unwrap_err()
                .code(),
            ErrorCode::InvalidCoordinate
        );
        assert_eq!(
            region(0, 1, EndpointMode::Exclusive)
                .to_selection(&catalog, &narrow, id)
                .unwrap_err()
                .code(),
            ErrorCode::OutOfRange
        );
        assert_eq!(
            region(1, 5, EndpointMode::Inclusive)
                .to_selection(&catalog, &narrow, id)
                .unwrap_err()
                .code(),
            ErrorCode::OutOfRange
        );
        assert_eq!(
            region(2, 2, EndpointMode::Exclusive)
                .to_selection(&catalog, &full, id)
                .unwrap_err()
                .code(),
            ErrorCode::InvalidBoundary
        );
        assert_eq!(
            region(99, 99, EndpointMode::Exclusive)
                .to_selection(&catalog, &full, id)
                .unwrap_err()
                .code(),
            ErrorCode::OutOfRange
        );
        let invalid = OrientedRegion {
            anchor: RegionCoord::Agent(AgentPosition::LineColUtf16(LineColUtf16::new(0, 2))),
            head: byte(0),
            direction: Direction::Forward,
            mode: EndpointMode::Exclusive,
        };
        assert_eq!(
            invalid
                .to_selection(&catalog, &full, id)
                .unwrap_err()
                .code(),
            ErrorCode::InvalidUtf16Boundary
        );
        let single = Selection::new(
            &catalog,
            &full,
            id,
            text.validate_offset(5).unwrap(),
            text.validate_offset(1).unwrap(),
        )
        .unwrap();
        let encode = |cat: &BufferCatalog, view: &BufferView, selection| {
            OrientedRegion::from_selection(
                cat,
                view,
                selection,
                CoordKind::Byte,
                EndpointMode::Exclusive,
            )
            .unwrap_err()
            .code()
        };
        let other_id = BufferId::new(2);
        catalog.register(other_id, text.clone()).unwrap();
        let other_view = BufferView::full(&catalog, other_id).unwrap();
        assert_eq!(
            encode(&catalog, &other_view, single),
            ErrorCode::InvalidCoordinate
        );
        let outside = Selection::new(
            &catalog,
            &full,
            id,
            text.validate_offset(0).unwrap(),
            text.validate_offset(1).unwrap(),
        )
        .unwrap();
        assert_eq!(encode(&catalog, &narrow, outside), ErrorCode::OutOfRange);
        assert_eq!(
            encode(&BufferCatalog::new(), &full, single),
            ErrorCode::UnknownBuffer
        );
        catalog.update(id, Text::from_str("😀abc")).unwrap();
        assert_eq!(encode(&catalog, &full, single), ErrorCode::InvalidBoundary);
        catalog.update(id, text.clone()).unwrap();
        assert_eq!(
            OrientedRegion::from_selection(
                &catalog,
                &full,
                single,
                CoordKind::Byte,
                EndpointMode::Inclusive
            )
            .unwrap()
            .to_selection(&catalog, &full, id)
            .unwrap(),
            single
        );
        let emacs = OrientedRegion {
            anchor: RegionCoord::Emacs(EmacsCharPos::new(99).unwrap()),
            head: byte(0),
            direction: Direction::Forward,
            mode: EndpointMode::Exclusive,
        };
        assert_eq!(
            emacs.to_selection(&catalog, &full, id).unwrap_err().code(),
            ErrorCode::OutOfRange
        );
        let neovim = OrientedRegion {
            anchor: RegionCoord::Neovim(LineColByte::new(0, 2)),
            head: byte(0),
            direction: Direction::Forward,
            mode: EndpointMode::Exclusive,
        };
        assert_eq!(
            neovim.to_selection(&catalog, &full, id).unwrap_err().code(),
            ErrorCode::InvalidBoundary
        );
        catalog.remove(id).unwrap();
        assert_eq!(encode(&catalog, &full, single), ErrorCode::BufferClosed);
        assert_eq!(
            region(0, 0, EndpointMode::Exclusive)
                .to_selection(&catalog, &full, id)
                .unwrap_err()
                .code(),
            ErrorCode::BufferClosed
        );
        assert_eq!(
            region(0, 0, EndpointMode::Exclusive)
                .to_selection(&BufferCatalog::new(), &full, id)
                .unwrap_err()
                .code(),
            ErrorCode::UnknownBuffer
        );
    }
}
