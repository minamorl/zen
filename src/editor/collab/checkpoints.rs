//! Durable collaborative checkpoint state.

use super::{
    delivery::{DeliveryError, DeliveryLog},
    journal::{CommitJournal, JournalError},
    proposal::RequestId,
    rebase::RebaseError,
    receipt::CommitReceipt,
    registry::ActorRegistry,
    retention::{RetainedHistory, RetentionError},
};
use crate::editor::{
    edit::{
        history::EditHistory,
        ids::{BufferId, TxnId},
        intent::EditIntent,
        transaction::{Transaction, TransactionError},
    },
    text::utf8::Text,
    workspace::{
        buffer_lifecycle::ClosePolicy,
        catalog::BufferCatalog,
        dirty::{DirtyTracker, SaveReceipt},
        events::WorkspaceEvent,
        snapshots::{VersionedWorkspace, WorkspaceError, WorkspaceSnapshot},
        version::Revision,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckpointError {
    InconsistentState,
    InactiveActor,
    Gone(BufferId),
    UnknownBuffer(BufferId),
    NameInUse,
    Dirty(BufferId),
    IdExhausted,
    BaseExpired { base: Revision, floor: Revision },
    FutureRevision { revision: Revision, tip: Revision },
    RequestIdReuse(RequestId),
    TxnIdReuse(TxnId),
    Delivery(DeliveryError),
    Retention(RetentionError),
    Workspace(WorkspaceError),
    Transaction(TransactionError),
    Rebase(RebaseError),
    Journal(JournalError),
}
impl fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "checkpoint operation failed: {self:?}")
    }
}
impl std::error::Error for CheckpointError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckpointOp {
    Open {
        request: RequestId,
        intent: EditIntent,
        name: String,
        text: Text,
    },
    Close {
        request: RequestId,
        intent: EditIntent,
        buffer: BufferId,
        policy: ClosePolicy,
    },
    Edit {
        request: RequestId,
        txn: Transaction,
    },
    MarkSaved {
        buffer: BufferId,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointOutcome {
    pub receipt: Option<CommitReceipt>,
    pub buffer: Option<BufferId>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct RetryRecord {
    request: RequestId,
    txn_id: TxnId,
    receipt: CommitReceipt,
    buffer: Option<BufferId>,
    op_digest: [u8; 32],
    applied_digest: Option<[u8; 32]>,
}

#[derive(Debug)]
pub struct CheckpointState {
    workspace: VersionedWorkspace,
    actors: ActorRegistry,
    names: BTreeMap<String, BufferId>,
    dirty: BTreeMap<BufferId, DirtyTracker>,
    next_id: Option<u64>,
    floor: Revision,
    journal: CommitJournal,
    edit_history: EditHistory,
    history: RetainedHistory,
    delivery: DeliveryLog,
    retained_edits: Vec<(RequestId, Transaction, Transaction, CommitReceipt)>,
    retry_by_request: BTreeMap<RequestId, RetryRecord>,
    retry_by_txn: BTreeMap<TxnId, RequestId>,
}

impl CheckpointState {
    pub fn new(actors: ActorRegistry) -> Self {
        Self::from_catalog(BufferCatalog::new(), actors)
    }
    pub fn from_catalog(catalog: BufferCatalog, actors: ActorRegistry) -> Self {
        let mut names = BTreeMap::new();
        let mut dirty = BTreeMap::new();
        for id in catalog.live_ids() {
            names.insert(id.raw().to_string(), id);
            let mut tracker = DirtyTracker::new();
            tracker
                .acknowledge(SaveReceipt {
                    buffer: id,
                    version: Revision::ZERO,
                    content: catalog.get(id).unwrap().clone(),
                })
                .unwrap();
            dirty.insert(id, tracker);
        }
        let next_id = catalog
            .live_ids()
            .into_iter()
            .chain(catalog.tombstoned_ids())
            .map(|id| id.raw())
            .max()
            .map_or(Some(0), |id| id.checked_add(1));
        let workspace = VersionedWorkspace::new(catalog);
        let snapshot = workspace.capture();
        Self {
            workspace,
            actors,
            names,
            dirty,
            next_id,
            floor: Revision::ZERO,
            journal: CommitJournal::default(),
            edit_history: EditHistory::new(),
            history: RetainedHistory::new(snapshot.clone()),
            delivery: DeliveryLog::new(snapshot),
            retained_edits: Vec::new(),
            retry_by_request: BTreeMap::new(),
            retry_by_txn: BTreeMap::new(),
        }
    }
    pub fn snapshot(&self) -> WorkspaceSnapshot {
        self.workspace.capture()
    }
    pub fn revision(&self) -> Revision {
        self.workspace.revision()
    }
    pub fn floor(&self) -> Revision {
        self.floor
    }
    pub fn actors(&self) -> &ActorRegistry {
        &self.actors
    }
    pub fn actors_mut(&mut self) -> &mut ActorRegistry {
        &mut self.actors
    }
    pub fn names(&self) -> &BTreeMap<String, BufferId> {
        &self.names
    }
    pub fn is_dirty(&self, id: BufferId) -> Result<bool, CheckpointError> {
        let text = self.live(id)?;
        Ok(self
            .dirty
            .get(&id)
            .ok_or(CheckpointError::InconsistentState)?
            .is_dirty(id, text))
    }
    pub fn journal(&self) -> &CommitJournal {
        &self.journal
    }
    pub fn history(&self) -> &RetainedHistory {
        &self.history
    }
    pub fn delivery(&self) -> &DeliveryLog {
        &self.delivery
    }
    pub fn receipt_by_request(&self, request: RequestId) -> Option<CommitReceipt> {
        self.retry_by_request.get(&request).map(|r| r.receipt)
    }
    pub fn receipt_by_txn(&self, txn: TxnId) -> Option<CommitReceipt> {
        self.retry_by_txn
            .get(&txn)
            .and_then(|r| self.receipt_by_request(*r))
    }
    fn live(&self, id: BufferId) -> Result<&Text, CheckpointError> {
        self.workspace.catalog().get(id).ok_or_else(|| {
            if self.workspace.catalog().is_tombstoned(id) {
                CheckpointError::Gone(id)
            } else {
                CheckpointError::UnknownBuffer(id)
            }
        })
    }
    fn validate_intent(&self, intent: EditIntent) -> Result<(), CheckpointError> {
        if !self.actors.is_active(intent.actor()) {
            return Err(CheckpointError::InactiveActor);
        }
        if intent.base() < self.floor {
            return Err(CheckpointError::BaseExpired {
                base: intent.base(),
                floor: self.floor,
            });
        }
        if intent.base() > self.revision() {
            return Err(CheckpointError::FutureRevision {
                revision: intent.base(),
                tip: self.revision(),
            });
        }
        Ok(())
    }
    pub fn apply(&mut self, op: CheckpointOp) -> Result<CheckpointOutcome, CheckpointError> {
        if let CheckpointOp::MarkSaved { buffer } = op {
            let content = self.live(buffer)?.clone();
            let version = self.revision();
            self.dirty
                .get_mut(&buffer)
                .ok_or(CheckpointError::InconsistentState)?
                .acknowledge(SaveReceipt {
                    buffer,
                    version,
                    content,
                })
                .map_err(|_| CheckpointError::InconsistentState)?;
            return Ok(CheckpointOutcome {
                receipt: None,
                buffer: Some(buffer),
            });
        }
        let (request, intent) = match &op {
            CheckpointOp::Open {
                request, intent, ..
            }
            | CheckpointOp::Close {
                request, intent, ..
            } => (*request, *intent),
            CheckpointOp::Edit { request, txn } => (*request, txn.intent()),
            CheckpointOp::MarkSaved { .. } => unreachable!(),
        };
        let digest = op_digest(&op);
        if let Some(record) = self.retry_by_request.get(&request) {
            if record.txn_id == intent.txn() && record.op_digest == digest {
                return Ok(CheckpointOutcome {
                    receipt: Some(record.receipt),
                    buffer: record.buffer,
                });
            }
            return Err(CheckpointError::RequestIdReuse(request));
        }
        if self.retry_by_txn.contains_key(&intent.txn()) {
            return Err(CheckpointError::TxnIdReuse(intent.txn()));
        }
        self.validate_intent(intent)?;
        let before = self.snapshot();
        let receipt = CommitReceipt::new(intent, self.revision())
            .map_err(|_| CheckpointError::Workspace(WorkspaceError::RevisionOverflow))?;
        let mut journal = self.journal.clone();
        journal.append(receipt).map_err(CheckpointError::Journal)?;
        let mut staged = VersionedWorkspace::resume_from_snapshot(&before);
        let mut names = self.names.clone();
        let mut dirty = self.dirty.clone();
        let mut next_id = self.next_id;
        let mut edit_history = self.edit_history.clone();
        let mut history = self.history.clone();
        let mut delivery = self.delivery.clone();
        let mut prepared_edit = None;
        let buffer = match &op {
            CheckpointOp::Open { name, text, .. } => {
                if names.contains_key(name) {
                    return Err(CheckpointError::NameInUse);
                }
                let id = BufferId::new(next_id.ok_or(CheckpointError::IdExhausted)?);
                if staged.catalog().contains_live(id) || staged.catalog().is_tombstoned(id) {
                    return Err(CheckpointError::IdExhausted);
                }
                staged
                    .register(id, text.clone())
                    .map_err(CheckpointError::Workspace)?;
                let mut tracker = DirtyTracker::new();
                tracker
                    .acknowledge(SaveReceipt {
                        buffer: id,
                        version: receipt.committed(),
                        content: text.clone(),
                    })
                    .map_err(|_| CheckpointError::InconsistentState)?;
                names.insert(name.clone(), id);
                dirty.insert(id, tracker);
                next_id = id.raw().checked_add(1);
                edit_history = EditHistory::new();
                Some(id)
            }
            CheckpointOp::Close { buffer, policy, .. } => {
                self.live(*buffer)?;
                if *policy == ClosePolicy::RequireClean && self.is_dirty(*buffer)? {
                    return Err(CheckpointError::Dirty(*buffer));
                }
                staged.remove(*buffer).map_err(CheckpointError::Workspace)?;
                names.retain(|_, id| id != buffer);
                dirty.remove(buffer);
                edit_history = EditHistory::new();
                Some(*buffer)
            }
            CheckpointOp::Edit { txn, .. } => {
                if intent.base() < history.floor() {
                    return Err(CheckpointError::BaseExpired {
                        base: intent.base(),
                        floor: history.floor(),
                    });
                }
                let applied = if intent.base() < self.revision() {
                    history.rebase(txn).map_err(|e| match e {
                        RetentionError::Rebase(e) => CheckpointError::Rebase(e),
                        other => CheckpointError::Retention(other),
                    })?
                } else {
                    txn.clone()
                };
                let result = applied
                    .apply_with_inverse(&before)
                    .map_err(CheckpointError::Transaction)?;
                edit_history
                    .record(
                        &before,
                        receipt.committed(),
                        result.catalog(),
                        applied.clone(),
                    )
                    .map_err(|_| CheckpointError::InconsistentState)?;
                let updates = applied
                    .patches()
                    .iter()
                    .map(|(id, _)| {
                        (
                            *id,
                            result.catalog().get(*id).expect("validated buffer").clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                staged
                    .update_batch(updates.clone())
                    .map_err(CheckpointError::Workspace)?;
                if history.tip() != before.revision()
                    || delivery.tip_snapshot() != &before
                    || history.rebase(txn).map_err(CheckpointError::Retention)? != applied
                {
                    return Err(CheckpointError::InconsistentState);
                }
                prepared_edit = Some((txn.clone(), applied, updates));
                None
            }
            CheckpointOp::MarkSaved { .. } => unreachable!(),
        };
        let applied_digest = prepared_edit
            .as_ref()
            .map(|(_, applied, _)| transaction_digest(applied));
        match &op {
            CheckpointOp::Open { text, .. } => self
                .workspace
                .register(buffer.expect("open buffer"), text.clone())
                .expect("validated open"),
            CheckpointOp::Close { .. } => {
                self.workspace
                    .remove(buffer.expect("close buffer"))
                    .expect("validated close");
            }
            CheckpointOp::Edit { .. } => {
                let (txn, applied, updates) = prepared_edit.expect("prepared edit");
                self.workspace
                    .update_batch(updates)
                    .expect("validated batch");
                let after = self.workspace.capture();
                history
                    .record(
                        request,
                        txn.clone(),
                        applied.clone(),
                        receipt,
                        after.clone(),
                    )
                    .expect("validated history");
                delivery
                    .record(
                        &history,
                        request,
                        txn.clone(),
                        WorkspaceEvent::from_committed(receipt, applied.clone()),
                        after,
                    )
                    .expect("validated delivery");
                self.retained_edits.push((request, txn, applied, receipt));
            }
            CheckpointOp::MarkSaved { .. } => unreachable!(),
        }
        if !matches!(op, CheckpointOp::Edit { .. }) {
            self.retained_edits.clear();
            let after = self.workspace.capture();
            history = RetainedHistory::new(after.clone());
            delivery = DeliveryLog::new(after);
        }
        self.names = names;
        self.dirty = dirty;
        self.next_id = next_id;
        self.journal = journal;
        self.edit_history = edit_history;
        self.history = history;
        self.delivery = delivery;
        self.retained_edits
            .retain(|(_, _, _, receipt)| receipt.committed() > self.history.floor());
        self.retry_by_request.insert(
            request,
            RetryRecord {
                request,
                txn_id: intent.txn(),
                receipt,
                buffer,
                op_digest: digest,
                applied_digest,
            },
        );
        self.retry_by_txn.insert(intent.txn(), request);
        Ok(CheckpointOutcome {
            receipt: Some(receipt),
            buffer,
        })
    }
    pub fn replay(
        &mut self,
        ops: &[CheckpointOp],
    ) -> Result<Vec<CheckpointOutcome>, CheckpointError> {
        let mut outcomes = Vec::new();
        for op in ops {
            outcomes.push(self.apply(op.clone())?);
        }
        Ok(outcomes)
    }
    pub fn compact(&mut self, floor: Revision) -> Result<(), CheckpointError> {
        if floor > self.revision() {
            return Err(CheckpointError::FutureRevision {
                revision: floor,
                tip: self.revision(),
            });
        }
        let floor = self.floor.max(floor);
        let retained_floor = floor.max(self.history.floor());
        let mut history = self.history.clone();
        let mut delivery = self.delivery.clone();
        history
            .compact(retained_floor)
            .map_err(CheckpointError::Retention)?;
        delivery
            .compact(retained_floor)
            .map_err(CheckpointError::Delivery)?;
        self.floor = floor;
        self.history = history;
        self.delivery = delivery;
        self.retained_edits
            .retain(|(_, _, _, receipt)| receipt.committed() > self.history.floor());
        Ok(())
    }
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            snapshot: self.snapshot(),
            actors: self.actors.clone(),
            names: self.names.clone(),
            dirty: self.dirty.clone(),
            next_id: self.next_id,
            floor: self.floor,
            journal: self.journal.clone(),
            retained_floor: self.history.base_snapshot().clone(),
            retained_edits: self.retained_edits.clone(),
            retry_by_request: self.retry_by_request.clone(),
            retry_by_txn: self.retry_by_txn.clone(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    snapshot: WorkspaceSnapshot,
    actors: ActorRegistry,
    names: BTreeMap<String, BufferId>,
    dirty: BTreeMap<BufferId, DirtyTracker>,
    next_id: Option<u64>,
    floor: Revision,
    journal: CommitJournal,
    retained_floor: WorkspaceSnapshot,
    retained_edits: Vec<(RequestId, Transaction, Transaction, CommitReceipt)>,
    retry_by_request: BTreeMap<RequestId, RetryRecord>,
    retry_by_txn: BTreeMap<TxnId, RequestId>,
}
impl Checkpoint {
    pub fn restore(&self) -> Result<CheckpointState, CheckpointError> {
        let catalog = self.snapshot.catalog();
        let live: BTreeSet<_> = catalog.live_ids().into_iter().collect();
        let named: BTreeSet<_> = self.names.values().copied().collect();
        let tracked: BTreeSet<_> = self.dirty.keys().copied().collect();
        let max = live
            .iter()
            .copied()
            .chain(catalog.tombstoned_ids())
            .map(|id| id.raw())
            .max();
        if live != named
            || named.len() != self.names.len()
            || live != tracked
            || match (self.next_id, max) {
                (Some(next), Some(max)) => next <= max,
                (None, Some(max)) => max != u64::MAX,
                (None, None) => true,
                _ => false,
            }
            || self.floor > self.snapshot.revision()
            || self.journal.tip() != self.snapshot.revision()
            || self.retry_by_request.len() != self.retry_by_txn.len()
            || self.retry_by_request.len() != self.journal.len()
            || self.retry_by_request.iter().any(|(request, record)| {
                record.request != *request
                    || record.txn_id != record.receipt.txn()
                    || record.receipt.committed() > self.snapshot.revision()
                    || self.retry_by_txn.get(&record.txn_id) != Some(request)
                    || self.journal.lookup_txn(record.txn_id) != Some(&record.receipt)
            })
        {
            return Err(CheckpointError::InconsistentState);
        }
        if self.retained_floor.revision() < self.floor
            || self.retained_floor.revision() > self.snapshot.revision()
        {
            return Err(CheckpointError::InconsistentState);
        }
        let mut workspace = VersionedWorkspace::resume_from_snapshot(&self.retained_floor);
        let base = workspace.capture();
        let mut history = RetainedHistory::new(base.clone());
        let mut delivery = DeliveryLog::new(base);
        let mut edit_history = EditHistory::new();
        for (request, original, applied, receipt) in &self.retained_edits {
            let before = workspace.capture();
            if self.retry_by_request.get(request).is_none_or(|record| {
                record.receipt != *receipt
                    || record.txn_id != original.intent().txn()
                    || record.buffer.is_some()
                    || record.applied_digest != Some(transaction_digest(applied))
                    || record.op_digest
                        != op_digest(&CheckpointOp::Edit {
                            request: *request,
                            txn: original.clone(),
                        })
            }) {
                return Err(CheckpointError::InconsistentState);
            }
            let result = applied
                .apply_with_inverse(&before)
                .map_err(|_| CheckpointError::InconsistentState)?;
            edit_history
                .record(
                    &before,
                    receipt.committed(),
                    result.catalog(),
                    applied.clone(),
                )
                .map_err(|_| CheckpointError::InconsistentState)?;
            let updates = applied
                .patches()
                .iter()
                .map(|(id, _)| result.catalog().get(*id).cloned().map(|text| (*id, text)))
                .collect::<Option<Vec<_>>>()
                .ok_or(CheckpointError::InconsistentState)?;
            workspace
                .update_batch(updates)
                .map_err(|_| CheckpointError::InconsistentState)?;
            let after = workspace.capture();
            history
                .record_restored(
                    *request,
                    original.clone(),
                    applied.clone(),
                    *receipt,
                    after.clone(),
                )
                .map_err(|_| CheckpointError::InconsistentState)?;
            delivery
                .record(
                    &history,
                    *request,
                    original.clone(),
                    WorkspaceEvent::from_committed(*receipt, applied.clone()),
                    after,
                )
                .map_err(|_| CheckpointError::InconsistentState)?;
        }
        if workspace.revision() != self.snapshot.revision()
            || workspace.catalog() != self.snapshot.catalog()
        {
            return Err(CheckpointError::InconsistentState);
        }
        Ok(CheckpointState {
            workspace,
            actors: self.actors.clone(),
            names: self.names.clone(),
            dirty: self.dirty.clone(),
            next_id: self.next_id,
            floor: self.floor,
            journal: self.journal.clone(),
            edit_history,
            history,
            delivery,
            retained_edits: self.retained_edits.clone(),
            retry_by_request: self.retry_by_request.clone(),
            retry_by_txn: self.retry_by_txn.clone(),
        })
    }
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_len(bytes: &mut Vec<u8>, value: usize) {
    put_u64(bytes, u64::try_from(value).expect("length fits u64"));
}

fn transaction_digest(txn: &Transaction) -> [u8; 32] {
    let mut bytes = b"zen:retention:txn:v1\0".to_vec();
    let intent = txn.intent();
    put_u64(&mut bytes, intent.actor().raw());
    put_u64(&mut bytes, intent.nonce().raw());
    put_u64(&mut bytes, intent.base().raw());
    put_u64(&mut bytes, intent.txn().actor().raw());
    put_u64(&mut bytes, intent.txn().nonce().raw());
    put_len(&mut bytes, txn.patches().len());
    for (id, patch) in txn.patches() {
        put_u64(&mut bytes, id.raw());
        put_len(&mut bytes, patch.splices().len());
        for splice in patch.splices() {
            put_len(&mut bytes, splice.span().start().as_usize());
            put_len(&mut bytes, splice.span().end().as_usize());
            let replacement = splice.replacement().as_bytes();
            put_len(&mut bytes, replacement.len());
            bytes.extend_from_slice(replacement);
        }
    }
    sha256(bytes)
}

fn op_digest(op: &CheckpointOp) -> [u8; 32] {
    let mut bytes = b"zen:checkpoint:op:v1\0".to_vec();
    let (request, intent) = match op {
        CheckpointOp::Open {
            request, intent, ..
        } => {
            bytes.push(0);
            (*request, *intent)
        }
        CheckpointOp::Close {
            request, intent, ..
        } => {
            bytes.push(1);
            (*request, *intent)
        }
        CheckpointOp::Edit { request, txn } => {
            bytes.push(2);
            (*request, txn.intent())
        }
        CheckpointOp::MarkSaved { .. } => unreachable!(),
    };
    put_u64(&mut bytes, request.raw());
    put_u64(&mut bytes, intent.actor().raw());
    put_u64(&mut bytes, intent.nonce().raw());
    put_u64(&mut bytes, intent.base().raw());
    match op {
        CheckpointOp::Open { name, text, .. } => {
            for value in [name.as_bytes(), text.as_bytes()] {
                put_len(&mut bytes, value.len());
                bytes.extend_from_slice(value);
            }
        }
        CheckpointOp::Close { buffer, policy, .. } => {
            put_u64(&mut bytes, buffer.raw());
            bytes.push(match policy {
                ClosePolicy::RequireClean => 0,
                ClosePolicy::Discard => 1,
            });
        }
        CheckpointOp::Edit { txn, .. } => {
            put_len(&mut bytes, txn.patches().len());
            for (id, patch) in txn.patches() {
                put_u64(&mut bytes, id.raw());
                put_len(&mut bytes, patch.splices().len());
                for splice in patch.splices() {
                    put_len(&mut bytes, splice.span().start().as_usize());
                    put_len(&mut bytes, splice.span().end().as_usize());
                    let value = splice.replacement().as_bytes();
                    put_len(&mut bytes, value.len());
                    bytes.extend_from_slice(value);
                }
            }
        }
        CheckpointOp::MarkSaved { .. } => unreachable!(),
    }
    sha256(bytes)
}

fn sha256(mut bytes: Vec<u8>) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    bytes.push(0x80);
    while bytes.len() % 64 != 56 {
        bytes.push(0);
    }
    bytes.extend_from_slice(&bit_len.to_be_bytes());
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    for block in bytes.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (word, chunk) in w.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*chunk);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (value, add) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *value = value.wrapping_add(add);
        }
    }
    let mut digest = [0; 32];
    for (chunk, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        *chunk = word.to_be_bytes();
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::role::ActorRole,
        edit::{
            ids::{ActorId, Nonce},
            patch::Patch,
            splice::Splice,
        },
        text::span::ByteSpan,
    };

    #[test]
    fn checkpoint_retry_and_lifecycle() {
        let actor = ActorId::new(1);
        let mut actors = ActorRegistry::new();
        actors.register(actor, ActorRole::Human).unwrap();
        let mut state = CheckpointState::new(actors);
        let intent =
            |nonce, base| EditIntent::for_actor(actor, Nonce::new(nonce), Revision::new(base));
        let opened = state
            .apply(CheckpointOp::Open {
                request: RequestId::new(1),
                intent: intent(1, 0),
                name: "a".into(),
                text: Text::from_str("old"),
            })
            .unwrap();
        let id = opened.buffer.unwrap();
        let patch = Patch::new(vec![Splice::new(
            ByteSpan::full_of(&Text::from_str("old")),
            Text::from_str("new"),
        )])
        .unwrap();
        let edit = CheckpointOp::Edit {
            request: RequestId::new(2),
            txn: Transaction::new(intent(2, 1), vec![(id, patch)]).unwrap(),
        };
        let outcome = state.apply(edit.clone()).unwrap();
        assert_eq!(state.delivery().events().len(), 1);
        state.checkpoint().restore().unwrap();
        state.compact(state.revision()).unwrap();
        let mut restored = state.checkpoint().restore().unwrap();
        assert_eq!(restored.apply(edit.clone()).unwrap(), outcome);
        let conflict = CheckpointOp::Edit {
            request: RequestId::new(2),
            txn: Transaction::new(intent(2, 1), vec![]).unwrap(),
        };
        assert_eq!(
            restored.apply(conflict),
            Err(CheckpointError::RequestIdReuse(RequestId::new(2)))
        );
        restored
            .apply(CheckpointOp::Close {
                request: RequestId::new(3),
                intent: intent(3, 2),
                buffer: id,
                policy: ClosePolicy::Discard,
            })
            .unwrap();
        let reopened = restored
            .apply(CheckpointOp::Open {
                request: RequestId::new(4),
                intent: intent(4, 3),
                name: "a".into(),
                text: Text::empty(),
            })
            .unwrap();
        assert_ne!(reopened.buffer, Some(id));
        assert!(restored
            .snapshot()
            .catalog()
            .contains_live(reopened.buffer.unwrap()));
    }

    #[test]
    fn restore_after_compacting_before_a_stale_edit() {
        let actor = ActorId::new(1);
        let mut actors = ActorRegistry::new();
        actors.register(actor, ActorRole::Human).unwrap();
        let mut state = CheckpointState::new(actors);
        let intent =
            |nonce, base| EditIntent::for_actor(actor, Nonce::new(nonce), Revision::new(base));
        let id = state
            .apply(CheckpointOp::Open {
                request: RequestId::new(1),
                intent: intent(1, 0),
                name: "a".into(),
                text: Text::from_str("abcd"),
            })
            .unwrap()
            .buffer
            .unwrap();
        let text = Text::from_str("abcd");
        for (nonce, start, end, replacement) in [(2, 0, 1, "A"), (3, 3, 4, "D")] {
            let patch = Patch::new(vec![Splice::new(
                ByteSpan::validate_in(&text, start, end).unwrap(),
                Text::from_str(replacement),
            )])
            .unwrap();
            state
                .apply(CheckpointOp::Edit {
                    request: RequestId::new(nonce),
                    txn: Transaction::new(intent(nonce, 1), vec![(id, patch)]).unwrap(),
                })
                .unwrap();
        }
        state.compact(Revision::new(2)).unwrap();
        assert!(state.checkpoint().restore().is_ok());
        let mut missing = state.checkpoint();
        let record = missing.retry_by_request.remove(&RequestId::new(2)).unwrap();
        missing.retry_by_txn.remove(&record.txn_id);
        assert!(matches!(
            missing.restore(),
            Err(CheckpointError::InconsistentState)
        ));
        let mut tampered = state.checkpoint();
        let applied = &mut tampered.retained_edits[0].2;
        let patch = Patch::new(vec![Splice::new(
            ByteSpan::validate_in(&Text::from_str("Abcd"), 0, 4).unwrap(),
            Text::from_str("AbcD"),
        )])
        .unwrap();
        *applied = Transaction::new(applied.intent(), vec![(id, patch)]).unwrap();
        assert!(matches!(
            tampered.restore(),
            Err(CheckpointError::InconsistentState)
        ));
    }

    fn setup() -> CheckpointState {
        let mut actors = ActorRegistry::new();
        actors.register(ActorId::new(1), ActorRole::Human).unwrap();
        CheckpointState::new(actors)
    }

    fn intent(n: u64, base: Revision) -> EditIntent {
        EditIntent::for_actor(ActorId::new(1), Nonce::new(n), base)
    }

    fn open(n: u64, base: Revision, name: &str, text: &str) -> CheckpointOp {
        CheckpointOp::Open {
            request: RequestId::new(n),
            intent: intent(n, base),
            name: name.into(),
            text: Text::from_str(text),
        }
    }

    fn edit(
        n: u64,
        base: Revision,
        id: BufferId,
        text: &str,
        start: usize,
        end: usize,
        replacement: &str,
    ) -> CheckpointOp {
        let patch = Patch::new(vec![Splice::new(
            ByteSpan::validate_in(&Text::from_str(text), start, end).unwrap(),
            Text::from_str(replacement),
        )])
        .unwrap();
        CheckpointOp::Edit {
            request: RequestId::new(n),
            txn: Transaction::new(intent(n, base), vec![(id, patch)]).unwrap(),
        }
    }

    fn close(n: u64, base: Revision, id: BufferId, policy: ClosePolicy) -> CheckpointOp {
        CheckpointOp::Close {
            request: RequestId::new(n),
            intent: intent(n, base),
            buffer: id,
            policy,
        }
    }

    fn equivalent(a: &CheckpointState, b: &CheckpointState) {
        assert_eq!(a.snapshot().revision(), b.snapshot().revision());
        assert_eq!(a.snapshot().catalog(), b.snapshot().catalog());
        assert_eq!(a.names(), b.names());
        for id in a.snapshot().catalog().live_ids() {
            assert_eq!(a.is_dirty(id), b.is_dirty(id));
        }
    }

    #[test]
    fn checkpoint_forks_follow_the_same_operations() {
        crate::editor::test_support::assert_law(
            &[0, 1, 2, 42, 99, u64::MAX],
            |rng| {
                (
                    rng.next_u64(),
                    (0..8).map(|_| rng.gen_bool()).collect::<Vec<_>>(),
                )
            },
            |(seed, choices)| {
                let saved = choices[0];
                for mode in 0..3 {
                    let mut a = setup();
                    let id = a
                        .apply(open(1, a.revision(), "a", "abcd"))
                        .unwrap()
                        .buffer
                        .unwrap();
                    let base = a.revision();
                    let first = edit(
                        2,
                        base,
                        id,
                        "abcd",
                        0,
                        1,
                        if seed & 1 == 0 { "A" } else { "Z" },
                    );
                    let first_outcome = a.apply(first.clone()).unwrap();
                    let stale = edit(3, base, id, "abcd", 3, 4, "D");
                    a.apply(stale.clone()).unwrap();
                    if mode == 1 {
                        a.compact(Revision::new(2)).unwrap();
                    }
                    if mode == 2 {
                        a.compact(a.revision()).unwrap();
                    }
                    let cp = a.checkpoint();
                    let mut b = cp.restore().unwrap();
                    equivalent(&a, &b);
                    assert_eq!(a.apply(first.clone()), Ok(first_outcome.clone()));
                    assert_eq!(b.apply(first.clone()), Ok(first_outcome));
                    let conflict = edit(2, base, id, "abcd", 0, 1, "Q");
                    assert_eq!(a.apply(conflict.clone()), b.apply(conflict));
                    let current = a.snapshot().catalog().get(id).unwrap().clone();
                    let current_text = std::str::from_utf8(current.as_bytes()).unwrap();
                    let op = edit(4, a.revision(), id, current_text, 1, 2, "B");
                    assert_eq!(a.apply(op.clone()), b.apply(op));
                    if saved {
                        let op = CheckpointOp::MarkSaved { buffer: id };
                        assert_eq!(a.apply(op.clone()), b.apply(op));
                    }
                    let op = close(
                        5,
                        a.revision(),
                        id,
                        if saved {
                            ClosePolicy::RequireClean
                        } else {
                            ClosePolicy::Discard
                        },
                    );
                    assert_eq!(a.apply(op.clone()), b.apply(op));
                    let op = open(6, a.revision(), "a", "again");
                    let result = a.apply(op.clone());
                    assert_eq!(result, b.apply(op));
                    assert_ne!(result.unwrap().buffer, Some(id));
                    assert_eq!(
                        a.apply(CheckpointOp::MarkSaved { buffer: id }),
                        Err(CheckpointError::Gone(id))
                    );
                    assert_eq!(
                        b.apply(CheckpointOp::MarkSaved { buffer: id }),
                        Err(CheckpointError::Gone(id))
                    );
                    equivalent(&a, &b);
                    for (i, save) in choices.iter().copied().enumerate() {
                        let n = 100 + i as u64 * 4;
                        let op = open(n, a.revision(), "b", "wxyz");
                        let result = a.apply(op.clone()).unwrap();
                        assert_eq!(b.apply(op), Ok(result.clone()));
                        let fresh = result.buffer.unwrap();
                        let base = a.revision();
                        let op = edit(n + 1, base, fresh, "wxyz", 0, 1, "W");
                        assert_eq!(a.apply(op.clone()), b.apply(op));
                        let op = edit(n + 2, base, fresh, "wxyz", 3, 4, "Z");
                        assert_eq!(a.apply(op.clone()), b.apply(op));
                        if save {
                            let op = CheckpointOp::MarkSaved { buffer: fresh };
                            assert_eq!(a.apply(op.clone()), b.apply(op));
                        }
                        let floor = if save { a.revision() } else { base };
                        assert_eq!(a.compact(floor), b.compact(floor));
                        let op = close(
                            n + 3,
                            a.revision(),
                            fresh,
                            if save {
                                ClosePolicy::RequireClean
                            } else {
                                ClosePolicy::Discard
                            },
                        );
                        assert_eq!(a.apply(op.clone()), b.apply(op));
                        equivalent(&a, &b);
                    }
                }
            },
        );
    }

    #[test]
    fn stale_edits_cross_checkpoint_after_lifecycle() {
        crate::editor::test_support::assert_law(
            &[0, 1, 2, 42, 99, u64::MAX],
            |rng| rng.gen_bool(),
            |choice| {
                let mut continuous = setup();
                let old = continuous
                    .apply(open(1, continuous.revision(), "a", "abcd"))
                    .unwrap()
                    .buffer
                    .unwrap();
                continuous
                    .apply(edit(2, continuous.revision(), old, "abcd", 0, 1, "A"))
                    .unwrap();
                continuous
                    .apply(CheckpointOp::MarkSaved { buffer: old })
                    .unwrap();
                continuous
                    .apply(close(
                        3,
                        continuous.revision(),
                        old,
                        ClosePolicy::RequireClean,
                    ))
                    .unwrap();
                let id = continuous
                    .apply(open(4, continuous.revision(), "a", "abcdef"))
                    .unwrap()
                    .buffer
                    .unwrap();
                let base = continuous.revision();
                continuous
                    .apply(edit(
                        5,
                        base,
                        id,
                        "abcdef",
                        0,
                        1,
                        if *choice { "A" } else { "Z" },
                    ))
                    .unwrap();
                continuous
                    .apply(edit(6, base, id, "abcdef", 5, 6, "F"))
                    .unwrap();
                assert!(!continuous.delivery().events().is_empty());
                let mut restored = continuous.checkpoint().restore().unwrap();
                let tail = [edit(7, base, id, "abcdef", 2, 3, "C")];
                let continuous_outcomes = continuous.replay(&tail).unwrap();
                let restored_outcomes = restored.replay(&tail).unwrap();
                assert_eq!(continuous_outcomes, restored_outcomes);
                assert_eq!(
                    restored.snapshot().catalog().get(id).unwrap().as_str(),
                    if *choice { "AbCdeF" } else { "ZbCdeF" }
                );
                equivalent(&continuous, &restored);
                assert!(continuous.snapshot().catalog().is_tombstoned(old));
                for state in [&mut continuous, &mut restored] {
                    assert_eq!(
                        state.apply(CheckpointOp::MarkSaved { buffer: old }),
                        Err(CheckpointError::Gone(old))
                    );
                    let op = edit(8, state.revision(), old, "abcd", 0, 1, "x");
                    assert_eq!(
                        state.apply(op),
                        Err(CheckpointError::Transaction(
                            TransactionError::BufferClosed(old)
                        ))
                    );
                }
                equivalent(&continuous, &restored);
            },
        );
    }

    #[test]
    fn compacted_retries_do_not_keep_payloads_or_publish() {
        let mut s = setup();
        let opened = open(1, s.revision(), "a", "abcd");
        let opened_result = s.apply(opened.clone()).unwrap();
        let old = opened_result.buffer.unwrap();
        let closed = close(4, s.revision(), old, ClosePolicy::Discard);
        let closed_result = s.apply(closed.clone()).unwrap();
        let id = s
            .apply(open(5, s.revision(), "a", "abcd"))
            .unwrap()
            .buffer
            .unwrap();
        let marker = "distinctive-marker-61729";
        let changed = edit(2, s.revision(), id, "abcd", 0, 4, marker);
        let changed_result = s.apply(changed.clone()).unwrap();
        let overwrite = edit(3, s.revision(), id, marker, 0, marker.len(), "done");
        s.apply(overwrite).unwrap();
        assert!(!s.delivery().events().is_empty());
        s.compact(s.revision()).unwrap();
        assert!(s.delivery().events().is_empty());
        let cp = s.checkpoint();
        assert!(!format!("{cp:?}").contains(marker));
        let mut restored = cp.restore().unwrap();
        restored.actors_mut().revoke(ActorId::new(1)).unwrap();
        let before = restored.checkpoint();
        for (op, expected) in [
            (opened, opened_result),
            (changed, changed_result),
            (closed, closed_result),
        ] {
            assert_eq!(restored.apply(op), Ok(expected));
            assert_eq!(restored.checkpoint(), before);
            assert!(restored.delivery().events().is_empty());
        }
        let conflict = open(1, Revision::ZERO, "other", "x");
        assert_eq!(
            restored.apply(conflict),
            Err(CheckpointError::RequestIdReuse(RequestId::new(1)))
        );
        let conflict = CheckpointOp::Open {
            request: RequestId::new(99),
            intent: intent(1, Revision::ZERO),
            name: "other".into(),
            text: Text::empty(),
        };
        assert_eq!(
            restored.apply(conflict),
            Err(CheckpointError::TxnIdReuse(intent(1, Revision::ZERO).txn()))
        );
        assert_eq!(restored.checkpoint(), before);
    }

    #[test]
    fn rejected_operations_are_atomic_and_restored_origin_is_isolated() {
        use crate::editor::workspace::snapshots::{SnapshotError, SnapshotStore};
        let mut s = setup();
        let id = s
            .apply(open(1, s.revision(), "a", "abcd"))
            .unwrap()
            .buffer
            .unwrap();
        let changed = edit(2, s.revision(), id, "abcd", 0, 1, "A");
        s.apply(changed).unwrap();
        let mut restored = s.checkpoint().restore().unwrap();
        let mut store = SnapshotStore::new();
        store.record(s.snapshot()).unwrap();
        assert_eq!(
            store.record(restored.snapshot()),
            Err(SnapshotError::DifferentWorkspace)
        );
        for state in [&mut s, &mut restored] {
            let before = state.checkpoint();
            let events = state.delivery().events().to_vec();
            let rev = state.revision();
            let unknown = BufferId::new(999);
            let mut inactive = open(11, rev, "b", "x");
            if let CheckpointOp::Open { intent, .. } = &mut inactive {
                *intent = EditIntent::for_actor(ActorId::new(99), Nonce::new(11), rev);
            }
            let failures = [
                (open(10, rev, "a", "x"), CheckpointError::NameInUse),
                (inactive, CheckpointError::InactiveActor),
                (
                    close(12, rev, id, ClosePolicy::RequireClean),
                    CheckpointError::Dirty(id),
                ),
                (
                    close(13, rev, unknown, ClosePolicy::Discard),
                    CheckpointError::UnknownBuffer(unknown),
                ),
                (
                    open(2, rev, "b", "x"),
                    CheckpointError::RequestIdReuse(RequestId::new(2)),
                ),
                (
                    open(14, Revision::new(rev.raw() + 1), "b", "x"),
                    CheckpointError::FutureRevision {
                        revision: Revision::new(rev.raw() + 1),
                        tip: rev,
                    },
                ),
            ];
            for (op, error) in failures {
                assert_eq!(state.apply(op), Err(error));
                assert_eq!(state.checkpoint(), before);
                assert_eq!(state.delivery().events(), events);
            }
            assert_eq!(
                state.compact(Revision::new(rev.raw() + 1)),
                Err(CheckpointError::FutureRevision {
                    revision: Revision::new(rev.raw() + 1),
                    tip: rev
                })
            );
            assert_eq!(state.checkpoint(), before);
            state
                .apply(close(15, rev, id, ClosePolicy::Discard))
                .unwrap();
            state.compact(state.revision()).unwrap();
            let before = state.checkpoint();
            let rev = state.revision();
            let failures = [
                (
                    close(16, rev, id, ClosePolicy::Discard),
                    CheckpointError::Gone(id),
                ),
                (
                    CheckpointOp::MarkSaved { buffer: id },
                    CheckpointError::Gone(id),
                ),
                (
                    edit(17, rev, id, "abcd", 0, 1, "X"),
                    CheckpointError::Transaction(TransactionError::BufferClosed(id)),
                ),
                (
                    edit(18, Revision::ZERO, id, "abcd", 0, 1, "X"),
                    CheckpointError::BaseExpired {
                        base: Revision::ZERO,
                        floor: rev,
                    },
                ),
                (
                    edit(19, Revision::new(rev.raw() + 1), id, "abcd", 0, 1, "X"),
                    CheckpointError::FutureRevision {
                        revision: Revision::new(rev.raw() + 1),
                        tip: rev,
                    },
                ),
            ];
            for (op, error) in failures {
                assert_eq!(state.apply(op), Err(error));
                assert_eq!(state.checkpoint(), before);
                assert!(state.delivery().events().is_empty());
            }
        }
    }
}
