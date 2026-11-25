//! Capability-based authorization for workspace buffers.
//!
//! Grants from the workspace default, an actor, and an actor-buffer pair are
//! additive. Removing a grant removes only that source; another source may
//! still grant the same capability. Roles are never consulted. Authorization
//! requires an active actor and every target to be live and granted the exact
//! operation, including for transactions with multiple targets. Empty target
//! sets require an active actor and otherwise satisfy the conjunction.
//!
//! Guards evaluate against the supplied catalog or snapshot. Callers must use
//! current authoritative state and coordinate authorization with publication;
//! a successful check is not a durable lease.

use std::collections::BTreeSet;
use std::fmt;

use super::registry::ActorRegistry;
use crate::editor::edit::{
    ids::{ActorId, BufferId},
    transaction::{Transaction, TransactionError},
};
use crate::editor::text::utf8::Text;
use crate::editor::workspace::{catalog::BufferCatalog, snapshots::WorkspaceSnapshot};

/// An independently granted operation; no operation implies another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    /// Read buffer text.
    Read,
    /// Propose edits.
    Propose,
    /// Review edits.
    Review,
    /// Commit edits.
    Commit,
}

impl Capability {
    /// All operations in deterministic order.
    pub const ALL: [Self; 4] = [Self::Read, Self::Propose, Self::Review, Self::Commit];
}

/// Authorization or subsequent transaction evaluation failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccessError {
    /// The actor is not active, including permanently revoked actors.
    InactiveActor(ActorId),
    /// The target was never registered.
    UnknownBuffer(BufferId),
    /// The target was removed.
    BufferClosed(BufferId),
    /// No applicable grant allows the requested operation.
    Denied {
        actor: ActorId,
        buffer: BufferId,
        capability: Capability,
    },
    /// Authorized transaction evaluation failed.
    Transaction(TransactionError),
}

impl fmt::Display for AccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InactiveActor(actor) => write!(f, "actor {actor} is not active"),
            Self::UnknownBuffer(buffer) => write!(f, "unknown buffer {buffer}"),
            Self::BufferClosed(buffer) => write!(f, "buffer {buffer} is closed"),
            Self::Denied {
                actor,
                buffer,
                capability,
            } => write!(f, "actor {actor} lacks {capability:?} for buffer {buffer}"),
            Self::Transaction(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for AccessError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transaction(error) => Some(error),
            _ => None,
        }
    }
}

/// Independent capability grants, initially empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccessPolicy {
    defaults: BTreeSet<Capability>,
    actors: BTreeSet<(ActorId, Capability)>,
    buffers: BTreeSet<(ActorId, BufferId, Capability)>,
}

impl AccessPolicy {
    /// Creates a policy with no grants.
    pub fn new() -> Self {
        Self::default()
    }
    /// Grants an operation to every active actor for every live buffer.
    pub fn grant_default(&mut self, capability: Capability) {
        self.defaults.insert(capability);
    }
    /// Removes the workspace-default grant, if present.
    pub fn revoke_default(&mut self, capability: Capability) {
        self.defaults.remove(&capability);
    }
    /// Grants an actor an operation on every live buffer.
    pub fn grant_actor(&mut self, actor: ActorId, capability: Capability) {
        self.actors.insert((actor, capability));
    }
    /// Removes an actor-wide grant, if present.
    pub fn revoke_actor(&mut self, actor: ActorId, capability: Capability) {
        self.actors.remove(&(actor, capability));
    }
    /// Grants an actor an operation on one buffer.
    pub fn grant_buffer(&mut self, actor: ActorId, buffer: BufferId, capability: Capability) {
        self.buffers.insert((actor, buffer, capability));
    }
    /// Removes an actor-buffer grant, if present.
    pub fn revoke_buffer(&mut self, actor: ActorId, buffer: BufferId, capability: Capability) {
        self.buffers.remove(&(actor, buffer, capability));
    }
    /// Returns whether any grant source supplies this capability.
    /// Registration and buffer liveness are checked by authorization, not here.
    pub fn is_granted(&self, actor: ActorId, buffer: BufferId, capability: Capability) -> bool {
        self.defaults.contains(&capability)
            || self.actors.contains(&(actor, capability))
            || self.buffers.contains(&(actor, buffer, capability))
    }
    /// Checks all targets in ascending ID order; duplicates have no effect.
    pub fn authorize(
        &self,
        registry: &ActorRegistry,
        catalog: &BufferCatalog,
        actor: ActorId,
        capability: Capability,
        buffers: impl IntoIterator<Item = BufferId>,
    ) -> Result<(), AccessError> {
        if !registry.is_active(actor) {
            return Err(AccessError::InactiveActor(actor));
        }
        for buffer in buffers.into_iter().collect::<BTreeSet<_>>() {
            if !catalog.contains_live(buffer) {
                return Err(if catalog.is_tombstoned(buffer) {
                    AccessError::BufferClosed(buffer)
                } else {
                    AccessError::UnknownBuffer(buffer)
                });
            }
            if !self.is_granted(actor, buffer, capability) {
                return Err(AccessError::Denied {
                    actor,
                    buffer,
                    capability,
                });
            }
        }
        Ok(())
    }
    /// Checks all targets of a transaction for the actor in its intent.
    pub fn authorize_transaction(
        &self,
        registry: &ActorRegistry,
        snapshot: &WorkspaceSnapshot,
        capability: Capability,
        transaction: &Transaction,
    ) -> Result<(), AccessError> {
        self.authorize(
            registry,
            snapshot.catalog(),
            transaction.intent().actor(),
            capability,
            transaction.patches().iter().map(|(id, _)| *id),
        )
    }
    /// Returns text only after a successful read authorization.
    pub fn read<'a>(
        &self,
        registry: &ActorRegistry,
        catalog: &'a BufferCatalog,
        actor: ActorId,
        buffer: BufferId,
    ) -> Result<&'a Text, AccessError> {
        self.authorize(registry, catalog, actor, Capability::Read, [buffer])?;
        Ok(catalog.get(buffer).expect("authorized live buffer"))
    }
    /// Authorizes and validates a commit without publishing or exposing its catalog.
    pub fn prepare_commit(
        &self,
        registry: &ActorRegistry,
        snapshot: &WorkspaceSnapshot,
        transaction: &Transaction,
    ) -> Result<(), AccessError> {
        self.authorize_transaction(registry, snapshot, Capability::Commit, transaction)?;
        transaction
            .apply_with_inverse(snapshot)
            .map(|_| ())
            .map_err(AccessError::Transaction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::role::ActorRole,
        edit::{ids::Nonce, intent::EditIntent, patch::Patch, splice::Splice},
        test_support::assert_law,
        text::{span::ByteSpan, utf8::Text},
        workspace::{snapshots::VersionedWorkspace, version::Revision},
    };

    fn registry(actor: ActorId, role: ActorRole) -> ActorRegistry {
        let mut registry = ActorRegistry::new();
        registry.register(actor, role).unwrap();
        registry
    }
    fn transaction(actor: ActorId, ids: &[BufferId]) -> Transaction {
        Transaction::new(
            EditIntent::for_actor(actor, Nonce::new(0), Revision::ZERO),
            ids.iter().map(|&id| (id, Patch::default())).collect(),
        )
        .unwrap()
    }
    fn catalog() -> BufferCatalog {
        let mut catalog = BufferCatalog::new();
        for id in 0..3 {
            catalog
                .register(BufferId::new(id), Text::from_str("text"))
                .unwrap();
        }
        catalog.remove(BufferId::new(2)).unwrap();
        catalog
    }

    #[test]
    fn exhaustive_conjunction_narrowing_and_role_neutrality() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| ActorId::new(rng.next_u64()),
            |&actor| {
                let human = registry(actor, ActorRole::Human);
                let agent = registry(actor, ActorRole::Agent);
                let mut revoked = human.clone();
                revoked.revoke(actor).unwrap();
                let unknown = ActorRegistry::new();
                let catalog = catalog();
                let snapshot = VersionedWorkspace::new(catalog.clone()).capture();
                let ids = [
                    BufferId::new(0),
                    BufferId::new(1),
                    BufferId::new(2),
                    BufferId::new(3),
                ];
                for capability in Capability::ALL {
                    // Exhaust all combinations of default, actor and the two live buffer grants.
                    for grants in 0..16 {
                        let mut policy = AccessPolicy::new();
                        if grants & 1 != 0 {
                            policy.grant_default(capability);
                        }
                        if grants & 2 != 0 {
                            policy.grant_actor(actor, capability);
                        }
                        for (i, &id) in ids.iter().take(2).enumerate() {
                            if grants & (4 << i) != 0 {
                                policy.grant_buffer(actor, id, capability);
                            }
                        }
                        for targets in 0..16 {
                            let selected: Vec<_> = ids
                                .iter()
                                .enumerate()
                                .filter_map(|(i, &id)| (targets & (1 << i) != 0).then_some(id))
                                .collect();
                            let expected = selected.iter().all(|&id| {
                                catalog.contains_live(id)
                                    && (grants & 3 != 0
                                        || (id.raw() < 2 && grants & (4 << id.raw()) != 0))
                            });
                            let check = |p: &AccessPolicy, r: &ActorRegistry| {
                                p.authorize(
                                    r,
                                    &catalog,
                                    actor,
                                    capability,
                                    selected.iter().copied(),
                                )
                                .is_ok()
                            };
                            assert_eq!(
                                check(&policy, &human),
                                expected,
                                "grants {grants}, targets {targets}, {capability:?}"
                            );
                            assert_eq!(check(&policy, &agent), expected);
                            assert!(!check(&policy, &unknown));
                            assert!(!check(&policy, &revoked));
                            let txn = transaction(actor, &selected);
                            assert!(policy
                                .authorize_transaction(&unknown, &snapshot, capability, &txn)
                                .is_err());
                            assert!(policy
                                .authorize_transaction(&revoked, &snapshot, capability, &txn)
                                .is_err());
                            assert_eq!(
                                policy
                                    .authorize_transaction(&human, &snapshot, capability, &txn)
                                    .is_ok(),
                                expected
                            );
                            assert_eq!(
                                policy
                                    .authorize_transaction(&agent, &snapshot, capability, &txn)
                                    .is_ok(),
                                expected
                            );
                            for source in 0..4 {
                                let mut narrowed = policy.clone();
                                match source {
                                    0 => narrowed.revoke_default(capability),
                                    1 => narrowed.revoke_actor(actor, capability),
                                    _ => narrowed.revoke_buffer(actor, ids[source - 2], capability),
                                }
                                for operation in Capability::ALL {
                                    let before = policy
                                        .authorize(
                                            &human,
                                            &catalog,
                                            actor,
                                            operation,
                                            selected.iter().copied(),
                                        )
                                        .is_ok();
                                    let after = narrowed
                                        .authorize(
                                            &human,
                                            &catalog,
                                            actor,
                                            operation,
                                            selected.iter().copied(),
                                        )
                                        .is_ok();
                                    assert!(!after || before);
                                    let before_txn = policy
                                        .authorize_transaction(&human, &snapshot, operation, &txn)
                                        .is_ok();
                                    let after_txn = narrowed
                                        .authorize_transaction(&human, &snapshot, operation, &txn)
                                        .is_ok();
                                    assert!(!after_txn || before_txn);
                                }
                                assert!(!check(&narrowed, &human) || expected);
                                assert!(
                                    narrowed
                                        .authorize_transaction(&human, &snapshot, capability, &txn)
                                        .is_err()
                                        || expected
                                );
                            }
                        }
                    }
                }
            },
        );
    }

    #[test]
    fn mixed_capability_grants_remain_monotone() {
        let actor = ActorId::new(1);
        let registry = registry(actor, ActorRole::Human);
        let catalog = catalog();
        let snapshot = VersionedWorkspace::new(catalog.clone()).capture();
        let ids = [BufferId::new(0), BufferId::new(1)];
        for retained in Capability::ALL {
            for removed in Capability::ALL {
                for source in 0..4 {
                    let mut policy = AccessPolicy::new();
                    policy.grant_default(retained);
                    match source {
                        0 => policy.grant_default(removed),
                        1 => policy.grant_actor(actor, removed),
                        _ => policy.grant_buffer(actor, ids[source - 2], removed),
                    }
                    let before = policy.clone();
                    match source {
                        0 => policy.revoke_default(removed),
                        1 => policy.revoke_actor(actor, removed),
                        _ => policy.revoke_buffer(actor, ids[source - 2], removed),
                    }
                    for operation in Capability::ALL {
                        for targets in [vec![], vec![ids[0]], vec![ids[1]], ids.to_vec()] {
                            let txn = transaction(actor, &targets);
                            let allowed = |p: &AccessPolicy| {
                                p.authorize(
                                    &registry,
                                    &catalog,
                                    actor,
                                    operation,
                                    targets.iter().copied(),
                                )
                                .is_ok()
                            };
                            let allowed_txn = |p: &AccessPolicy| {
                                p.authorize_transaction(&registry, &snapshot, operation, &txn)
                                    .is_ok()
                            };
                            assert!(!allowed(&policy) || allowed(&before));
                            assert!(!allowed_txn(&policy) || allowed_txn(&before));
                            if retained != removed && operation == retained {
                                assert!(allowed(&before) && allowed(&policy));
                                assert!(allowed_txn(&before) && allowed_txn(&policy));
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn guards_and_error_order_are_atomic() {
        let actor = ActorId::new(5);
        let registry = registry(actor, ActorRole::Human);
        let catalog = catalog();
        let snapshot = VersionedWorkspace::new(catalog.clone()).capture();
        let mut policy = AccessPolicy::new();
        for capability in Capability::ALL {
            policy.grant_actor(actor, capability);
        }
        assert_eq!(
            policy
                .read(&registry, &catalog, actor, BufferId::new(0))
                .unwrap()
                .as_str(),
            "text"
        );
        assert_eq!(
            policy.read(&registry, &catalog, actor, BufferId::new(2)),
            Err(AccessError::BufferClosed(BufferId::new(2)))
        );
        assert_eq!(
            policy.authorize(
                &registry,
                &catalog,
                actor,
                Capability::Read,
                [BufferId::new(3), BufferId::new(2)]
            ),
            Err(AccessError::BufferClosed(BufferId::new(2)))
        );
        let patch = Patch::new(vec![Splice::new(
            ByteSpan::full_of(&Text::from_str("text")),
            Text::from_str("changed"),
        )])
        .unwrap();
        let txn = Transaction::new(
            EditIntent::for_actor(actor, Nonce::new(0), Revision::ZERO),
            vec![(BufferId::new(0), patch.clone()), (BufferId::new(1), patch)],
        )
        .unwrap();
        assert_eq!(policy.prepare_commit(&registry, &snapshot, &txn), Ok(()));
        policy.revoke_actor(actor, Capability::Read);
        assert_eq!(
            policy.read(&registry, &catalog, actor, BufferId::new(0)),
            Err(AccessError::Denied {
                actor,
                buffer: BufferId::new(0),
                capability: Capability::Read
            })
        );
        assert_eq!(policy.prepare_commit(&registry, &snapshot, &txn), Ok(()));
        policy.revoke_actor(actor, Capability::Commit);
        policy.grant_buffer(actor, BufferId::new(0), Capability::Commit);
        assert_eq!(
            policy.prepare_commit(&registry, &snapshot, &txn),
            Err(AccessError::Denied {
                actor,
                buffer: BufferId::new(1),
                capability: Capability::Commit
            })
        );
        assert_eq!(snapshot.catalog(), &catalog);
        let stale = Transaction::new(
            EditIntent::for_actor(actor, Nonce::new(1), Revision::new(1)),
            vec![],
        )
        .unwrap();
        assert!(matches!(
            policy.prepare_commit(&registry, &snapshot, &stale),
            Err(AccessError::Transaction(
                TransactionError::RevisionMismatch { .. }
            ))
        ));
        assert_eq!(
            AccessError::UnknownBuffer(BufferId::new(3)).to_string(),
            "unknown buffer 3"
        );
    }

    #[test]
    fn grants_are_independent_and_removal_is_idempotent() {
        let actor = ActorId::new(1);
        let buffer = BufferId::new(0);
        let mut policy = AccessPolicy::new();
        let other = ActorId::new(2);
        let mut registry = registry(actor, ActorRole::Human);
        registry.register(other, ActorRole::Agent).unwrap();
        policy.grant_actor(other, Capability::Read);
        policy.grant_buffer(other, buffer, Capability::Commit);
        assert!(policy
            .authorize(&registry, &catalog(), actor, Capability::Read, [buffer])
            .is_err());
        assert!(policy
            .authorize(&registry, &catalog(), actor, Capability::Commit, [buffer])
            .is_err());
        policy.revoke_actor(other, Capability::Read);
        policy.revoke_buffer(other, buffer, Capability::Commit);
        policy.grant_default(Capability::Read);
        policy.grant_actor(actor, Capability::Read);
        policy.grant_buffer(actor, buffer, Capability::Read);
        policy.revoke_default(Capability::Read);
        policy.revoke_actor(actor, Capability::Read);
        assert!(policy.is_granted(actor, buffer, Capability::Read));
        assert!(!policy.is_granted(actor, buffer, Capability::Commit));
        policy.revoke_buffer(actor, buffer, Capability::Read);
        policy.revoke_buffer(actor, buffer, Capability::Read);
        assert_eq!(policy, AccessPolicy::new());
    }
}
