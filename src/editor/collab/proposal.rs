//! Reviewable transactions with explicit, human approval.
//!
//! Preview is pure. Approval binds the request, canonical transaction patches,
//! revision and reviewer. Commit rechecks that binding, current permissions and
//! revision before submitting, so the sequencer cannot rebase reviewed edits.

use std::fmt;

use super::{
    access::{AccessError, AccessPolicy, Capability},
    receipt::SubmitOutcome,
    registry::ActorRegistry,
    sequencer::Sequencer,
};
use crate::editor::{
    edit::{
        ids::{ActorId, BufferId},
        patch::Patch,
        transaction::{Transaction, TransactionError, TransactionResult},
    },
    workspace::{snapshots::WorkspaceSnapshot, version::Revision},
};

/// Identifies a review request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(u64);

impl RequestId {
    /// Creates an identifier.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
    /// Returns the underlying value.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// A review request and its canonical transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    request: RequestId,
    transaction: Transaction,
}

/// Evidence of a specific human review. Its fields cannot be changed by callers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval {
    request: RequestId,
    patches: Vec<(BufferId, Patch)>,
    revision: Revision,
    approver: ActorId,
    snapshot: WorkspaceSnapshot,
}

/// A proposal could not be previewed, approved or submitted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProposalError {
    /// Access control rejected an operation.
    Access(AccessError),
    /// The edit cannot be evaluated against the snapshot.
    Transaction(TransactionError),
    /// The reviewer is not a registered human.
    NotHuman(ActorId),
    /// An empty target set cannot be authorized for review or commit.
    EmptyProposal,
    /// The supplied approval is for different content or reviewer.
    ApprovalMismatch,
    /// The workspace has moved since review.
    RevisionMismatch {
        reviewed: Revision,
        actual: Revision,
    },
}

impl fmt::Display for ProposalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Access(error) => error.fmt(f),
            Self::Transaction(error) => error.fmt(f),
            Self::NotHuman(actor) => write!(f, "actor {actor} is not a human reviewer"),
            Self::EmptyProposal => f.write_str("proposal has no target buffers"),
            Self::ApprovalMismatch => f.write_str("approval does not match proposal or reviewer"),
            Self::RevisionMismatch { reviewed, actual } => write!(
                f,
                "reviewed revision {reviewed} does not match current revision {actual}"
            ),
        }
    }
}
impl std::error::Error for ProposalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Access(error) => Some(error),
            Self::Transaction(error) => Some(error),
            _ => None,
        }
    }
}

impl Proposal {
    /// Wraps a transaction, whose intent identifies its proposer and reviewed revision.
    pub fn new(request: RequestId, transaction: Transaction) -> Self {
        Self {
            request,
            transaction,
        }
    }
    /// Returns the request identifier.
    pub fn request(&self) -> RequestId {
        self.request
    }
    /// Returns the canonical transaction.
    pub fn transaction(&self) -> &Transaction {
        &self.transaction
    }
    /// Evaluates the edit without changing the snapshot or workspace.
    pub fn preview(
        &self,
        snapshot: &WorkspaceSnapshot,
    ) -> Result<TransactionResult, ProposalError> {
        self.transaction
            .apply_with_inverse(snapshot)
            .map_err(ProposalError::Transaction)
    }
    /// Checks review permission and validity before recording a human approval.
    pub fn approve(
        &self,
        snapshot: &WorkspaceSnapshot,
        registry: &ActorRegistry,
        policy: &AccessPolicy,
        approver: ActorId,
    ) -> Result<Approval, ProposalError> {
        if self.transaction.patches().is_empty() {
            return Err(ProposalError::EmptyProposal);
        }
        policy
            .authorize(
                registry,
                snapshot.catalog(),
                approver,
                Capability::Review,
                self.transaction.patches().iter().map(|(id, _)| *id),
            )
            .map_err(ProposalError::Access)?;
        if !registry
            .role_of(approver)
            .is_some_and(|role| role.is_human())
        {
            return Err(ProposalError::NotHuman(approver));
        }
        self.preview(snapshot)?;
        Ok(Approval {
            request: self.request,
            patches: self.transaction.patches().to_vec(),
            revision: snapshot.revision(),
            approver,
            snapshot: snapshot.clone(),
        })
    }
    /// Rechecks the binding and both actors' current permissions before submission.
    /// `approver` is the identity expected by the caller, not one derived from the approval.
    pub fn commit(
        &self,
        approval: &Approval,
        approver: ActorId,
        policy: &AccessPolicy,
        sequencer: &mut Sequencer,
    ) -> Result<SubmitOutcome, ProposalError> {
        if approval.request != self.request
            || approval.patches != self.transaction.patches()
            || approval.revision != self.transaction.intent().base()
            || approval.approver != approver
        {
            return Err(ProposalError::ApprovalMismatch);
        }
        let snapshot = sequencer.snapshot();
        if snapshot.revision() != approval.revision {
            return Err(ProposalError::RevisionMismatch {
                reviewed: approval.revision,
                actual: snapshot.revision(),
            });
        }
        if snapshot != approval.snapshot {
            return Err(ProposalError::ApprovalMismatch);
        }
        let registry = sequencer.actors();
        policy
            .authorize(
                registry,
                snapshot.catalog(),
                approver,
                Capability::Review,
                self.transaction.patches().iter().map(|(id, _)| *id),
            )
            .map_err(ProposalError::Access)?;
        if !registry
            .role_of(approver)
            .is_some_and(|role| role.is_human())
        {
            return Err(ProposalError::NotHuman(approver));
        }
        policy
            .prepare_commit(registry, &snapshot, &self.transaction)
            .map_err(ProposalError::Access)?;
        Ok(sequencer.submit(self.transaction.clone()))
    }
}

impl Approval {
    /// Returns the reviewer whose identity was bound at approval time.
    pub fn approver(&self) -> ActorId {
        self.approver
    }
    /// Returns the reviewed revision.
    pub fn revision(&self) -> Revision {
        self.revision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::role::ActorRole,
        edit::{ids::Nonce, intent::EditIntent, splice::Splice},
        test_support::{assert_law, texts},
        text::{span::ByteSpan, utf8::Text},
        workspace::catalog::BufferCatalog,
    };

    #[test]
    fn proposal_laws() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for text in corpus {
                let proposer = ActorId::new(1);
                let reviewer = ActorId::new(2);
                let other = ActorId::new(3);
                let buffer = BufferId::new(1);
                let mut actors = ActorRegistry::new();
                actors.register(proposer, ActorRole::Agent).unwrap();
                actors.register(reviewer, ActorRole::Human).unwrap();
                actors.register(other, ActorRole::Agent).unwrap();
                let mut catalog = BufferCatalog::new();
                catalog.register(buffer, Text::from_str(text)).unwrap();
                let mut sequencer = Sequencer::new(catalog.clone(), actors.clone());
                let mut direct = Sequencer::new(catalog, actors.clone());
                let patch = Patch::new(vec![Splice::new(
                    ByteSpan::validate_in(&Text::from_str(text), 0, 0).unwrap(),
                    Text::from_str("x"),
                )])
                .unwrap();
                let txn = Transaction::new(
                    EditIntent::for_actor(proposer, Nonce::new(1), Revision::ZERO),
                    vec![(buffer, patch)],
                )
                .unwrap();
                let proposal = Proposal::new(RequestId::new(7), txn.clone());
                let mut policy = AccessPolicy::new();
                policy.grant_actor(reviewer, Capability::Review);
                policy.grant_actor(proposer, Capability::Commit);
                let before = sequencer.snapshot();
                let empty = Proposal::new(
                    RequestId::new(9),
                    Transaction::new(txn.intent(), vec![]).unwrap(),
                );
                assert_eq!(
                    empty.approve(&before, &actors, &AccessPolicy::new(), reviewer),
                    Err(ProposalError::EmptyProposal)
                );
                let preview = proposal.preview(&before).unwrap();
                assert_eq!(sequencer.snapshot(), before);
                let approval = proposal
                    .approve(&before, &actors, &policy, reviewer)
                    .unwrap();
                let foreign_before = direct.snapshot();
                assert_eq!(
                    proposal.commit(&approval, reviewer, &policy, &mut direct),
                    Err(ProposalError::ApprovalMismatch)
                );
                assert_eq!(direct.snapshot(), foreign_before);
                let mut tampered = approval.clone();
                tampered.request = RequestId::new(8);
                let mut variants = vec![tampered];
                let mut tampered = approval.clone();
                tampered.patches[0].1 = Patch::default();
                variants.push(tampered);
                let mut tampered = approval.clone();
                tampered.revision = Revision::new(1);
                variants.push(tampered);
                let mut tampered = approval.clone();
                tampered.approver = other;
                variants.push(tampered);
                for invalid in variants {
                    assert_eq!(
                        proposal.commit(&invalid, reviewer, &policy, &mut sequencer),
                        Err(ProposalError::ApprovalMismatch)
                    );
                    assert_eq!(sequencer.snapshot(), before);
                }
                assert!(matches!(
                    proposal.approve(&before, &actors, &policy, other),
                    Err(ProposalError::Access(_))
                ));
                let mut agent_policy = policy.clone();
                agent_policy.grant_actor(other, Capability::Review);
                assert_eq!(
                    proposal.approve(&before, &actors, &agent_policy, other),
                    Err(ProposalError::NotHuman(other))
                );
                let mut denied = policy.clone();
                denied.revoke_actor(reviewer, Capability::Review);
                assert!(matches!(
                    proposal.commit(&approval, reviewer, &denied, &mut sequencer),
                    Err(ProposalError::Access(_))
                ));
                denied = policy.clone();
                denied.revoke_actor(proposer, Capability::Commit);
                assert!(matches!(
                    proposal.commit(&approval, reviewer, &denied, &mut sequencer),
                    Err(ProposalError::Access(_))
                ));
                assert_eq!(sequencer.snapshot(), before);
                assert!(proposal
                    .commit(&approval, reviewer, &policy, &mut sequencer)
                    .unwrap()
                    .is_committed());
                assert!(direct.submit(txn).is_committed());
                assert_eq!(sequencer.snapshot().catalog(), direct.snapshot().catalog());
                assert_eq!(sequencer.snapshot().catalog(), preview.catalog());
                assert_eq!(sequencer.revision(), direct.revision());
                assert!(matches!(
                    proposal.commit(&approval, reviewer, &policy, &mut sequencer),
                    Err(ProposalError::RevisionMismatch { .. })
                ));
            }
        });
    }
}
