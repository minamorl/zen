//! Pure indentation plans for half-open ranges of lines.

use std::{fmt, ops::Range};

use crate::editor::edit::{
    patch::{Patch, PatchError},
    splice::Splice,
};
use crate::editor::text::{
    indent::{IndentError, IndentStyle},
    lines::LineIndex,
    span::{ByteSpan, SpanError},
    utf8::Text,
};

/// A fixed visual indentation column and its representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndentPolicy {
    pub column: usize,
    pub tab_width: usize,
    pub style: IndentStyle,
}

/// A region or patch is invalid for the supplied text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IndentRegionError {
    InvalidRegion {
        start: usize,
        end: usize,
        line_count: usize,
    },
    InvalidSpan(SpanError),
    Indent(IndentError),
    Patch(PatchError),
    OutsideRegion {
        span: ByteSpan,
    },
    NonIndentEdit {
        span: ByteSpan,
    },
}

impl fmt::Display for IndentRegionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRegion {
                start,
                end,
                line_count,
            } => write!(
                f,
                "invalid line range {start}..{end} for {line_count} lines"
            ),
            Self::InvalidSpan(error) => error.fmt(f),
            Self::Indent(error) => error.fmt(f),
            Self::Patch(error) => error.fmt(f),
            Self::OutsideRegion { span } => write!(f, "edit outside region: {span:?}"),
            Self::NonIndentEdit { span } => {
                write!(f, "edit changes non-indentation content: {span:?}")
            }
        }
    }
}
impl std::error::Error for IndentRegionError {}

fn check_region(index: &LineIndex, region: &Range<usize>) -> Result<(), IndentRegionError> {
    if region.start > region.end || region.end > index.line_count() {
        return Err(IndentRegionError::InvalidRegion {
            start: region.start,
            end: region.end,
            line_count: index.line_count(),
        });
    }
    Ok(())
}

fn leading_span(text: &Text, index: &LineIndex, line: usize) -> ByteSpan {
    let span = index.line_span(line).expect("checked line");
    let start = span.start().as_usize();
    let end = start
        + text.as_bytes()[start..span.end().as_usize()]
            .iter()
            .take_while(|&&byte| matches!(byte, b' ' | b'\t'))
            .count();
    ByteSpan::validate_in(text, start, end).expect("ASCII boundaries")
}

/// Validates an externally supplied patch against the current text and region.
/// Only spaces and tabs in a target line's leading indentation may be replaced.
pub fn validate_patch(
    text: &Text,
    region: Range<usize>,
    patch: &Patch,
) -> Result<(), IndentRegionError> {
    let index = LineIndex::from_text(text);
    check_region(&index, &region)?;
    for splice in patch.splices() {
        let span = splice.span();
        span.slice_in(text)
            .map_err(IndentRegionError::InvalidSpan)?;
        let line = index
            .offset_to_line_col(text, span.start())
            .expect("validated offset")
            .line;
        let region_end = if region.is_empty() {
            None
        } else {
            Some(
                index
                    .line_span_with_lf(region.end - 1)
                    .expect("valid line")
                    .end(),
            )
        };
        if !region.contains(&line) || region_end.is_none_or(|end| span.end() > end) {
            return Err(IndentRegionError::OutsideRegion { span });
        }
        if !leading_span(text, &index, line).contains_span(span)
            || !splice
                .replacement()
                .as_bytes()
                .iter()
                .all(|b| matches!(b, b' ' | b'\t'))
        {
            return Err(IndentRegionError::NonIndentEdit { span });
        }
    }
    Ok(())
}

/// Plans a canonical patch without changing the input. The line range is half-open.
pub fn plan(
    text: &Text,
    region: Range<usize>,
    policy: IndentPolicy,
) -> Result<Patch, IndentRegionError> {
    let index = LineIndex::from_text(text);
    check_region(&index, &region)?;
    if policy.tab_width == 0 {
        return Err(IndentRegionError::Indent(IndentError::ZeroTabWidth));
    }
    if region.is_empty() {
        return Ok(Patch::default());
    }
    let (tabs, spaces) = match policy.style {
        IndentStyle::Spaces => (0, policy.column),
        IndentStyle::TabsAndSpaces => (
            policy.column / policy.tab_width,
            policy.column % policy.tab_width,
        ),
    };
    let mut replacement = String::new();
    let size = tabs
        .checked_add(spaces)
        .ok_or(IndentRegionError::Indent(IndentError::OutputTooLarge))?;
    replacement
        .try_reserve(size)
        .map_err(|_| IndentRegionError::Indent(IndentError::OutputTooLarge))?;
    replacement.extend(std::iter::repeat_n('\t', tabs));
    replacement.extend(std::iter::repeat_n(' ', spaces));
    let mut splices = Vec::new();
    for line in region.clone() {
        let indent = leading_span(text, &index, line);
        if indent.slice_in(text).expect("fresh indentation") != replacement {
            splices.push(Splice::new(indent, Text::from_str(&replacement)));
        }
    }
    let patch = Patch::new(splices).map_err(IndentRegionError::Patch)?;
    validate_patch(text, region, &patch)?;
    Ok(patch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};

    #[test]
    fn region_laws() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let source = format!(" \t界\n\t  {source}\n  \n😀");
                let text = Text::from_str(&source);
                let index = LineIndex::from_text(&text);
                for start in 0..=index.line_count() {
                    for end in start..=index.line_count() {
                        for style in [IndentStyle::Spaces, IndentStyle::TabsAndSpaces] {
                            let policy = IndentPolicy {
                                column: 5,
                                tab_width: 4,
                                style,
                            };
                            let patch = plan(&text, start..end, policy).unwrap();
                            for line in [
                                start.checked_sub(1),
                                (end < index.line_count()).then_some(end),
                            ]
                            .into_iter()
                            .flatten()
                            {
                                let at = index.line_span(line).unwrap().start();
                                let outside =
                                    Patch::new(vec![Splice::insert(at, Text::from_str(" "))])
                                        .unwrap();
                                assert!(matches!(
                                    validate_patch(&text, start..end, &outside),
                                    Err(IndentRegionError::OutsideRegion { .. })
                                ));
                            }
                            if start < end
                                && start > 0
                                && index.line_span_with_lf(start).unwrap().len_bytes() > 0
                            {
                                let a = index.line_span(start - 1).unwrap().start();
                                let start_offset =
                                    index.line_span(start).unwrap().start().as_usize();
                                let b = text
                                    .validate_offset(
                                        start_offset
                                            + text.as_str()[start_offset..]
                                                .chars()
                                                .next()
                                                .unwrap()
                                                .len_utf8(),
                                    )
                                    .unwrap();
                                let crossing =
                                    Patch::new(vec![Splice::delete(ByteSpan::new(a, b).unwrap())])
                                        .unwrap();
                                assert!(matches!(
                                    validate_patch(&text, start..end, &crossing),
                                    Err(IndentRegionError::OutsideRegion { .. })
                                ));
                            }
                            if start < end
                                && end < index.line_count()
                                && index.line_span(end).unwrap().len_bytes() > 0
                            {
                                let a = index.line_span(end - 1).unwrap().start();
                                let start_offset = index.line_span(end).unwrap().start().as_usize();
                                let b = text
                                    .validate_offset(
                                        start_offset
                                            + text.as_str()[start_offset..]
                                                .chars()
                                                .next()
                                                .unwrap()
                                                .len_utf8(),
                                    )
                                    .unwrap();
                                let crossing =
                                    Patch::new(vec![Splice::delete(ByteSpan::new(a, b).unwrap())])
                                        .unwrap();
                                assert!(matches!(
                                    validate_patch(&text, start..end, &crossing),
                                    Err(IndentRegionError::OutsideRegion { .. })
                                ));
                            }
                            assert_eq!(validate_patch(&text, start..end, &patch), Ok(()));
                            let next = patch.apply(&text).unwrap();
                            let next_index = LineIndex::from_text(&next);
                            assert_eq!(index.line_count(), next_index.line_count());
                            assert!(plan(&next, start..end, policy)
                                .unwrap()
                                .splices()
                                .is_empty());
                            for line in 0..index.line_count() {
                                let a = index.line_span_with_lf(line).unwrap();
                                let b = next_index.line_span_with_lf(line).unwrap();
                                let old = a.slice_in(&text).unwrap();
                                let new = b.slice_in(&next).unwrap();
                                if (start..end).contains(&line) {
                                    let expected = match style {
                                        IndentStyle::Spaces => "     ",
                                        IndentStyle::TabsAndSpaces => "\t ",
                                    };
                                    let actual_indent = leading_span(&next, &next_index, line);
                                    assert_eq!(actual_indent.slice_in(&next).unwrap(), expected);
                                    assert_eq!(
                                        old.trim_start_matches([' ', '\t']),
                                        new.trim_start_matches([' ', '\t'])
                                    );
                                } else {
                                    assert_eq!(old, new);
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn rejects_invalid_external_edits() {
        let text = Text::from_str(" x\n\t界\nend");
        let splice = |start, end, replacement| {
            Patch::new(vec![Splice::new(
                ByteSpan::validate_in(&text, start, end).unwrap(),
                Text::from_str(replacement),
            )])
            .unwrap()
        };
        assert!(matches!(
            validate_patch(&text, 0..3, &splice(2, 3, "")),
            Err(IndentRegionError::NonIndentEdit { .. })
        ));
        assert!(matches!(
            validate_patch(&text, 1..2, &splice(0, 1, "")),
            Err(IndentRegionError::OutsideRegion { .. })
        ));
        assert!(matches!(
            validate_patch(&text, 1..2, &splice(4, 7, "")),
            Err(IndentRegionError::NonIndentEdit { .. })
        ));
        assert!(matches!(
            validate_patch(&text, 1..2, &splice(3, 4, "\n")),
            Err(IndentRegionError::NonIndentEdit { .. })
        ));
        assert!(matches!(
            plan(
                &text,
                Range { start: 2, end: 1 },
                IndentPolicy {
                    column: 0,
                    tab_width: 4,
                    style: IndentStyle::Spaces
                }
            ),
            Err(IndentRegionError::InvalidRegion { .. })
        ));
        let foreign = Text::from_str("aaaaaaaaaaa");
        let bad = Patch::new(vec![Splice::delete(
            ByteSpan::validate_in(&foreign, 5, 6).unwrap(),
        )])
        .unwrap();
        assert!(matches!(
            validate_patch(&text, 0..3, &bad),
            Err(IndentRegionError::InvalidSpan(_))
        ));
        let huge = Text::from_str("\t 界");
        assert!(plan(
            &huge,
            0..0,
            IndentPolicy {
                column: usize::MAX,
                tab_width: 4,
                style: IndentStyle::Spaces
            }
        )
        .unwrap()
        .splices()
        .is_empty());
        let patch = plan(
            &huge,
            0..1,
            IndentPolicy {
                column: 0,
                tab_width: usize::MAX,
                style: IndentStyle::Spaces,
            },
        )
        .unwrap();
        assert_eq!(patch.apply(&huge).unwrap().as_str(), "界");
    }
}
