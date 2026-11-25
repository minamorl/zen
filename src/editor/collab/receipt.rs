//! Commit receipts and outcomes of submitting an edit.

use std::fmt;

use crate::editor::edit::ids::TxnId;
use crate::editor::edit::intent::EditIntent;
use crate::editor::workspace::version::{Revision, RevisionOverflow};

/// Evidence that an intent committed at the revision following `previous`.
///
/// # Laws
///
/// Every receipt satisfies `committed() == previous().next().unwrap()` and
/// `committed() > previous()`: the private fields can only be initialized by
/// [`CommitReceipt::new`], which stores the successful result of `next`.
/// Checked revision advancement is strictly increasing and cannot wrap.
/// At [`Revision::MAX`], construction returns [`RevisionOverflow`], so no
/// receipt can be created for that previous revision.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct CommitReceipt {
    intent: EditIntent,
    previous: Revision,
    committed: Revision,
}

impl CommitReceipt {
    /// Creates a receipt for the next revision, or fails on overflow.
    pub fn new(intent: EditIntent, previous: Revision) -> Result<Self, RevisionOverflow> {
        Ok(Self {
            intent,
            previous,
            committed: previous.next()?,
        })
    }

    /// Returns the committed intent.
    pub const fn intent(self) -> EditIntent {
        self.intent
    }
    /// Returns the committed transaction identifier.
    pub const fn txn(self) -> TxnId {
        self.intent.txn()
    }
    /// Returns the revision before the commit.
    pub const fn previous(self) -> Revision {
        self.previous
    }
    /// Returns the revision after the commit.
    pub const fn committed(self) -> Revision {
        self.committed
    }
}

// The enum, exhaustive slice, and display strings share one list.
macro_rules! reject_reasons {
    ($( $variant:ident => ($doc:literal, $display:literal) ),+ $(,)?) => {
        #[doc = "The reason a submission was rejected."]
        #[doc = ""]
        #[doc = "`ALL` contains each variant exactly once: both the enum and"]
        #[doc = "the slice are generated from the same list of variants."]
        #[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum RejectReason {
            $(#[doc = $doc] $variant),+
        }

        impl RejectReason {
            #[doc = "All rejection reasons, once each, in declaration order."]
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }

        impl fmt::Display for RejectReason {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(match self {
                    $(Self::$variant => $display),+
                })
            }
        }
    };
}

reject_reasons! {
    Conflict => ("The edit conflicts with another edit.", "conflict"),
    RevisionMismatch => ("The supplied revision does not match.", "revision mismatch"),
    BaseExpired => ("The base revision is no longer available.", "base expired"),
    TxnIdReuse => ("A transaction identifier was reused.", "transaction ID reuse"),
    PermissionDenied => ("The actor lacks permission.", "permission denied"),
    UnknownBuffer => ("The buffer is unknown.", "unknown buffer"),
    BufferClosed => ("The buffer is closed.", "buffer closed"),
    UnknownActor => ("The actor is unknown.", "unknown actor"),
    RevokedActor => ("The actor's access was revoked.", "revoked actor"),
    InvalidInput => ("The input is invalid.", "invalid input"),
}

/// The result of submitting an intent.
///
/// # Laws
///
/// A committed outcome yields its receipt and its committed revision. A no-op
/// or rejection yields no receipt and returns its stored observed revision:
/// the accessors match on the variant and do not advance that revision.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// The edit advanced the revision.
    Committed(CommitReceipt),
    /// The edit made no change at the observed revision.
    NoOp {
        intent: EditIntent,
        revision: Revision,
    },
    /// The edit was rejected at the observed revision.
    Rejected {
        intent: EditIntent,
        revision: Revision,
        reason: RejectReason,
    },
}

impl SubmitOutcome {
    /// Returns the submitted intent.
    pub const fn intent(&self) -> EditIntent {
        match *self {
            Self::Committed(receipt) => receipt.intent(),
            Self::NoOp { intent, .. } | Self::Rejected { intent, .. } => intent,
        }
    }

    /// Returns the committed or observed revision.
    pub const fn resulting_revision(&self) -> Revision {
        match *self {
            Self::Committed(receipt) => receipt.committed(),
            Self::NoOp { revision, .. } | Self::Rejected { revision, .. } => revision,
        }
    }

    /// Returns a receipt only when the edit committed.
    pub const fn committed_receipt(&self) -> Option<CommitReceipt> {
        match *self {
            Self::Committed(receipt) => Some(receipt),
            Self::NoOp { .. } | Self::Rejected { .. } => None,
        }
    }

    /// Returns whether the edit committed.
    pub const fn is_committed(&self) -> bool {
        matches!(self, Self::Committed(_))
    }
    /// Returns whether the edit made no change.
    pub const fn is_noop(&self) -> bool {
        matches!(self, Self::NoOp { .. })
    }
    /// Returns whether the edit was rejected.
    pub const fn is_rejected(&self) -> bool {
        matches!(self, Self::Rejected { .. })
    }
    /// Returns the rejection reason only for a rejection.
    pub const fn reject_reason(&self) -> Option<RejectReason> {
        match *self {
            Self::Rejected { reason, .. } => Some(reason),
            Self::Committed(_) | Self::NoOp { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::edit::ids::{ActorId, Nonce};
    use crate::editor::test_support::Rng;
    use std::collections::HashSet;

    fn check(intent: EditIntent, revision: Revision) {
        let receipt = CommitReceipt::new(intent, revision);
        if revision == Revision::MAX {
            assert_eq!(receipt, Err(RevisionOverflow));
        } else {
            let receipt = receipt.unwrap();
            assert_eq!(receipt.intent(), intent);
            assert_eq!(receipt.txn(), intent.txn());
            assert_eq!(receipt.previous(), revision);
            assert_eq!(receipt.committed(), revision.next().unwrap());
            assert!(receipt.committed() > revision);
            let outcome = SubmitOutcome::Committed(receipt);
            assert_eq!(outcome.intent(), intent);
            assert_eq!(outcome.resulting_revision(), receipt.committed());
            assert_eq!(outcome.committed_receipt(), Some(receipt));
            assert!(outcome.is_committed());
            assert!(!outcome.is_noop() && !outcome.is_rejected());
            assert_eq!(outcome.reject_reason(), None);
        }
        let noop = SubmitOutcome::NoOp { intent, revision };
        assert_eq!(noop.intent(), intent);
        assert_eq!(noop.resulting_revision(), revision);
        assert_eq!(noop.committed_receipt(), None);
        assert!(noop.is_noop());
        assert!(!noop.is_committed() && !noop.is_rejected());
        assert_eq!(noop.reject_reason(), None);
        for &reason in RejectReason::ALL {
            let rejected = SubmitOutcome::Rejected {
                intent,
                revision,
                reason,
            };
            assert_eq!(rejected.intent(), intent);
            assert_eq!(rejected.resulting_revision(), revision);
            assert_eq!(rejected.committed_receipt(), None);
            assert!(rejected.is_rejected());
            assert!(!rejected.is_committed() && !rejected.is_noop());
            assert_eq!(rejected.reject_reason(), Some(reason));
        }
    }

    #[test]
    fn boundary_and_random_outcome_laws() {
        for value in [0, 1, u64::MAX - 1, u64::MAX] {
            check(
                EditIntent::for_actor(ActorId::new(1), Nonce::new(2), Revision::new(3)),
                Revision::new(value),
            );
        }
        for seed in [0, 1, 42, u64::MAX] {
            let mut rng = Rng::new(seed);
            for _ in 0..1024 {
                let intent = EditIntent::for_actor(
                    ActorId::new(rng.next_u64()),
                    Nonce::new(rng.next_u64()),
                    Revision::new(rng.next_u64()),
                );
                check(intent, Revision::new(rng.next_u64()));
            }
        }
    }

    #[test]
    fn all_reasons_are_unique_and_exhaustive() {
        let expected = [
            RejectReason::Conflict,
            RejectReason::RevisionMismatch,
            RejectReason::BaseExpired,
            RejectReason::TxnIdReuse,
            RejectReason::PermissionDenied,
            RejectReason::UnknownBuffer,
            RejectReason::BufferClosed,
            RejectReason::UnknownActor,
            RejectReason::RevokedActor,
            RejectReason::InvalidInput,
        ];
        assert_eq!(RejectReason::ALL, expected);
        assert_eq!(
            RejectReason::ALL
                .iter()
                .copied()
                .collect::<HashSet<_>>()
                .len(),
            expected.len()
        );
        for (reason, text) in RejectReason::ALL.iter().zip([
            "conflict",
            "revision mismatch",
            "base expired",
            "transaction ID reuse",
            "permission denied",
            "unknown buffer",
            "buffer closed",
            "unknown actor",
            "revoked actor",
            "invalid input",
        ]) {
            assert_eq!(reason.to_string(), text);
        }
    }
}
