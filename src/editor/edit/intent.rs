//! User editing intentions.

use std::fmt;

use crate::editor::edit::ids::{ActorId, Nonce, TxnId};
use crate::editor::workspace::version::Revision;

/// Identifies an edit request by its transaction and base revision.
///
/// # Laws
///
/// * **Direct product:** `EditIntent::from_parts(i.into_parts()) == i` and
///   `EditIntent::new(t, r).into_parts() == (t, r)`. Both follow because the
///   value stores exactly those two components and decomposition returns them.
/// * **Base sensitivity:** if `r1 != r2`, then
///   `EditIntent::new(t, r1) != EditIntent::new(t, r2)`. Equality compares
///   both stored components, including the distinct revisions.
/// * **Total order:** ordering is lexicographic over `(txn, base)`. The
///   component orders are total, and lexicographic composition preserves a
///   total order consistent with equality.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EditIntent {
    txn: TxnId,
    base: Revision,
}

impl EditIntent {
    /// Creates an intent from its transaction and base revision.
    pub const fn new(txn: TxnId, base: Revision) -> Self {
        Self { txn, base }
    }

    /// Creates an intent from an actor, nonce, and base revision.
    pub const fn for_actor(actor: ActorId, nonce: Nonce, base: Revision) -> Self {
        Self::new(TxnId::new(actor, nonce), base)
    }

    /// Returns the transaction identifier.
    pub const fn txn(self) -> TxnId {
        self.txn
    }

    /// Returns the transaction's actor.
    pub const fn actor(self) -> ActorId {
        self.txn.actor()
    }

    /// Returns the transaction's nonce.
    pub const fn nonce(self) -> Nonce {
        self.txn.nonce()
    }

    /// Returns the base revision.
    pub const fn base(self) -> Revision {
        self.base
    }

    /// Decomposes this intent into its transaction and base revision.
    pub const fn into_parts(self) -> (TxnId, Revision) {
        (self.txn, self.base)
    }

    /// Reconstructs an intent from its transaction and base revision.
    pub const fn from_parts((txn, base): (TxnId, Revision)) -> Self {
        Self::new(txn, base)
    }

    /// Returns this intent with a different base revision.
    pub const fn with_base(self, base: Revision) -> Self {
        Self::new(self.txn, base)
    }
}

impl fmt::Display for EditIntent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.txn, self.base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::Rng;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn check(parts: (TxnId, Revision), other: (TxnId, Revision), third: (TxnId, Revision)) {
        let x = EditIntent::from_parts(parts);
        let y = EditIntent::from_parts(other);
        let z = EditIntent::from_parts(third);
        assert_eq!(EditIntent::from_parts(x.into_parts()), x);
        assert_eq!(EditIntent::new(parts.0, parts.1).into_parts(), parts);
        assert_eq!((x.txn(), x.base()), parts);
        assert_eq!((x.actor(), x.nonce()), parts.0.into_parts());
        assert_eq!(EditIntent::for_actor(x.actor(), x.nonce(), x.base()), x);
        assert_eq!(x.with_base(other.1).into_parts(), (parts.0, other.1));
        assert_eq!(x == x.with_base(other.1), parts.1 == other.1);
        assert_eq!(x == y, parts == other);
        assert_eq!(x.cmp(&y), parts.cmp(&other));
        assert!(x.le(&x));
        assert!(!(x <= y && y <= x) || x == y);
        assert!(!(x <= y && y <= z) || x <= z);
        assert!(x <= y || y <= x);
        let mut a = DefaultHasher::new();
        let mut b = DefaultHasher::new();
        x.hash(&mut a);
        EditIntent::from_parts(x.into_parts()).hash(&mut b);
        assert_eq!(a.finish(), b.finish());
    }

    fn parts(actor: u64, nonce: u64, base: u64) -> (TxnId, Revision) {
        (
            TxnId::new(ActorId::new(actor), Nonce::new(nonce)),
            Revision::new(base),
        )
    }

    #[test]
    fn boundary_and_random_laws() {
        let edges = [0, 1, u64::MAX];
        for &actor in &edges {
            for &nonce in &edges {
                for &base in &edges {
                    for &other in &edges {
                        let x = parts(actor, nonce, base);
                        check(x, parts(actor, nonce, other), parts(other, nonce, base));
                        check(x, parts(actor, other, base), parts(other, nonce, base));
                    }
                }
            }
        }
        for seed in [0, 1, 42, u64::MAX] {
            let mut rng = Rng::new(seed);
            for _ in 0..1024 {
                let x = parts(rng.next_u64(), rng.next_u64(), rng.next_u64());
                let y = parts(rng.next_u64(), rng.next_u64(), rng.next_u64());
                let z = parts(rng.next_u64(), rng.next_u64(), rng.next_u64());
                check(x, y, z);
                check(x, (x.0, y.1), z);
            }
        }
    }

    #[test]
    fn nonce_precedes_base_in_ordering() {
        let lower_nonce = EditIntent::from_parts(parts(1, 1, 1));
        let higher_nonce = EditIntent::from_parts(parts(1, 2, 0));
        assert!(lower_nonce < higher_nonce);
        assert!(higher_nonce > lower_nonce);
        assert_eq!(
            lower_nonce.cmp(&higher_nonce),
            lower_nonce.into_parts().cmp(&higher_nonce.into_parts())
        );
    }

    #[test]
    fn display_intent() {
        assert_eq!(EditIntent::from_parts(parts(1, 2, 3)).to_string(), "1:2@3");
    }
}
