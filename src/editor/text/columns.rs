//! Visual columns over UTF-8 lines, counting each scalar as one except tabs.
//!
//! Tabs advance to the next nonzero-width tab stop. LF terminates a line and
//! CR is content. An LF boundary belongs to the preceding line. Columns are
//! strictly increasing at character boundaries unless their value overflows.
//! An index and an offset are value types: each is revalidated against the
//! target text, so values originating in another text are accepted only when
//! their line boundaries or UTF-8 boundary respectively remain valid.

use super::lines::{LineError, LineIndex};
use super::utf8::{ByteOffset, Text};
use std::fmt;

/// A zero-based line and visual column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VisualPosition {
    /// Zero-based line number.
    pub line: usize,
    /// Scalar column with tabs expanded to tab stops.
    pub column: usize,
}

/// Direction to choose when a visual column falls inside a tab.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rounding {
    /// Choose the boundary before the target.
    Floor,
    /// Choose the boundary after the target.
    Ceil,
}

/// A failure to convert visual columns and byte offsets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ColumnError {
    /// Tab stops require a nonzero width.
    ZeroTabWidth,
    /// The visual column cannot fit in a `usize`.
    ColumnOverflow,
    /// The index has different line boundaries from the target text.
    IndexMismatch,
    /// A line or byte offset is invalid for the target text.
    Line(LineError),
}

impl fmt::Display for ColumnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroTabWidth => write!(f, "tab width must be nonzero"),
            Self::ColumnOverflow => write!(f, "visual column overflow"),
            Self::IndexMismatch => write!(f, "line index does not match text"),
            Self::Line(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for ColumnError {}

fn check_index(text: &Text, index: &LineIndex, tab_width: usize) -> Result<(), ColumnError> {
    if tab_width == 0 {
        return Err(ColumnError::ZeroTabWidth);
    }
    let expected = LineIndex::from_text(text);
    if index.line_count() != expected.line_count()
        || (0..expected.line_count()).any(|line| {
            index.line_span(line) != expected.line_span(line)
                || index.line_span_with_lf(line) != expected.line_span_with_lf(line)
        })
    {
        return Err(ColumnError::IndexMismatch);
    }
    Ok(())
}

fn advance(column: usize, ch: char, tab_width: usize) -> Result<usize, ColumnError> {
    let width = if ch == '\t' {
        tab_width - column % tab_width
    } else {
        1
    };
    column.checked_add(width).ok_or(ColumnError::ColumnOverflow)
}

/// Converts a validated byte offset to its line and visual column.
///
/// Both `index` and `offset` are revalidated against `text`. The boundary at
/// an LF is the end of the preceding line; the boundary after it starts the next.
pub fn offset_to_visual(
    text: &Text,
    index: &LineIndex,
    offset: ByteOffset,
    tab_width: usize,
) -> Result<VisualPosition, ColumnError> {
    check_index(text, index, tab_width)?;
    let pos = index
        .offset_to_line_col(text, offset)
        .map_err(ColumnError::Line)?;
    let start = index
        .line_span(pos.line)
        .map_err(ColumnError::Line)?
        .start()
        .as_usize();
    let mut column = 0;
    for ch in text.as_str()[start..offset.as_usize()].chars() {
        column = advance(column, ch, tab_width)?;
    }
    Ok(VisualPosition {
        line: pos.line,
        column,
    })
}

/// Converts a visual column to a UTF-8 boundary on `line`.
///
/// Exact columns select their boundary. Inside a tab, rounding selects the
/// boundary before or after it. Columns beyond the line end clamp to that end.
/// The supplied index is revalidated against `text`.
pub fn visual_to_offset(
    text: &Text,
    index: &LineIndex,
    line: usize,
    column: usize,
    tab_width: usize,
    rounding: Rounding,
) -> Result<ByteOffset, ColumnError> {
    check_index(text, index, tab_width)?;
    let span = index.line_span(line).map_err(ColumnError::Line)?;
    let start = span.start().as_usize();
    let end = span.end().as_usize();
    let mut current = 0;
    for (relative, ch) in text.as_str()[start..end].char_indices() {
        if column == current {
            return Ok(text
                .validate_offset(start + relative)
                .expect("character boundary"));
        }
        let width = if ch == '\t' {
            tab_width - current % tab_width
        } else {
            1
        };
        if column - current < width {
            let offset = match rounding {
                Rounding::Floor => start + relative,
                Rounding::Ceil => start + relative + ch.len_utf8(),
            };
            return Ok(text.validate_offset(offset).expect("character boundary"));
        }
        current += width;
    }
    Ok(span.end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};

    // Repeated addition models tab stops independently of the production formula.
    fn scan(line: &str, start: usize, width: usize) -> Vec<(usize, usize)> {
        let mut result = vec![(start, 0)];
        let mut column = 0;
        for (i, ch) in line.char_indices() {
            column += 1;
            if ch == '\t' {
                while column % width != 0 {
                    column += 1;
                }
            }
            result.push((start + i + ch.len_utf8(), column));
        }
        result
    }

    #[test]
    fn reference_columns_and_rounding() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            let residues: Vec<_> = (0..8)
                .map(|n| format!("{}\t界\tX", "界".repeat(n)))
                .collect();
            for source in corpus.iter().chain(&residues).map(String::as_str).chain([
                "\t\t界\t😀\t",
                "a\tb\t\t\n\t界\r\n",
                "\n",
                "",
            ]) {
                let text = Text::from_str(source);
                let index = LineIndex::from_text(&text);
                let mut start = 0;
                for (line, content) in source.split('\n').enumerate() {
                    for width in 1..=8 {
                        let expected = scan(content, start, width);
                        for &(byte, column) in &expected {
                            let offset = text.validate_offset(byte).unwrap();
                            assert_eq!(
                                offset_to_visual(&text, &index, offset, width),
                                Ok(VisualPosition { line, column })
                            );
                            for rounding in [Rounding::Floor, Rounding::Ceil] {
                                assert_eq!(
                                    visual_to_offset(&text, &index, line, column, width, rounding),
                                    Ok(offset)
                                );
                            }
                        }
                        for pair in expected.windows(2) {
                            assert!(pair[0].1 < pair[1].1);
                            for target in pair[0].1 + 1..pair[1].1 {
                                assert_eq!(&source[pair[0].0..pair[1].0], "\t");
                                for (rounding, expected_byte) in
                                    [(Rounding::Floor, pair[0].0), (Rounding::Ceil, pair[1].0)]
                                {
                                    let actual = visual_to_offset(
                                        &text, &index, line, target, width, rounding,
                                    )
                                    .unwrap();
                                    assert_eq!(actual.as_usize(), expected_byte);
                                }
                            }
                        }
                        let end = expected.last().unwrap();
                        for rounding in [Rounding::Floor, Rounding::Ceil] {
                            assert_eq!(
                                visual_to_offset(&text, &index, line, end.1 + 1, width, rounding)
                                    .unwrap()
                                    .as_usize(),
                                end.0
                            );
                        }
                    }
                    start += content.len() + 1;
                }
            }
        });
    }

    #[test]
    fn validation_and_overflow() {
        let text = Text::from_str("界\n\tX");
        let index = LineIndex::from_text(&text);
        let foreign = Text::from_str("abcdefg");
        let at = |n| foreign.validate_offset(n).unwrap();
        assert_eq!(
            offset_to_visual(&text, &index, at(1), 4),
            Err(ColumnError::Line(LineError::NotCharBoundary { offset: 1 }))
        );
        assert_eq!(
            offset_to_visual(&text, &index, at(7), 4),
            Err(ColumnError::Line(LineError::OutOfBounds {
                offset: 7,
                len: 6
            }))
        );
        let wrong = LineIndex::from_text(&foreign);
        assert_eq!(
            offset_to_visual(&text, &wrong, at(0), 4),
            Err(ColumnError::IndexMismatch)
        );
        assert_eq!(
            visual_to_offset(&text, &wrong, 0, 0, 4, Rounding::Floor),
            Err(ColumnError::IndexMismatch)
        );
        let shifted = LineIndex::from_text(&Text::from_str("a\nbcde"));
        assert_eq!(
            offset_to_visual(&text, &shifted, at(0), 4),
            Err(ColumnError::IndexMismatch)
        );
        assert_eq!(
            visual_to_offset(&text, &shifted, 1, 1, 4, Rounding::Ceil),
            Err(ColumnError::IndexMismatch)
        );
        assert_eq!(
            offset_to_visual(&text, &index, at(0), 0),
            Err(ColumnError::ZeroTabWidth)
        );
        assert_eq!(
            visual_to_offset(&text, &index, 0, 0, 0, Rounding::Ceil),
            Err(ColumnError::ZeroTabWidth)
        );
        assert_eq!(
            visual_to_offset(&text, &index, 2, 0, 4, Rounding::Floor),
            Err(ColumnError::Line(LineError::LineOutOfRange {
                line: 2,
                line_count: 2
            }))
        );
        let huge = Text::from_str("\tX");
        let huge_index = LineIndex::from_text(&huge);
        assert_eq!(
            offset_to_visual(
                &huge,
                &huge_index,
                huge.validate_offset(2).unwrap(),
                usize::MAX
            ),
            Err(ColumnError::ColumnOverflow)
        );
        assert_eq!(
            visual_to_offset(
                &huge,
                &huge_index,
                0,
                usize::MAX,
                usize::MAX,
                Rounding::Floor
            )
            .unwrap()
            .as_usize(),
            1
        );
        let huge = Text::from_str("\t\t");
        let huge_index = LineIndex::from_text(&huge);
        for (rounding, expected) in [(Rounding::Floor, 1), (Rounding::Ceil, 2)] {
            assert_eq!(
                visual_to_offset(
                    &huge,
                    &huge_index,
                    0,
                    usize::MAX,
                    usize::MAX / 2 + 1,
                    rounding
                )
                .unwrap()
                .as_usize(),
                expected
            );
        }
    }
}
