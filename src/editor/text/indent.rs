//! Indentation and tab expansion using scalar columns.
//!
//! Each non-tab Unicode scalar advances one column; tabs advance to the next
//! multiple of the tab width. CR and LF are ordinary scalars during expansion.
//! Leading indentation consists only of ASCII spaces and tabs, so line endings
//! and all other whitespace remain untouched. A supplied [`LineIndex`] is
//! checked against the target text's complete line boundaries before use;
//! returned [`ByteSpan`] offsets are valid for that text and must be
//! revalidated with [`ByteSpan::slice_in`] when used with another text.

use super::lines::{LineError, LineIndex};
use super::span::ByteSpan;
use super::utf8::Text;
use std::fmt;

/// An indentation operation failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndentError {
    /// A tab stop cannot have zero width.
    ZeroTabWidth,
    /// The resulting visual column cannot fit in a `usize`.
    ColumnOverflow,
    /// The intermediate output buffer cannot be allocated. Construction of
    /// the returned `Text` performs a separate, infallible allocation.
    OutputTooLarge,
    /// The line number is invalid.
    Line(LineError),
    /// The index's line boundaries differ from those of the target text.
    IndexMismatch,
}

impl fmt::Display for IndentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroTabWidth => write!(f, "tab width must be nonzero"),
            Self::ColumnOverflow => write!(f, "visual column overflow"),
            Self::OutputTooLarge => write!(f, "output cannot be allocated"),
            Self::Line(error) => error.fmt(f),
            Self::IndexMismatch => write!(f, "line index does not match text"),
        }
    }
}
impl std::error::Error for IndentError {}

/// Number of columns to the next tab stop, including at a stop.
pub fn tab_stop_width(column: usize, tab_width: usize) -> Result<usize, IndentError> {
    if tab_width == 0 {
        return Err(IndentError::ZeroTabWidth);
    }
    Ok(tab_width - column % tab_width)
}

/// Expands tabs to spaces and returns the resulting text and final column.
///
/// The initial column is explicit, allowing consecutive fragments to compose.
pub fn expand_tabs(
    text: &Text,
    start_column: usize,
    tab_width: usize,
) -> Result<(Text, usize), IndentError> {
    if tab_width == 0 {
        return Err(IndentError::ZeroTabWidth);
    }
    let mut result = String::new();
    let mut column = start_column;
    for ch in text.as_str().chars() {
        let width = if ch == '\t' {
            tab_stop_width(column, tab_width)?
        } else {
            1
        };
        column = column
            .checked_add(width)
            .ok_or(IndentError::ColumnOverflow)?;
        result
            .try_reserve(if ch == '\t' { width } else { ch.len_utf8() })
            .map_err(|_| IndentError::OutputTooLarge)?;
        if ch == '\t' {
            result.extend(std::iter::repeat_n(' ', width));
        } else {
            result.push(ch);
        }
    }
    Ok((Text::from_str(&result), column))
}

/// Measured leading spaces and tabs on a line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Indentation {
    /// Span of the leading spaces and tabs in the original text.
    pub span: ByteSpan,
    /// Visual column reached from column zero.
    pub column: usize,
    /// Number of space bytes.
    pub spaces: usize,
    /// Number of tab bytes.
    pub tabs: usize,
}

fn check_index(text: &Text, index: &LineIndex) -> Result<(), IndentError> {
    let expected = LineIndex::from_text(text);
    if index.line_count() != expected.line_count()
        || (0..expected.line_count()).any(|line| {
            index.line_span(line) != expected.line_span(line)
                || index.line_span_with_lf(line) != expected.line_span_with_lf(line)
        })
    {
        return Err(IndentError::IndexMismatch);
    }
    Ok(())
}

fn measure(
    text: &Text,
    index: &LineIndex,
    line: usize,
    tab_width: usize,
) -> Result<Indentation, IndentError> {
    let span = index.line_span(line).map_err(IndentError::Line)?;
    let start = span.start().as_usize();
    let mut end = start;
    let mut column = 0usize;
    let mut spaces = 0;
    let mut tabs = 0;
    for byte in text.as_bytes()[start..span.end().as_usize()]
        .iter()
        .copied()
    {
        let width = match byte {
            b' ' => {
                spaces += 1;
                1
            }
            b'\t' => {
                tabs += 1;
                tab_stop_width(column, tab_width)?
            }
            _ => break,
        };
        column = column
            .checked_add(width)
            .ok_or(IndentError::ColumnOverflow)?;
        end += 1;
    }
    Ok(Indentation {
        span: ByteSpan::validate_in(text, start, end).expect("ASCII boundaries"),
        column,
        spaces,
        tabs,
    })
}

/// Measures one line's indentation after checking the index against `text`.
pub fn leading_indentation(
    text: &Text,
    index: &LineIndex,
    line: usize,
    tab_width: usize,
) -> Result<Indentation, IndentError> {
    if tab_width == 0 {
        return Err(IndentError::ZeroTabWidth);
    }
    check_index(text, index)?;
    measure(text, index, line, tab_width)
}

/// Output representation for leading indentation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IndentStyle {
    /// Use only spaces.
    Spaces,
    /// Use as many tabs as possible, followed by spaces.
    TabsAndSpaces,
}

/// Normalizes each line's indentation, retaining all remaining bytes verbatim.
///
/// The supplied index is checked against the target text before use.
pub fn normalize_indentation(
    text: &Text,
    index: &LineIndex,
    tab_width: usize,
    style: IndentStyle,
) -> Result<Text, IndentError> {
    if tab_width == 0 {
        return Err(IndentError::ZeroTabWidth);
    }
    check_index(text, index)?;
    let mut result = String::new();
    for line in 0..index.line_count() {
        let indent = measure(text, index, line, tab_width)?;
        let (tabs, spaces) = match style {
            IndentStyle::Spaces => (0, indent.column),
            IndentStyle::TabsAndSpaces => (indent.column / tab_width, indent.column % tab_width),
        };
        let end = index
            .line_span_with_lf(line)
            .expect("valid line")
            .end()
            .as_usize();
        let remainder = &text.as_str()[indent.span.end().as_usize()..end];
        let extra = tabs
            .checked_add(spaces)
            .and_then(|n| n.checked_add(remainder.len()))
            .ok_or(IndentError::OutputTooLarge)?;
        result
            .try_reserve(extra)
            .map_err(|_| IndentError::OutputTooLarge)?;
        result.extend(std::iter::repeat_n('\t', tabs));
        result.extend(std::iter::repeat_n(' ', spaces));
        result.push_str(remainder);
    }
    Ok(Text::from_str(&result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};

    #[test]
    fn examples_and_errors() {
        let body = Text::from_str(" \tfoo \t bar\r\n");
        let body_index = LineIndex::from_text(&body);
        assert_eq!(
            normalize_indentation(&body, &body_index, 4, IndentStyle::Spaces)
                .unwrap()
                .as_str(),
            "    foo \t bar\r\n"
        );
        assert_eq!(
            normalize_indentation(&body, &body_index, 4, IndentStyle::TabsAndSpaces)
                .unwrap()
                .as_str(),
            "\tfoo \t bar\r\n"
        );
        let text = Text::from_str(" \t  x\r\n\t\r\n  \n");
        let index = LineIndex::from_text(&text);
        let first = leading_indentation(&text, &index, 0, 4).unwrap();
        assert_eq!(
            (
                first.span.slice_in(&text).unwrap(),
                first.column,
                first.spaces,
                first.tabs
            ),
            (" \t  ", 6, 3, 1)
        );
        assert_eq!(
            normalize_indentation(&text, &index, 4, IndentStyle::Spaces)
                .unwrap()
                .as_str(),
            "      x\r\n    \r\n  \n"
        );
        assert_eq!(
            normalize_indentation(&text, &index, 4, IndentStyle::TabsAndSpaces)
                .unwrap()
                .as_str(),
            "\t  x\r\n\t\r\n  \n"
        );
        assert_eq!(
            expand_tabs(&Text::from_str("a\t界\r\n\tz"), 2, 4),
            Ok((Text::from_str("a 界\r\n z"), 9))
        );
        let huge = Text::from_str("\t");
        assert_eq!(
            expand_tabs(&huge, 0, usize::MAX),
            Err(IndentError::OutputTooLarge)
        );
        assert_eq!(
            normalize_indentation(
                &huge,
                &LineIndex::from_text(&huge),
                usize::MAX,
                IndentStyle::Spaces
            ),
            Err(IndentError::OutputTooLarge)
        );
        assert_eq!(tab_stop_width(4, 4), Ok(4));
        assert_eq!(tab_stop_width(0, 0), Err(IndentError::ZeroTabWidth));
        assert_eq!(expand_tabs(&text, 0, 0), Err(IndentError::ZeroTabWidth));
        assert_eq!(
            expand_tabs(&Text::from_str("x"), usize::MAX, 4),
            Err(IndentError::ColumnOverflow)
        );
        assert!(matches!(
            leading_indentation(&text, &index, 99, 4),
            Err(IndentError::Line(LineError::LineOutOfRange { .. }))
        ));
        assert_eq!(
            leading_indentation(&text, &LineIndex::from_text(&Text::from_str("x")), 0, 4),
            Err(IndentError::IndexMismatch)
        );
    }

    #[test]
    fn composition_and_normalization_laws() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let source = format!(" \t{source}\n\t  {source}\r\n");
                let text = Text::from_str(&source);
                for width in 1..=8 {
                    for start in 0..8 {
                        let (whole, col) = expand_tabs(&text, start, width).unwrap();
                        for boundary in text.char_boundaries() {
                            let (a, b) = text.split_at(boundary).unwrap();
                            let (left, middle) = expand_tabs(&a, start, width).unwrap();
                            let (right, end) = expand_tabs(&b, middle, width).unwrap();
                            assert_eq!(whole, left.concat(&right));
                            assert_eq!(col, end);
                        }
                    }
                    let index = LineIndex::from_text(&text);
                    for style in [IndentStyle::Spaces, IndentStyle::TabsAndSpaces] {
                        let normalized =
                            normalize_indentation(&text, &index, width, style).unwrap();
                        let new_index = LineIndex::from_text(&normalized);
                        assert_eq!(
                            normalize_indentation(&normalized, &new_index, width, style).unwrap(),
                            normalized
                        );
                        assert_eq!(index.line_count(), new_index.line_count());
                        for line in 0..index.line_count() {
                            let a = leading_indentation(&text, &index, line, width).unwrap();
                            let b =
                                leading_indentation(&normalized, &new_index, line, width).unwrap();
                            assert_eq!(a.column, b.column);
                            let ae = index.line_span_with_lf(line).unwrap().end().as_usize();
                            let be = new_index.line_span_with_lf(line).unwrap().end().as_usize();
                            assert_eq!(
                                &text.as_str()[a.span.end().as_usize()..ae],
                                &normalized.as_str()[b.span.end().as_usize()..be]
                            );
                        }
                    }
                }
            }
        });
    }
}
