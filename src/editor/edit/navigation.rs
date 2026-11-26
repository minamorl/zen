//! Navigation commands.

use std::fmt;

use crate::editor::text::columns::{self, ColumnError, Rounding};
use crate::editor::text::delimiters::DelimiterTable;
use crate::editor::text::lines::LineIndex;
use crate::editor::text::span::ByteSpan;
use crate::editor::text::utf8::{ByteOffset, Text};
use crate::editor::text::word::{self, WordError};
use crate::editor::workspace::catalog::BufferCatalog;
use crate::editor::workspace::selections::{Selection, SelectionError};
use crate::editor::workspace::view::{BufferView, ViewError};

/// Invalid navigation input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NavigationError {
    /// Invalid view or offset.
    View(ViewError),
    /// Invalid selection.
    Selection(SelectionError),
    /// Invalid word range.
    Word(WordError),
    /// Invalid visual conversion.
    Column(ColumnError),
}

impl fmt::Display for NavigationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::View(e) => e.fmt(f),
            Self::Selection(e) => e.fmt(f),
            Self::Word(e) => e.fmt(f),
            Self::Column(e) => e.fmt(f),
        }
    }
}
impl std::error::Error for NavigationError {}

fn checked<'a>(
    catalog: &'a BufferCatalog,
    view: &BufferView,
    point: ByteOffset,
) -> Result<&'a Text, NavigationError> {
    view.visible_text(catalog).map_err(NavigationError::View)?;
    let text = catalog.get(view.buffer()).expect("live view");
    ByteSpan::validate_in(text, point.as_usize(), point.as_usize())
        .map_err(|e| NavigationError::View(ViewError::InvalidSpan(e)))?;
    view.check_target(view.buffer(), ByteSpan::empty_at(point))
        .map_err(NavigationError::View)?;
    Ok(text)
}

/// Word characters are Unicode alphanumeric scalars and underscore. Modal
/// motions additionally treat consecutive non-whitespace punctuation as a word.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WordMotion {
    /// Move to the boundary after the next word.
    ForwardEnd,
    /// Move to the next word start.
    NextStart,
    /// Move to the previous word start.
    PreviousStart,
    /// Move to the last scalar of the current or next word.
    NextEnd,
}

fn class(ch: char) -> u8 {
    if word::is_whitespace(ch) {
        0
    } else if word::is_word_char(ch) {
        1
    } else {
        2
    }
}

/// Navigate within the visible span, clamping at its endpoints.
pub fn move_word(
    catalog: &BufferCatalog,
    view: &BufferView,
    point: ByteOffset,
    motion: WordMotion,
) -> Result<ByteOffset, NavigationError> {
    let text = checked(catalog, view, point)?;
    let span = view.visible();
    if motion == WordMotion::ForwardEnd {
        return Ok(word::next_word_end(text, span, point, word::is_word_char)
            .map_err(NavigationError::Word)?
            .unwrap_or(span.end()));
    }
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut current = 0;
    for (i, ch) in text.as_str()[span.start().as_usize()..span.end().as_usize()].char_indices() {
        let pos = span.start().as_usize() + i;
        let next = class(ch);
        if next != current {
            if current != 0 {
                runs.last_mut().expect("open run").1 = pos;
            }
            if next != 0 {
                runs.push((pos, pos));
            }
            current = next;
        }
    }
    if current != 0 {
        runs.last_mut().expect("open run").1 = span.end().as_usize();
    }
    let p = point.as_usize();
    let result = match motion {
        WordMotion::NextStart => runs
            .iter()
            .map(|r| r.0)
            .find(|&n| n > p)
            .unwrap_or(span.end().as_usize()),
        WordMotion::PreviousStart => runs
            .iter()
            .rev()
            .map(|r| r.0)
            .find(|&n| n < p)
            .unwrap_or(span.start().as_usize()),
        WordMotion::NextEnd => runs
            .iter()
            .filter_map(|&(a, b)| {
                text.as_str()[a..b]
                    .char_indices()
                    .last()
                    .map(|(i, _)| a + i)
            })
            .find(|&n| n > p)
            .unwrap_or(span.end().as_usize()),
        WordMotion::ForwardEnd => unreachable!(),
    };
    Ok(text.validate_offset(result).expect("run boundary"))
}

/// Jump to the matching delimiter, or return none if unmatched or outside the view.
pub fn matching_delimiter(
    catalog: &BufferCatalog,
    view: &BufferView,
    point: ByteOffset,
    table: &DelimiterTable,
) -> Result<Option<ByteOffset>, NavigationError> {
    let text = checked(catalog, view, point)?;
    let visible = view.visible();
    let base = visible.start().as_usize();
    let visible_text = Text::from_str(&text.as_str()[base..visible.end().as_usize()]);
    Ok(table
        .scan(&visible_text)
        .matching(point.as_usize() - base)
        .expect("validated point")
        .map(|p| {
            text.validate_offset(base + p.as_usize())
                .expect("visible delimiter boundary")
        }))
}

/// Traverse the innermost enclosing pair to its opener or its outer end.
/// A pair extending outside the view produces no result.
pub fn traverse_pair(
    catalog: &BufferCatalog,
    view: &BufferView,
    point: ByteOffset,
    table: &DelimiterTable,
    forward: bool,
) -> Result<Option<ByteOffset>, NavigationError> {
    let text = checked(catalog, view, point)?;
    let visible = view.visible();
    let base = visible.start().as_usize();
    let visible_text = Text::from_str(&text.as_str()[base..visible.end().as_usize()]);
    Ok(table
        .scan(&visible_text)
        .enclosing(point.as_usize() - base)
        .expect("validated point")
        .map(|pair| {
            let offset = if forward { pair.outer.end() } else { pair.open };
            text.validate_offset(base + offset.as_usize())
                .expect("visible delimiter boundary")
        }))
}

/// Move vertically to a rounded visual column and return the sticky goal.
/// Target lines and resulting offsets are clipped to the visible range.
pub fn move_visual(
    catalog: &BufferCatalog,
    view: &BufferView,
    point: ByteOffset,
    delta: isize,
    goal: Option<usize>,
    tab_width: usize,
    rounding: Rounding,
) -> Result<(ByteOffset, usize), NavigationError> {
    let text = checked(catalog, view, point)?;
    let index = LineIndex::from_text(text);
    let line_of = |p| {
        index
            .offset_to_line_col(text, p)
            .map(|pos| pos.line)
            .map_err(|e| NavigationError::Column(ColumnError::Line(e)))
    };
    let first_line = line_of(view.visible().start())?;
    let last_line = line_of(view.visible().end())?;
    let point_line = line_of(point)?;
    let goal = match goal {
        Some(goal) => goal,
        None => {
            columns::offset_to_visual(text, &index, point, tab_width)
                .map_err(NavigationError::Column)?
                .column
        }
    };
    let target = point_line
        .saturating_add_signed(delta)
        .clamp(first_line, last_line);
    let result = columns::visual_to_offset(text, &index, target, goal, tab_width, rounding)
        .map_err(NavigationError::Column)?;
    Ok((
        result.clamp(view.visible().start(), view.visible().end()),
        goal,
    ))
}

/// How a motion updates selection endpoints.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionMotion {
    /// Collapse both endpoints to the new head.
    Move,
    /// Keep the anchor and move only the head.
    Extend,
}

/// Apply a word motion to all selections atomically, preserving order and duplicates.
pub fn move_selections(
    catalog: &BufferCatalog,
    view: &BufferView,
    selections: &[Selection],
    motion: WordMotion,
    mode: SelectionMotion,
) -> Result<Vec<Selection>, NavigationError> {
    view.visible_text(catalog).map_err(NavigationError::View)?;
    selections
        .iter()
        .map(|&selection| {
            selection
                .validate(catalog, view)
                .map_err(NavigationError::Selection)?;
            let head = move_word(catalog, view, selection.head(), motion)?;
            let anchor = if mode == SelectionMotion::Extend {
                selection.anchor()
            } else {
                head
            };
            Selection::new(catalog, view, selection.buffer(), anchor, head)
                .map_err(NavigationError::Selection)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use super::*;
    use crate::editor::edit::ids::BufferId;
    use crate::editor::test_support::{assert_law, texts, Rng};
    use crate::editor::text::span::SpanError;

    const ALL_MOTIONS: [WordMotion; 4] = [
        WordMotion::ForwardEnd,
        WordMotion::NextStart,
        WordMotion::NextEnd,
        WordMotion::PreviousStart,
    ];

    fn setup(source: &str) -> (Text, BufferCatalog, BufferView) {
        let text = Text::from_str(source);
        let mut catalog = BufferCatalog::new();
        let id = BufferId::new(1);
        catalog.register(id, text.clone()).unwrap();
        let view = BufferView::full(&catalog, id).unwrap();
        (text, catalog, view)
    }

    // Independent character-transition reference oracle for word navigation.
    fn reference_move_word(
        source: &str,
        visible: Range<usize>,
        point: usize,
        motion: WordMotion,
    ) -> usize {
        #[derive(Clone, Copy, Eq, PartialEq)]
        enum Kind {
            Space,
            Word,
            Punct,
        }
        let classify = |ch: char| {
            if ch.is_whitespace() {
                Kind::Space
            } else if ch.is_alphanumeric() || ch == '_' {
                Kind::Word
            } else {
                Kind::Punct
            }
        };
        let chars: Vec<(usize, char, Kind)> = source[visible.start..visible.end]
            .char_indices()
            .map(|(rel, ch)| (visible.start + rel, ch, classify(ch)))
            .collect();
        match motion {
            WordMotion::ForwardEnd => chars
                .iter()
                .enumerate()
                .filter_map(|(i, &(off, ch, kind))| {
                    let is_end = kind == Kind::Word
                        && (i + 1 == chars.len() || chars[i + 1].2 != Kind::Word);
                    is_end.then_some(off + ch.len_utf8())
                })
                .find(|&end| end > point)
                .unwrap_or(visible.end),
            WordMotion::NextStart => chars
                .iter()
                .enumerate()
                .filter_map(|(i, &(off, _, kind))| {
                    let is_start = kind != Kind::Space && (i == 0 || chars[i - 1].2 != kind);
                    is_start.then_some(off)
                })
                .find(|&start| start > point)
                .unwrap_or(visible.end),
            WordMotion::PreviousStart => chars
                .iter()
                .enumerate()
                .rev()
                .filter_map(|(i, &(off, _, kind))| {
                    let is_start = kind != Kind::Space && (i == 0 || chars[i - 1].2 != kind);
                    is_start.then_some(off)
                })
                .find(|&start| start < point)
                .unwrap_or(visible.start),
            WordMotion::NextEnd => chars
                .iter()
                .enumerate()
                .filter_map(|(i, &(off, _, kind))| {
                    let is_last =
                        kind != Kind::Space && (i + 1 == chars.len() || chars[i + 1].2 != kind);
                    is_last.then_some(off)
                })
                .find(|&last| last > point)
                .unwrap_or(visible.end),
        }
    }

    // Independent stack-based reference matcher over only the half-open visible slice.
    fn reference_delimiter_pairs(
        source: &str,
        visible: Range<usize>,
        defs: &[(char, char)],
    ) -> Vec<(usize, usize, usize)> {
        let mut stack: Vec<(usize, char)> = Vec::new();
        let mut pairs = Vec::new();
        for (rel, ch) in source[visible.start..visible.end].char_indices() {
            let abs = visible.start + rel;
            if let Some(&(_, expected_close)) = defs.iter().find(|&&(open, _)| open == ch) {
                stack.push((abs, expected_close));
            } else if defs.iter().any(|&(_, close)| close == ch)
                && stack.last().is_some_and(|&(_, expected)| expected == ch)
            {
                let (open, _) = stack.pop().unwrap();
                pairs.push((open, abs, abs + ch.len_utf8()));
            }
        }
        pairs.sort_by_key(|&(open, _, _)| open);
        pairs
    }

    fn reference_matching(pairs: &[(usize, usize, usize)], point: usize) -> Option<usize> {
        pairs.iter().find_map(|&(open, close, _)| {
            if open == point {
                Some(close)
            } else if close == point {
                Some(open)
            } else {
                None
            }
        })
    }

    fn reference_traverse(
        pairs: &[(usize, usize, usize)],
        point: usize,
        forward: bool,
    ) -> Option<usize> {
        pairs
            .iter()
            .filter(|&&(open, _, outer_end)| open <= point && point < outer_end)
            .max_by_key(|&&(open, _, _)| open)
            .map(|&(open, _, outer_end)| if forward { outer_end } else { open })
    }

    // Independent line-and-tab-stop reference oracle for vertical visual movement.
    fn reference_move_visual(
        source: &str,
        visible: Range<usize>,
        point: usize,
        delta: isize,
        requested_goal: Option<usize>,
        tab_width: usize,
        rounding: Rounding,
    ) -> (usize, usize) {
        let mut lines = Vec::new();
        let mut start = 0;
        for content in source.split('\n') {
            let end = start + content.len();
            lines.push((start, end));
            start = end + 1;
        }
        let line_of = |off: usize| {
            lines
                .iter()
                .position(|&(s, e)| s <= off && off <= e)
                .unwrap()
        };
        let first_line = line_of(visible.start);
        let last_line = line_of(visible.end);
        let point_line = line_of(point);
        let goal = requested_goal.unwrap_or_else(|| {
            let (ls, _) = lines[point_line];
            let mut col = 0;
            for ch in source[ls..point].chars() {
                col += 1;
                if ch == '\t' {
                    while col % tab_width != 0 {
                        col += 1;
                    }
                }
            }
            col
        });
        let target_line = point_line
            .saturating_add_signed(delta)
            .clamp(first_line, last_line);
        let (ts, te) = lines[target_line];
        let mut col = 0;
        let mut target_off = te;
        for (rel, ch) in source[ts..te].char_indices() {
            if col == goal {
                target_off = ts + rel;
                break;
            }
            let mut next_col = col + 1;
            if ch == '\t' {
                while next_col % tab_width != 0 {
                    next_col += 1;
                }
            }
            if goal < next_col {
                target_off = match rounding {
                    Rounding::Floor => ts + rel,
                    Rounding::Ceil => ts + rel + ch.len_utf8(),
                };
                break;
            }
            col = next_col;
        }
        (target_off.clamp(visible.start, visible.end), goal)
    }

    #[test]
    fn examples_and_pair_traversal() {
        let source = "a_b!! 界 (x[y]) z";
        let (text, catalog, view) = setup(source);
        let p = |n| text.validate_offset(n).unwrap();
        assert_eq!(
            move_word(&catalog, &view, p(0), WordMotion::ForwardEnd).unwrap(),
            p(3)
        );
        assert_eq!(
            move_word(&catalog, &view, p(0), WordMotion::NextStart).unwrap(),
            p(3)
        );
        assert_eq!(
            move_word(&catalog, &view, p(3), WordMotion::NextEnd).unwrap(),
            p(4)
        );
        assert_eq!(
            move_word(&catalog, &view, p(5), WordMotion::PreviousStart).unwrap(),
            p(3)
        );
        assert_eq!(
            move_word(&catalog, &view, p(3), WordMotion::ForwardEnd).unwrap(),
            p(9)
        );
        assert_eq!(
            move_word(&catalog, &view, p(4), WordMotion::NextEnd).unwrap(),
            p(6)
        );

        let table = DelimiterTable::new(&[('(', ')'), ('[', ']')]).unwrap();
        // '(' at 10 matches ')' at 15 (outer 10..16); '[' at 12 matches ']' at 14 (outer 12..15).
        assert_eq!(
            matching_delimiter(&catalog, &view, p(10), &table).unwrap(),
            Some(p(15))
        );
        assert_eq!(
            matching_delimiter(&catalog, &view, p(15), &table).unwrap(),
            Some(p(10))
        );
        assert_eq!(
            matching_delimiter(&catalog, &view, p(12), &table).unwrap(),
            Some(p(14))
        );
        assert_eq!(
            matching_delimiter(&catalog, &view, p(14), &table).unwrap(),
            Some(p(12))
        );
        assert_eq!(
            matching_delimiter(&catalog, &view, p(0), &table).unwrap(),
            None
        );
        assert_eq!(
            matching_delimiter(&catalog, &view, p(11), &table).unwrap(),
            None
        );
        assert_eq!(
            matching_delimiter(&catalog, &view, p(13), &table).unwrap(),
            None
        );

        for (offset, expected_open, expected_end) in [
            (10, Some(10), Some(16)),
            (11, Some(10), Some(16)),
            (12, Some(12), Some(15)),
            (13, Some(12), Some(15)),
            (14, Some(12), Some(15)),
            (15, Some(10), Some(16)),
            (16, None, None),
            (0, None, None),
        ] {
            assert_eq!(
                traverse_pair(&catalog, &view, p(offset), &table, false).unwrap(),
                expected_open.map(p)
            );
            assert_eq!(
                traverse_pair(&catalog, &view, p(offset), &table, true).unwrap(),
                expected_end.map(p)
            );
        }
    }

    #[test]
    fn narrowed_view_delimiters_exclude_outside_text() {
        let table = DelimiterTable::new(&[('(', ')'), ('[', ']'), ('{', '}')]).unwrap();

        // Closing delimiter starting right at visible.end() must never match.
        let (text, catalog, full) = setup("(a)");
        let p = |n| text.validate_offset(n).unwrap();
        let half = full
            .narrow(&catalog, ByteSpan::new(p(0), p(2)).unwrap())
            .unwrap();
        for off in [0, 1, 2] {
            assert_eq!(
                matching_delimiter(&catalog, &half, p(off), &table).unwrap(),
                None
            );
            assert_eq!(
                traverse_pair(&catalog, &half, p(off), &table, true).unwrap(),
                None
            );
            assert_eq!(
                traverse_pair(&catalog, &half, p(off), &table, false).unwrap(),
                None
            );
        }

        // Opening delimiter before visible.start() must never match.
        let right = full
            .narrow(&catalog, ByteSpan::new(p(1), p(3)).unwrap())
            .unwrap();
        for off in [1, 2, 3] {
            assert_eq!(
                matching_delimiter(&catalog, &right, p(off), &table).unwrap(),
                None
            );
            assert_eq!(
                traverse_pair(&catalog, &right, p(off), &table, true).unwrap(),
                None
            );
            assert_eq!(
                traverse_pair(&catalog, &right, p(off), &table, false).unwrap(),
                None
            );
        }

        // Full view includes both delimiters; interior point traverses both ways.
        assert_eq!(
            matching_delimiter(&catalog, &full, p(0), &table).unwrap(),
            Some(p(2))
        );
        assert_eq!(
            matching_delimiter(&catalog, &full, p(2), &table).unwrap(),
            Some(p(0))
        );
        assert_eq!(
            matching_delimiter(&catalog, &full, p(3), &table).unwrap(),
            None
        );
        for off in [0, 1, 2] {
            assert_eq!(
                traverse_pair(&catalog, &full, p(off), &table, false).unwrap(),
                Some(p(0))
            );
            assert_eq!(
                traverse_pair(&catalog, &full, p(off), &table, true).unwrap(),
                Some(p(3))
            );
        }
        assert_eq!(
            traverse_pair(&catalog, &full, p(3), &table, true).unwrap(),
            None
        );
        assert_eq!(
            traverse_pair(&catalog, &full, p(3), &table, false).unwrap(),
            None
        );

        // Nested pairs with narrowed views cutting off outer/middle closers.
        let (nested, nested_catalog, nested_full) = setup("(x[y{界}])");
        let np = |n| nested.validate_offset(n).unwrap();
        // View 2..10 is "[y{界}]"; ')' at 10 is outside.
        let mid_view = nested_full
            .narrow(&nested_catalog, ByteSpan::new(np(2), np(10)).unwrap())
            .unwrap();
        assert_eq!(
            matching_delimiter(&nested_catalog, &mid_view, np(2), &table).unwrap(),
            Some(np(9))
        );
        assert_eq!(
            matching_delimiter(&nested_catalog, &mid_view, np(10), &table).unwrap(),
            None
        );
        assert_eq!(
            traverse_pair(&nested_catalog, &mid_view, np(3), &table, false).unwrap(),
            Some(np(2))
        );
        assert_eq!(
            traverse_pair(&nested_catalog, &mid_view, np(3), &table, true).unwrap(),
            Some(np(10))
        );
        assert_eq!(
            traverse_pair(&nested_catalog, &mid_view, np(5), &table, false).unwrap(),
            Some(np(4))
        );
        assert_eq!(
            traverse_pair(&nested_catalog, &mid_view, np(5), &table, true).unwrap(),
            Some(np(9))
        );
        assert_eq!(
            traverse_pair(&nested_catalog, &mid_view, np(10), &table, true).unwrap(),
            None
        );
        assert_eq!(
            traverse_pair(&nested_catalog, &mid_view, np(10), &table, false).unwrap(),
            None
        );

        // View 2..9 is "[y{界}"; ']' at 9 is outside, only {界} remains matched.
        let inner_only = nested_full
            .narrow(&nested_catalog, ByteSpan::new(np(2), np(9)).unwrap())
            .unwrap();
        assert_eq!(
            matching_delimiter(&nested_catalog, &inner_only, np(2), &table).unwrap(),
            None
        );
        assert_eq!(
            matching_delimiter(&nested_catalog, &inner_only, np(9), &table).unwrap(),
            None
        );
        assert_eq!(
            matching_delimiter(&nested_catalog, &inner_only, np(4), &table).unwrap(),
            Some(np(8))
        );
        assert_eq!(
            traverse_pair(&nested_catalog, &inner_only, np(3), &table, true).unwrap(),
            None
        );
        assert_eq!(
            traverse_pair(&nested_catalog, &inner_only, np(5), &table, true).unwrap(),
            Some(np(9))
        );
        assert_eq!(
            traverse_pair(&nested_catalog, &inner_only, np(5), &table, false).unwrap(),
            Some(np(4))
        );

        // Multi-byte delimiters cut right before closing delimiter.
        let mb_table = DelimiterTable::new(&[('「', '」')]).unwrap();
        let (mb, mb_catalog, mb_full) = setup("界「中」界");
        let mp = |n| mb.validate_offset(n).unwrap();
        let mb_cut = mb_full
            .narrow(&mb_catalog, ByteSpan::new(mp(3), mp(9)).unwrap())
            .unwrap();
        for off in [3, 6, 9] {
            assert_eq!(
                matching_delimiter(&mb_catalog, &mb_cut, mp(off), &mb_table).unwrap(),
                None
            );
            assert_eq!(
                traverse_pair(&mb_catalog, &mb_cut, mp(off), &mb_table, true).unwrap(),
                None
            );
            assert_eq!(
                traverse_pair(&mb_catalog, &mb_cut, mp(off), &mb_table, false).unwrap(),
                None
            );
        }
        let mb_exact = mb_full
            .narrow(&mb_catalog, ByteSpan::new(mp(3), mp(12)).unwrap())
            .unwrap();
        assert_eq!(
            matching_delimiter(&mb_catalog, &mb_exact, mp(3), &mb_table).unwrap(),
            Some(mp(9))
        );
        assert_eq!(
            matching_delimiter(&mb_catalog, &mb_exact, mp(9), &mb_table).unwrap(),
            Some(mp(3))
        );
        assert_eq!(
            traverse_pair(&mb_catalog, &mb_exact, mp(6), &mb_table, false).unwrap(),
            Some(mp(3))
        );
        assert_eq!(
            traverse_pair(&mb_catalog, &mb_exact, mp(6), &mb_table, true).unwrap(),
            Some(mp(12))
        );
        assert_eq!(
            traverse_pair(&mb_catalog, &mb_exact, mp(12), &mb_table, true).unwrap(),
            None
        );
    }

    #[test]
    fn randomized_delimiter_laws_against_reference_matcher() {
        let defs = [('(', ')'), ('[', ']'), ('{', '}')];
        let table = DelimiterTable::new(&defs).unwrap();
        let alphabet = ['(', ')', '[', ']', '{', '}', 'a', '界', '😀', ' '];
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| {
                let mut corpus: Vec<String> = [
                    "",
                    "(a)",
                    "([)]",
                    "({[]})",
                    "((a)",
                    "(a))",
                    "][}{)(",
                    "([a)]",
                    "(x[y{界}])",
                    "{([😀])}",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect();
                for _ in 0..48 {
                    let mut s = String::new();
                    for _ in 0..rng.gen_range(0..24) {
                        s.push(*rng.choose(&alphabet).unwrap());
                    }
                    corpus.push(s);
                }
                corpus
            },
            |corpus| {
                let mut rng = Rng::new(73);
                for source in corpus {
                    let (text, catalog, full) = setup(source);
                    let points = text.char_boundaries();
                    let mut views = vec![(0usize, points.len() - 1)];
                    for _ in 0..12 {
                        let a = rng.gen_range(0..points.len());
                        let b = rng.gen_range(0..points.len());
                        views.push((a.min(b), a.max(b)));
                    }
                    for (lo, hi) in views {
                        let span = ByteSpan::new(points[lo], points[hi]).unwrap();
                        let view = full.narrow(&catalog, span).unwrap();
                        let range = points[lo].as_usize()..points[hi].as_usize();
                        let expected_pairs = reference_delimiter_pairs(source, range, &defs);
                        for &point in &points[lo..hi + 1] {
                            let expected_match =
                                reference_matching(&expected_pairs, point.as_usize());
                            let actual_match =
                                matching_delimiter(&catalog, &view, point, &table).unwrap();
                            assert_eq!(actual_match.map(ByteOffset::as_usize), expected_match);
                            if let Some(other) = actual_match {
                                assert!(other >= view.visible().start());
                                assert!(other < view.visible().end());
                                assert_eq!(
                                    matching_delimiter(&catalog, &view, other, &table).unwrap(),
                                    Some(point)
                                );
                            }
                            for forward in [false, true] {
                                let expected_trav =
                                    reference_traverse(&expected_pairs, point.as_usize(), forward);
                                let actual_trav =
                                    traverse_pair(&catalog, &view, point, &table, forward).unwrap();
                                assert_eq!(actual_trav.map(ByteOffset::as_usize), expected_trav);
                            }
                        }
                    }
                }
            },
        );
    }

    #[test]
    fn randomized_word_navigation_against_reference_oracle() {
        let fragments = [
            "a", "Z", "9", "_", "!", ".", "-", " ", "\t", "\n", "\u{3000}", "界", "α", "😀",
            "\u{0301}", "。",
        ];
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| {
                let mut corpus: Vec<String> = [
                    "",
                    "   \t\u{3000}\n",
                    "!!!...",
                    "abc_123",
                    "a_b!! 界 (x[y]) z",
                    "e\u{0301}界😀!! \u{3000}a_9--β",
                    "界中😀😀  x_y",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect();
                for _ in 0..48 {
                    let mut s = String::new();
                    for _ in 0..rng.gen_range(0..18) {
                        s.push_str(rng.choose(&fragments).unwrap());
                    }
                    corpus.push(s);
                }
                corpus
            },
            |corpus| {
                let mut rng = Rng::new(31);
                for source in corpus {
                    let (text, catalog, full) = setup(source);
                    let points = text.char_boundaries();
                    let mut views = vec![(0usize, points.len() - 1)];
                    for _ in 0..10 {
                        let a = rng.gen_range(0..points.len());
                        let b = rng.gen_range(0..points.len());
                        views.push((a.min(b), a.max(b)));
                    }
                    for (lo, hi) in views {
                        let span = ByteSpan::new(points[lo], points[hi]).unwrap();
                        let view = full.narrow(&catalog, span).unwrap();
                        let vis_range = points[lo].as_usize()..points[hi].as_usize();
                        for motion in ALL_MOTIONS {
                            for &point in &points[lo..hi + 1] {
                                let expected = reference_move_word(
                                    source,
                                    vis_range.clone(),
                                    point.as_usize(),
                                    motion,
                                );
                                let actual = move_word(&catalog, &view, point, motion).unwrap();
                                assert_eq!(actual.as_usize(), expected);
                            }
                            let bound = if motion == WordMotion::PreviousStart {
                                points[lo]
                            } else {
                                points[hi]
                            };
                            let mut pos = points[rng.gen_range(lo..hi + 1)];
                            while pos != bound {
                                let next = move_word(&catalog, &view, pos, motion).unwrap();
                                if motion == WordMotion::PreviousStart {
                                    assert!(next < pos);
                                } else {
                                    assert!(next > pos);
                                }
                                pos = next;
                            }
                            assert_eq!(move_word(&catalog, &view, bound, motion).unwrap(), bound);
                        }
                    }
                }
            },
        );
    }

    #[test]
    fn move_visual_sticky_goal_rounding_clamping_and_overflow() {
        // Explicit offset assertions on short and empty lines with sticky goal preservation.
        let (text, catalog, view) = setup("a\t界\nx\na\t界");
        let p = |n| text.validate_offset(n).unwrap();
        let (short, goal) =
            move_visual(&catalog, &view, p(2), 1, None, 4, Rounding::Floor).unwrap();
        assert_eq!((short, goal), (p(7), 4));
        let (restored, goal2) =
            move_visual(&catalog, &view, short, 1, Some(goal), 4, Rounding::Floor).unwrap();
        assert_eq!((restored, goal2), (p(10), 4));
        // Sticky goal larger than every line is preserved verbatim while clamping offset to line end.
        assert_eq!(
            move_visual(&catalog, &view, p(0), 1, Some(99), 4, Rounding::Floor).unwrap(),
            (p(7), 99)
        );
        // Target line clamps at document top and bottom even for extreme deltas.
        assert_eq!(
            move_visual(
                &catalog,
                &view,
                p(6),
                isize::MIN,
                Some(0),
                4,
                Rounding::Floor
            )
            .unwrap(),
            (p(0), 0)
        );
        assert_eq!(
            move_visual(
                &catalog,
                &view,
                p(0),
                isize::MAX,
                Some(0),
                4,
                Rounding::Floor
            )
            .unwrap(),
            (p(8), 0)
        );

        // Empty short line preserves sticky goal and lands on the empty line's exact offset.
        let (empty_mid, em_catalog, em_view) = setup("abcd\n\nefgh");
        let ep = |n| empty_mid.validate_offset(n).unwrap();
        let (at_empty, em_goal) =
            move_visual(&em_catalog, &em_view, ep(3), 1, None, 4, Rounding::Floor).unwrap();
        assert_eq!((at_empty, em_goal), (ep(5), 3));
        assert_eq!(
            move_visual(
                &em_catalog,
                &em_view,
                at_empty,
                1,
                Some(em_goal),
                4,
                Rounding::Floor
            )
            .unwrap(),
            (ep(9), 3)
        );

        // Mid-tab rounding distinction: Rounding::Floor vs Rounding::Ceil at goal 2 with tab_width 4.
        let (tab_text, tab_catalog, tab_view) = setup("ab\n\tZ");
        let tp = |n| tab_text.validate_offset(n).unwrap();
        assert_eq!(
            move_visual(&tab_catalog, &tab_view, tp(2), 1, None, 4, Rounding::Floor).unwrap(),
            (tp(3), 2)
        );
        assert_eq!(
            move_visual(&tab_catalog, &tab_view, tp(2), 1, None, 4, Rounding::Ceil).unwrap(),
            (tp(4), 2)
        );
        assert_eq!(
            move_visual(
                &tab_catalog,
                &tab_view,
                tp(0),
                1,
                Some(2),
                4,
                Rounding::Floor
            )
            .unwrap(),
            (tp(3), 2)
        );
        assert_eq!(
            move_visual(
                &tab_catalog,
                &tab_view,
                tp(0),
                1,
                Some(2),
                4,
                Rounding::Ceil
            )
            .unwrap(),
            (tp(4), 2)
        );

        // Mid-tab after a prefix scalar: cols 1..4 are covered by '\t' at offset 5.
        let (pref_text, pref_catalog, pref_view) = setup("abc\na\tZ");
        let pp = |n| pref_text.validate_offset(n).unwrap();
        for mid_goal in [2, 3] {
            assert_eq!(
                move_visual(
                    &pref_catalog,
                    &pref_view,
                    pp(0),
                    1,
                    Some(mid_goal),
                    4,
                    Rounding::Floor
                )
                .unwrap(),
                (pp(5), mid_goal)
            );
            assert_eq!(
                move_visual(
                    &pref_catalog,
                    &pref_view,
                    pp(0),
                    1,
                    Some(mid_goal),
                    4,
                    Rounding::Ceil
                )
                .unwrap(),
                (pp(6), mid_goal)
            );
        }
        for (exact_goal, expected) in [(1, pp(5)), (4, pp(6))] {
            for rounding in [Rounding::Floor, Rounding::Ceil] {
                assert_eq!(
                    move_visual(
                        &pref_catalog,
                        &pref_view,
                        pp(0),
                        1,
                        Some(exact_goal),
                        4,
                        rounding
                    )
                    .unwrap(),
                    (expected, exact_goal)
                );
            }
        }

        // Narrowed view clamps both target lines and resulting byte offsets.
        let (narrow_text, narrow_catalog, narrow_full) = setup("012345\nabcdef\nABCDEF\nzzzz");
        let np = |n| narrow_text.validate_offset(n).unwrap();
        // Visible slice 9..18 is "cdef\nABCD" (lines 1..=2, clipped at start of line 1 and end of line 2).
        let narrow_view = narrow_full
            .narrow(&narrow_catalog, ByteSpan::new(np(9), np(18)).unwrap())
            .unwrap();
        assert_eq!(
            move_visual(
                &narrow_catalog,
                &narrow_view,
                np(15),
                -5,
                Some(0),
                4,
                Rounding::Floor
            )
            .unwrap(),
            (np(9), 0)
        );
        assert_eq!(
            move_visual(
                &narrow_catalog,
                &narrow_view,
                np(9),
                5,
                Some(5),
                4,
                Rounding::Floor
            )
            .unwrap(),
            (np(18), 5)
        );

        // Regression: do not compute visual columns for view endpoints or point when goal is Some(_).
        let (huge_text, huge_catalog, huge_view) = setup("a\n\tX");
        let hp = |n| huge_text.validate_offset(n).unwrap();
        assert_eq!(huge_view.visible(), ByteSpan::new(hp(0), hp(4)).unwrap());
        for rounding in [Rounding::Floor, Rounding::Ceil] {
            assert_eq!(
                move_visual(
                    &huge_catalog,
                    &huge_view,
                    hp(0),
                    0,
                    Some(0),
                    usize::MAX,
                    rounding
                )
                .unwrap(),
                (hp(0), 0)
            );
            assert_eq!(
                move_visual(
                    &huge_catalog,
                    &huge_view,
                    hp(4),
                    -1,
                    Some(0),
                    usize::MAX,
                    rounding
                )
                .unwrap(),
                (hp(0), 0)
            );
        }
        assert_eq!(
            move_visual(
                &huge_catalog,
                &huge_view,
                hp(0),
                0,
                None,
                usize::MAX,
                Rounding::Floor
            )
            .unwrap(),
            (hp(0), 0)
        );
        // Genuine overflow when goal is None at an overflowing point.
        assert_eq!(
            move_visual(
                &huge_catalog,
                &huge_view,
                hp(4),
                0,
                None,
                usize::MAX,
                Rounding::Floor
            ),
            Err(NavigationError::Column(ColumnError::ColumnOverflow))
        );
        // Narrowed view whose start lies after an overflowing tab must also succeed.
        let (start_huge, sh_catalog, sh_full) = setup("\tXa\nb");
        let shp = |n| start_huge.validate_offset(n).unwrap();
        let sh_view = sh_full
            .narrow(&sh_catalog, ByteSpan::new(shp(2), shp(5)).unwrap())
            .unwrap();
        assert_eq!(
            move_visual(
                &sh_catalog,
                &sh_view,
                shp(4),
                0,
                Some(0),
                usize::MAX,
                Rounding::Floor
            )
            .unwrap(),
            (shp(4), 0)
        );
        assert_eq!(
            move_visual(
                &sh_catalog,
                &sh_view,
                shp(4),
                0,
                None,
                usize::MAX,
                Rounding::Floor
            )
            .unwrap(),
            (shp(4), 0)
        );

        // Zero tab width fails for both None and Some goals.
        for goal in [None, Some(0)] {
            assert_eq!(
                move_visual(&catalog, &view, p(0), 1, goal, 0, Rounding::Floor),
                Err(NavigationError::Column(ColumnError::ZeroTabWidth))
            );
        }
    }

    #[test]
    fn randomized_move_visual_against_reference_oracle() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            let mut rng = Rng::new(19);
            for base in corpus {
                let source = format!("{base}\n\t界\t{base}\n\n{base}");
                let (text, catalog, full) = setup(&source);
                let points = text.char_boundaries();
                for _ in 0..24 {
                    let a = rng.gen_range(0..points.len());
                    let b = rng.gen_range(0..points.len());
                    let lo = a.min(b);
                    let hi = a.max(b);
                    let view = full
                        .narrow(&catalog, ByteSpan::new(points[lo], points[hi]).unwrap())
                        .unwrap();
                    let point = points[rng.gen_range(lo..hi + 1)];
                    let delta = rng.gen_range(0..21) as isize - 10;
                    let tab_width = rng.gen_range(1..9);
                    let requested_goal = if rng.gen_bool() {
                        Some(rng.gen_range(0..30))
                    } else {
                        None
                    };
                    for rounding in [Rounding::Floor, Rounding::Ceil] {
                        let (result, goal) = move_visual(
                            &catalog,
                            &view,
                            point,
                            delta,
                            requested_goal,
                            tab_width,
                            rounding,
                        )
                        .unwrap();
                        let (expected_off, expected_goal) = reference_move_visual(
                            &source,
                            points[lo].as_usize()..points[hi].as_usize(),
                            point.as_usize(),
                            delta,
                            requested_goal,
                            tab_width,
                            rounding,
                        );
                        assert_eq!((result.as_usize(), goal), (expected_off, expected_goal));
                        if let Some(req) = requested_goal {
                            assert_eq!(goal, req);
                        }
                        assert!(view.contains_offset(result));
                        assert!(text.as_str().is_char_boundary(result.as_usize()));
                    }
                }
            }
        });
    }

    #[test]
    fn move_selections_motions_empty_and_rejections() {
        let (text, mut catalog, full) = setup("alpha!! beta 界 gamma");
        let p = |n| text.validate_offset(n).unwrap();
        let id = full.buffer();
        let narrow = full
            .narrow(&catalog, ByteSpan::new(p(2), p(18)).unwrap())
            .unwrap();

        // Empty selection slice returns empty output on valid views.
        for view in [full, narrow] {
            for motion in ALL_MOTIONS {
                for mode in [SelectionMotion::Move, SelectionMotion::Extend] {
                    assert_eq!(
                        move_selections(&catalog, &view, &[], motion, mode).unwrap(),
                        Vec::new()
                    );
                }
            }
        }

        // Forward, backward, empty, and duplicate selections across all motions and modes.
        let forward = Selection::new(&catalog, &full, id, p(0), p(5)).unwrap();
        let backward = Selection::new(&catalog, &full, id, p(12), p(5)).unwrap();
        let empty = Selection::new(&catalog, &full, id, p(8), p(8)).unwrap();
        let batch = [forward, backward, forward, empty];
        for motion in ALL_MOTIONS {
            for mode in [SelectionMotion::Move, SelectionMotion::Extend] {
                let moved = move_selections(&catalog, &full, &batch, motion, mode).unwrap();
                assert_eq!(moved.len(), batch.len());
                assert_eq!(moved[0], moved[2]);
                for (before, after) in batch.iter().zip(&moved) {
                    let expected_head = move_word(&catalog, &full, before.head(), motion).unwrap();
                    let expected_anchor = if mode == SelectionMotion::Extend {
                        before.anchor()
                    } else {
                        expected_head
                    };
                    assert_eq!(after.buffer(), before.buffer());
                    assert_eq!(after.head(), expected_head);
                    assert_eq!(after.anchor(), expected_anchor);
                }
            }
        }

        // SelectionError rejections against a valid view + mixed [valid, invalid] atomicity.
        let valid_in_narrow = Selection::new(&catalog, &narrow, id, p(5), p(8)).unwrap();

        // 1. BufferMismatch against a valid view.
        let other_id = BufferId::new(2);
        let other_text = Text::from_str("alpha!! beta 界 gamma");
        catalog.register(other_id, other_text).unwrap();
        let other_view = BufferView::full(&catalog, other_id).unwrap();
        let foreign_sel = Selection::new(&catalog, &other_view, other_id, p(2), p(8)).unwrap();
        for batch in [
            vec![foreign_sel],
            vec![valid_in_narrow, foreign_sel],
            vec![foreign_sel, valid_in_narrow],
        ] {
            let snapshot = batch.clone();
            assert_eq!(
                move_selections(
                    &catalog,
                    &narrow,
                    &batch,
                    WordMotion::NextStart,
                    SelectionMotion::Move
                ),
                Err(NavigationError::Selection(SelectionError::BufferMismatch {
                    expected: id,
                    actual: other_id,
                }))
            );
            assert_eq!(batch, snapshot);
        }

        // 2. SpanOutsideVisible against a valid narrowed view (forward and backward).
        let outside_left = Selection::new(&catalog, &full, id, p(0), p(5)).unwrap();
        let outside_right_back = Selection::new(&catalog, &full, id, p(20), p(12)).unwrap();
        for bad in [outside_left, outside_right_back] {
            for batch in [vec![bad], vec![valid_in_narrow, bad]] {
                let snapshot = batch.clone();
                assert_eq!(
                    move_selections(
                        &catalog,
                        &narrow,
                        &batch,
                        WordMotion::NextStart,
                        SelectionMotion::Extend
                    ),
                    Err(NavigationError::Selection(
                        SelectionError::SpanOutsideVisible {
                            requested: bad.range(),
                            visible: narrow.visible(),
                        }
                    ))
                );
                assert_eq!(batch, snapshot);
            }
        }

        // 3. Non-boundary endpoints (head and anchor) against a valid view.
        let mb_id = BufferId::new(3);
        let ascii6 = Text::from_str("abcdef");
        let ap = |n| ascii6.validate_offset(n).unwrap();
        catalog.register(mb_id, ascii6.clone()).unwrap();
        let mb_view = BufferView::full(&catalog, mb_id).unwrap();
        let valid_mb_sel = Selection::new(&catalog, &mb_view, mb_id, ap(0), ap(3)).unwrap();
        let bad_head = Selection::new(&catalog, &mb_view, mb_id, ap(0), ap(1)).unwrap();
        let bad_anchor = Selection::new(&catalog, &mb_view, mb_id, ap(1), ap(0)).unwrap();
        // Replace with two 3-byte scalars: view 0..6 stays valid, offset 1 is mid-scalar.
        catalog.update(mb_id, Text::from_str("界界")).unwrap();
        assert_eq!(mb_view.visible_text(&catalog), Ok("界界"));
        for bad in [bad_head, bad_anchor] {
            for batch in [vec![bad], vec![valid_mb_sel, bad]] {
                let snapshot = batch.clone();
                assert_eq!(
                    move_selections(
                        &catalog,
                        &mb_view,
                        &batch,
                        WordMotion::NextStart,
                        SelectionMotion::Move
                    ),
                    Err(NavigationError::Selection(SelectionError::InvalidSpan(
                        SpanError::NotCharBoundary { offset: 1 }
                    )))
                );
                assert_eq!(batch, snapshot);
            }
        }

        // 4. Out-of-bounds endpoints against a valid shrunk view (and against a stale view).
        let oob_forward = Selection::new(&catalog, &mb_view, mb_id, ap(0), ap(6)).unwrap();
        let oob_backward = Selection::new(&catalog, &mb_view, mb_id, ap(6), ap(0)).unwrap();
        catalog.update(mb_id, Text::from_str("ab")).unwrap();
        let shrunk_view = BufferView::full(&catalog, mb_id).unwrap();
        let valid_shrunk = Selection::new(&catalog, &shrunk_view, mb_id, ap(0), ap(2)).unwrap();
        for bad in [oob_forward, oob_backward] {
            for batch in [vec![bad], vec![valid_shrunk, bad]] {
                let snapshot = batch.clone();
                assert_eq!(
                    move_selections(
                        &catalog,
                        &shrunk_view,
                        &batch,
                        WordMotion::NextStart,
                        SelectionMotion::Move
                    ),
                    Err(NavigationError::Selection(
                        SelectionError::SpanOutsideVisible {
                            requested: bad.range(),
                            visible: shrunk_view.visible(),
                        }
                    ))
                );
                assert_eq!(batch, snapshot);
            }
        }
        assert_eq!(
            move_selections(
                &catalog,
                &mb_view,
                &[oob_forward],
                WordMotion::NextStart,
                SelectionMotion::Move
            ),
            Err(NavigationError::View(ViewError::InvalidSpan(
                SpanError::OutOfBounds { offset: 6, len: 2 }
            )))
        );
    }

    #[test]
    fn randomized_move_selections_matches_direct_move_word() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            let mut rng = Rng::new(53);
            for base in corpus {
                let source = format!("{base} a_b!! 界");
                let (text, catalog, full) = setup(&source);
                let points = text.char_boundaries();
                for _ in 0..16 {
                    let a = rng.gen_range(0..points.len());
                    let b = rng.gen_range(0..points.len());
                    let lo = a.min(b);
                    let hi = a.max(b);
                    let view = full
                        .narrow(&catalog, ByteSpan::new(points[lo], points[hi]).unwrap())
                        .unwrap();
                    let p1 = points[rng.gen_range(lo..hi + 1)];
                    let p2 = points[rng.gen_range(lo..hi + 1)];
                    let fwd =
                        Selection::new(&catalog, &view, view.buffer(), p1.min(p2), p1.max(p2))
                            .unwrap();
                    let bwd =
                        Selection::new(&catalog, &view, view.buffer(), p1.max(p2), p1.min(p2))
                            .unwrap();
                    let input = [fwd, bwd, fwd];
                    for motion in ALL_MOTIONS {
                        for mode in [SelectionMotion::Move, SelectionMotion::Extend] {
                            let result =
                                move_selections(&catalog, &view, &input, motion, mode).unwrap();
                            assert_eq!(result.len(), input.len());
                            assert_eq!(result[0], result[2]);
                            for (before, after) in input.iter().zip(&result) {
                                let expected_head =
                                    move_word(&catalog, &view, before.head(), motion).unwrap();
                                let expected_anchor = if mode == SelectionMotion::Extend {
                                    before.anchor()
                                } else {
                                    expected_head
                                };
                                assert_eq!(after.head(), expected_head);
                                assert_eq!(after.anchor(), expected_anchor);
                                after.validate(&catalog, &view).unwrap();
                            }
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn error_paths_across_all_navigation_commands() {
        let table = DelimiterTable::new(&[('(', ')'), ('[', ']')]).unwrap();
        let assert_all_err = |catalog: &BufferCatalog,
                              view: &BufferView,
                              point: ByteOffset,
                              expected: NavigationError| {
            for motion in ALL_MOTIONS {
                assert_eq!(
                    move_word(catalog, view, point, motion),
                    Err(expected.clone())
                );
            }
            assert_eq!(
                matching_delimiter(catalog, view, point, &table),
                Err(expected.clone())
            );
            for forward in [false, true] {
                assert_eq!(
                    traverse_pair(catalog, view, point, &table, forward),
                    Err(expected.clone())
                );
            }
            for goal in [None, Some(0)] {
                assert_eq!(
                    move_visual(catalog, view, point, 0, goal, 4, Rounding::Floor),
                    Err(expected.clone())
                );
            }
        };

        // 1. OutOfBounds across all four navigation functions.
        let (short_text, short_catalog, short_view) = setup("abc");
        let foreign = Text::from_str("abcdefgh");
        let oob = foreign.validate_offset(5).unwrap();
        assert_all_err(
            &short_catalog,
            &short_view,
            oob,
            NavigationError::View(ViewError::InvalidSpan(SpanError::OutOfBounds {
                offset: 5,
                len: short_text.len_bytes(),
            })),
        );

        // 2. NotCharBoundary across all four navigation functions.
        let (_mb_text, mb_catalog, mb_view) = setup("界a");
        let mid = foreign.validate_offset(1).unwrap();
        assert_all_err(
            &mb_catalog,
            &mb_view,
            mid,
            NavigationError::View(ViewError::InvalidSpan(SpanError::NotCharBoundary {
                offset: 1,
            })),
        );

        // 3. SpanOutsideVisible (both before start and after end) across all four functions.
        let (text, mut catalog, full) = setup("ab(cd)ef");
        let p = |n| text.validate_offset(n).unwrap();
        let narrow = full
            .narrow(&catalog, ByteSpan::new(p(2), p(6)).unwrap())
            .unwrap();
        for outside in [p(1), p(7)] {
            assert_all_err(
                &catalog,
                &narrow,
                outside,
                NavigationError::View(ViewError::SpanOutsideVisible {
                    requested: ByteSpan::empty_at(outside),
                    visible: narrow.visible(),
                }),
            );
        }

        // 4. UnknownBuffer across all four functions and move_selections.
        let empty_catalog = BufferCatalog::new();
        let id = full.buffer();
        let unknown_err = NavigationError::View(ViewError::UnknownBuffer(id));
        assert_all_err(&empty_catalog, &full, p(0), unknown_err.clone());
        assert_eq!(
            move_selections(
                &empty_catalog,
                &full,
                &[],
                WordMotion::NextStart,
                SelectionMotion::Move
            ),
            Err(unknown_err)
        );

        // 5. BufferClosed across all four functions and move_selections.
        catalog.remove(id).unwrap();
        let closed_err = NavigationError::View(ViewError::BufferClosed(id));
        assert_all_err(&catalog, &full, p(0), closed_err.clone());
        assert_eq!(
            move_selections(
                &catalog,
                &full,
                &[],
                WordMotion::NextStart,
                SelectionMotion::Move
            ),
            Err(closed_err)
        );
    }
}
