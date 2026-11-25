//! Actor-scoped undo and redo requests shared by editor frontends.
//!
//! Frontends resolve a latest target before calling this boundary and supply
//! its candidates. Exactly one candidate is required; no history ordering is
//! guessed here. Retrying a redo must use the same resolved target and ID.

use std::fmt;

use crate::editor::collab::{
    receipt::CommitReceipt,
    redo::{Redo, RedoError},
    selective_undo::UndoError,
};
use crate::editor::edit::ids::{ActorId, TxnId};

/// A target supplied explicitly or resolved by a frontend.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Target<'a> {
    /// An explicit committed transaction.
    Explicit(TxnId),
    /// Candidates produced by resolving the latest actor target.
    Resolved(&'a [TxnId]),
}

/// Commands understood by the supported frontends.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FrontendCommand<'a> {
    /// Emacs undo-only.
    EmacsUndoOnly(Target<'a>),
    /// Emacs undo-redo.
    EmacsUndoRedo(Target<'a>),
    /// Neovim `u`.
    NeovimUndo(Target<'a>),
    /// Neovim Ctrl-r.
    NeovimRedo(Target<'a>),
    /// Agent undo with an explicit transaction.
    AgentUndo(TxnId),
    /// Agent redo with an explicit transaction.
    AgentRedo(TxnId),
}

/// Canonical undo request.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CoreUndoRequest {
    /// Requesting actor.
    pub actor: ActorId,
    /// Original committed transaction.
    pub target: TxnId,
    /// New compensation transaction.
    pub operation: TxnId,
}

/// Canonical redo request.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CoreRedoRequest {
    /// Requesting actor.
    pub actor: ActorId,
    /// Original transaction or its compensation.
    pub target: TxnId,
    /// New redo transaction.
    pub operation: TxnId,
}

/// Canonical operation independent of its frontend.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CoreRequest {
    /// Undo a transaction.
    Undo(CoreUndoRequest),
    /// Redo a transaction.
    Redo(CoreRedoRequest),
}

/// A normalization or core execution failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InteropError {
    /// The latest target did not resolve to exactly one candidate.
    AmbiguousTarget,
    /// The operation ID belongs to another actor.
    InvalidOperationActor,
    /// The target ID belongs to another actor.
    TargetActorMismatch { target: TxnId, actor: ActorId },
    /// The undo was rejected by the core.
    Undo(UndoError),
    /// The redo was rejected by the core.
    Redo(RedoError),
}

impl fmt::Display for InteropError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "undo interop failed: {self:?}")
    }
}
impl std::error::Error for InteropError {}

fn validate(actor: ActorId, target: TxnId, operation: TxnId) -> Result<(), InteropError> {
    if operation.actor() != actor {
        return Err(InteropError::InvalidOperationActor);
    }
    if target.actor() != actor {
        return Err(InteropError::TargetActorMismatch { target, actor });
    }
    Ok(())
}

/// Normalizes a frontend command without changing core state.
pub fn normalize(
    actor: ActorId,
    operation: TxnId,
    command: FrontendCommand<'_>,
) -> Result<CoreRequest, InteropError> {
    let (target, is_redo) = match command {
        FrontendCommand::EmacsUndoOnly(t) | FrontendCommand::NeovimUndo(t) => (t, false),
        FrontendCommand::EmacsUndoRedo(t) | FrontendCommand::NeovimRedo(t) => (t, true),
        FrontendCommand::AgentUndo(t) => (Target::Explicit(t), false),
        FrontendCommand::AgentRedo(t) => (Target::Explicit(t), true),
    };
    let target = match target {
        Target::Explicit(target) => target,
        Target::Resolved([target]) => *target,
        Target::Resolved(_) => return Err(InteropError::AmbiguousTarget),
    };
    validate(actor, target, operation)?;
    Ok(if is_redo {
        CoreRequest::Redo(CoreRedoRequest {
            actor,
            target,
            operation,
        })
    } else {
        CoreRequest::Undo(CoreUndoRequest {
            actor,
            target,
            operation,
        })
    })
}

/// Executes a canonical request, preserving core errors and redo retry semantics.
pub fn execute(core: &mut Redo, request: CoreRequest) -> Result<CommitReceipt, InteropError> {
    match request {
        CoreRequest::Undo(request) => {
            validate(request.actor, request.target, request.operation)?;
            core.undo(request.actor, request.target, request.operation)
                .map_err(InteropError::Undo)
        }
        CoreRequest::Redo(request) => {
            validate(request.actor, request.target, request.operation)?;
            core.redo(request.actor, request.target, request.operation)
                .map_err(InteropError::Redo)
        }
    }
}

/// Normalizes and executes one frontend command.
pub fn submit(
    core: &mut Redo,
    actor: ActorId,
    operation: TxnId,
    command: FrontendCommand<'_>,
) -> Result<CommitReceipt, InteropError> {
    execute(core, normalize(actor, operation, command)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::{
            registry::ActorRegistry, role::ActorRole, selective_undo::SelectiveUndo,
            sequencer::Sequencer,
        },
        edit::{
            ids::{BufferId, Nonce},
            intent::EditIntent,
            patch::Patch,
            splice::Splice,
            transaction::Transaction,
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
        catalog
            .register(BufferId::new(0), Text::from_str("abcdef"))
            .unwrap();
        let mut actors = ActorRegistry::new();
        for actor in 1..=2 {
            actors
                .register(ActorId::new(actor), ActorRole::Human)
                .unwrap();
        }
        Redo::new(SelectiveUndo::new(Sequencer::new(catalog, actors)))
    }
    fn edit(core: &mut Redo, actor: u64, nonce: u64, base: u64, pos: usize, value: &str) {
        let span = ByteSpan::validate_in(&Text::from_str("abcdef"), pos, pos + 1).unwrap();
        let patch = Patch::new(vec![Splice::new(span, Text::from_str(value))]).unwrap();
        let txn = Transaction::new(
            EditIntent::new(id(actor, nonce), Revision::new(base)),
            vec![(BufferId::new(0), patch)],
        )
        .unwrap();
        assert!(core.submit(txn).is_committed());
    }
    fn state(
        core: &Redo,
    ) -> (
        crate::editor::workspace::snapshots::WorkspaceSnapshot,
        crate::editor::workspace::snapshots::SnapshotStore,
        crate::editor::collab::journal::CommitJournal,
        crate::editor::edit::history::EditHistory,
    ) {
        (
            core.sequencer().snapshot(),
            core.sequencer().snapshots().clone(),
            core.sequencer().journal().clone(),
            core.sequencer().history().clone(),
        )
    }

    #[test]
    fn frontend_equivalence_and_retry() {
        let mut rng = Rng::new(42);
        for _ in 0..128 {
            let nonce = rng.next_u64();
            let target = id(1, nonce);
            let undo = id(1, nonce.wrapping_add(1));
            let redo = id(1, nonce.wrapping_add(2));
            let candidates = [target];
            let undos = [
                FrontendCommand::EmacsUndoOnly(Target::Resolved(&candidates)),
                FrontendCommand::NeovimUndo(Target::Explicit(target)),
                FrontendCommand::AgentUndo(target),
            ];
            let redos = [
                FrontendCommand::EmacsUndoRedo(Target::Explicit(target)),
                FrontendCommand::NeovimRedo(Target::Resolved(&candidates)),
                FrontendCommand::AgentRedo(target),
            ];
            let mut outcomes = Vec::new();
            for i in 0..3 {
                let mut core = setup();
                edit(&mut core, 1, nonce, 0, 1, "X");
                assert_eq!(
                    normalize(ActorId::new(1), undo, undos[i]),
                    Ok(CoreRequest::Undo(CoreUndoRequest {
                        actor: ActorId::new(1),
                        target,
                        operation: undo,
                    }))
                );
                let u = submit(&mut core, ActorId::new(1), undo, undos[i]).unwrap();
                assert_eq!(core.sequencer().journal().lookup_txn(undo), Some(&u));
                assert_eq!(
                    normalize(ActorId::new(1), redo, redos[i]),
                    Ok(CoreRequest::Redo(CoreRedoRequest {
                        actor: ActorId::new(1),
                        target,
                        operation: redo,
                    }))
                );
                let r = submit(&mut core, ActorId::new(1), redo, redos[i]).unwrap();
                assert_eq!(core.sequencer().journal().lookup_txn(redo), Some(&r));
                let before = state(&core);
                for command in redos {
                    let retried = submit(&mut core, ActorId::new(1), redo, command).unwrap();
                    assert_eq!(core.sequencer().journal().lookup_txn(redo), Some(&retried));
                    assert_eq!(retried, r);
                }
                assert_eq!(state(&core), before);
                edit(&mut core, 2, 0, 3, 4, "Y");
                let later = state(&core);
                for command in redos {
                    let retried = submit(&mut core, ActorId::new(1), redo, command).unwrap();
                    assert_eq!(core.sequencer().journal().lookup_txn(redo), Some(&retried));
                    assert_eq!(retried, r);
                }
                assert_eq!(state(&core), later);
                let snapshot = core.sequencer().snapshot();
                outcomes.push((u, r, snapshot.revision(), snapshot.catalog().clone()));
            }
            assert_eq!(outcomes[0], outcomes[1]);
            assert_eq!(outcomes[1], outcomes[2]);
        }
    }

    #[test]
    fn rejection_is_atomic() {
        let mut core = setup();
        edit(&mut core, 1, 0, 0, 1, "X");
        let before = state(&core);
        let actor = ActorId::new(1);
        let target = id(1, 0);
        let many = [target, id(1, 9)];
        for command in [
            FrontendCommand::EmacsUndoOnly(Target::Resolved(&[])),
            FrontendCommand::NeovimUndo(Target::Resolved(&many)),
        ] {
            assert_eq!(
                submit(&mut core, actor, id(1, 1), command),
                Err(InteropError::AmbiguousTarget)
            );
            assert_eq!(state(&core), before);
        }
        assert!(matches!(
            submit(
                &mut core,
                actor,
                id(1, 1),
                FrontendCommand::AgentUndo(id(2, 0))
            ),
            Err(InteropError::TargetActorMismatch { .. })
        ));
        assert_eq!(
            submit(
                &mut core,
                actor,
                id(2, 1),
                FrontendCommand::AgentUndo(target)
            ),
            Err(InteropError::InvalidOperationActor)
        );
        assert_eq!(
            submit(
                &mut core,
                actor,
                id(1, 1),
                FrontendCommand::AgentUndo(id(1, 99))
            ),
            Err(InteropError::Undo(UndoError::UnknownTarget(id(1, 99))))
        );
        assert_eq!(
            submit(
                &mut core,
                actor,
                id(1, 1),
                FrontendCommand::AgentRedo(target)
            ),
            Err(InteropError::Redo(RedoError::NotUndone(target)))
        );
        assert_eq!(state(&core), before);
        submit(
            &mut core,
            actor,
            id(1, 1),
            FrontendCommand::AgentUndo(target),
        )
        .unwrap();
        let before = state(&core);
        assert!(matches!(
            submit(
                &mut core,
                actor,
                id(1, 2),
                FrontendCommand::AgentUndo(target)
            ),
            Err(InteropError::Undo(UndoError::AlreadyUndone { .. }))
        ));
        assert_eq!(state(&core), before);
    }

    #[test]
    fn conflict_is_atomic() {
        let mut core = setup();
        edit(&mut core, 1, 0, 0, 1, "X");
        edit(&mut core, 2, 0, 1, 1, "Y");
        let before = state(&core);
        assert_eq!(
            submit(
                &mut core,
                ActorId::new(1),
                id(1, 1),
                FrontendCommand::AgentUndo(id(1, 0))
            ),
            Err(InteropError::Undo(UndoError::Rejected(
                crate::editor::collab::receipt::RejectReason::Conflict
            )))
        );
        assert_eq!(state(&core), before);
    }

    #[test]
    fn committed_candidates_and_redo_conflict_preserve_links() {
        let actor = ActorId::new(1);
        let target = id(1, 0);
        let other = id(1, 1);
        let foreign = id(2, 0);
        let compensation = id(1, 2);
        let redo = id(1, 3);
        let mut core = setup();
        edit(&mut core, 1, 0, 0, 0, "X");
        edit(&mut core, 1, 1, 1, 4, "Y");
        edit(&mut core, 2, 0, 2, 5, "Z");
        let before = state(&core);
        let candidates = [target, other];
        for command in [
            FrontendCommand::EmacsUndoOnly(Target::Resolved(&candidates)),
            FrontendCommand::NeovimRedo(Target::Resolved(&candidates)),
        ] {
            assert_eq!(
                submit(&mut core, actor, compensation, command),
                Err(InteropError::AmbiguousTarget)
            );
        }
        for command in [
            FrontendCommand::AgentUndo(foreign),
            FrontendCommand::AgentRedo(foreign),
        ] {
            assert!(matches!(
                submit(&mut core, actor, compensation, command),
                Err(InteropError::TargetActorMismatch { .. })
            ));
        }
        assert_eq!(state(&core), before);
        assert_eq!(core.compensation_for(target), None);
        assert_eq!(core.redo_for(compensation), None);
        submit(
            &mut core,
            actor,
            compensation,
            FrontendCommand::AgentUndo(target),
        )
        .unwrap();
        edit(&mut core, 2, 1, 4, 0, "Q");
        let before = state(&core);
        assert_eq!(
            submit(&mut core, actor, redo, FrontendCommand::AgentRedo(target)),
            Err(InteropError::Redo(RedoError::Rejected(
                crate::editor::collab::receipt::RejectReason::Conflict
            )))
        );
        assert_eq!(state(&core), before);
        assert_eq!(core.compensation_for(target), Some(compensation));
        assert_eq!(core.redo_for(compensation), None);
    }
}
