//! Structural matching of single-character delimiters in UTF-8 text.
//!
//! A closer matches only the top of the stack. A mismatched closer is
//! reported without disturbing that stack, so matched pairs never cross.

use super::span::ByteSpan;
use super::utf8::{ByteOffset, Text, Utf8Error};
use std::fmt;

/// An invalid delimiter table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TableError {
    /// At least one pair is required.
    Empty,
    /// A character appears more than once, including in opposite roles.
    RepeatedCharacter(char),
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "delimiter table is empty"),
            Self::RepeatedCharacter(ch) => write!(f, "delimiter character {ch:?} is repeated"),
        }
    }
}

impl std::error::Error for TableError {}

/// Unambiguous opener and closer characters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DelimiterTable {
    pairs: Vec<(char, char)>,
}

impl DelimiterTable {
    /// Rejects an empty table and any character used more than once.
    pub fn new(pairs: &[(char, char)]) -> Result<Self, TableError> {
        if pairs.is_empty() {
            return Err(TableError::Empty);
        }
        let mut seen = std::collections::HashSet::new();
        for &(open, close) in pairs {
            for ch in [open, close] {
                if !seen.insert(ch) {
                    return Err(TableError::RepeatedCharacter(ch));
                }
            }
        }
        Ok(Self {
            pairs: pairs.to_vec(),
        })
    }

    /// Scans the text and records both matches and unmatched delimiters.
    pub fn scan(&self, text: &Text) -> Delimiters {
        let mut stack: Vec<(usize, char)> = Vec::new();
        let mut pairs = Vec::new();
        let mut unmatched_closers = Vec::new();
        for (offset, ch) in text.as_str().char_indices() {
            if let Some(&(_, close)) = self.pairs.iter().find(|&&(open, _)| open == ch) {
                stack.push((offset, close));
            } else if self.pairs.iter().any(|&(_, close)| close == ch) {
                if stack.last().is_some_and(|&(_, close)| close == ch) {
                    let (open, _) = stack.pop().expect("matching stack entry exists");
                    let outer = ByteSpan::validate_in(text, open, offset + ch.len_utf8())
                        .expect("character indices are boundaries");
                    let open_end = open + text.as_str()[open..].chars().next().unwrap().len_utf8();
                    let inner = ByteSpan::validate_in(text, open_end, offset)
                        .expect("delimiter boundaries are ordered");
                    pairs.push(DelimiterPair {
                        open: outer.start(),
                        close: text.validate_offset(offset).expect("character boundary"),
                        outer,
                        inner,
                    });
                } else {
                    unmatched_closers
                        .push(text.validate_offset(offset).expect("character boundary"));
                }
            }
        }
        pairs.sort_by_key(|pair| pair.open);
        Delimiters {
            text: text.clone(),
            pairs,
            unmatched_openers: stack
                .into_iter()
                .map(|(offset, _)| text.validate_offset(offset).expect("character boundary"))
                .collect(),
            unmatched_closers,
        }
    }
}

/// A matched pair and its half-open outer and inner byte spans.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelimiterPair {
    /// Byte offset of the opening character.
    pub open: ByteOffset,
    /// Byte offset of the closing character.
    pub close: ByteOffset,
    /// Both delimiters and the text between them.
    pub outer: ByteSpan,
    /// The text strictly between the delimiters.
    pub inner: ByteSpan,
}

/// A scan tied to the exact text used to create it.
#[derive(Clone, Debug)]
pub struct Delimiters {
    text: Text,
    pairs: Vec<DelimiterPair>,
    unmatched_openers: Vec<ByteOffset>,
    unmatched_closers: Vec<ByteOffset>,
}

impl Delimiters {
    /// Matched pairs in opening-offset order.
    pub fn pairs(&self) -> &[DelimiterPair] {
        &self.pairs
    }

    /// Unmatched openers in text order.
    pub fn unmatched_openers(&self) -> &[ByteOffset] {
        &self.unmatched_openers
    }

    /// Unmatched closers in text order.
    pub fn unmatched_closers(&self) -> &[ByteOffset] {
        &self.unmatched_closers
    }

    /// Finds the other delimiter at a byte boundary, or none at other positions.
    pub fn matching(&self, offset: usize) -> Result<Option<ByteOffset>, Utf8Error> {
        let offset = self.text.validate_offset(offset)?;
        Ok(self.pairs.iter().find_map(|pair| {
            if pair.open == offset {
                Some(pair.close)
            } else if pair.close == offset {
                Some(pair.open)
            } else {
                None
            }
        }))
    }

    /// Finds the innermost pair whose outer half-open span contains the position.
    /// The opener and closer positions are included; the boundary after the closer is not.
    pub fn enclosing(&self, offset: usize) -> Result<Option<DelimiterPair>, Utf8Error> {
        let offset = self.text.validate_offset(offset)?;
        Ok(self
            .pairs
            .iter()
            .rev()
            .find(|pair| pair.open <= offset && offset < pair.outer.end())
            .copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, Rng};

    fn reference(
        source: &str,
        table: &[(char, char)],
    ) -> (Vec<(usize, usize)>, Vec<usize>, Vec<usize>) {
        let mut stack = Vec::new();
        let mut matches = Vec::new();
        let mut closers = Vec::new();
        for (pos, ch) in source.char_indices() {
            if let Some(index) = table.iter().position(|pair| pair.0 == ch) {
                stack.push((pos, index));
            } else if let Some(index) = table.iter().position(|pair| pair.1 == ch) {
                match stack.last() {
                    Some(&(_, top)) if top == index => matches.push((stack.pop().unwrap().0, pos)),
                    _ => closers.push(pos),
                }
            }
        }
        matches.sort_unstable();
        (
            matches,
            stack.into_iter().map(|item| item.0).collect(),
            closers,
        )
    }

    fn corpus(rng: &mut Rng) -> Vec<String> {
        let mut texts: Vec<String> = [
            "",
            "()",
            "([)]",
            "([{}])",
            "「界（😀）』」",
            "(()",
            "][",
            "()[]",
            "「」",
            "¢😀《>¢x😀",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let alphabet = [
            '(', ')', '[', ']', '{', '}', '「', '」', '（', '）', '¢', '😀', '《', '>', '界', 'a',
        ];
        for _ in 0..100 {
            let mut text = String::new();
            for _ in 0..rng.gen_range(0..65) {
                text.push(*rng.choose(&alphabet).unwrap());
            }
            texts.push(text);
        }
        texts
    }

    #[test]
    fn generated_stack_and_range_laws() {
        let definitions = [
            ('(', ')'),
            ('[', ']'),
            ('{', '}'),
            ('「', '」'),
            ('（', '）'),
            ('¢', '😀'),
            ('《', '>'),
        ];
        let table = DelimiterTable::new(&definitions).unwrap();
        assert_law(&[0, 1, 42, 99, u64::MAX], corpus, |texts| {
            for source in texts {
                let text = Text::from_str(source);
                let scan = table.scan(&text);
                let (expected, openers, closers) = reference(source, &definitions);
                let actual: Vec<_> = scan
                    .pairs()
                    .iter()
                    .map(|p| (p.open.as_usize(), p.close.as_usize()))
                    .collect();
                assert_eq!(actual, expected);
                assert_eq!(
                    scan.unmatched_openers()
                        .iter()
                        .map(|p| p.as_usize())
                        .collect::<Vec<_>>(),
                    openers
                );
                assert_eq!(
                    scan.unmatched_closers()
                        .iter()
                        .map(|p| p.as_usize())
                        .collect::<Vec<_>>(),
                    closers
                );
                for pair in scan.pairs() {
                    let a = pair.open.as_usize();
                    let b = pair.close.as_usize();
                    let inner_start = a + source[a..].chars().next().unwrap().len_utf8();
                    let outer_end = b + source[b..].chars().next().unwrap().len_utf8();
                    assert_eq!(
                        (pair.outer.start().as_usize(), pair.outer.end().as_usize()),
                        (a, outer_end)
                    );
                    assert_eq!(
                        (pair.inner.start().as_usize(), pair.inner.end().as_usize()),
                        (inner_start, b)
                    );
                    assert_eq!(scan.matching(a).unwrap(), Some(pair.close));
                    assert_eq!(scan.matching(b).unwrap(), Some(pair.open));
                    assert_eq!(
                        scan.matching(scan.matching(a).unwrap().unwrap().as_usize())
                            .unwrap(),
                        Some(pair.open)
                    );
                    assert_eq!(pair.outer.slice_in(&text).unwrap(), &source[a..outer_end]);
                    assert_eq!(pair.inner.slice_in(&text).unwrap(), &source[inner_start..b]);
                    for other in scan.pairs().iter().filter(|other| other.open != pair.open) {
                        let c = other.open.as_usize();
                        let d = other.close.as_usize();
                        assert!(b < c || d < a || (a < c && d < b) || (c < a && b < d));
                    }
                }
                for offset in 0..=source.len() + 1 {
                    if offset > source.len() {
                        let error = Utf8Error::OutOfBounds {
                            offset,
                            len: source.len(),
                        };
                        assert_eq!(scan.matching(offset), Err(error.clone()));
                        assert_eq!(scan.enclosing(offset), Err(error));
                    } else if !source.is_char_boundary(offset) {
                        let error = Utf8Error::NotCharBoundary { offset };
                        assert_eq!(scan.matching(offset), Err(error.clone()));
                        assert_eq!(scan.enclosing(offset), Err(error));
                    } else {
                        let expected_match = expected.iter().find_map(|&(a, b)| {
                            if a == offset {
                                Some(b)
                            } else if b == offset {
                                Some(a)
                            } else {
                                None
                            }
                        });
                        assert_eq!(
                            scan.matching(offset).unwrap().map(ByteOffset::as_usize),
                            expected_match
                        );
                        let expected_enclosing = expected
                            .iter()
                            .filter(|&&(a, b)| {
                                a <= offset
                                    && offset < b + source[b..].chars().next().unwrap().len_utf8()
                            })
                            .max_by_key(|&&(a, _)| a)
                            .copied();
                        assert_eq!(
                            scan.enclosing(offset)
                                .unwrap()
                                .map(|p| (p.open.as_usize(), p.close.as_usize())),
                            expected_enclosing
                        );
                    }
                }
            }
        });
    }

    #[test]
    fn table_validation() {
        assert_eq!(DelimiterTable::new(&[]), Err(TableError::Empty));
        for pairs in [
            &[('(', '(')][..],
            &[('(', ')'), ('(', ']')],
            &[('(', ')'), ('[', ')')],
            &[('(', ')'), (')', ']')],
            &[('界', '界')],
        ] {
            assert!(matches!(
                DelimiterTable::new(pairs),
                Err(TableError::RepeatedCharacter(_))
            ));
        }
        assert!(DelimiterTable::new(&[('「', '」'), ('（', '）')]).is_ok());
    }
}
