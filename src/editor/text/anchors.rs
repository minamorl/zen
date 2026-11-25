//! Byte anchors transported through a single replacement.
//!
//! For replacement `[s, e)` with `m` inserted bytes, positions before `s`
//! stay fixed, positions after `e` shift by `m - (e - s)`, and positions in
//! the closed interval `[s, e]` map to `s` (left) or `s + m` (right).
//! This also covers insertion, where `s == e`.
//!
//! For either fixed affinity the three pieces are nondecreasing: the middle
//! lies between the last front position and the first shifted rear position.
//! At every position left is at most right, since the only difference is
//! `s` versus `s + m`. Exterior positions follow the stated exact shift; when
//! `s == e` and `m == 0`, every piece is the identity. Independent affinities
//! can nevertheless invert a range's endpoints, so that outcome is retained
//! explicitly instead of being sorted into a different range.

use super::span::ByteSpan;
use super::utf8::{ByteOffset, Text};
use std::fmt;

/// The side of replacement text to which an affected anchor adheres.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Affinity {
    /// Remain before inserted text.
    Left,
    /// Move after inserted text.
    Right,
}

/// A validated byte offset and its insertion affinity.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct Anchor {
    /// Offset in the text to which this anchor belongs.
    pub offset: ByteOffset,
    /// Behavior at replacement boundaries and inside replaced text.
    pub affinity: Affinity,
}

/// A single half-open replacement and the length of its inserted bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ReplacementEffect {
    /// Range in the original text.
    pub span: ByteSpan,
    /// Byte length of the inserted text.
    pub inserted_len: usize,
}

/// Invalid text bounds or checked arithmetic overflow during transport.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnchorError {
    /// A byte length or offset exceeds `usize`.
    Overflow,
    /// A boundary, output length, or unchanged exterior does not match.
    InvalidBounds,
}

impl fmt::Display for AnchorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow => write!(f, "anchor transport overflow"),
            Self::InvalidBounds => write!(f, "invalid replacement or anchor bounds"),
        }
    }
}

impl std::error::Error for AnchorError {}

/// Result of transporting independently-affined range endpoints.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportedSpan {
    /// Endpoints remain strictly ordered.
    Range { start: Anchor, end: Anchor },
    /// Endpoints coincide.
    Collapsed { start: Anchor, end: Anchor },
    /// Start moved past end; both original endpoint identities are preserved.
    Inverted { start: Anchor, end: Anchor },
}

impl ReplacementEffect {
    /// Transport an anchor from `before` to `after`.
    ///
    /// Both texts must describe this replacement: their exterior bytes agree,
    /// the inserted length matches, and all input and output offsets must be
    /// UTF-8 boundaries. Foreign offsets are revalidated. Arithmetic is checked
    /// before the output text is inspected.
    ///
    /// Ordered, validated bounds make both the removed length and the rear
    /// offset subtractions safe. Adding the inserted length to the start,
    /// resulting text length, or shifted rear offset returns `Overflow` if
    /// the sum cannot be represented.
    pub fn transport(
        self,
        anchor: Anchor,
        before: &Text,
        after: &Text,
    ) -> Result<Anchor, AnchorError> {
        let s = self.span.start().as_usize();
        let e = self.span.end().as_usize();
        let p = anchor.offset.as_usize();
        before
            .validate_offset(s)
            .map_err(|_| AnchorError::InvalidBounds)?;
        before
            .validate_offset(e)
            .map_err(|_| AnchorError::InvalidBounds)?;
        before
            .validate_offset(p)
            .map_err(|_| AnchorError::InvalidBounds)?;
        let inserted_end = s
            .checked_add(self.inserted_len)
            .ok_or(AnchorError::Overflow)?;
        let new_len = (before.len_bytes() - (e - s))
            .checked_add(self.inserted_len)
            .ok_or(AnchorError::Overflow)?;
        let mapped = if p < s {
            p
        } else if p > e {
            (p - (e - s))
                .checked_add(self.inserted_len)
                .ok_or(AnchorError::Overflow)?
        } else if anchor.affinity == Affinity::Left {
            s
        } else {
            inserted_end
        };
        if after.len_bytes() != new_len
            || !after.as_str().is_char_boundary(inserted_end)
            || before.as_bytes()[..s] != after.as_bytes()[..s]
            || before.as_bytes()[e..] != after.as_bytes()[inserted_end..]
        {
            return Err(AnchorError::InvalidBounds);
        }
        let offset = after
            .validate_offset(mapped)
            .map_err(|_| AnchorError::InvalidBounds)?;
        Ok(Anchor {
            offset,
            affinity: anchor.affinity,
        })
    }

    /// Transport ordered input endpoints while retaining collapsed or inverted results.
    pub fn transport_endpoints(
        self,
        start: Anchor,
        end: Anchor,
        before: &Text,
        after: &Text,
    ) -> Result<TransportedSpan, AnchorError> {
        if start.offset > end.offset {
            return Err(AnchorError::InvalidBounds);
        }
        let start = self.transport(start, before, after)?;
        let end = self.transport(end, before, after)?;
        Ok(match start.offset.cmp(&end.offset) {
            std::cmp::Ordering::Less => TransportedSpan::Range { start, end },
            std::cmp::Ordering::Equal => TransportedSpan::Collapsed { start, end },
            std::cmp::Ordering::Greater => TransportedSpan::Inverted { start, end },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};

    #[test]
    fn exhaustive_boundary_transport_laws() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let before = Text::from_str(source);
                let boundaries = before.char_boundaries();
                for (i, &s) in boundaries.iter().enumerate() {
                    for &e in &boundaries[i..] {
                        for inserted in ["", "x", "界", "😀"] {
                            let (s0, e0) = (s.as_usize(), e.as_usize());
                            let after = Text::from_str(&format!(
                                "{}{}{}",
                                &source[..s0],
                                inserted,
                                &source[e0..]
                            ));
                            let effect = ReplacementEffect {
                                span: ByteSpan::new(s, e).unwrap(),
                                inserted_len: inserted.len(),
                            };
                            let mut previous = [0, 0];
                            for (index, &p) in boundaries.iter().enumerate() {
                                let mut mapped = [0, 0];
                                for (g, affinity) in
                                    [Affinity::Left, Affinity::Right].into_iter().enumerate()
                                {
                                    let anchor = Anchor {
                                        offset: p,
                                        affinity,
                                    };
                                    mapped[g] = effect
                                        .transport(anchor, &before, &after)
                                        .unwrap()
                                        .offset
                                        .as_usize();
                                    let n = p.as_usize();
                                    let expected = if n < s0 {
                                        n
                                    } else if n > e0 {
                                        n - (e0 - s0) + inserted.len()
                                    } else if affinity == Affinity::Left {
                                        s0
                                    } else {
                                        s0 + inserted.len()
                                    };
                                    assert_eq!(mapped[g], expected);
                                    if index > 0 {
                                        assert!(previous[g] <= mapped[g]);
                                    }
                                    if s == e && inserted.is_empty() {
                                        assert_eq!(mapped[g], n);
                                    }
                                }
                                assert!(mapped[0] <= mapped[1]);
                                previous = mapped;
                            }
                            for (j, &start) in boundaries.iter().enumerate() {
                                for &end in &boundaries[j..] {
                                    for sa in [Affinity::Left, Affinity::Right] {
                                        for ea in [Affinity::Left, Affinity::Right] {
                                            let a = Anchor {
                                                offset: start,
                                                affinity: sa,
                                            };
                                            let b = Anchor {
                                                offset: end,
                                                affinity: ea,
                                            };
                                            let left =
                                                effect.transport(a, &before, &after).unwrap();
                                            let right =
                                                effect.transport(b, &before, &after).unwrap();
                                            let outcome = effect
                                                .transport_endpoints(a, b, &before, &after)
                                                .unwrap();
                                            let expected = match left.offset.cmp(&right.offset) {
                                                std::cmp::Ordering::Less => {
                                                    TransportedSpan::Range {
                                                        start: left,
                                                        end: right,
                                                    }
                                                }
                                                std::cmp::Ordering::Equal => {
                                                    TransportedSpan::Collapsed {
                                                        start: left,
                                                        end: right,
                                                    }
                                                }
                                                std::cmp::Ordering::Greater => {
                                                    TransportedSpan::Inverted {
                                                        start: left,
                                                        end: right,
                                                    }
                                                }
                                            };
                                            assert_eq!(outcome, expected);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn inversion_overflow_and_invalid_bounds() {
        let before = Text::from_str("abc");
        let after = Text::from_str("aXc");
        let at = |n, affinity| Anchor {
            offset: before.validate_offset(n).unwrap(),
            affinity,
        };
        let effect = ReplacementEffect {
            span: ByteSpan::validate_in(&before, 1, 2).unwrap(),
            inserted_len: 1,
        };
        let result = effect
            .transport_endpoints(
                at(1, Affinity::Right),
                at(2, Affinity::Left),
                &before,
                &after,
            )
            .unwrap();
        match result {
            TransportedSpan::Inverted { start, end } => {
                assert_eq!((start.offset.as_usize(), end.offset.as_usize()), (2, 1))
            }
            _ => panic!("expected inverted endpoints"),
        }
        let overflow = ReplacementEffect {
            inserted_len: usize::MAX,
            ..effect
        };
        assert_eq!(
            overflow.transport(at(0, Affinity::Left), &before, &after),
            Err(AnchorError::Overflow)
        );
        let length_overflow = ReplacementEffect {
            span: ByteSpan::validate_in(&before, 0, 1).unwrap(),
            inserted_len: usize::MAX,
        };
        assert_eq!(
            length_overflow.transport(at(0, Affinity::Left), &before, &after),
            Err(AnchorError::Overflow)
        );
        assert_eq!(
            effect.transport(at(0, Affinity::Left), &before, &Text::from_str("zXc")),
            Err(AnchorError::InvalidBounds)
        );
        assert_eq!(
            effect.transport_endpoints(
                at(2, Affinity::Left),
                at(1, Affinity::Left),
                &before,
                &after
            ),
            Err(AnchorError::InvalidBounds)
        );
        let foreign = Text::from_str("界");
        assert_eq!(
            effect.transport(at(1, Affinity::Left), &foreign, &after),
            Err(AnchorError::InvalidBounds)
        );
        let foreign_effect = ReplacementEffect {
            span: ByteSpan::full_of(&foreign),
            inserted_len: 0,
        };
        assert_eq!(
            foreign_effect.transport(at(1, Affinity::Left), &foreign, &Text::empty()),
            Err(AnchorError::InvalidBounds)
        );
    }
}
