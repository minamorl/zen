//! Actor-scoped redo built on selective undo and sequencer admission.
//!
//! A redo submits the undo commit's inverse at that commit's revision. The
//! sequencer transports it across disjoint intervening edits, or rejects it
//! atomically if a required snapshot is missing or a footprint conflicts.

use std::collections::BTreeMap;
use std::fmt;

use super::{
    receipt::{CommitReceipt, RejectReason, SubmitOutcome},
    selective_undo::{SelectiveUndo, UndoError},
    sequencer::Sequencer,
};
use crate::editor::edit::{
    ids::{ActorId, TxnId},
    intent::EditIntent,
    transaction::Transaction,
};
use crate::editor::workspace::version::Revision;

/// A redo failure; neither the workspace nor the redo links are changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RedoError {
    /// The requested target is not in the committed history.
    UnknownTarget(TxnId),
    /// The actor did not originate the requested target.
    NotOwner { target: TxnId, actor: ActorId },
    /// The target has no selective undo compensation.
    NotUndone(TxnId),
    /// This compensation has already been redone.
    AlreadyRedone { compensation: TxnId, redo: TxnId },
    /// The redo identity does not belong to the requesting actor.
    InvalidRedoActor,
    /// The redo identity is already associated with another request or commit.
    RedoIdReuse(TxnId),
    /// Sequencer admission rejected the inverse.
    Rejected(RejectReason),
    /// The inverse did not change the workspace.
    NoOp,
}

impl fmt::Display for RedoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "redo failed: {self:?}")
    }
}
impl std::error::Error for RedoError {}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct RedoRecord {
    receipt: CommitReceipt,
}

/// Owns selective undo and the redo links for the same sequencer.
///
/// Submit edits and undo through this object to keep the links in sync.
#[derive(Debug)]
pub struct Redo {
    undo: SelectiveUndo,
    redone: BTreeMap<TxnId, RedoRecord>,
    requests: BTreeMap<TxnId, (ActorId, TxnId, CommitReceipt)>,
    latest: BTreeMap<TxnId, TxnId>,
    roots: BTreeMap<TxnId, TxnId>,
}

impl Redo {
    /// Attaches redo tracking to selective undo and its existing links.
    pub fn new(undo: SelectiveUndo) -> Self {
        Self {
            undo,
            redone: BTreeMap::new(),
            requests: BTreeMap::new(),
            latest: BTreeMap::new(),
            roots: BTreeMap::new(),
        }
    }

    /// Returns the underlying sequencer for inspection.
    pub fn sequencer(&self) -> &Sequencer {
        self.undo.sequencer()
    }

    /// Removes retained snapshots older than the floor.
    pub fn prune_before(&mut self, floor: Revision) {
        self.undo.prune_before(floor);
    }

    /// Submits an ordinary transaction.
    pub fn submit(&mut self, transaction: Transaction) -> SubmitOutcome {
        self.undo.submit(transaction)
    }

    /// Returns the active compensation for an original target, if undone.
    pub fn compensation_for(&self, target: TxnId) -> Option<TxnId> {
        let effective = self.effective_target(target);
        self.undo
            .compensation_for(effective)
            .filter(|id| !self.redone.contains_key(id))
    }

    /// Returns the redo receipt for a compensation, if already redone.
    pub fn redo_for(&self, compensation: TxnId) -> Option<CommitReceipt> {
        self.redone.get(&compensation).map(|r| r.receipt)
    }

    /// Undoes a target, or its latest redo after a previous redo cycle.
    pub fn undo(
        &mut self,
        actor: ActorId,
        target: TxnId,
        compensation: TxnId,
    ) -> Result<CommitReceipt, UndoError> {
        let effective = self.effective_target(target);
        self.undo.undo(actor, effective, compensation)
    }

    fn effective_target(&self, target: TxnId) -> TxnId {
        let root = self.roots.get(&target).copied().unwrap_or(target);
        self.latest.get(&root).copied().unwrap_or(root)
    }

    /// Redoes an original target or its compensating undo commit.
    ///
    /// An exact retry returns its original receipt, even after later commits.
    pub fn redo(
        &mut self,
        actor: ActorId,
        target: TxnId,
        redo: TxnId,
    ) -> Result<CommitReceipt, RedoError> {
        if let Some(&(old_actor, old_target, receipt)) = self.requests.get(&redo) {
            return if (actor, target) == (old_actor, old_target) {
                Ok(receipt)
            } else {
                Err(RedoError::RedoIdReuse(redo))
            };
        }
        let seq = self.undo.sequencer();
        if seq.journal().lookup_txn(redo).is_some() {
            return Err(RedoError::RedoIdReuse(redo));
        }
        let entry = seq
            .history()
            .get_by_txn_id(target)
            .ok_or(RedoError::UnknownTarget(target))?;
        if entry.actor() != actor {
            return Err(RedoError::NotOwner { target, actor });
        }
        if redo.actor() != actor {
            return Err(RedoError::InvalidRedoActor);
        }
        let (original, compensation) = if let Some(original) = self.undo.target_for(target) {
            (original, target)
        } else {
            let effective = self.effective_target(target);
            let compensation = self
                .undo
                .compensation_for(effective)
                .ok_or(RedoError::NotUndone(target))?;
            (effective, compensation)
        };
        if let Some(record) = self.redone.get(&compensation) {
            return Err(RedoError::AlreadyRedone {
                compensation,
                redo: record.receipt.txn(),
            });
        }
        let entry = self
            .undo
            .sequencer()
            .history()
            .get_by_txn_id(compensation)
            .ok_or(RedoError::UnknownTarget(compensation))?;
        let txn = Transaction::new(
            EditIntent::new(redo, entry.committed()),
            entry.inverse().patches().to_vec(),
        )
        .expect("history inverse has distinct buffers");
        match self.undo.submit(txn) {
            SubmitOutcome::Committed(receipt) => {
                let root = self.roots.get(&original).copied().unwrap_or(original);
                self.latest.insert(root, redo);
                self.roots.insert(redo, root);
                self.redone.insert(compensation, RedoRecord { receipt });
                self.requests.insert(redo, (actor, target, receipt));
                Ok(receipt)
            }
            SubmitOutcome::Rejected { reason, .. } => Err(RedoError::Rejected(reason)),
            SubmitOutcome::NoOp { .. } => Err(RedoError::NoOp),
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
    fn setup() -> Redo {
        let mut catalog = BufferCatalog::new();
        for buffer in 0..2 {
            catalog
                .register(BufferId::new(buffer), Text::from_str("abcdefghij"))
                .unwrap();
        }
        let mut actors = ActorRegistry::new();
        for actor in 1..=2 {
            actors
                .register(ActorId::new(actor), ActorRole::Human)
                .unwrap();
        }
        Redo::new(SelectiveUndo::new(Sequencer::new(catalog, actors)))
    }
    fn edit(
        actor: u64,
        nonce: u64,
        base: u64,
        buffer: u64,
        start: usize,
        end: usize,
        value: &str,
    ) -> Transaction {
        let span = ByteSpan::validate_in(&Text::from_str("abcdefghij"), start, end).unwrap();
        Transaction::new(
            EditIntent::new(id(actor, nonce), Revision::new(base)),
            vec![(
                BufferId::new(buffer),
                Patch::new(vec![Splice::new(span, Text::from_str(value))]).unwrap(),
            )],
        )
        .unwrap()
    }
    fn text(e: &Redo, buffer: u64) -> String {
        e.sequencer()
            .snapshot()
            .catalog()
            .get(BufferId::new(buffer))
            .unwrap()
            .as_str()
            .to_owned()
    }
    fn state(
        e: &Redo,
    ) -> (
        crate::editor::workspace::snapshots::WorkspaceSnapshot,
        crate::editor::workspace::snapshots::SnapshotStore,
        crate::editor::collab::journal::CommitJournal,
        crate::editor::edit::history::EditHistory,
        Option<TxnId>,
        Option<CommitReceipt>,
    ) {
        (
            e.sequencer().snapshot(),
            e.sequencer().snapshots().clone(),
            e.sequencer().journal().clone(),
            e.sequencer().history().clone(),
            e.compensation_for(id(1, 0)),
            e.redo_for(id(1, 1)),
        )
    }
    #[test]
    fn shifted_redo_retry_and_next_undo() {
        let mut e = setup();
        assert!(e.submit(edit(1, 0, 0, 0, 6, 7, "X")).is_committed());
        e.undo(ActorId::new(1), id(1, 0), id(1, 1)).unwrap();
        assert!(e.submit(edit(2, 0, 2, 0, 0, 0, "ZZ")).is_committed());
        let receipt = e.redo(ActorId::new(1), id(1, 0), id(1, 2)).unwrap();
        assert_eq!(text(&e, 0), "ZZabcdefXhij");
        let before = state(&e);
        assert_eq!(e.redo(ActorId::new(1), id(1, 0), id(1, 2)), Ok(receipt));
        assert_eq!(
            e.redo(ActorId::new(1), id(1, 1), id(1, 2)),
            Err(RedoError::RedoIdReuse(id(1, 2)))
        );
        assert_eq!(
            e.redo(ActorId::new(1), id(1, 1), id(1, 3)),
            Err(RedoError::AlreadyRedone {
                compensation: id(1, 1),
                redo: id(1, 2)
            })
        );
        assert_eq!(state(&e), before);
        e.undo(ActorId::new(1), id(1, 0), id(1, 3)).unwrap();
        assert_eq!(e.compensation_for(id(1, 0)), Some(id(1, 3)));
        assert_eq!(text(&e, 0), "ZZabcdefghij");
        e.redo(ActorId::new(1), id(1, 3), id(1, 4)).unwrap();
        assert_eq!(text(&e, 0), "ZZabcdefXhij");
        e.undo(ActorId::new(1), id(1, 2), id(1, 5)).unwrap();
        assert_eq!(e.compensation_for(id(1, 0)), Some(id(1, 5)));
        assert_eq!(e.compensation_for(id(1, 2)), Some(id(1, 5)));
        assert_eq!(text(&e, 0), "ZZabcdefghij");
    }

    #[test]
    fn expired_undo_base_is_rejected_by_redo_atomically() {
        let mut e = setup();
        assert!(e.submit(edit(1, 0, 0, 0, 4, 5, "X")).is_committed());
        e.undo(ActorId::new(1), id(1, 0), id(1, 1)).unwrap();
        assert!(e.submit(edit(1, 9, 2, 0, 0, 1, "Q")).is_committed());
        e.prune_before(Revision::new(3));
        let redo_before = state(&e);
        // Debug includes the entire private undo tracking map as well as its sequencer.
        let undo_before = format!("{:?}", e.undo);
        let links = (
            e.redone.clone(),
            e.requests.clone(),
            e.latest.clone(),
            e.roots.clone(),
        );
        for target in [id(1, 0), id(1, 1)] {
            assert_eq!(
                e.redo(ActorId::new(1), target, id(1, 2)),
                Err(RedoError::Rejected(RejectReason::BaseExpired))
            );
            assert_eq!(state(&e), redo_before);
            assert_eq!(format!("{:?}", e.undo), undo_before);
            assert_eq!(
                (
                    e.redone.clone(),
                    e.requests.clone(),
                    e.latest.clone(),
                    e.roots.clone()
                ),
                links
            );
        }
    }
    #[test]
    fn invalid_requests_and_conflicts_are_atomic() {
        let mut e = setup();
        assert!(e.submit(edit(1, 0, 0, 0, 4, 5, "X")).is_committed());
        let before = state(&e);
        assert_eq!(
            e.redo(ActorId::new(1), id(1, 99), id(1, 2)),
            Err(RedoError::UnknownTarget(id(1, 99)))
        );
        assert_eq!(
            e.redo(ActorId::new(2), id(1, 0), id(2, 2)),
            Err(RedoError::NotOwner {
                target: id(1, 0),
                actor: ActorId::new(2)
            })
        );
        assert_eq!(
            e.redo(ActorId::new(1), id(1, 0), id(2, 2)),
            Err(RedoError::InvalidRedoActor)
        );
        assert_eq!(
            e.redo(ActorId::new(1), id(1, 0), id(1, 2)),
            Err(RedoError::NotUndone(id(1, 0)))
        );
        assert_eq!(
            e.redo(ActorId::new(1), id(1, 0), id(1, 0)),
            Err(RedoError::RedoIdReuse(id(1, 0)))
        );
        assert_eq!(state(&e), before);
        e.undo(ActorId::new(1), id(1, 0), id(1, 1)).unwrap();
        assert!(e.submit(edit(2, 0, 2, 0, 4, 5, "Y")).is_committed());
        let before = state(&e);
        assert_eq!(
            e.redo(ActorId::new(1), id(1, 1), id(1, 2)),
            Err(RedoError::Rejected(RejectReason::Conflict))
        );
        assert_eq!(state(&e), before);
    }
    #[test]
    fn multi_buffer_conflict_is_atomic() {
        let mut e = setup();
        let a = edit(1, 0, 0, 0, 4, 5, "X");
        let b = edit(1, 0, 0, 1, 4, 5, "Y");
        let txn = Transaction::new(
            a.intent(),
            vec![a.patches()[0].clone(), b.patches()[0].clone()],
        )
        .unwrap();
        assert!(e.submit(txn).is_committed());
        e.undo(ActorId::new(1), id(1, 0), id(1, 1)).unwrap();
        assert!(e.submit(edit(2, 0, 2, 1, 4, 5, "Z")).is_committed());
        let before = state(&e);
        assert_eq!(
            e.redo(ActorId::new(1), id(1, 0), id(1, 2)),
            Err(RedoError::Rejected(RejectReason::Conflict))
        );
        assert_eq!(state(&e), before);
        assert_eq!(text(&e, 0), "abcdefghij");
        assert_eq!(text(&e, 1), "abcdZfghij");
    }
    #[test]
    fn multi_buffer_redo_preserves_disjoint_edits_and_retry() {
        let mut e = setup();
        let a = edit(1, 0, 0, 0, 4, 4, "XY");
        let b = edit(1, 0, 0, 1, 4, 6, "");
        let txn = Transaction::new(
            a.intent(),
            vec![a.patches()[0].clone(), b.patches()[0].clone()],
        )
        .unwrap();
        assert!(e.submit(txn).is_committed());
        e.undo(ActorId::new(1), id(1, 0), id(1, 1)).unwrap();
        assert!(e.submit(edit(2, 0, 2, 0, 0, 0, "ZZ")).is_committed());
        assert!(e.submit(edit(2, 1, 3, 1, 0, 1, "Q")).is_committed());
        let receipt = e.redo(ActorId::new(1), id(1, 0), id(1, 2)).unwrap();
        assert_eq!(text(&e, 0), "ZZabcdXYefghij");
        assert_eq!(text(&e, 1), "Qbcdghij");
        assert!(e.submit(edit(2, 2, 5, 1, 0, 1, "R")).is_committed());
        let before = state(&e);
        assert_eq!(e.redo(ActorId::new(1), id(1, 0), id(1, 2)), Ok(receipt));
        assert_eq!(state(&e), before);
    }
    #[test]
    fn deterministic_disjoint_redo_property() {
        let mut rng = Rng::new(42);
        for _ in 0..256 {
            let pos = rng.gen_range(5..9);
            let prefix = ["Q", "ZZ", "界"][rng.gen_range(0..3)];
            let mut e = setup();
            assert!(e.submit(edit(1, 0, 0, 0, pos, pos + 1, "X")).is_committed());
            e.undo(ActorId::new(1), id(1, 0), id(1, 1)).unwrap();
            assert!(e.submit(edit(2, 0, 2, 0, 0, 0, prefix)).is_committed());
            e.redo(ActorId::new(1), id(1, 1), id(1, 2)).unwrap();
            let mut expected = String::from("abcdefghij");
            expected.replace_range(pos..pos + 1, "X");
            assert_eq!(text(&e, 0), format!("{prefix}{expected}"));
        }
    }
}
