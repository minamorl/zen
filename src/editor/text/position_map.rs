//! Exact byte-position transport through a sequence of replacements.
//!
//! Each replacement maps positions before its range unchanged, positions after
//! it by the length delta, and the closed range to its left or right edge
//! according to affinity. The map retains every replacement in order: reducing
//! the sequence to a single text edit would lose anchor history.
//!
//! For either affinity each replacement is nondecreasing. Its left result is
//! at most its right result, so these properties are preserved by composition:
//! monotonicity bridges the intermediate left and right results. Evaluation is
//! function composition, which is associative; the empty sequence is identity.
//! Thus evaluating a composed map is exactly sequential evaluation, including
//! at replacement boundaries. Composition checks the intermediate byte length,
//! and evaluation rejects offsets outside the source domain.
//!
//! Lengths and offsets here describe byte geometry, not UTF-8 validation.
//! Callers must validate a mapped offset against the actual destination text
//! before using it as a [`super::utf8::ByteOffset`].

use super::anchors::{Affinity, Anchor};
use std::fmt;

/// A half-open range of source bytes and the number of inserted bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteReplacement {
    /// First removed byte.
    pub start: usize,
    /// End of the removed range, exclusive.
    pub end: usize,
    /// Length in bytes of the inserted text.
    pub inserted_len: usize,
}

/// An anchor in byte geometry, without a claim of UTF-8 boundary validity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PositionAnchor {
    /// Byte offset.
    pub offset: usize,
    /// Gravity at an affected position.
    pub affinity: Affinity,
}

impl From<Anchor> for PositionAnchor {
    fn from(anchor: Anchor) -> Self {
        Self {
            offset: anchor.offset.as_usize(),
            affinity: anchor.affinity,
        }
    }
}

/// Two independently affined endpoints, including collapsed or inverted ones.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnchoredSpan {
    /// Original start endpoint identity.
    pub start: PositionAnchor,
    /// Original end endpoint identity.
    pub end: PositionAnchor,
}

/// A failure to construct, compose, or evaluate a position map.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PositionMapError {
    /// Replacement endpoints are inverted or outside the source.
    InvalidReplacement {
        start: usize,
        end: usize,
        source_len: usize,
    },
    /// The resulting byte length cannot be represented.
    Overflow,
    /// Consecutive maps have different intermediate lengths.
    BoundaryMismatch {
        target_len: usize,
        source_len: usize,
    },
    /// The requested position is outside the source domain.
    OutOfBounds { offset: usize, source_len: usize },
}

impl fmt::Display for PositionMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidReplacement { start, end, .. } if start > end => {
                write!(f, "replacement start {start} exceeds end {end}")
            }
            Self::InvalidReplacement {
                start,
                end,
                source_len,
            } => write!(
                f,
                "replacement {start}..{end} exceeds source length {source_len}"
            ),
            Self::Overflow => write!(f, "position map length overflow"),
            Self::BoundaryMismatch {
                target_len,
                source_len,
            } => write!(
                f,
                "position map boundary mismatch: {target_len} != {source_len}"
            ),
            Self::OutOfBounds { offset, source_len } => {
                write!(f, "offset {offset} exceeds source length {source_len}")
            }
        }
    }
}

impl std::error::Error for PositionMapError {}

/// A composable sequence of exact dual-gravity byte-position transports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PositionMap {
    source_len: usize,
    target_len: usize,
    replacements: Vec<ByteReplacement>,
}

impl PositionMap {
    /// The identity on all offsets from zero through `len`.
    pub fn identity(len: usize) -> Self {
        Self {
            source_len: len,
            target_len: len,
            replacements: Vec::new(),
        }
    }

    /// Constructs a map for one replacement, checking bounds and arithmetic.
    pub fn from_replacement(
        source_len: usize,
        replacement: ByteReplacement,
    ) -> Result<Self, PositionMapError> {
        if replacement.start > replacement.end || replacement.end > source_len {
            return Err(PositionMapError::InvalidReplacement {
                start: replacement.start,
                end: replacement.end,
                source_len,
            });
        }
        let target_len = (source_len - (replacement.end - replacement.start))
            .checked_add(replacement.inserted_len)
            .ok_or(PositionMapError::Overflow)?;
        Ok(Self {
            source_len,
            target_len,
            replacements: vec![replacement],
        })
    }

    /// Source byte length.
    pub fn source_len(&self) -> usize {
        self.source_len
    }

    /// Destination byte length.
    pub fn target_len(&self) -> usize {
        self.target_len
    }

    /// Applies `self` followed by `next`; rejects incompatible boundaries.
    pub fn compose(&self, next: &Self) -> Result<Self, PositionMapError> {
        if self.target_len != next.source_len {
            return Err(PositionMapError::BoundaryMismatch {
                target_len: self.target_len,
                source_len: next.source_len,
            });
        }
        let mut replacements = self.replacements.clone();
        replacements.extend_from_slice(&next.replacements);
        Ok(Self {
            source_len: self.source_len,
            target_len: next.target_len,
            replacements,
        })
    }

    /// Maps a byte offset with a fixed affinity, rejecting out-of-domain input.
    pub fn map_offset(&self, offset: usize, affinity: Affinity) -> Result<usize, PositionMapError> {
        if offset > self.source_len {
            return Err(PositionMapError::OutOfBounds {
                offset,
                source_len: self.source_len,
            });
        }
        let mut p = offset;
        for r in &self.replacements {
            p = if p < r.start {
                p
            } else if p > r.end {
                p - (r.end - r.start) + r.inserted_len
            } else if affinity == Affinity::Left {
                r.start
            } else {
                r.start + r.inserted_len
            };
        }
        Ok(p)
    }

    /// Maps an existing validated anchor or a byte-geometry anchor.
    pub fn map_anchor(
        &self,
        anchor: impl Into<PositionAnchor>,
    ) -> Result<PositionAnchor, PositionMapError> {
        let anchor = anchor.into();
        Ok(PositionAnchor {
            offset: self.map_offset(anchor.offset, anchor.affinity)?,
            affinity: anchor.affinity,
        })
    }

    /// Maps endpoints independently, retaining their identities and order.
    ///
    /// A collapsed result has equal offsets; an inverted result has its start
    /// greater than its end. Neither is sorted or otherwise normalized.
    pub fn map_anchored_span(&self, span: AnchoredSpan) -> Result<AnchoredSpan, PositionMapError> {
        Ok(AnchoredSpan {
            start: self.map_anchor(span.start)?,
            end: self.map_anchor(span.end)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::utf8::Text;
    use super::*;

    fn maps(len: usize) -> Vec<PositionMap> {
        let mut result = vec![PositionMap::identity(len)];
        for start in 0..=len {
            for end in start..=len {
                for inserted_len in 0..=2 {
                    result.push(
                        PositionMap::from_replacement(
                            len,
                            ByteReplacement {
                                start,
                                end,
                                inserted_len,
                            },
                        )
                        .unwrap(),
                    );
                }
            }
        }
        result
    }

    #[test]
    fn exhaustive_sequence_laws() {
        for len in 0..=2 {
            for a in maps(len) {
                assert_eq!(PositionMap::identity(len).compose(&a).unwrap(), a);
                assert_eq!(
                    a.compose(&PositionMap::identity(a.target_len())).unwrap(),
                    a
                );
                for b in maps(a.target_len()) {
                    let ab = a.compose(&b).unwrap();
                    for c in maps(b.target_len()) {
                        let left = ab.compose(&c).unwrap();
                        let right = a.compose(&b.compose(&c).unwrap()).unwrap();
                        let mut previous = [0, 0];
                        for p in 0..=len {
                            let mut mapped = [0, 0];
                            for (i, g) in [Affinity::Left, Affinity::Right].into_iter().enumerate()
                            {
                                let sequential = c
                                    .map_offset(
                                        b.map_offset(a.map_offset(p, g).unwrap(), g).unwrap(),
                                        g,
                                    )
                                    .unwrap();
                                mapped[i] = left.map_offset(p, g).unwrap();
                                assert_eq!(mapped[i], right.map_offset(p, g).unwrap());
                                assert_eq!(mapped[i], sequential);
                                assert_eq!(
                                    ab.map_offset(p, g).unwrap(),
                                    b.map_offset(a.map_offset(p, g).unwrap(), g).unwrap()
                                );
                                if p > 0 {
                                    assert!(previous[i] <= mapped[i]);
                                }
                            }
                            assert!(mapped[0] <= mapped[1]);
                            previous = mapped;
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn insertion_then_deletion_retains_history_and_endpoints() {
        let insert = PositionMap::from_replacement(
            3,
            ByteReplacement {
                start: 0,
                end: 0,
                inserted_len: 2,
            },
        )
        .unwrap();
        let delete = PositionMap::from_replacement(
            5,
            ByteReplacement {
                start: 2,
                end: 5,
                inserted_len: 0,
            },
        )
        .unwrap();
        let map = insert.compose(&delete).unwrap();
        assert_eq!(map.map_offset(3, Affinity::Left), Ok(2));
        let a = |offset, affinity| PositionAnchor { offset, affinity };
        let span = insert
            .map_anchored_span(AnchoredSpan {
                start: a(0, Affinity::Right),
                end: a(0, Affinity::Left),
            })
            .unwrap();
        assert_eq!((span.start.offset, span.end.offset), (2, 0));
        let collapsed = delete
            .map_anchored_span(AnchoredSpan {
                start: a(2, Affinity::Left),
                end: a(5, Affinity::Right),
            })
            .unwrap();
        assert_eq!((collapsed.start.offset, collapsed.end.offset), (2, 2));
        let anchor = Anchor {
            offset: Text::from_str("abc").validate_offset(3).unwrap(),
            affinity: Affinity::Left,
        };
        assert_eq!(map.map_anchor(anchor).unwrap(), a(2, Affinity::Left));
    }

    #[test]
    fn rejects_invalid_boundaries_and_overflow() {
        let id = PositionMap::identity(3);
        assert_eq!(
            id.compose(&PositionMap::identity(4)),
            Err(PositionMapError::BoundaryMismatch {
                target_len: 3,
                source_len: 4
            })
        );
        assert_eq!(
            id.map_offset(4, Affinity::Left),
            Err(PositionMapError::OutOfBounds {
                offset: 4,
                source_len: 3
            })
        );
        for (start, end) in [(2, 1), (0, 4)] {
            assert_eq!(
                PositionMap::from_replacement(
                    3,
                    ByteReplacement {
                        start,
                        end,
                        inserted_len: 0
                    }
                ),
                Err(PositionMapError::InvalidReplacement {
                    start,
                    end,
                    source_len: 3
                })
            );
        }
        assert_eq!(
            PositionMapError::InvalidReplacement {
                start: 2,
                end: 1,
                source_len: 3
            }
            .to_string(),
            "replacement start 2 exceeds end 1"
        );
        assert_eq!(
            PositionMapError::InvalidReplacement {
                start: 0,
                end: 4,
                source_len: 3
            }
            .to_string(),
            "replacement 0..4 exceeds source length 3"
        );
        assert_eq!(
            PositionMap::from_replacement(
                1,
                ByteReplacement {
                    start: 0,
                    end: 0,
                    inserted_len: usize::MAX
                }
            ),
            Err(PositionMapError::Overflow)
        );
    }
}
