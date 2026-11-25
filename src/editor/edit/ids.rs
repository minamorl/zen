//! Immutable identifiers for edit operations.
//!
//! Each scalar identifier has the total order of its underlying `u64`. A
//! [`TxnId`] has the lexicographic order of `(actor, nonce)`. These orders are
//! reflexive (`x <= x`), antisymmetric (`x <= y && y <= x` implies `x == y`),
//! transitive (`x <= y && y <= z` implies `x <= z`), and total (`x <= y ||
//! y <= x`). The scalar laws follow from the total order on `u64`; the
//! transaction laws follow from lexicographic composition of two total orders.
//!
//! A transaction stores exactly its actor and nonce. Therefore
//! `TxnId::from_parts(id.into_parts()) == id` and
//! `TxnId::new(a, n).into_parts() == (a, n)`. Equality compares both fields,
//! so distinct pairs yield distinct transaction identifiers (injectivity).

use std::fmt;

macro_rules! scalar_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(u64);

        impl $name {
            /// Creates an identifier from its raw value.
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            /// Returns the raw value.
            pub const fn raw(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

scalar_id!(
    /// Identifies an editor buffer.
    BufferId
);
scalar_id!(
    /// Identifies an actor.
    ActorId
);
scalar_id!(
    /// Identifies a nonce within an actor's transactions.
    Nonce
);

impl Nonce {
    /// Returns the next nonce, or `None` at `u64::MAX`.
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }
}

/// Identifies a transaction by its actor and nonce.
///
/// Ordering is lexicographic: actor first, then nonce.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TxnId {
    actor: ActorId,
    nonce: Nonce,
}

impl TxnId {
    /// Creates a transaction identifier from its components.
    pub const fn new(actor: ActorId, nonce: Nonce) -> Self {
        Self { actor, nonce }
    }

    /// Returns the actor component.
    pub const fn actor(self) -> ActorId {
        self.actor
    }

    /// Returns the nonce component.
    pub const fn nonce(self) -> Nonce {
        self.nonce
    }

    /// Decomposes this identifier into its actor and nonce.
    pub const fn into_parts(self) -> (ActorId, Nonce) {
        (self.actor, self.nonce)
    }

    /// Reconstructs an identifier from its actor and nonce.
    pub const fn from_parts((actor, nonce): (ActorId, Nonce)) -> Self {
        Self::new(actor, nonce)
    }
}

impl fmt::Display for TxnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.actor, self.nonce)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::Rng;
    use std::fmt::Debug;

    fn order_laws<T: Ord + Copy + Debug>(x: T, y: T, z: T) {
        assert!(x.le(&x));
        assert!(!(x <= y && y <= x) || x == y);
        assert!(!(x <= y && y <= z) || x <= z);
        assert!(x <= y || y <= x);
    }

    fn check_values(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) {
        let (x, y, z) = (BufferId::new(a), BufferId::new(b), BufferId::new(c));
        order_laws(x, y, z);
        assert_eq!(x.raw(), a);
        assert_eq!(x.cmp(&y), a.cmp(&b));
        let (x, y, z) = (ActorId::new(a), ActorId::new(b), ActorId::new(c));
        order_laws(x, y, z);
        assert_eq!(x.raw(), a);
        assert_eq!(x.cmp(&y), a.cmp(&b));
        let (x, y, z) = (Nonce::new(a), Nonce::new(b), Nonce::new(c));
        order_laws(x, y, z);
        assert_eq!(x.raw(), a);
        assert_eq!(x.next().map(Nonce::raw), a.checked_add(1));
        assert_eq!(x.cmp(&y), a.cmp(&b));

        let parts = (ActorId::new(a), Nonce::new(d));
        let other = (ActorId::new(b), Nonce::new(e));
        let third = (ActorId::new(c), Nonce::new(f));
        let x = TxnId::from_parts(parts);
        let y = TxnId::from_parts(other);
        let z = TxnId::from_parts(third);
        order_laws(x, y, z);
        assert_eq!(TxnId::from_parts(x.into_parts()), x);
        assert_eq!(TxnId::new(parts.0, parts.1).into_parts(), parts);
        assert_eq!((x.actor(), x.nonce()), parts);
        assert_eq!(x == y, parts == other);
        assert_eq!(x.cmp(&y), parts.cmp(&other));
    }

    #[test]
    fn boundary_and_random_laws() {
        let edges = [0, 1, u64::MAX];
        for &a in &edges {
            for &b in &edges {
                for &c in &edges {
                    for &d in &edges {
                        for &e in &edges {
                            for &f in &edges {
                                check_values(a, b, c, d, e, f);
                            }
                        }
                    }
                }
            }
        }
        for seed in [0, 1, 42, u64::MAX] {
            let mut rng = Rng::new(seed);
            for _ in 0..1024 {
                check_values(
                    rng.next_u64(),
                    rng.next_u64(),
                    rng.next_u64(),
                    rng.next_u64(),
                    rng.next_u64(),
                    rng.next_u64(),
                );
            }
        }
    }

    #[test]
    fn display_identifiers() {
        assert_eq!(BufferId::new(1).to_string(), "1");
        assert_eq!(ActorId::new(0).to_string(), "0");
        assert_eq!(Nonce::new(u64::MAX).to_string(), u64::MAX.to_string());
        assert_eq!(
            TxnId::new(ActorId::new(1), Nonce::new(2)).to_string(),
            "1:2"
        );
    }
}
