//! Cross-cutting collaboration state-machine invariants and law suite.
//!
//! # Verified Laws
//!
//! 1. **Disjoint edit order-independence:** across all permutations of 2-actor
//!    and 3-actor disjoint concurrent edits (single-buffer and multi-buffer,
//!    mixing human, automated, and agent participants), final document text is
//!    identical regardless of submission order.
//! 2. **Touching and overlapping conflict rejection:** when concurrent edits
//!    touch (shared/adjacent endpoints or coincident insertions under
//!    closed-interval rules) or overlap, strictly one commits and conflicting
//!    submissions are rejected atomically with zero state mutation.
//! 3. **Receipt idempotence:** replaying or resubmitting an already-committed
//!    operation with the same identity and payload returns the exact original
//!    receipt without re-applying the edit, advancing revision, or altering
//!    text, journal, delivery, register, or undo state; conflicting payload
//!    reuse with the same request or transaction identity is rejected
//!    atomically.
//! 4. **Proposal approval invalidation:** pending and approved proposals become
//!    invalid and are rejected atomically if the target revision or region
//!    diverges (intervening edit, buffer close, proposer/approver permission or
//!    actor revocation, non-human approval attempt, or double-commit).
//! 5. **Selective undo and redo preservation:** selectively undoing one actor's
//!    commit preserves other actors' disjoint edits byte-for-byte, and redoing
//!    restores consistently; conflicting, unauthorized, already-undone,
//!    not-undone, closed-buffer, or expired undo/redo requests are rejected
//!    atomically without disturbing other actors' edits.
//! 6. **Replay after compaction and retention:** compacting the journal and
//!    retention window into a checkpoint and replaying retained tail events or
//!    operations produces observations (text, revision, delivery/catch-up,
//!    receipts, conflict behavior, subsequent edits) identical to uncompacted
//!    replay.
//! 7. **Concurrent linearizability:** multi-actor interleaved histories
//!    executed through [`super::concurrent::ConcurrentCoordinator`] admit a
//!    real-time-order-preserving linearization whose sequential execution agrees
//!    with both [`super::coordinator::Coordinator`] and the independent oracle.

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::{Arc, Barrier};
    use std::thread;

    use crate::editor::{
        collab::{
            access::{AccessError, AccessPolicy, Capability},
            checkpoints::{CheckpointError, CheckpointOp, CheckpointOutcome, CheckpointState},
            concurrent::{
                execute_sequential, find_linearization, AccessChange, ConcurrentCoordinator,
                ConcurrentError, ConcurrentOp, ConcurrentResponse, SnapshotView,
            },
            coordinator::{Coordinator, CoordinatorError, Operation},
            delivery::{self, CatchUp},
            journal::{CommitJournal, JournalError},
            proposal::{Proposal, ProposalError, RequestId},
            rebase::RebaseError,
            receipt::{CommitReceipt, RejectReason, SubmitOutcome},
            redo::{Redo, RedoError},
            registry::ActorRegistry,
            retention::RetentionError,
            role::ActorRole,
            selective_undo::{SelectiveUndo, UndoError},
            sequencer::Sequencer,
        },
        edit::{
            ids::{ActorId, BufferId, Nonce, TxnId},
            intent::EditIntent,
            patch::Patch,
            restore::{self, RestoreOutcome},
            splice::Splice,
            transaction::{Transaction, TransactionError},
        },
        test_support::{assert_law, Rng},
        text::{span::ByteSpan, utf8::Text},
        workspace::{
            buffer_lifecycle::ClosePolicy,
            catalog::BufferCatalog,
            registers::{RegisterKind, RegisterName},
            selections::Selection,
            session::WorkspaceSession,
            version::Revision,
            view::BufferView,
        },
    };

    const HUMAN_A: u64 = 1;
    const AUTOMATED_B: u64 = 2;
    const AGENT_C: u64 = 3;
    const HUMAN_D: u64 = 4;

    // =========================================================================
    // Section 1: Independent Reference / Oracle Model
    //
    // Uses only plain std primitives (String, BTreeMap, BTreeSet, Vec, u64,
    // usize) and arithmetic interval transport. Does NOT reuse production
    // Coordinator, Rebase, Footprint, Journal, SelectiveUndo, Redo, Access,
    // Registry, RetainedHistory, DeliveryLog, or Checkpoint internals.
    // =========================================================================

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum OracleRole {
        Human,
        Agent,
    }

    #[derive(Clone, Copy, Debug)]
    struct OracleUndoLink {
        root: (u64, u64),
        is_undo: bool,
        txn_id: (u64, u64),
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum OracleCap {
        Read,
        Propose,
        Review,
        Commit,
    }

    impl OracleCap {
        const ALL: [Self; 4] = [Self::Read, Self::Propose, Self::Review, Self::Commit];

        fn to_prod(self) -> Capability {
            match self {
                Self::Read => Capability::Read,
                Self::Propose => Capability::Propose,
                Self::Review => Capability::Review,
                Self::Commit => Capability::Commit,
            }
        }
    }

    impl OracleRole {
        fn to_prod(self) -> ActorRole {
            match self {
                Self::Human => ActorRole::Human,
                Self::Agent => ActorRole::Agent,
            }
        }

        fn from_prod(role: ActorRole) -> Self {
            match role {
                ActorRole::Human => Self::Human,
                ActorRole::Agent => Self::Agent,
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct OracleSplice {
        start: usize,
        end: usize,
        replacement: String,
    }

    impl OracleSplice {
        fn new(start: usize, end: usize, replacement: impl Into<String>) -> Self {
            Self {
                start,
                end,
                replacement: replacement.into(),
            }
        }
    }

    type OraclePatches = BTreeMap<u64, Vec<OracleSplice>>;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct OracleReceipt {
        actor: u64,
        nonce: u64,
        base: u64,
        previous: u64,
        committed: u64,
    }

    impl OracleReceipt {
        fn from_prod(receipt: CommitReceipt) -> Self {
            Self {
                actor: receipt.intent().actor().raw(),
                nonce: receipt.intent().nonce().raw(),
                base: receipt.intent().base().raw(),
                previous: receipt.previous().raw(),
                committed: receipt.committed().raw(),
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct OracleOutcome {
        receipt: Option<OracleReceipt>,
        buffer: Option<u64>,
    }

    impl OracleOutcome {
        fn from_prod(outcome: &CheckpointOutcome) -> Self {
            Self {
                receipt: outcome.receipt.map(OracleReceipt::from_prod),
                buffer: outcome.buffer.map(BufferId::raw),
            }
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum OracleClosePolicy {
        RequireClean,
        Discard,
    }

    impl OracleClosePolicy {
        fn to_prod(self) -> ClosePolicy {
            match self {
                Self::RequireClean => ClosePolicy::RequireClean,
                Self::Discard => ClosePolicy::Discard,
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum OracleOp {
        Open {
            actor: u64,
            nonce: u64,
            base: u64,
            name: String,
            text: String,
        },
        Close {
            actor: u64,
            nonce: u64,
            base: u64,
            buffer: u64,
            policy: OracleClosePolicy,
        },
        Edit {
            actor: u64,
            nonce: u64,
            base: u64,
            patches: OraclePatches,
        },
        Cut {
            actor: u64,
            nonce: u64,
            base: u64,
            buffer: u64,
            start: usize,
            end: usize,
            register: String,
        },
        Undo {
            actor: u64,
            target: (u64, u64),
            txn: (u64, u64),
        },
        Redo {
            actor: u64,
            target: (u64, u64),
            txn: (u64, u64),
        },
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum OracleErrorKind {
        RequestReuse,
        TxnReuse,
        InactiveActor,
        PermissionDenied,
        UnknownBuffer,
        BufferClosed,
        RevisionMismatch,
        BaseExpired,
        FutureRevision,
        Conflict,
        NoChange,
        DirtyBuffer,
        NameInUse,
        MissingProposal,
        Unapproved,
        StaleApproval,
        NotHuman,
        EmptyProposal,
        ApprovalMismatch,
        UnknownTarget,
        NotOwner,
        AlreadyUndone,
        NotUndone,
        InvalidTransaction,
        InvalidEdit,
    }

    fn classify_coordinator_error(error: &CoordinatorError) -> OracleErrorKind {
        match error {
            CoordinatorError::RequestReuse(_) => OracleErrorKind::RequestReuse,
            CoordinatorError::TransactionReuse(_) => OracleErrorKind::TxnReuse,
            CoordinatorError::Access(err) => classify_access_error(err),
            CoordinatorError::Proposal(err) => match err {
                ProposalError::Access(access) => classify_access_error(access),
                ProposalError::NotHuman(_) => OracleErrorKind::NotHuman,
                ProposalError::EmptyProposal => OracleErrorKind::EmptyProposal,
                ProposalError::ApprovalMismatch => OracleErrorKind::ApprovalMismatch,
                ProposalError::RevisionMismatch { .. } => OracleErrorKind::RevisionMismatch,
                ProposalError::Transaction(txn_err) => classify_txn_error(txn_err),
            },
            CoordinatorError::Checkpoint(err) => match err {
                CheckpointError::InactiveActor => OracleErrorKind::InactiveActor,
                CheckpointError::Gone(_) => OracleErrorKind::BufferClosed,
                CheckpointError::UnknownBuffer(_) => OracleErrorKind::UnknownBuffer,
                CheckpointError::NameInUse => OracleErrorKind::NameInUse,
                CheckpointError::Dirty(_) => OracleErrorKind::DirtyBuffer,
                CheckpointError::BaseExpired { .. } => OracleErrorKind::BaseExpired,
                CheckpointError::FutureRevision { .. } => OracleErrorKind::FutureRevision,
                CheckpointError::RequestIdReuse(_) => OracleErrorKind::RequestReuse,
                CheckpointError::TxnIdReuse(_) => OracleErrorKind::TxnReuse,
                CheckpointError::Retention(ret) => match ret {
                    RetentionError::BaseExpired { .. } => OracleErrorKind::BaseExpired,
                    RetentionError::FutureRevision { .. } => OracleErrorKind::FutureRevision,
                    RetentionError::TxnIdReuse(_) => OracleErrorKind::TxnReuse,
                    RetentionError::RequestIdReuse(_) => OracleErrorKind::RequestReuse,
                    RetentionError::Rebase(rebase_err) => classify_rebase_error(rebase_err),
                    RetentionError::InvalidCommit => OracleErrorKind::InvalidEdit,
                },
                CheckpointError::Rebase(rebase_err) => classify_rebase_error(rebase_err),
                CheckpointError::Transaction(txn_err) => classify_txn_error(txn_err),
                CheckpointError::Delivery(_)
                | CheckpointError::Workspace(_)
                | CheckpointError::Journal(_)
                | CheckpointError::InconsistentState
                | CheckpointError::IdExhausted => OracleErrorKind::InvalidEdit,
            },
            CoordinatorError::Register(_) => OracleErrorKind::InvalidEdit,
            CoordinatorError::MissingProposal(_) => OracleErrorKind::MissingProposal,
            CoordinatorError::Unapproved(_) => OracleErrorKind::Unapproved,
            CoordinatorError::StaleApproval(_) => OracleErrorKind::StaleApproval,
            CoordinatorError::RevisionMismatch { .. } => OracleErrorKind::RevisionMismatch,
            CoordinatorError::UnknownTarget(_) => OracleErrorKind::UnknownTarget,
            CoordinatorError::NotOwner(_) => OracleErrorKind::NotOwner,
            CoordinatorError::InvalidTransaction(_) => OracleErrorKind::InvalidTransaction,
            CoordinatorError::AlreadyUndone(_) => OracleErrorKind::AlreadyUndone,
            CoordinatorError::NotUndone(_) => OracleErrorKind::NotUndone,
            CoordinatorError::NoChange => OracleErrorKind::NoChange,
        }
    }

    fn classify_access_error(err: &AccessError) -> OracleErrorKind {
        match err {
            AccessError::InactiveActor(_) => OracleErrorKind::InactiveActor,
            AccessError::UnknownBuffer(_) => OracleErrorKind::UnknownBuffer,
            AccessError::BufferClosed(_) => OracleErrorKind::BufferClosed,
            AccessError::Denied { .. } => OracleErrorKind::PermissionDenied,
            AccessError::Transaction(txn_err) => classify_txn_error(txn_err),
        }
    }

    fn classify_txn_error(err: &TransactionError) -> OracleErrorKind {
        match err {
            TransactionError::RevisionMismatch { .. } => OracleErrorKind::RevisionMismatch,
            TransactionError::UnknownBuffer(_) => OracleErrorKind::UnknownBuffer,
            TransactionError::BufferClosed(_) => OracleErrorKind::BufferClosed,
            TransactionError::DuplicateBufferId(_)
            | TransactionError::Patch { .. }
            | TransactionError::RevisionOverflow => OracleErrorKind::InvalidEdit,
        }
    }

    fn classify_rebase_error(err: &RebaseError) -> OracleErrorKind {
        match err {
            RebaseError::Conflict(_) => OracleErrorKind::Conflict,
            RebaseError::RevisionMismatch { .. } => OracleErrorKind::RevisionMismatch,
            RebaseError::Transaction(txn_err) => classify_txn_error(txn_err),
            RebaseError::RevisionOverflow
            | RebaseError::TargetMismatch
            | RebaseError::DisconnectedChain
            | RebaseError::Anchor(_)
            | RebaseError::Span(_)
            | RebaseError::Patch { .. } => OracleErrorKind::InvalidEdit,
        }
    }

    /// Closed-interval intersection test: touching endpoints and coincident
    /// insertions conflict (`left.start <= right.end && right.start <= left.end`).
    fn closed_intervals_conflict(left: (usize, usize), right: (usize, usize)) -> bool {
        left.0 <= right.1 && right.0 <= left.1
    }

    /// Plain-string patch evaluator that computes both forward text and exact
    /// inverse splices without touching production edit/patch code.
    fn apply_splices_plain(
        text: &str,
        splices: &[OracleSplice],
    ) -> Result<(String, Vec<OracleSplice>), OracleErrorKind> {
        let mut sorted = splices.to_vec();
        sorted.sort_by_key(|s| (s.start, s.end));
        for pair in sorted.windows(2) {
            if pair[0].end >= pair[1].start {
                return Err(OracleErrorKind::InvalidEdit);
            }
        }
        for s in &sorted {
            if s.start > s.end
                || s.end > text.len()
                || !text.is_char_boundary(s.start)
                || !text.is_char_boundary(s.end)
            {
                return Err(OracleErrorKind::InvalidEdit);
            }
        }
        let mut out = String::new();
        let mut inverse = Vec::with_capacity(sorted.len());
        let mut cursor = 0;
        for s in &sorted {
            out.push_str(&text[cursor..s.start]);
            let inv_start = out.len();
            out.push_str(&s.replacement);
            let inv_end = out.len();
            inverse.push(OracleSplice {
                start: inv_start,
                end: inv_end,
                replacement: text[s.start..s.end].to_owned(),
            });
            cursor = s.end;
        }
        out.push_str(&text[cursor..]);
        Ok((out, inverse))
    }

    /// Pure arithmetic coordinate transport across one committed patch on the
    /// same buffer. Rejects any closed-interval contact or overlap.
    fn transport_splices_plain(
        proposal: &[OracleSplice],
        committed: &[OracleSplice],
    ) -> Result<Vec<OracleSplice>, OracleErrorKind> {
        for p in proposal {
            for c in committed {
                if closed_intervals_conflict((p.start, p.end), (c.start, c.end)) {
                    return Err(OracleErrorKind::Conflict);
                }
            }
        }
        let mut moved = Vec::with_capacity(proposal.len());
        for p in proposal {
            let mut shift: isize = 0;
            for c in committed {
                if c.end < p.start {
                    shift += c.replacement.len() as isize - (c.end - c.start) as isize;
                }
            }
            let new_start = usize::try_from(p.start as isize + shift)
                .map_err(|_| OracleErrorKind::InvalidEdit)?;
            let new_end = usize::try_from(p.end as isize + shift)
                .map_err(|_| OracleErrorKind::InvalidEdit)?;
            moved.push(OracleSplice {
                start: new_start,
                end: new_end,
                replacement: p.replacement.clone(),
            });
        }
        Ok(moved)
    }

    fn canonicalize_patches(patches: &OraclePatches) -> OraclePatches {
        let mut out = BTreeMap::new();
        for (&buf, splices) in patches {
            let mut sorted = splices.clone();
            sorted.sort_by_key(|s| (s.start, s.end));
            out.insert(buf, sorted);
        }
        out
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum OracleStepKind {
        Open(u64, String),
        Close(u64),
        Edit(OraclePatches),
    }

    #[derive(Clone, Debug)]
    struct OracleProposal {
        proposer: u64,
        nonce: u64,
        base: u64,
        patches: OraclePatches,
        approval: Option<(u64, u64)>,
        reviewers: BTreeSet<u64>,
    }

    #[derive(Clone, Debug)]
    struct OracleUndoRecord {
        owner: u64,
        root: (u64, u64),
        is_compensation: bool,
        undone_by: Option<(u64, u64)>,
        inverse: Option<OraclePatches>,
        inverse_base: u64,
    }

    #[derive(Clone, Debug)]
    struct OracleState {
        revision: u64,
        floor: u64,
        delivery_floor: u64,
        next_buffer_id: u64,
        live: BTreeMap<u64, String>,
        tombstones: BTreeSet<u64>,
        names: BTreeMap<String, u64>,
        clean_text: BTreeMap<u64, String>,
        active_actors: BTreeMap<u64, OracleRole>,
        revoked_actors: BTreeSet<u64>,
        default_grants: BTreeSet<OracleCap>,
        actor_grants: BTreeSet<(u64, OracleCap)>,
        buffer_grants: BTreeSet<(u64, u64, OracleCap)>,
        journal: Vec<OracleReceipt>,
        receipt_by_request: BTreeMap<u64, (OracleOp, OracleOutcome)>,
        receipt_by_txn: BTreeMap<(u64, u64), OracleReceipt>,
        edit_receipt_by_request: BTreeMap<u64, OracleReceipt>,
        pending_proposals: BTreeMap<u64, OracleProposal>,
        approved_commits: BTreeMap<u64, (BTreeSet<u64>, OracleOutcome)>,
        undo_records: BTreeMap<(u64, u64), OracleUndoRecord>,
        history_snapshots: BTreeMap<u64, BTreeMap<u64, String>>,
        history_tombstones: BTreeMap<u64, BTreeSet<u64>>,
        history_steps: BTreeMap<u64, OracleStepKind>,
        delivery_events: Vec<(OracleReceipt, OraclePatches)>,
        registers: BTreeMap<(u64, String), String>,
        rings: BTreeMap<u64, Vec<String>>,
    }

    impl OracleState {
        fn new() -> Self {
            let mut history_snapshots = BTreeMap::new();
            history_snapshots.insert(0, BTreeMap::new());
            let mut history_tombstones = BTreeMap::new();
            history_tombstones.insert(0, BTreeSet::new());
            Self {
                revision: 0,
                floor: 0,
                delivery_floor: 0,
                next_buffer_id: 0,
                live: BTreeMap::new(),
                tombstones: BTreeSet::new(),
                names: BTreeMap::new(),
                clean_text: BTreeMap::new(),
                active_actors: BTreeMap::new(),
                revoked_actors: BTreeSet::new(),
                default_grants: BTreeSet::new(),
                actor_grants: BTreeSet::new(),
                buffer_grants: BTreeSet::new(),
                journal: Vec::new(),
                receipt_by_request: BTreeMap::new(),
                receipt_by_txn: BTreeMap::new(),
                edit_receipt_by_request: BTreeMap::new(),
                pending_proposals: BTreeMap::new(),
                approved_commits: BTreeMap::new(),
                undo_records: BTreeMap::new(),
                history_snapshots,
                history_tombstones,
                history_steps: BTreeMap::new(),
                delivery_events: Vec::new(),
                registers: BTreeMap::new(),
                rings: BTreeMap::new(),
            }
        }

        fn register_actor(&mut self, actor: u64, role: OracleRole) -> Result<(), OracleErrorKind> {
            if self.revoked_actors.contains(&actor) {
                return Err(OracleErrorKind::InactiveActor);
            }
            if let Some(&existing) = self.active_actors.get(&actor) {
                return if existing == role {
                    Ok(())
                } else {
                    Err(OracleErrorKind::InvalidEdit)
                };
            }
            self.active_actors.insert(actor, role);
            Ok(())
        }

        fn revoke_actor(&mut self, actor: u64) -> Result<(), OracleErrorKind> {
            if self.revoked_actors.contains(&actor) || self.active_actors.remove(&actor).is_none() {
                return Err(OracleErrorKind::InactiveActor);
            }
            self.revoked_actors.insert(actor);
            Ok(())
        }

        fn grant_default(&mut self, cap: OracleCap) {
            self.default_grants.insert(cap);
        }

        fn revoke_default(&mut self, cap: OracleCap) {
            self.default_grants.remove(&cap);
        }

        fn grant_actor(&mut self, actor: u64, cap: OracleCap) {
            self.actor_grants.insert((actor, cap));
        }

        fn revoke_actor_cap(&mut self, actor: u64, cap: OracleCap) {
            self.actor_grants.remove(&(actor, cap));
        }

        fn grant_buffer(&mut self, actor: u64, buffer: u64, cap: OracleCap) {
            self.buffer_grants.insert((actor, buffer, cap));
        }

        fn revoke_buffer(&mut self, actor: u64, buffer: u64, cap: OracleCap) {
            self.buffer_grants.remove(&(actor, buffer, cap));
        }

        fn is_granted(&self, actor: u64, buffer: u64, cap: OracleCap) -> bool {
            self.default_grants.contains(&cap)
                || self.actor_grants.contains(&(actor, cap))
                || self.buffer_grants.contains(&(actor, buffer, cap))
        }

        fn authorize(
            &self,
            actor: u64,
            cap: OracleCap,
            buffers: impl IntoIterator<Item = u64>,
        ) -> Result<(), OracleErrorKind> {
            if !self.active_actors.contains_key(&actor) {
                return Err(OracleErrorKind::InactiveActor);
            }
            let unique: BTreeSet<u64> = buffers.into_iter().collect();
            for buf in unique {
                if !self.live.contains_key(&buf) {
                    return if self.tombstones.contains(&buf) {
                        Err(OracleErrorKind::BufferClosed)
                    } else {
                        Err(OracleErrorKind::UnknownBuffer)
                    };
                }
                if !self.is_granted(actor, buf, cap) {
                    return Err(OracleErrorKind::PermissionDenied);
                }
            }
            Ok(())
        }

        fn preview_at_current(
            &self,
            base: u64,
            patches: &OraclePatches,
        ) -> Result<(BTreeMap<u64, String>, OraclePatches, bool), OracleErrorKind> {
            if base != self.revision {
                return Err(OracleErrorKind::RevisionMismatch);
            }
            let mut next_live = self.live.clone();
            let mut inverses = BTreeMap::new();
            let mut is_noop = true;
            for (&buf, splices) in patches {
                let Some(text) = self.live.get(&buf) else {
                    return if self.tombstones.contains(&buf) {
                        Err(OracleErrorKind::BufferClosed)
                    } else {
                        Err(OracleErrorKind::UnknownBuffer)
                    };
                };
                let (updated, inverse) = apply_splices_plain(text, splices)?;
                is_noop &= updated == *text;
                next_live.insert(buf, updated);
                inverses.insert(buf, inverse);
            }
            Ok((next_live, inverses, is_noop))
        }

        fn rebase_patches(
            &self,
            base: u64,
            patches: &OraclePatches,
        ) -> Result<OraclePatches, OracleErrorKind> {
            if base < self.floor {
                return Err(OracleErrorKind::BaseExpired);
            }
            if base > self.revision {
                return Err(OracleErrorKind::FutureRevision);
            }
            let base_live = self
                .history_snapshots
                .get(&base)
                .ok_or(OracleErrorKind::BaseExpired)?;
            let base_tombstones = self
                .history_tombstones
                .get(&base)
                .ok_or(OracleErrorKind::BaseExpired)?;
            for (&buf, splices) in patches {
                let Some(text) = base_live.get(&buf) else {
                    return if base_tombstones.contains(&buf) {
                        Err(OracleErrorKind::BufferClosed)
                    } else {
                        Err(OracleErrorKind::UnknownBuffer)
                    };
                };
                apply_splices_plain(text, splices)?;
            }
            let mut current = canonicalize_patches(patches);
            for rev in (base + 1)..=self.revision {
                let step = self
                    .history_steps
                    .get(&rev)
                    .ok_or(OracleErrorKind::BaseExpired)?;
                match step {
                    OracleStepKind::Open(id, _) => {
                        if current.contains_key(id) {
                            return Err(OracleErrorKind::UnknownBuffer);
                        }
                    }
                    OracleStepKind::Close(id) => {
                        if current.contains_key(id) {
                            return Err(OracleErrorKind::BufferClosed);
                        }
                    }
                    OracleStepKind::Edit(committed) => {
                        for (&buf, prop_splices) in &mut current {
                            if let Some(comm_splices) = committed.get(&buf) {
                                *prop_splices =
                                    transport_splices_plain(prop_splices, comm_splices)?;
                            }
                        }
                    }
                }
            }
            Ok(current)
        }

        fn propose(
            &mut self,
            request: u64,
            proposer: u64,
            nonce: u64,
            base: u64,
            patches: OraclePatches,
        ) -> Result<(), OracleErrorKind> {
            let patches = canonicalize_patches(&patches);
            if let Some(existing) = self.pending_proposals.get(&request) {
                return if existing.proposer == proposer
                    && existing.nonce == nonce
                    && existing.base == base
                    && existing.patches == patches
                {
                    Ok(())
                } else {
                    Err(OracleErrorKind::RequestReuse)
                };
            }
            let expected_op = OracleOp::Edit {
                actor: proposer,
                nonce,
                base,
                patches: patches.clone(),
            };
            if let Some((old_op, _)) = self.receipt_by_request.get(&request) {
                if self.approved_commits.contains_key(&request) && *old_op == expected_op {
                    return Ok(());
                }
                return Err(OracleErrorKind::RequestReuse);
            }
            self.authorize(proposer, OracleCap::Propose, patches.keys().copied())?;
            self.preview_at_current(base, &patches)?;
            self.pending_proposals.insert(
                request,
                OracleProposal {
                    proposer,
                    nonce,
                    base,
                    patches,
                    approval: None,
                    reviewers: BTreeSet::new(),
                },
            );
            Ok(())
        }

        fn approve(&mut self, request: u64, reviewer: u64) -> Result<(), OracleErrorKind> {
            if let Some((reviewers, _)) = self.approved_commits.get(&request) {
                return if reviewers.contains(&reviewer) {
                    Ok(())
                } else {
                    Err(OracleErrorKind::ApprovalMismatch)
                };
            }
            let pending = self
                .pending_proposals
                .get(&request)
                .ok_or(OracleErrorKind::MissingProposal)?;
            if pending.reviewers.contains(&reviewer) {
                return Ok(());
            }
            if pending.patches.is_empty() {
                return Err(OracleErrorKind::EmptyProposal);
            }
            self.authorize(reviewer, OracleCap::Review, pending.patches.keys().copied())?;
            if self.active_actors.get(&reviewer) != Some(&OracleRole::Human) {
                return Err(OracleErrorKind::NotHuman);
            }
            self.preview_at_current(pending.base, &pending.patches)?;
            let current_rev = self.revision;
            let pending = self
                .pending_proposals
                .get_mut(&request)
                .expect("pending proposal");
            pending.approval = Some((reviewer, current_rev));
            pending.reviewers.insert(reviewer);
            Ok(())
        }

        fn commit_proposal(&mut self, request: u64) -> Result<OracleOutcome, OracleErrorKind> {
            if let Some((_, outcome)) = self.approved_commits.get(&request) {
                return Ok(*outcome);
            }
            let pending = self
                .pending_proposals
                .get(&request)
                .ok_or(OracleErrorKind::MissingProposal)?
                .clone();
            let (approver, approved_rev) = pending.approval.ok_or(OracleErrorKind::Unapproved)?;
            if approved_rev != self.revision {
                return Err(OracleErrorKind::StaleApproval);
            }
            if pending.patches.is_empty() {
                return Err(OracleErrorKind::EmptyProposal);
            }
            self.authorize(approver, OracleCap::Review, pending.patches.keys().copied())?;
            if self.active_actors.get(&approver) != Some(&OracleRole::Human) {
                return Err(OracleErrorKind::NotHuman);
            }
            self.preview_at_current(pending.base, &pending.patches)?;
            let op = OracleOp::Edit {
                actor: pending.proposer,
                nonce: pending.nonce,
                base: pending.base,
                patches: pending.patches,
            };
            let outcome = self.apply_inner(request, op, true)?;
            self.pending_proposals.remove(&request);
            self.approved_commits
                .insert(request, (pending.reviewers, outcome));
            Ok(outcome)
        }

        fn apply(&mut self, request: u64, op: OracleOp) -> Result<OracleOutcome, OracleErrorKind> {
            self.apply_inner(request, op, false)
        }

        fn apply_inner(
            &mut self,
            request: u64,
            op: OracleOp,
            approved: bool,
        ) -> Result<OracleOutcome, OracleErrorKind> {
            let op = match op {
                OracleOp::Edit {
                    actor,
                    nonce,
                    base,
                    patches,
                } => OracleOp::Edit {
                    actor,
                    nonce,
                    base,
                    patches: canonicalize_patches(&patches),
                },
                other => other,
            };
            if self.pending_proposals.contains_key(&request) && !approved {
                return Err(OracleErrorKind::RequestReuse);
            }
            if let Some((old_op, outcome)) = self.receipt_by_request.get(&request) {
                return if *old_op == op {
                    Ok(*outcome)
                } else {
                    Err(OracleErrorKind::RequestReuse)
                };
            }
            match &op {
                OracleOp::Open {
                    actor,
                    nonce,
                    base,
                    name,
                    text,
                } => {
                    let id = self.next_buffer_id;
                    if !self.active_actors.contains_key(actor) {
                        return Err(OracleErrorKind::InactiveActor);
                    }
                    if !self.is_granted(*actor, id, OracleCap::Commit) {
                        return Err(OracleErrorKind::PermissionDenied);
                    }
                    if *base != self.revision {
                        return Err(OracleErrorKind::RevisionMismatch);
                    }
                    if self.receipt_by_txn.contains_key(&(*actor, *nonce)) {
                        return Err(OracleErrorKind::TxnReuse);
                    }
                    if self.names.contains_key(name) {
                        return Err(OracleErrorKind::NameInUse);
                    }
                    let previous = self.revision;
                    let committed = previous + 1;
                    let receipt = OracleReceipt {
                        actor: *actor,
                        nonce: *nonce,
                        base: *base,
                        previous,
                        committed,
                    };
                    self.revision = committed;
                    self.delivery_floor = committed;
                    self.delivery_events.clear();
                    self.next_buffer_id += 1;
                    self.live.insert(id, text.clone());
                    self.names.insert(name.clone(), id);
                    self.clean_text.insert(id, text.clone());
                    self.history_snapshots.insert(committed, self.live.clone());
                    self.history_tombstones
                        .insert(committed, self.tombstones.clone());
                    self.history_steps
                        .insert(committed, OracleStepKind::Open(id, text.clone()));
                    self.journal.push(receipt);
                    self.receipt_by_txn.insert((*actor, *nonce), receipt);
                    let outcome = OracleOutcome {
                        receipt: Some(receipt),
                        buffer: Some(id),
                    };
                    self.receipt_by_request.insert(request, (op, outcome));
                    Ok(outcome)
                }
                OracleOp::Close {
                    actor,
                    nonce,
                    base,
                    buffer,
                    policy,
                } => {
                    self.authorize(*actor, OracleCap::Commit, [*buffer])?;
                    if *base != self.revision {
                        return Err(OracleErrorKind::RevisionMismatch);
                    }
                    if self.receipt_by_txn.contains_key(&(*actor, *nonce)) {
                        return Err(OracleErrorKind::TxnReuse);
                    }
                    let is_dirty = self.clean_text.get(buffer) != self.live.get(buffer);
                    if *policy == OracleClosePolicy::RequireClean && is_dirty {
                        return Err(OracleErrorKind::DirtyBuffer);
                    }
                    let previous = self.revision;
                    let committed = previous + 1;
                    let receipt = OracleReceipt {
                        actor: *actor,
                        nonce: *nonce,
                        base: *base,
                        previous,
                        committed,
                    };
                    self.revision = committed;
                    self.delivery_floor = committed;
                    self.delivery_events.clear();
                    self.live.remove(buffer);
                    self.tombstones.insert(*buffer);
                    self.names.retain(|_, id| id != buffer);
                    self.clean_text.remove(buffer);
                    self.history_snapshots.insert(committed, self.live.clone());
                    self.history_tombstones
                        .insert(committed, self.tombstones.clone());
                    self.history_steps
                        .insert(committed, OracleStepKind::Close(*buffer));
                    self.journal.push(receipt);
                    self.receipt_by_txn.insert((*actor, *nonce), receipt);
                    let outcome = OracleOutcome {
                        receipt: Some(receipt),
                        buffer: Some(*buffer),
                    };
                    self.receipt_by_request.insert(request, (op, outcome));
                    Ok(outcome)
                }
                OracleOp::Edit {
                    actor,
                    nonce,
                    base,
                    patches,
                } => {
                    let outcome = self.commit_edit(
                        request,
                        *actor,
                        *nonce,
                        *base,
                        patches,
                        None,
                        None,
                        op.clone(),
                    )?;
                    Ok(outcome)
                }
                OracleOp::Cut {
                    actor,
                    nonce,
                    base,
                    buffer,
                    start,
                    end,
                    register,
                } => {
                    self.authorize(*actor, OracleCap::Read, [*buffer])?;
                    if *base != self.revision {
                        return Err(OracleErrorKind::InvalidEdit);
                    }
                    let text = self
                        .live
                        .get(buffer)
                        .ok_or(OracleErrorKind::UnknownBuffer)?;
                    if *start > *end
                        || *end > text.len()
                        || !text.is_char_boundary(*start)
                        || !text.is_char_boundary(*end)
                    {
                        return Err(OracleErrorKind::InvalidEdit);
                    }
                    let cut_text = text[*start..*end].to_owned();
                    let patches =
                        BTreeMap::from([(*buffer, vec![OracleSplice::new(*start, *end, "")])]);
                    let outcome = self.commit_edit(
                        request,
                        *actor,
                        *nonce,
                        *base,
                        &patches,
                        None,
                        Some((*actor, register.clone(), cut_text)),
                        op.clone(),
                    )?;
                    Ok(outcome)
                }
                OracleOp::Undo { actor, target, txn } | OracleOp::Redo { actor, target, txn } => {
                    if txn.0 != *actor {
                        return Err(OracleErrorKind::InvalidTransaction);
                    }
                    let record = self
                        .undo_records
                        .get(target)
                        .ok_or(OracleErrorKind::UnknownTarget)?
                        .clone();
                    if record.owner != *actor {
                        return Err(OracleErrorKind::NotOwner);
                    }
                    let is_undo = matches!(op, OracleOp::Undo { .. });
                    if is_undo && record.is_compensation {
                        return Err(OracleErrorKind::InvalidTransaction);
                    }
                    let root = record.root;
                    let root_record = self
                        .undo_records
                        .get(&root)
                        .ok_or(OracleErrorKind::UnknownTarget)?
                        .clone();
                    let source = if is_undo {
                        if root_record.undone_by.is_some() {
                            return Err(OracleErrorKind::AlreadyUndone);
                        }
                        root_record
                    } else {
                        let comp = root_record.undone_by.ok_or(OracleErrorKind::NotUndone)?;
                        if record.is_compensation && *target != comp {
                            return Err(OracleErrorKind::NotUndone);
                        }
                        self.undo_records
                            .get(&comp)
                            .ok_or(OracleErrorKind::UnknownTarget)?
                            .clone()
                    };
                    let inverse_patches = source
                        .inverse
                        .as_ref()
                        .ok_or(OracleErrorKind::BaseExpired)?;
                    let outcome = self.commit_edit(
                        request,
                        txn.0,
                        txn.1,
                        source.inverse_base,
                        inverse_patches,
                        Some(OracleUndoLink {
                            root,
                            is_undo,
                            txn_id: *txn,
                        }),
                        None,
                        op.clone(),
                    )?;
                    Ok(outcome)
                }
            }
        }

        #[allow(clippy::too_many_arguments)]
        fn commit_edit(
            &mut self,
            request: u64,
            actor: u64,
            nonce: u64,
            base: u64,
            patches: &OraclePatches,
            undo_link: Option<OracleUndoLink>,
            register_update: Option<(u64, String, String)>,
            original_op: OracleOp,
        ) -> Result<OracleOutcome, OracleErrorKind> {
            self.authorize(actor, OracleCap::Commit, patches.keys().copied())?;
            if self.receipt_by_txn.contains_key(&(actor, nonce)) {
                return Err(OracleErrorKind::TxnReuse);
            }
            let applied = self.rebase_patches(base, patches)?;
            let (next_live, inverses, is_noop) =
                self.preview_at_current(self.revision, &applied)?;
            if is_noop {
                return Err(OracleErrorKind::NoChange);
            }
            let previous = self.revision;
            let committed = previous + 1;
            let receipt = OracleReceipt {
                actor,
                nonce,
                base,
                previous,
                committed,
            };
            self.revision = committed;
            self.live = next_live;
            self.history_snapshots.insert(committed, self.live.clone());
            self.history_tombstones
                .insert(committed, self.tombstones.clone());
            self.history_steps
                .insert(committed, OracleStepKind::Edit(applied.clone()));
            self.delivery_events.push((receipt, applied));
            self.journal.push(receipt);
            self.receipt_by_txn.insert((actor, nonce), receipt);
            self.edit_receipt_by_request.insert(request, receipt);
            let root = undo_link.map_or((actor, nonce), |link| link.root);
            let is_comp = undo_link.is_some_and(|link| link.is_undo);
            self.undo_records.insert(
                (actor, nonce),
                OracleUndoRecord {
                    owner: actor,
                    root,
                    is_compensation: is_comp,
                    undone_by: None,
                    inverse: Some(inverses.clone()),
                    inverse_base: committed,
                },
            );
            if let Some(link) = undo_link {
                let root_entry = self.undo_records.get_mut(&link.root).expect("root exists");
                root_entry.undone_by = link.is_undo.then_some(link.txn_id);
                if !link.is_undo {
                    root_entry.inverse = Some(inverses);
                    root_entry.inverse_base = committed;
                }
            }
            if let Some((reg_actor, reg_name, reg_val)) = register_update {
                self.registers
                    .insert((reg_actor, reg_name), reg_val.clone());
                self.rings.entry(reg_actor).or_default().insert(0, reg_val);
            }
            let outcome = OracleOutcome {
                receipt: Some(receipt),
                buffer: None,
            };
            self.receipt_by_request
                .insert(request, (original_op, outcome));
            Ok(outcome)
        }

        fn compact(&mut self, target_floor: u64) -> Result<(), OracleErrorKind> {
            if target_floor > self.revision {
                return Err(OracleErrorKind::FutureRevision);
            }
            let new_floor = self.floor.max(target_floor);
            self.floor = new_floor;
            let new_delivery_floor = self.delivery_floor.max(new_floor);
            let drop_count = usize::try_from(new_delivery_floor - self.delivery_floor)
                .expect("delivery floor diff fits usize");
            if drop_count > 0 {
                self.delivery_events.drain(..drop_count);
            }
            self.delivery_floor = new_delivery_floor;
            self.history_snapshots.retain(|&r, _| r >= new_floor);
            self.history_tombstones.retain(|&r, _| r >= new_floor);
            self.history_steps.retain(|&r, _| r > new_floor);
            for rec in self.undo_records.values_mut() {
                if rec.inverse_base <= new_floor {
                    rec.inverse = None;
                }
            }
            Ok(())
        }

        fn history_replay(&self, base: u64) -> Result<BTreeMap<u64, String>, OracleErrorKind> {
            if base < self.floor {
                return Err(OracleErrorKind::BaseExpired);
            }
            if base > self.revision {
                return Err(OracleErrorKind::FutureRevision);
            }
            let mut live = self
                .history_snapshots
                .get(&base)
                .ok_or(OracleErrorKind::BaseExpired)?
                .clone();
            for rev in (base + 1)..=self.revision {
                match self
                    .history_steps
                    .get(&rev)
                    .ok_or(OracleErrorKind::BaseExpired)?
                {
                    OracleStepKind::Open(id, text) => {
                        live.insert(*id, text.clone());
                    }
                    OracleStepKind::Close(id) => {
                        live.remove(id);
                    }
                    OracleStepKind::Edit(patches) => {
                        for (&buf, splices) in patches {
                            let cur = live.get(&buf).ok_or(OracleErrorKind::UnknownBuffer)?;
                            let (next, _) = apply_splices_plain(cur, splices)?;
                            live.insert(buf, next);
                        }
                    }
                }
            }
            Ok(live)
        }

        fn delivery_replay_from_floor(&self) -> BTreeMap<u64, String> {
            let mut live = self
                .history_snapshots
                .get(&self.delivery_floor)
                .expect("delivery floor snapshot")
                .clone();
            for (_, patches) in &self.delivery_events {
                for (&buf, splices) in patches {
                    let cur = live.get(&buf).expect("live buffer during delivery replay");
                    let (next, _) = apply_splices_plain(cur, splices).expect("valid delivery");
                    live.insert(buf, next);
                }
            }
            live
        }

        fn dirty_buffers(&self) -> BTreeSet<u64> {
            self.live
                .iter()
                .filter(|&(id, text)| self.clean_text.get(id) != Some(text))
                .map(|(&id, _)| id)
                .collect()
        }
    }

    // =========================================================================
    // Section 2: Observable State & State-Drift Comparator
    // =========================================================================

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct StateObservation {
        revision: u64,
        floor: u64,
        delivery_floor: u64,
        live_buffers: BTreeMap<u64, String>,
        tombstones: BTreeSet<u64>,
        names: BTreeMap<String, u64>,
        dirty_buffers: BTreeSet<u64>,
        active_actors: BTreeMap<u64, OracleRole>,
        revoked_actors: BTreeSet<u64>,
        journal: Vec<OracleReceipt>,
        receipt_by_request: BTreeMap<u64, OracleReceipt>,
        receipt_by_txn: BTreeMap<(u64, u64), OracleReceipt>,
        edit_receipt_by_request: BTreeMap<u64, OracleReceipt>,
        delivery_event_receipts: Vec<OracleReceipt>,
        delivery_replay_catalog: BTreeMap<u64, String>,
        history_replay_catalog: BTreeMap<u64, String>,
        expired_catch_up_snapshot: Option<BTreeMap<u64, String>>,
        compaction_floor_rejects_expired: bool,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum StateDrift {
        Revision {
            expected: u64,
            actual: u64,
        },
        Floor {
            expected: u64,
            actual: u64,
        },
        DeliveryFloor {
            expected: u64,
            actual: u64,
        },
        LiveBufferSet {
            expected: BTreeSet<u64>,
            actual: BTreeSet<u64>,
        },
        LiveBufferText {
            buffer: u64,
            expected: String,
            actual: String,
        },
        Tombstones {
            expected: BTreeSet<u64>,
            actual: BTreeSet<u64>,
        },
        Names {
            expected: BTreeMap<String, u64>,
            actual: BTreeMap<String, u64>,
        },
        DirtyBuffers {
            expected: BTreeSet<u64>,
            actual: BTreeSet<u64>,
        },
        ActiveActors {
            expected: BTreeMap<u64, OracleRole>,
            actual: BTreeMap<u64, OracleRole>,
        },
        RevokedActors {
            expected: BTreeSet<u64>,
            actual: BTreeSet<u64>,
        },
        Journal {
            expected: Vec<OracleReceipt>,
            actual: Vec<OracleReceipt>,
        },
        ReceiptByRequest {
            expected: BTreeMap<u64, OracleReceipt>,
            actual: BTreeMap<u64, OracleReceipt>,
        },
        ReceiptByTxn {
            expected: BTreeMap<(u64, u64), OracleReceipt>,
            actual: BTreeMap<(u64, u64), OracleReceipt>,
        },
        EditReceiptByRequest {
            expected: BTreeMap<u64, OracleReceipt>,
            actual: BTreeMap<u64, OracleReceipt>,
        },
        DeliveryEventReceipts {
            expected: Vec<OracleReceipt>,
            actual: Vec<OracleReceipt>,
        },
        DeliveryReplayCatalog {
            expected: BTreeMap<u64, String>,
            actual: BTreeMap<u64, String>,
        },
        HistoryReplayCatalog {
            expected: BTreeMap<u64, String>,
            actual: BTreeMap<u64, String>,
        },
        ExpiredCatchUpSnapshot {
            expected: Option<BTreeMap<u64, String>>,
            actual: Option<BTreeMap<u64, String>>,
        },
        CompactionExpiryRejection {
            expected: bool,
            actual: bool,
        },
    }

    fn catalog_to_map(catalog: &BufferCatalog) -> BTreeMap<u64, String> {
        catalog
            .live_ids()
            .into_iter()
            .map(|id| (id.raw(), catalog.get(id).expect("live").as_str().to_owned()))
            .collect()
    }

    fn observe_coordinator(
        coord: &Coordinator,
        known_actors: &[u64],
        known_requests: &[u64],
        known_txns: &[(u64, u64)],
    ) -> StateObservation {
        let ws = coord.workspace();
        let snap = coord.snapshot();
        let cat = snap.catalog();
        let revision = snap.revision().raw();
        let floor = ws.floor().raw();
        let delivery_floor = ws.delivery().floor().raw();
        let live_buffers = catalog_to_map(cat);
        let tombstones: BTreeSet<u64> = cat
            .tombstoned_ids()
            .into_iter()
            .map(BufferId::raw)
            .collect();
        let names: BTreeMap<String, u64> = ws
            .names()
            .iter()
            .map(|(k, v)| (k.clone(), v.raw()))
            .collect();
        let dirty_buffers: BTreeSet<u64> = cat
            .live_ids()
            .into_iter()
            .filter(|&id| ws.is_dirty(id).expect("live dirty check"))
            .map(BufferId::raw)
            .collect();
        let active_actors: BTreeMap<u64, OracleRole> = ws
            .actors()
            .active_actors()
            .into_iter()
            .map(|(id, role)| (id.raw(), OracleRole::from_prod(role)))
            .collect();
        let revoked_actors: BTreeSet<u64> = known_actors
            .iter()
            .copied()
            .filter(|&a| ws.actors().is_revoked(ActorId::new(a)))
            .collect();
        let journal: Vec<OracleReceipt> = ws
            .journal()
            .range(Revision::ZERO, ws.journal().tip())
            .expect("full journal range")
            .iter()
            .copied()
            .map(OracleReceipt::from_prod)
            .collect();
        let mut receipt_by_request = BTreeMap::new();
        let mut edit_receipt_by_request = BTreeMap::new();
        for &req in known_requests {
            if let Some(r) = ws.receipt_by_request(RequestId::new(req)) {
                receipt_by_request.insert(req, OracleReceipt::from_prod(r));
            }
            if let Some(r) = ws.history().receipt_by_request(RequestId::new(req)) {
                edit_receipt_by_request.insert(req, OracleReceipt::from_prod(r));
            }
        }
        let mut receipt_by_txn = BTreeMap::new();
        for &(a, n) in known_txns {
            let tid = TxnId::new(ActorId::new(a), Nonce::new(n));
            if let Some(r) = ws.receipt_by_txn(tid) {
                receipt_by_txn.insert((a, n), OracleReceipt::from_prod(r));
            }
        }
        let delivery_event_receipts: Vec<OracleReceipt> = ws
            .delivery()
            .events()
            .iter()
            .map(|ev| OracleReceipt::from_prod(ev.receipt()))
            .collect();
        let delivery_replay_catalog = match ws
            .delivery()
            .catch_up(Revision::new(delivery_floor))
            .expect("catch_up at delivery floor")
        {
            CatchUp::Events(events) => {
                let (replayed_cat, replayed_rev) =
                    delivery::replay(ws.delivery().base_snapshot(), &events)
                        .expect("replay delivered events");
                assert_eq!(replayed_rev.raw(), revision);
                catalog_to_map(&replayed_cat)
            }
            CatchUp::Snapshot(_) => panic!("catch_up at delivery_floor must return Events"),
        };
        let history_replay_catalog = catalog_to_map(
            &ws.history()
                .replay(Revision::new(floor))
                .expect("history replay from floor"),
        );
        let expired_catch_up_snapshot = if delivery_floor > 0 {
            match ws
                .delivery()
                .catch_up(Revision::new(delivery_floor - 1))
                .expect("expired catch_up")
            {
                CatchUp::Snapshot(s) => {
                    assert_eq!(s.revision().raw(), revision);
                    Some(catalog_to_map(s.catalog()))
                }
                CatchUp::Events(_) => {
                    panic!("catch_up below delivery_floor must return Snapshot")
                }
            }
        } else {
            None
        };
        let compaction_floor_rejects_expired = if floor > 0 {
            matches!(
                ws.history().replay(Revision::new(floor - 1)),
                Err(RetentionError::BaseExpired { .. })
            )
        } else {
            true
        };
        StateObservation {
            revision,
            floor,
            delivery_floor,
            live_buffers,
            tombstones,
            names,
            dirty_buffers,
            active_actors,
            revoked_actors,
            journal,
            receipt_by_request,
            receipt_by_txn,
            edit_receipt_by_request,
            delivery_event_receipts,
            delivery_replay_catalog,
            history_replay_catalog,
            expired_catch_up_snapshot,
            compaction_floor_rejects_expired,
        }
    }

    fn observe_oracle(
        oracle: &OracleState,
        known_requests: &[u64],
        known_txns: &[(u64, u64)],
    ) -> StateObservation {
        let mut receipt_by_request = BTreeMap::new();
        let mut edit_receipt_by_request = BTreeMap::new();
        for &req in known_requests {
            if let Some((_, outcome)) = oracle.receipt_by_request.get(&req) {
                if let Some(r) = outcome.receipt {
                    receipt_by_request.insert(req, r);
                }
            }
            if let Some(&r) = oracle.edit_receipt_by_request.get(&req) {
                edit_receipt_by_request.insert(req, r);
            }
        }
        let mut receipt_by_txn = BTreeMap::new();
        for &txn in known_txns {
            if let Some(&r) = oracle.receipt_by_txn.get(&txn) {
                receipt_by_txn.insert(txn, r);
            }
        }
        let delivery_event_receipts: Vec<OracleReceipt> =
            oracle.delivery_events.iter().map(|(r, _)| *r).collect();
        let delivery_replay_catalog = oracle.delivery_replay_from_floor();
        let history_replay_catalog = oracle
            .history_replay(oracle.floor)
            .expect("oracle replay at floor");
        let expired_catch_up_snapshot = if oracle.delivery_floor > 0 {
            Some(oracle.live.clone())
        } else {
            None
        };
        let compaction_floor_rejects_expired = if oracle.floor > 0 {
            oracle.history_replay(oracle.floor - 1) == Err(OracleErrorKind::BaseExpired)
        } else {
            true
        };
        StateObservation {
            revision: oracle.revision,
            floor: oracle.floor,
            delivery_floor: oracle.delivery_floor,
            live_buffers: oracle.live.clone(),
            tombstones: oracle.tombstones.clone(),
            names: oracle.names.clone(),
            dirty_buffers: oracle.dirty_buffers(),
            active_actors: oracle.active_actors.clone(),
            revoked_actors: oracle.revoked_actors.clone(),
            journal: oracle.journal.clone(),
            receipt_by_request,
            receipt_by_txn,
            edit_receipt_by_request,
            delivery_event_receipts,
            delivery_replay_catalog,
            history_replay_catalog,
            expired_catch_up_snapshot,
            compaction_floor_rejects_expired,
        }
    }

    fn compare_observations(
        expected: &StateObservation,
        actual: &StateObservation,
    ) -> Result<(), Vec<StateDrift>> {
        let mut drifts = Vec::new();
        if expected.revision != actual.revision {
            drifts.push(StateDrift::Revision {
                expected: expected.revision,
                actual: actual.revision,
            });
        }
        if expected.floor != actual.floor {
            drifts.push(StateDrift::Floor {
                expected: expected.floor,
                actual: actual.floor,
            });
        }
        if expected.delivery_floor != actual.delivery_floor {
            drifts.push(StateDrift::DeliveryFloor {
                expected: expected.delivery_floor,
                actual: actual.delivery_floor,
            });
        }
        let exp_keys: BTreeSet<u64> = expected.live_buffers.keys().copied().collect();
        let act_keys: BTreeSet<u64> = actual.live_buffers.keys().copied().collect();
        if exp_keys != act_keys {
            drifts.push(StateDrift::LiveBufferSet {
                expected: exp_keys,
                actual: act_keys,
            });
        } else {
            for (&buf, exp_text) in &expected.live_buffers {
                let act_text = &actual.live_buffers[&buf];
                if exp_text != act_text {
                    drifts.push(StateDrift::LiveBufferText {
                        buffer: buf,
                        expected: exp_text.clone(),
                        actual: act_text.clone(),
                    });
                }
            }
        }
        if expected.tombstones != actual.tombstones {
            drifts.push(StateDrift::Tombstones {
                expected: expected.tombstones.clone(),
                actual: actual.tombstones.clone(),
            });
        }
        if expected.names != actual.names {
            drifts.push(StateDrift::Names {
                expected: expected.names.clone(),
                actual: actual.names.clone(),
            });
        }
        if expected.dirty_buffers != actual.dirty_buffers {
            drifts.push(StateDrift::DirtyBuffers {
                expected: expected.dirty_buffers.clone(),
                actual: actual.dirty_buffers.clone(),
            });
        }
        if expected.active_actors != actual.active_actors {
            drifts.push(StateDrift::ActiveActors {
                expected: expected.active_actors.clone(),
                actual: actual.active_actors.clone(),
            });
        }
        if expected.revoked_actors != actual.revoked_actors {
            drifts.push(StateDrift::RevokedActors {
                expected: expected.revoked_actors.clone(),
                actual: actual.revoked_actors.clone(),
            });
        }
        if expected.journal != actual.journal {
            drifts.push(StateDrift::Journal {
                expected: expected.journal.clone(),
                actual: actual.journal.clone(),
            });
        }
        if expected.receipt_by_request != actual.receipt_by_request {
            drifts.push(StateDrift::ReceiptByRequest {
                expected: expected.receipt_by_request.clone(),
                actual: actual.receipt_by_request.clone(),
            });
        }
        if expected.receipt_by_txn != actual.receipt_by_txn {
            drifts.push(StateDrift::ReceiptByTxn {
                expected: expected.receipt_by_txn.clone(),
                actual: actual.receipt_by_txn.clone(),
            });
        }
        if expected.edit_receipt_by_request != actual.edit_receipt_by_request {
            drifts.push(StateDrift::EditReceiptByRequest {
                expected: expected.edit_receipt_by_request.clone(),
                actual: actual.edit_receipt_by_request.clone(),
            });
        }
        if expected.delivery_event_receipts != actual.delivery_event_receipts {
            drifts.push(StateDrift::DeliveryEventReceipts {
                expected: expected.delivery_event_receipts.clone(),
                actual: actual.delivery_event_receipts.clone(),
            });
        }
        if expected.delivery_replay_catalog != actual.delivery_replay_catalog {
            drifts.push(StateDrift::DeliveryReplayCatalog {
                expected: expected.delivery_replay_catalog.clone(),
                actual: actual.delivery_replay_catalog.clone(),
            });
        }
        if expected.history_replay_catalog != actual.history_replay_catalog {
            drifts.push(StateDrift::HistoryReplayCatalog {
                expected: expected.history_replay_catalog.clone(),
                actual: actual.history_replay_catalog.clone(),
            });
        }
        if expected.expired_catch_up_snapshot != actual.expired_catch_up_snapshot {
            drifts.push(StateDrift::ExpiredCatchUpSnapshot {
                expected: expected.expired_catch_up_snapshot.clone(),
                actual: actual.expired_catch_up_snapshot.clone(),
            });
        }
        if expected.compaction_floor_rejects_expired != actual.compaction_floor_rejects_expired {
            drifts.push(StateDrift::CompactionExpiryRejection {
                expected: expected.compaction_floor_rejects_expired,
                actual: actual.compaction_floor_rejects_expired,
            });
        }
        if drifts.is_empty() {
            Ok(())
        } else {
            Err(drifts)
        }
    }

    fn assert_no_drift(expected: &StateObservation, actual: &StateObservation, context: &str) {
        if let Err(drifts) = compare_observations(expected, actual) {
            panic!("state drift detected ({context}): {drifts:#?}");
        }
    }

    // =========================================================================
    // Section 3: Builders & Paired Harness
    // =========================================================================

    fn build_span(text: &str, start: usize, end: usize) -> ByteSpan {
        ByteSpan::validate_in(&Text::from_str(text), start, end).expect("valid byte span")
    }

    fn build_txn_from_oracle(
        actor: u64,
        nonce: u64,
        base: u64,
        patches: &OraclePatches,
        base_texts: &BTreeMap<u64, String>,
    ) -> Transaction {
        let mut prod_patches = Vec::with_capacity(patches.len());
        for (&buf, splices) in patches {
            let reference_text = base_texts
                .get(&buf)
                .map(String::as_str)
                .unwrap_or("0123456789abcdef");
            let mut prod_splices = Vec::with_capacity(splices.len());
            for s in splices {
                let span = if s.end <= reference_text.len()
                    && reference_text.is_char_boundary(s.start)
                    && reference_text.is_char_boundary(s.end)
                {
                    build_span(reference_text, s.start, s.end)
                } else {
                    let filler = "a".repeat(s.end.max(1));
                    build_span(&filler, s.start, s.end)
                };
                prod_splices.push(Splice::new(span, Text::from_str(&s.replacement)));
            }
            prod_patches.push((
                BufferId::new(buf),
                Patch::new(prod_splices).expect("valid patch"),
            ));
        }
        Transaction::new(
            EditIntent::for_actor(ActorId::new(actor), Nonce::new(nonce), Revision::new(base)),
            prod_patches,
        )
        .expect("valid transaction")
    }

    fn build_prod_op(
        op: &OracleOp,
        coord: &Coordinator,
        base_texts: &BTreeMap<u64, String>,
    ) -> Operation {
        match op {
            OracleOp::Open {
                actor,
                nonce,
                base,
                name,
                text,
            } => Operation::Open {
                intent: EditIntent::for_actor(
                    ActorId::new(*actor),
                    Nonce::new(*nonce),
                    Revision::new(*base),
                ),
                name: name.clone(),
                text: Text::from_str(text),
            },
            OracleOp::Close {
                actor,
                nonce,
                base,
                buffer,
                policy,
            } => Operation::Close {
                intent: EditIntent::for_actor(
                    ActorId::new(*actor),
                    Nonce::new(*nonce),
                    Revision::new(*base),
                ),
                buffer: BufferId::new(*buffer),
                policy: policy.to_prod(),
            },
            OracleOp::Edit {
                actor,
                nonce,
                base,
                patches,
            } => Operation::Edit(build_txn_from_oracle(
                *actor, *nonce, *base, patches, base_texts,
            )),
            OracleOp::Cut {
                actor,
                nonce,
                base,
                buffer,
                start,
                end,
                register,
            } => {
                let buf_id = BufferId::new(*buffer);
                let snap = coord.snapshot();
                let mut base_catalog = BufferCatalog::new();
                let catalog_ref = if let Some(raw_text) = base_texts.get(buffer) {
                    base_catalog
                        .register(buf_id, Text::from_str(raw_text))
                        .expect("register base text for cut");
                    &base_catalog
                } else {
                    snap.catalog()
                };
                let view = BufferView::full(catalog_ref, buf_id).expect("live buffer view");
                let text = catalog_ref.get(buf_id).expect("live buffer text");
                let sel = Selection::new(
                    catalog_ref,
                    &view,
                    buf_id,
                    text.validate_offset(*start).expect("valid start"),
                    text.validate_offset(*end).expect("valid end"),
                )
                .expect("valid selection");
                Operation::Cut {
                    intent: EditIntent::for_actor(
                        ActorId::new(*actor),
                        Nonce::new(*nonce),
                        Revision::new(*base),
                    ),
                    view,
                    selections: vec![sel],
                    name: RegisterName::new(register).expect("valid register name"),
                    kind: RegisterKind::Charwise,
                }
            }
            OracleOp::Undo { actor, target, txn } => Operation::Undo {
                actor: ActorId::new(*actor),
                target: TxnId::new(ActorId::new(target.0), Nonce::new(target.1)),
                transaction: TxnId::new(ActorId::new(txn.0), Nonce::new(txn.1)),
            },
            OracleOp::Redo { actor, target, txn } => Operation::Redo {
                actor: ActorId::new(*actor),
                target: TxnId::new(ActorId::new(target.0), Nonce::new(target.1)),
                transaction: TxnId::new(ActorId::new(txn.0), Nonce::new(txn.1)),
            },
        }
    }

    struct PairedHarness {
        coord: Coordinator,
        oracle: OracleState,
        known_actors: Vec<u64>,
        known_requests: Vec<u64>,
        known_txns: Vec<(u64, u64)>,
        /// Historical live text maps retained so stale-base edits can build
        /// spans even after compaction drops snapshots from the oracle.
        saved_texts: BTreeMap<u64, BTreeMap<u64, String>>,
    }

    impl PairedHarness {
        /// Standard 3-actor setup mixing Human (full rights), Automated actor
        /// (direct Commit + Read, no Review), and Agent actor (Read + Propose,
        /// optional Commit granted per-test or via defaults).
        fn new_three_actor(agent_direct_commit: bool) -> Self {
            let mut registry = ActorRegistry::new();
            let mut policy = AccessPolicy::new();
            let mut oracle = OracleState::new();

            let specs = [
                (HUMAN_A, OracleRole::Human),
                (AUTOMATED_B, OracleRole::Agent),
                (AGENT_C, OracleRole::Agent),
            ];
            for (actor, role) in specs {
                registry
                    .register(ActorId::new(actor), role.to_prod())
                    .expect("register actor");
                oracle.register_actor(actor, role).expect("oracle register");
            }

            // Human gets all capabilities.
            for cap in OracleCap::ALL {
                policy.grant_actor(ActorId::new(HUMAN_A), cap.to_prod());
                oracle.grant_actor(HUMAN_A, cap);
            }
            // Automated actor gets Read + Commit + Propose (not Review).
            for cap in [OracleCap::Read, OracleCap::Commit, OracleCap::Propose] {
                policy.grant_actor(ActorId::new(AUTOMATED_B), cap.to_prod());
                oracle.grant_actor(AUTOMATED_B, cap);
            }
            // Agent gets Read + Propose, plus Commit when requested.
            for cap in [OracleCap::Read, OracleCap::Propose] {
                policy.grant_actor(ActorId::new(AGENT_C), cap.to_prod());
                oracle.grant_actor(AGENT_C, cap);
            }
            if agent_direct_commit {
                policy.grant_actor(ActorId::new(AGENT_C), Capability::Commit);
                oracle.grant_actor(AGENT_C, OracleCap::Commit);
            }

            let coord = Coordinator::new(registry, policy);
            let mut saved_texts = BTreeMap::new();
            saved_texts.insert(0, BTreeMap::new());
            Self {
                coord,
                oracle,
                known_actors: vec![HUMAN_A, AUTOMATED_B, AGENT_C],
                known_requests: Vec::new(),
                known_txns: Vec::new(),
                saved_texts,
            }
        }

        fn register_extra_actor(&mut self, actor: u64, role: OracleRole, caps: &[OracleCap]) {
            self.coord
                .actors_mut()
                .register(ActorId::new(actor), role.to_prod())
                .expect("register extra");
            self.oracle
                .register_actor(actor, role)
                .expect("oracle register extra");
            if !self.known_actors.contains(&actor) {
                self.known_actors.push(actor);
            }
            for &cap in caps {
                self.coord
                    .access_mut()
                    .grant_actor(ActorId::new(actor), cap.to_prod());
                self.oracle.grant_actor(actor, cap);
            }
            self.verify_sync("register_extra_actor");
        }

        fn track_request(&mut self, request: u64) {
            if !self.known_requests.contains(&request) {
                self.known_requests.push(request);
            }
        }

        fn track_txn(&mut self, txn: (u64, u64)) {
            if !self.known_txns.contains(&txn) {
                self.known_txns.push(txn);
            }
        }

        fn verify_sync(&self, label: &str) {
            let expected = observe_oracle(&self.oracle, &self.known_requests, &self.known_txns);
            let actual = observe_coordinator(
                &self.coord,
                &self.known_actors,
                &self.known_requests,
                &self.known_txns,
            );
            assert_no_drift(&expected, &actual, label);
        }

        fn apply_op(
            &mut self,
            request: u64,
            op: OracleOp,
            label: &str,
        ) -> Result<OracleOutcome, OracleErrorKind> {
            self.track_request(request);
            match &op {
                OracleOp::Open { actor, nonce, .. }
                | OracleOp::Close { actor, nonce, .. }
                | OracleOp::Edit { actor, nonce, .. }
                | OracleOp::Cut { actor, nonce, .. } => self.track_txn((*actor, *nonce)),
                OracleOp::Undo { txn, .. } | OracleOp::Redo { txn, .. } => self.track_txn(*txn),
            }
            let base_rev = match &op {
                OracleOp::Open { base, .. }
                | OracleOp::Close { base, .. }
                | OracleOp::Edit { base, .. }
                | OracleOp::Cut { base, .. } => *base,
                OracleOp::Undo { .. } | OracleOp::Redo { .. } => self.oracle.revision,
            };
            let base_texts = self
                .saved_texts
                .get(&base_rev)
                .cloned()
                .unwrap_or_else(|| self.oracle.live.clone());
            let prod_op = build_prod_op(&op, &self.coord, &base_texts);
            let before_cp = self.coord.checkpoint();
            let before_obs = observe_coordinator(
                &self.coord,
                &self.known_actors,
                &self.known_requests,
                &self.known_txns,
            );
            let expected = self.oracle.apply(request, op);
            let actual = self.coord.apply(RequestId::new(request), prod_op);
            match (&expected, &actual) {
                (Ok(exp_out), Ok(act_out)) => {
                    assert_eq!(
                        OracleOutcome::from_prod(act_out),
                        *exp_out,
                        "outcome mismatch on {label}"
                    );
                    self.saved_texts
                        .insert(self.oracle.revision, self.oracle.live.clone());
                    self.verify_sync(label);
                    Ok(*exp_out)
                }
                (Err(exp_err), Err(act_err)) => {
                    assert_eq!(
                        classify_coordinator_error(act_err),
                        *exp_err,
                        "error mismatch on {label}: actual={act_err:?}"
                    );
                    assert_eq!(
                        self.coord.checkpoint(),
                        before_cp,
                        "rejected op mutated checkpoint on {label}"
                    );
                    let after_obs = observe_coordinator(
                        &self.coord,
                        &self.known_actors,
                        &self.known_requests,
                        &self.known_txns,
                    );
                    assert_no_drift(&before_obs, &after_obs, &format!("{label}:atomic_reject"));
                    self.verify_sync(&format!("{label}:post_reject_oracle"));
                    Err(*exp_err)
                }
                _ => panic!(
                    "expected/actual divergence on {label}: expected={expected:?}, actual={actual:?}"
                ),
            }
        }

        fn propose(
            &mut self,
            request: u64,
            proposer: u64,
            nonce: u64,
            base: u64,
            patches: OraclePatches,
            label: &str,
        ) -> Result<(), OracleErrorKind> {
            self.track_request(request);
            self.track_txn((proposer, nonce));
            let base_texts = self
                .saved_texts
                .get(&base)
                .cloned()
                .unwrap_or_else(|| self.oracle.live.clone());
            let txn = build_txn_from_oracle(proposer, nonce, base, &patches, &base_texts);
            let proposal = Proposal::new(RequestId::new(request), txn);
            let before_cp = self.coord.checkpoint();
            let expected = self.oracle.propose(request, proposer, nonce, base, patches);
            let actual = self.coord.propose(proposal);
            match (&expected, &actual) {
                (Ok(()), Ok(())) => {
                    self.verify_sync(label);
                    Ok(())
                }
                (Err(exp_err), Err(act_err)) => {
                    assert_eq!(
                        classify_coordinator_error(act_err),
                        *exp_err,
                        "propose error mismatch on {label}: actual={act_err:?}"
                    );
                    assert_eq!(
                        self.coord.checkpoint(),
                        before_cp,
                        "rejected propose mutated checkpoint on {label}"
                    );
                    self.verify_sync(label);
                    Err(*exp_err)
                }
                _ => panic!(
                    "propose divergence on {label}: expected={expected:?}, actual={actual:?}"
                ),
            }
        }

        fn approve(
            &mut self,
            request: u64,
            reviewer: u64,
            label: &str,
        ) -> Result<(), OracleErrorKind> {
            let before_cp = self.coord.checkpoint();
            let expected = self.oracle.approve(request, reviewer);
            let actual = self
                .coord
                .approve(RequestId::new(request), ActorId::new(reviewer));
            match (&expected, &actual) {
                (Ok(()), Ok(())) => {
                    self.verify_sync(label);
                    Ok(())
                }
                (Err(exp_err), Err(act_err)) => {
                    assert_eq!(
                        classify_coordinator_error(act_err),
                        *exp_err,
                        "approve error mismatch on {label}: actual={act_err:?}"
                    );
                    assert_eq!(
                        self.coord.checkpoint(),
                        before_cp,
                        "rejected approve mutated checkpoint on {label}"
                    );
                    self.verify_sync(label);
                    Err(*exp_err)
                }
                _ => panic!(
                    "approve divergence on {label}: expected={expected:?}, actual={actual:?}"
                ),
            }
        }

        fn commit_proposal(
            &mut self,
            request: u64,
            label: &str,
        ) -> Result<OracleOutcome, OracleErrorKind> {
            let before_cp = self.coord.checkpoint();
            let expected = self.oracle.commit_proposal(request);
            let actual = self.coord.commit_proposal(RequestId::new(request));
            match (&expected, &actual) {
                (Ok(exp_out), Ok(act_out)) => {
                    assert_eq!(
                        OracleOutcome::from_prod(act_out),
                        *exp_out,
                        "commit_proposal outcome mismatch on {label}"
                    );
                    self.saved_texts
                        .insert(self.oracle.revision, self.oracle.live.clone());
                    self.verify_sync(label);
                    Ok(*exp_out)
                }
                (Err(exp_err), Err(act_err)) => {
                    assert_eq!(
                        classify_coordinator_error(act_err),
                        *exp_err,
                        "commit_proposal error mismatch on {label}: actual={act_err:?}"
                    );
                    assert_eq!(
                        self.coord.checkpoint(),
                        before_cp,
                        "rejected commit_proposal mutated checkpoint on {label}"
                    );
                    self.verify_sync(label);
                    Err(*exp_err)
                }
                _ => panic!(
                    "commit_proposal divergence on {label}: expected={expected:?}, actual={actual:?}"
                ),
            }
        }

        fn compact(&mut self, floor: u64, label: &str) -> Result<(), OracleErrorKind> {
            let before_cp = self.coord.checkpoint();
            let expected = self.oracle.compact(floor);
            let actual = self.coord.compact(Revision::new(floor));
            match (&expected, &actual) {
                (Ok(()), Ok(())) => {
                    self.verify_sync(label);
                    Ok(())
                }
                (Err(exp_err), Err(act_err)) => {
                    assert_eq!(
                        classify_coordinator_error(act_err),
                        *exp_err,
                        "compact error mismatch on {label}: actual={act_err:?}"
                    );
                    assert_eq!(
                        self.coord.checkpoint(),
                        before_cp,
                        "rejected compact mutated checkpoint on {label}"
                    );
                    self.verify_sync(label);
                    Err(*exp_err)
                }
                _ => panic!(
                    "compact divergence on {label}: expected={expected:?}, actual={actual:?}"
                ),
            }
        }

        fn checkpoint_restore(&mut self, label: &str) {
            let cp = self.coord.checkpoint();
            self.coord = Coordinator::restore(&cp).expect("restore valid checkpoint");
            self.verify_sync(label);
        }

        fn revoke_actor(&mut self, actor: u64, label: &str) {
            self.coord
                .actors_mut()
                .revoke(ActorId::new(actor))
                .expect("revoke actor");
            self.oracle
                .revoke_actor(actor)
                .expect("oracle revoke actor");
            self.verify_sync(label);
        }

        fn grant_actor_cap(&mut self, actor: u64, cap: OracleCap) {
            self.coord
                .access_mut()
                .grant_actor(ActorId::new(actor), cap.to_prod());
            self.oracle.grant_actor(actor, cap);
        }

        fn revoke_actor_cap(&mut self, actor: u64, cap: OracleCap) {
            self.coord
                .access_mut()
                .revoke_actor(ActorId::new(actor), cap.to_prod());
            self.oracle.revoke_actor_cap(actor, cap);
        }

        fn grant_buffer_cap(&mut self, actor: u64, buffer: u64, cap: OracleCap) {
            self.coord.access_mut().grant_buffer(
                ActorId::new(actor),
                BufferId::new(buffer),
                cap.to_prod(),
            );
            self.oracle.grant_buffer(actor, buffer, cap);
        }

        fn revoke_buffer_cap(&mut self, actor: u64, buffer: u64, cap: OracleCap) {
            self.coord.access_mut().revoke_buffer(
                ActorId::new(actor),
                BufferId::new(buffer),
                cap.to_prod(),
            );
            self.oracle.revoke_buffer(actor, buffer, cap);
        }
    }

    fn all_permutations(n: usize) -> Vec<Vec<usize>> {
        fn generate(items: &mut [usize], idx: usize, out: &mut Vec<Vec<usize>>) {
            if idx == items.len() {
                out.push(items.to_vec());
            } else {
                for i in idx..items.len() {
                    items.swap(idx, i);
                    generate(items, idx + 1, out);
                    items.swap(idx, i);
                }
            }
        }
        let mut base: Vec<usize> = (0..n).collect();
        let mut out = Vec::new();
        generate(&mut base, 0, &mut out);
        out
    }

    fn char_boundaries(text: &str) -> Vec<usize> {
        text.char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(text.len()))
            .collect()
    }

    fn single_edit(
        actor: u64,
        nonce: u64,
        base: u64,
        buffer: u64,
        start: usize,
        end: usize,
        replacement: impl Into<String>,
    ) -> OracleOp {
        OracleOp::Edit {
            actor,
            nonce,
            base,
            patches: BTreeMap::from([(buffer, vec![OracleSplice::new(start, end, replacement)])]),
        }
    }

    // =========================================================================
    // Law 1: Disjoint Edit Order-Independence (Exhaustive + Randomized)
    // =========================================================================

    #[test]
    fn exhaustive_disjoint_edit_order_independence_two_and_three_actors() {
        // Part A: Exhaustive 2-actor single-buffer disjoint edits across ASCII and UTF-8.
        let mut two_actor_cases = 0usize;
        for (source, role_pair) in [
            ("abcdef", (HUMAN_A, AUTOMATED_B)),
            ("a界b😀c", (HUMAN_A, AGENT_C)),
            ("x\ny\r\nz", (AUTOMATED_B, AGENT_C)),
        ] {
            let bounds = char_boundaries(source);
            let mut spans = Vec::new();
            for (i, &s) in bounds.iter().enumerate() {
                for &e in &bounds[i..] {
                    spans.push((s, e));
                }
            }
            for &(s1, e1) in &spans {
                for &(s2, e2) in &spans {
                    if closed_intervals_conflict((s1, e1), (s2, e2)) {
                        continue;
                    }
                    // Pick non-noop replacements for both spans.
                    let r1 = if &source[s1..e1] == "X" { "界" } else { "X" };
                    let r2 = if &source[s2..e2] == "Y" { "😀" } else { "Y" };
                    let ops = [
                        single_edit(role_pair.0, 10, 1, 0, s1, e1, r1),
                        single_edit(role_pair.1, 20, 1, 0, s2, e2, r2),
                    ];

                    let mut results = Vec::new();
                    let mut seq_results = Vec::new();
                    for perm in all_permutations(2) {
                        let mut harness = PairedHarness::new_three_actor(true);
                        harness
                            .apply_op(
                                1,
                                OracleOp::Open {
                                    actor: HUMAN_A,
                                    nonce: 1,
                                    base: 0,
                                    name: "doc".into(),
                                    text: source.into(),
                                },
                                "open_doc",
                            )
                            .expect("open");
                        for (step, &idx) in perm.iter().enumerate() {
                            harness
                                .apply_op(
                                    10 + step as u64,
                                    ops[idx].clone(),
                                    "disjoint_2actor_step",
                                )
                                .expect("disjoint edit must commit");
                        }
                        results.push(harness.oracle.live[&0].clone());

                        // Cross-check standalone Sequencer too.
                        let mut cat = BufferCatalog::new();
                        cat.register(BufferId::new(0), Text::from_str(source))
                            .expect("reg");
                        let mut actors = ActorRegistry::new();
                        actors
                            .register(ActorId::new(HUMAN_A), ActorRole::Human)
                            .expect("reg");
                        actors
                            .register(ActorId::new(AUTOMATED_B), ActorRole::Agent)
                            .expect("reg");
                        actors
                            .register(ActorId::new(AGENT_C), ActorRole::Agent)
                            .expect("reg");
                        let mut seq = Sequencer::new(cat, actors);
                        let base_map = BTreeMap::from([(0, source.to_owned())]);
                        for &idx in &perm {
                            let OracleOp::Edit {
                                actor,
                                nonce,
                                patches,
                                ..
                            } = &ops[idx]
                            else {
                                unreachable!()
                            };
                            let txn = build_txn_from_oracle(*actor, *nonce, 0, patches, &base_map);
                            assert!(seq.submit(txn).is_committed());
                        }
                        seq_results.push(
                            seq.snapshot()
                                .catalog()
                                .get(BufferId::new(0))
                                .expect("live")
                                .as_str()
                                .to_owned(),
                        );
                    }
                    assert_eq!(
                        results[0], results[1],
                        "2-actor order dependence for {source:?} ({s1}..{e1}, {s2}..{e2})"
                    );
                    assert_eq!(seq_results[0], seq_results[1]);
                    assert_eq!(results[0], seq_results[0]);
                    two_actor_cases += 1;
                }
            }
        }
        assert!(two_actor_cases > 100);

        // Part B: Exhaustive 3-actor single-buffer disjoint edits across ALL 6 permutations
        // mixing Human, Automated, and Agent actors with inserts, deletes, and UTF-8 replacements.
        let source = "a界b😀c!d";
        let b = char_boundaries(source);
        // Strictly separated spans: [b[0], b[1]] ("a"), [b[2], b[3]] ("b"), [b[4], b[5]] ("c"),
        // plus pure insertion at b[7] (end).
        let repl_triples = [
            ("H", "AUTO", "🤖"),
            ("", "界界", ""),
            ("123", "", "e\u{0301}"),
            ("α", "β", "γ"),
        ];
        let span_triples = [
            ((b[0], b[1]), (b[2], b[3]), (b[4], b[5])),
            ((b[0], b[0]), (b[2], b[3]), (b[6], b[7])),
            ((b[0], b[1]), (b[3], b[3]), (b[5], b[7])),
        ];
        let perms3 = all_permutations(3);
        assert_eq!(perms3.len(), 6);
        for &(sp1, sp2, sp3) in &span_triples {
            for &(r1, r2, r3) in &repl_triples {
                let non_noop = |sp: (usize, usize), r: &'static str| -> &'static str {
                    if &source[sp.0..sp.1] == r {
                        "INS"
                    } else {
                        r
                    }
                };
                let ops = [
                    single_edit(HUMAN_A, 10, 1, 0, sp1.0, sp1.1, non_noop(sp1, r1)),
                    single_edit(AUTOMATED_B, 20, 1, 0, sp2.0, sp2.1, non_noop(sp2, r2)),
                    single_edit(AGENT_C, 30, 1, 0, sp3.0, sp3.1, non_noop(sp3, r3)),
                ];
                let mut baseline: Option<String> = None;
                for perm in &perms3 {
                    let mut harness = PairedHarness::new_three_actor(true);
                    harness
                        .apply_op(
                            1,
                            OracleOp::Open {
                                actor: HUMAN_A,
                                nonce: 1,
                                base: 0,
                                name: "shared".into(),
                                text: source.into(),
                            },
                            "open_shared",
                        )
                        .expect("open");
                    for (step, &idx) in perm.iter().enumerate() {
                        harness
                            .apply_op(10 + step as u64, ops[idx].clone(), "disjoint_3actor_step")
                            .expect("3-actor disjoint edit must commit");
                    }
                    let final_text = harness.oracle.live[&0].clone();
                    match &baseline {
                        None => baseline = Some(final_text),
                        Some(expected) => assert_eq!(
                            &final_text, expected,
                            "3-actor permutation {perm:?} diverged"
                        ),
                    }
                }
            }
        }

        // Part C: Exhaustive 2-actor and 3-actor MULTI-BUFFER disjoint edits across all permutations.
        // Includes both distinct-buffer edits at identical offsets and cross-buffer multi-buffer txns.
        let multi_ops = [
            OracleOp::Edit {
                actor: HUMAN_A,
                nonce: 10,
                base: 3,
                patches: BTreeMap::from([
                    (0, vec![OracleSplice::new(0, 1, "H0")]),
                    (1, vec![OracleSplice::new(4, 5, "H1")]),
                ]),
            },
            OracleOp::Edit {
                actor: AUTOMATED_B,
                nonce: 20,
                base: 3,
                patches: BTreeMap::from([
                    (0, vec![OracleSplice::new(3, 4, "AUTO")]),
                    (2, vec![OracleSplice::new(0, 1, "B2")]),
                ]),
            },
            OracleOp::Edit {
                actor: AGENT_C,
                nonce: 30,
                base: 3,
                patches: BTreeMap::from([
                    (1, vec![OracleSplice::new(0, 1, "界")]),
                    (2, vec![OracleSplice::new(3, 4, "😀")]),
                ]),
            },
        ];
        let mut expected_multi: Option<BTreeMap<u64, String>> = None;
        for perm in &perms3 {
            let mut harness = PairedHarness::new_three_actor(true);
            for (n, initial) in [(0, "012345"), (1, "abcdef"), (2, "uvwxyz")] {
                harness
                    .apply_op(
                        n + 1,
                        OracleOp::Open {
                            actor: HUMAN_A,
                            nonce: n + 1,
                            base: n,
                            name: format!("buf-{n}"),
                            text: initial.into(),
                        },
                        "open_multi",
                    )
                    .expect("open");
            }
            for (step, &idx) in perm.iter().enumerate() {
                harness
                    .apply_op(10 + step as u64, multi_ops[idx].clone(), "multi_buf_perm")
                    .expect("multi-buffer disjoint edit must commit");
            }
            let final_live = harness.oracle.live.clone();
            match &expected_multi {
                None => expected_multi = Some(final_live),
                Some(exp) => assert_eq!(
                    &final_live, exp,
                    "multi-buffer 3-actor permutation {perm:?} diverged"
                ),
            }
        }
        let exp = expected_multi.expect("computed multi-buffer result");
        assert_eq!(exp[&0], "H012AUTO45");
        assert_eq!(exp[&1], "界bcdH1f");
        assert_eq!(exp[&2], "B2vw😀yz");

        // Same byte offset in distinct buffers across 3 actors (all 6 permutations).
        let distinct_buf_ops = [
            single_edit(HUMAN_A, 10, 3, 0, 0, 2, "ONE"),
            single_edit(AUTOMATED_B, 20, 3, 1, 0, 2, "TWO"),
            single_edit(AGENT_C, 30, 3, 2, 0, 2, "THREE"),
        ];
        let mut expected_distinct: Option<BTreeMap<u64, String>> = None;
        for perm in &perms3 {
            let mut harness = PairedHarness::new_three_actor(true);
            for n in 0..3 {
                harness
                    .apply_op(
                        n + 1,
                        OracleOp::Open {
                            actor: HUMAN_A,
                            nonce: n + 1,
                            base: n,
                            name: format!("d-{n}"),
                            text: "abcd".into(),
                        },
                        "open_distinct",
                    )
                    .expect("open");
            }
            for (step, &idx) in perm.iter().enumerate() {
                harness
                    .apply_op(
                        10 + step as u64,
                        distinct_buf_ops[idx].clone(),
                        "distinct_buf_perm",
                    )
                    .expect("distinct-buffer edit must commit");
            }
            let final_live = harness.oracle.live.clone();
            match &expected_distinct {
                None => expected_distinct = Some(final_live),
                Some(exp_map) => assert_eq!(&final_live, exp_map),
            }
        }
    }

    #[test]
    fn randomized_disjoint_edit_order_independence() {
        let mut perm_checks = 0usize;
        assert_law(
            &[0, 1, 7, 42, 1337, u64::MAX],
            |rng| {
                let fragments = ["a", "b", "界", "😀", "e\u{0301}", "\n", "z"];
                let mut cases = Vec::new();
                for _ in 0..16 {
                    let buf_count = rng.gen_range(1..4);
                    let mut initial_texts = Vec::with_capacity(buf_count);
                    for _ in 0..buf_count {
                        let mut s = String::new();
                        for _ in 0..rng.gen_range(7..12) {
                            s.push_str(rng.choose(&fragments).expect("frag"));
                        }
                        initial_texts.push(s);
                    }
                    let repls: Vec<String> = (0..3)
                        .map(|i| {
                            let choices = ["", "H", "界", "😀", "XY"];
                            format!("{}{i}", choices[rng.gen_range(0..choices.len())])
                        })
                        .collect();
                    let three_actors = rng.gen_bool();
                    cases.push((initial_texts, repls, three_actors));
                }
                cases
            },
            |cases| {
                let actors = [HUMAN_A, AUTOMATED_B, AGENT_C];
                for (initial_texts, repls, three_actors) in cases {
                    let n_actors = if *three_actors { 3 } else { 2 };
                    let base_rev = initial_texts.len() as u64;
                    let mut ops = Vec::with_capacity(n_actors);
                    for actor_idx in 0..n_actors {
                        let mut patches = BTreeMap::new();
                        for (buf_idx, text) in initial_texts.iter().enumerate() {
                            let bounds = char_boundaries(text);
                            // Partition char indices strictly with >= 1 char gap:
                            // slot 0: [0..1], slot 1: [2..3], slot 2: [4..5] (always non-contacting).
                            let slot = (actor_idx + buf_idx) % 3;
                            let start = bounds[slot * 2];
                            let end = bounds[slot * 2 + 1];
                            let mut replacement = repls[actor_idx].clone();
                            if text[start..end] == replacement {
                                replacement.push('!');
                            }
                            patches.insert(
                                buf_idx as u64,
                                vec![OracleSplice::new(start, end, replacement)],
                            );
                        }
                        ops.push(OracleOp::Edit {
                            actor: actors[actor_idx],
                            nonce: 100 + actor_idx as u64,
                            base: base_rev,
                            patches,
                        });
                    }

                    let mut expected: Option<BTreeMap<u64, String>> = None;
                    for perm in all_permutations(n_actors) {
                        let mut harness = PairedHarness::new_three_actor(true);
                        for (buf_idx, text) in initial_texts.iter().enumerate() {
                            harness
                                .apply_op(
                                    buf_idx as u64 + 1,
                                    OracleOp::Open {
                                        actor: HUMAN_A,
                                        nonce: buf_idx as u64 + 1,
                                        base: buf_idx as u64,
                                        name: format!("r-{buf_idx}"),
                                        text: text.clone(),
                                    },
                                    "rand_open",
                                )
                                .expect("open");
                        }
                        for (step, &idx) in perm.iter().enumerate() {
                            harness
                                .apply_op(200 + step as u64, ops[idx].clone(), "rand_disjoint")
                                .expect("disjoint edit must commit");
                        }
                        let final_live = harness.oracle.live.clone();
                        match &expected {
                            None => expected = Some(final_live),
                            Some(exp) => assert_eq!(&final_live, exp),
                        }
                        perm_checks += 1;
                    }
                }
            },
        );
        assert!(perm_checks >= 200);
    }

    // =========================================================================
    // Law 2: Touching and Overlapping Conflict Rejection
    // =========================================================================

    #[test]
    fn exhaustive_touching_and_overlapping_conflict_rejection() {
        let mut touching_hits = 0usize;
        let mut overlapping_hits = 0usize;
        let mut coincident_insert_hits = 0usize;
        let mut endpoint_insert_hits = 0usize;

        // Part A: Exhaustive 2-actor closed-interval conflict matrix on ASCII and UTF-8.
        for source in ["abcd", "a界b😀"] {
            let bounds = char_boundaries(source);
            let mut spans = Vec::new();
            for (i, &s) in bounds.iter().enumerate() {
                for &e in &bounds[i..] {
                    spans.push((s, e));
                }
            }
            for &(s1, e1) in &spans {
                for &(s2, e2) in &spans {
                    if !closed_intervals_conflict((s1, e1), (s2, e2)) {
                        continue;
                    }
                    if s1 == e1 && s2 == e2 && s1 == s2 {
                        coincident_insert_hits += 1;
                    } else if (s1 == e1 && (s1 == s2 || s1 == e2))
                        || (s2 == e2 && (s2 == s1 || s2 == e1))
                    {
                        endpoint_insert_hits += 1;
                    } else if e1 == s2 || e2 == s1 {
                        touching_hits += 1;
                    } else {
                        overlapping_hits += 1;
                    }

                    let r1 = if &source[s1..e1] == "P" { "Q" } else { "P" };
                    let r2 = if &source[s2..e2] == "R" { "S" } else { "R" };
                    let ops = [
                        single_edit(HUMAN_A, 10, 1, 0, s1, e1, r1),
                        single_edit(AUTOMATED_B, 20, 1, 0, s2, e2, r2),
                    ];
                    for perm in all_permutations(2) {
                        let mut harness = PairedHarness::new_three_actor(true);
                        harness
                            .apply_op(
                                1,
                                OracleOp::Open {
                                    actor: HUMAN_A,
                                    nonce: 1,
                                    base: 0,
                                    name: "c".into(),
                                    text: source.into(),
                                },
                                "conflict_open",
                            )
                            .expect("open");
                        // First commits.
                        harness
                            .apply_op(10, ops[perm[0]].clone(), "conflict_winner")
                            .expect("first edit commits");
                        let winner_text = harness.oracle.live[&0].clone();
                        // Second MUST reject atomically with Conflict and zero state drift.
                        let err = harness
                            .apply_op(11, ops[perm[1]].clone(), "conflict_loser")
                            .expect_err("conflicting edit must reject");
                        assert_eq!(err, OracleErrorKind::Conflict);
                        assert_eq!(harness.oracle.live[&0], winner_text);
                        assert_eq!(harness.oracle.revision, 2);

                        // Cross-check standalone Sequencer as well.
                        let mut cat = BufferCatalog::new();
                        cat.register(BufferId::new(0), Text::from_str(source))
                            .expect("reg");
                        let mut actors = ActorRegistry::new();
                        actors
                            .register(ActorId::new(HUMAN_A), ActorRole::Human)
                            .expect("reg");
                        actors
                            .register(ActorId::new(AUTOMATED_B), ActorRole::Agent)
                            .expect("reg");
                        let mut seq = Sequencer::new(cat, actors);
                        let base_map = BTreeMap::from([(0, source.to_owned())]);
                        let to_txn = |op: &OracleOp| {
                            let OracleOp::Edit {
                                actor,
                                nonce,
                                patches,
                                ..
                            } = op
                            else {
                                unreachable!()
                            };
                            build_txn_from_oracle(*actor, *nonce, 0, patches, &base_map)
                        };
                        assert!(seq.submit(to_txn(&ops[perm[0]])).is_committed());
                        let before_seq =
                            (seq.snapshot(), seq.journal().clone(), seq.history().clone());
                        let outcome = seq.submit(to_txn(&ops[perm[1]]));
                        assert_eq!(outcome.reject_reason(), Some(RejectReason::Conflict));
                        assert_eq!(
                            (seq.snapshot(), seq.journal().clone(), seq.history().clone()),
                            before_seq
                        );
                    }
                }
            }
        }
        assert!(touching_hits > 0);
        assert!(overlapping_hits > 0);
        assert!(coincident_insert_hits > 0);
        assert!(endpoint_insert_hits > 0);

        // Part B: 3-actor conflict triangle (all 3 pairwise touch/overlap -> strictly 1 commits).
        let triangle = [
            single_edit(HUMAN_A, 10, 1, 0, 1, 3, "AA"),
            single_edit(AUTOMATED_B, 20, 1, 0, 2, 4, "BB"),
            single_edit(AGENT_C, 30, 1, 0, 3, 5, "CC"),
        ];
        for perm in all_permutations(3) {
            let mut harness = PairedHarness::new_three_actor(true);
            harness
                .apply_op(
                    1,
                    OracleOp::Open {
                        actor: HUMAN_A,
                        nonce: 1,
                        base: 0,
                        name: "tri".into(),
                        text: "0123456".into(),
                    },
                    "open_tri",
                )
                .expect("open");
            let mut committed = 0usize;
            let mut rejected = 0usize;
            for (step, &idx) in perm.iter().enumerate() {
                match harness.apply_op(10 + step as u64, triangle[idx].clone(), "tri_step") {
                    Ok(_) => committed += 1,
                    Err(OracleErrorKind::Conflict) => rejected += 1,
                    Err(other) => panic!("unexpected error: {other:?}"),
                }
            }
            assert_eq!(committed, 1);
            assert_eq!(rejected, 2);
            assert_eq!(harness.oracle.revision, 2);
        }

        // Part C: 3-actor conflict chain (A touches B, B touches C, A & C disjoint).
        // A: [0..2], B: [2..4], C: [4..6].
        let chain = [
            single_edit(HUMAN_A, 10, 1, 0, 0, 2, "A"),
            single_edit(AUTOMATED_B, 20, 1, 0, 2, 4, "B"),
            single_edit(AGENT_C, 30, 1, 0, 4, 6, "C"),
        ];
        let mut ac_finals = Vec::new();
        for perm in all_permutations(3) {
            let mut harness = PairedHarness::new_three_actor(true);
            harness
                .apply_op(
                    1,
                    OracleOp::Open {
                        actor: HUMAN_A,
                        nonce: 1,
                        base: 0,
                        name: "chain".into(),
                        text: "0123456".into(),
                    },
                    "open_chain",
                )
                .expect("open");
            let mut outcomes = Vec::new();
            for (step, &idx) in perm.iter().enumerate() {
                outcomes.push((
                    idx,
                    harness
                        .apply_op(10 + step as u64, chain[idx].clone(), "chain_step")
                        .is_ok(),
                ));
            }
            if perm[0] == 1 {
                // B first -> B wins, A and C both rejected atomically.
                assert_eq!(
                    outcomes,
                    vec![(1, true), (perm[1], false), (perm[2], false)]
                );
                assert_eq!(harness.oracle.live[&0], "01B456");
            } else {
                // A or C first -> both A and C commit, B rejected atomically.
                for (idx, ok) in outcomes {
                    assert_eq!(ok, idx != 1);
                }
                ac_finals.push(harness.oracle.live[&0].clone());
            }
        }
        assert_eq!(ac_finals.len(), 4);
        for text in &ac_finals {
            assert_eq!(text, "A23C6");
        }

        // Part D: Multi-buffer partial conflict (buffer 0 disjoint, buffer 1 touches) rejects atomically.
        let mut harness = PairedHarness::new_three_actor(true);
        for (n, text) in [(0, "abcdef"), (1, "012345")] {
            harness
                .apply_op(
                    n + 1,
                    OracleOp::Open {
                        actor: HUMAN_A,
                        nonce: n + 1,
                        base: n,
                        name: format!("mb-{n}"),
                        text: text.into(),
                    },
                    "open_mb",
                )
                .expect("open");
        }
        harness
            .apply_op(
                10,
                OracleOp::Edit {
                    actor: HUMAN_A,
                    nonce: 10,
                    base: 2,
                    patches: BTreeMap::from([
                        (0, vec![OracleSplice::new(0, 1, "X")]),
                        (1, vec![OracleSplice::new(1, 3, "Y")]),
                    ]),
                },
                "mb_first",
            )
            .expect("first commits");
        let err = harness
            .apply_op(
                11,
                OracleOp::Edit {
                    actor: AUTOMATED_B,
                    nonce: 20,
                    base: 2,
                    patches: BTreeMap::from([
                        // Disjoint on buffer 0:
                        (0, vec![OracleSplice::new(4, 5, "Z")]),
                        // Touching on buffer 1 ([3..5] shares boundary 3 with [1..3]):
                        (1, vec![OracleSplice::new(3, 5, "W")]),
                    ]),
                },
                "mb_partial_conflict",
            )
            .expect_err("partial multi-buffer conflict must reject atomically");
        assert_eq!(err, OracleErrorKind::Conflict);
        assert_eq!(harness.oracle.live[&0], "Xbcdef");
        assert_eq!(harness.oracle.live[&1], "0Y345");
    }

    // =========================================================================
    // Law 3: Receipt Idempotence & Conflicting Reuse Rejection
    // =========================================================================

    #[test]
    fn exhaustive_receipt_idempotence_and_conflicting_reuse() {
        let mut harness = PairedHarness::new_three_actor(true);
        let open_a = OracleOp::Open {
            actor: HUMAN_A,
            nonce: 1,
            base: 0,
            name: "alpha".into(),
            text: "abcdefghij".into(),
        };
        let out_open_a = harness
            .apply_op(1, open_a.clone(), "open_a")
            .expect("open a");
        let open_b = OracleOp::Open {
            actor: AUTOMATED_B,
            nonce: 2,
            base: 1,
            name: "beta".into(),
            text: "0123456789".into(),
        };
        let out_open_b = harness
            .apply_op(2, open_b.clone(), "open_b")
            .expect("open b");

        // Direct edit by Human A.
        let edit_a = single_edit(HUMAN_A, 3, 2, 0, 0, 2, "HH");
        let out_edit_a = harness
            .apply_op(3, edit_a.clone(), "edit_a")
            .expect("edit a");

        // Concurrent disjoint rebased edit by Automated B (stale base 2).
        let edit_b = single_edit(AUTOMATED_B, 4, 2, 0, 6, 8, "BB");
        let out_edit_b = harness
            .apply_op(4, edit_b.clone(), "edit_b")
            .expect("edit b");

        // Cut by Human A on buffer 1.
        let cut_a = OracleOp::Cut {
            actor: HUMAN_A,
            nonce: 5,
            base: 4,
            buffer: 1,
            start: 0,
            end: 2,
            register: "a".into(),
        };
        let out_cut_a = harness.apply_op(5, cut_a.clone(), "cut_a").expect("cut a");
        let reg_after_cut = harness.coord.registers().clone();
        let rings_after_cut = harness.coord.rings().clone();

        // Proposal by Agent C approved by Human A and committed.
        let prop_patches = BTreeMap::from([(1, vec![OracleSplice::new(4, 6, "AGENT")])]);
        harness
            .propose(6, AGENT_C, 6, 5, prop_patches.clone(), "propose_c")
            .expect("propose");
        // Idempotent re-propose with identical payload succeeds without state drift.
        harness
            .propose(6, AGENT_C, 6, 5, prop_patches.clone(), "repropose_c")
            .expect("idempotent repropose");
        // Conflicting re-propose with same request id rejects atomically.
        assert_eq!(
            harness.propose(
                6,
                AGENT_C,
                6,
                5,
                BTreeMap::from([(1, vec![OracleSplice::new(4, 6, "TAMPER")])]),
                "conflict_repropose"
            ),
            Err(OracleErrorKind::RequestReuse)
        );
        harness.approve(6, HUMAN_A, "approve_c").expect("approve");
        let out_prop_c = harness
            .commit_proposal(6, "commit_c")
            .expect("commit proposal");
        // Re-calling commit_proposal is idempotent.
        assert_eq!(
            harness
                .commit_proposal(6, "commit_c_retry")
                .expect("idempotent commit_proposal"),
            out_prop_c
        );

        // Selective Undo + Redo by Automated B.
        let undo_b = OracleOp::Undo {
            actor: AUTOMATED_B,
            target: (AUTOMATED_B, 4),
            txn: (AUTOMATED_B, 7),
        };
        let out_undo_b = harness
            .apply_op(7, undo_b.clone(), "undo_b")
            .expect("undo b");
        let redo_b = OracleOp::Redo {
            actor: AUTOMATED_B,
            target: (AUTOMATED_B, 4),
            txn: (AUTOMATED_B, 8),
        };
        let out_redo_b = harness
            .apply_op(8, redo_b.clone(), "redo_b")
            .expect("redo b");

        // Close buffer 1.
        let close_b = OracleOp::Close {
            actor: HUMAN_A,
            nonce: 9,
            base: 8,
            buffer: 1,
            policy: OracleClosePolicy::Discard,
        };
        let out_close_b = harness
            .apply_op(9, close_b.clone(), "close_b")
            .expect("close b");

        // Save all committed operations for replay across 3 phases:
        // Phase 1: Immediate replay (after later commits)
        // Phase 2: After full compaction + checkpoint restore
        // Phase 3: After actor & capability revocation!
        let committed_ops: Vec<(u64, OracleOp, OracleOutcome)> = vec![
            (1, open_a.clone(), out_open_a),
            (2, open_b.clone(), out_open_b),
            (3, edit_a.clone(), out_edit_a),
            (4, edit_b.clone(), out_edit_b),
            (5, cut_a.clone(), out_cut_a),
            (7, undo_b.clone(), out_undo_b),
            (8, redo_b.clone(), out_redo_b),
            (9, close_b.clone(), out_close_b),
        ];

        for phase in 0..3 {
            if phase == 1 {
                let tip = harness.oracle.revision;
                harness
                    .compact(tip, "compact_before_retry")
                    .expect("compact");
                harness.checkpoint_restore("restore_before_retry");
            } else if phase == 2 {
                harness.revoke_actor(AUTOMATED_B, "revoke_b_before_retry");
                harness.revoke_actor_cap(HUMAN_A, OracleCap::Commit);
                harness.revoke_actor_cap(AGENT_C, OracleCap::Commit);
            }

            let cp_before = harness.coord.checkpoint();
            for (req, op, expected_outcome) in &committed_ops {
                let retry_out = harness
                    .apply_op(*req, op.clone(), &format!("retry_phase_{phase}_req_{req}"))
                    .expect("exact retry must return original outcome");
                assert_eq!(retry_out, *expected_outcome);
                assert_eq!(harness.coord.checkpoint(), cp_before);
                assert_eq!(harness.coord.registers(), &reg_after_cut);
                assert_eq!(harness.coord.rings(), &rings_after_cut);
            }
            // Idempotent proposal commit retry survives compaction & revocation too.
            assert_eq!(
                harness
                    .commit_proposal(6, &format!("prop_retry_phase_{phase}"))
                    .expect("proposal commit retry"),
                out_prop_c
            );
            assert_eq!(harness.coord.checkpoint(), cp_before);

            // Conflicting payload reuse on every committed RequestId MUST reject atomically!
            let conflict_payloads = [
                (1, single_edit(HUMAN_A, 1, 0, 0, 0, 1, "Z")),
                (2, single_edit(AUTOMATED_B, 2, 1, 0, 0, 1, "Z")),
                (3, single_edit(HUMAN_A, 3, 2, 0, 0, 2, "DIFFERENT")),
                (4, single_edit(AUTOMATED_B, 4, 2, 0, 6, 8, "DIFFERENT")),
                (5, single_edit(HUMAN_A, 5, 4, 0, 0, 1, "DIFFERENT")),
                (6, single_edit(AGENT_C, 6, 5, 0, 0, 1, "DIFFERENT")),
                (
                    7,
                    OracleOp::Undo {
                        actor: AUTOMATED_B,
                        target: (HUMAN_A, 3),
                        txn: (AUTOMATED_B, 7),
                    },
                ),
                (
                    8,
                    OracleOp::Redo {
                        actor: AUTOMATED_B,
                        target: (HUMAN_A, 3),
                        txn: (AUTOMATED_B, 8),
                    },
                ),
                (
                    9,
                    OracleOp::Open {
                        actor: HUMAN_A,
                        nonce: 9,
                        base: 8,
                        name: "other".into(),
                        text: "x".into(),
                    },
                ),
            ];
            for (req, bad_op) in conflict_payloads {
                let err = harness
                    .apply_op(req, bad_op, &format!("reuse_phase_{phase}_req_{req}"))
                    .expect_err("conflicting request reuse must reject");
                assert_eq!(err, OracleErrorKind::RequestReuse);
                assert_eq!(harness.coord.checkpoint(), cp_before);
            }
        }

        // Reusing an already-committed TxnId under a FRESH RequestId rejects atomically.
        let mut fresh_harness = PairedHarness::new_three_actor(true);
        fresh_harness
            .apply_op(
                1,
                OracleOp::Open {
                    actor: HUMAN_A,
                    nonce: 1,
                    base: 0,
                    name: "f".into(),
                    text: "abcdef".into(),
                },
                "fresh_open",
            )
            .expect("open");
        let first_cut = OracleOp::Cut {
            actor: HUMAN_A,
            nonce: 2,
            base: 1,
            buffer: 0,
            start: 0,
            end: 2,
            register: "a".into(),
        };
        let first_cut_out = fresh_harness
            .apply_op(2, first_cut.clone(), "cut_once")
            .expect("cut");
        let reg_snapshot = fresh_harness.coord.registers().clone();
        let ring_snapshot = fresh_harness.coord.rings().clone();
        // Exact Cut retry does not duplicate kill-ring entry or re-cut buffer!
        let retry_cut_out = fresh_harness
            .apply_op(2, first_cut, "cut_retry")
            .expect("cut retry");
        assert_eq!(retry_cut_out, first_cut_out);
        assert_eq!(fresh_harness.coord.registers(), &reg_snapshot);
        assert_eq!(fresh_harness.coord.rings(), &ring_snapshot);
        // Same TxnId (HUMAN_A, 2) under new RequestId 99 rejects as TxnReuse.
        assert_eq!(
            fresh_harness.apply_op(
                99,
                single_edit(HUMAN_A, 2, 2, 0, 0, 1, "X"),
                "txn_reuse_edit"
            ),
            Err(OracleErrorKind::TxnReuse)
        );
        // Same TxnId (HUMAN_A, 1) under new RequestId 100 for Open rejects as TxnReuse.
        assert_eq!(
            fresh_harness.apply_op(
                100,
                OracleOp::Open {
                    actor: HUMAN_A,
                    nonce: 1,
                    base: 2,
                    name: "g".into(),
                    text: "z".into(),
                },
                "txn_reuse_open",
            ),
            Err(OracleErrorKind::TxnReuse)
        );

        // Cross-check standalone Sequencer & CommitJournal idempotence + reuse rejection.
        let mut cat = BufferCatalog::new();
        cat.register(BufferId::new(0), Text::from_str("hello"))
            .expect("reg");
        let mut actors = ActorRegistry::new();
        actors
            .register(ActorId::new(HUMAN_A), ActorRole::Human)
            .expect("reg");
        let mut seq = Sequencer::new(cat, actors);
        let base_map = BTreeMap::from([(0, "hello".to_owned())]);
        let txn = build_txn_from_oracle(
            HUMAN_A,
            1,
            0,
            &BTreeMap::from([(0, vec![OracleSplice::new(0, 1, "H")])]),
            &base_map,
        );
        let receipt = seq.submit(txn.clone()).committed_receipt().expect("commit");
        let before_seq = (seq.snapshot(), seq.journal().clone(), seq.history().clone());
        assert_eq!(seq.submit(txn), SubmitOutcome::Committed(receipt));
        assert_eq!(
            (seq.snapshot(), seq.journal().clone(), seq.history().clone()),
            before_seq
        );
        let altered = build_txn_from_oracle(
            HUMAN_A,
            1,
            0,
            &BTreeMap::from([(0, vec![OracleSplice::new(0, 1, "X")])]),
            &base_map,
        );
        assert_eq!(
            seq.submit(altered).reject_reason(),
            Some(RejectReason::TxnIdReuse)
        );
        assert_eq!(
            (seq.snapshot(), seq.journal().clone(), seq.history().clone()),
            before_seq
        );
        let mut journal = CommitJournal::default();
        journal.append(receipt).expect("append");
        assert_eq!(journal.append(receipt), Ok(()));
        let bad_receipt = CommitReceipt::new(
            EditIntent::for_actor(ActorId::new(HUMAN_A), Nonce::new(1), Revision::new(9)),
            Revision::new(1),
        )
        .expect("receipt");
        assert_eq!(
            journal.append(bad_receipt),
            Err(JournalError::TxnIdReuse { txn: receipt.txn() })
        );
    }

    // =========================================================================
    // Law 4: Proposal Approval Invalidation
    // =========================================================================

    #[test]
    fn exhaustive_proposal_approval_invalidation() {
        #[derive(Clone, Copy, Debug)]
        enum InvalidationCause {
            ConflictingEditBeforeApprove,
            ConflictingEditAfterApprove,
            DisjointEditAfterApprove,
            BufferCloseBeforeApprove,
            BufferCloseAfterApprove,
            ProposerCommitRevokedAfterApprove,
            ProposerActorRevokedAfterApprove,
            ApproverReviewRevokedAfterApprove,
            ApproverActorRevokedAfterApprove,
            ConcurrentApprovedProposalWinsFirst,
            DirectEditCommitsSameRegionBeforeProposal,
        }

        let all_causes = [
            InvalidationCause::ConflictingEditBeforeApprove,
            InvalidationCause::ConflictingEditAfterApprove,
            InvalidationCause::DisjointEditAfterApprove,
            InvalidationCause::BufferCloseBeforeApprove,
            InvalidationCause::BufferCloseAfterApprove,
            InvalidationCause::ProposerCommitRevokedAfterApprove,
            InvalidationCause::ProposerActorRevokedAfterApprove,
            InvalidationCause::ApproverReviewRevokedAfterApprove,
            InvalidationCause::ApproverActorRevokedAfterApprove,
            InvalidationCause::ConcurrentApprovedProposalWinsFirst,
            InvalidationCause::DirectEditCommitsSameRegionBeforeProposal,
        ];

        for cause in all_causes {
            let mut harness = PairedHarness::new_three_actor(true);
            harness
                .apply_op(
                    1,
                    OracleOp::Open {
                        actor: HUMAN_A,
                        nonce: 1,
                        base: 0,
                        name: "doc".into(),
                        text: "abcdef".into(),
                    },
                    "prop_inv_open_0",
                )
                .expect("open 0");
            harness
                .apply_op(
                    2,
                    OracleOp::Open {
                        actor: HUMAN_A,
                        nonce: 2,
                        base: 1,
                        name: "other".into(),
                        text: "012345".into(),
                    },
                    "prop_inv_open_1",
                )
                .expect("open 1");

            // Agent proposes edit on buffer 0 [0..2] -> "AG" at base 2.
            let agent_patches = BTreeMap::from([(0, vec![OracleSplice::new(0, 2, "AG")])]);
            harness
                .propose(10, AGENT_C, 10, 2, agent_patches, "propose_agent")
                .expect("propose");

            // Unapproved proposal cannot commit.
            assert_eq!(
                harness.commit_proposal(10, "unapproved_commit"),
                Err(OracleErrorKind::Unapproved)
            );
            // Non-human actors (Automated and Agent) cannot approve even if granted Review!
            for non_human in [AUTOMATED_B, AGENT_C] {
                harness.grant_actor_cap(non_human, OracleCap::Review);
                assert_eq!(
                    harness.approve(10, non_human, "non_human_approve"),
                    Err(OracleErrorKind::NotHuman)
                );
                harness.revoke_actor_cap(non_human, OracleCap::Review);
            }

            match cause {
                InvalidationCause::ConflictingEditBeforeApprove => {
                    harness
                        .apply_op(
                            20,
                            single_edit(AUTOMATED_B, 20, 2, 0, 0, 2, "AUTO"),
                            "conflict_before_approve",
                        )
                        .expect("conflicting edit commits");
                    assert_eq!(
                        harness.approve(10, HUMAN_A, "approve_stale"),
                        Err(OracleErrorKind::RevisionMismatch)
                    );
                    assert_eq!(
                        harness.commit_proposal(10, "commit_still_unapproved"),
                        Err(OracleErrorKind::Unapproved)
                    );
                }
                InvalidationCause::ConflictingEditAfterApprove => {
                    harness.approve(10, HUMAN_A, "approve").expect("approve");
                    harness
                        .apply_op(
                            20,
                            single_edit(AUTOMATED_B, 20, 2, 0, 1, 3, "OV"),
                            "conflict_after_approve",
                        )
                        .expect("conflicting edit commits");
                    assert_eq!(
                        harness.commit_proposal(10, "commit_stale_conflict"),
                        Err(OracleErrorKind::StaleApproval)
                    );
                    assert_eq!(harness.oracle.live[&0], "aOVdef");
                }
                InvalidationCause::DisjointEditAfterApprove => {
                    harness.approve(10, HUMAN_A, "approve").expect("approve");
                    harness
                        .apply_op(
                            20,
                            single_edit(AUTOMATED_B, 20, 2, 1, 0, 1, "Z"),
                            "disjoint_after_approve",
                        )
                        .expect("disjoint edit commits");
                    assert_eq!(
                        harness.commit_proposal(10, "commit_stale_disjoint"),
                        Err(OracleErrorKind::StaleApproval)
                    );
                    assert_eq!(harness.oracle.live[&0], "abcdef");
                }
                InvalidationCause::BufferCloseBeforeApprove => {
                    harness
                        .apply_op(
                            20,
                            OracleOp::Close {
                                actor: HUMAN_A,
                                nonce: 20,
                                base: 2,
                                buffer: 0,
                                policy: OracleClosePolicy::RequireClean,
                            },
                            "close_before_approve",
                        )
                        .expect("close");
                    assert_eq!(
                        harness.approve(10, HUMAN_A, "approve_closed"),
                        Err(OracleErrorKind::BufferClosed)
                    );
                }
                InvalidationCause::BufferCloseAfterApprove => {
                    harness.approve(10, HUMAN_A, "approve").expect("approve");
                    harness
                        .apply_op(
                            20,
                            OracleOp::Close {
                                actor: HUMAN_A,
                                nonce: 20,
                                base: 2,
                                buffer: 0,
                                policy: OracleClosePolicy::RequireClean,
                            },
                            "close_after_approve",
                        )
                        .expect("close");
                    assert_eq!(
                        harness.commit_proposal(10, "commit_closed"),
                        Err(OracleErrorKind::StaleApproval)
                    );
                }
                InvalidationCause::ProposerCommitRevokedAfterApprove => {
                    harness.approve(10, HUMAN_A, "approve").expect("approve");
                    harness.revoke_actor_cap(AGENT_C, OracleCap::Commit);
                    assert_eq!(
                        harness.commit_proposal(10, "commit_proposer_denied"),
                        Err(OracleErrorKind::PermissionDenied)
                    );
                    assert_eq!(harness.oracle.live[&0], "abcdef");
                }
                InvalidationCause::ProposerActorRevokedAfterApprove => {
                    harness.approve(10, HUMAN_A, "approve").expect("approve");
                    harness.revoke_actor(AGENT_C, "revoke_proposer");
                    assert_eq!(
                        harness.commit_proposal(10, "commit_proposer_inactive"),
                        Err(OracleErrorKind::InactiveActor)
                    );
                    assert_eq!(harness.oracle.live[&0], "abcdef");
                }
                InvalidationCause::ApproverReviewRevokedAfterApprove => {
                    harness.approve(10, HUMAN_A, "approve").expect("approve");
                    harness.revoke_actor_cap(HUMAN_A, OracleCap::Review);
                    assert_eq!(
                        harness.commit_proposal(10, "commit_approver_denied"),
                        Err(OracleErrorKind::PermissionDenied)
                    );
                    assert_eq!(harness.oracle.live[&0], "abcdef");
                }
                InvalidationCause::ApproverActorRevokedAfterApprove => {
                    harness.approve(10, HUMAN_A, "approve").expect("approve");
                    harness.revoke_actor(HUMAN_A, "revoke_approver");
                    assert_eq!(
                        harness.commit_proposal(10, "commit_approver_inactive"),
                        Err(OracleErrorKind::InactiveActor)
                    );
                    assert_eq!(harness.oracle.live[&0], "abcdef");
                }
                InvalidationCause::ConcurrentApprovedProposalWinsFirst => {
                    let second_patches = BTreeMap::from([(0, vec![OracleSplice::new(4, 6, "B2")])]);
                    harness
                        .propose(11, AUTOMATED_B, 11, 2, second_patches, "propose_second")
                        .expect("propose second");
                    harness
                        .approve(10, HUMAN_A, "approve_10")
                        .expect("approve 10");
                    harness
                        .approve(11, HUMAN_A, "approve_11")
                        .expect("approve 11");
                    harness
                        .commit_proposal(11, "commit_11_wins")
                        .expect("commit 11");
                    // Proposal 10's approval is now stale and rejected atomically!
                    assert_eq!(
                        harness.commit_proposal(10, "commit_10_stale"),
                        Err(OracleErrorKind::StaleApproval)
                    );
                    assert_eq!(harness.oracle.live[&0], "abcdB2");
                }
                InvalidationCause::DirectEditCommitsSameRegionBeforeProposal => {
                    harness.approve(10, HUMAN_A, "approve").expect("approve");
                    harness
                        .apply_op(
                            20,
                            single_edit(HUMAN_A, 20, 2, 0, 0, 2, "AG"),
                            "direct_commits_first",
                        )
                        .expect("direct commit");
                    assert_eq!(
                        harness.commit_proposal(10, "commit_after_direct"),
                        Err(OracleErrorKind::StaleApproval)
                    );
                }
            }
        }

        // Multi-reviewer & buffer-scoped revocation check.
        let mut harness = PairedHarness::new_three_actor(true);
        harness.register_extra_actor(
            HUMAN_D,
            OracleRole::Human,
            &[OracleCap::Read, OracleCap::Review],
        );
        harness
            .apply_op(
                1,
                OracleOp::Open {
                    actor: HUMAN_A,
                    nonce: 1,
                    base: 0,
                    name: "m".into(),
                    text: "hello".into(),
                },
                "open_m",
            )
            .expect("open");
        harness
            .propose(
                10,
                AGENT_C,
                10,
                1,
                BTreeMap::from([(0, vec![OracleSplice::new(0, 5, "world")])]),
                "propose_m",
            )
            .expect("propose");
        harness
            .approve(10, HUMAN_A, "approve_a")
            .expect("approve a");
        harness
            .approve(10, HUMAN_D, "approve_d")
            .expect("approve d");
        // Revoking first reviewer HUMAN_A still allows commit via latest reviewer HUMAN_D.
        harness.revoke_actor_cap(HUMAN_A, OracleCap::Review);
        // Revoking HUMAN_D now blocks commit.
        harness.revoke_actor_cap(HUMAN_D, OracleCap::Review);
        assert_eq!(
            harness.commit_proposal(10, "both_revoked"),
            Err(OracleErrorKind::PermissionDenied)
        );
        // Granting buffer-scoped Review to HUMAN_D restores commit validity, and revoking it
        // invalidates commit again atomically.
        harness.grant_buffer_cap(HUMAN_D, 0, OracleCap::Review);
        harness.revoke_buffer_cap(HUMAN_D, 0, OracleCap::Review);
        assert_eq!(
            harness.commit_proposal(10, "buffer_review_revoked"),
            Err(OracleErrorKind::PermissionDenied)
        );
        harness.grant_buffer_cap(HUMAN_D, 0, OracleCap::Review);
        harness
            .commit_proposal(10, "commit_restored_review")
            .expect("commit succeeds");
        assert_eq!(harness.oracle.live[&0], "world");

        // Standalone Proposal::commit double-commit rejection on Sequencer.
        let mut cat = BufferCatalog::new();
        cat.register(BufferId::new(0), Text::from_str("hello"))
            .expect("reg");
        let mut actors = ActorRegistry::new();
        actors
            .register(ActorId::new(HUMAN_A), ActorRole::Human)
            .expect("reg");
        actors
            .register(ActorId::new(AGENT_C), ActorRole::Agent)
            .expect("reg");
        let mut policy = AccessPolicy::new();
        policy.grant_actor(ActorId::new(HUMAN_A), Capability::Review);
        policy.grant_actor(ActorId::new(AGENT_C), Capability::Commit);
        let mut seq = Sequencer::new(cat, actors.clone());
        let base_map = BTreeMap::from([(0, "hello".to_owned())]);
        let txn = build_txn_from_oracle(
            AGENT_C,
            1,
            0,
            &BTreeMap::from([(0, vec![OracleSplice::new(0, 5, "world")])]),
            &base_map,
        );
        let proposal = Proposal::new(RequestId::new(1), txn);
        let approval = proposal
            .approve(&seq.snapshot(), &actors, &policy, ActorId::new(HUMAN_A))
            .expect("approve");
        assert!(proposal
            .commit(&approval, ActorId::new(HUMAN_A), &policy, &mut seq)
            .expect("first commit")
            .is_committed());
        let after_first = (seq.snapshot(), seq.journal().clone(), seq.history().clone());
        // Double-commit of the same Proposal+Approval MUST reject atomically!
        assert!(matches!(
            proposal.commit(&approval, ActorId::new(HUMAN_A), &policy, &mut seq),
            Err(ProposalError::RevisionMismatch {
                reviewed: Revision::ZERO,
                actual,
            }) if actual == Revision::new(1)
        ));
        assert_eq!(
            (seq.snapshot(), seq.journal().clone(), seq.history().clone()),
            after_first
        );
    }

    // =========================================================================
    // Law 5: Selective Undo and Redo Preservation
    // =========================================================================

    #[test]
    fn exhaustive_selective_undo_and_redo_preservation() {
        // Part A: Exhaustive 2-actor and 3-actor disjoint edit + selective undo + redo
        // across all submission orders and all undo target choices!
        let base_text = "a界b😀c!d";
        let b = char_boundaries(base_text);
        let ops3 = [
            // Actor 1 (Human): insert before "a"
            single_edit(HUMAN_A, 10, 1, 0, b[0], b[0], "PRE"),
            // Actor 2 (Automated): replace "b" with multi-byte text
            single_edit(AUTOMATED_B, 20, 1, 0, b[2], b[3], "オート"),
            // Actor 3 (Agent): delete "c!"
            single_edit(AGENT_C, 30, 1, 0, b[4], b[6], ""),
        ];
        let owners = [(HUMAN_A, 10), (AUTOMATED_B, 20), (AGENT_C, 30)];

        for perm in all_permutations(3) {
            for target_idx in 0..3 {
                let mut harness = PairedHarness::new_three_actor(true);
                harness
                    .apply_op(
                        1,
                        OracleOp::Open {
                            actor: HUMAN_A,
                            nonce: 1,
                            base: 0,
                            name: "u".into(),
                            text: base_text.into(),
                        },
                        "open_u",
                    )
                    .expect("open");
                for (step, &idx) in perm.iter().enumerate() {
                    harness
                        .apply_op(10 + step as u64, ops3[idx].clone(), "apply_3actor_for_undo")
                        .expect("commit");
                }
                let text_all_three = harness.oracle.live[&0].clone();
                assert_eq!(text_all_three, "PREa界オート😀d");

                // Compute independent ground truth when only the OTHER two edits apply!
                let mut two_only = PairedHarness::new_three_actor(true);
                two_only
                    .apply_op(
                        1,
                        OracleOp::Open {
                            actor: HUMAN_A,
                            nonce: 1,
                            base: 0,
                            name: "u".into(),
                            text: base_text.into(),
                        },
                        "open_ref",
                    )
                    .expect("open");
                for (step, &idx) in perm.iter().enumerate() {
                    if idx != target_idx {
                        two_only
                            .apply_op(10 + step as u64, ops3[idx].clone(), "ref_two_only")
                            .expect("commit");
                    }
                }
                let expected_after_undo = two_only.oracle.live[&0].clone();

                let (owner, target_nonce) = owners[target_idx];
                let other_actor = owners[(target_idx + 1) % 3].0;

                // Non-owner cannot undo or redo.
                assert_eq!(
                    harness.apply_op(
                        50,
                        OracleOp::Undo {
                            actor: other_actor,
                            target: (owner, target_nonce),
                            txn: (other_actor, 50),
                        },
                        "non_owner_undo"
                    ),
                    Err(OracleErrorKind::NotOwner)
                );
                // Redo before undo is rejected atomically.
                assert_eq!(
                    harness.apply_op(
                        51,
                        OracleOp::Redo {
                            actor: owner,
                            target: (owner, target_nonce),
                            txn: (owner, 51),
                        },
                        "redo_before_undo"
                    ),
                    Err(OracleErrorKind::NotUndone)
                );

                // Selectively undo target_idx: other actors' edits MUST remain byte-for-byte intact!
                harness
                    .apply_op(
                        60,
                        OracleOp::Undo {
                            actor: owner,
                            target: (owner, target_nonce),
                            txn: (owner, 60),
                        },
                        "selective_undo",
                    )
                    .expect("selective undo commits");
                assert_eq!(
                    harness.oracle.live[&0], expected_after_undo,
                    "selective undo failed to preserve other actors' edits (perm={perm:?}, target={target_idx})"
                );

                // Double undo and undo of compensation commit are rejected atomically.
                assert_eq!(
                    harness.apply_op(
                        61,
                        OracleOp::Undo {
                            actor: owner,
                            target: (owner, target_nonce),
                            txn: (owner, 61),
                        },
                        "double_undo"
                    ),
                    Err(OracleErrorKind::AlreadyUndone)
                );
                assert_eq!(
                    harness.apply_op(
                        62,
                        OracleOp::Undo {
                            actor: owner,
                            target: (owner, 60),
                            txn: (owner, 62),
                        },
                        "undo_compensation"
                    ),
                    Err(OracleErrorKind::InvalidTransaction)
                );

                // Now redo target_idx: restores all three edits consistently!
                harness
                    .apply_op(
                        70,
                        OracleOp::Redo {
                            actor: owner,
                            target: (owner, target_nonce),
                            txn: (owner, 70),
                        },
                        "selective_redo",
                    )
                    .expect("selective redo commits");
                assert_eq!(harness.oracle.live[&0], text_all_three);

                // Second undo->redo cycle after an intervening disjoint edit by another actor!
                let cur_rev = harness.oracle.revision;
                let cur_len = harness.oracle.live[&0].len();
                harness
                    .apply_op(
                        75,
                        single_edit(other_actor, 75, cur_rev, 0, cur_len, cur_len, "TAIL"),
                        "intervening_tail",
                    )
                    .expect("intervening tail");
                harness
                    .apply_op(
                        80,
                        OracleOp::Undo {
                            actor: owner,
                            target: (owner, target_nonce),
                            txn: (owner, 80),
                        },
                        "second_undo_after_tail",
                    )
                    .expect("second undo");
                assert_eq!(
                    harness.oracle.live[&0],
                    format!("{expected_after_undo}TAIL")
                );
                harness
                    .apply_op(
                        90,
                        OracleOp::Redo {
                            actor: owner,
                            target: (owner, target_nonce),
                            txn: (owner, 90),
                        },
                        "second_redo_after_tail",
                    )
                    .expect("second redo");
                assert_eq!(harness.oracle.live[&0], format!("{text_all_three}TAIL"));
            }
        }

        // Part B: Multi-buffer selective undo & redo + partial multi-buffer conflict rejection.
        let mut harness = PairedHarness::new_three_actor(true);
        for (n, text) in [(0, "abcdef"), (1, "012345")] {
            harness
                .apply_op(
                    n + 1,
                    OracleOp::Open {
                        actor: HUMAN_A,
                        nonce: n + 1,
                        base: n,
                        name: format!("su-{n}"),
                        text: text.into(),
                    },
                    "su_open",
                )
                .expect("open");
        }
        // Actor 1 edits both buffers.
        harness
            .apply_op(
                10,
                OracleOp::Edit {
                    actor: HUMAN_A,
                    nonce: 10,
                    base: 2,
                    patches: BTreeMap::from([
                        (0, vec![OracleSplice::new(1, 3, "XX")]),
                        (1, vec![OracleSplice::new(1, 3, "YY")]),
                    ]),
                },
                "mb_actor1_edit",
            )
            .expect("actor 1 edit");
        // Actor 2 edits buffer 0 disjointly and buffer 1 disjointly.
        harness
            .apply_op(
                11,
                OracleOp::Edit {
                    actor: AUTOMATED_B,
                    nonce: 11,
                    base: 3,
                    patches: BTreeMap::from([
                        (0, vec![OracleSplice::new(5, 6, "Z")]),
                        (1, vec![OracleSplice::new(5, 6, "W")]),
                    ]),
                },
                "mb_actor2_disjoint",
            )
            .expect("actor 2 disjoint");
        // Selective undo of Actor 1 reverts both buffers while preserving Actor 2's Z and W!
        harness
            .apply_op(
                12,
                OracleOp::Undo {
                    actor: HUMAN_A,
                    target: (HUMAN_A, 10),
                    txn: (HUMAN_A, 12),
                },
                "mb_undo",
            )
            .expect("mb undo");
        assert_eq!(harness.oracle.live[&0], "abcdeZ");
        assert_eq!(harness.oracle.live[&1], "01234W");
        // Now Actor 2 touches buffer 1 at boundary 3 (touching [1..3] of Actor 1's undone span).
        harness
            .apply_op(
                13,
                single_edit(AUTOMATED_B, 13, 5, 1, 3, 4, "!"),
                "mb_touching_after_undo",
            )
            .expect("touching edit commits");
        // Actor 1's Redo MUST reject atomically with Conflict; buffer 0 is NOT modified!
        assert_eq!(
            harness.apply_op(
                14,
                OracleOp::Redo {
                    actor: HUMAN_A,
                    target: (HUMAN_A, 10),
                    txn: (HUMAN_A, 14),
                },
                "mb_redo_conflict"
            ),
            Err(OracleErrorKind::Conflict)
        );
        assert_eq!(harness.oracle.live[&0], "abcdeZ");
        assert_eq!(harness.oracle.live[&1], "012!4W");

        // Part C: Conflicting undo (touching & overlapping) leaves other actors' edits intact.
        for (conflict_span, label) in [
            ((1, 2), "undo_conflict_overlap"),
            ((3, 4), "undo_conflict_touch_right"),
            ((1, 1), "undo_conflict_insert_left_edge"),
            ((3, 3), "undo_conflict_insert_right_edge"),
        ] {
            let mut h = PairedHarness::new_three_actor(true);
            h.apply_op(
                1,
                OracleOp::Open {
                    actor: HUMAN_A,
                    nonce: 1,
                    base: 0,
                    name: "cu".into(),
                    text: "abcdef".into(),
                },
                "cu_open",
            )
            .expect("open");
            // Actor 1 replaces [1..3] ("bc") with "XY" -> "aXYdef" (inverse spans [1..3]).
            h.apply_op(2, single_edit(HUMAN_A, 2, 1, 0, 1, 3, "XY"), "cu_target")
                .expect("target");
            // Actor 2 makes a touching/overlapping edit on "aXYdef".
            h.apply_op(
                3,
                single_edit(AUTOMATED_B, 3, 2, 0, conflict_span.0, conflict_span.1, "Q"),
                label,
            )
            .expect("intervening conflict");
            let preserved = h.oracle.live[&0].clone();
            assert_eq!(
                h.apply_op(
                    4,
                    OracleOp::Undo {
                        actor: HUMAN_A,
                        target: (HUMAN_A, 2),
                        txn: (HUMAN_A, 4),
                    },
                    &format!("{label}_reject")
                ),
                Err(OracleErrorKind::Conflict)
            );
            assert_eq!(h.oracle.live[&0], preserved);
        }

        // Part D: Cross-check standalone SelectiveUndo & Redo over Sequencer.
        let mut cat = BufferCatalog::new();
        cat.register(BufferId::new(0), Text::from_str("0123456789"))
            .expect("reg");
        let mut actors = ActorRegistry::new();
        actors
            .register(ActorId::new(HUMAN_A), ActorRole::Human)
            .expect("reg");
        actors
            .register(ActorId::new(AUTOMATED_B), ActorRole::Agent)
            .expect("reg");
        let mut engine = Redo::new(SelectiveUndo::new(Sequencer::new(cat, actors)));
        let base_map = BTreeMap::from([(0, "0123456789".to_owned())]);
        let t1 = build_txn_from_oracle(
            HUMAN_A,
            1,
            0,
            &BTreeMap::from([(0, vec![OracleSplice::new(1, 2, "AA")])]),
            &base_map,
        );
        assert!(engine.submit(t1).is_committed());
        let after_t1 = BTreeMap::from([(0, "0AA23456789".to_owned())]);
        let t2 = build_txn_from_oracle(
            AUTOMATED_B,
            1,
            1,
            &BTreeMap::from([(0, vec![OracleSplice::new(7, 8, "BB")])]),
            &after_t1,
        );
        assert!(engine.submit(t2).is_committed());
        assert_eq!(
            engine
                .sequencer()
                .snapshot()
                .catalog()
                .get(BufferId::new(0))
                .expect("live")
                .as_str(),
            "0AA2345BB789"
        );
        let target = TxnId::new(ActorId::new(HUMAN_A), Nonce::new(1));
        assert_eq!(
            engine.undo(
                ActorId::new(AUTOMATED_B),
                target,
                TxnId::new(ActorId::new(AUTOMATED_B), Nonce::new(9))
            ),
            Err(UndoError::NotOwner {
                target,
                actor: ActorId::new(AUTOMATED_B)
            })
        );
        engine
            .undo(
                ActorId::new(HUMAN_A),
                target,
                TxnId::new(ActorId::new(HUMAN_A), Nonce::new(2)),
            )
            .expect("undo");
        assert_eq!(
            engine
                .sequencer()
                .snapshot()
                .catalog()
                .get(BufferId::new(0))
                .expect("live")
                .as_str(),
            "012345BB789"
        );
        engine
            .redo(
                ActorId::new(HUMAN_A),
                target,
                TxnId::new(ActorId::new(HUMAN_A), Nonce::new(3)),
            )
            .expect("redo");
        assert_eq!(
            engine
                .sequencer()
                .snapshot()
                .catalog()
                .get(BufferId::new(0))
                .expect("live")
                .as_str(),
            "0AA2345BB789"
        );
        let compensation = TxnId::new(ActorId::new(HUMAN_A), Nonce::new(2));
        let redo_id = TxnId::new(ActorId::new(HUMAN_A), Nonce::new(3));
        let before_redundant_redo = (
            engine.sequencer().snapshot(),
            engine.sequencer().journal().clone(),
            engine.sequencer().history().clone(),
        );
        // Re-redoing the original target after redo has made its latest version live rejects as NotUndone.
        assert_eq!(
            engine.redo(
                ActorId::new(HUMAN_A),
                target,
                TxnId::new(ActorId::new(HUMAN_A), Nonce::new(4))
            ),
            Err(RedoError::NotUndone(target))
        );
        // Re-redoing the already-redone compensation rejects as AlreadyRedone.
        assert_eq!(
            engine.redo(
                ActorId::new(HUMAN_A),
                compensation,
                TxnId::new(ActorId::new(HUMAN_A), Nonce::new(4))
            ),
            Err(RedoError::AlreadyRedone {
                compensation,
                redo: redo_id,
            })
        );
        assert_eq!(
            (
                engine.sequencer().snapshot(),
                engine.sequencer().journal().clone(),
                engine.sequencer().history().clone(),
            ),
            before_redundant_redo
        );
    }

    // =========================================================================
    // Law 6: Replay After Compaction and Retention
    // =========================================================================

    #[test]
    fn exhaustive_replay_after_compaction_and_retention() {
        #[derive(Clone)]
        enum HistoryStep {
            Apply(u64, OracleOp),
            ProposeApproveCommit(u64, u64, u64, u64, OraclePatches),
        }

        let sentinel = "superseded-secret-payload-99182";
        // Complete 3-actor multi-buffer history covering Open, superseded Edit,
        // Proposal approve+commit, Undo, Redo, and stale-base disjoint rebased Edit.
        let script = vec![
            HistoryStep::Apply(
                1,
                OracleOp::Open {
                    actor: HUMAN_A,
                    nonce: 1,
                    base: 0,
                    name: "main".into(),
                    text: "0123456789".into(),
                },
            ),
            HistoryStep::Apply(
                2,
                OracleOp::Open {
                    actor: AUTOMATED_B,
                    nonce: 2,
                    base: 1,
                    name: "aux".into(),
                    text: "abcdef".into(),
                },
            ),
            // Rev 3: Human replaces [0..2] with sentinel
            HistoryStep::Apply(3, single_edit(HUMAN_A, 3, 2, 0, 0, 2, sentinel)),
            // Rev 4: Human supersedes sentinel with "HH"
            HistoryStep::Apply(4, single_edit(HUMAN_A, 4, 3, 0, 0, sentinel.len(), "HH")),
            // Rev 5: Automated edits buffer 0 at [6..8] (base 4 -> "HH2345AA89")
            HistoryStep::Apply(5, single_edit(AUTOMATED_B, 5, 4, 0, 6, 8, "AA")),
            // Rev 6: Agent proposes edit on buffer 1 [0..2] -> "AG"
            HistoryStep::ProposeApproveCommit(
                6,
                AGENT_C,
                6,
                5,
                BTreeMap::from([(1, vec![OracleSplice::new(0, 2, "AG")])]),
            ),
            // Rev 7: Automated undoes its commit (AUTOMATED_B, 5)
            HistoryStep::Apply(
                7,
                OracleOp::Undo {
                    actor: AUTOMATED_B,
                    target: (AUTOMATED_B, 5),
                    txn: (AUTOMATED_B, 7),
                },
            ),
            // Rev 8: Automated redoes its commit (AUTOMATED_B, 5)
            HistoryStep::Apply(
                8,
                OracleOp::Redo {
                    actor: AUTOMATED_B,
                    target: (AUTOMATED_B, 5),
                    txn: (AUTOMATED_B, 8),
                },
            ),
            // Rev 9: Human edits buffer 1 disjointly at [4..6] -> "Z" from stale base 6
            // (rebased across rev 7 Undo and rev 8 Redo on buffer 0).
            HistoryStep::Apply(9, single_edit(HUMAN_A, 9, 6, 1, 4, 6, "Z")),
        ];

        let run_step = |harness: &mut PairedHarness, step: &HistoryStep, tag: &str| match step {
            HistoryStep::Apply(req, op) => {
                harness.apply_op(*req, op.clone(), tag).expect("step apply");
            }
            HistoryStep::ProposeApproveCommit(req, proposer, nonce, base, patches) => {
                harness
                    .propose(*req, *proposer, *nonce, *base, patches.clone(), tag)
                    .expect("step propose");
                harness.approve(*req, HUMAN_A, tag).expect("step approve");
                harness.commit_proposal(*req, tag).expect("step commit");
            }
        };

        // Run uncompacted baseline (floor = 0).
        let mut uncompacted = PairedHarness::new_three_actor(true);
        let mut uncompacted_snapshots = vec![uncompacted.coord.snapshot()];
        for (i, step) in script.iter().enumerate() {
            run_step(&mut uncompacted, step, &format!("uncompacted_{i}"));
            uncompacted_snapshots.push(uncompacted.coord.snapshot());
        }
        let uncompacted_obs = observe_coordinator(
            &uncompacted.coord,
            &uncompacted.known_actors,
            &uncompacted.known_requests,
            &uncompacted.known_txns,
        );

        // Exhaustively test EVERY split point k in 0..=script.len() and EVERY valid floor f <= k.
        // Note: if step 7 (Undo of rev 5) or step 8 (Redo of rev 7) is in the tail, compacting to
        // floor >= 5 (for Undo) or floor >= 7 (for Redo) intentionally expires those inverse targets.
        // So we verify BOTH:
        // - Exact equivalence for every floor f that preserves tail replay bases/inverses, AND
        // - Exact atomic BaseExpired rejection when f expires a tail undo/redo target!
        let mut full_equiv_runs = 0usize;
        let mut expired_tail_runs = 0usize;

        for split in 0..=script.len() {
            for floor in 0..=split as u64 {
                let mut compacted = PairedHarness::new_three_actor(true);
                for (i, step) in script[..split].iter().enumerate() {
                    run_step(&mut compacted, step, &format!("prefix_{split}_{floor}_{i}"));
                }
                compacted
                    .compact(floor, &format!("compact_{split}_{floor}"))
                    .expect("compact");
                if floor >= 4 {
                    // Superseded sentinel from rev 3 must not leak in compacted checkpoint!
                    let cp_debug = format!("{:?}", compacted.coord.checkpoint());
                    assert!(
                        !cp_debug.contains(sentinel),
                        "superseded payload leaked into checkpoint at floor {floor}"
                    );
                }
                compacted.checkpoint_restore(&format!("restore_{split}_{floor}"));

                let tail_hits_expired_undo = split < 7 && floor >= 5;
                let tail_hits_expired_redo = split < 8 && floor >= 7;
                let tail_hits_expired_edit = split < 9 && floor > 6;
                if tail_hits_expired_undo || tail_hits_expired_redo || tail_hits_expired_edit {
                    // Replay up to the expired undo/redo/stale-base edit and verify atomic BaseExpired rejection!
                    for (offset, step) in script[split..].iter().enumerate() {
                        let idx = split + offset;
                        if (idx == 6 && floor >= 5)
                            || (idx == 7 && floor >= 7)
                            || (idx == 8 && floor > 6)
                        {
                            let HistoryStep::Apply(req, op) = step else {
                                unreachable!()
                            };
                            assert_eq!(
                                compacted.apply_op(*req, op.clone(), "expired_tail_step"),
                                Err(OracleErrorKind::BaseExpired)
                            );
                            expired_tail_runs += 1;
                            break;
                        }
                        run_step(&mut compacted, step, "tail_before_expiry");
                    }
                    continue;
                }

                // Replay the entire retained tail on the compacted+restored coordinator.
                for (offset, step) in script[split..].iter().enumerate() {
                    run_step(
                        &mut compacted,
                        step,
                        &format!("tail_{split}_{floor}_{offset}"),
                    );
                }

                // Compare every observable field against uncompacted baseline!
                let comp_obs = observe_coordinator(
                    &compacted.coord,
                    &compacted.known_actors,
                    &compacted.known_requests,
                    &compacted.known_txns,
                );
                assert_eq!(comp_obs.revision, uncompacted_obs.revision);
                assert_eq!(comp_obs.live_buffers, uncompacted_obs.live_buffers);
                assert_eq!(comp_obs.tombstones, uncompacted_obs.tombstones);
                assert_eq!(comp_obs.names, uncompacted_obs.names);
                assert_eq!(comp_obs.dirty_buffers, uncompacted_obs.dirty_buffers);
                assert_eq!(comp_obs.journal, uncompacted_obs.journal);
                assert_eq!(
                    comp_obs.receipt_by_request,
                    uncompacted_obs.receipt_by_request
                );
                assert_eq!(comp_obs.receipt_by_txn, uncompacted_obs.receipt_by_txn);
                assert_eq!(
                    comp_obs.edit_receipt_by_request,
                    uncompacted_obs.edit_receipt_by_request
                );
                assert_eq!(
                    comp_obs.history_replay_catalog,
                    uncompacted_obs.live_buffers
                );
                assert_eq!(
                    comp_obs.delivery_replay_catalog,
                    uncompacted_obs.live_buffers
                );

                // RetainedHistory::replay for EVERY base in 0..=tip:
                let tip = comp_obs.revision;
                for base in 0..=tip {
                    let comp_replay = compacted
                        .coord
                        .workspace()
                        .history()
                        .replay(Revision::new(base));
                    let uncomp_replay = uncompacted
                        .coord
                        .workspace()
                        .history()
                        .replay(Revision::new(base));
                    if base < floor {
                        assert!(matches!(
                            comp_replay,
                            Err(RetentionError::BaseExpired { .. })
                        ));
                    } else {
                        assert_eq!(
                            comp_replay.expect("retained replay"),
                            uncomp_replay.expect("uncompacted replay")
                        );
                    }
                }

                // DeliveryLog::catch_up for EVERY since in 0..=tip:
                let dfloor = compacted.coord.workspace().delivery().floor().raw();
                for since in 0..=tip {
                    let cu = compacted
                        .coord
                        .workspace()
                        .delivery()
                        .catch_up(Revision::new(since))
                        .expect("catch_up");
                    if since < dfloor {
                        let CatchUp::Snapshot(snap) = cu else {
                            panic!("expected snapshot below delivery floor");
                        };
                        assert_eq!(catalog_to_map(snap.catalog()), uncompacted_obs.live_buffers);
                        assert_eq!(snap.revision().raw(), tip);
                    } else {
                        let CatchUp::Events(events) = cu else {
                            panic!("expected incremental events at/above delivery floor");
                        };
                        // Verify event receipts match uncompacted delivery events from the same revision.
                        let uncomp_cu = uncompacted
                            .coord
                            .workspace()
                            .delivery()
                            .catch_up(Revision::new(since))
                            .expect("uncompacted catch_up");
                        if let CatchUp::Events(uncomp_events) = uncomp_cu {
                            let comp_receipts: Vec<_> =
                                events.iter().map(|e| e.receipt()).collect();
                            let uncomp_receipts: Vec<_> =
                                uncomp_events.iter().map(|e| e.receipt()).collect();
                            assert_eq!(comp_receipts, uncomp_receipts);
                        }
                        if since == dfloor {
                            let (replayed_cat, replayed_rev) = delivery::replay(
                                compacted.coord.workspace().delivery().base_snapshot(),
                                &events,
                            )
                            .expect("delivery replay");
                            assert_eq!(catalog_to_map(&replayed_cat), uncompacted_obs.live_buffers);
                            assert_eq!(replayed_rev.raw(), tip);
                        }
                    }
                }

                // Exact retry of pre-floor request 3 returns identical receipt & zero mutation!
                let pre_floor_retry = compacted
                    .apply_op(
                        3,
                        single_edit(HUMAN_A, 3, 2, 0, 0, 2, sentinel),
                        "retry_pre_floor",
                    )
                    .expect("pre-floor retry succeeds");
                assert_eq!(
                    pre_floor_retry.receipt,
                    Some(uncompacted_obs.receipt_by_request[&3])
                );

                // Subsequent edit & conflict behavior equivalence against uncompacted clone:
                let mut uncomp_clone =
                    Coordinator::restore(&uncompacted.coord.checkpoint()).expect("clone uncomp");
                let mut comp_clone =
                    Coordinator::restore(&compacted.coord.checkpoint()).expect("clone comp");

                // 1) Stale edit at base < floor rejects with BaseExpired in compacted (if floor > 4).
                if floor > 4 {
                    let expired_op = build_prod_op(
                        &single_edit(HUMAN_A, 200, 4, 0, 2, 3, "Q"),
                        &comp_clone,
                        &uncompacted.saved_texts[&4],
                    );
                    let before_cp = comp_clone.checkpoint();
                    assert!(matches!(
                        comp_clone.apply(RequestId::new(200), expired_op),
                        Err(CoordinatorError::Checkpoint(CheckpointError::Retention(
                            RetentionError::BaseExpired { .. }
                        )))
                    ));
                    assert_eq!(comp_clone.checkpoint(), before_cp);
                }

                // 2) Conflicting edit at base = max(floor, 8) (touching/overlapping rev 9 on buf 1):
                if floor <= 8 {
                    let conflict_op = build_prod_op(
                        &single_edit(AUTOMATED_B, 201, 8, 1, 4, 5, "K"),
                        &comp_clone,
                        &uncompacted.saved_texts[&8],
                    );
                    let u_err = uncomp_clone
                        .apply(RequestId::new(201), conflict_op.clone())
                        .expect_err("uncomp conflict");
                    let c_err = comp_clone
                        .apply(RequestId::new(201), conflict_op)
                        .expect_err("comp conflict");
                    assert_eq!(c_err, u_err);
                }

                // 3) Disjoint subsequent edit at base = max(floor, 8) commits identically!
                let base_ok = floor.max(8);
                let ok_op = build_prod_op(
                    &single_edit(HUMAN_A, 202, base_ok, 0, 2, 3, "M"),
                    &comp_clone,
                    &uncompacted.saved_texts[&base_ok],
                );
                let u_out = uncomp_clone
                    .apply(RequestId::new(202), ok_op.clone())
                    .expect("uncomp subsequent");
                let c_out = comp_clone
                    .apply(RequestId::new(202), ok_op)
                    .expect("comp subsequent");
                assert_eq!(c_out, u_out);
                assert_eq!(
                    catalog_to_map(comp_clone.snapshot().catalog()),
                    catalog_to_map(uncomp_clone.snapshot().catalog())
                );

                full_equiv_runs += 1;
            }
        }
        assert!(full_equiv_runs >= 30);
        assert!(expired_tail_runs >= 5);
        assert_eq!(uncompacted_snapshots.len(), script.len() + 1);

        // Also test CheckpointState::replay directly: uncompacted replay vs compact+restore+tail replay.
        let mut actors = ActorRegistry::new();
        actors
            .register(ActorId::new(HUMAN_A), ActorRole::Human)
            .expect("reg");
        actors
            .register(ActorId::new(AUTOMATED_B), ActorRole::Agent)
            .expect("reg");
        let base_zero = BTreeMap::from([(0, "abcdef".to_owned())]);
        let after_one = BTreeMap::from([(0, "Xbcdef".to_owned())]);
        let after_two = BTreeMap::from([(0, "XbcdYf".to_owned())]);
        let cp_ops = vec![
            CheckpointOp::Open {
                request: RequestId::new(1),
                intent: EditIntent::for_actor(ActorId::new(HUMAN_A), Nonce::new(1), Revision::ZERO),
                name: "cp".into(),
                text: Text::from_str("abcdef"),
            },
            CheckpointOp::Edit {
                request: RequestId::new(2),
                txn: build_txn_from_oracle(
                    HUMAN_A,
                    2,
                    1,
                    &BTreeMap::from([(0, vec![OracleSplice::new(0, 1, "X")])]),
                    &base_zero,
                ),
            },
            CheckpointOp::Edit {
                request: RequestId::new(3),
                txn: build_txn_from_oracle(
                    AUTOMATED_B,
                    3,
                    2,
                    &BTreeMap::from([(0, vec![OracleSplice::new(4, 5, "Y")])]),
                    &after_one,
                ),
            },
            CheckpointOp::Edit {
                request: RequestId::new(4),
                txn: build_txn_from_oracle(
                    HUMAN_A,
                    4,
                    3,
                    &BTreeMap::from([(0, vec![OracleSplice::new(2, 3, "Z")])]),
                    &after_two,
                ),
            },
            CheckpointOp::Close {
                request: RequestId::new(5),
                intent: EditIntent::for_actor(
                    ActorId::new(HUMAN_A),
                    Nonce::new(5),
                    Revision::new(4),
                ),
                buffer: BufferId::new(0),
                policy: ClosePolicy::Discard,
            },
        ];

        let mut full_cp = CheckpointState::new(actors.clone());
        let full_outcomes = full_cp.replay(&cp_ops).expect("full replay");

        let mut split_cp = CheckpointState::new(actors);
        let prefix_outcomes = split_cp.replay(&cp_ops[..2]).expect("prefix replay");
        split_cp.compact(Revision::new(2)).expect("compact cp");
        let mut restored_cp = split_cp.checkpoint().restore().expect("restore cp");
        let tail_outcomes = restored_cp.replay(&cp_ops[2..]).expect("tail replay");
        let combined: Vec<_> = prefix_outcomes.into_iter().chain(tail_outcomes).collect();
        assert_eq!(combined, full_outcomes);
        assert_eq!(
            catalog_to_map(restored_cp.snapshot().catalog()),
            catalog_to_map(full_cp.snapshot().catalog())
        );
        assert!(restored_cp
            .snapshot()
            .catalog()
            .is_tombstoned(BufferId::new(0)));
        assert_eq!(
            restored_cp.snapshot().catalog().tombstoned_ids(),
            full_cp.snapshot().catalog().tombstoned_ids()
        );
        assert_eq!(restored_cp.journal(), full_cp.journal());
    }

    // =========================================================================
    // Deterministic Randomized Multi-Actor State-Machine Driver with Coverage
    // =========================================================================

    #[derive(Default, Debug)]
    struct CoverageCounters {
        disjoint_rebased_edit: usize,
        touching_conflict_rejected: usize,
        overlapping_conflict_rejected: usize,
        receipt_idempotent_retry: usize,
        conflicting_request_reuse_rejected: usize,
        txn_id_reuse_rejected: usize,
        proposal_committed: usize,
        proposal_invalidated_by_edit: usize,
        proposal_invalidated_by_close: usize,
        proposal_invalidated_by_permission_revoke: usize,
        proposal_non_human_approve_rejected: usize,
        selective_undo_preserved_other_actor: usize,
        selective_undo_conflict_rejected: usize,
        selective_redo_preserved_other_actor: usize,
        selective_redo_conflict_rejected: usize,
        compaction_checkpoint_restore: usize,
        compaction_expired_rejected: usize,
        delivery_catch_up_events: usize,
        delivery_catch_up_snapshot: usize,
        restore_edit_committed: usize,
        session_sync_verified: usize,
    }

    impl CoverageCounters {
        fn assert_all_nonzero(&self) {
            assert!(self.disjoint_rebased_edit > 0, "disjoint_rebased_edit=0");
            assert!(
                self.touching_conflict_rejected > 0,
                "touching_conflict_rejected=0"
            );
            assert!(
                self.overlapping_conflict_rejected > 0,
                "overlapping_conflict_rejected=0"
            );
            assert!(
                self.receipt_idempotent_retry > 0,
                "receipt_idempotent_retry=0"
            );
            assert!(
                self.conflicting_request_reuse_rejected > 0,
                "conflicting_request_reuse_rejected=0"
            );
            assert!(self.txn_id_reuse_rejected > 0, "txn_id_reuse_rejected=0");
            assert!(self.proposal_committed > 0, "proposal_committed=0");
            assert!(
                self.proposal_invalidated_by_edit > 0,
                "proposal_invalidated_by_edit=0"
            );
            assert!(
                self.proposal_invalidated_by_close > 0,
                "proposal_invalidated_by_close=0"
            );
            assert!(
                self.proposal_invalidated_by_permission_revoke > 0,
                "proposal_invalidated_by_permission_revoke=0"
            );
            assert!(
                self.proposal_non_human_approve_rejected > 0,
                "proposal_non_human_approve_rejected=0"
            );
            assert!(
                self.selective_undo_preserved_other_actor > 0,
                "selective_undo_preserved_other_actor=0"
            );
            assert!(
                self.selective_undo_conflict_rejected > 0,
                "selective_undo_conflict_rejected=0"
            );
            assert!(
                self.selective_redo_preserved_other_actor > 0,
                "selective_redo_preserved_other_actor=0"
            );
            assert!(
                self.selective_redo_conflict_rejected > 0,
                "selective_redo_conflict_rejected=0"
            );
            assert!(
                self.compaction_checkpoint_restore > 0,
                "compaction_checkpoint_restore=0"
            );
            assert!(
                self.compaction_expired_rejected > 0,
                "compaction_expired_rejected=0"
            );
            assert!(
                self.delivery_catch_up_events > 0,
                "delivery_catch_up_events=0"
            );
            assert!(
                self.delivery_catch_up_snapshot > 0,
                "delivery_catch_up_snapshot=0"
            );
            assert!(self.restore_edit_committed > 0, "restore_edit_committed=0");
            assert!(self.session_sync_verified > 0, "session_sync_verified=0");
        }
    }

    #[test]
    fn randomized_multi_actor_state_machine_laws_with_coverage() {
        let mut cov = CoverageCounters::default();
        let seeds = [0, 1, 7, 42, 99, 1337, u64::MAX];

        assert_law(
            &seeds,
            |rng: &mut Rng| (0..90).map(|_| rng.next_u64()).collect::<Vec<_>>(),
            |choices| {
                let mut harness = PairedHarness::new_three_actor(true);
                let mut next_req = 10u64;
                let mut next_nonce = 10u64;
                let mut alloc = || {
                    let r = next_req;
                    let n = next_nonce;
                    next_req += 1;
                    next_nonce += 1;
                    (r, n)
                };

                // Open 2 initial buffers with multi-byte UTF-8 and ASCII content.
                for (name, initial) in [("doc0", "a界b😀c!d012"), ("doc1", "0123456789")] {
                    let (req, nonce) = alloc();
                    let base = harness.oracle.revision;
                    harness
                        .apply_op(
                            req,
                            OracleOp::Open {
                                actor: HUMAN_A,
                                nonce,
                                base,
                                name: name.into(),
                                text: initial.into(),
                            },
                            "rand_init_open",
                        )
                        .expect("init open");
                }

                // Keep a live WorkspaceSession synced with the coordinator's snapshot stream
                // until a restore resets snapshot origin.
                let mut session = Some(WorkspaceSession::new(
                    harness.coord.snapshot(),
                    harness.coord.workspace().actors().clone(),
                    harness.coord.access_mut().clone(),
                ));

                let mut committed_edits: Vec<(u64, OracleOp, OracleOutcome)> = Vec::new();
                let mut undoable_targets: Vec<(u64, u64, u64, usize, usize)> = Vec::new();
                let mut undone_targets: Vec<(u64, u64, u64, usize, usize)> = Vec::new();

                let sync_session = |sess: &mut Option<WorkspaceSession>,
                                    coord: &Coordinator,
                                    cov: &mut CoverageCounters| {
                    if let Some(s) = sess.as_mut() {
                        let rev = s.revision();
                        if rev < coord.workspace().revision() {
                            match coord.workspace().delivery().catch_up(rev) {
                                Ok(CatchUp::Events(events)) => {
                                    for ev in &events {
                                        s.advance(ev).expect("session advance");
                                    }
                                    assert_eq!(s.revision(), coord.workspace().revision());
                                    for id in coord.snapshot().catalog().live_ids() {
                                        assert_eq!(
                                            s.read(ActorId::new(HUMAN_A), id)
                                                .expect("session read")
                                                .as_str(),
                                            coord
                                                .snapshot()
                                                .catalog()
                                                .get(id)
                                                .expect("live")
                                                .as_str()
                                        );
                                    }
                                    cov.session_sync_verified += 1;
                                }
                                _ => {
                                    // Lifecycle op or compaction jumped delivery floor; re-seat session.
                                    *sess = None;
                                }
                            }
                        }
                    }
                };

                for (step_idx, &word) in choices.iter().enumerate() {
                    let action = word % 14;
                    let actors = [HUMAN_A, AUTOMATED_B, AGENT_C];
                    let actor = actors[(word as usize / 14) % 3];
                    let other_actor = actors[((word as usize / 14) + 1) % 3];
                    let live_ids: Vec<u64> = harness.oracle.live.keys().copied().collect();
                    let buf = live_ids[(word as usize / 42) % live_ids.len()];
                    let tag = format!("step_{step_idx}_act_{action}");

                    match action {
                        // 0: Disjoint stale-base edit (two actors editing strictly separated regions).
                        0 => {
                            let text = harness.oracle.live[&buf].clone();
                            let bounds = char_boundaries(&text);
                            if bounds.len() >= 6 {
                                let base = harness.oracle.revision;
                                let (r1, n1) = alloc();
                                let repl1 = if &text[bounds[0]..bounds[1]] == "L" {
                                    "界"
                                } else {
                                    "L"
                                };
                                harness
                                    .apply_op(
                                        r1,
                                        single_edit(
                                            actor, n1, base, buf, bounds[0], bounds[1], repl1,
                                        ),
                                        &format!("{tag}_first"),
                                    )
                                    .expect("first disjoint");
                                let (r2, n2) = alloc();
                                let last = bounds.len() - 1;
                                let repl2 = if &text[bounds[last - 1]..bounds[last]] == "R" {
                                    "😀"
                                } else {
                                    "R"
                                };
                                let op2 = single_edit(
                                    other_actor,
                                    n2,
                                    base,
                                    buf,
                                    bounds[last - 1],
                                    bounds[last],
                                    repl2,
                                );
                                let out2 = harness
                                    .apply_op(r2, op2.clone(), &format!("{tag}_rebased"))
                                    .expect("stale disjoint edit rebases");
                                committed_edits.push((r2, op2, out2));
                                let new_len = harness.oracle.live[&buf].len();
                                let delta = repl2.len();
                                undoable_targets.push((
                                    other_actor,
                                    n2,
                                    buf,
                                    new_len - delta,
                                    new_len,
                                ));
                                cov.disjoint_rebased_edit += 1;
                                sync_session(&mut session, &harness.coord, &mut cov);
                            }
                        }

                        // 1: Touching conflict rejection at shared boundary.
                        1 => {
                            let text = harness.oracle.live[&buf].clone();
                            let bounds = char_boundaries(&text);
                            if bounds.len() >= 4 {
                                let base = harness.oracle.revision;
                                let (r1, n1) = alloc();
                                let repl1 = if &text[bounds[0]..bounds[1]] == "T" {
                                    "U"
                                } else {
                                    "T"
                                };
                                harness
                                    .apply_op(
                                        r1,
                                        single_edit(
                                            actor, n1, base, buf, bounds[0], bounds[1], repl1,
                                        ),
                                        &format!("{tag}_win"),
                                    )
                                    .expect("winner");
                                let (r2, n2) = alloc();
                                let err = harness
                                    .apply_op(
                                        r2,
                                        single_edit(
                                            other_actor,
                                            n2,
                                            base,
                                            buf,
                                            bounds[1],
                                            bounds[2],
                                            "TOUCH",
                                        ),
                                        &format!("{tag}_touch_reject"),
                                    )
                                    .expect_err("touching edit rejects");
                                assert_eq!(err, OracleErrorKind::Conflict);
                                cov.touching_conflict_rejected += 1;
                                sync_session(&mut session, &harness.coord, &mut cov);
                            }
                        }

                        // 2: Overlapping conflict rejection.
                        2 => {
                            let text = harness.oracle.live[&buf].clone();
                            let bounds = char_boundaries(&text);
                            if bounds.len() >= 4 {
                                let base = harness.oracle.revision;
                                let (r1, n1) = alloc();
                                let repl1 = if &text[bounds[0]..bounds[2]] == "OV" {
                                    "OW"
                                } else {
                                    "OV"
                                };
                                harness
                                    .apply_op(
                                        r1,
                                        single_edit(
                                            actor, n1, base, buf, bounds[0], bounds[2], repl1,
                                        ),
                                        &format!("{tag}_win"),
                                    )
                                    .expect("winner");
                                let (r2, n2) = alloc();
                                let err = harness
                                    .apply_op(
                                        r2,
                                        single_edit(
                                            other_actor,
                                            n2,
                                            base,
                                            buf,
                                            bounds[1],
                                            bounds[3],
                                            "CLASH",
                                        ),
                                        &format!("{tag}_overlap_reject"),
                                    )
                                    .expect_err("overlapping edit rejects");
                                assert_eq!(err, OracleErrorKind::Conflict);
                                cov.overlapping_conflict_rejected += 1;
                                sync_session(&mut session, &harness.coord, &mut cov);
                            }
                        }

                        // 3: Receipt idempotence + conflicting request reuse + txn_id reuse.
                        3 => {
                            if let Some((old_req, old_op, old_out)) = committed_edits
                                .get((word as usize / 7) % committed_edits.len().max(1))
                                .cloned()
                            {
                                let retry = harness
                                    .apply_op(old_req, old_op.clone(), &format!("{tag}_idempotent"))
                                    .expect("idempotent retry");
                                assert_eq!(retry, old_out);
                                cov.receipt_idempotent_retry += 1;

                                // Conflicting payload with same RequestId.
                                let conflict_op = single_edit(
                                    HUMAN_A,
                                    999_000 + step_idx as u64,
                                    harness.oracle.revision,
                                    buf,
                                    0,
                                    0,
                                    "BAD",
                                );
                                assert_eq!(
                                    harness.apply_op(
                                        old_req,
                                        conflict_op,
                                        &format!("{tag}_req_reuse")
                                    ),
                                    Err(OracleErrorKind::RequestReuse)
                                );
                                cov.conflicting_request_reuse_rejected += 1;

                                // Same TxnId under fresh RequestId.
                                if let Some(receipt) = old_out.receipt {
                                    let (fresh_req, _) = alloc();
                                    let reuse_txn_op = single_edit(
                                        receipt.actor,
                                        receipt.nonce,
                                        harness.oracle.revision,
                                        buf,
                                        0,
                                        0,
                                        "REUSE",
                                    );
                                    assert_eq!(
                                        harness.apply_op(
                                            fresh_req,
                                            reuse_txn_op,
                                            &format!("{tag}_txn_reuse")
                                        ),
                                        Err(OracleErrorKind::TxnReuse)
                                    );
                                    cov.txn_id_reuse_rejected += 1;
                                }
                            }
                        }

                        // 4: Proposal commit (with non-human approval rejection check first).
                        4 => {
                            let text = harness.oracle.live[&buf].clone();
                            let bounds = char_boundaries(&text);
                            if bounds.len() >= 2 {
                                let (req, nonce) = alloc();
                                let base = harness.oracle.revision;
                                let repl = if &text[bounds[0]..bounds[1]] == "P" {
                                    "Q"
                                } else {
                                    "P"
                                };
                                let patches = BTreeMap::from([(
                                    buf,
                                    vec![OracleSplice::new(bounds[0], bounds[1], repl)],
                                )]);
                                harness
                                    .propose(req, AGENT_C, nonce, base, patches.clone(), &tag)
                                    .expect("propose");
                                // Non-human approval attempt fails.
                                harness.grant_actor_cap(AGENT_C, OracleCap::Review);
                                assert_eq!(
                                    harness.approve(req, AGENT_C, &format!("{tag}_agent_approve")),
                                    Err(OracleErrorKind::NotHuman)
                                );
                                harness.revoke_actor_cap(AGENT_C, OracleCap::Review);
                                cov.proposal_non_human_approve_rejected += 1;

                                harness
                                    .approve(req, HUMAN_A, &format!("{tag}_human_approve"))
                                    .expect("human approve");
                                let out = harness
                                    .commit_proposal(req, &format!("{tag}_commit_prop"))
                                    .expect("commit proposal");
                                committed_edits.push((
                                    req,
                                    OracleOp::Edit {
                                        actor: AGENT_C,
                                        nonce,
                                        base,
                                        patches,
                                    },
                                    out,
                                ));
                                cov.proposal_committed += 1;
                                sync_session(&mut session, &harness.coord, &mut cov);
                            }
                        }

                        // 5: Proposal invalidated by intervening edit.
                        5 => {
                            let text = harness.oracle.live[&buf].clone();
                            let bounds = char_boundaries(&text);
                            if bounds.len() >= 2 {
                                let (req, nonce) = alloc();
                                let base = harness.oracle.revision;
                                let patches = BTreeMap::from([(
                                    buf,
                                    vec![OracleSplice::new(bounds[0], bounds[1], "STALE")],
                                )]);
                                harness
                                    .propose(req, AGENT_C, nonce, base, patches, &tag)
                                    .expect("propose");
                                harness
                                    .approve(req, HUMAN_A, &format!("{tag}_approve"))
                                    .expect("approve");
                                let (r2, n2) = alloc();
                                harness
                                    .apply_op(
                                        r2,
                                        single_edit(AUTOMATED_B, n2, base, buf, 0, 0, "!"),
                                        &format!("{tag}_advance"),
                                    )
                                    .expect("intervening edit");
                                assert_eq!(
                                    harness.commit_proposal(req, &format!("{tag}_stale_commit")),
                                    Err(OracleErrorKind::StaleApproval)
                                );
                                cov.proposal_invalidated_by_edit += 1;
                                sync_session(&mut session, &harness.coord, &mut cov);
                            }
                        }

                        // 6: Proposal invalidated by buffer close (on a temporary buffer).
                        6 => {
                            let (open_req, open_nonce) = alloc();
                            let base = harness.oracle.revision;
                            let tmp_buf = harness
                                .apply_op(
                                    open_req,
                                    OracleOp::Open {
                                        actor: HUMAN_A,
                                        nonce: open_nonce,
                                        base,
                                        name: format!("tmp-{step_idx}"),
                                        text: "temp".into(),
                                    },
                                    &format!("{tag}_open_tmp"),
                                )
                                .expect("open tmp")
                                .buffer
                                .expect("buf id");
                            let (prop_req, prop_nonce) = alloc();
                            let rev_open = harness.oracle.revision;
                            harness
                                .propose(
                                    prop_req,
                                    AGENT_C,
                                    prop_nonce,
                                    rev_open,
                                    BTreeMap::from([(
                                        tmp_buf,
                                        vec![OracleSplice::new(0, 4, "gone")],
                                    )]),
                                    &format!("{tag}_propose_tmp"),
                                )
                                .expect("propose tmp");
                            harness
                                .approve(prop_req, HUMAN_A, &format!("{tag}_approve_tmp"))
                                .expect("approve tmp");
                            let (close_req, close_nonce) = alloc();
                            harness
                                .apply_op(
                                    close_req,
                                    OracleOp::Close {
                                        actor: HUMAN_A,
                                        nonce: close_nonce,
                                        base: rev_open,
                                        buffer: tmp_buf,
                                        policy: OracleClosePolicy::RequireClean,
                                    },
                                    &format!("{tag}_close_tmp"),
                                )
                                .expect("close tmp");
                            assert_eq!(
                                harness.commit_proposal(prop_req, &format!("{tag}_closed_commit")),
                                Err(OracleErrorKind::StaleApproval)
                            );
                            cov.proposal_invalidated_by_close += 1;
                            session = None;
                        }

                        // 7: Proposal invalidated by permission revocation (Review or Commit).
                        7 => {
                            let text = harness.oracle.live[&buf].clone();
                            let bounds = char_boundaries(&text);
                            if bounds.len() >= 2 {
                                let (req, nonce) = alloc();
                                let base = harness.oracle.revision;
                                harness
                                    .propose(
                                        req,
                                        AGENT_C,
                                        nonce,
                                        base,
                                        BTreeMap::from([(
                                            buf,
                                            vec![OracleSplice::new(bounds[0], bounds[1], "PERM")],
                                        )]),
                                        &tag,
                                    )
                                    .expect("propose");
                                harness
                                    .approve(req, HUMAN_A, &format!("{tag}_approve"))
                                    .expect("approve");
                                if (word >> 8) & 1 == 0 {
                                    harness.revoke_actor_cap(HUMAN_A, OracleCap::Review);
                                    assert_eq!(
                                        harness.commit_proposal(req, &format!("{tag}_rev_denied")),
                                        Err(OracleErrorKind::PermissionDenied)
                                    );
                                    harness.grant_actor_cap(HUMAN_A, OracleCap::Review);
                                } else {
                                    harness.revoke_actor_cap(AGENT_C, OracleCap::Commit);
                                    assert_eq!(
                                        harness.commit_proposal(req, &format!("{tag}_com_denied")),
                                        Err(OracleErrorKind::PermissionDenied)
                                    );
                                    harness.grant_actor_cap(AGENT_C, OracleCap::Commit);
                                }
                                cov.proposal_invalidated_by_permission_revoke += 1;
                            }
                        }

                        // 8: Selective undo preserving another actor's disjoint edit.
                        8 => {
                            let text = harness.oracle.live[&buf].clone();
                            let bounds = char_boundaries(&text);
                            if bounds.len() >= 6 {
                                let (r1, n1) = alloc();
                                let base1 = harness.oracle.revision;
                                let repl1 = if &text[bounds[0]..bounds[1]] == "U" {
                                    "V"
                                } else {
                                    "U"
                                };
                                harness
                                    .apply_op(
                                        r1,
                                        single_edit(
                                            actor, n1, base1, buf, bounds[0], bounds[1], repl1,
                                        ),
                                        &format!("{tag}_target"),
                                    )
                                    .expect("target edit");
                                // Disjoint edit by other_actor at the far end of the buffer.
                                let (r2, n2) = alloc();
                                let base2 = harness.oracle.revision;
                                let cur = harness.oracle.live[&buf].clone();
                                let cur_bounds = char_boundaries(&cur);
                                let last = cur_bounds.len() - 1;
                                let repl2 = if &cur[cur_bounds[last - 1]..cur_bounds[last]] == "W" {
                                    "K"
                                } else {
                                    "W"
                                };
                                harness
                                    .apply_op(
                                        r2,
                                        single_edit(
                                            other_actor,
                                            n2,
                                            base2,
                                            buf,
                                            cur_bounds[last - 1],
                                            cur_bounds[last],
                                            repl2,
                                        ),
                                        &format!("{tag}_other_disjoint"),
                                    )
                                    .expect("other disjoint");
                                let (ru, nu) = alloc();
                                harness
                                    .apply_op(
                                        ru,
                                        OracleOp::Undo {
                                            actor,
                                            target: (actor, n1),
                                            txn: (actor, nu),
                                        },
                                        &format!("{tag}_undo"),
                                    )
                                    .expect("selective undo succeeds");
                                assert!(harness.oracle.live[&buf].ends_with(repl2));
                                undone_targets.push((actor, n1, buf, 0, bounds[1]));
                                cov.selective_undo_preserved_other_actor += 1;
                                sync_session(&mut session, &harness.coord, &mut cov);
                            }
                        }

                        // 9: Selective undo conflict rejection (other actor edits same/touching span).
                        9 => {
                            let text = harness.oracle.live[&buf].clone();
                            let bounds = char_boundaries(&text);
                            if bounds.len() >= 3 {
                                let (r1, n1) = alloc();
                                let base1 = harness.oracle.revision;
                                let repl1 = "XY";
                                harness
                                    .apply_op(
                                        r1,
                                        single_edit(
                                            actor, n1, base1, buf, bounds[0], bounds[1], repl1,
                                        ),
                                        &format!("{tag}_target"),
                                    )
                                    .expect("target");
                                let (r2, n2) = alloc();
                                let base2 = harness.oracle.revision;
                                // Touch/overlap at [0..1] in the modified buffer.
                                harness
                                    .apply_op(
                                        r2,
                                        single_edit(other_actor, n2, base2, buf, 0, 1, "Q"),
                                        &format!("{tag}_clash"),
                                    )
                                    .expect("clash");
                                let preserved = harness.oracle.live[&buf].clone();
                                let (ru, nu) = alloc();
                                assert_eq!(
                                    harness.apply_op(
                                        ru,
                                        OracleOp::Undo {
                                            actor,
                                            target: (actor, n1),
                                            txn: (actor, nu),
                                        },
                                        &format!("{tag}_undo_conflict")
                                    ),
                                    Err(OracleErrorKind::Conflict)
                                );
                                assert_eq!(harness.oracle.live[&buf], preserved);
                                cov.selective_undo_conflict_rejected += 1;
                                sync_session(&mut session, &harness.coord, &mut cov);
                            }
                        }

                        // 10: Selective redo preserving another actor's disjoint edit.
                        10 => {
                            let text = harness.oracle.live[&buf].clone();
                            let bounds = char_boundaries(&text);
                            if bounds.len() >= 6 {
                                let (r1, n1) = alloc();
                                let base1 = harness.oracle.revision;
                                let repl1 = "RR";
                                harness
                                    .apply_op(
                                        r1,
                                        single_edit(
                                            actor, n1, base1, buf, bounds[0], bounds[1], repl1,
                                        ),
                                        &format!("{tag}_orig"),
                                    )
                                    .expect("orig");
                                let (ru, nu) = alloc();
                                harness
                                    .apply_op(
                                        ru,
                                        OracleOp::Undo {
                                            actor,
                                            target: (actor, n1),
                                            txn: (actor, nu),
                                        },
                                        &format!("{tag}_undo"),
                                    )
                                    .expect("undo");
                                let (r2, n2) = alloc();
                                let base2 = harness.oracle.revision;
                                let cur = harness.oracle.live[&buf].clone();
                                let cur_bounds = char_boundaries(&cur);
                                let last = cur_bounds.len() - 1;
                                let suffix = if &cur[cur_bounds[last - 1]..cur_bounds[last]] == "S"
                                {
                                    "T"
                                } else {
                                    "S"
                                };
                                harness
                                    .apply_op(
                                        r2,
                                        single_edit(
                                            other_actor,
                                            n2,
                                            base2,
                                            buf,
                                            cur_bounds[last - 1],
                                            cur_bounds[last],
                                            suffix,
                                        ),
                                        &format!("{tag}_disjoint_before_redo"),
                                    )
                                    .expect("disjoint before redo");
                                let (rr, nr) = alloc();
                                harness
                                    .apply_op(
                                        rr,
                                        OracleOp::Redo {
                                            actor,
                                            target: (actor, n1),
                                            txn: (actor, nr),
                                        },
                                        &format!("{tag}_redo"),
                                    )
                                    .expect("redo succeeds");
                                assert!(harness.oracle.live[&buf].starts_with(repl1));
                                assert!(harness.oracle.live[&buf].ends_with(suffix));
                                cov.selective_redo_preserved_other_actor += 1;
                                sync_session(&mut session, &harness.coord, &mut cov);
                            }
                        }

                        // 11: Selective redo conflict rejection.
                        11 => {
                            let text = harness.oracle.live[&buf].clone();
                            let bounds = char_boundaries(&text);
                            if bounds.len() >= 3 {
                                let (r1, n1) = alloc();
                                let base1 = harness.oracle.revision;
                                let repl1 = if &text[bounds[0]..bounds[1]] == "M" {
                                    "N"
                                } else {
                                    "M"
                                };
                                harness
                                    .apply_op(
                                        r1,
                                        single_edit(
                                            actor, n1, base1, buf, bounds[0], bounds[1], repl1,
                                        ),
                                        &format!("{tag}_orig"),
                                    )
                                    .expect("orig");
                                let (ru, nu) = alloc();
                                harness
                                    .apply_op(
                                        ru,
                                        OracleOp::Undo {
                                            actor,
                                            target: (actor, n1),
                                            txn: (actor, nu),
                                        },
                                        &format!("{tag}_undo"),
                                    )
                                    .expect("undo");
                                let (r2, n2) = alloc();
                                let base2 = harness.oracle.revision;
                                harness
                                    .apply_op(
                                        r2,
                                        single_edit(
                                            other_actor,
                                            n2,
                                            base2,
                                            buf,
                                            bounds[0],
                                            bounds[1],
                                            "BLOCK",
                                        ),
                                        &format!("{tag}_block_redo"),
                                    )
                                    .expect("block redo");
                                let preserved = harness.oracle.live[&buf].clone();
                                let (rr, nr) = alloc();
                                assert_eq!(
                                    harness.apply_op(
                                        rr,
                                        OracleOp::Redo {
                                            actor,
                                            target: (actor, n1),
                                            txn: (actor, nr),
                                        },
                                        &format!("{tag}_redo_conflict")
                                    ),
                                    Err(OracleErrorKind::Conflict)
                                );
                                assert_eq!(harness.oracle.live[&buf], preserved);
                                cov.selective_redo_conflict_rejected += 1;
                                sync_session(&mut session, &harness.coord, &mut cov);
                            }
                        }

                        // 12: Compaction + checkpoint restore + delivery catch-up + expired rejection.
                        12 => {
                            let tip = harness.oracle.revision;
                            let cur_floor = harness.oracle.floor;
                            if tip > cur_floor {
                                let new_floor = if tip - cur_floor >= 2 {
                                    cur_floor + 1 + (word % (tip - cur_floor))
                                } else {
                                    tip
                                };
                                harness
                                    .compact(new_floor, &format!("{tag}_compact"))
                                    .expect("compact");
                                harness.checkpoint_restore(&format!("{tag}_restore"));
                                cov.compaction_checkpoint_restore += 1;
                                session = None;

                                // Check delivery catch-up both above and below delivery floor.
                                let dfloor = harness.coord.workspace().delivery().floor().raw();
                                if let Ok(CatchUp::Events(evs)) = harness
                                    .coord
                                    .workspace()
                                    .delivery()
                                    .catch_up(Revision::new(dfloor))
                                {
                                    let (cat, r) = delivery::replay(
                                        harness.coord.workspace().delivery().base_snapshot(),
                                        &evs,
                                    )
                                    .expect("replay");
                                    assert_eq!(catalog_to_map(&cat), harness.oracle.live);
                                    assert_eq!(r.raw(), harness.oracle.revision);
                                    cov.delivery_catch_up_events += 1;
                                }
                                if dfloor > 0 {
                                    match harness
                                        .coord
                                        .workspace()
                                        .delivery()
                                        .catch_up(Revision::new(dfloor - 1))
                                        .expect("expired catch_up")
                                    {
                                        CatchUp::Snapshot(s) => {
                                            assert_eq!(
                                                catalog_to_map(s.catalog()),
                                                harness.oracle.live
                                            );
                                            cov.delivery_catch_up_snapshot += 1;
                                        }
                                        CatchUp::Events(_) => panic!("expected Snapshot"),
                                    }
                                }

                                // Stale edit with base < new_floor must reject atomically with BaseExpired.
                                let (re, ne) = alloc();
                                let err = harness
                                    .apply_op(
                                        re,
                                        single_edit(actor, ne, new_floor - 1, buf, 0, 0, "OLD"),
                                        &format!("{tag}_expired_edit"),
                                    )
                                    .expect_err("expired base rejects");
                                assert_eq!(err, OracleErrorKind::BaseExpired);
                                cov.compaction_expired_rejected += 1;
                            }
                        }

                        // 13: Historical text restoration edit via editor::edit::restore::restore_many.
                        _ => {
                            let snap = harness.coord.snapshot();
                            let target_str = format!("restored-{step_idx}-界");
                            let (req, nonce) = alloc();
                            let outcome = restore::restore_many(
                                &snap,
                                [(BufferId::new(buf), Text::from_str(&target_str))],
                                ActorId::new(actor),
                                Nonce::new(nonce),
                            )
                            .expect("prepare restore");
                            if let RestoreOutcome::Transaction(txn) = outcome {
                                let patches: OraclePatches = txn
                                    .patches()
                                    .iter()
                                    .map(|(id, p)| {
                                        (
                                            id.raw(),
                                            p.splices()
                                                .iter()
                                                .map(|s| {
                                                    OracleSplice::new(
                                                        s.span().start().as_usize(),
                                                        s.span().end().as_usize(),
                                                        s.replacement().as_str(),
                                                    )
                                                })
                                                .collect(),
                                        )
                                    })
                                    .collect();
                                let op = OracleOp::Edit {
                                    actor,
                                    nonce,
                                    base: snap.revision().raw(),
                                    patches,
                                };
                                let out = harness
                                    .apply_op(req, op.clone(), &format!("{tag}_restore_edit"))
                                    .expect("restore edit commits");
                                assert_eq!(harness.oracle.live[&buf], target_str);
                                committed_edits.push((req, op, out));
                                cov.restore_edit_committed += 1;
                                sync_session(&mut session, &harness.coord, &mut cov);
                            }
                        }
                    }

                    // Keep undoable/undone bookkeeping bounded.
                    if undoable_targets.len() > 8 {
                        undoable_targets.remove(0);
                    }
                    if undone_targets.len() > 8 {
                        undone_targets.remove(0);
                    }
                }
            },
        );

        cov.assert_all_nonzero();
    }

    // =========================================================================
    // Cross-Check: ConcurrentCoordinator Multi-Actor Linearizability
    // =========================================================================

    fn replay_concurrent_op_on_oracle(
        oracle: &mut OracleState,
        op: &ConcurrentOp,
        response: &ConcurrentResponse,
    ) {
        let to_oracle_patches = |txn: &Transaction| -> OraclePatches {
            txn.patches()
                .iter()
                .map(|(id, p)| {
                    (
                        id.raw(),
                        p.splices()
                            .iter()
                            .map(|s| {
                                OracleSplice::new(
                                    s.span().start().as_usize(),
                                    s.span().end().as_usize(),
                                    s.replacement().as_str(),
                                )
                            })
                            .collect(),
                    )
                })
                .collect()
        };

        match op {
            ConcurrentOp::Apply { request, operation } => {
                let oracle_op = match operation {
                    Operation::Open { intent, name, text } => OracleOp::Open {
                        actor: intent.actor().raw(),
                        nonce: intent.nonce().raw(),
                        base: intent.base().raw(),
                        name: name.clone(),
                        text: text.as_str().to_owned(),
                    },
                    Operation::Close {
                        intent,
                        buffer,
                        policy,
                    } => OracleOp::Close {
                        actor: intent.actor().raw(),
                        nonce: intent.nonce().raw(),
                        base: intent.base().raw(),
                        buffer: buffer.raw(),
                        policy: match policy {
                            ClosePolicy::RequireClean => OracleClosePolicy::RequireClean,
                            ClosePolicy::Discard => OracleClosePolicy::Discard,
                        },
                    },
                    Operation::Edit(txn) => OracleOp::Edit {
                        actor: txn.intent().actor().raw(),
                        nonce: txn.intent().nonce().raw(),
                        base: txn.intent().base().raw(),
                        patches: to_oracle_patches(txn),
                    },
                    Operation::Undo {
                        actor,
                        target,
                        transaction,
                    } => OracleOp::Undo {
                        actor: actor.raw(),
                        target: (target.actor().raw(), target.nonce().raw()),
                        txn: (transaction.actor().raw(), transaction.nonce().raw()),
                    },
                    Operation::Redo {
                        actor,
                        target,
                        transaction,
                    } => OracleOp::Redo {
                        actor: actor.raw(),
                        target: (target.actor().raw(), target.nonce().raw()),
                        txn: (transaction.actor().raw(), transaction.nonce().raw()),
                    },
                    Operation::Cut { .. } => panic!("Cut not submitted in concurrent schedule"),
                };
                let oracle_res = oracle.apply(request.raw(), oracle_op);
                let ConcurrentResponse::Outcome(actual_res) = response else {
                    panic!("expected Outcome response");
                };
                match (oracle_res, actual_res) {
                    (Ok(exp), Ok(act)) => assert_eq!(OracleOutcome::from_prod(act), exp),
                    (Err(exp_err), Err(ConcurrentError::Coordinator(act_err))) => {
                        assert_eq!(classify_coordinator_error(act_err), exp_err);
                    }
                    other => panic!("concurrent apply mismatch: {other:?}"),
                }
            }
            ConcurrentOp::Propose(proposal) => {
                let txn = proposal.transaction();
                let oracle_res = oracle.propose(
                    proposal.request().raw(),
                    txn.intent().actor().raw(),
                    txn.intent().nonce().raw(),
                    txn.intent().base().raw(),
                    to_oracle_patches(txn),
                );
                let ConcurrentResponse::Unit(actual_res) = response else {
                    panic!("expected Unit response");
                };
                assert_eq!(oracle_res.is_ok(), actual_res.is_ok());
            }
            ConcurrentOp::Approve { request, actor } => {
                let oracle_res = oracle.approve(request.raw(), actor.raw());
                let ConcurrentResponse::Unit(actual_res) = response else {
                    panic!("expected Unit response");
                };
                assert_eq!(oracle_res.is_ok(), actual_res.is_ok());
            }
            ConcurrentOp::CommitProposal { request } => {
                let oracle_res = oracle.commit_proposal(request.raw());
                let ConcurrentResponse::Outcome(actual_res) = response else {
                    panic!("expected Outcome response");
                };
                match (oracle_res, actual_res) {
                    (Ok(exp), Ok(act)) => assert_eq!(OracleOutcome::from_prod(act), exp),
                    (Err(exp_err), Err(ConcurrentError::Coordinator(act_err))) => {
                        assert_eq!(classify_coordinator_error(act_err), exp_err);
                    }
                    other => panic!("concurrent commit_proposal mismatch: {other:?}"),
                }
            }
            ConcurrentOp::Compact { floor } => {
                let oracle_res = oracle.compact(floor.raw());
                let ConcurrentResponse::Unit(actual_res) = response else {
                    panic!("expected Unit response");
                };
                assert_eq!(oracle_res.is_ok(), actual_res.is_ok());
            }
            ConcurrentOp::UpdateAccess(change) => {
                let cap_of = |c: Capability| match c {
                    Capability::Read => OracleCap::Read,
                    Capability::Propose => OracleCap::Propose,
                    Capability::Review => OracleCap::Review,
                    Capability::Commit => OracleCap::Commit,
                };
                match *change {
                    AccessChange::GrantDefault(c) => oracle.grant_default(cap_of(c)),
                    AccessChange::RevokeDefault(c) => oracle.revoke_default(cap_of(c)),
                    AccessChange::GrantActor(a, c) => oracle.grant_actor(a.raw(), cap_of(c)),
                    AccessChange::RevokeActor(a, c) => oracle.revoke_actor_cap(a.raw(), cap_of(c)),
                    AccessChange::GrantBuffer(a, b, c) => {
                        oracle.grant_buffer(a.raw(), b.raw(), cap_of(c));
                    }
                    AccessChange::RevokeBuffer(a, b, c) => {
                        oracle.revoke_buffer(a.raw(), b.raw(), cap_of(c));
                    }
                }
                assert_eq!(response, &ConcurrentResponse::Unit(Ok(())));
            }
            ConcurrentOp::ReadBuffers { actor, buffers } => {
                let auth = oracle.authorize(
                    actor.raw(),
                    OracleCap::Read,
                    buffers.iter().map(|b| b.raw()),
                );
                let ConcurrentResponse::BuffersRead(actual_res) = response else {
                    panic!("expected BuffersRead");
                };
                match (auth, actual_res) {
                    (Ok(()), Ok(read)) => {
                        assert_eq!(read.revision().raw(), oracle.revision);
                        for &b in buffers {
                            assert_eq!(
                                read.get(b).expect("buffer in read").as_str(),
                                oracle.live[&b.raw()]
                            );
                        }
                    }
                    (Err(_), Err(_)) => {}
                    other => panic!("concurrent read_buffers mismatch: {other:?}"),
                }
            }
            ConcurrentOp::Snapshot => {
                let ConcurrentResponse::Snapshot(Ok(view)) = response else {
                    panic!("expected Snapshot Ok");
                };
                assert_eq!(view.revision().raw(), oracle.revision);
                assert_eq!(catalog_to_map(view.catalog()), oracle.live);
            }
            ConcurrentOp::CatchUp { since } => {
                let ConcurrentResponse::CatchUp(actual_res) = response else {
                    panic!("expected CatchUp");
                };
                if since.raw() > oracle.revision {
                    assert!(actual_res.is_err());
                } else {
                    assert!(actual_res.is_ok());
                }
            }
            ConcurrentOp::JournalRange { start, end } => {
                let ConcurrentResponse::JournalRange(Ok(receipts)) = response else {
                    panic!("expected JournalRange Ok");
                };
                let exp: Vec<_> = oracle.journal[start.raw() as usize..end.raw() as usize].to_vec();
                let act: Vec<_> = receipts
                    .iter()
                    .copied()
                    .map(OracleReceipt::from_prod)
                    .collect();
                assert_eq!(act, exp);
            }
            _ => {}
        }
    }

    #[test]
    fn concurrent_coordinator_linearizability_crosscheck() {
        // Schedule 1: 3 concurrent actors racing on disjoint edits + conflicting pair +
        // duplicate retries + multi-buffer consistent reads.
        let mut init = PairedHarness::new_three_actor(true);
        init.apply_op(
            1,
            OracleOp::Open {
                actor: HUMAN_A,
                nonce: 1,
                base: 0,
                name: "s0".into(),
                text: "0123456789".into(),
            },
            "conc_open_0",
        )
        .expect("open 0");
        init.apply_op(
            2,
            OracleOp::Open {
                actor: AUTOMATED_B,
                nonce: 2,
                base: 1,
                name: "s1".into(),
                text: "abcdefghij".into(),
            },
            "conc_open_1",
        )
        .expect("open 1");

        let initial_cp = init.coord.checkpoint();
        let initial_oracle = init.oracle.clone();
        let base_texts = init.oracle.live.clone();
        let conc = ConcurrentCoordinator::restore(&initial_cp).expect("restore conc");
        let barrier = Arc::new(Barrier::new(3));

        // Build transactions anchored at base revision 2.
        let h_disjoint = build_txn_from_oracle(
            HUMAN_A,
            10,
            2,
            &BTreeMap::from([(0, vec![OracleSplice::new(0, 1, "H")])]),
            &base_texts,
        );
        let b_disjoint = build_txn_from_oracle(
            AUTOMATED_B,
            20,
            2,
            &BTreeMap::from([(0, vec![OracleSplice::new(4, 5, "B")])]),
            &base_texts,
        );
        let c_disjoint = build_txn_from_oracle(
            AGENT_C,
            30,
            2,
            &BTreeMap::from([(0, vec![OracleSplice::new(8, 9, "C")])]),
            &base_texts,
        );
        // Conflicting race on buffer 1 [2..5] vs [4..7] (overlap at byte 4..5).
        let h_race = build_txn_from_oracle(
            HUMAN_A,
            11,
            2,
            &BTreeMap::from([(1, vec![OracleSplice::new(2, 5, "HR")])]),
            &base_texts,
        );
        let b_race = build_txn_from_oracle(
            AUTOMATED_B,
            21,
            2,
            &BTreeMap::from([(1, vec![OracleSplice::new(4, 7, "BR")])]),
            &base_texts,
        );
        // Conflicting RequestId reuse racing with exact retry of RequestId 10.
        let bad_reuse = build_txn_from_oracle(
            HUMAN_A,
            99,
            2,
            &BTreeMap::from([(0, vec![OracleSplice::new(0, 1, "BAD")])]),
            &base_texts,
        );

        let reuse_barrier = Arc::new(Barrier::new(2));

        let (race_h, race_b) = thread::scope(|scope| {
            let c1 = conc.clone();
            let b1 = Arc::clone(&barrier);
            let rb1 = Arc::clone(&reuse_barrier);
            let h_disjoint_clone = h_disjoint.clone();
            let t1 = scope.spawn(move || {
                b1.wait();
                let o1 = c1.apply(RequestId::new(10), Operation::Edit(h_disjoint.clone()));
                // Release t2's conflicting reuse of RequestId(10) only after h_disjoint
                // has claimed RequestId(10), while still racing t1's retry against t2's reuse.
                rb1.wait();
                let o1 = o1.expect("h disjoint");
                let o1_retry = c1
                    .apply(RequestId::new(10), Operation::Edit(h_disjoint_clone))
                    .expect("h retry");
                assert_eq!(o1, o1_retry);
                let _ =
                    c1.read_buffers(ActorId::new(HUMAN_A), &[BufferId::new(0), BufferId::new(1)]);
                c1.apply(RequestId::new(11), Operation::Edit(h_race))
            });

            let c2 = conc.clone();
            let b2 = Arc::clone(&barrier);
            let rb2 = Arc::clone(&reuse_barrier);
            let t2 = scope.spawn(move || {
                b2.wait();
                let b_res = c2.apply(RequestId::new(20), Operation::Edit(b_disjoint));
                rb2.wait();
                b_res.expect("b disjoint");
                let reuse_err = c2.apply(RequestId::new(10), Operation::Edit(bad_reuse));
                assert_eq!(
                    reuse_err,
                    Err(ConcurrentError::Coordinator(
                        CoordinatorError::RequestReuse(RequestId::new(10))
                    ))
                );
                let _ = c2.read_buffers(
                    ActorId::new(AUTOMATED_B),
                    &[BufferId::new(0), BufferId::new(1)],
                );
                c2.apply(RequestId::new(21), Operation::Edit(b_race))
            });

            let c3 = conc.clone();
            let b3 = Arc::clone(&barrier);
            let t3 = scope.spawn(move || {
                b3.wait();
                c3.apply(RequestId::new(30), Operation::Edit(c_disjoint))
                    .expect("c disjoint");
                let _ = c3.snapshot();
                let _ = c3.catch_up(Revision::new(2));
                let _ = c3.journal_range(Revision::ZERO, Revision::new(2));
            });

            let rh = t1.join().expect("t1");
            let rb = t2.join().expect("t2");
            t3.join().expect("t3");
            (rh, rb)
        });

        // Strictly one of the overlapping race edits succeeded; the other rejected with Conflict!
        assert_ne!(race_h.is_ok(), race_b.is_ok());
        // All 3 disjoint edits on buffer 0 MUST be present in order-independent final text!
        let final_snap = conc.snapshot().expect("final snap");
        assert_eq!(
            final_snap
                .catalog()
                .get(BufferId::new(0))
                .expect("buf 0")
                .as_str(),
            "H123B567C9"
        );

        let history = conc.recorded_history().expect("history");
        let order = find_linearization(&initial_cp, &history)
            .expect("concurrent multi-actor history MUST be linearizable");
        assert_eq!(order.len(), history.len());

        // Replay the witnessing linearization on BOTH sequential Coordinator and independent OracleState!
        let mut seq_coord = Coordinator::restore(&initial_cp).expect("restore seq");
        let mut seq_oracle = initial_oracle;
        for &idx in &order {
            let entry = &history[idx];
            let seq_resp = execute_sequential(&mut seq_coord, &entry.operation);
            assert_eq!(
                seq_resp, entry.response,
                "sequential mismatch at entry {idx}"
            );
            replay_concurrent_op_on_oracle(&mut seq_oracle, &entry.operation, &entry.response);
        }

        let known_actors = [HUMAN_A, AUTOMATED_B, AGENT_C];
        let known_reqs = [1, 2, 10, 11, 20, 21, 30];
        let known_txns = [
            (HUMAN_A, 1),
            (AUTOMATED_B, 2),
            (HUMAN_A, 10),
            (HUMAN_A, 11),
            (AUTOMATED_B, 20),
            (AUTOMATED_B, 21),
            (AGENT_C, 30),
        ];
        let conc_coord =
            Coordinator::restore(&conc.checkpoint().expect("conc cp")).expect("restore conc cp");
        let obs_oracle = observe_oracle(&seq_oracle, &known_reqs, &known_txns);
        let obs_seq = observe_coordinator(&seq_coord, &known_actors, &known_reqs, &known_txns);
        let obs_conc = observe_coordinator(&conc_coord, &known_actors, &known_reqs, &known_txns);
        assert_no_drift(&obs_oracle, &obs_seq, "conc_schedule1_oracle_vs_seq");
        assert_no_drift(&obs_seq, &obs_conc, "conc_schedule1_seq_vs_conc");

        // Schedule 2: Concurrent proposal workflow, selective undo, compaction, and reads.
        let conc2_init_cp = conc_coord.checkpoint();
        let conc2_init_oracle = seq_oracle.clone();
        let cur_rev = seq_oracle.revision;
        let cur_texts = seq_oracle.live.clone();
        let conc2 = ConcurrentCoordinator::restore(&conc2_init_cp).expect("restore conc2");
        let barrier2 = Arc::new(Barrier::new(3));

        let agent_prop = Proposal::new(
            RequestId::new(40),
            build_txn_from_oracle(
                AGENT_C,
                40,
                cur_rev,
                &BTreeMap::from([(0, vec![OracleSplice::new(2, 3, "P")])]),
                &cur_texts,
            ),
        );

        thread::scope(|scope| {
            let c1 = conc2.clone();
            let b1 = Arc::clone(&barrier2);
            let t1 = scope.spawn(move || {
                b1.wait();
                let _ = c1.propose(agent_prop);
                let _ = c1.approve(RequestId::new(40), ActorId::new(HUMAN_A));
                let _ = c1.commit_proposal(RequestId::new(40));
            });

            let c2 = conc2.clone();
            let b2 = Arc::clone(&barrier2);
            let t2 = scope.spawn(move || {
                b2.wait();
                let _ = c2.apply(
                    RequestId::new(41),
                    Operation::Undo {
                        actor: ActorId::new(AUTOMATED_B),
                        target: TxnId::new(ActorId::new(AUTOMATED_B), Nonce::new(20)),
                        transaction: TxnId::new(ActorId::new(AUTOMATED_B), Nonce::new(41)),
                    },
                );
                let _ = c2.compact(Revision::new(cur_rev));
            });

            let c3 = conc2.clone();
            let b3 = Arc::clone(&barrier2);
            let t3 = scope.spawn(move || {
                b3.wait();
                let _ =
                    c3.read_buffers(ActorId::new(AGENT_C), &[BufferId::new(0), BufferId::new(1)]);
                let _ = c3.catch_up(Revision::new(2));
                let _ = c3.snapshot();
            });

            t1.join().expect("t1");
            t2.join().expect("t2");
            t3.join().expect("t3");
        });

        let history2 = conc2.recorded_history().expect("history2");
        let order2 = find_linearization(&conc2_init_cp, &history2)
            .expect("concurrent schedule 2 MUST be linearizable");
        let mut seq_coord2 = Coordinator::restore(&conc2_init_cp).expect("restore seq2");
        let mut seq_oracle2 = conc2_init_oracle;
        for &idx in &order2 {
            let entry = &history2[idx];
            let seq_resp = execute_sequential(&mut seq_coord2, &entry.operation);
            assert_eq!(seq_resp, entry.response);
            replay_concurrent_op_on_oracle(&mut seq_oracle2, &entry.operation, &entry.response);
        }
        let known_reqs2 = [1, 2, 10, 11, 20, 21, 30, 40, 41];
        let known_txns2 = [
            (HUMAN_A, 1),
            (AUTOMATED_B, 2),
            (HUMAN_A, 10),
            (HUMAN_A, 11),
            (AUTOMATED_B, 20),
            (AUTOMATED_B, 21),
            (AGENT_C, 30),
            (AGENT_C, 40),
            (AUTOMATED_B, 41),
        ];
        let conc_coord2 =
            Coordinator::restore(&conc2.checkpoint().expect("cp2")).expect("restore cp2");
        let obs_oracle2 = observe_oracle(&seq_oracle2, &known_reqs2, &known_txns2);
        let obs_seq2 = observe_coordinator(&seq_coord2, &known_actors, &known_reqs2, &known_txns2);
        let obs_conc2 =
            observe_coordinator(&conc_coord2, &known_actors, &known_reqs2, &known_txns2);
        assert_no_drift(&obs_oracle2, &obs_seq2, "conc_schedule2_oracle_vs_seq");
        assert_no_drift(&obs_seq2, &obs_conc2, "conc_schedule2_seq_vs_conc");
    }

    // =========================================================================
    // Negative Self-Check: Prove Comparator & Linearizability Detector Fail
    // on Injected Text, Receipt, Compaction, and Concurrent Divergences
    // =========================================================================

    #[test]
    fn negative_self_check_detects_injected_divergences() {
        let mut harness = PairedHarness::new_three_actor(true);
        harness
            .apply_op(
                1,
                OracleOp::Open {
                    actor: HUMAN_A,
                    nonce: 1,
                    base: 0,
                    name: "self-check".into(),
                    text: "hello".into(),
                },
                "sc_open",
            )
            .expect("open");
        harness
            .apply_op(2, single_edit(HUMAN_A, 2, 1, 0, 0, 5, "world"), "sc_edit")
            .expect("edit");
        harness.compact(1, "sc_compact").expect("compact");

        let baseline = observe_oracle(
            &harness.oracle,
            &harness.known_requests,
            &harness.known_txns,
        );
        let matching = observe_coordinator(
            &harness.coord,
            &harness.known_actors,
            &harness.known_requests,
            &harness.known_txns,
        );
        assert_eq!(compare_observations(&baseline, &matching), Ok(()));

        // 1. Injected text divergence MUST fail.
        let mut bad_text = matching.clone();
        bad_text.live_buffers.insert(0, "corrupted".into());
        let text_drifts =
            compare_observations(&baseline, &bad_text).expect_err("text drift must fail");
        assert!(text_drifts
            .iter()
            .any(|d| matches!(d, StateDrift::LiveBufferText { buffer: 0, .. })));
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                assert_no_drift(&baseline, &bad_text, "injected_text")
            }))
            .is_err(),
            "assert_no_drift must panic on text divergence"
        );

        // 2. Injected receipt divergence (journal + request lookup + txn lookup) MUST fail.
        let mut bad_receipt = matching.clone();
        bad_receipt.journal[1].committed += 1;
        bad_receipt
            .receipt_by_request
            .get_mut(&2)
            .expect("req 2")
            .nonce += 99;
        bad_receipt
            .receipt_by_txn
            .get_mut(&(HUMAN_A, 2))
            .expect("txn 2")
            .previous += 5;
        let receipt_drifts =
            compare_observations(&baseline, &bad_receipt).expect_err("receipt drift must fail");
        assert!(receipt_drifts
            .iter()
            .any(|d| matches!(d, StateDrift::Journal { .. })));
        assert!(receipt_drifts
            .iter()
            .any(|d| matches!(d, StateDrift::ReceiptByRequest { .. })));
        assert!(receipt_drifts
            .iter()
            .any(|d| matches!(d, StateDrift::ReceiptByTxn { .. })));
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                assert_no_drift(&baseline, &bad_receipt, "injected_receipt")
            }))
            .is_err(),
            "assert_no_drift must panic on receipt divergence"
        );

        // 3. Injected compaction divergence (floor + history replay + delivery replay + expiry flag) MUST fail.
        let mut bad_compaction = matching.clone();
        bad_compaction.floor = 0;
        bad_compaction
            .history_replay_catalog
            .insert(0, "stale".into());
        bad_compaction
            .delivery_replay_catalog
            .insert(0, "stale-delivery".into());
        bad_compaction.compaction_floor_rejects_expired = false;
        let comp_drifts = compare_observations(&baseline, &bad_compaction)
            .expect_err("compaction drift must fail");
        assert!(comp_drifts
            .iter()
            .any(|d| matches!(d, StateDrift::Floor { .. })));
        assert!(comp_drifts
            .iter()
            .any(|d| matches!(d, StateDrift::HistoryReplayCatalog { .. })));
        assert!(comp_drifts
            .iter()
            .any(|d| matches!(d, StateDrift::DeliveryReplayCatalog { .. })));
        assert!(comp_drifts
            .iter()
            .any(|d| matches!(d, StateDrift::CompactionExpiryRejection { .. })));
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                assert_no_drift(&baseline, &bad_compaction, "injected_compaction")
            }))
            .is_err(),
            "assert_no_drift must panic on compaction divergence"
        );

        // 4. Injected revision / tombstone / actor divergence MUST fail.
        let mut bad_meta = matching.clone();
        bad_meta.revision += 1;
        bad_meta.tombstones.insert(99);
        bad_meta.active_actors.remove(&AGENT_C);
        let meta_drifts =
            compare_observations(&baseline, &bad_meta).expect_err("meta drift must fail");
        assert!(meta_drifts
            .iter()
            .any(|d| matches!(d, StateDrift::Revision { .. })));
        assert!(meta_drifts
            .iter()
            .any(|d| matches!(d, StateDrift::Tombstones { .. })));
        assert!(meta_drifts
            .iter()
            .any(|d| matches!(d, StateDrift::ActiveActors { .. })));

        // 5. Injected concurrent history corruption MUST fail linearizability check!
        let init_cp = harness.coord.checkpoint();
        let conc = ConcurrentCoordinator::restore(&init_cp).expect("restore");
        let snap = conc.snapshot().expect("snap");
        assert_eq!(snap.revision(), Revision::new(2));
        let mut history = conc.recorded_history().expect("history");
        assert_eq!(history.len(), 1);
        let mut tampered_catalog = BufferCatalog::new();
        tampered_catalog
            .register(BufferId::new(0), Text::from_str("TAMPERED"))
            .expect("reg");
        let fake_snap =
            crate::editor::workspace::snapshots::VersionedWorkspace::new(tampered_catalog)
                .capture();
        history[0].response = ConcurrentResponse::Snapshot(Ok(SnapshotView::from(&fake_snap)));
        assert_eq!(
            find_linearization(&init_cp, &history),
            None,
            "linearizability checker must reject tampered concurrent response"
        );
    }
}
