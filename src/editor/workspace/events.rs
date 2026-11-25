//! Atomic, revisioned projections of committed workspace edits.
//!
//! Each new commit produces one event containing the applied (possibly
//! rebased) transaction. Rejections, no-ops and idempotent retries produce
//! none. Patches are ordered by buffer ID by `Transaction`.

use crate::editor::{
    collab::{
        receipt::{CommitReceipt, SubmitOutcome},
        sequencer::Sequencer,
    },
    edit::{
        ids::BufferId,
        patch::Patch,
        transaction::{Transaction, TransactionError},
    },
    workspace::{catalog::BufferCatalog, version::Revision},
};

/// All buffer patches published together at one committed revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceEvent {
    receipt: CommitReceipt,
    applied: Transaction,
}

impl WorkspaceEvent {
    /// Projects a receipt already recorded by this sequencer, if its history remains available.
    /// Callers consuming receipts are responsible for deduplicating retries.
    pub fn from_receipt(sequencer: &Sequencer, receipt: CommitReceipt) -> Option<Self> {
        if sequencer.journal().lookup_txn(receipt.txn()) != Some(&receipt) {
            return None;
        }
        let entry = sequencer.history().get_by_revision(receipt.committed())?;
        if entry.txn_id() != receipt.txn() || entry.previous() != receipt.previous() {
            return None;
        }
        Some(Self {
            receipt,
            applied: entry.forward().clone(),
        })
    }

    /// The receipt of the commit represented by this event.
    pub fn receipt(&self) -> CommitReceipt {
        self.receipt
    }

    /// The exact revision published by the commit.
    pub fn revision(&self) -> Revision {
        self.receipt.committed()
    }

    /// Applied patches in ascending buffer ID order, in previous-revision coordinates.
    pub fn patches(&self) -> &[(BufferId, Patch)] {
        self.applied.patches()
    }

    /// Replays the event against the catalog at its previous revision.
    /// The caller is responsible for matching the catalog to that revision.
    pub fn apply_to_catalog(
        &self,
        catalog: &BufferCatalog,
    ) -> Result<BufferCatalog, TransactionError> {
        self.applied.apply_to_catalog(catalog)
    }
}

/// Submits an edit and projects a newly published commit into one event.
///
/// Identical retries return their original committed outcome but no new event.
/// Rebased commits project the actual history entry, not the submitted patches.
pub fn submit_with_event(
    sequencer: &mut Sequencer,
    transaction: Transaction,
) -> (SubmitOutcome, Option<WorkspaceEvent>) {
    let previous = sequencer.revision();
    let outcome = sequencer.submit(transaction);
    let event = outcome.committed_receipt().and_then(|receipt| {
        (sequencer.revision() > previous).then(|| {
            WorkspaceEvent::from_receipt(sequencer, receipt)
                .expect("new commit has a journal receipt and history entry")
        })
    });
    (outcome, event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::{registry::ActorRegistry, role::ActorRole},
        edit::{
            ids::{ActorId, Nonce},
            intent::EditIntent,
            splice::Splice,
        },
        test_support::{assert_law, Rng},
        text::{span::ByteSpan, utf8::Text},
        workspace::catalog::BufferCatalog,
    };

    fn setup() -> Sequencer {
        let mut catalog = BufferCatalog::new();
        for id in 0..3 {
            catalog
                .register(BufferId::new(id), Text::from_str("abcd"))
                .unwrap();
        }
        let mut actors = ActorRegistry::new();
        actors.register(ActorId::new(1), ActorRole::Human).unwrap();
        Sequencer::new(catalog, actors)
    }

    fn txn(base: Revision, nonce: u64, edits: &[(u64, usize, &str)]) -> Transaction {
        let source = Text::from_str("abcd");
        let patches = edits
            .iter()
            .map(|&(id, pos, value)| {
                let span = ByteSpan::validate_in(&source, pos, pos + 1).unwrap();
                (
                    BufferId::new(id),
                    Patch::new(vec![Splice::new(span, Text::from_str(value))]).unwrap(),
                )
            })
            .collect();
        Transaction::new(
            EditIntent::for_actor(ActorId::new(1), Nonce::new(nonce), base),
            patches,
        )
        .unwrap()
    }

    #[test]
    fn deterministic_commits_replay_and_group_buffers() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| {
                (0..128)
                    .map(|_| (rng.next_u64(), rng.next_u64()))
                    .collect::<Vec<_>>()
            },
            |choices| {
                let mut sequencer = setup();
                let mut replay = sequencer.snapshot().catalog().clone();
                let mut last = sequencer.revision();
                for (nonce, &(choice, value)) in choices.iter().enumerate() {
                    let before_rejection = sequencer.snapshot();
                    let invalid = txn(last, (nonce as u64) + 1000, &[(3 + choice % 1000, 0, "x")]);
                    let (rejected, absent) = submit_with_event(&mut sequencer, invalid);
                    assert!(rejected.is_rejected());
                    assert_eq!(absent, None);
                    assert_eq!(sequencer.snapshot(), before_rejection);
                    let first = choice % 3;
                    let second = (first + 1) % 3;
                    let replacement = if value & 1 == 0 { "x" } else { "y" };
                    let transaction = txn(
                        last,
                        nonce as u64,
                        &[(second, 0, replacement), (first, 0, replacement)],
                    );
                    let before = sequencer.snapshot();
                    let (outcome, event) = submit_with_event(&mut sequencer, transaction.clone());
                    if let Some(event) = event {
                        assert_eq!(outcome.committed_receipt(), Some(event.receipt()));
                        assert_eq!(event.revision(), sequencer.revision());
                        assert_eq!(event.receipt().previous(), last);
                        assert!(event.revision() > last);
                        assert_eq!(event.patches().len(), 2);
                        assert!(event.patches()[0].0 < event.patches()[1].0);
                        replay = event.apply_to_catalog(&replay).unwrap();
                        assert_eq!(&replay, sequencer.snapshot().catalog());
                        last = event.revision();
                        let (retry, duplicate) = submit_with_event(&mut sequencer, transaction);
                        assert_eq!(retry, outcome);
                        assert_eq!(duplicate, None);
                    } else {
                        assert!(outcome.is_noop());
                        assert_eq!(sequencer.snapshot(), before);
                    }
                }
            },
        );
    }

    #[test]
    fn rejected_noop_and_rebased_commits() {
        let mut s = setup();
        let mut replay = s.snapshot().catalog().clone();
        let stale = txn(Revision::ZERO, 2, &[(0, 3, "z")]);
        let (outcome, event) = submit_with_event(&mut s, txn(Revision::ZERO, 1, &[(0, 0, "long")]));
        assert!(outcome.is_committed());
        replay = event.unwrap().apply_to_catalog(&replay).unwrap();
        let (outcome, event) = submit_with_event(&mut s, stale);
        assert!(outcome.is_committed());
        let event = event.unwrap();
        assert_eq!(
            WorkspaceEvent::from_receipt(&s, event.receipt()),
            Some(event.clone())
        );
        replay = event.apply_to_catalog(&replay).unwrap();
        assert_eq!(&replay, s.snapshot().catalog());
        let before = s.snapshot();
        let revision = s.revision();
        let (outcome, event) = submit_with_event(&mut s, txn(revision, 3, &[(9, 0, "x")]));
        assert!(outcome.is_rejected());
        assert_eq!(event, None);
        assert_eq!(s.snapshot(), before);
        let (outcome, event) = submit_with_event(
            &mut s,
            Transaction::new(
                EditIntent::for_actor(ActorId::new(1), Nonce::new(4), revision),
                vec![],
            )
            .unwrap(),
        );
        assert!(outcome.is_noop());
        assert_eq!(event, None);
        assert_eq!(s.snapshot(), before);
    }

    #[test]
    fn projects_existing_receipts_only() {
        let mut s = setup();
        let transaction = txn(Revision::ZERO, 20, &[(1, 0, "x"), (0, 0, "y")]);
        let fake = CommitReceipt::new(transaction.intent(), Revision::ZERO).unwrap();
        assert_eq!(WorkspaceEvent::from_receipt(&s, fake), None);
        let before = s.snapshot();
        let receipt = s.submit(transaction).committed_receipt().unwrap();
        let event = WorkspaceEvent::from_receipt(&s, receipt).unwrap();
        assert_eq!(event.receipt(), receipt);
        assert_eq!(event.revision(), s.revision());
        assert_eq!(event.patches().len(), 2);
        assert_eq!(
            event.apply_to_catalog(before.catalog()).unwrap(),
            *s.snapshot().catalog()
        );
        let wrong_revision = CommitReceipt::new(receipt.intent(), receipt.committed()).unwrap();
        assert_eq!(WorkspaceEvent::from_receipt(&s, wrong_revision), None);
    }
}
