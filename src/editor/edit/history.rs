//! Append-only records of committed transactions.
//!
//! # Laws
//!
//! * Forward replay: admission computes the forward transaction on the supplied
//!   previous snapshot and compares the entire result to the committed catalog.
//!   Thus replay gives that catalog. Consecutive admissions also compare the
//!   next previous catalog with the last committed catalog, so induction gives
//!   the same result for every contiguous slice.
//! * Inverse restoration: admission saves the inverse computed from that exact
//!   snapshot. The transaction inverse law restores each affected text, and
//!   untouched catalog entries remain unchanged. Induction in reverse order
//!   extends this to every contiguous slice.
//! * Identity: a record is appended for each accepted revision and transaction
//!   ID, irrespective of content equality. Returning to old text therefore
//!   cannot collapse records or identify their revisions or transaction IDs.
//! * Chain and provenance: admission checks the previous revision, the intent's
//!   base, the successor revision, the previous catalog, and uniqueness of the
//!   transaction ID before changing any state. Entries retain the original
//!   transaction and intent; lookups return those same stored entries.

use std::fmt;

use crate::editor::edit::{
    ids::{ActorId, TxnId},
    intent::EditIntent,
    transaction::{Transaction, TransactionError},
};
use crate::editor::workspace::{
    catalog::BufferCatalog, snapshots::WorkspaceSnapshot, version::Revision,
};

/// One verified transition, including its original transaction and inverse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    previous: Revision,
    committed: Revision,
    forward: Transaction,
    inverse: Transaction,
}

impl HistoryEntry {
    /// The revision on which the edit was based.
    pub fn previous(&self) -> Revision {
        self.previous
    }
    /// The revision at which the edit was committed.
    pub fn committed(&self) -> Revision {
        self.committed
    }
    /// The original transaction and its forward patches.
    pub fn forward(&self) -> &Transaction {
        &self.forward
    }
    /// The exact inverse patches, based at the committed revision.
    pub fn inverse(&self) -> &Transaction {
        &self.inverse
    }
    /// The original edit intent.
    pub fn intent(&self) -> EditIntent {
        self.forward.intent()
    }
    /// The original transaction identifier.
    pub fn txn_id(&self) -> TxnId {
        self.intent().txn()
    }
    /// The actor that originated the transaction.
    pub fn actor(&self) -> ActorId {
        self.intent().actor()
    }
}

/// A proposed record failed validation without changing history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryError {
    /// The previous revision does not continue the recorded chain.
    NonContiguous {
        expected: Revision,
        actual: Revision,
    },
    /// The transaction intent has a different base from the previous snapshot.
    BaseMismatch {
        expected: Revision,
        actual: Revision,
    },
    /// The committed revision is not the successor of the previous revision.
    CommittedMismatch {
        expected: Revision,
        actual: Revision,
    },
    /// There is no successor revision.
    RevisionOverflow,
    /// A transaction identifier was already recorded.
    DuplicateTxnId(TxnId),
    /// The previous catalog differs from the last committed catalog.
    PreviousCatalogMismatch,
    /// The supplied committed catalog differs from the transaction result.
    CommittedCatalogMismatch,
    /// The transaction could not be evaluated.
    Transaction(TransactionError),
}

impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonContiguous { expected, actual } => {
                write!(f, "expected previous revision {expected}, got {actual}")
            }
            Self::BaseMismatch { expected, actual } => {
                write!(f, "expected transaction base {expected}, got {actual}")
            }
            Self::CommittedMismatch { expected, actual } => {
                write!(f, "expected committed revision {expected}, got {actual}")
            }
            Self::RevisionOverflow => f.write_str("workspace revision overflow"),
            Self::DuplicateTxnId(id) => write!(f, "duplicate transaction {id}"),
            Self::PreviousCatalogMismatch => f.write_str("previous catalog differs from history"),
            Self::CommittedCatalogMismatch => {
                f.write_str("committed catalog differs from transaction result")
            }
            Self::Transaction(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for HistoryError {}

/// Append-only committed edits; this type does not itself publish a workspace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EditHistory {
    entries: Vec<HistoryEntry>,
    last_catalog: Option<BufferCatalog>,
}

impl EditHistory {
    /// Creates an empty history, whose first entry may start at any revision.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the entries in commit order.
    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// Looks up an entry by its committed revision.
    pub fn get_by_revision(&self, revision: Revision) -> Option<&HistoryEntry> {
        self.entries
            .iter()
            .find(|entry| entry.committed == revision)
    }

    /// Looks up an entry by its originating transaction identifier.
    pub fn get_by_txn_id(&self, id: TxnId) -> Option<&HistoryEntry> {
        self.entries.iter().find(|entry| entry.txn_id() == id)
    }

    /// Validates and records an already committed transition.
    ///
    /// `committed_catalog` is checked against the computed result; the caller
    /// must ensure that the given revision and catalog were actually published.
    /// Even a content-preserving commit must explicitly advance the revision.
    pub fn record(
        &mut self,
        previous: &WorkspaceSnapshot,
        committed: Revision,
        committed_catalog: &BufferCatalog,
        forward: Transaction,
    ) -> Result<(), HistoryError> {
        let base = previous.revision();
        if let Some(last) = self.entries.last() {
            if last.committed != base {
                return Err(HistoryError::NonContiguous {
                    expected: last.committed,
                    actual: base,
                });
            }
            if self.last_catalog.as_ref() != Some(previous.catalog()) {
                return Err(HistoryError::PreviousCatalogMismatch);
            }
        }
        if forward.intent().base() != base {
            return Err(HistoryError::BaseMismatch {
                expected: base,
                actual: forward.intent().base(),
            });
        }
        let expected = base.next().map_err(|_| HistoryError::RevisionOverflow)?;
        if committed != expected {
            return Err(HistoryError::CommittedMismatch {
                expected,
                actual: committed,
            });
        }
        if self.get_by_txn_id(forward.intent().txn()).is_some() {
            return Err(HistoryError::DuplicateTxnId(forward.intent().txn()));
        }
        let result = forward
            .apply_with_inverse(previous)
            .map_err(HistoryError::Transaction)?;
        if result.catalog() != committed_catalog {
            return Err(HistoryError::CommittedCatalogMismatch);
        }
        // No-op transactions receive their old base from transaction evaluation;
        // a recorded commit always advances, so rebase their inverse as well.
        let inverse = Transaction::new(
            result.inverse().intent().with_base(committed),
            result.inverse().patches().to_vec(),
        )
        .map_err(HistoryError::Transaction)?;
        let entry = HistoryEntry {
            previous: base,
            committed,
            forward,
            inverse,
        };
        self.last_catalog = Some(committed_catalog.clone());
        self.entries.push(entry);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::edit::{
        ids::{BufferId, Nonce},
        patch::Patch,
        splice::Splice,
    };
    use crate::editor::text::{span::ByteSpan, utf8::Text};
    use crate::editor::workspace::snapshots::VersionedWorkspace;

    fn catalog() -> BufferCatalog {
        let mut catalog = BufferCatalog::new();
        for id in 0..2 {
            catalog
                .register(BufferId::new(id), Text::from_str("ab"))
                .unwrap();
        }
        catalog
    }

    fn snapshot(catalog: &BufferCatalog, revision: u64) -> WorkspaceSnapshot {
        let mut workspace = VersionedWorkspace::new(catalog.clone());
        for _ in 0..revision {
            let text = workspace.catalog().get(BufferId::new(0)).unwrap().clone();
            workspace.update(BufferId::new(0), text).unwrap();
        }
        workspace.capture()
    }

    fn transaction(previous: &WorkspaceSnapshot, nonce: u64, choices: usize) -> Transaction {
        let mut patches = Vec::new();
        for id in 0..2 {
            let buffer = BufferId::new(id);
            let text = previous.catalog().get(buffer).unwrap();
            let replacement = ["", "ab", "界"][(choices / 3_usize.pow(id as u32)) % 3];
            patches.push((
                buffer,
                Patch::new(vec![Splice::new(
                    ByteSpan::full_of(text),
                    Text::from_str(replacement),
                )])
                .unwrap(),
            ));
        }
        Transaction::new(
            EditIntent::for_actor(
                ActorId::new(nonce % 2),
                Nonce::new(nonce),
                previous.revision(),
            ),
            patches,
        )
        .unwrap()
    }

    #[test]
    fn generated_histories_replay_and_restore_every_slice() {
        for sequence in 0..9_usize.pow(3) {
            let mut history = EditHistory::new();
            let mut catalogs = vec![catalog()];
            let mut choices = sequence;
            for step in 0..3 {
                let previous = snapshot(catalogs.last().unwrap(), step);
                let txn = transaction(&previous, step, choices % 9);
                choices /= 9;
                let committed = txn.apply_with_inverse(&previous).unwrap().catalog().clone();
                history
                    .record(&previous, Revision::new(step + 1), &committed, txn.clone())
                    .unwrap();
                let entry = history.entries().last().unwrap();
                assert_eq!(entry.intent(), txn.intent());
                assert_eq!(entry.actor(), txn.intent().actor());
                assert_eq!(history.get_by_revision(entry.committed()), Some(entry));
                assert_eq!(history.get_by_txn_id(txn.intent().txn()), Some(entry));
                assert_eq!(entry.previous(), Revision::new(step));
                assert_eq!(entry.inverse().intent().base(), entry.committed());
                catalogs.push(committed);
            }
            for start in 0..=3 {
                for end in start..=3 {
                    let mut forward = catalogs[start].clone();
                    for entry in &history.entries()[start..end] {
                        forward = entry.forward().apply_to_catalog(&forward).unwrap();
                    }
                    assert_eq!(forward, catalogs[end]);
                    for entry in history.entries()[start..end].iter().rev() {
                        forward = entry.inverse().apply_to_catalog(&forward).unwrap();
                    }
                    assert_eq!(forward, catalogs[start]);
                }
            }
            for entry in history.entries() {
                let by_revision = history.get_by_revision(entry.committed()).unwrap();
                let by_txn = history.get_by_txn_id(entry.txn_id()).unwrap();
                assert_eq!(by_revision, entry);
                assert_eq!(by_txn, entry);
                assert_eq!(by_txn.actor(), entry.intent().actor());
                assert_eq!(by_revision.intent(), entry.intent());
            }
            let saved = history.clone();
            let previous = snapshot(catalogs.last().unwrap(), 3);
            let fresh = transaction(&previous, 20, 0);
            let committed = fresh
                .apply_with_inverse(&previous)
                .unwrap()
                .catalog()
                .clone();
            for entry in saved.entries() {
                let duplicate = Transaction::new(
                    EditIntent::new(entry.txn_id(), previous.revision()),
                    fresh.patches().to_vec(),
                )
                .unwrap();
                assert_eq!(
                    history.record(&previous, Revision::new(4), &committed, duplicate),
                    Err(HistoryError::DuplicateTxnId(entry.txn_id()))
                );
                assert_eq!(history, saved);
            }
            let skipped = snapshot(catalogs.last().unwrap(), 4);
            assert!(matches!(
                history.record(
                    &skipped,
                    Revision::new(5),
                    &committed,
                    transaction(&skipped, 21, 0)
                ),
                Err(HistoryError::NonContiguous { .. })
            ));
            assert_eq!(history, saved);
            let wrong_base = Transaction::new(
                fresh.intent().with_base(Revision::new(2)),
                fresh.patches().to_vec(),
            )
            .unwrap();
            assert!(matches!(
                history.record(&previous, Revision::new(4), &committed, wrong_base),
                Err(HistoryError::BaseMismatch { .. })
            ));
            assert_eq!(history, saved);
            assert!(matches!(
                history.record(&previous, Revision::new(5), &committed, fresh),
                Err(HistoryError::CommittedMismatch { .. })
            ));
            assert_eq!(history, saved);
        }
    }

    #[test]
    fn returning_to_old_content_does_not_collapse_commits() {
        let initial = catalog();
        let first = snapshot(&initial, 0);
        let txn = transaction(&first, 0, 0);
        let empty = txn.apply_with_inverse(&first).unwrap().catalog().clone();
        let mut history = EditHistory::new();
        history
            .record(&first, Revision::new(1), &empty, txn)
            .unwrap();
        let second = snapshot(&empty, 1);
        let txn = transaction(&second, 1, 4);
        let restored = txn.apply_with_inverse(&second).unwrap().catalog().clone();
        assert_eq!(restored, initial);
        history
            .record(&second, Revision::new(2), &restored, txn)
            .unwrap();
        assert_ne!(
            history.entries()[0].committed(),
            history.entries()[1].committed()
        );
        assert_ne!(history.entries()[0].txn_id(), history.entries()[1].txn_id());
        assert_eq!(history.entries().len(), 2);
        let third = snapshot(&restored, 2);
        let old_id = history.entries()[0].txn_id();
        let duplicate = Transaction::new(
            EditIntent::new(old_id, third.revision()),
            transaction(&third, 3, 0).patches().to_vec(),
        )
        .unwrap();
        let saved = history.clone();
        assert_eq!(
            history.record(&third, Revision::new(3), &empty, duplicate),
            Err(HistoryError::DuplicateTxnId(old_id))
        );
        assert_eq!(history, saved);
    }

    #[test]
    fn rejected_records_are_atomic() {
        let original = catalog();
        let first = snapshot(&original, 0);
        let txn = transaction(&first, 0, 0);
        let next = txn.apply_with_inverse(&first).unwrap().catalog().clone();
        let mut history = EditHistory::new();
        history
            .record(&first, Revision::new(1), &next, txn.clone())
            .unwrap();
        let saved = history.clone();
        let second = snapshot(&next, 1);
        let good = transaction(&second, 1, 4);
        let result = good.apply_with_inverse(&second).unwrap().catalog().clone();
        let skipped = snapshot(&next, 2);
        let skipped_txn = transaction(&skipped, 4, 0);
        assert!(matches!(
            history.record(&skipped, Revision::new(3), &next, skipped_txn),
            Err(HistoryError::NonContiguous { .. })
        ));
        assert_eq!(history, saved);
        assert!(matches!(
            history.record(&first, Revision::new(1), &next, txn.clone()),
            Err(HistoryError::NonContiguous { .. })
        ));
        assert!(matches!(
            history.record(&second, Revision::new(2), &result, txn),
            Err(HistoryError::BaseMismatch { .. })
        ));
        assert!(matches!(
            history.record(&second, Revision::new(3), &result, good.clone()),
            Err(HistoryError::CommittedMismatch { .. })
        ));
        let duplicate = Transaction::new(
            EditIntent::new(saved.entries()[0].txn_id(), second.revision()),
            good.patches().to_vec(),
        )
        .unwrap();
        assert!(matches!(
            history.record(&second, Revision::new(2), &result, duplicate),
            Err(HistoryError::DuplicateTxnId(_))
        ));
        assert_eq!(
            history.record(&second, Revision::new(2), &next, good.clone()),
            Err(HistoryError::CommittedCatalogMismatch)
        );
        let foreign = snapshot(&original, 1);
        assert_eq!(
            history.record(&foreign, Revision::new(2), &result, good),
            Err(HistoryError::PreviousCatalogMismatch)
        );
        assert_eq!(history, saved);
    }

    #[test]
    fn inverse_retains_exact_separated_splices() {
        let mut original = catalog();
        let id = BufferId::new(0);
        let text = Text::from_str("abcdef");
        original.update(id, text.clone()).unwrap();
        let previous = snapshot(&original, 0);
        let patch = Patch::new(vec![
            Splice::new(
                ByteSpan::validate_in(&text, 1, 2).unwrap(),
                Text::from_str("界"),
            ),
            Splice::new(
                ByteSpan::validate_in(&text, 4, 5).unwrap(),
                Text::from_str(""),
            ),
        ])
        .unwrap();
        let txn = Transaction::new(
            EditIntent::for_actor(ActorId::new(1), Nonce::new(1), Revision::ZERO),
            vec![(id, patch)],
        )
        .unwrap();
        let committed = txn.apply_with_inverse(&previous).unwrap().catalog().clone();
        let after = committed.get(id).unwrap();
        assert_eq!(after.as_str(), "a界cdf");
        let expected = Patch::new(vec![
            Splice::new(
                ByteSpan::validate_in(after, 1, 4).unwrap(),
                Text::from_str("b"),
            ),
            Splice::new(
                ByteSpan::validate_in(after, 6, 6).unwrap(),
                Text::from_str("e"),
            ),
        ])
        .unwrap();
        let mut history = EditHistory::new();
        history
            .record(&previous, Revision::new(1), &committed, txn)
            .unwrap();
        assert_eq!(history.entries()[0].inverse().patches(), &[(id, expected)]);
        assert_eq!(
            history.entries()[0]
                .inverse()
                .apply_to_catalog(&committed)
                .unwrap(),
            original
        );
    }
}
