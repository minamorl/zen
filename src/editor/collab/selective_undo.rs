//! Selective undo using committed inverses and the sequencer's rebase rules.
//!
//! Undo records are added only after the compensating transaction commits.
//! The sequencer validates ownership-independent admission, retained snapshots,
//! and closed-footprint conflicts before publishing any change.

use std::collections::BTreeMap;
use std::fmt;

use super::{
    receipt::{CommitReceipt, RejectReason, SubmitOutcome},
    sequencer::Sequencer,
};
use crate::editor::edit::{
    ids::{ActorId, TxnId},
    intent::EditIntent,
    transaction::Transaction,
};

/// A selective undo that could not be committed; no state is changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndoError {
    /// No committed transaction has this identity.
    UnknownTarget(TxnId),
    /// The requesting actor did not originate the target.
    NotOwner { target: TxnId, actor: ActorId },
    /// This target already has a compensating commit.
    AlreadyUndone { target: TxnId, compensation: TxnId },
    /// A compensating commit is not an ordinary undo target.
    CompensationTarget { target: TxnId, original: TxnId },
    /// The compensation identity belongs to a different actor.
    InvalidCompensationActor,
    /// The compensation identity is already committed.
    CompensationIdReuse(TxnId),
    /// The inverse could not be admitted, for example due to conflict or pruning.
    Rejected(RejectReason),
    /// The inverse had no effect and therefore could not be committed.
    NoOp,
}

impl fmt::Display for UndoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "selective undo failed: {self:?}")
    }
}
impl std::error::Error for UndoError {}

/// A sequencer and the deterministic mapping from undone targets to commits.
///
/// All ordinary commits should be submitted through this instance so that the
/// mapping remains attached to the sequencer whose history it describes.
#[derive(Debug)]
pub struct SelectiveUndo {
    sequencer: Sequencer,
    undone: BTreeMap<TxnId, TxnId>,
}

impl SelectiveUndo {
    /// Attaches empty undo tracking to a sequencer.
    pub fn new(sequencer: Sequencer) -> Self {
        Self {
            sequencer,
            undone: BTreeMap::new(),
        }
    }

    /// Returns the sequencer for inspecting its workspace and history.
    pub fn sequencer(&self) -> &Sequencer {
        &self.sequencer
    }

    /// Submits an ordinary edit to the same sequencer.
    pub fn submit(&mut self, transaction: Transaction) -> SubmitOutcome {
        self.sequencer.submit(transaction)
    }

    /// Returns the compensation committed for a target, if any.
    pub fn compensation_for(&self, target: TxnId) -> Option<TxnId> {
        self.undone.get(&target).copied()
    }

    /// Finds the original target of a compensating transaction.
    pub fn target_for(&self, compensation: TxnId) -> Option<TxnId> {
        self.undone
            .iter()
            .find_map(|(&target, &id)| (id == compensation).then_some(target))
    }

    /// Commits a fresh compensation for an actor's target.
    ///
    /// `compensation` must be a previously unused transaction ID of `actor`.
    /// Repeating an undo rejects it without changing either the workspace or
    /// the original link. Intervening edits are checked in their own revision
    /// frames by the sequencer; missing snapshots and conflicts are rejected.
    pub fn undo(
        &mut self,
        actor: ActorId,
        target: TxnId,
        compensation: TxnId,
    ) -> Result<CommitReceipt, UndoError> {
        let entry = self
            .sequencer
            .history()
            .get_by_txn_id(target)
            .ok_or(UndoError::UnknownTarget(target))?;
        if entry.actor() != actor {
            return Err(UndoError::NotOwner { target, actor });
        }
        if let Some(original) = self.target_for(target) {
            return Err(UndoError::CompensationTarget { target, original });
        }
        if let Some(&compensation) = self.undone.get(&target) {
            return Err(UndoError::AlreadyUndone {
                target,
                compensation,
            });
        }
        if compensation.actor() != actor {
            return Err(UndoError::InvalidCompensationActor);
        }
        if self.sequencer.journal().lookup_txn(compensation).is_some() {
            return Err(UndoError::CompensationIdReuse(compensation));
        }
        let inverse = entry.inverse();
        let txn = Transaction::new(
            EditIntent::new(compensation, entry.committed()),
            inverse.patches().to_vec(),
        )
        .expect("history inverse has distinct buffers");
        match self.sequencer.submit(txn) {
            SubmitOutcome::Committed(receipt) => {
                self.undone.insert(target, receipt.txn());
                Ok(receipt)
            }
            SubmitOutcome::Rejected { reason, .. } => Err(UndoError::Rejected(reason)),
            SubmitOutcome::NoOp { .. } => Err(UndoError::NoOp),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::{registry::ActorRegistry, role::ActorRole},
        edit::{
            ids::{BufferId, Nonce},
            patch::Patch,
            splice::Splice,
        },
        test_support::Rng,
        text::{span::ByteSpan, utf8::Text},
        workspace::{catalog::BufferCatalog, version::Revision},
    };

    fn id(actor: u64, nonce: u64) -> TxnId {
        TxnId::new(ActorId::new(actor), Nonce::new(nonce))
    }
    fn setup() -> SelectiveUndo {
        let mut catalog = BufferCatalog::new();
        catalog
            .register(BufferId::new(0), Text::from_str("abcdefghij"))
            .unwrap();
        catalog
            .register(BufferId::new(1), Text::from_str("abcdefghij"))
            .unwrap();
        let mut actors = ActorRegistry::new();
        for actor in 1..=2 {
            actors
                .register(ActorId::new(actor), ActorRole::Human)
                .unwrap();
        }
        SelectiveUndo::new(Sequencer::new(catalog, actors))
    }
    fn edit(
        actor: u64,
        nonce: u64,
        base: u64,
        start: usize,
        end: usize,
        value: &str,
    ) -> Transaction {
        let span = ByteSpan::validate_in(&Text::from_str("abcdefghij"), start, end).unwrap();
        Transaction::new(
            EditIntent::new(id(actor, nonce), Revision::new(base)),
            vec![(
                BufferId::new(0),
                Patch::new(vec![Splice::new(span, Text::from_str(value))]).unwrap(),
            )],
        )
        .unwrap()
    }
    fn text(engine: &SelectiveUndo) -> String {
        engine
            .sequencer()
            .snapshot()
            .catalog()
            .get(BufferId::new(0))
            .unwrap()
            .as_str()
            .to_owned()
    }
    fn state(
        engine: &SelectiveUndo,
    ) -> (
        crate::editor::workspace::snapshots::WorkspaceSnapshot,
        crate::editor::workspace::snapshots::SnapshotStore,
        crate::editor::collab::journal::CommitJournal,
        crate::editor::edit::history::EditHistory,
        BTreeMap<TxnId, TxnId>,
    ) {
        (
            engine.sequencer().snapshot(),
            engine.sequencer().snapshots().clone(),
            engine.sequencer().journal().clone(),
            engine.sequencer().history().clone(),
            engine.undone.clone(),
        )
    }
    #[test]
    fn disjoint_rebase_and_links() {
        let mut e = setup();
        assert!(e.submit(edit(1, 0, 0, 1, 2, "XYZ")).is_committed());
        assert!(e.submit(edit(2, 0, 1, 7, 8, "Q")).is_committed());
        assert_eq!(text(&e), "aXYZcdeQghij");
        let receipt = e.undo(ActorId::new(1), id(1, 0), id(1, 1)).unwrap();
        assert_eq!(receipt.previous(), Revision::new(2));
        assert_eq!(text(&e), "abcdeQghij");
        assert_eq!(e.compensation_for(id(1, 0)), Some(id(1, 1)));
        assert_eq!(e.target_for(receipt.txn()), Some(id(1, 0)));
        let before = state(&e);
        assert_eq!(
            e.undo(ActorId::new(1), id(1, 1), id(1, 2)),
            Err(UndoError::CompensationTarget {
                target: id(1, 1),
                original: id(1, 0)
            })
        );
        assert_eq!(state(&e), before);
        assert_eq!(
            e.undo(ActorId::new(1), id(1, 0), id(1, 2)),
            Err(UndoError::AlreadyUndone {
                target: id(1, 0),
                compensation: id(1, 1)
            })
        );
        assert_eq!(state(&e), before);
    }
    #[test]
    fn rejection_is_atomic() {
        let mut e = setup();
        assert!(e.submit(edit(1, 0, 0, 2, 3, "X")).is_committed());
        let before = state(&e);
        for (actor, target, compensation, error) in [
            (
                2,
                id(1, 0),
                id(2, 1),
                UndoError::NotOwner {
                    target: id(1, 0),
                    actor: ActorId::new(2),
                },
            ),
            (1, id(1, 99), id(1, 1), UndoError::UnknownTarget(id(1, 99))),
            (1, id(1, 0), id(2, 1), UndoError::InvalidCompensationActor),
            (
                1,
                id(1, 0),
                id(1, 0),
                UndoError::CompensationIdReuse(id(1, 0)),
            ),
        ] {
            assert_eq!(
                e.undo(ActorId::new(actor), target, compensation),
                Err(error)
            );
            assert_eq!(state(&e), before);
        }
        assert!(e.submit(edit(2, 0, 1, 2, 3, "Y")).is_committed());
        let before = state(&e);
        assert_eq!(
            e.undo(ActorId::new(1), id(1, 0), id(1, 1)),
            Err(UndoError::Rejected(RejectReason::Conflict))
        );
        assert_eq!(state(&e), before);
    }
    #[test]
    fn deterministic_disjoint_sequences_restore_only_target() {
        let mut rng = Rng::new(42);
        for _ in 0..256 {
            let first = rng.gen_range(0..3);
            let second = rng.gen_range(6..10);
            let mut e = setup();
            assert!(e
                .submit(edit(1, 0, 0, first, first + 1, "X"))
                .is_committed());
            assert!(e
                .submit(edit(2, 0, 1, second, second + 1, "Y"))
                .is_committed());
            e.undo(ActorId::new(1), id(1, 0), id(1, 1)).unwrap();
            let mut expected = String::from("abcdefghij");
            expected.replace_range(second..second + 1, "Y");
            assert_eq!(text(&e), expected);
        }
    }
    #[test]
    fn offsets_move_through_insertion_and_deletion() {
        let mut e = setup();
        assert!(e.submit(edit(1, 0, 0, 6, 7, "X")).is_committed());
        assert!(e.submit(edit(2, 0, 1, 0, 0, "ZZ")).is_committed());
        assert!(e.submit(edit(2, 1, 2, 2, 3, "")).is_committed());
        assert_eq!(text(&e), "ZZbcdefXhij");
        e.undo(ActorId::new(1), id(1, 0), id(1, 1)).unwrap();
        assert_eq!(text(&e), "ZZbcdefghij");
    }

    #[test]
    fn undo_insertion_and_deletion_preserves_disjoint_commits() {
        for (start, end, replacement, after_target) in
            [(4, 4, "界", "abcd界efghij"), (4, 6, "", "abcdghij")]
        {
            let mut e = setup();
            assert!(e
                .submit(edit(1, 0, 0, start, end, replacement))
                .is_committed());
            assert_eq!(text(&e), after_target);
            assert!(e.submit(edit(2, 0, 1, 0, 1, "Q")).is_committed());
            let receipt = e.undo(ActorId::new(1), id(1, 0), id(1, 1)).unwrap();
            assert_eq!(text(&e), "Qbcdefghij");
            assert_eq!(e.compensation_for(id(1, 0)), Some(receipt.txn()));
            assert_eq!(e.target_for(receipt.txn()), Some(id(1, 0)));
        }
    }

    #[test]
    fn insertion_and_deletion_targets_reject_conflicting_edits_atomically() {
        for (end, replacement, conflict_start, conflict_end) in [(4, "XY", 4, 5), (6, "", 4, 4)] {
            let mut e = setup();
            assert!(e.submit(edit(1, 0, 0, 4, end, replacement)).is_committed());
            assert!(e
                .submit(edit(2, 0, 1, conflict_start, conflict_end, "Z"))
                .is_committed());
            let before = state(&e);
            assert_eq!(
                e.undo(ActorId::new(1), id(1, 0), id(1, 1)),
                Err(UndoError::Rejected(RejectReason::Conflict))
            );
            assert_eq!(state(&e), before);
            assert_eq!(e.compensation_for(id(1, 0)), None);
        }
    }

    fn two_buffer_target() -> Transaction {
        let insertion = edit(1, 0, 0, 4, 4, "XY");
        let deletion = edit(1, 0, 0, 4, 6, "");
        Transaction::new(
            insertion.intent(),
            vec![
                (BufferId::new(0), insertion.patches()[0].1.clone()),
                (BufferId::new(1), deletion.patches()[0].1.clone()),
            ],
        )
        .unwrap()
    }

    #[test]
    fn undo_restores_every_buffer_and_preserves_disjoint_edit() {
        let mut e = setup();
        assert!(e.submit(two_buffer_target()).is_committed());
        assert!(e.submit(edit(2, 0, 1, 0, 1, "Q")).is_committed());
        e.undo(ActorId::new(1), id(1, 0), id(1, 1)).unwrap();
        let snapshot = e.sequencer().snapshot();
        assert_eq!(
            snapshot.catalog().get(BufferId::new(0)).unwrap().as_str(),
            "Qbcdefghij"
        );
        assert_eq!(
            snapshot.catalog().get(BufferId::new(1)).unwrap().as_str(),
            "abcdefghij"
        );
    }

    #[test]
    fn conflict_in_second_buffer_rejects_whole_undo() {
        let mut e = setup();
        assert!(e.submit(two_buffer_target()).is_committed());
        let insertion = edit(2, 0, 1, 4, 4, "Z");
        let conflict = Transaction::new(
            insertion.intent(),
            vec![(BufferId::new(1), insertion.patches()[0].1.clone())],
        )
        .unwrap();
        assert!(e.submit(conflict).is_committed());
        let before = state(&e);
        assert_eq!(
            e.undo(ActorId::new(1), id(1, 0), id(1, 1)),
            Err(UndoError::Rejected(RejectReason::Conflict))
        );
        assert_eq!(state(&e), before);
        assert_eq!(e.compensation_for(id(1, 0)), None);
    }

    #[test]
    fn moved_conflict_and_expired_snapshot_leave_everything_unchanged() {
        let mut e = setup();
        assert!(e.submit(edit(1, 0, 0, 6, 7, "X")).is_committed());
        assert!(e.submit(edit(2, 0, 1, 0, 0, "ZZ")).is_committed());
        assert!(e.submit(edit(2, 1, 2, 8, 9, "Y")).is_committed());
        let before = state(&e);
        assert_eq!(
            e.undo(ActorId::new(1), id(1, 0), id(1, 1)),
            Err(UndoError::Rejected(RejectReason::Conflict))
        );
        assert_eq!(state(&e), before);

        let mut expired = setup();
        assert!(expired.submit(edit(1, 0, 0, 6, 7, "X")).is_committed());
        assert!(expired.submit(edit(2, 0, 1, 0, 0, "Z")).is_committed());
        expired.sequencer.prune_before(Revision::new(2));
        let before = state(&expired);
        assert_eq!(
            expired.undo(ActorId::new(1), id(1, 0), id(1, 1)),
            Err(UndoError::Rejected(RejectReason::BaseExpired))
        );
        assert_eq!(state(&expired), before);
    }
}
