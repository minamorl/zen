//! Rectangular text regions mapped to line-contained UTF-8 byte spans.
//!
//! Corners normalize independently. Exclusive endpoints form a half-open
//! interval; inclusive endpoints include the rightmost cell. Intersected tabs
//! and wide scalars round outward. An empty exclusive rectangle rounds down
//! to an empty span. Columns beyond line ends clamp without inserting padding.
//! CRLF is excluded from content; a standalone CR remains content.
//! Construct byte-offset corners with [`position_at_offset`] so that CRLF
//! line endings do not add a visual column to the rectangle.
//!
//! The default counts each non-tab scalar as one cell, as `columns` does.
//! Callers can supply scalar widths or multi-scalar display units for their
//! own display convention. Display unit segmentation is supplied by callers;
//! this module does not implement Unicode grapheme segmentation.

use super::columns::{self, ColumnError, VisualPosition};
use super::lines::{LineError, LineIndex};
use super::span::ByteSpan;
use super::utf8::{ByteOffset, Text};
use std::fmt;

/// Interpretation of the normalized rightmost column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Endpoint {
    /// Exclude the rightmost column.
    Exclusive,
    /// Include the rightmost cell.
    Inclusive,
}

/// Invalid rectangle parameters or display widths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RectangleError {
    /// Tab stops must have positive width.
    ZeroTabWidth,
    /// Non-tab scalars must have positive width.
    ZeroCharacterWidth,
    /// A display unit has an invalid byte length or crosses a tab.
    InvalidDisplayUnit,
    /// A column exceeded `usize`.
    ColumnOverflow,
    /// A corner references a nonexistent line.
    Line(LineError),
}

impl fmt::Display for RectangleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroTabWidth => write!(f, "tab width must be nonzero"),
            Self::ZeroCharacterWidth => write!(f, "character width must be nonzero"),
            Self::InvalidDisplayUnit => write!(f, "invalid display unit"),
            Self::ColumnOverflow => write!(f, "column overflow"),
            Self::Line(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for RectangleError {}

fn content_end(text: &Text, span: ByteSpan) -> usize {
    let start = span.start().as_usize();
    let end = span.end().as_usize();
    if text.as_bytes().get(end) == Some(&b'\n') && end > start && text.as_bytes()[end - 1] == b'\r'
    {
        end - 1
    } else {
        end
    }
}

/// Converts a byte offset into a CRLF-aware rectangle corner.
///
/// Offsets at the CR or LF of a CRLF line ending map to the logical end of
/// that line. The boundary after LF belongs to the next line. A standalone CR
/// remains content. Offsets are revalidated against `text`, and columns use
/// the scalar and tab convention of [`columns::offset_to_visual`].
pub fn position_at_offset(
    text: &Text,
    offset: ByteOffset,
    tab_width: usize,
) -> Result<VisualPosition, ColumnError> {
    let index = LineIndex::from_text(text);
    let line = index
        .offset_to_line_col(text, offset)
        .map_err(ColumnError::Line)?
        .line;
    let span = index.line_span(line).map_err(ColumnError::Line)?;
    let start = span.start().as_usize();
    let end = content_end(text, span);
    let logical = Text::from_str(&text.as_str()[start..end]);
    let logical_offset = logical
        .validate_offset(offset.as_usize().min(end) - start)
        .expect("clamped offset is a character boundary");
    let logical_index = LineIndex::from_text(&logical);
    let column =
        columns::offset_to_visual(&logical, &logical_index, logical_offset, tab_width)?.column;
    Ok(VisualPosition { line, column })
}

/// Maps a rectangle using the scalar-width convention of `columns`.
/// Use [`position_at_offset`] to construct corners from byte offsets.
pub fn rectangle_spans(
    text: &Text,
    first: VisualPosition,
    second: VisualPosition,
    endpoint: Endpoint,
    tab_width: usize,
) -> Result<Vec<ByteSpan>, RectangleError> {
    rectangle_spans_with_width(text, first, second, endpoint, tab_width, |_| 1)
}

/// Maps a rectangle using caller-defined display units.
///
/// For each non-tab suffix of a line, `unit` returns the UTF-8 byte length and
/// positive cell width of its first display unit. The length must end on a
/// character boundary and must not include a tab. Tabs are handled separately.
/// Multi-scalar units are indivisible: selection rounds outward to their byte
/// boundaries. Callers must provide consistent segmentation and widths, such
/// as those used by their editor; no grapheme or emoji rules are inferred here.
pub fn rectangle_spans_with_units<F: Fn(&str) -> (usize, usize)>(
    text: &Text,
    first: VisualPosition,
    second: VisualPosition,
    endpoint: Endpoint,
    tab_width: usize,
    unit: F,
) -> Result<Vec<ByteSpan>, RectangleError> {
    rectangle_spans_scan(text, first, second, endpoint, tab_width, unit)
}

/// Maps a rectangle using a positive width for each non-tab scalar.
///
/// The width function must be consistent throughout a call. One span per
/// line is returned in ascending order, including empty lines. Virtual cells
/// beyond the end of a line map to an empty span at the end.
pub fn rectangle_spans_with_width<F: Fn(char) -> usize>(
    text: &Text,
    first: VisualPosition,
    second: VisualPosition,
    endpoint: Endpoint,
    tab_width: usize,
    width: F,
) -> Result<Vec<ByteSpan>, RectangleError> {
    rectangle_spans_with_units(text, first, second, endpoint, tab_width, |remaining| {
        let ch = remaining.chars().next().expect("nonempty line suffix");
        (ch.len_utf8(), width(ch))
    })
}

fn rectangle_spans_scan<F: Fn(&str) -> (usize, usize)>(
    text: &Text,
    first: VisualPosition,
    second: VisualPosition,
    endpoint: Endpoint,
    tab_width: usize,
    unit: F,
) -> Result<Vec<ByteSpan>, RectangleError> {
    if tab_width == 0 {
        return Err(RectangleError::ZeroTabWidth);
    }
    let index = LineIndex::from_text(text);
    let top = first.line.min(second.line);
    let bottom = first.line.max(second.line);
    index.line_span(bottom).map_err(RectangleError::Line)?;
    let left = first.column.min(second.column);
    let right = first.column.max(second.column);
    let end_column = match endpoint {
        Endpoint::Exclusive => Some(right),
        Endpoint::Inclusive => right.checked_add(1),
    };
    let mut spans = Vec::with_capacity(bottom - top + 1);
    for line in top..=bottom {
        let content = index.line_span(line).map_err(RectangleError::Line)?;
        let start = content.start().as_usize();
        let end = content_end(text, content);
        let mut column = 0usize;
        let mut left_byte = None;
        let mut right_byte = None;
        let mut byte = start;
        while byte < end {
            let remaining = &text.as_str()[byte..end];
            let (len, cells) = if remaining.starts_with('\t') {
                (1, tab_width - column % tab_width)
            } else {
                unit(remaining)
            };
            if len == 0
                || len > remaining.len()
                || !remaining.is_char_boundary(len)
                || (len > 1 && remaining.as_bytes()[..len].contains(&b'\t'))
            {
                return Err(RectangleError::InvalidDisplayUnit);
            }
            if cells == 0 {
                return Err(RectangleError::ZeroCharacterWidth);
            }
            let checked_next = column.checked_add(cells);
            let next = checked_next.unwrap_or(usize::MAX);
            if left_byte.is_none() && (left < next || checked_next.is_none()) {
                left_byte = Some(byte);
            }
            if right_byte.is_none()
                && (checked_next.is_none() || end_column.is_some_and(|end| end <= next))
            {
                right_byte = Some(if end_column.is_some_and(|end| end <= column) {
                    byte
                } else {
                    byte + len
                });
            }
            if left_byte.is_some() && right_byte.is_some() {
                break;
            }
            column = checked_next.ok_or(RectangleError::ColumnOverflow)?;
            byte += len;
        }
        let begin = left_byte.unwrap_or(end);
        let finish = if endpoint == Endpoint::Exclusive && left == right {
            begin
        } else {
            right_byte.unwrap_or(end)
        };
        spans.push(ByteSpan::validate_in(text, begin, finish).expect("line character boundaries"));
    }
    Ok(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};

    fn pos(line: usize, column: usize) -> VisualPosition {
        VisualPosition { line, column }
    }

    fn width(ch: char) -> usize {
        if matches!(ch, '界' | '😀') {
            2
        } else {
            1
        }
    }

    #[test]
    fn offset_corners_ignore_crlf() {
        for (lf, crlf, expected_corner, expected_slices) in [
            ("a\nXYZ", "a\r\nXYZ", pos(0, 1), vec!["a", "X"]),
            (
                "界\t猫\nabcdef",
                "界\t猫\r\nabcdef",
                pos(0, 5),
                vec!["界\t猫", "abcde"],
            ),
        ] {
            for source in [lf, crlf] {
                let text = Text::from_str(source);
                let lines = LineIndex::from_text(&text);
                let first =
                    position_at_offset(&text, lines.line_span(0).unwrap().end(), 4).unwrap();
                let second =
                    position_at_offset(&text, lines.line_span(1).unwrap().start(), 4).unwrap();
                assert_eq!(first, expected_corner);
                assert_eq!(second, pos(1, 0));
                if source == crlf {
                    let cr = text.validate_offset(source.find('\r').unwrap()).unwrap();
                    assert_eq!(position_at_offset(&text, cr, 4), Ok(expected_corner));
                }
                for (a, b) in [(first, second), (second, first)] {
                    let spans = rectangle_spans(&text, a, b, Endpoint::Exclusive, 4).unwrap();
                    let slices: Vec<_> = spans
                        .iter()
                        .map(|span| span.slice_in(&text).unwrap())
                        .collect();
                    assert_eq!(slices, expected_slices);
                }
            }
        }
        let text = Text::from_str("界\r");
        assert_eq!(
            position_at_offset(&text, text.validate_offset(text.len_bytes()).unwrap(), 4),
            Ok(pos(0, 2))
        );
        let other = Text::from_str("abcdef");
        assert_eq!(
            position_at_offset(&text, other.validate_offset(1).unwrap(), 4),
            Err(ColumnError::Line(LineError::NotCharBoundary { offset: 1 }))
        );
        assert_eq!(
            position_at_offset(&text, other.validate_offset(5).unwrap(), 4),
            Err(ColumnError::Line(LineError::OutOfBounds {
                offset: 5,
                len: 4
            }))
        );
        assert_eq!(
            position_at_offset(&text, text.validate_offset(0).unwrap(), 0),
            Err(ColumnError::ZeroTabWidth)
        );
    }

    fn expected(line: &str, left: usize, right: usize, endpoint: Endpoint) -> (usize, usize) {
        let mut boundaries = vec![(0, 0usize)];
        let mut col = 0usize;
        for (byte, ch) in line.char_indices() {
            col += if ch == '\t' { 4 - col % 4 } else { width(ch) };
            boundaries.push((byte + ch.len_utf8(), col));
        }
        let begin = boundaries.iter().rev().find(|(_, c)| *c <= left).unwrap().0;
        let target = if endpoint == Endpoint::Inclusive {
            right.saturating_add(1)
        } else {
            right
        };
        let end = if left == target {
            begin
        } else {
            boundaries
                .iter()
                .find(|(_, c)| *c >= target)
                .unwrap_or(boundaries.last().unwrap())
                .0
        };
        (begin, end)
    }

    #[test]
    fn rectangle_laws() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus.iter().map(String::as_str).chain([
                "",
                "\n",
                "a\r\n\t界😀\n",
                "\t界😀",
                "x\r",
            ]) {
                let text = Text::from_str(source);
                let index = LineIndex::from_text(&text);
                let bottom = index.line_count() - 1;
                for left in 0..=5 {
                    for right in left..=6 {
                        for endpoint in [Endpoint::Exclusive, Endpoint::Inclusive] {
                            let map = |a, b| {
                                rectangle_spans_with_width(&text, a, b, endpoint, 4, width).unwrap()
                            };
                            let spans = map(pos(0, left), pos(bottom, right));
                            assert_eq!(spans, map(pos(bottom, right), pos(0, left)));
                            assert_eq!(spans, map(pos(0, right), pos(bottom, left)));
                            assert_eq!(spans, map(pos(bottom, left), pos(0, right)));
                            assert_eq!(spans.len(), index.line_count());
                            for (line, span) in spans.iter().enumerate() {
                                assert!(index.line_span(line).unwrap().contains_span(*span));
                                let slice = span.slice_in(&text).unwrap();
                                assert!(!slice.contains('\n'));
                                let range = index.line_span(line).unwrap();
                                let start = range.start().as_usize();
                                let mut end = range.end().as_usize();
                                if source.as_bytes().get(end) == Some(&b'\n')
                                    && end > start
                                    && source.as_bytes()[end - 1] == b'\r'
                                {
                                    end -= 1;
                                }
                                let (a, b) = expected(&source[start..end], left, right, endpoint);
                                assert_eq!(
                                    (span.start().as_usize(), span.end().as_usize()),
                                    (start + a, start + b)
                                );
                            }
                            assert!(spans
                                .windows(2)
                                .all(|pair| pair[0].end() <= pair[1].start()));
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn rounding_clamping_and_endpoints() {
        let text = Text::from_str("\t界😀x\r\nq\n");
        let map = |left, right, endpoint| {
            rectangle_spans_with_width(&text, pos(0, left), pos(2, right), endpoint, 4, width)
                .unwrap()
        };
        let slices = |spans: Vec<ByteSpan>| {
            spans
                .into_iter()
                .map(|s| s.slice_in(&text).unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(slices(map(1, 5, Endpoint::Exclusive)), ["\t界", "", ""]);
        assert_eq!(slices(map(5, 5, Endpoint::Exclusive)), ["", "", ""]);
        assert_eq!(slices(map(5, 5, Endpoint::Inclusive)), ["界", "", ""]);
        assert_eq!(slices(map(6, 7, Endpoint::Exclusive)), ["😀", "", ""]);
        assert_eq!(slices(map(20, 30, Endpoint::Inclusive)), ["", "", ""]);
        assert_eq!(
            rectangle_spans(&text, pos(0, 0), pos(0, 0), Endpoint::Inclusive, 4).unwrap()[0]
                .slice_in(&text),
            Ok("\t")
        );
        assert_eq!(
            rectangle_spans(&text, pos(0, 0), pos(0, 0), Endpoint::Exclusive, 0),
            Err(RectangleError::ZeroTabWidth)
        );
        assert_eq!(
            rectangle_spans_with_width(&text, pos(1, 0), pos(1, 0), Endpoint::Exclusive, 4, |_| 0),
            Err(RectangleError::ZeroCharacterWidth)
        );
        assert_eq!(
            slices(map(usize::MAX, usize::MAX, Endpoint::Inclusive)),
            ["", "", ""]
        );
        assert!(matches!(
            rectangle_spans(&text, pos(3, 0), pos(0, 0), Endpoint::Exclusive, 4),
            Err(RectangleError::Line(_))
        ));
    }

    #[test]
    fn selected_lines_and_nonleading_tab() {
        let text = Text::from_str("ignored\nx\t界😀\nq\nlast");
        let crossing =
            rectangle_spans_with_width(&text, pos(1, 2), pos(1, 5), Endpoint::Exclusive, 4, width)
                .unwrap();
        assert_eq!(
            (crossing[0].start().as_usize(), crossing[0].end().as_usize()),
            (9, 13)
        );
        assert_eq!(crossing[0].slice_in(&text), Ok("\t界"));
        for (a, b) in [
            (pos(1, 4), pos(2, 5)),
            (pos(2, 5), pos(1, 4)),
            (pos(1, 5), pos(2, 4)),
            (pos(2, 4), pos(1, 5)),
        ] {
            let spans =
                rectangle_spans_with_width(&text, a, b, Endpoint::Exclusive, 4, width).unwrap();
            assert_eq!(spans.len(), 2);
            assert_eq!(
                (spans[0].start().as_usize(), spans[0].end().as_usize()),
                (10, 13)
            );
            assert_eq!(spans[0].slice_in(&text), Ok("界"));
            assert_eq!(
                (spans[1].start().as_usize(), spans[1].end().as_usize()),
                (19, 19)
            );
        }
    }

    #[test]
    fn huge_tab_width_rounding() {
        let text = Text::from_str("\t\t");
        let spans = rectangle_spans(
            &text,
            pos(0, usize::MAX - 1),
            pos(0, usize::MAX),
            Endpoint::Exclusive,
            usize::MAX / 2 + 1,
        )
        .unwrap();
        assert_eq!(
            (spans[0].start().as_usize(), spans[0].end().as_usize()),
            (1, 2)
        );
        let text = Text::from_str("\tX");
        let spans =
            rectangle_spans(&text, pos(0, 0), pos(0, 1), Endpoint::Exclusive, usize::MAX).unwrap();
        assert_eq!(
            (spans[0].start().as_usize(), spans[0].end().as_usize()),
            (0, 1)
        );
        for (left, expected) in [(usize::MAX, (1, 2)), (usize::MAX - 1, (0, 2))] {
            let spans = rectangle_spans(
                &text,
                pos(0, left),
                pos(0, usize::MAX),
                Endpoint::Inclusive,
                usize::MAX,
            )
            .unwrap();
            assert_eq!(
                (spans[0].start().as_usize(), spans[0].end().as_usize()),
                expected
            );
        }
    }

    #[test]
    fn multi_scalar_display_units() {
        let text = Text::from_str("👩‍💻X\r\n\t👩‍💻X\n");
        let unit = |s: &str| {
            if s.starts_with("👩‍💻") {
                ("👩‍💻".len(), 2)
            } else {
                (s.chars().next().unwrap().len_utf8(), 1)
            }
        };
        let map = |line, left, right, mode| {
            rectangle_spans_with_units(&text, pos(line, left), pos(line, right), mode, 4, unit)
                .unwrap()[0]
                .slice_in(&text)
                .unwrap()
        };
        assert_eq!(map(0, 0, 1, Endpoint::Inclusive), "👩‍💻");
        assert_eq!(map(0, 2, 2, Endpoint::Inclusive), "X");
        assert_eq!(map(0, 1, 2, Endpoint::Exclusive), "👩‍💻");
        assert_eq!(map(0, 1, 1, Endpoint::Exclusive), "");
        assert_eq!(map(1, 4, 5, Endpoint::Inclusive), "👩‍💻");
        assert_eq!(map(1, 6, 6, Endpoint::Inclusive), "X");
        for invalid in [(0, 1), (1, 1), (usize::MAX, 1)] {
            assert_eq!(
                rectangle_spans_with_units(
                    &text,
                    pos(0, 0),
                    pos(0, 1),
                    Endpoint::Exclusive,
                    4,
                    |_| invalid
                ),
                Err(RectangleError::InvalidDisplayUnit)
            );
        }
        assert_eq!(
            rectangle_spans_with_units(&text, pos(0, 0), pos(0, 1), Endpoint::Exclusive, 4, |_| (
                "👩‍💻".len(),
                0
            )),
            Err(RectangleError::ZeroCharacterWidth)
        );
        let tab = Text::from_str("a\tb");
        assert_eq!(
            rectangle_spans_with_units(&tab, pos(0, 0), pos(0, 1), Endpoint::Exclusive, 4, |_| (
                2, 1
            )),
            Err(RectangleError::InvalidDisplayUnit)
        );
    }
}
