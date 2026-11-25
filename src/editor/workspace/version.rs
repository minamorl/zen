//! Workspace and buffer versions.

use std::fmt;

/// A monotonically increasing workspace revision.
///
/// # Laws
///
/// * **Strict monotonicity:** If `r.next() == Ok(r2)`, then `r2 > r` and
///   `r2.steps_since(r) == Some(1)`. The checked addition succeeds only when
///   the mathematical sum exists in `u64`, so it is exactly one greater.
/// * **Additive action:** `r.advance(0) == Ok(r)`. If
///   `r.advance(n) == Ok(r2)`, then `r2.raw() == r.raw() + n`.
///   Iterating `next()` `n` times has the same result as `advance(n)`: each
///   successful step adds one, and either procedure fails exactly when the sum
///   exceeds `u64::MAX`.
/// * **No wraparound:** `Revision::MAX.next()` and every overflowing
///   `advance` return `Err(RevisionOverflow)`, because both use checked addition.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Revision(u64);

/// The requested revision exceeds `u64::MAX`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RevisionOverflow;

impl fmt::Display for RevisionOverflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("workspace revision overflow")
    }
}

impl std::error::Error for RevisionOverflow {}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Revision {
    /// The initial revision.
    pub const ZERO: Self = Self(0);
    /// The greatest representable revision.
    pub const MAX: Self = Self(u64::MAX);

    /// Constructs a revision from its raw value.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the underlying revision number.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Returns the next revision, or an error at the maximum.
    pub fn next(self) -> Result<Self, RevisionOverflow> {
        self.advance(1)
    }

    /// Advances by `steps`, returning an error if the sum overflows.
    pub fn advance(self, steps: u64) -> Result<Self, RevisionOverflow> {
        self.0.checked_add(steps).map(Self).ok_or(RevisionOverflow)
    }

    /// Returns the distance from `base`, or `None` if `base` is later.
    pub fn steps_since(self, base: Self) -> Option<u64> {
        self.0.checked_sub(base.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{Revision, RevisionOverflow};
    use crate::editor::test_support::Rng;

    #[test]
    fn boundaries_and_display() {
        assert_eq!(Revision::default(), Revision::ZERO);
        assert_eq!(Revision::new(1).to_string(), "1");
        assert_eq!(RevisionOverflow.to_string(), "workspace revision overflow");
        for value in [0, 1, u64::MAX - 1, u64::MAX] {
            let r = Revision::new(value);
            assert_eq!(r.raw(), value);
            assert_eq!(r.advance(0), Ok(r));
            assert_eq!(r.steps_since(r), Some(0));
            assert_eq!(
                r.next(),
                value
                    .checked_add(1)
                    .map(Revision::new)
                    .ok_or(RevisionOverflow)
            );
            assert_eq!(r.advance(u64::MAX - value), Ok(Revision::MAX));
            if value > 0 {
                assert_eq!(r.advance(u64::MAX - value + 1), Err(RevisionOverflow));
            }
        }
        assert_eq!(Revision::MAX.next(), Err(RevisionOverflow));
        assert_eq!(Revision::ZERO.steps_since(Revision::new(1)), None);
    }

    #[test]
    fn deterministic_revision_laws() {
        let mut rng = Rng::new(0x7631_7265_7669_7369);
        for _ in 0..4096 {
            let value = rng.next_u64();
            let steps = rng.next_u64();
            let r = Revision::new(value);
            let expected = value
                .checked_add(steps)
                .map(Revision::new)
                .ok_or(RevisionOverflow);
            assert_eq!(r.advance(steps), expected);
            if let Ok(end) = expected {
                assert_eq!(end.raw(), value + steps);
                assert_eq!(end.steps_since(r), Some(steps));
                if steps > 0 {
                    assert_eq!(r.steps_since(end), None);
                }
            }
            if let Ok(next) = r.next() {
                assert!(next > r);
                assert_eq!(next.steps_since(r), Some(1));
            }
            let small = rng.next_u64() % 32;
            // Include starts close to MAX so iteration also exercises failure.
            let start = if rng.next_u64() & 1 == 0 {
                r
            } else {
                Revision::new(u64::MAX - small / 2)
            };
            let mut iterated = Ok(start);
            for _ in 0..small {
                iterated = iterated.and_then(Revision::next);
            }
            assert_eq!(iterated, start.advance(small));
        }
    }
}
