//! Single-order admission of collaborative edits.
//!
//! # Laws
//!
//! Each first, valid, effective submission is prepared against the current
//! snapshot, then published by one atomic batch at the successor revision.
//! Its snapshot, journal receipt and forward/inverse history follow that same
//! transition. Preparation and validation precede publication, so rejection
//! and effective no-ops leave all four structures unchanged.
//! Committed transaction identities retain their original submissions: exact
//! retries return the original receipt and changed reuse is rejected, even
//! after revocation or pruning. Other submissions require an active actor and
//! a retained base. Each intervening history entry and pair of retained
//! snapshots transports disjoint stale edits into the next coordinate frame;
//! overlapping footprints reject. Actor roles are never consulted, making the
//! transition and receipt independent of whether an active actor is human or
//! agent. Induction on submissions gives these properties for any sequence.

use std::collections::BTreeMap;

use super::{
    journal::CommitJournal,
    rebase::{rebase_over_step, RebaseError, RebaseStep},
    receipt::{CommitReceipt, RejectReason, SubmitOutcome},
    registry::ActorRegistry,
};
use crate::editor::{
    edit::{
        history::EditHistory,
        ids::TxnId,
        transaction::{Transaction, TransactionError},
    },
    workspace::{
        catalog::BufferCatalog,
        snapshots::{SnapshotStore, VersionedWorkspace, WorkspaceSnapshot},
        version::Revision,
    },
};

/// Owns the authoritative workspace and its ordered commit records.
#[derive(Debug)]
pub struct Sequencer {
    actors: ActorRegistry,
    workspace: VersionedWorkspace,
    snapshots: SnapshotStore,
    journal: CommitJournal,
    history: EditHistory,
    submitted: BTreeMap<TxnId, Transaction>,
}

impl Sequencer {
    /// Starts at revision zero with the supplied catalog and actor registry.
    pub fn new(catalog: BufferCatalog, actors: ActorRegistry) -> Self {
        let workspace = VersionedWorkspace::new(catalog);
        let mut snapshots = SnapshotStore::new();
        snapshots
            .record(workspace.capture())
            .expect("initial snapshot");
        Self {
            actors,
            workspace,
            snapshots,
            journal: CommitJournal::default(),
            history: EditHistory::new(),
            submitted: BTreeMap::new(),
        }
    }

    /// Returns the current immutable capture.
    pub fn snapshot(&self) -> WorkspaceSnapshot {
        self.workspace.capture()
    }
    /// Returns the current revision.
    pub fn revision(&self) -> Revision {
        self.workspace.revision()
    }
    /// Returns retained captures.
    pub fn snapshots(&self) -> &SnapshotStore {
        &self.snapshots
    }
    /// Returns the commit journal.
    pub fn journal(&self) -> &CommitJournal {
        &self.journal
    }
    /// Returns the edit history.
    pub fn history(&self) -> &EditHistory {
        &self.history
    }
    /// Returns the actor registry.
    pub fn actors(&self) -> &ActorRegistry {
        &self.actors
    }
    /// Allows actor registration and revocation independently of edit submission.
    pub fn actors_mut(&mut self) -> &mut ActorRegistry {
        &mut self.actors
    }
    /// Removes retained snapshots older than the floor.
    pub fn prune_before(&mut self, floor: Revision) {
        self.snapshots.prune_before(floor);
    }

    /// Submits an edit, returning the original receipt for an identical retry.
    pub fn submit(&mut self, txn: Transaction) -> SubmitOutcome {
        let intent = txn.intent();
        let revision = self.revision();
        let reject = |reason| SubmitOutcome::Rejected {
            intent,
            revision,
            reason,
        };
        if let Some(original) = self.submitted.get(&intent.txn()) {
            return if *original == txn {
                SubmitOutcome::Committed(
                    *self
                        .journal
                        .lookup_txn(intent.txn())
                        .expect("committed submission has receipt"),
                )
            } else {
                reject(RejectReason::TxnIdReuse)
            };
        }
        if self.actors.is_revoked(intent.actor()) {
            return reject(RejectReason::RevokedActor);
        }
        if !self.actors.is_active(intent.actor()) {
            return reject(RejectReason::UnknownActor);
        }
        if intent.base() > revision {
            return reject(RejectReason::RevisionMismatch);
        }
        let Some(base) = self.snapshots.get(intent.base()) else {
            return reject(RejectReason::BaseExpired);
        };
        if let Err(error) = txn.apply_with_inverse(base) {
            return reject(transaction_reason(error));
        }
        let mut forward = txn.clone();
        let mut cursor = intent.base();
        while cursor < revision {
            let Some(before) = self.snapshots.get(cursor) else {
                return reject(RejectReason::BaseExpired);
            };
            let next = cursor.next().expect("cursor below current revision");
            let Some(after) = self.snapshots.get(next) else {
                return reject(RejectReason::BaseExpired);
            };
            let entry = self
                .history
                .get_by_revision(next)
                .expect("contiguous history");
            forward = match rebase_over_step(
                &forward,
                RebaseStep {
                    before,
                    committed: entry.forward(),
                    after,
                },
            ) {
                Ok(value) => value,
                Err(RebaseError::Conflict(_)) => return reject(RejectReason::Conflict),
                Err(RebaseError::Transaction(error)) => return reject(transaction_reason(error)),
                Err(_) => return reject(RejectReason::InvalidInput),
            };
            cursor = next;
        }
        let previous = self.workspace.capture();
        let result = match forward.apply_with_inverse(&previous) {
            Ok(result) => result,
            Err(error) => return reject(transaction_reason(error)),
        };
        if result.is_noop() {
            return SubmitOutcome::NoOp { intent, revision };
        }
        let receipt = match CommitReceipt::new(intent, revision) {
            Ok(receipt) => receipt,
            Err(_) => return reject(RejectReason::RevisionMismatch),
        };
        let mut history = self.history.clone();
        history
            .record(
                &previous,
                receipt.committed(),
                result.catalog(),
                forward.clone(),
            )
            .expect("prepared contiguous history");
        let mut journal = self.journal.clone();
        journal
            .append(receipt)
            .expect("prepared contiguous journal");
        let updates = forward
            .patches()
            .iter()
            .map(|(id, _)| {
                (
                    *id,
                    result.catalog().get(*id).expect("validated buffer").clone(),
                )
            })
            .collect::<Vec<_>>();
        self.workspace
            .update_batch(updates)
            .expect("prepared batch");
        self.snapshots
            .record(self.workspace.capture())
            .expect("new workspace revision");
        self.history = history;
        self.journal = journal;
        self.submitted.insert(intent.txn(), txn);
        SubmitOutcome::Committed(receipt)
    }
}

fn transaction_reason(error: TransactionError) -> RejectReason {
    match error {
        TransactionError::UnknownBuffer(_) => RejectReason::UnknownBuffer,
        TransactionError::BufferClosed(_) => RejectReason::BufferClosed,
        TransactionError::RevisionMismatch { .. } | TransactionError::RevisionOverflow => {
            RejectReason::RevisionMismatch
        }
        TransactionError::DuplicateBufferId(_) | TransactionError::Patch { .. } => {
            RejectReason::InvalidInput
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::role::ActorRole,
        edit::{
            ids::{ActorId, BufferId, Nonce},
            intent::EditIntent,
            patch::Patch,
            splice::Splice,
        },
        text::{span::ByteSpan, utf8::Text},
    };

    fn setup(role: ActorRole) -> Sequencer {
        let mut catalog = BufferCatalog::new();
        for n in 0..2 {
            catalog
                .register(BufferId::new(n), Text::from_str("abcd"))
                .unwrap();
        }
        let mut actors = ActorRegistry::new();
        actors.register(ActorId::new(1), role).unwrap();
        actors.register(ActorId::new(2), ActorRole::Agent).unwrap();
        Sequencer::new(catalog, actors)
    }

    fn transaction(
        actor: u64,
        nonce: u64,
        base: u64,
        buffer: u64,
        start: usize,
        end: usize,
        replacement: &str,
    ) -> Transaction {
        let span = ByteSpan::validate_in(&Text::from_str("0123456789"), start, end).unwrap();
        let patch = Patch::new(vec![Splice::new(span, Text::from_str(replacement))]).unwrap();
        Transaction::new(
            EditIntent::for_actor(ActorId::new(actor), Nonce::new(nonce), Revision::new(base)),
            vec![(BufferId::new(buffer), patch)],
        )
        .unwrap()
    }

    #[test]
    fn exhaustive_sequences_match_sequential_reference() {
        for mut sequence in 0..4_u32.pow(5) {
            let mut s = setup(ActorRole::Human);
            let mut reference = [String::from("abcd"), String::from("abcd")];
            let mut revision = 0;
            for nonce in 0..5 {
                let choice = sequence % 4;
                sequence /= 4;
                let buffer = (choice % 2) as usize;
                let value = if choice < 2 { "x" } else { "a" };
                let txn = transaction(
                    1 + u64::from(choice % 2),
                    nonce,
                    revision,
                    buffer as u64,
                    0,
                    1,
                    value,
                );
                let before = (
                    s.snapshot(),
                    s.snapshots().clone(),
                    s.journal().clone(),
                    s.history().clone(),
                );
                let outcome = s.submit(txn.clone());
                if &reference[buffer][..1] == value {
                    assert!(outcome.is_noop());
                    assert_eq!(
                        (
                            s.snapshot(),
                            s.snapshots().clone(),
                            s.journal().clone(),
                            s.history().clone()
                        ),
                        before
                    );
                } else {
                    reference[buffer].replace_range(0..1, value);
                    revision += 1;
                    assert_eq!(outcome.resulting_revision(), Revision::new(revision));
                    assert!(outcome.is_committed());
                    let after = (
                        s.snapshot(),
                        s.snapshots().clone(),
                        s.journal().clone(),
                        s.history().clone(),
                    );
                    assert_eq!(s.submit(txn), outcome);
                    assert_eq!(
                        (
                            s.snapshot(),
                            s.snapshots().clone(),
                            s.journal().clone(),
                            s.history().clone()
                        ),
                        after
                    );
                }
                assert_eq!(s.revision(), Revision::new(revision));
                assert_eq!(s.journal().len(), revision as usize);
                assert_eq!(s.history().entries().len(), revision as usize);
                for (id, text) in reference.iter().enumerate() {
                    assert_eq!(
                        s.snapshot()
                            .catalog()
                            .get(BufferId::new(id as u64))
                            .unwrap()
                            .as_str(),
                        text
                    );
                }
            }
        }
    }

    #[test]
    fn rebase_rejections_and_role_neutrality() {
        let mut s = setup(ActorRole::Human);
        let first = transaction(1, 0, 0, 0, 0, 1, "X");
        let receipt = s.submit(first.clone());
        let stale = transaction(2, 0, 0, 0, 3, 4, "Y");
        assert!(s.submit(stale).is_committed());
        assert_eq!(
            s.snapshot()
                .catalog()
                .get(BufferId::new(0))
                .unwrap()
                .as_str(),
            "XbcY"
        );
        let saved = (
            s.snapshot(),
            s.snapshots().clone(),
            s.journal().clone(),
            s.history().clone(),
        );
        for (txn, reason) in [
            (transaction(1, 0, 0, 0, 0, 1, "Z"), RejectReason::TxnIdReuse),
            (
                transaction(9, 0, 0, 0, 0, 1, "Z"),
                RejectReason::UnknownActor,
            ),
            (
                transaction(1, 2, 9, 0, 0, 1, "Z"),
                RejectReason::RevisionMismatch,
            ),
            (transaction(1, 3, 0, 0, 0, 1, "Z"), RejectReason::Conflict),
            (
                transaction(1, 4, 2, 9, 0, 1, "Z"),
                RejectReason::UnknownBuffer,
            ),
            (
                transaction(1, 5, 2, 0, 0, 9, "Z"),
                RejectReason::InvalidInput,
            ),
        ] {
            assert_eq!(s.submit(txn).reject_reason(), Some(reason));
            assert_eq!(
                (
                    s.snapshot(),
                    s.snapshots().clone(),
                    s.journal().clone(),
                    s.history().clone()
                ),
                saved
            );
        }
        s.actors_mut().revoke(ActorId::new(1)).unwrap();
        assert_eq!(s.submit(first), receipt);
        assert_eq!(
            s.submit(transaction(1, 8, 2, 0, 0, 1, "Z")).reject_reason(),
            Some(RejectReason::RevokedActor)
        );
        s.prune_before(Revision::new(1));
        assert_eq!(
            s.submit(transaction(2, 8, 0, 0, 3, 4, "Z")).reject_reason(),
            Some(RejectReason::BaseExpired)
        );
        let mut human = setup(ActorRole::Human);
        let mut agent = setup(ActorRole::Agent);
        let txn = transaction(1, 1, 0, 0, 0, 1, "Q");
        assert_eq!(human.submit(txn.clone()), agent.submit(txn));
        assert_eq!(human.snapshot().catalog(), agent.snapshot().catalog());
    }

    #[test]
    fn closed_and_utf8_errors_are_atomic_and_batch_commits_once() {
        let mut catalog = BufferCatalog::new();
        catalog
            .register(BufferId::new(0), Text::from_str("éabc"))
            .unwrap();
        catalog
            .register(BufferId::new(1), Text::from_str("abcd"))
            .unwrap();
        catalog.register(BufferId::new(2), Text::empty()).unwrap();
        catalog.remove(BufferId::new(2)).unwrap();
        let mut actors = ActorRegistry::new();
        actors.register(ActorId::new(1), ActorRole::Human).unwrap();
        let mut s = Sequencer::new(catalog, actors);
        let before = (
            s.snapshot(),
            s.snapshots().clone(),
            s.journal().clone(),
            s.history().clone(),
        );
        for (txn, reason) in [
            (
                transaction(1, 0, 0, 2, 0, 0, "x"),
                RejectReason::BufferClosed,
            ),
            (
                transaction(1, 1, 0, 0, 1, 2, "x"),
                RejectReason::InvalidInput,
            ),
        ] {
            assert_eq!(s.submit(txn).reject_reason(), Some(reason));
            assert_eq!(
                (
                    s.snapshot(),
                    s.snapshots().clone(),
                    s.journal().clone(),
                    s.history().clone()
                ),
                before
            );
        }
        let first = transaction(1, 2, 0, 0, 2, 3, "X");
        let second = transaction(1, 3, 0, 1, 0, 1, "Y");
        let both = Transaction::new(
            first.intent(),
            vec![first.patches()[0].clone(), second.patches()[0].clone()],
        )
        .unwrap();
        assert!(s.submit(both).is_committed());
        assert_eq!(s.revision(), Revision::new(1));
        assert_eq!(
            s.snapshot()
                .catalog()
                .get(BufferId::new(0))
                .unwrap()
                .as_str(),
            "éXbc"
        );
        assert_eq!(
            s.snapshot()
                .catalog()
                .get(BufferId::new(1))
                .unwrap()
                .as_str(),
            "Ybcd"
        );
        assert!(s.snapshot().catalog().is_tombstoned(BufferId::new(2)));
    }
}
