//! Thread-safe, linearizable concurrent access over [`Coordinator`].
//!
//! # Laws
//!
//! * **Linearizability:** every concurrent operation executes at a single atomic
//!   linearization point protected by the coordinator's read-write lock.
//!   Invocations sample a monotonic atomic sequence counter (`invoked_at`)
//!   before acquiring the state lock, and completions sample the same counter
//!   (`completed_at`) after releasing the state lock. Consequently, every
//!   recorded concurrent history admits a sequential execution of
//!   [`Coordinator`] that produces identical responses and strictly respects
//!   real-time happens-before order (`a.completed_at < b.invoked_at` implies
//!   `a` precedes `b`).
//! * **Consistent reads:** authorized buffer reads and snapshots capture a
//!   single atomic `(revision, catalog)` pair under a shared read lock and check
//!   active actor registration and read capabilities against that exact state.
//!   Readers never observe torn multi-buffer states, and inactive or
//!   unauthorized actors fail cleanly without leaking buffer contents.
//! * **Idempotent retries:** duplicate [`RequestId`] retries racing across
//!   threads serialize through the exclusive write lock, so exact retries
//!   preserve idempotency and return identical receipts while conflicting reuse
//!   is rejected atomically without mutating workspace state.
//! * **Deadlock and poison safety:** lock acquisition follows a strict two-level
//!   hierarchy (coordinator state lock, then history log lock) and never invokes
//!   caller callbacks while holding any lock. RAII guards prevent lock leaks
//!   across errors or panics, and poisoned locks return
//!   [`ConcurrentError::LockPoisoned`] rather than panicking.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use super::{
    access::{AccessError, AccessPolicy, Capability},
    checkpoints::{CheckpointError, CheckpointOutcome},
    coordinator::{Coordinator, CoordinatorCheckpoint, CoordinatorError, Operation},
    delivery::{CatchUp, DeliveryError},
    journal::JournalError,
    proposal::{Proposal, RequestId},
    receipt::CommitReceipt,
    registry::{ActorRegistry, RegistryError},
    role::ActorRole,
};
use crate::editor::{
    edit::{
        ids::{ActorId, BufferId, TxnId},
        transaction::Transaction,
    },
    text::utf8::Text,
    workspace::{
        catalog::BufferCatalog,
        registers::{RegisterEntry, RegisterName},
        snapshots::WorkspaceSnapshot,
        version::Revision,
    },
};

/// An error returned by concurrent coordinator operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConcurrentError {
    /// An internal synchronization primitive was poisoned by a prior panic.
    LockPoisoned,
    /// The underlying coordinator rejected the operation.
    Coordinator(CoordinatorError),
    /// Access control rejected a read or query.
    Access(AccessError),
    /// Actor registry mutation failed.
    Registry(RegistryError),
    /// Commit journal query failed.
    Journal(JournalError),
    /// Delivery log catch-up query failed.
    Delivery(DeliveryError),
    /// Workspace checkpoint query failed.
    Checkpoint(CheckpointError),
}

impl fmt::Display for ConcurrentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LockPoisoned => f.write_str("concurrent coordinator lock poisoned"),
            Self::Coordinator(error) => write!(f, "coordinator operation failed: {error:?}"),
            Self::Access(error) => error.fmt(f),
            Self::Registry(error) => error.fmt(f),
            Self::Journal(error) => error.fmt(f),
            Self::Delivery(error) => error.fmt(f),
            Self::Checkpoint(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ConcurrentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Access(error) => Some(error),
            Self::Registry(error) => Some(error),
            Self::Journal(error) => Some(error),
            Self::Delivery(error) => Some(error),
            Self::Checkpoint(error) => Some(error),
            Self::LockPoisoned | Self::Coordinator(_) => None,
        }
    }
}

impl From<CoordinatorError> for ConcurrentError {
    fn from(error: CoordinatorError) -> Self {
        Self::Coordinator(error)
    }
}

impl From<AccessError> for ConcurrentError {
    fn from(error: AccessError) -> Self {
        Self::Access(error)
    }
}

impl From<RegistryError> for ConcurrentError {
    fn from(error: RegistryError) -> Self {
        Self::Registry(error)
    }
}

impl From<JournalError> for ConcurrentError {
    fn from(error: JournalError) -> Self {
        Self::Journal(error)
    }
}

impl From<DeliveryError> for ConcurrentError {
    fn from(error: DeliveryError) -> Self {
        Self::Delivery(error)
    }
}

impl From<CheckpointError> for ConcurrentError {
    fn from(error: CheckpointError) -> Self {
        Self::Checkpoint(error)
    }
}

/// An authorized point-in-time read of one live buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferRead {
    revision: Revision,
    buffer: BufferId,
    text: Text,
}

impl BufferRead {
    /// Returns the workspace revision from which `text` was read.
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// Returns the identifier of the buffer that was read.
    pub const fn buffer(&self) -> BufferId {
        self.buffer
    }

    /// Returns the buffer contents at `revision`.
    pub fn text(&self) -> &Text {
        &self.text
    }
}

/// An authorized point-in-time read of multiple live buffers from one snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuffersRead {
    revision: Revision,
    buffers: BTreeMap<BufferId, Text>,
}

impl BuffersRead {
    /// Returns the workspace revision shared by all returned buffer texts.
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// Returns the map of authorized buffer contents at `revision`.
    pub fn buffers(&self) -> &BTreeMap<BufferId, Text> {
        &self.buffers
    }

    /// Returns the text for a specific buffer in this read, if requested.
    pub fn get(&self, buffer: BufferId) -> Option<&Text> {
        self.buffers.get(&buffer)
    }
}

/// Value-comparable summary of a [`WorkspaceSnapshot`] independent of instance origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotView {
    revision: Revision,
    catalog: BufferCatalog,
}

impl SnapshotView {
    /// Extracts the revision and catalog from a workspace snapshot.
    pub fn from_snapshot(snapshot: &WorkspaceSnapshot) -> Self {
        Self {
            revision: snapshot.revision(),
            catalog: snapshot.catalog().clone(),
        }
    }

    /// Returns the snapshot's revision.
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// Returns the snapshot's buffer catalog.
    pub fn catalog(&self) -> &BufferCatalog {
        &self.catalog
    }
}

impl From<&WorkspaceSnapshot> for SnapshotView {
    fn from(snapshot: &WorkspaceSnapshot) -> Self {
        Self::from_snapshot(snapshot)
    }
}

/// Value-comparable summary of a [`CatchUp`] response independent of instance origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatchUpView {
    /// Incremental committed events in contiguous revision order.
    Events(Vec<(CommitReceipt, Transaction, SnapshotView, SnapshotView)>),
    /// Full tip snapshot returned when `since` predates the delivery floor.
    Snapshot(SnapshotView),
}

impl From<&CatchUp> for CatchUpView {
    fn from(catch_up: &CatchUp) -> Self {
        match catch_up {
            CatchUp::Events(events) => Self::Events(
                events
                    .iter()
                    .map(|event| {
                        (
                            event.receipt(),
                            event.transaction().clone(),
                            SnapshotView::from_snapshot(event.before()),
                            SnapshotView::from_snapshot(event.after()),
                        )
                    })
                    .collect(),
            ),
            CatchUp::Snapshot(snapshot) => Self::Snapshot(SnapshotView::from_snapshot(snapshot)),
        }
    }
}

/// An atomic grant or revocation applied to the coordinator's [`AccessPolicy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessChange {
    /// Grants a workspace-default capability.
    GrantDefault(Capability),
    /// Revokes a workspace-default capability.
    RevokeDefault(Capability),
    /// Grants an actor-wide capability.
    GrantActor(ActorId, Capability),
    /// Revokes an actor-wide capability.
    RevokeActor(ActorId, Capability),
    /// Grants a buffer-scoped capability to an actor.
    GrantBuffer(ActorId, BufferId, Capability),
    /// Revokes a buffer-scoped capability from an actor.
    RevokeBuffer(ActorId, BufferId, Capability),
}

impl AccessChange {
    fn apply(self, policy: &mut AccessPolicy) {
        match self {
            Self::GrantDefault(capability) => policy.grant_default(capability),
            Self::RevokeDefault(capability) => policy.revoke_default(capability),
            Self::GrantActor(actor, capability) => policy.grant_actor(actor, capability),
            Self::RevokeActor(actor, capability) => policy.revoke_actor(actor, capability),
            Self::GrantBuffer(actor, buffer, capability) => {
                policy.grant_buffer(actor, buffer, capability);
            }
            Self::RevokeBuffer(actor, buffer, capability) => {
                policy.revoke_buffer(actor, buffer, capability);
            }
        }
    }
}

/// An operation submitted through [`ConcurrentCoordinator`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConcurrentOp {
    /// Admits a direct coordinator operation (`Open`, `Close`, `Edit`, `Cut`, `Undo`, `Redo`).
    Apply {
        request: RequestId,
        operation: Operation,
    },
    /// Submits a reviewable edit proposal.
    Propose(Proposal),
    /// Records a human reviewer's approval on a pending proposal.
    Approve { request: RequestId, actor: ActorId },
    /// Commits an approved pending proposal.
    CommitProposal { request: RequestId },
    /// Advances the history compaction floor.
    Compact { floor: Revision },
    /// Registers an actor with a role.
    RegisterActor { actor: ActorId, role: ActorRole },
    /// Permanently revokes an active actor.
    RevokeActor { actor: ActorId },
    /// Applies a single grant or revocation to the access policy.
    UpdateAccess(AccessChange),
    /// Performs an authorized single-buffer read at a consistent revision.
    ReadBuffer { actor: ActorId, buffer: BufferId },
    /// Performs an authorized multi-buffer read at a single consistent revision.
    ReadBuffers {
        actor: ActorId,
        buffers: Vec<BufferId>,
    },
    /// Captures an authorized workspace snapshot covering all live buffers.
    ReadSnapshot { actor: ActorId },
    /// Captures the authoritative workspace snapshot without actor filtering.
    Snapshot,
    /// Reads the current workspace revision.
    Revision,
    /// Reads the current retention floor.
    Floor,
    /// Looks up a commit receipt by request identifier.
    ReceiptByRequest { request: RequestId },
    /// Looks up a commit receipt by transaction identifier.
    ReceiptByTxn { txn: TxnId },
    /// Looks up a commit receipt by committed revision.
    ReceiptByRevision { revision: Revision },
    /// Queries receipts committed in `(start, end]`.
    JournalRange { start: Revision, end: Revision },
    /// Queries delivery catch-up from `since`.
    CatchUp { since: Revision },
    /// Reads an actor's register entry.
    Register { actor: ActorId, name: RegisterName },
}

impl ConcurrentOp {
    /// Returns whether this operation is purely read-only across all coordinator states.
    pub const fn is_read_only(&self) -> bool {
        matches!(
            self,
            Self::ReadBuffer { .. }
                | Self::ReadBuffers { .. }
                | Self::ReadSnapshot { .. }
                | Self::Snapshot
                | Self::Revision
                | Self::Floor
                | Self::ReceiptByRequest { .. }
                | Self::ReceiptByTxn { .. }
                | Self::ReceiptByRevision { .. }
                | Self::JournalRange { .. }
                | Self::CatchUp { .. }
                | Self::Register { .. }
        )
    }
}

/// The value-comparable response recorded for a [`ConcurrentOp`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConcurrentResponse {
    /// Response for `Apply` and `CommitProposal`.
    Outcome(Result<CheckpointOutcome, ConcurrentError>),
    /// Response for `Propose`, `Approve`, `Compact`, `RegisterActor`, `RevokeActor`, `UpdateAccess`.
    Unit(Result<(), ConcurrentError>),
    /// Response for `ReadBuffer`.
    BufferRead(Result<BufferRead, ConcurrentError>),
    /// Response for `ReadBuffers`.
    BuffersRead(Result<BuffersRead, ConcurrentError>),
    /// Response for `ReadSnapshot` and `Snapshot`.
    Snapshot(Result<SnapshotView, ConcurrentError>),
    /// Response for `Revision` and `Floor`.
    Revision(Result<Revision, ConcurrentError>),
    /// Response for `ReceiptByRequest`, `ReceiptByTxn`, and `ReceiptByRevision`.
    Receipt(Result<Option<CommitReceipt>, ConcurrentError>),
    /// Response for `JournalRange`.
    JournalRange(Result<Vec<CommitReceipt>, ConcurrentError>),
    /// Response for `CatchUp`.
    CatchUp(Result<CatchUpView, ConcurrentError>),
    /// Response for `Register`.
    Register(Result<Option<RegisterEntry>, ConcurrentError>),
}

/// A recorded invocation/response pair with monotonic real-time sequence timestamps.
///
/// `invoked_at` is sampled before acquiring the state lock and `completed_at` is
/// sampled after releasing the state lock. Operation `a` strictly happens-before
/// operation `b` whenever `a.completed_at < b.invoked_at`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Monotonic identifier assigned when the entry was recorded.
    pub id: u64,
    /// Sequence timestamp captured before acquiring the state lock.
    pub invoked_at: u64,
    /// Sequence timestamp captured after releasing the state lock.
    pub completed_at: u64,
    /// The submitted operation.
    pub operation: ConcurrentOp,
    /// The observed response.
    pub response: ConcurrentResponse,
}

#[derive(Debug)]
struct GuardedState {
    coordinator: Coordinator,
    access: AccessPolicy,
}

#[derive(Debug)]
struct HistoryState {
    next_id: u64,
    entries: Vec<HistoryEntry>,
}

#[derive(Debug)]
struct Shared {
    state: RwLock<GuardedState>,
    clock: AtomicU64,
    recording: AtomicBool,
    history: Mutex<HistoryState>,
}

/// Thread-safe, linearizable concurrent access wrapper over [`Coordinator`].
///
/// Cloning a `ConcurrentCoordinator` produces a handle to the same underlying
/// coordinator and history recorder, allowing safe sharing across threads.
#[derive(Clone, Debug)]
pub struct ConcurrentCoordinator {
    shared: Arc<Shared>,
}

impl ConcurrentCoordinator {
    /// Creates a new concurrent coordinator with history recording enabled.
    pub fn new(actors: ActorRegistry, access: AccessPolicy) -> Self {
        Self::from_coordinator(Coordinator::new(actors, access))
    }

    /// Wraps an existing [`Coordinator`] with history recording enabled.
    pub fn from_coordinator(mut coordinator: Coordinator) -> Self {
        let access = coordinator.access_mut().clone();
        Self {
            shared: Arc::new(Shared {
                state: RwLock::new(GuardedState {
                    coordinator,
                    access,
                }),
                clock: AtomicU64::new(0),
                recording: AtomicBool::new(true),
                history: Mutex::new(HistoryState {
                    next_id: 0,
                    entries: Vec::new(),
                }),
            }),
        }
    }

    /// Restores a concurrent coordinator from a [`CoordinatorCheckpoint`].
    pub fn restore(checkpoint: &CoordinatorCheckpoint) -> Result<Self, CoordinatorError> {
        let coordinator = Coordinator::restore(checkpoint)?;
        Ok(Self::from_coordinator(coordinator))
    }

    /// Creates a new concurrent coordinator with history recording initially disabled.
    pub fn without_recording(actors: ActorRegistry, access: AccessPolicy) -> Self {
        let coordinator = Self::new(actors, access);
        coordinator.shared.recording.store(false, Ordering::SeqCst);
        coordinator
    }

    /// Returns whether any internal lock has been poisoned by a prior panic.
    pub fn is_poisoned(&self) -> bool {
        self.shared.state.is_poisoned() || self.shared.history.is_poisoned()
    }

    /// Enables or disables history recording for subsequent operations.
    pub fn set_recording(&self, enabled: bool) -> Result<(), ConcurrentError> {
        let _guard = self
            .shared
            .history
            .lock()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        self.shared.recording.store(enabled, Ordering::SeqCst);
        Ok(())
    }

    /// Returns whether history recording is currently enabled.
    pub fn is_recording(&self) -> Result<bool, ConcurrentError> {
        if self.shared.history.is_poisoned() {
            return Err(ConcurrentError::LockPoisoned);
        }
        Ok(self.shared.recording.load(Ordering::SeqCst))
    }

    /// Returns a cloned snapshot of all recorded history entries so far.
    pub fn recorded_history(&self) -> Result<Vec<HistoryEntry>, ConcurrentError> {
        let guard = self
            .shared
            .history
            .lock()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        Ok(guard.entries.clone())
    }

    /// Drains and returns all recorded history entries so far.
    pub fn take_history(&self) -> Result<Vec<HistoryEntry>, ConcurrentError> {
        let mut guard = self
            .shared
            .history
            .lock()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        Ok(std::mem::take(&mut guard.entries))
    }

    /// Clears all recorded history entries without altering coordinator state.
    pub fn clear_history(&self) -> Result<(), ConcurrentError> {
        let mut guard = self
            .shared
            .history
            .lock()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        guard.entries.clear();
        Ok(())
    }

    fn tick(&self) -> u64 {
        self.shared.clock.fetch_add(1, Ordering::SeqCst)
    }

    fn check_history_health(&self) -> Result<(), ConcurrentError> {
        if self.shared.history.is_poisoned() {
            Err(ConcurrentError::LockPoisoned)
        } else {
            Ok(())
        }
    }

    /// Pushes a history entry while the caller holds the history lock.
    fn push_history_entry(
        &self,
        history: &mut HistoryState,
        invoked_at: u64,
        completed_at: u64,
        make_entry: impl FnOnce() -> (ConcurrentOp, ConcurrentResponse),
    ) {
        if !self.shared.recording.load(Ordering::SeqCst) {
            return;
        }
        let (operation, response) = make_entry();
        let id = history.next_id;
        history.next_id += 1;
        history.entries.push(HistoryEntry {
            id,
            invoked_at,
            completed_at,
            operation,
            response,
        });
    }

    /// Records a completed read operation after the caller has released the state lock.
    fn record_read_entry(
        &self,
        invoked_at: u64,
        completed_at: u64,
        make_entry: impl FnOnce() -> (ConcurrentOp, ConcurrentResponse),
    ) -> Result<(), ConcurrentError> {
        let mut history_guard = self
            .shared
            .history
            .lock()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        self.push_history_entry(&mut history_guard, invoked_at, completed_at, make_entry);
        drop(history_guard);
        Ok(())
    }

    /// Admits one direct coordinator operation (`Open`, `Close`, `Edit`, `Cut`, `Undo`, `Redo`).
    ///
    /// Exact retries with the same `request` return the original [`CheckpointOutcome`].
    pub fn apply(
        &self,
        request: RequestId,
        operation: Operation,
    ) -> Result<CheckpointOutcome, ConcurrentError> {
        let invoked_at = self.tick();
        let mut state_guard = self
            .shared
            .state
            .write()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let mut history_guard = self
            .shared
            .history
            .lock()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let result = state_guard
            .coordinator
            .apply(request, operation.clone())
            .map_err(ConcurrentError::Coordinator);
        drop(state_guard);
        let completed_at = self.tick();
        self.push_history_entry(&mut history_guard, invoked_at, completed_at, || {
            (
                ConcurrentOp::Apply { request, operation },
                ConcurrentResponse::Outcome(result.clone()),
            )
        });
        drop(history_guard);
        result
    }

    /// Submits a reviewable edit proposal without publishing a workspace revision.
    pub fn propose(&self, proposal: Proposal) -> Result<(), ConcurrentError> {
        let invoked_at = self.tick();
        let mut state_guard = self
            .shared
            .state
            .write()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let mut history_guard = self
            .shared
            .history
            .lock()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let result = state_guard
            .coordinator
            .propose(proposal.clone())
            .map_err(ConcurrentError::Coordinator);
        drop(state_guard);
        let completed_at = self.tick();
        self.push_history_entry(&mut history_guard, invoked_at, completed_at, || {
            (
                ConcurrentOp::Propose(proposal),
                ConcurrentResponse::Unit(result.clone()),
            )
        });
        drop(history_guard);
        result
    }

    /// Approves a pending proposal as a human reviewer.
    pub fn approve(&self, request: RequestId, actor: ActorId) -> Result<(), ConcurrentError> {
        let invoked_at = self.tick();
        let mut state_guard = self
            .shared
            .state
            .write()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let mut history_guard = self
            .shared
            .history
            .lock()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let result = state_guard
            .coordinator
            .approve(request, actor)
            .map_err(ConcurrentError::Coordinator);
        drop(state_guard);
        let completed_at = self.tick();
        self.push_history_entry(&mut history_guard, invoked_at, completed_at, || {
            (
                ConcurrentOp::Approve { request, actor },
                ConcurrentResponse::Unit(result.clone()),
            )
        });
        drop(history_guard);
        result
    }

    /// Commits a previously approved pending proposal.
    pub fn commit_proposal(
        &self,
        request: RequestId,
    ) -> Result<CheckpointOutcome, ConcurrentError> {
        let invoked_at = self.tick();
        let mut state_guard = self
            .shared
            .state
            .write()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let mut history_guard = self
            .shared
            .history
            .lock()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let result = state_guard
            .coordinator
            .commit_proposal(request)
            .map_err(ConcurrentError::Coordinator);
        drop(state_guard);
        let completed_at = self.tick();
        self.push_history_entry(&mut history_guard, invoked_at, completed_at, || {
            (
                ConcurrentOp::CommitProposal { request },
                ConcurrentResponse::Outcome(result.clone()),
            )
        });
        drop(history_guard);
        result
    }

    /// Advances the retention compaction floor.
    pub fn compact(&self, floor: Revision) -> Result<(), ConcurrentError> {
        let invoked_at = self.tick();
        let mut state_guard = self
            .shared
            .state
            .write()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let mut history_guard = self
            .shared
            .history
            .lock()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let result = state_guard
            .coordinator
            .compact(floor)
            .map_err(ConcurrentError::Coordinator);
        drop(state_guard);
        let completed_at = self.tick();
        self.push_history_entry(&mut history_guard, invoked_at, completed_at, || {
            (
                ConcurrentOp::Compact { floor },
                ConcurrentResponse::Unit(result.clone()),
            )
        });
        drop(history_guard);
        result
    }

    /// Registers an actor with a role in the coordinator's actor registry.
    pub fn register_actor(&self, actor: ActorId, role: ActorRole) -> Result<(), ConcurrentError> {
        let invoked_at = self.tick();
        let mut state_guard = self
            .shared
            .state
            .write()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let mut history_guard = self
            .shared
            .history
            .lock()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let result = state_guard
            .coordinator
            .actors_mut()
            .register(actor, role)
            .map_err(ConcurrentError::Registry);
        drop(state_guard);
        let completed_at = self.tick();
        self.push_history_entry(&mut history_guard, invoked_at, completed_at, || {
            (
                ConcurrentOp::RegisterActor { actor, role },
                ConcurrentResponse::Unit(result.clone()),
            )
        });
        drop(history_guard);
        result
    }

    /// Permanently revokes an active actor.
    pub fn revoke_actor(&self, actor: ActorId) -> Result<(), ConcurrentError> {
        let invoked_at = self.tick();
        let mut state_guard = self
            .shared
            .state
            .write()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let mut history_guard = self
            .shared
            .history
            .lock()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let result = state_guard
            .coordinator
            .actors_mut()
            .revoke(actor)
            .map_err(ConcurrentError::Registry);
        drop(state_guard);
        let completed_at = self.tick();
        self.push_history_entry(&mut history_guard, invoked_at, completed_at, || {
            (
                ConcurrentOp::RevokeActor { actor },
                ConcurrentResponse::Unit(result.clone()),
            )
        });
        drop(history_guard);
        result
    }

    /// Atomically updates the coordinator's access policy.
    pub fn update_access(&self, change: AccessChange) -> Result<(), ConcurrentError> {
        let invoked_at = self.tick();
        let mut state_guard = self
            .shared
            .state
            .write()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let mut history_guard = self
            .shared
            .history
            .lock()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        change.apply(state_guard.coordinator.access_mut());
        change.apply(&mut state_guard.access);
        drop(state_guard);
        let completed_at = self.tick();
        self.push_history_entry(&mut history_guard, invoked_at, completed_at, || {
            (
                ConcurrentOp::UpdateAccess(change),
                ConcurrentResponse::Unit(Ok(())),
            )
        });
        drop(history_guard);
        Ok(())
    }

    /// Reads one buffer after verifying that `actor` is active and holds [`Capability::Read`].
    pub fn read_buffer(
        &self,
        actor: ActorId,
        buffer: BufferId,
    ) -> Result<BufferRead, ConcurrentError> {
        let invoked_at = self.tick();
        let read_guard = self
            .shared
            .state
            .read()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let snapshot = read_guard.coordinator.snapshot();
        let result = read_guard
            .access
            .read(
                read_guard.coordinator.workspace().actors(),
                snapshot.catalog(),
                actor,
                buffer,
            )
            .map(|text| BufferRead {
                revision: snapshot.revision(),
                buffer,
                text: text.clone(),
            })
            .map_err(ConcurrentError::Access);
        drop(read_guard);
        let completed_at = self.tick();
        self.record_read_entry(invoked_at, completed_at, || {
            (
                ConcurrentOp::ReadBuffer { actor, buffer },
                ConcurrentResponse::BufferRead(result.clone()),
            )
        })?;
        result
    }

    /// Reads a consistent set of buffers from a single atomic snapshot after
    /// authorizing `actor` for [`Capability::Read`] on every requested buffer.
    pub fn read_buffers(
        &self,
        actor: ActorId,
        buffers: &[BufferId],
    ) -> Result<BuffersRead, ConcurrentError> {
        let invoked_at = self.tick();
        let read_guard = self
            .shared
            .state
            .read()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let snapshot = read_guard.coordinator.snapshot();
        let result = read_guard
            .access
            .authorize(
                read_guard.coordinator.workspace().actors(),
                snapshot.catalog(),
                actor,
                Capability::Read,
                buffers.iter().copied(),
            )
            .map(|()| {
                let unique: BTreeSet<BufferId> = buffers.iter().copied().collect();
                let texts = unique
                    .into_iter()
                    .map(|id| {
                        (
                            id,
                            snapshot
                                .catalog()
                                .get(id)
                                .expect("authorized live buffer")
                                .clone(),
                        )
                    })
                    .collect();
                BuffersRead {
                    revision: snapshot.revision(),
                    buffers: texts,
                }
            })
            .map_err(ConcurrentError::Access);
        drop(read_guard);
        let completed_at = self.tick();
        self.record_read_entry(invoked_at, completed_at, || {
            (
                ConcurrentOp::ReadBuffers {
                    actor,
                    buffers: buffers.to_vec(),
                },
                ConcurrentResponse::BuffersRead(result.clone()),
            )
        })?;
        result
    }

    /// Captures a consistent [`WorkspaceSnapshot`] after verifying `actor` is active
    /// and holds [`Capability::Read`] on every live buffer in that snapshot.
    pub fn read_snapshot(&self, actor: ActorId) -> Result<WorkspaceSnapshot, ConcurrentError> {
        let invoked_at = self.tick();
        let read_guard = self
            .shared
            .state
            .read()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let snapshot = read_guard.coordinator.snapshot();
        let result = read_guard
            .access
            .authorize(
                read_guard.coordinator.workspace().actors(),
                snapshot.catalog(),
                actor,
                Capability::Read,
                snapshot.catalog().live_ids(),
            )
            .map(|()| snapshot)
            .map_err(ConcurrentError::Access);
        drop(read_guard);
        let completed_at = self.tick();
        self.record_read_entry(invoked_at, completed_at, || {
            (
                ConcurrentOp::ReadSnapshot { actor },
                ConcurrentResponse::Snapshot(
                    result
                        .as_ref()
                        .map(SnapshotView::from)
                        .map_err(Clone::clone),
                ),
            )
        })?;
        result
    }

    /// Captures the current authoritative [`WorkspaceSnapshot`].
    pub fn snapshot(&self) -> Result<WorkspaceSnapshot, ConcurrentError> {
        let invoked_at = self.tick();
        let read_guard = self
            .shared
            .state
            .read()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let snapshot = read_guard.coordinator.snapshot();
        drop(read_guard);
        let completed_at = self.tick();
        self.record_read_entry(invoked_at, completed_at, || {
            (
                ConcurrentOp::Snapshot,
                ConcurrentResponse::Snapshot(Ok(SnapshotView::from_snapshot(&snapshot))),
            )
        })?;
        Ok(snapshot)
    }

    /// Returns the current workspace revision.
    pub fn revision(&self) -> Result<Revision, ConcurrentError> {
        let invoked_at = self.tick();
        let read_guard = self
            .shared
            .state
            .read()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let revision = read_guard.coordinator.workspace().revision();
        drop(read_guard);
        let completed_at = self.tick();
        self.record_read_entry(invoked_at, completed_at, || {
            (
                ConcurrentOp::Revision,
                ConcurrentResponse::Revision(Ok(revision)),
            )
        })?;
        Ok(revision)
    }

    /// Returns the current compaction floor revision.
    pub fn floor(&self) -> Result<Revision, ConcurrentError> {
        let invoked_at = self.tick();
        let read_guard = self
            .shared
            .state
            .read()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let floor = read_guard.coordinator.workspace().floor();
        drop(read_guard);
        let completed_at = self.tick();
        self.record_read_entry(invoked_at, completed_at, || {
            (ConcurrentOp::Floor, ConcurrentResponse::Revision(Ok(floor)))
        })?;
        Ok(floor)
    }

    /// Looks up a committed receipt by [`RequestId`].
    pub fn receipt_by_request(
        &self,
        request: RequestId,
    ) -> Result<Option<CommitReceipt>, ConcurrentError> {
        let invoked_at = self.tick();
        let read_guard = self
            .shared
            .state
            .read()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let receipt = read_guard
            .coordinator
            .workspace()
            .receipt_by_request(request);
        drop(read_guard);
        let completed_at = self.tick();
        self.record_read_entry(invoked_at, completed_at, || {
            (
                ConcurrentOp::ReceiptByRequest { request },
                ConcurrentResponse::Receipt(Ok(receipt)),
            )
        })?;
        Ok(receipt)
    }

    /// Looks up a committed receipt by [`TxnId`].
    pub fn receipt_by_txn(&self, txn: TxnId) -> Result<Option<CommitReceipt>, ConcurrentError> {
        let invoked_at = self.tick();
        let read_guard = self
            .shared
            .state
            .read()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let receipt = read_guard.coordinator.workspace().receipt_by_txn(txn);
        drop(read_guard);
        let completed_at = self.tick();
        self.record_read_entry(invoked_at, completed_at, || {
            (
                ConcurrentOp::ReceiptByTxn { txn },
                ConcurrentResponse::Receipt(Ok(receipt)),
            )
        })?;
        Ok(receipt)
    }

    /// Looks up a committed receipt by committed [`Revision`].
    pub fn receipt_by_revision(
        &self,
        revision: Revision,
    ) -> Result<Option<CommitReceipt>, ConcurrentError> {
        let invoked_at = self.tick();
        let read_guard = self
            .shared
            .state
            .read()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let receipt = read_guard
            .coordinator
            .workspace()
            .journal()
            .lookup_revision(revision)
            .copied();
        drop(read_guard);
        let completed_at = self.tick();
        self.record_read_entry(invoked_at, completed_at, || {
            (
                ConcurrentOp::ReceiptByRevision { revision },
                ConcurrentResponse::Receipt(Ok(receipt)),
            )
        })?;
        Ok(receipt)
    }

    /// Returns receipts committed in `(start, end]`.
    pub fn journal_range(
        &self,
        start: Revision,
        end: Revision,
    ) -> Result<Vec<CommitReceipt>, ConcurrentError> {
        let invoked_at = self.tick();
        let read_guard = self
            .shared
            .state
            .read()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let result = read_guard
            .coordinator
            .workspace()
            .journal()
            .range(start, end)
            .map(<[CommitReceipt]>::to_vec)
            .map_err(ConcurrentError::Journal);
        drop(read_guard);
        let completed_at = self.tick();
        self.record_read_entry(invoked_at, completed_at, || {
            (
                ConcurrentOp::JournalRange { start, end },
                ConcurrentResponse::JournalRange(result.clone()),
            )
        })?;
        result
    }

    /// Returns incremental delivery events or a catch-up snapshot starting from `since`.
    pub fn catch_up(&self, since: Revision) -> Result<CatchUp, ConcurrentError> {
        let invoked_at = self.tick();
        let read_guard = self
            .shared
            .state
            .read()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let result = read_guard
            .coordinator
            .workspace()
            .delivery()
            .catch_up(since)
            .map_err(ConcurrentError::Delivery);
        drop(read_guard);
        let completed_at = self.tick();
        self.record_read_entry(invoked_at, completed_at, || {
            (
                ConcurrentOp::CatchUp { since },
                ConcurrentResponse::CatchUp(
                    result.as_ref().map(CatchUpView::from).map_err(Clone::clone),
                ),
            )
        })?;
        result
    }

    /// Returns a cloned copy of an actor's named register entry, if present.
    pub fn register(
        &self,
        actor: ActorId,
        name: &RegisterName,
    ) -> Result<Option<RegisterEntry>, ConcurrentError> {
        let invoked_at = self.tick();
        let read_guard = self
            .shared
            .state
            .read()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        let entry = read_guard.coordinator.register(actor, name).cloned();
        drop(read_guard);
        let completed_at = self.tick();
        self.record_read_entry(invoked_at, completed_at, || {
            (
                ConcurrentOp::Register {
                    actor,
                    name: name.clone(),
                },
                ConcurrentResponse::Register(Ok(entry.clone())),
            )
        })?;
        Ok(entry)
    }

    /// Captures a complete, restorable [`CoordinatorCheckpoint`].
    pub fn checkpoint(&self) -> Result<CoordinatorCheckpoint, ConcurrentError> {
        let guard = self
            .shared
            .state
            .read()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        self.check_history_health()?;
        Ok(guard.coordinator.checkpoint())
    }

    /// Returns whether a live buffer currently has unsaved modifications.
    pub fn is_buffer_dirty(&self, buffer: BufferId) -> Result<bool, ConcurrentError> {
        let guard = self
            .shared
            .state
            .read()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        self.check_history_health()?;
        guard
            .coordinator
            .workspace()
            .is_dirty(buffer)
            .map_err(ConcurrentError::Checkpoint)
    }

    /// Returns the current map from buffer names to live buffer identifiers.
    pub fn buffer_names(&self) -> Result<BTreeMap<String, BufferId>, ConcurrentError> {
        let guard = self
            .shared
            .state
            .read()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        self.check_history_health()?;
        Ok(guard.coordinator.workspace().names().clone())
    }

    /// Returns active actors in ascending identifier order.
    pub fn active_actors(&self) -> Result<Vec<(ActorId, ActorRole)>, ConcurrentError> {
        let guard = self
            .shared
            .state
            .read()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        self.check_history_health()?;
        Ok(guard.coordinator.workspace().actors().active_actors())
    }

    /// Returns whether an actor is currently active.
    pub fn is_actor_active(&self, actor: ActorId) -> Result<bool, ConcurrentError> {
        let guard = self
            .shared
            .state
            .read()
            .map_err(|_| ConcurrentError::LockPoisoned)?;
        self.check_history_health()?;
        Ok(guard.coordinator.workspace().actors().is_active(actor))
    }

    /// Executes any [`ConcurrentOp`] through this concurrent coordinator.
    ///
    /// Returns `Err(ConcurrentError::LockPoisoned)` only if a synchronization
    /// primitive was poisoned; domain rejections are returned inside the
    /// [`ConcurrentResponse`] variant so callers can verify linearizability.
    pub fn execute(&self, op: ConcurrentOp) -> Result<ConcurrentResponse, ConcurrentError> {
        fn lift<T>(
            result: Result<T, ConcurrentError>,
            wrap: impl FnOnce(Result<T, ConcurrentError>) -> ConcurrentResponse,
        ) -> Result<ConcurrentResponse, ConcurrentError> {
            match result {
                Err(ConcurrentError::LockPoisoned) => Err(ConcurrentError::LockPoisoned),
                other => Ok(wrap(other)),
            }
        }

        match op {
            ConcurrentOp::Apply { request, operation } => {
                lift(self.apply(request, operation), ConcurrentResponse::Outcome)
            }
            ConcurrentOp::Propose(proposal) => {
                lift(self.propose(proposal), ConcurrentResponse::Unit)
            }
            ConcurrentOp::Approve { request, actor } => {
                lift(self.approve(request, actor), ConcurrentResponse::Unit)
            }
            ConcurrentOp::CommitProposal { request } => {
                lift(self.commit_proposal(request), ConcurrentResponse::Outcome)
            }
            ConcurrentOp::Compact { floor } => lift(self.compact(floor), ConcurrentResponse::Unit),
            ConcurrentOp::RegisterActor { actor, role } => {
                lift(self.register_actor(actor, role), ConcurrentResponse::Unit)
            }
            ConcurrentOp::RevokeActor { actor } => {
                lift(self.revoke_actor(actor), ConcurrentResponse::Unit)
            }
            ConcurrentOp::UpdateAccess(change) => {
                lift(self.update_access(change), ConcurrentResponse::Unit)
            }
            ConcurrentOp::ReadBuffer { actor, buffer } => lift(
                self.read_buffer(actor, buffer),
                ConcurrentResponse::BufferRead,
            ),
            ConcurrentOp::ReadBuffers { actor, buffers } => lift(
                self.read_buffers(actor, &buffers),
                ConcurrentResponse::BuffersRead,
            ),
            ConcurrentOp::ReadSnapshot { actor } => lift(
                self.read_snapshot(actor).map(|s| SnapshotView::from(&s)),
                ConcurrentResponse::Snapshot,
            ),
            ConcurrentOp::Snapshot => lift(
                self.snapshot().map(|s| SnapshotView::from(&s)),
                ConcurrentResponse::Snapshot,
            ),
            ConcurrentOp::Revision => lift(self.revision(), ConcurrentResponse::Revision),
            ConcurrentOp::Floor => lift(self.floor(), ConcurrentResponse::Revision),
            ConcurrentOp::ReceiptByRequest { request } => lift(
                self.receipt_by_request(request),
                ConcurrentResponse::Receipt,
            ),
            ConcurrentOp::ReceiptByTxn { txn } => {
                lift(self.receipt_by_txn(txn), ConcurrentResponse::Receipt)
            }
            ConcurrentOp::ReceiptByRevision { revision } => lift(
                self.receipt_by_revision(revision),
                ConcurrentResponse::Receipt,
            ),
            ConcurrentOp::JournalRange { start, end } => lift(
                self.journal_range(start, end),
                ConcurrentResponse::JournalRange,
            ),
            ConcurrentOp::CatchUp { since } => lift(
                self.catch_up(since).map(|c| CatchUpView::from(&c)),
                ConcurrentResponse::CatchUp,
            ),
            ConcurrentOp::Register { actor, name } => {
                lift(self.register(actor, &name), ConcurrentResponse::Register)
            }
        }
    }
}

/// Evaluates a [`ConcurrentOp`] sequentially against a plain [`Coordinator`].
///
/// This function defines the sequential reference specification against which
/// concurrent histories are checked for linearizability.
pub fn execute_sequential(coordinator: &mut Coordinator, op: &ConcurrentOp) -> ConcurrentResponse {
    match op {
        ConcurrentOp::Apply { request, operation } => ConcurrentResponse::Outcome(
            coordinator
                .apply(*request, operation.clone())
                .map_err(ConcurrentError::Coordinator),
        ),
        ConcurrentOp::Propose(proposal) => ConcurrentResponse::Unit(
            coordinator
                .propose(proposal.clone())
                .map_err(ConcurrentError::Coordinator),
        ),
        ConcurrentOp::Approve { request, actor } => ConcurrentResponse::Unit(
            coordinator
                .approve(*request, *actor)
                .map_err(ConcurrentError::Coordinator),
        ),
        ConcurrentOp::CommitProposal { request } => ConcurrentResponse::Outcome(
            coordinator
                .commit_proposal(*request)
                .map_err(ConcurrentError::Coordinator),
        ),
        ConcurrentOp::Compact { floor } => ConcurrentResponse::Unit(
            coordinator
                .compact(*floor)
                .map_err(ConcurrentError::Coordinator),
        ),
        ConcurrentOp::RegisterActor { actor, role } => ConcurrentResponse::Unit(
            coordinator
                .actors_mut()
                .register(*actor, *role)
                .map_err(ConcurrentError::Registry),
        ),
        ConcurrentOp::RevokeActor { actor } => ConcurrentResponse::Unit(
            coordinator
                .actors_mut()
                .revoke(*actor)
                .map_err(ConcurrentError::Registry),
        ),
        ConcurrentOp::UpdateAccess(change) => {
            change.apply(coordinator.access_mut());
            ConcurrentResponse::Unit(Ok(()))
        }
        ConcurrentOp::ReadBuffer { actor, buffer } => {
            let snapshot = coordinator.snapshot();
            let actors = coordinator.workspace().actors().clone();
            let result = coordinator
                .access_mut()
                .read(&actors, snapshot.catalog(), *actor, *buffer)
                .map(|text| BufferRead {
                    revision: snapshot.revision(),
                    buffer: *buffer,
                    text: text.clone(),
                })
                .map_err(ConcurrentError::Access);
            ConcurrentResponse::BufferRead(result)
        }
        ConcurrentOp::ReadBuffers { actor, buffers } => {
            let snapshot = coordinator.snapshot();
            let actors = coordinator.workspace().actors().clone();
            let result = coordinator
                .access_mut()
                .authorize(
                    &actors,
                    snapshot.catalog(),
                    *actor,
                    Capability::Read,
                    buffers.iter().copied(),
                )
                .map(|()| {
                    let unique: BTreeSet<BufferId> = buffers.iter().copied().collect();
                    let texts = unique
                        .into_iter()
                        .map(|id| {
                            (
                                id,
                                snapshot
                                    .catalog()
                                    .get(id)
                                    .expect("authorized live buffer")
                                    .clone(),
                            )
                        })
                        .collect();
                    BuffersRead {
                        revision: snapshot.revision(),
                        buffers: texts,
                    }
                })
                .map_err(ConcurrentError::Access);
            ConcurrentResponse::BuffersRead(result)
        }
        ConcurrentOp::ReadSnapshot { actor } => {
            let snapshot = coordinator.snapshot();
            let actors = coordinator.workspace().actors().clone();
            let result = coordinator
                .access_mut()
                .authorize(
                    &actors,
                    snapshot.catalog(),
                    *actor,
                    Capability::Read,
                    snapshot.catalog().live_ids(),
                )
                .map(|()| SnapshotView::from_snapshot(&snapshot))
                .map_err(ConcurrentError::Access);
            ConcurrentResponse::Snapshot(result)
        }
        ConcurrentOp::Snapshot => {
            ConcurrentResponse::Snapshot(Ok(SnapshotView::from_snapshot(&coordinator.snapshot())))
        }
        ConcurrentOp::Revision => {
            ConcurrentResponse::Revision(Ok(coordinator.workspace().revision()))
        }
        ConcurrentOp::Floor => ConcurrentResponse::Revision(Ok(coordinator.workspace().floor())),
        ConcurrentOp::ReceiptByRequest { request } => {
            ConcurrentResponse::Receipt(Ok(coordinator.workspace().receipt_by_request(*request)))
        }
        ConcurrentOp::ReceiptByTxn { txn } => {
            ConcurrentResponse::Receipt(Ok(coordinator.workspace().receipt_by_txn(*txn)))
        }
        ConcurrentOp::ReceiptByRevision { revision } => {
            ConcurrentResponse::Receipt(Ok(coordinator
                .workspace()
                .journal()
                .lookup_revision(*revision)
                .copied()))
        }
        ConcurrentOp::JournalRange { start, end } => ConcurrentResponse::JournalRange(
            coordinator
                .workspace()
                .journal()
                .range(*start, *end)
                .map(<[CommitReceipt]>::to_vec)
                .map_err(ConcurrentError::Journal),
        ),
        ConcurrentOp::CatchUp { since } => ConcurrentResponse::CatchUp(
            coordinator
                .workspace()
                .delivery()
                .catch_up(*since)
                .map(|catch_up| CatchUpView::from(&catch_up))
                .map_err(ConcurrentError::Delivery),
        ),
        ConcurrentOp::Register { actor, name } => {
            ConcurrentResponse::Register(Ok(coordinator.register(*actor, name).cloned()))
        }
    }
}

/// Independent linearizability checker over recorded histories.
///
/// Searches for a permutation of `0..history.len()` such that:
/// 1. Real-time happens-before order is strictly respected: whenever
///    `history[a].completed_at < history[b].invoked_at`, `a` precedes `b`.
/// 2. Replaying the operations sequentially on a fresh [`Coordinator`] restored
///    from `initial` yields the exact recorded response for every operation.
///
/// Returns `Some(order)` with a witnessing linearization order, or `None` if no
/// valid sequential execution exists.
pub fn find_linearization(
    initial: &CoordinatorCheckpoint,
    history: &[HistoryEntry],
) -> Option<Vec<usize>> {
    if history
        .iter()
        .any(|entry| entry.invoked_at > entry.completed_at)
    {
        return None;
    }
    let coordinator = Coordinator::restore(initial).ok()?;
    let n = history.len();
    let mut done = vec![false; n];
    let mut order = Vec::with_capacity(n);
    let mut failed: BTreeSet<(Vec<bool>, String)> = BTreeSet::new();

    fn search(
        coordinator: &Coordinator,
        history: &[HistoryEntry],
        done: &mut [bool],
        order: &mut Vec<usize>,
        failed: &mut BTreeSet<(Vec<bool>, String)>,
    ) -> bool {
        if order.len() == history.len() {
            return true;
        }

        let checkpoint = coordinator.checkpoint();
        // Format provides deterministic, origin-independent structural equality
        // across independent `Coordinator::restore` instances.
        let state_key = format!("{checkpoint:?}");
        if failed.contains(&(done.to_vec(), state_key.clone())) {
            return false;
        }

        // Pure read-only operations never mutate coordinator state in any
        // execution. Any ready read-only operation whose response matches the
        // current state can therefore be linearized immediately without loss of
        // completeness: moving it to the front of any valid completion preserves
        // both happens-before constraints and all subsequent states.
        //
        // In an interval order, an unscheduled event `j` has no unscheduled
        // happens-before predecessor iff `j.invoked_at <= min_{i not done} i.completed_at`.
        let mut eager_reads = Vec::new();
        let mut probe = Coordinator::restore(&checkpoint).expect("valid checkpoint");
        loop {
            let mut progressed = false;
            let mut min_c = u64::MAX;
            for (index, entry) in history.iter().enumerate() {
                if !done[index] && entry.completed_at < min_c {
                    min_c = entry.completed_at;
                }
            }
            for (index, entry) in history.iter().enumerate() {
                if !done[index]
                    && entry.invoked_at <= min_c
                    && entry.operation.is_read_only()
                    && execute_sequential(&mut probe, &entry.operation) == entry.response
                {
                    done[index] = true;
                    order.push(index);
                    eager_reads.push(index);
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }

        if order.len() == history.len() {
            return true;
        }

        let mut min_c = u64::MAX;
        for (index, entry) in history.iter().enumerate() {
            if !done[index] && entry.completed_at < min_c {
                min_c = entry.completed_at;
            }
        }

        let mut candidates: Vec<usize> = history
            .iter()
            .enumerate()
            .filter(|&(index, entry)| {
                !done[index] && entry.invoked_at <= min_c && !entry.operation.is_read_only()
            })
            .map(|(index, _)| index)
            .collect();
        // Exploring by ascending completion time finds valid linearizations
        // quickly while remaining exhaustive when backtracking is needed.
        candidates.sort_by_key(|&index| (history[index].completed_at, history[index].invoked_at));

        for candidate in candidates {
            let mut next = Coordinator::restore(&checkpoint).expect("valid checkpoint");
            if execute_sequential(&mut next, &history[candidate].operation)
                == history[candidate].response
            {
                done[candidate] = true;
                order.push(candidate);
                if search(&next, history, done, order, failed) {
                    return true;
                }
                order.pop();
                done[candidate] = false;
            }
        }

        // Undo eager read commitments before recording this dead state.
        for index in eager_reads.into_iter().rev() {
            order.pop();
            done[index] = false;
        }
        failed.insert((done.to_vec(), state_key));
        false
    }

    if search(&coordinator, history, &mut done, &mut order, &mut failed) {
        Some(order)
    } else {
        None
    }
}

/// Returns whether `history` is linearizable against a sequential [`Coordinator`]
/// initialized from `initial`.
pub fn is_linearizable(initial: &CoordinatorCheckpoint, history: &[HistoryEntry]) -> bool {
    find_linearization(initial, history).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::{
            checkpoints::CheckpointError, proposal::ProposalError, rebase::RebaseError,
            retention::RetentionError,
        },
        edit::{ids::Nonce, intent::EditIntent, patch::Patch, splice::Splice},
        test_support::Rng,
        text::span::ByteSpan,
        workspace::{
            buffer_lifecycle::ClosePolicy, registers::RegisterKind, selections::Selection,
            view::BufferView,
        },
    };
    use std::error::Error;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::Barrier;

    fn intent(actor: u64, nonce: u64, base: u64) -> EditIntent {
        EditIntent::for_actor(ActorId::new(actor), Nonce::new(nonce), Revision::new(base))
    }

    fn splice_replace(source: &str, start: usize, end: usize, replacement: &str) -> Patch {
        let text = Text::from_str(source);
        let span = ByteSpan::validate_in(&text, start, end).unwrap();
        Patch::new(vec![Splice::new(span, Text::from_str(replacement))]).unwrap()
    }

    fn full_replace(source: &str, replacement: &str) -> Patch {
        splice_replace(source, 0, source.len(), replacement)
    }

    fn edit_txn(actor: u64, nonce: u64, base: u64, patches: Vec<(BufferId, Patch)>) -> Transaction {
        Transaction::new(intent(actor, nonce, base), patches).unwrap()
    }

    fn default_registry() -> ActorRegistry {
        let mut actors = ActorRegistry::new();
        actors.register(ActorId::new(1), ActorRole::Human).unwrap();
        actors.register(ActorId::new(2), ActorRole::Human).unwrap();
        actors.register(ActorId::new(3), ActorRole::Agent).unwrap();
        actors.register(ActorId::new(4), ActorRole::Agent).unwrap();
        actors
    }

    fn full_access() -> AccessPolicy {
        let mut access = AccessPolicy::new();
        for capability in Capability::ALL {
            access.grant_default(capability);
        }
        access
    }

    fn seeded_coordinator(buffers: &[(&str, &str)]) -> (ConcurrentCoordinator, Vec<BufferId>) {
        let coord = ConcurrentCoordinator::new(default_registry(), full_access());
        let mut ids = Vec::with_capacity(buffers.len());
        for (index, &(name, content)) in buffers.iter().enumerate() {
            let rev = index as u64;
            let req = RequestId::new(1000 + index as u64);
            let outcome = coord
                .apply(
                    req,
                    Operation::Open {
                        intent: intent(1, 1000 + index as u64, rev),
                        name: name.to_owned(),
                        text: Text::from_str(content),
                    },
                )
                .unwrap();
            ids.push(outcome.buffer.unwrap());
        }
        coord.clear_history().unwrap();
        (coord, ids)
    }

    /// Independently verifies that `order` is a valid permutation of `0..history.len()`,
    /// respects every happens-before constraint, matches every recorded response on a
    /// fresh sequential `Coordinator`, and arrives at `expected_final`.
    fn verify_witness(
        initial: &CoordinatorCheckpoint,
        history: &[HistoryEntry],
        order: &[usize],
        expected_final: Option<&WorkspaceSnapshot>,
    ) {
        assert_eq!(order.len(), history.len());
        let mut position = vec![usize::MAX; history.len()];
        for (pos, &idx) in order.iter().enumerate() {
            assert!(idx < history.len());
            assert_eq!(position[idx], usize::MAX, "duplicate index in witness");
            position[idx] = pos;
        }
        for (a, entry_a) in history.iter().enumerate() {
            for (b, entry_b) in history.iter().enumerate() {
                if entry_a.completed_at < entry_b.invoked_at {
                    assert!(
                        position[a] < position[b],
                        "happens-before violated: {a} ({:?}) must precede {b} ({:?})",
                        (entry_a.invoked_at, entry_a.completed_at),
                        (entry_b.invoked_at, entry_b.completed_at)
                    );
                }
            }
        }
        let mut sequential = Coordinator::restore(initial).unwrap();
        for &idx in order {
            let actual = execute_sequential(&mut sequential, &history[idx].operation);
            assert_eq!(
                actual, history[idx].response,
                "response mismatch at history[{idx}]"
            );
        }
        if let Some(expected) = expected_final {
            assert_eq!(
                SnapshotView::from_snapshot(&sequential.snapshot()),
                SnapshotView::from_snapshot(expected)
            );
        }
    }

    /// Exhaustively enumerates all valid `(Invoke, Linearize, Respond)` interleavings
    /// of actor scripts, executing each schedule against a fresh `ConcurrentCoordinator`
    /// initialized from `initial` and checking linearizability for both the external
    /// interleaved history and the internal recorded history.
    fn run_exhaustive_interleavings(
        initial: &CoordinatorCheckpoint,
        scripts: &[Vec<ConcurrentOp>],
        inspect_schedule: impl FnMut(&[HistoryEntry], &WorkspaceSnapshot),
    ) -> usize {
        // Each operation has 3 phases: 0 = invoke, 1 = linearize, 2 = respond.
        #[derive(Clone)]
        struct ActorProgress {
            op_index: usize,
            phase: u8,
            invoked_at: u64,
            response: Option<ConcurrentResponse>,
        }

        struct ExhaustiveRunner<'a, F> {
            initial: &'a CoordinatorCheckpoint,
            scripts: &'a [Vec<ConcurrentOp>],
            coord: ConcurrentCoordinator,
            progress: Vec<ActorProgress>,
            external: Vec<HistoryEntry>,
            schedules: usize,
            inspect_schedule: F,
        }

        impl<F: FnMut(&[HistoryEntry], &WorkspaceSnapshot)> ExhaustiveRunner<'_, F> {
            fn step(&mut self, clock: u64) {
                if self
                    .progress
                    .iter()
                    .enumerate()
                    .all(|(actor, p)| p.op_index == self.scripts[actor].len())
                {
                    self.schedules += 1;
                    let final_snapshot = self.coord.snapshot().unwrap();
                    let ext_order = find_linearization(self.initial, &self.external)
                        .expect("external interleaved history must be linearizable");
                    verify_witness(
                        self.initial,
                        &self.external,
                        &ext_order,
                        Some(&final_snapshot),
                    );

                    // Exclude the final inspection snapshot we just took when checking
                    // internal history against external history.
                    let mut internal = self.coord.recorded_history().unwrap();
                    internal.pop();
                    let int_order = find_linearization(self.initial, &internal)
                        .expect("internal recorded history must be linearizable");
                    verify_witness(self.initial, &internal, &int_order, Some(&final_snapshot));

                    (self.inspect_schedule)(&self.external, &final_snapshot);
                    return;
                }

                let saved = self.coord.checkpoint().unwrap();
                let saved_history = self.coord.recorded_history().unwrap();

                for actor in 0..self.scripts.len() {
                    if self.progress[actor].op_index == self.scripts[actor].len() {
                        continue;
                    }
                    let op_index = self.progress[actor].op_index;
                    let phase = self.progress[actor].phase;
                    match phase {
                        0 => {
                            let prev_invoked = self.progress[actor].invoked_at;
                            self.progress[actor].invoked_at = clock;
                            self.progress[actor].phase = 1;
                            self.step(clock + 1);
                            self.progress[actor].phase = 0;
                            self.progress[actor].invoked_at = prev_invoked;
                        }
                        1 => {
                            let op = self.scripts[actor][op_index].clone();
                            let response = self.coord.execute(op).unwrap();
                            self.progress[actor].response = Some(response);
                            self.progress[actor].phase = 2;
                            self.step(clock + 1);
                            self.progress[actor].phase = 1;
                            self.progress[actor].response = None;
                            // Restore coordinator and internal history for sibling branch.
                            let restored = ConcurrentCoordinator::restore(&saved).unwrap();
                            {
                                let mut st = self.coord.shared.state.write().unwrap();
                                let mut rst = restored.shared.state.write().unwrap();
                                std::mem::swap(&mut *st, &mut *rst);
                            }
                            {
                                let mut h = self.coord.shared.history.lock().unwrap();
                                h.next_id = saved_history.len() as u64;
                                h.entries = saved_history.clone();
                            }
                        }
                        _ => {
                            let entry = HistoryEntry {
                                id: self.external.len() as u64,
                                invoked_at: self.progress[actor].invoked_at,
                                completed_at: clock,
                                operation: self.scripts[actor][op_index].clone(),
                                response: self.progress[actor].response.take().unwrap(),
                            };
                            self.external.push(entry);
                            let saved_invoked = self.progress[actor].invoked_at;
                            self.progress[actor].op_index += 1;
                            self.progress[actor].phase = 0;
                            self.progress[actor].invoked_at = 0;
                            self.step(clock + 1);
                            self.progress[actor].op_index -= 1;
                            self.progress[actor].phase = 2;
                            self.progress[actor].invoked_at = saved_invoked;
                            let popped = self.external.pop().unwrap();
                            self.progress[actor].response = Some(popped.response);
                        }
                    }
                }
            }
        }

        let mut runner = ExhaustiveRunner {
            initial,
            scripts,
            coord: ConcurrentCoordinator::restore(initial).unwrap(),
            progress: (0..scripts.len())
                .map(|_| ActorProgress {
                    op_index: 0,
                    phase: 0,
                    invoked_at: 0,
                    response: None,
                })
                .collect(),
            external: Vec::new(),
            schedules: 0,
            inspect_schedule,
        };
        runner.step(0);
        runner.schedules
    }

    #[test]
    fn types_are_send_sync_and_errors_implement_traits() {
        fn assert_send_sync<T: Send + Sync>() {}
        fn assert_error<T: Error + Send + Sync + 'static>() {}

        assert_send_sync::<ConcurrentCoordinator>();
        assert_send_sync::<ConcurrentError>();
        assert_send_sync::<HistoryEntry>();
        assert_send_sync::<BufferRead>();
        assert_send_sync::<BuffersRead>();
        assert_send_sync::<SnapshotView>();
        assert_send_sync::<CatchUpView>();
        assert_error::<ConcurrentError>();

        assert_eq!(
            ConcurrentError::LockPoisoned.to_string(),
            "concurrent coordinator lock poisoned"
        );
        assert!(ConcurrentError::LockPoisoned.source().is_none());

        let access_err = ConcurrentError::from(AccessError::InactiveActor(ActorId::new(9)));
        assert!(access_err.source().is_some());
        assert!(access_err.to_string().contains("not active"));

        let reg_err = ConcurrentError::from(RegistryError::UnknownActor(ActorId::new(9)));
        assert!(reg_err.source().is_some());
        assert!(reg_err.to_string().contains("unknown actor"));

        let journal_err = ConcurrentError::from(JournalError::InvalidRange {
            start: Revision::new(2),
            end: Revision::new(1),
        });
        assert!(journal_err.source().is_some());
        assert!(journal_err.to_string().contains("invalid journal range"));

        let delivery_err = ConcurrentError::from(DeliveryError::InvalidEvent);
        assert!(delivery_err.source().is_some());
        assert!(delivery_err.to_string().contains("event"));

        let ckpt_err = ConcurrentError::from(CheckpointError::NameInUse);
        assert!(ckpt_err.source().is_some());
        assert!(ckpt_err.to_string().contains("checkpoint"));

        let coord_err = ConcurrentError::from(CoordinatorError::NoChange);
        assert!(coord_err.source().is_none());
        assert!(coord_err.to_string().contains("NoChange"));
    }

    #[test]
    fn linearizability_checker_rejects_non_linearizable_histories() {
        let (coord, ids) = seeded_coordinator(&[("a", "alpha"), ("b", "beta")]);
        let initial = coord.checkpoint().unwrap();
        let a = ids[0];
        let b = ids[1];

        let edit_both = Operation::Edit(edit_txn(
            1,
            10,
            2,
            vec![
                (a, full_replace("alpha", "ALPHA")),
                (b, full_replace("beta", "BETA")),
            ],
        ));
        let mut seq = Coordinator::restore(&initial).unwrap();
        let outcome = seq.apply(RequestId::new(10), edit_both.clone()).unwrap();

        // 1. Stale read strictly AFTER the write completed must be rejected.
        let stale_history = vec![
            HistoryEntry {
                id: 0,
                invoked_at: 0,
                completed_at: 1,
                operation: ConcurrentOp::Apply {
                    request: RequestId::new(10),
                    operation: edit_both.clone(),
                },
                response: ConcurrentResponse::Outcome(Ok(outcome.clone())),
            },
            HistoryEntry {
                id: 1,
                invoked_at: 2,
                completed_at: 3,
                operation: ConcurrentOp::ReadBuffer {
                    actor: ActorId::new(2),
                    buffer: a,
                },
                response: ConcurrentResponse::BufferRead(Ok(BufferRead {
                    revision: Revision::new(2),
                    buffer: a,
                    text: Text::from_str("alpha"),
                })),
            },
        ];
        assert_eq!(find_linearization(&initial, &stale_history), None);

        // Overlapping read returning pre-write state IS linearizable.
        let mut overlapping_history = stale_history;
        overlapping_history[1].invoked_at = 0;
        let witness = find_linearization(&initial, &overlapping_history)
            .expect("overlapping pre-write read must be linearizable");
        assert_eq!(witness, vec![1, 0]);

        // 2. Torn multi-buffer read (a updated, b stale) must be rejected even when overlapping.
        let mut torn_map = BTreeMap::new();
        torn_map.insert(a, Text::from_str("ALPHA"));
        torn_map.insert(b, Text::from_str("beta"));
        let torn_history = vec![
            HistoryEntry {
                id: 0,
                invoked_at: 0,
                completed_at: 5,
                operation: ConcurrentOp::Apply {
                    request: RequestId::new(10),
                    operation: edit_both.clone(),
                },
                response: ConcurrentResponse::Outcome(Ok(outcome.clone())),
            },
            HistoryEntry {
                id: 1,
                invoked_at: 1,
                completed_at: 4,
                operation: ConcurrentOp::ReadBuffers {
                    actor: ActorId::new(2),
                    buffers: vec![a, b],
                },
                response: ConcurrentResponse::BuffersRead(Ok(BuffersRead {
                    revision: Revision::new(3),
                    buffers: torn_map,
                })),
            },
        ];
        assert!(!is_linearizable(&initial, &torn_history));

        // 3. Duplicate RequestId returning a fabricated second revision receipt must be rejected.
        let bogus_receipt = CommitReceipt::new(intent(1, 10, 2), Revision::new(3)).unwrap();
        let bogus_retry = vec![
            HistoryEntry {
                id: 0,
                invoked_at: 0,
                completed_at: 2,
                operation: ConcurrentOp::Apply {
                    request: RequestId::new(10),
                    operation: edit_both.clone(),
                },
                response: ConcurrentResponse::Outcome(Ok(outcome.clone())),
            },
            HistoryEntry {
                id: 1,
                invoked_at: 1,
                completed_at: 3,
                operation: ConcurrentOp::Apply {
                    request: RequestId::new(10),
                    operation: edit_both,
                },
                response: ConcurrentResponse::Outcome(Ok(CheckpointOutcome {
                    receipt: Some(bogus_receipt),
                    buffer: None,
                })),
            },
        ];
        assert!(!is_linearizable(&initial, &bogus_retry));

        // 4. Inverted interval (invoked_at > completed_at) must be rejected.
        let inverted = vec![HistoryEntry {
            id: 0,
            invoked_at: 5,
            completed_at: 2,
            operation: ConcurrentOp::Revision,
            response: ConcurrentResponse::Revision(Ok(Revision::new(2))),
        }];
        assert!(!is_linearizable(&initial, &inverted));

        // 5. Unauthorized inactive actor falsely succeeding on read must be rejected.
        let unauthorized = vec![HistoryEntry {
            id: 0,
            invoked_at: 0,
            completed_at: 1,
            operation: ConcurrentOp::ReadBuffer {
                actor: ActorId::new(99),
                buffer: a,
            },
            response: ConcurrentResponse::BufferRead(Ok(BufferRead {
                revision: Revision::new(2),
                buffer: a,
                text: Text::from_str("alpha"),
            })),
        }];
        assert!(!is_linearizable(&initial, &unauthorized));
    }

    #[test]
    fn exhaustive_three_actor_disjoint_edits_and_read() {
        let (coord, ids) = seeded_coordinator(&[("doc", "hello world")]);
        let initial = coord.checkpoint().unwrap();
        let doc = ids[0];

        // Actor 1 replaces "hello" (0..5), Actor 2 replaces "world" (6..11), Actor 3 reads.
        let scripts = vec![
            vec![ConcurrentOp::Apply {
                request: RequestId::new(1),
                operation: Operation::Edit(edit_txn(
                    1,
                    1,
                    1,
                    vec![(doc, splice_replace("hello world", 0, 5, "HELLO"))],
                )),
            }],
            vec![ConcurrentOp::Apply {
                request: RequestId::new(2),
                operation: Operation::Edit(edit_txn(
                    2,
                    2,
                    1,
                    vec![(doc, splice_replace("hello world", 6, 11, "WORLD"))],
                )),
            }],
            vec![ConcurrentOp::ReadBuffer {
                actor: ActorId::new(3),
                buffer: doc,
            }],
        ];

        let mut observed_reads = BTreeSet::new();
        let count = run_exhaustive_interleavings(&initial, &scripts, |history, final_snap| {
            assert_eq!(final_snap.revision(), Revision::new(3));
            assert_eq!(
                final_snap.catalog().get(doc).unwrap().as_str(),
                "HELLO WORLD"
            );
            for entry in history {
                if let ConcurrentResponse::BufferRead(Ok(read)) = &entry.response {
                    observed_reads.insert((read.revision().raw(), read.text().as_str().to_owned()));
                }
            }
        });

        assert_eq!(count, 1680);
        assert_eq!(
            observed_reads,
            BTreeSet::from([
                (1, "hello world".to_owned()),
                (2, "HELLO world".to_owned()),
                (2, "hello WORLD".to_owned()),
                (3, "HELLO WORLD".to_owned()),
            ])
        );
    }

    #[test]
    fn exhaustive_three_actor_conflicting_edits_and_read() {
        let (coord, ids) = seeded_coordinator(&[("doc", "abcdef")]);
        let initial = coord.checkpoint().unwrap();
        let doc = ids[0];

        // Actor 1 edits 1..4, Actor 2 edits 2..5 (overlapping!), Actor 3 reads snapshot.
        let scripts = vec![
            vec![ConcurrentOp::Apply {
                request: RequestId::new(1),
                operation: Operation::Edit(edit_txn(
                    1,
                    1,
                    1,
                    vec![(doc, splice_replace("abcdef", 1, 4, "XXX"))],
                )),
            }],
            vec![ConcurrentOp::Apply {
                request: RequestId::new(2),
                operation: Operation::Edit(edit_txn(
                    2,
                    2,
                    1,
                    vec![(doc, splice_replace("abcdef", 2, 5, "YYY"))],
                )),
            }],
            vec![ConcurrentOp::ReadSnapshot {
                actor: ActorId::new(3),
            }],
        ];

        let mut winners = BTreeSet::new();
        let count = run_exhaustive_interleavings(&initial, &scripts, |history, final_snap| {
            assert_eq!(final_snap.revision(), Revision::new(2));
            let text = final_snap.catalog().get(doc).unwrap().as_str().to_owned();
            assert!(text == "aXXXef" || text == "abYYYf");
            winners.insert(text);

            let mut committed = 0usize;
            let mut conflicted = 0usize;
            for entry in history {
                if let ConcurrentResponse::Outcome(res) = &entry.response {
                    match res {
                        Ok(_) => committed += 1,
                        Err(ConcurrentError::Coordinator(CoordinatorError::Checkpoint(
                            CheckpointError::Retention(RetentionError::Rebase(
                                RebaseError::Conflict { .. },
                            )),
                        ))) => conflicted += 1,
                        other => panic!("unexpected conflicting-edit response: {other:?}"),
                    }
                }
            }
            assert_eq!((committed, conflicted), (1, 1));
        });

        assert_eq!(count, 1680);
        assert_eq!(
            winners,
            BTreeSet::from(["aXXXef".to_owned(), "abYYYf".to_owned()])
        );
    }

    #[test]
    fn exhaustive_duplicate_request_id_retries_and_conflicting_reuse() {
        let (coord, ids) = seeded_coordinator(&[("doc", "start")]);
        let initial = coord.checkpoint().unwrap();
        let doc = ids[0];

        let shared_req = RequestId::new(42);
        let op_a = Operation::Edit(edit_txn(
            1,
            42,
            1,
            vec![(doc, full_replace("start", "updated"))],
        ));
        let op_b = Operation::Edit(edit_txn(
            2,
            43,
            1,
            vec![(doc, full_replace("start", "different"))],
        ));

        // 1. Two identical retries + concurrent reader (1,680 interleavings).
        let exact_scripts = vec![
            vec![ConcurrentOp::Apply {
                request: shared_req,
                operation: op_a.clone(),
            }],
            vec![ConcurrentOp::Apply {
                request: shared_req,
                operation: op_a.clone(),
            }],
            vec![ConcurrentOp::ReadBuffer {
                actor: ActorId::new(3),
                buffer: doc,
            }],
        ];
        let count_exact =
            run_exhaustive_interleavings(&initial, &exact_scripts, |history, final_snap| {
                assert_eq!(final_snap.revision(), Revision::new(2));
                assert_eq!(final_snap.catalog().get(doc).unwrap().as_str(), "updated");
                let outcomes: Vec<_> = history
                    .iter()
                    .filter_map(|entry| match &entry.response {
                        ConcurrentResponse::Outcome(res) => Some(res.clone()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(outcomes.len(), 2);
                assert!(outcomes[0].is_ok());
                assert_eq!(outcomes[0], outcomes[1]);
            });
        assert_eq!(count_exact, 1680);

        // 2. Two identical retries + one conflicting payload under the same RequestId (1,680 interleavings).
        let reuse_scripts = vec![
            vec![ConcurrentOp::Apply {
                request: shared_req,
                operation: op_a.clone(),
            }],
            vec![ConcurrentOp::Apply {
                request: shared_req,
                operation: op_a,
            }],
            vec![ConcurrentOp::Apply {
                request: shared_req,
                operation: op_b,
            }],
        ];
        let mut reuse_outcomes = BTreeSet::new();
        let count_reuse =
            run_exhaustive_interleavings(&initial, &reuse_scripts, |history, final_snap| {
                assert_eq!(final_snap.revision(), Revision::new(2));
                let final_text = final_snap.catalog().get(doc).unwrap().as_str().to_owned();
                let oks = history
                    .iter()
                    .filter(|e| matches!(e.response, ConcurrentResponse::Outcome(Ok(_))))
                    .count();
                let reuses = history
                    .iter()
                    .filter(|e| {
                        matches!(
                            e.response,
                            ConcurrentResponse::Outcome(Err(ConcurrentError::Coordinator(
                                CoordinatorError::RequestReuse(r)
                            ))) if r == shared_req
                        )
                    })
                    .count();
                if final_text == "updated" {
                    assert_eq!((oks, reuses), (2, 1));
                } else {
                    assert_eq!(final_text, "different");
                    assert_eq!((oks, reuses), (1, 2));
                }
                reuse_outcomes.insert(final_text);
            });
        assert_eq!(count_reuse, 1680);
        assert_eq!(
            reuse_outcomes,
            BTreeSet::from(["updated".to_owned(), "different".to_owned()])
        );
    }

    #[test]
    fn exhaustive_proposal_approve_commit_and_competing_edit() {
        let (coord, ids) = seeded_coordinator(&[("doc", "hello world")]);
        let initial = coord.checkpoint().unwrap();
        let doc = ids[0];
        let req = RequestId::new(7);
        let proposal = Proposal::new(
            req,
            edit_txn(
                3,
                7,
                1,
                vec![(doc, splice_replace("hello world", 0, 5, "HELLO"))],
            ),
        );

        // 1. Out-of-order 3-actor race of propose, approve, and commit_proposal (1,680 interleavings).
        let scripts = vec![
            vec![ConcurrentOp::Propose(proposal.clone())],
            vec![ConcurrentOp::Approve {
                request: req,
                actor: ActorId::new(1),
            }],
            vec![ConcurrentOp::CommitProposal { request: req }],
        ];
        let mut committed_seen = false;
        let mut unapproved_seen = false;
        let mut missing_seen = false;
        let count = run_exhaustive_interleavings(&initial, &scripts, |history, final_snap| {
            for entry in history {
                if let ConcurrentOp::CommitProposal { .. } = entry.operation {
                    match &entry.response {
                        ConcurrentResponse::Outcome(Ok(_)) => {
                            committed_seen = true;
                            assert_eq!(final_snap.revision(), Revision::new(2));
                            assert_eq!(
                                final_snap.catalog().get(doc).unwrap().as_str(),
                                "HELLO world"
                            );
                        }
                        ConcurrentResponse::Outcome(Err(ConcurrentError::Coordinator(
                            CoordinatorError::Unapproved(r),
                        ))) if *r == req => unapproved_seen = true,
                        ConcurrentResponse::Outcome(Err(ConcurrentError::Coordinator(
                            CoordinatorError::MissingProposal(r),
                        ))) if *r == req => missing_seen = true,
                        other => panic!("unexpected commit_proposal response: {other:?}"),
                    }
                }
            }
        });
        assert_eq!(count, 1680);
        assert!(committed_seen && unapproved_seen && missing_seen);

        // 2. Two actors (2 ops each = 924 interleavings):
        // Actor 1 approves and commits an existing proposal; Actor 2 submits a direct edit and reads.
        coord.propose(proposal).unwrap();
        coord.clear_history().unwrap();
        let proposed_initial = coord.checkpoint().unwrap();

        let two_actor_scripts = vec![
            vec![
                ConcurrentOp::Approve {
                    request: req,
                    actor: ActorId::new(1),
                },
                ConcurrentOp::CommitProposal { request: req },
            ],
            vec![
                ConcurrentOp::Apply {
                    request: RequestId::new(8),
                    operation: Operation::Edit(edit_txn(
                        2,
                        8,
                        1,
                        vec![(doc, splice_replace("hello world", 6, 11, "WORLD"))],
                    )),
                },
                ConcurrentOp::ReadBuffer {
                    actor: ActorId::new(2),
                    buffer: doc,
                },
            ],
        ];
        let mut stale_seen = false;
        let mut unapproved_after_competing_seen = false;
        let mut both_committed_seen = false;
        let count_two =
            run_exhaustive_interleavings(&proposed_initial, &two_actor_scripts, |history, snap| {
                for entry in history {
                    if let ConcurrentOp::CommitProposal { .. } = entry.operation {
                        match &entry.response {
                            ConcurrentResponse::Outcome(Ok(_)) => {
                                both_committed_seen = true;
                                assert_eq!(snap.revision(), Revision::new(3));
                                assert_eq!(
                                    snap.catalog().get(doc).unwrap().as_str(),
                                    "HELLO WORLD"
                                );
                            }
                            ConcurrentResponse::Outcome(Err(ConcurrentError::Coordinator(
                                CoordinatorError::StaleApproval(r),
                            ))) if *r == req => {
                                stale_seen = true;
                                assert_eq!(snap.revision(), Revision::new(2));
                            }
                            ConcurrentResponse::Outcome(Err(ConcurrentError::Coordinator(
                                CoordinatorError::Unapproved(r),
                            ))) if *r == req => {
                                unapproved_after_competing_seen = true;
                                assert_eq!(snap.revision(), Revision::new(2));
                            }
                            other => panic!("unexpected proposal commit outcome: {other:?}"),
                        }
                    }
                }
            });
        assert_eq!(count_two, 924);
        assert!(stale_seen && unapproved_after_competing_seen && both_committed_seen);
    }

    #[test]
    fn exhaustive_multibuffer_atomic_read_and_revocation() {
        let (coord, ids) = seeded_coordinator(&[("a", "v0"), ("b", "v0")]);
        let initial = coord.checkpoint().unwrap();
        let a = ids[0];
        let b = ids[1];

        let scripts = vec![
            vec![ConcurrentOp::Apply {
                request: RequestId::new(10),
                operation: Operation::Edit(edit_txn(
                    1,
                    10,
                    2,
                    vec![(a, full_replace("v0", "v1")), (b, full_replace("v0", "v1"))],
                )),
            }],
            vec![ConcurrentOp::ReadBuffers {
                actor: ActorId::new(3),
                buffers: vec![a, b],
            }],
            vec![ConcurrentOp::RevokeActor {
                actor: ActorId::new(3),
            }],
        ];

        let mut saw_v0 = false;
        let mut saw_v1 = false;
        let mut saw_revoked = false;
        let count = run_exhaustive_interleavings(&initial, &scripts, |history, _| {
            for entry in history {
                if let ConcurrentOp::ReadBuffers { .. } = entry.operation {
                    match &entry.response {
                        ConcurrentResponse::BuffersRead(Ok(read)) => {
                            let ta = read.get(a).unwrap().as_str();
                            let tb = read.get(b).unwrap().as_str();
                            assert_eq!(ta, tb, "torn read observed: a={ta}, b={tb}");
                            if ta == "v0" {
                                saw_v0 = true;
                            } else {
                                assert_eq!(ta, "v1");
                                saw_v1 = true;
                            }
                        }
                        ConcurrentResponse::BuffersRead(Err(ConcurrentError::Access(
                            AccessError::InactiveActor(actor),
                        ))) if *actor == ActorId::new(3) => saw_revoked = true,
                        other => panic!("unexpected ReadBuffers response: {other:?}"),
                    }
                }
            }
        });
        assert_eq!(count, 1680);
        assert!(saw_v0 && saw_v1 && saw_revoked);
    }

    #[test]
    fn exhaustive_undo_redo_compaction_and_read_during_submit() {
        let (coord, ids) = seeded_coordinator(&[("doc", "v0")]);
        let doc = ids[0];
        let actor = ActorId::new(1);
        let root_txn = TxnId::new(actor, Nonce::new(10));
        coord
            .apply(
                RequestId::new(10),
                Operation::Edit(edit_txn(1, 10, 1, vec![(doc, full_replace("v0", "v1"))])),
            )
            .unwrap();
        coord.clear_history().unwrap();
        let initial = coord.checkpoint().unwrap();

        // 1. Undo -> Redo racing with Compact(2) -> ReadBuffer (924 interleavings).
        let scripts = vec![
            vec![
                ConcurrentOp::Apply {
                    request: RequestId::new(11),
                    operation: Operation::Undo {
                        actor,
                        target: root_txn,
                        transaction: TxnId::new(actor, Nonce::new(11)),
                    },
                },
                ConcurrentOp::Apply {
                    request: RequestId::new(12),
                    operation: Operation::Redo {
                        actor,
                        target: root_txn,
                        transaction: TxnId::new(actor, Nonce::new(12)),
                    },
                },
            ],
            vec![
                ConcurrentOp::Compact {
                    floor: Revision::new(2),
                },
                ConcurrentOp::ReadBuffer {
                    actor: ActorId::new(2),
                    buffer: doc,
                },
            ],
        ];
        let mut saw_expired = false;
        let mut saw_redone = false;
        let count = run_exhaustive_interleavings(&initial, &scripts, |history, _| {
            for entry in history {
                if let ConcurrentOp::Apply {
                    request,
                    operation: Operation::Undo { .. },
                } = &entry.operation
                {
                    assert_eq!(*request, RequestId::new(11));
                    if matches!(
                        entry.response,
                        ConcurrentResponse::Outcome(Err(ConcurrentError::Coordinator(
                            CoordinatorError::Checkpoint(CheckpointError::Retention(
                                RetentionError::BaseExpired { .. }
                            ))
                        )))
                    ) {
                        saw_expired = true;
                    }
                }
                if let ConcurrentOp::Apply {
                    operation: Operation::Redo { .. },
                    ..
                } = &entry.operation
                {
                    if matches!(entry.response, ConcurrentResponse::Outcome(Ok(_))) {
                        saw_redone = true;
                    }
                }
            }
        });
        assert_eq!(count, 924);
        assert!(saw_expired && saw_redone);

        // 2. Read-during-submit + ReceiptByRequest + ReceiptByTxn (924 interleavings).
        let receipt_scripts = vec![
            vec![
                ConcurrentOp::Apply {
                    request: RequestId::new(20),
                    operation: Operation::Edit(edit_txn(
                        1,
                        20,
                        2,
                        vec![(doc, full_replace("v1", "v2"))],
                    )),
                },
                ConcurrentOp::ReceiptByRequest {
                    request: RequestId::new(20),
                },
            ],
            vec![
                ConcurrentOp::ReadBuffer {
                    actor: ActorId::new(2),
                    buffer: doc,
                },
                ConcurrentOp::ReceiptByTxn {
                    txn: TxnId::new(ActorId::new(1), Nonce::new(20)),
                },
            ],
        ];
        let count_receipts = run_exhaustive_interleavings(&initial, &receipt_scripts, |_, snap| {
            assert_eq!(snap.revision(), Revision::new(3));
            assert_eq!(snap.catalog().get(doc).unwrap().as_str(), "v2");
        });
        assert_eq!(count_receipts, 924);
    }

    #[test]
    fn exhaustive_lifecycle_open_close_and_read() {
        let (coord, ids) = seeded_coordinator(&[("a", "initial")]);
        let initial = coord.checkpoint().unwrap();
        let a = ids[0];

        let scripts = vec![
            vec![ConcurrentOp::Apply {
                request: RequestId::new(30),
                operation: Operation::Close {
                    intent: intent(1, 30, 1),
                    buffer: a,
                    policy: ClosePolicy::Discard,
                },
            }],
            vec![ConcurrentOp::Apply {
                request: RequestId::new(31),
                operation: Operation::Open {
                    intent: intent(2, 31, 1),
                    name: "b".to_owned(),
                    text: Text::from_str("opened"),
                },
            }],
            vec![ConcurrentOp::ReadBuffer {
                actor: ActorId::new(3),
                buffer: a,
            }],
        ];
        let mut saw_closed_read = false;
        let mut saw_live_read = false;
        let count = run_exhaustive_interleavings(&initial, &scripts, |history, _| {
            for entry in history {
                if let ConcurrentOp::ReadBuffer { .. } = entry.operation {
                    match &entry.response {
                        ConcurrentResponse::BufferRead(Ok(read)) => {
                            assert_eq!(read.text().as_str(), "initial");
                            saw_live_read = true;
                        }
                        ConcurrentResponse::BufferRead(Err(ConcurrentError::Access(
                            AccessError::BufferClosed(id),
                        ))) if *id == a => saw_closed_read = true,
                        other => panic!("unexpected read during lifecycle: {other:?}"),
                    }
                }
            }
        });
        assert_eq!(count, 1680);
        assert!(saw_closed_read && saw_live_read);
    }

    /// Runs a wave of per-thread operation lists concurrently using a `Barrier`
    /// and `std::thread::scope`, capturing exact external invocation/completion
    /// sequence numbers and verifying linearizability of both external and internal
    /// histories against a fresh sequential `Coordinator`.
    fn run_multithreaded_round(
        coord: &ConcurrentCoordinator,
        initial: &CoordinatorCheckpoint,
        thread_ops: Vec<Vec<ConcurrentOp>>,
    ) -> (Vec<HistoryEntry>, Vec<HistoryEntry>) {
        let thread_count = thread_ops.len();
        let barrier = Barrier::new(thread_count);
        let ext_clock = AtomicU64::new(0);
        let ext_id = AtomicU64::new(0);

        let per_thread: Vec<Vec<HistoryEntry>> = std::thread::scope(|scope| {
            let handles: Vec<_> = thread_ops
                .into_iter()
                .map(|ops| {
                    let coord = coord.clone();
                    let barrier = &barrier;
                    let ext_clock = &ext_clock;
                    let ext_id = &ext_id;
                    scope.spawn(move || {
                        let mut local = Vec::with_capacity(ops.len());
                        barrier.wait();
                        for op in ops {
                            let invoked_at = ext_clock.fetch_add(1, Ordering::SeqCst);
                            let response = coord.execute(op.clone()).unwrap();
                            let completed_at = ext_clock.fetch_add(1, Ordering::SeqCst);
                            let id = ext_id.fetch_add(1, Ordering::SeqCst);
                            local.push(HistoryEntry {
                                id,
                                invoked_at,
                                completed_at,
                                operation: op,
                                response,
                            });
                        }
                        local
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let external: Vec<HistoryEntry> = per_thread.into_iter().flatten().collect();
        let internal = coord.recorded_history().unwrap();
        let final_snapshot = coord.snapshot().unwrap();

        let ext_order = find_linearization(initial, &external)
            .expect("external multi-threaded history must be linearizable");
        verify_witness(initial, &external, &ext_order, Some(&final_snapshot));

        let int_order = find_linearization(initial, &internal)
            .expect("internal multi-threaded history must be linearizable");
        verify_witness(initial, &internal, &int_order, Some(&final_snapshot));

        (external, internal)
    }

    #[test]
    fn multithreaded_race_submit_read_and_duplicate_retry_rounds() {
        let mut rng = Rng::new(0x5eed_cafe_1234_5678);

        for round in 0..20u64 {
            let (coord, ids) = seeded_coordinator(&[("a", "aaaa bbbb cccc"), ("b", "0000 1111")]);
            let initial = coord.checkpoint().unwrap();
            let a = ids[0];
            let b = ids[1];
            let base = 2;
            let base_req = round * 100 + 1;

            let retry_req = RequestId::new(base_req);
            let retry_op = Operation::Edit(edit_txn(
                1,
                base_req,
                base,
                vec![(a, splice_replace("aaaa bbbb cccc", 0, 4, "AAAA"))],
            ));
            let disjoint_op = Operation::Edit(edit_txn(
                2,
                base_req + 1,
                base,
                vec![(a, splice_replace("aaaa bbbb cccc", 10, 14, "CCCC"))],
            ));
            let conflicting_op = Operation::Edit(edit_txn(
                3,
                base_req + 2,
                base,
                vec![(a, splice_replace("aaaa bbbb cccc", 2, 7, "CONFLICT"))],
            ));
            let paired_b_op = Operation::Edit(edit_txn(
                4,
                base_req + 3,
                base,
                vec![(b, splice_replace("0000 1111", 0, 4, "XXXX"))],
            ));

            let inactive_actor = ActorId::new(99);
            let reader_actor = ActorId::new(if rng.gen_bool() { 1 } else { 3 });

            let mut thread_ops = vec![
                vec![
                    ConcurrentOp::Apply {
                        request: retry_req,
                        operation: retry_op.clone(),
                    },
                    ConcurrentOp::ReceiptByRequest { request: retry_req },
                ],
                vec![
                    ConcurrentOp::Apply {
                        request: retry_req,
                        operation: retry_op,
                    },
                    ConcurrentOp::JournalRange {
                        start: Revision::ZERO,
                        end: Revision::new(2),
                    },
                ],
                vec![
                    ConcurrentOp::Apply {
                        request: RequestId::new(base_req + 1),
                        operation: disjoint_op,
                    },
                    ConcurrentOp::ReadBuffer {
                        actor: reader_actor,
                        buffer: a,
                    },
                ],
                vec![
                    ConcurrentOp::Apply {
                        request: RequestId::new(base_req + 2),
                        operation: conflicting_op,
                    },
                    ConcurrentOp::ReadBuffers {
                        actor: ActorId::new(2),
                        buffers: vec![a, b],
                    },
                ],
                vec![
                    ConcurrentOp::Apply {
                        request: RequestId::new(base_req + 3),
                        operation: paired_b_op,
                    },
                    ConcurrentOp::CatchUp {
                        since: Revision::new(2),
                    },
                ],
                vec![
                    ConcurrentOp::ReadSnapshot {
                        actor: inactive_actor,
                    },
                    ConcurrentOp::ReadSnapshot {
                        actor: ActorId::new(1),
                    },
                ],
            ];
            rng.shuffle(&mut thread_ops);

            let (external, _) = run_multithreaded_round(&coord, &initial, thread_ops);

            // Both duplicate retry submissions must agree on their exact response.
            let retry_responses: Vec<_> = external
                .iter()
                .filter(|entry| {
                    matches!(
                        entry.operation,
                        ConcurrentOp::Apply { request, .. } if request == retry_req
                    )
                })
                .map(|entry| &entry.response)
                .collect();
            assert_eq!(retry_responses.len(), 2);
            assert_eq!(retry_responses[0], retry_responses[1]);

            // Inactive actor read must always fail cleanly.
            for entry in &external {
                if matches!(
                    entry.operation,
                    ConcurrentOp::ReadSnapshot { actor } if actor == inactive_actor
                ) {
                    assert_eq!(
                        entry.response,
                        ConcurrentResponse::Snapshot(Err(ConcurrentError::Access(
                            AccessError::InactiveActor(inactive_actor)
                        )))
                    );
                }
            }
        }
    }

    #[test]
    fn multithreaded_duplicate_request_id_retry_storm() {
        for round in 0..16u64 {
            let (coord, ids) = seeded_coordinator(&[("doc", "hello")]);
            let initial = coord.checkpoint().unwrap();
            let doc = ids[0];
            let req = RequestId::new(500 + round);

            // Exact retry storm: 6 threads racing the exact same request simultaneously.
            let op = Operation::Edit(edit_txn(
                1,
                500 + round,
                1,
                vec![(doc, full_replace("hello", "world"))],
            ));
            let ops: Vec<Vec<ConcurrentOp>> = (0..6)
                .map(|_| {
                    vec![
                        ConcurrentOp::Apply {
                            request: req,
                            operation: op.clone(),
                        },
                        ConcurrentOp::ReceiptByRequest { request: req },
                    ]
                })
                .collect();
            let (external, _) = run_multithreaded_round(&coord, &initial, ops);

            assert_eq!(coord.revision().unwrap(), Revision::new(2));
            let outcomes: Vec<_> = external
                .iter()
                .filter_map(|e| match (&e.operation, &e.response) {
                    (ConcurrentOp::Apply { .. }, ConcurrentResponse::Outcome(r)) => Some(r.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(outcomes.len(), 6);
            let first = outcomes[0].as_ref().expect("retry storm must commit once");
            for outcome in &outcomes {
                assert_eq!(outcome.as_ref().unwrap(), first);
            }
        }

        // Conflicting payload storm under the same RequestId.
        for round in 0..12u64 {
            let (coord, ids) = seeded_coordinator(&[("doc", "hello")]);
            let initial = coord.checkpoint().unwrap();
            let doc = ids[0];
            let req = RequestId::new(700 + round);
            let op_one = Operation::Edit(edit_txn(
                1,
                700 + round,
                1,
                vec![(doc, full_replace("hello", "one"))],
            ));
            let op_two = Operation::Edit(edit_txn(
                2,
                800 + round,
                1,
                vec![(doc, full_replace("hello", "two"))],
            ));
            let ops = vec![
                vec![ConcurrentOp::Apply {
                    request: req,
                    operation: op_one.clone(),
                }],
                vec![ConcurrentOp::Apply {
                    request: req,
                    operation: op_one,
                }],
                vec![ConcurrentOp::Apply {
                    request: req,
                    operation: op_two.clone(),
                }],
                vec![ConcurrentOp::Apply {
                    request: req,
                    operation: op_two,
                }],
                vec![ConcurrentOp::ReadBuffer {
                    actor: ActorId::new(3),
                    buffer: doc,
                }],
            ];
            let (external, _) = run_multithreaded_round(&coord, &initial, ops);
            assert_eq!(coord.revision().unwrap(), Revision::new(2));
            let ok_count = external
                .iter()
                .filter(|e| matches!(e.response, ConcurrentResponse::Outcome(Ok(_))))
                .count();
            let reuse_count = external
                .iter()
                .filter(|e| {
                    matches!(
                        e.response,
                        ConcurrentResponse::Outcome(Err(ConcurrentError::Coordinator(
                            CoordinatorError::RequestReuse(r)
                        ))) if r == req
                    )
                })
                .count();
            assert_eq!((ok_count, reuse_count), (2, 2));
        }
    }

    #[test]
    fn multithreaded_consistent_non_torn_reads_under_write_contention() {
        for round in 0..10u64 {
            let (coord, ids) = seeded_coordinator(&[("left", "v0"), ("right", "v0")]);
            let initial = coord.checkpoint().unwrap();
            let left = ids[0];
            let right = ids[1];

            let barrier = Barrier::new(6);
            let ext_clock = AtomicU64::new(0);
            let ext_id = AtomicU64::new(0);

            let per_thread: Vec<Vec<HistoryEntry>> = std::thread::scope(|scope| {
                let mut handles = Vec::new();
                // Two writer threads atomically update both buffers to matching values.
                for writer_idx in 0..2u64 {
                    let coord = coord.clone();
                    let barrier = &barrier;
                    let ext_clock = &ext_clock;
                    let ext_id = &ext_id;
                    handles.push(scope.spawn(move || {
                        let mut local = Vec::new();
                        let actor = writer_idx + 1;
                        barrier.wait();
                        for step in 0..3u64 {
                            let snap = coord.snapshot().unwrap();
                            let cur_rev = snap.revision().raw();
                            let cur_left = snap.catalog().get(left).unwrap().as_str();
                            let cur_right = snap.catalog().get(right).unwrap().as_str();
                            assert_eq!(cur_left, cur_right, "snapshot torn before write");

                            let tag = format!("r{round}-w{writer_idx}-s{step}");
                            let nonce = round * 100 + writer_idx * 10 + step + 1;
                            let op = ConcurrentOp::Apply {
                                request: RequestId::new(nonce),
                                operation: Operation::Edit(edit_txn(
                                    actor,
                                    nonce,
                                    cur_rev,
                                    vec![
                                        (left, full_replace(cur_left, &tag)),
                                        (right, full_replace(cur_right, &tag)),
                                    ],
                                )),
                            };
                            let invoked_at = ext_clock.fetch_add(1, Ordering::SeqCst);
                            let response = coord.execute(op.clone()).unwrap();
                            let completed_at = ext_clock.fetch_add(1, Ordering::SeqCst);
                            let id = ext_id.fetch_add(1, Ordering::SeqCst);
                            local.push(HistoryEntry {
                                id,
                                invoked_at,
                                completed_at,
                                operation: op,
                                response,
                            });
                        }
                        local
                    }));
                }

                // Four reader threads continuously assert left == right on every read.
                for reader_idx in 0..4u64 {
                    let coord = coord.clone();
                    let barrier = &barrier;
                    let ext_clock = &ext_clock;
                    let ext_id = &ext_id;
                    handles.push(scope.spawn(move || {
                        let mut local = Vec::new();
                        let actor = ActorId::new(if reader_idx % 2 == 0 { 3 } else { 4 });
                        barrier.wait();
                        for step in 0..4 {
                            let op = if step % 2 == 0 {
                                ConcurrentOp::ReadBuffers {
                                    actor,
                                    buffers: vec![left, right],
                                }
                            } else {
                                ConcurrentOp::ReadSnapshot { actor }
                            };
                            let invoked_at = ext_clock.fetch_add(1, Ordering::SeqCst);
                            let response = coord.execute(op.clone()).unwrap();
                            let completed_at = ext_clock.fetch_add(1, Ordering::SeqCst);
                            match &response {
                                ConcurrentResponse::BuffersRead(Ok(read)) => {
                                    assert_eq!(
                                        read.get(left).unwrap().as_str(),
                                        read.get(right).unwrap().as_str(),
                                        "torn ReadBuffers observed"
                                    );
                                }
                                ConcurrentResponse::Snapshot(Ok(snap)) => {
                                    assert_eq!(
                                        snap.catalog().get(left).unwrap().as_str(),
                                        snap.catalog().get(right).unwrap().as_str(),
                                        "torn ReadSnapshot observed"
                                    );
                                }
                                other => panic!("unexpected authorized read response: {other:?}"),
                            }
                            let id = ext_id.fetch_add(1, Ordering::SeqCst);
                            local.push(HistoryEntry {
                                id,
                                invoked_at,
                                completed_at,
                                operation: op,
                                response,
                            });
                        }
                        local
                    }));
                }

                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });

            let external: Vec<HistoryEntry> = per_thread.into_iter().flatten().collect();
            let internal = coord.recorded_history().unwrap();
            let final_snapshot = coord.snapshot().unwrap();
            assert_eq!(
                final_snapshot.catalog().get(left).unwrap().as_str(),
                final_snapshot.catalog().get(right).unwrap().as_str()
            );

            // External history omits the writers' helper `snapshot()` calls; verify
            // both the external subset (which has all writes) and full internal log.
            let ext_order = find_linearization(&initial, &external)
                .expect("external non-torn history must be linearizable");
            verify_witness(&initial, &external, &ext_order, Some(&final_snapshot));

            let int_order = find_linearization(&initial, &internal)
                .expect("internal non-torn history must be linearizable");
            verify_witness(&initial, &internal, &int_order, Some(&final_snapshot));
        }
    }

    #[test]
    fn multithreaded_proposal_approval_commit_and_agent_denial_rounds() {
        for round in 0..15u64 {
            let (coord, ids) = seeded_coordinator(&[("doc", "hello world")]);
            let initial = coord.checkpoint().unwrap();
            let doc = ids[0];
            let req = RequestId::new(200 + round);
            let proposal = Proposal::new(
                req,
                edit_txn(
                    3,
                    200 + round,
                    1,
                    vec![(doc, splice_replace("hello world", 0, 5, "HELLO"))],
                ),
            );
            let competing = Operation::Edit(edit_txn(
                2,
                300 + round,
                1,
                vec![(doc, splice_replace("hello world", 6, 11, "WORLD"))],
            ));

            let ops = vec![
                vec![
                    ConcurrentOp::Propose(proposal.clone()),
                    ConcurrentOp::CommitProposal { request: req },
                ],
                vec![
                    ConcurrentOp::Propose(proposal),
                    ConcurrentOp::Approve {
                        request: req,
                        actor: ActorId::new(1),
                    },
                ],
                vec![
                    ConcurrentOp::Approve {
                        request: req,
                        actor: ActorId::new(3),
                    },
                    ConcurrentOp::CommitProposal { request: req },
                ],
                vec![
                    ConcurrentOp::Apply {
                        request: RequestId::new(300 + round),
                        operation: competing,
                    },
                    ConcurrentOp::ReadSnapshot {
                        actor: ActorId::new(1),
                    },
                ],
                vec![
                    ConcurrentOp::Approve {
                        request: req,
                        actor: ActorId::new(2),
                    },
                    ConcurrentOp::ReceiptByRequest { request: req },
                ],
            ];
            let (external, _) = run_multithreaded_round(&coord, &initial, ops);

            // Agent actor 3 must NEVER successfully approve a proposal.
            for entry in &external {
                if matches!(
                    entry.operation,
                    ConcurrentOp::Approve { actor, .. } if actor == ActorId::new(3)
                ) {
                    assert!(
                        matches!(
                            entry.response,
                            ConcurrentResponse::Unit(Err(ConcurrentError::Coordinator(
                                CoordinatorError::Proposal(
                                    ProposalError::NotHuman(_) | ProposalError::ApprovalMismatch
                                ) | CoordinatorError::MissingProposal(_)
                            )))
                        ),
                        "agent approval should fail cleanly, got {:?}",
                        entry.response
                    );
                }
            }
        }
    }

    #[test]
    fn multithreaded_undo_redo_cut_lifecycle_and_compaction_rounds() {
        for round in 0..12u64 {
            let (coord, ids) = seeded_coordinator(&[("a", "abcdef"), ("b", "temp")]);
            let a = ids[0];
            let b = ids[1];
            let actor1 = ActorId::new(1);
            let root_txn = TxnId::new(actor1, Nonce::new(10));
            coord
                .apply(
                    RequestId::new(10),
                    Operation::Edit(edit_txn(
                        1,
                        10,
                        2,
                        vec![(a, splice_replace("abcdef", 0, 2, "AB"))],
                    )),
                )
                .unwrap();

            let snap = coord.snapshot();
            let snap = snap.unwrap();
            let view = BufferView::full(snap.catalog(), a).unwrap();
            let text_a = snap.catalog().get(a).unwrap();
            let selection = Selection::new(
                snap.catalog(),
                &view,
                a,
                text_a.validate_offset(4).unwrap(),
                text_a.validate_offset(6).unwrap(),
            )
            .unwrap();
            let reg_name = RegisterName::new("k").unwrap();

            coord.clear_history().unwrap();
            let initial = coord.checkpoint().unwrap();

            let ops = vec![
                vec![
                    ConcurrentOp::Apply {
                        request: RequestId::new(11),
                        operation: Operation::Undo {
                            actor: actor1,
                            target: root_txn,
                            transaction: TxnId::new(actor1, Nonce::new(11)),
                        },
                    },
                    ConcurrentOp::Apply {
                        request: RequestId::new(12),
                        operation: Operation::Redo {
                            actor: actor1,
                            target: root_txn,
                            transaction: TxnId::new(actor1, Nonce::new(12)),
                        },
                    },
                ],
                vec![
                    ConcurrentOp::Apply {
                        request: RequestId::new(13),
                        operation: Operation::Cut {
                            intent: intent(2, 13, 3),
                            view,
                            selections: vec![selection],
                            name: reg_name.clone(),
                            kind: RegisterKind::Charwise,
                        },
                    },
                    ConcurrentOp::Register {
                        actor: ActorId::new(2),
                        name: reg_name,
                    },
                ],
                vec![
                    ConcurrentOp::Compact {
                        floor: Revision::new(3),
                    },
                    ConcurrentOp::Floor,
                ],
                vec![
                    ConcurrentOp::Apply {
                        request: RequestId::new(14),
                        operation: Operation::Close {
                            intent: intent(2, 14, 3),
                            buffer: b,
                            policy: if round % 2 == 0 {
                                ClosePolicy::RequireClean
                            } else {
                                ClosePolicy::Discard
                            },
                        },
                    },
                    ConcurrentOp::ReadBuffer {
                        actor: ActorId::new(3),
                        buffer: b,
                    },
                ],
                vec![
                    ConcurrentOp::Apply {
                        request: RequestId::new(15),
                        operation: Operation::Open {
                            intent: intent(1, 15, 3),
                            name: format!("extra-{round}"),
                            text: Text::from_str("fresh"),
                        },
                    },
                    ConcurrentOp::ReceiptByRevision {
                        revision: Revision::new(3),
                    },
                ],
            ];
            run_multithreaded_round(&coord, &initial, ops);
        }
    }

    #[test]
    fn multithreaded_access_revocation_and_unauthorized_reads() {
        for round in 0..12u64 {
            let (coord, ids) = seeded_coordinator(&[("secret", "top-secret")]);
            let initial = coord.checkpoint().unwrap();
            let secret = ids[0];
            let target_actor = ActorId::new(3);
            let unknown_actor = ActorId::new(404);

            let ops = vec![
                vec![
                    ConcurrentOp::RevokeActor {
                        actor: target_actor,
                    },
                    ConcurrentOp::ReadBuffer {
                        actor: target_actor,
                        buffer: secret,
                    },
                ],
                vec![
                    ConcurrentOp::ReadBuffer {
                        actor: target_actor,
                        buffer: secret,
                    },
                    ConcurrentOp::ReadSnapshot {
                        actor: target_actor,
                    },
                ],
                vec![
                    ConcurrentOp::UpdateAccess(AccessChange::RevokeDefault(Capability::Read)),
                    ConcurrentOp::UpdateAccess(AccessChange::GrantBuffer(
                        ActorId::new(1),
                        secret,
                        Capability::Read,
                    )),
                ],
                vec![
                    ConcurrentOp::ReadBuffer {
                        actor: ActorId::new(1),
                        buffer: secret,
                    },
                    ConcurrentOp::ReadBuffer {
                        actor: unknown_actor,
                        buffer: secret,
                    },
                ],
                vec![
                    ConcurrentOp::Apply {
                        request: RequestId::new(900 + round),
                        operation: Operation::Edit(edit_txn(
                            3,
                            900 + round,
                            1,
                            vec![(secret, full_replace("top-secret", "leaked"))],
                        )),
                    },
                    ConcurrentOp::RegisterActor {
                        actor: ActorId::new(50),
                        role: ActorRole::Agent,
                    },
                ],
            ];
            let (external, _) = run_multithreaded_round(&coord, &initial, ops);

            // Thread 0 revoked `target_actor` before reading: its post-revoke read MUST fail
            // with InactiveActor(target_actor).
            let thread0_revoke = &external[0];
            let thread0_read = &external[1];
            assert_eq!(
                thread0_revoke.operation,
                ConcurrentOp::RevokeActor {
                    actor: target_actor,
                }
            );
            assert_eq!(thread0_revoke.response, ConcurrentResponse::Unit(Ok(())));
            assert_eq!(
                thread0_read.operation,
                ConcurrentOp::ReadBuffer {
                    actor: target_actor,
                    buffer: secret,
                }
            );
            assert!(thread0_revoke.completed_at < thread0_read.invoked_at);
            assert_eq!(
                thread0_read.response,
                ConcurrentResponse::BufferRead(Err(ConcurrentError::Access(
                    AccessError::InactiveActor(target_actor)
                )))
            );

            // Thread 3 read for `unknown_actor` MUST fail with InactiveActor(unknown_actor).
            let thread3_unknown_read = &external[7];
            assert_eq!(
                thread3_unknown_read.operation,
                ConcurrentOp::ReadBuffer {
                    actor: unknown_actor,
                    buffer: secret,
                }
            );
            assert_eq!(
                thread3_unknown_read.response,
                ConcurrentResponse::BufferRead(Err(ConcurrentError::Access(
                    AccessError::InactiveActor(unknown_actor)
                )))
            );
        }
    }

    #[test]
    fn lock_poisoning_and_panic_safety() {
        let (coord, ids) = seeded_coordinator(&[("doc", "hello")]);
        let doc = ids[0];

        // 1. A panic while holding a shared read guard must NOT leak or poison the RwLock.
        let coord_clone = coord.clone();
        let read_panic = std::thread::spawn(move || {
            let _guard = coord_clone.shared.state.read().unwrap();
            panic!("simulated reader panic");
        })
        .join();
        assert!(read_panic.is_err());
        assert!(!coord.is_poisoned());
        assert_eq!(
            coord
                .read_buffer(ActorId::new(1), doc)
                .unwrap()
                .text()
                .as_str(),
            "hello"
        );
        coord
            .apply(
                RequestId::new(1),
                Operation::Edit(edit_txn(1, 1, 1, vec![(doc, full_replace("hello", "ok"))])),
            )
            .unwrap();
        assert_eq!(coord.revision().unwrap(), Revision::new(2));

        // 2. A panic while holding an exclusive write guard poisons the state lock without
        //    deadlocking; every subsequent operation returns Err(ConcurrentError::LockPoisoned).
        let poisoned = coord.clone();
        let write_panic = std::thread::spawn(move || {
            let _guard = poisoned.shared.state.write().unwrap();
            panic!("simulated writer panic");
        })
        .join();
        assert!(write_panic.is_err());
        assert!(coord.is_poisoned());

        let req = RequestId::new(2);
        let dummy_op = Operation::Edit(edit_txn(1, 2, 2, vec![(doc, full_replace("ok", "next"))]));
        let dummy_prop = Proposal::new(
            RequestId::new(3),
            edit_txn(1, 3, 2, vec![(doc, full_replace("ok", "prop"))]),
        );
        let reg_name = RegisterName::new("a").unwrap();

        assert_eq!(
            coord.apply(req, dummy_op.clone()),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord.propose(dummy_prop.clone()),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord.approve(req, ActorId::new(1)),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord.commit_proposal(req),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord.compact(Revision::new(1)),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord.register_actor(ActorId::new(9), ActorRole::Human),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord.revoke_actor(ActorId::new(1)),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord.update_access(AccessChange::GrantDefault(Capability::Read)),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord.read_buffer(ActorId::new(1), doc),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord.read_buffers(ActorId::new(1), &[doc]),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord.read_snapshot(ActorId::new(1)),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(coord.snapshot(), Err(ConcurrentError::LockPoisoned));
        assert_eq!(coord.revision(), Err(ConcurrentError::LockPoisoned));
        assert_eq!(coord.floor(), Err(ConcurrentError::LockPoisoned));
        assert_eq!(
            coord.receipt_by_request(req),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord.receipt_by_txn(TxnId::new(ActorId::new(1), Nonce::new(1))),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord.receipt_by_revision(Revision::new(1)),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord.journal_range(Revision::ZERO, Revision::new(1)),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord.catch_up(Revision::ZERO),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord.register(ActorId::new(1), &reg_name),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(coord.checkpoint(), Err(ConcurrentError::LockPoisoned));
        assert_eq!(
            coord.is_buffer_dirty(doc),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(coord.buffer_names(), Err(ConcurrentError::LockPoisoned));
        assert_eq!(coord.active_actors(), Err(ConcurrentError::LockPoisoned));
        assert_eq!(
            coord.is_actor_active(ActorId::new(1)),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord.execute(ConcurrentOp::Apply {
                request: req,
                operation: dummy_op,
            }),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord.execute(ConcurrentOp::Propose(dummy_prop)),
            Err(ConcurrentError::LockPoisoned)
        );

        // 3. A poisoned history lock prevents partial writes across all mutating ops and
        //    reports LockPoisoned cleanly.
        let (coord2, ids2) = seeded_coordinator(&[("doc", "intact")]);
        let doc2 = ids2[0];
        let before = coord2.checkpoint().unwrap();
        let panic_res = catch_unwind(AssertUnwindSafe(|| {
            let _h = coord2.shared.history.lock().unwrap();
            panic!("simulated history panic");
        }));
        assert!(panic_res.is_err());
        assert!(coord2.is_poisoned());
        assert_eq!(
            coord2.apply(
                RequestId::new(50),
                Operation::Edit(edit_txn(
                    1,
                    50,
                    1,
                    vec![(doc2, full_replace("intact", "should-not-apply"))],
                )),
            ),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord2.propose(Proposal::new(
                RequestId::new(51),
                edit_txn(
                    3,
                    51,
                    1,
                    vec![(doc2, full_replace("intact", "should-not-propose"))],
                ),
            )),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord2.approve(RequestId::new(51), ActorId::new(1)),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord2.commit_proposal(RequestId::new(51)),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord2.compact(Revision::new(1)),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord2.register_actor(ActorId::new(77), ActorRole::Agent),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord2.revoke_actor(ActorId::new(3)),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord2.update_access(AccessChange::RevokeDefault(Capability::Read)),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(
            coord2.recorded_history(),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(coord2.take_history(), Err(ConcurrentError::LockPoisoned));
        assert_eq!(coord2.clear_history(), Err(ConcurrentError::LockPoisoned));
        assert_eq!(
            coord2.set_recording(false),
            Err(ConcurrentError::LockPoisoned)
        );
        assert_eq!(coord2.is_recording(), Err(ConcurrentError::LockPoisoned));
        // Coordinator state inside `state` was NOT mutated when history lock was poisoned.
        let inner_after = coord2.shared.state.read().unwrap().coordinator.checkpoint();
        assert_eq!(inner_after, before);

        // 4. Poisoning the history lock concurrently while a writer holds the state write lock
        //    and waits on the history lock must still prevent any state mutation.
        let (coord3, ids3) = seeded_coordinator(&[("doc", "untouched")]);
        let doc3 = ids3[0];
        let before3 = coord3.checkpoint().unwrap();
        let barrier = Barrier::new(2);
        let (poisoner_res, writer_res) = std::thread::scope(|scope| {
            let c_poison = coord3.clone();
            let b_poison = &barrier;
            let poisoner = scope.spawn(move || {
                let _history_guard = c_poison.shared.history.lock().unwrap();
                b_poison.wait();
                loop {
                    match c_poison.shared.state.try_read() {
                        Ok(guard) => {
                            drop(guard);
                            std::thread::yield_now();
                        }
                        Err(std::sync::TryLockError::WouldBlock) => break,
                        Err(std::sync::TryLockError::Poisoned(_)) => {
                            panic!("state lock unexpectedly poisoned")
                        }
                    }
                }
                panic!("simulated concurrent history poison");
            });

            let c_write = coord3.clone();
            let b_write = &barrier;
            let writer = scope.spawn(move || {
                b_write.wait();
                c_write.apply(
                    RequestId::new(52),
                    Operation::Edit(edit_txn(
                        1,
                        52,
                        1,
                        vec![(doc3, full_replace("untouched", "corrupted"))],
                    )),
                )
            });

            (poisoner.join(), writer.join().unwrap())
        });
        assert!(poisoner_res.is_err());
        assert_eq!(writer_res, Err(ConcurrentError::LockPoisoned));
        assert!(coord3.is_poisoned());
        let after3 = coord3.shared.state.read().unwrap().coordinator.checkpoint();
        assert_eq!(after3, before3);
    }

    #[test]
    fn overlapping_concurrent_reads_and_writes_record_overlapping_intervals() {
        let (coord, ids) = seeded_coordinator(&[("a", "v0"), ("b", "v0")]);
        let initial = coord.checkpoint().unwrap();
        let a = ids[0];
        let b = ids[1];

        // 1. Hold the state write lock until two writers and two readers have all sampled
        //    `invoked_at`, then release so they execute concurrently and sample `completed_at`
        //    after releasing the state lock.
        let start_clock = coord.shared.clock.load(Ordering::SeqCst);
        std::thread::scope(|scope| {
            let hold_write = coord.shared.state.write().unwrap();

            let c1 = coord.clone();
            let w1 = scope.spawn(move || {
                c1.apply(
                    RequestId::new(101),
                    Operation::Edit(edit_txn(1, 101, 2, vec![(a, full_replace("v0", "v1"))])),
                )
            });

            let c2 = coord.clone();
            let w2 = scope.spawn(move || {
                c2.apply(
                    RequestId::new(102),
                    Operation::Edit(edit_txn(2, 102, 2, vec![(b, full_replace("v0", "v1"))])),
                )
            });

            let c3 = coord.clone();
            let r1 = scope.spawn(move || c3.read_buffer(ActorId::new(3), a));

            let c4 = coord.clone();
            let r2 = scope.spawn(move || c4.read_buffers(ActorId::new(4), &[a, b]));

            while coord.shared.clock.load(Ordering::SeqCst) < start_clock + 4 {
                std::thread::yield_now();
            }
            drop(hold_write);

            w1.join().unwrap().unwrap();
            w2.join().unwrap().unwrap();
            r1.join().unwrap().unwrap();
            r2.join().unwrap().unwrap();
        });

        let history = coord.recorded_history().unwrap();
        let final_snapshot = coord.snapshot().unwrap();
        assert_eq!(history.len(), 4);

        for entry in &history {
            assert!(
                entry.invoked_at < entry.completed_at,
                "invalid interval: {:?}",
                (entry.invoked_at, entry.completed_at)
            );
            assert!(entry.invoked_at < start_clock + 4);
            assert!(entry.completed_at >= start_clock + 4);
        }

        let mut overlapping_read_read = 0usize;
        let mut overlapping_write_write = 0usize;
        let mut overlapping_read_write = 0usize;
        for i in 0..history.len() {
            for j in (i + 1)..history.len() {
                let lhs = &history[i];
                let rhs = &history[j];
                if lhs.invoked_at < rhs.completed_at && rhs.invoked_at < lhs.completed_at {
                    match (lhs.operation.is_read_only(), rhs.operation.is_read_only()) {
                        (true, true) => overlapping_read_read += 1,
                        (false, false) => overlapping_write_write += 1,
                        _ => overlapping_read_write += 1,
                    }
                }
            }
        }
        assert_eq!(overlapping_read_read, 1);
        assert_eq!(overlapping_write_write, 1);
        assert_eq!(overlapping_read_write, 4);

        let witness = find_linearization(&initial, &history)
            .expect("overlapping concurrent history must be linearizable");
        verify_witness(&initial, &history, &witness, Some(&final_snapshot));

        // 2. Readers release the state read lock and sample `completed_at` BEFORE acquiring
        //    the history lock, so holding the history lock never blocks parallel readers or
        //    keeps the state read lock held.
        coord.clear_history().unwrap();
        let read_initial = coord.checkpoint().unwrap();
        let read_start_clock = coord.shared.clock.load(Ordering::SeqCst);
        std::thread::scope(|scope| {
            let hold_history = coord.shared.history.lock().unwrap();

            let c1 = coord.clone();
            let r1 = scope.spawn(move || c1.read_buffer(ActorId::new(3), a));

            let c2 = coord.clone();
            let r2 = scope.spawn(move || c2.read_snapshot(ActorId::new(4)));

            while coord.shared.clock.load(Ordering::SeqCst) < read_start_clock + 4 {
                std::thread::yield_now();
            }
            assert!(
                coord.shared.state.try_write().is_ok(),
                "readers must release state read lock before acquiring history lock"
            );
            drop(hold_history);

            r1.join().unwrap().unwrap();
            r2.join().unwrap().unwrap();
        });

        let read_history = coord.recorded_history().unwrap();
        let read_final = coord.snapshot().unwrap();
        assert_eq!(read_history.len(), 2);
        let read_witness = find_linearization(&read_initial, &read_history)
            .expect("parallel read history must be linearizable");
        verify_witness(
            &read_initial,
            &read_history,
            &read_witness,
            Some(&read_final),
        );
    }

    #[test]
    fn recording_controls_queries_and_checkpoint_restore() {
        let coord = ConcurrentCoordinator::without_recording(default_registry(), full_access());
        assert!(!coord.is_recording().unwrap());
        let open = coord
            .apply(
                RequestId::new(1),
                Operation::Open {
                    intent: intent(1, 1, 0),
                    name: "notes.txt".to_owned(),
                    text: Text::from_str("hello"),
                },
            )
            .unwrap();
        let id = open.buffer.unwrap();
        assert!(coord.recorded_history().unwrap().is_empty());
        assert!(!coord.is_buffer_dirty(id).unwrap());
        assert_eq!(coord.buffer_names().unwrap().get("notes.txt"), Some(&id));
        assert!(coord.is_actor_active(ActorId::new(1)).unwrap());
        assert_eq!(coord.active_actors().unwrap().len(), 4);

        coord.set_recording(true).unwrap();
        assert!(coord.is_recording().unwrap());
        let initial = coord.checkpoint().unwrap();
        let edit_outcome = coord
            .apply(
                RequestId::new(2),
                Operation::Edit(edit_txn(
                    1,
                    2,
                    1,
                    vec![(id, full_replace("hello", "world"))],
                )),
            )
            .unwrap();
        assert!(coord.is_buffer_dirty(id).unwrap());
        assert_eq!(
            coord.receipt_by_request(RequestId::new(2)).unwrap(),
            edit_outcome.receipt
        );
        assert_eq!(
            coord
                .receipt_by_txn(TxnId::new(ActorId::new(1), Nonce::new(2)))
                .unwrap(),
            edit_outcome.receipt
        );
        assert_eq!(
            coord.receipt_by_revision(Revision::new(2)).unwrap(),
            edit_outcome.receipt
        );
        assert_eq!(
            coord
                .journal_range(Revision::new(1), Revision::new(2))
                .unwrap(),
            vec![edit_outcome.receipt.unwrap()]
        );

        let taken = coord.take_history().unwrap();
        assert!(coord.recorded_history().unwrap().is_empty());
        assert!(is_linearizable(&initial, &taken));

        let restored = ConcurrentCoordinator::restore(&coord.checkpoint().unwrap()).unwrap();
        assert_eq!(
            restored
                .read_buffer(ActorId::new(2), id)
                .unwrap()
                .text()
                .as_str(),
            "world"
        );
    }
}
