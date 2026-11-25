//! One revision stream for collaborative buffer lifecycle and text edits.
//!
//! A successful operation publishes exactly one snapshot and receipt. Closed
//! IDs remain tombstoned, and identity checks precede authorization so exact
//! retries return the original receipt even after permissions change.

use std::{collections::BTreeMap, fmt};

use super::{
    access::{AccessError, AccessPolicy, Capability},
    proposal::{Proposal, RequestId},
    rebase::{rebase_over_step, RebaseError, RebaseStep},
    receipt::CommitReceipt,
    registry::ActorRegistry,
};
use crate::editor::{
    edit::{
        ids::{BufferId, TxnId},
        intent::EditIntent,
        transaction::{Transaction, TransactionError},
    },
    text::utf8::Text,
    workspace::{
        buffer_lifecycle::ClosePolicy,
        catalog::BufferCatalog,
        snapshots::{SnapshotStore, VersionedWorkspace, WorkspaceError, WorkspaceSnapshot},
    },
};

/// The complete payload bound to a request and transaction identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    /// Allocate a fresh ID and open a named buffer.
    Open { name: String, text: Text },
    /// Close a live buffer, optionally discarding unsaved edits.
    Close {
        buffer: BufferId,
        policy: ClosePolicy,
    },
    /// Commit an atomic transaction.
    Edit(Transaction),
}

/// One published operation, its exact receipt and edit inverse, if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub request: RequestId,
    pub receipt: CommitReceipt,
    pub operation: Operation,
    pub buffer: Option<BufferId>,
    pub forward: Option<Transaction>,
    inverse: Option<Transaction>,
}

/// A rejected submission; no workspace or history state was changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleError {
    Gone(BufferId),
    Access(AccessError),
    Workspace(WorkspaceError),
    Transaction(TransactionError),
    Rebase(RebaseError),
    IdentityReuse,
    IntentMismatch,
    RevisionMismatch,
    NameInUse,
    Dirty(BufferId),
    IdExhausted,
    NoChange,
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "collaborative lifecycle rejected: {self:?}")
    }
}
impl std::error::Error for LifecycleError {}

/// Owns the sole authoritative workspace, policy and ordered commit log.
#[derive(Debug)]
pub struct CollaborativeLifecycle {
    workspace: VersionedWorkspace,
    snapshots: SnapshotStore,
    actors: ActorRegistry,
    access: AccessPolicy,
    names: BTreeMap<String, BufferId>,
    baseline: BTreeMap<BufferId, Text>,
    next_id: Option<u64>,
    commits: Vec<Commit>,
    requests: BTreeMap<RequestId, usize>,
    transactions: BTreeMap<TxnId, usize>,
}

impl CollaborativeLifecycle {
    /// Creates an empty workspace at revision zero.
    pub fn new(actors: ActorRegistry, access: AccessPolicy) -> Self {
        let workspace = VersionedWorkspace::new(BufferCatalog::new());
        let mut snapshots = SnapshotStore::new();
        snapshots
            .record(workspace.capture())
            .expect("initial snapshot");
        Self {
            workspace,
            snapshots,
            actors,
            access,
            names: BTreeMap::new(),
            baseline: BTreeMap::new(),
            next_id: Some(0),
            commits: Vec::new(),
            requests: BTreeMap::new(),
            transactions: BTreeMap::new(),
        }
    }
    pub fn snapshot(&self) -> WorkspaceSnapshot {
        self.workspace.capture()
    }
    pub fn snapshots(&self) -> &SnapshotStore {
        &self.snapshots
    }
    pub fn commits(&self) -> &[Commit] {
        &self.commits
    }
    pub fn actors_mut(&mut self) -> &mut ActorRegistry {
        &mut self.actors
    }
    pub fn access_mut(&mut self) -> &mut AccessPolicy {
        &mut self.access
    }
    pub fn buffer_named(&self, name: &str) -> Option<BufferId> {
        self.names.get(name).copied()
    }
    /// Compare current content with the clean content captured at open.
    pub fn is_dirty(&self, buffer: BufferId) -> Result<bool, LifecycleError> {
        let text = self
            .workspace
            .catalog()
            .get(buffer)
            .ok_or_else(|| self.missing(buffer))?;
        Ok(self.baseline.get(&buffer) != Some(text))
    }
    fn missing(&self, id: BufferId) -> LifecycleError {
        if self.workspace.catalog().is_tombstoned(id) {
            LifecycleError::Gone(id)
        } else {
            LifecycleError::Access(AccessError::UnknownBuffer(id))
        }
    }
    fn authorize(&self, intent: EditIntent, targets: &[BufferId]) -> Result<(), LifecycleError> {
        for &id in targets {
            if self.workspace.catalog().is_tombstoned(id) {
                return Err(LifecycleError::Gone(id));
            }
        }
        self.access
            .authorize(
                &self.actors,
                self.workspace.catalog(),
                intent.actor(),
                Capability::Commit,
                targets.iter().copied(),
            )
            .map_err(LifecycleError::Access)
    }
    /// Submit a proposal using its request and transaction identities.
    pub fn submit_proposal(&mut self, proposal: Proposal) -> Result<CommitReceipt, LifecycleError> {
        let intent = proposal.transaction().intent();
        self.submit(
            proposal.request(),
            intent,
            Operation::Edit(proposal.transaction().clone()),
        )
    }
    /// Serialize a lifecycle operation or edit against the same revision stream.
    pub fn submit(
        &mut self,
        request: RequestId,
        intent: EditIntent,
        operation: Operation,
    ) -> Result<CommitReceipt, LifecycleError> {
        let by_request = self.requests.get(&request).copied();
        let by_txn = self.transactions.get(&intent.txn()).copied();
        if by_request.is_some() || by_txn.is_some() {
            if let (Some(a), Some(b)) = (by_request, by_txn) {
                let original = &self.commits[a];
                if a == b && original.receipt.intent() == intent && original.operation == operation
                {
                    return Ok(original.receipt);
                }
            }
            if let Operation::Edit(txn) = &operation {
                for (id, _) in txn.patches() {
                    if self.workspace.catalog().is_tombstoned(*id) {
                        return Err(LifecycleError::Gone(*id));
                    }
                }
            }
            return Err(LifecycleError::IdentityReuse);
        }
        if let Operation::Edit(txn) = &operation {
            if txn.intent() != intent {
                return Err(LifecycleError::IntentMismatch);
            }
        }
        let previous = self.workspace.revision();
        let receipt = CommitReceipt::new(intent, previous)
            .map_err(|_| LifecycleError::Workspace(WorkspaceError::RevisionOverflow))?;
        let mut buffer = None;
        let mut forward = None;
        let mut inverse = None;
        match &operation {
            Operation::Open { name, text } => {
                let id = BufferId::new(self.next_id.ok_or(LifecycleError::IdExhausted)?);
                if !self.actors.is_active(intent.actor()) {
                    return Err(LifecycleError::Access(AccessError::InactiveActor(
                        intent.actor(),
                    )));
                }
                if !self
                    .access
                    .is_granted(intent.actor(), id, Capability::Commit)
                {
                    return Err(LifecycleError::Access(AccessError::Denied {
                        actor: intent.actor(),
                        buffer: id,
                        capability: Capability::Commit,
                    }));
                }
                if intent.base() != previous {
                    return Err(LifecycleError::RevisionMismatch);
                }
                if self.names.contains_key(name) {
                    return Err(LifecycleError::NameInUse);
                }
                self.workspace
                    .register(id, text.clone())
                    .map_err(LifecycleError::Workspace)?;
                self.next_id = id.raw().checked_add(1);
                self.names.insert(name.clone(), id);
                self.baseline.insert(id, text.clone());
                buffer = Some(id);
            }
            Operation::Close { buffer: id, policy } => {
                self.authorize(intent, &[*id])?;
                if intent.base() != previous {
                    return Err(LifecycleError::RevisionMismatch);
                }
                if *policy == ClosePolicy::RequireClean && self.is_dirty(*id)? {
                    return Err(LifecycleError::Dirty(*id));
                }
                self.workspace
                    .remove(*id)
                    .map_err(LifecycleError::Workspace)?;
                self.names.retain(|_, value| value != id);
                self.baseline.remove(id);
                buffer = Some(*id);
            }
            Operation::Edit(txn) => {
                let targets: Vec<_> = txn.patches().iter().map(|(id, _)| *id).collect();
                self.authorize(intent, &targets)?;
                let base = self
                    .snapshots
                    .get(intent.base())
                    .ok_or(LifecycleError::RevisionMismatch)?;
                txn.apply_with_inverse(base)
                    .map_err(LifecycleError::Transaction)?;
                let mut rebased = txn.clone();
                for entry in &self.commits {
                    if entry.receipt.previous() < intent.base() {
                        continue;
                    }
                    if let Some(committed) = &entry.forward {
                        rebased = rebase_over_step(
                            &rebased,
                            RebaseStep {
                                before: self
                                    .snapshots
                                    .get(entry.receipt.previous())
                                    .expect("retained snapshot"),
                                committed,
                                after: self
                                    .snapshots
                                    .get(entry.receipt.committed())
                                    .expect("retained snapshot"),
                            },
                        )
                        .map_err(LifecycleError::Rebase)?;
                    } else {
                        rebased = Transaction::new(
                            rebased.intent().with_base(entry.receipt.committed()),
                            rebased.patches().to_vec(),
                        )
                        .map_err(LifecycleError::Transaction)?;
                    }
                }
                let result = rebased
                    .apply_with_inverse(&self.workspace.capture())
                    .map_err(LifecycleError::Transaction)?;
                if result.is_noop() {
                    return Err(LifecycleError::NoChange);
                }
                let updates = targets
                    .iter()
                    .map(|id| {
                        (
                            *id,
                            result.catalog().get(*id).expect("validated target").clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                self.workspace
                    .update_batch(updates)
                    .map_err(LifecycleError::Workspace)?;
                inverse = Some(result.inverse().clone());
                forward = Some(rebased);
            }
        }
        self.snapshots
            .record(self.workspace.capture())
            .expect("successor snapshot");
        let commit = Commit {
            request,
            receipt,
            operation,
            buffer,
            forward,
            inverse,
        };
        let index = self.commits.len();
        self.requests.insert(request, index);
        self.transactions.insert(intent.txn(), index);
        self.commits.push(commit.clone());
        Ok(receipt)
    }
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
        test_support::assert_law,
        text::span::ByteSpan,
        workspace::version::Revision,
    };

    fn intent(n: u64, base: u64) -> EditIntent {
        EditIntent::for_actor(ActorId::new(1), Nonce::new(n), Revision::new(base))
    }
    fn request(n: u64) -> RequestId {
        RequestId::new(n)
    }
    fn edit(n: u64, base: u64, id: BufferId, text: &str) -> Operation {
        let patch = Patch::new(vec![Splice::new(
            ByteSpan::validate_in(&Text::from_str("abc"), 0, 1).unwrap(),
            Text::from_str(text),
        )])
        .unwrap();
        Operation::Edit(Transaction::new(intent(n, base), vec![(id, patch)]).unwrap())
    }
    fn setup() -> CollaborativeLifecycle {
        let mut actors = ActorRegistry::new();
        actors.register(ActorId::new(1), ActorRole::Human).unwrap();
        let mut access = AccessPolicy::new();
        access.grant_default(Capability::Commit);
        CollaborativeLifecycle::new(actors, access)
    }
    #[test]
    fn lifecycle_order_retry_and_tombstone_laws() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| (rng.gen_bool(), rng.next_u64()),
            |&(discard, seed)| {
                let mut s = setup();
                let open = Operation::Open {
                    name: "foo".into(),
                    text: Text::from_str("abc"),
                };
                let first = s.submit(request(1), intent(1, 0), open.clone()).unwrap();
                let id = s.buffer_named("foo").unwrap();
                let stale = edit(9, 1, id, "z");
                let change = edit(2, 1, id, if seed % 2 == 0 { "x" } else { "y" });
                let Operation::Edit(proposal_txn) = change.clone() else {
                    unreachable!()
                };
                let second = s
                    .submit_proposal(Proposal::new(request(2), proposal_txn))
                    .unwrap();
                assert!(s.is_dirty(id).unwrap());
                let baseline_before = s.baseline.clone();
                let before = (s.snapshot(), s.commits().to_vec(), s.snapshots().clone());
                assert_eq!(
                    s.submit(
                        request(3),
                        intent(3, 2),
                        Operation::Close {
                            buffer: id,
                            policy: ClosePolicy::RequireClean
                        }
                    ),
                    Err(LifecycleError::Dirty(id))
                );
                assert_eq!(s.baseline, baseline_before);
                assert_eq!(
                    (s.snapshot(), s.commits().to_vec(), s.snapshots().clone()),
                    before
                );
                let close = Operation::Close {
                    buffer: id,
                    policy: if discard {
                        ClosePolicy::Discard
                    } else {
                        ClosePolicy::RequireClean
                    },
                };
                if !discard {
                    assert!(matches!(
                        s.submit(request(3), intent(3, 2), close),
                        Err(LifecycleError::Dirty(_))
                    ));
                }
                let third = s
                    .submit(
                        request(3),
                        intent(3, 2),
                        Operation::Close {
                            buffer: id,
                            policy: ClosePolicy::Discard,
                        },
                    )
                    .unwrap();
                let closed = (s.snapshot(), s.commits().to_vec(), s.snapshots().clone());
                assert_eq!(
                    s.submit(
                        request(3),
                        intent(3, 2),
                        Operation::Close {
                            buffer: id,
                            policy: ClosePolicy::Discard
                        }
                    )
                    .unwrap(),
                    third
                );
                assert_eq!(
                    (s.snapshot(), s.commits().to_vec(), s.snapshots().clone()),
                    closed
                );
                let Operation::Edit(stale_txn) = stale else {
                    unreachable!()
                };
                assert_eq!(
                    s.submit_proposal(Proposal::new(request(9), stale_txn.clone())),
                    Err(LifecycleError::Gone(id))
                );
                assert_eq!(
                    (s.snapshot(), s.commits().to_vec(), s.snapshots().clone()),
                    closed
                );
                let fourth = s.submit(request(4), intent(4, 3), open.clone()).unwrap();
                let new_id = s.buffer_named("foo").unwrap();
                assert_ne!(id, new_id);
                let dirty_before = s.is_dirty(new_id).unwrap();
                assert!(!dirty_before);
                let saved = (s.snapshot(), s.commits().to_vec(), s.snapshots().clone());
                assert_eq!(
                    s.submit_proposal(Proposal::new(request(9), stale_txn)),
                    Err(LifecycleError::Gone(id))
                );
                assert_eq!(s.is_dirty(new_id).unwrap(), dirty_before);
                assert_eq!(
                    (s.snapshot(), s.commits().to_vec(), s.snapshots().clone()),
                    saved
                );
                assert_eq!(s.snapshot().catalog().get(new_id).unwrap().as_str(), "abc");
                assert_eq!(
                    s.submit(request(1), intent(9, 1), edit(9, 1, id, "z")),
                    Err(LifecycleError::Gone(id))
                );
                assert_eq!(
                    s.submit(request(9), intent(1, 1), edit(1, 1, id, "z")),
                    Err(LifecycleError::Gone(id))
                );
                let at = |revision| {
                    s.snapshots()
                        .get(Revision::new(revision))
                        .unwrap()
                        .catalog()
                };
                assert!(at(0).is_empty());
                assert_eq!(at(1).get(id).unwrap().as_str(), "abc");
                assert_eq!(
                    at(2).get(id).unwrap().as_str(),
                    if seed % 2 == 0 { "xbc" } else { "ybc" }
                );
                assert!(at(3).is_tombstoned(id));
                assert!(at(3).get(id).is_none());
                assert!(at(4).is_tombstoned(id));
                assert_eq!(at(4).get(new_id).unwrap().as_str(), "abc");
                for (i, commit) in [first, second, third, fourth].iter().enumerate() {
                    assert_eq!(commit.previous(), Revision::new(i as u64));
                    assert_eq!(commit.committed(), Revision::new(i as u64 + 1));
                    assert_eq!(
                        s.snapshots().get(commit.committed()).unwrap().revision(),
                        commit.committed()
                    );
                }
                s.access_mut().revoke_default(Capability::Commit);
                assert_eq!(
                    s.submit(request(1), intent(1, 0), open.clone()).unwrap(),
                    first
                );
                assert_eq!(s.submit(request(2), intent(2, 1), change).unwrap(), second);
                assert_eq!(
                    s.submit(
                        request(1),
                        intent(1, 0),
                        Operation::Open {
                            name: "other".into(),
                            text: Text::empty()
                        }
                    ),
                    Err(LifecycleError::IdentityReuse)
                );
                assert!(matches!(
                    s.submit(
                        request(5),
                        intent(5, 4),
                        Operation::Open {
                            name: "other".into(),
                            text: Text::empty()
                        }
                    ),
                    Err(LifecycleError::Access(_))
                ));
                assert!(matches!(
                    s.submit(
                        request(6),
                        intent(6, 4),
                        Operation::Close {
                            buffer: new_id,
                            policy: ClosePolicy::Discard
                        }
                    ),
                    Err(LifecycleError::Access(_))
                ));
                assert_eq!(s.is_dirty(new_id).unwrap(), dirty_before);
                let Operation::Edit(denied) = edit(7, 4, new_id, "q") else {
                    unreachable!()
                };
                assert!(matches!(
                    s.submit_proposal(Proposal::new(request(7), denied)),
                    Err(LifecycleError::Access(_))
                ));
                assert_eq!(s.is_dirty(new_id).unwrap(), dirty_before);
                assert_eq!(
                    s.submit(
                        request(1),
                        intent(8, 4),
                        Operation::Close {
                            buffer: new_id,
                            policy: ClosePolicy::Discard
                        }
                    ),
                    Err(LifecycleError::IdentityReuse)
                );
                assert_eq!(
                    s.submit(
                        request(8),
                        intent(1, 4),
                        Operation::Close {
                            buffer: new_id,
                            policy: ClosePolicy::Discard
                        }
                    ),
                    Err(LifecycleError::IdentityReuse)
                );
                assert_eq!(
                    (s.snapshot(), s.commits().to_vec(), s.snapshots().clone()),
                    saved
                );
                assert_eq!(s.is_dirty(new_id).unwrap(), dirty_before);
            },
        );
    }

    #[test]
    fn gone_preserves_other_dirty_buffer() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| rng.gen_bool(),
            |&choice| {
                let mut s = setup();
                for (n, name) in [(1, "a"), (2, "b")] {
                    s.submit(
                        request(n),
                        intent(n, n - 1),
                        Operation::Open {
                            name: name.into(),
                            text: Text::from_str("abc"),
                        },
                    )
                    .unwrap();
                }
                let a = s.buffer_named("a").unwrap();
                let b = s.buffer_named("b").unwrap();
                let stale = edit(8, 2, a, "z");
                s.submit(
                    request(3),
                    intent(3, 2),
                    edit(3, 2, b, if choice { "x" } else { "y" }),
                )
                .unwrap();
                s.submit(
                    request(4),
                    intent(4, 3),
                    Operation::Close {
                        buffer: a,
                        policy: ClosePolicy::RequireClean,
                    },
                )
                .unwrap();
                let baseline = s.baseline.clone();
                let saved = (s.snapshot(), s.commits().to_vec(), s.snapshots().clone());
                assert!(s.is_dirty(b).unwrap());
                assert_eq!(
                    s.submit(request(8), intent(8, 2), stale),
                    Err(LifecycleError::Gone(a))
                );
                assert_eq!(s.baseline, baseline);
                assert!(s.is_dirty(b).unwrap());
                assert_eq!(
                    (s.snapshot(), s.commits().to_vec(), s.snapshots().clone()),
                    saved
                );
            },
        );
    }
}
