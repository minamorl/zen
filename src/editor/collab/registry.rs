//! Collaboration participant registration.
//!
//! # Laws
//!
//! * **Commutativity:** operations on distinct actors commute, including their
//!   results. Each operation reads and changes only the entries for its actor,
//!   so neither operation can affect the other's result or changes.
//! * **Idempotence:** registering an active actor again with the same role
//!   leaves the state unchanged. When the role matches, registration returns
//!   `Ok(())` before insertion, without changing either collection.
//! * **Revocation finality:** a revoked actor has no role and cannot be
//!   registered again. Revocation removes its active mapping and records its
//!   identity permanently; registration checks that record before inserting.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::editor::collab::role::ActorRole;
use crate::editor::edit::ids::ActorId;

/// An unsuccessful registry operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistryError {
    /// An active actor was registered with a different role.
    RoleConflict {
        /// The actor whose role conflicts.
        actor: ActorId,
        /// The actor's current role.
        existing: ActorRole,
        /// The role requested by the caller.
        requested: ActorRole,
    },
    /// The actor was already revoked.
    ActorRevoked(ActorId),
    /// The actor has never been registered.
    UnknownActor(ActorId),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RoleConflict {
                actor,
                existing,
                requested,
            } => {
                write!(f, "actor {actor} has role {existing}, not {requested}")
            }
            Self::ActorRevoked(actor) => write!(f, "actor {actor} is revoked"),
            Self::UnknownActor(actor) => write!(f, "unknown actor {actor}"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// Active participants and permanently revoked actor identities.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActorRegistry {
    active: BTreeMap<ActorId, ActorRole>,
    revoked: BTreeSet<ActorId>,
}

impl ActorRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an actor, or leaves an identical active registration intact.
    pub fn register(&mut self, actor: ActorId, role: ActorRole) -> Result<(), RegistryError> {
        if self.revoked.contains(&actor) {
            return Err(RegistryError::ActorRevoked(actor));
        }
        if let Some(&existing) = self.active.get(&actor) {
            return if existing == role {
                Ok(())
            } else {
                Err(RegistryError::RoleConflict {
                    actor,
                    existing,
                    requested: role,
                })
            };
        }
        self.active.insert(actor, role);
        Ok(())
    }

    /// Permanently revokes an active actor.
    pub fn revoke(&mut self, actor: ActorId) -> Result<(), RegistryError> {
        if self.revoked.contains(&actor) {
            return Err(RegistryError::ActorRevoked(actor));
        }
        if self.active.remove(&actor).is_none() {
            return Err(RegistryError::UnknownActor(actor));
        }
        self.revoked.insert(actor);
        Ok(())
    }

    /// Returns the role of an active actor.
    pub fn role_of(&self, actor: ActorId) -> Option<ActorRole> {
        self.active.get(&actor).copied()
    }

    /// Returns whether the actor is active.
    pub fn is_active(&self, actor: ActorId) -> bool {
        self.active.contains_key(&actor)
    }

    /// Returns whether the actor has been revoked.
    pub fn is_revoked(&self, actor: ActorId) -> bool {
        self.revoked.contains(&actor)
    }

    /// Returns active actors in ascending actor ID order.
    pub fn active_actors(&self) -> Vec<(ActorId, ActorRole)> {
        self.active
            .iter()
            .map(|(&actor, &role)| (actor, role))
            .collect()
    }

    /// Returns the number of active actors.
    pub fn len_active(&self) -> usize {
        self.active.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::assert_law;

    #[derive(Clone, Copy, Debug)]
    enum Op {
        Register(ActorRole),
        Revoke,
    }

    fn apply(registry: &mut ActorRegistry, actor: ActorId, op: Op) -> Result<(), RegistryError> {
        match op {
            Op::Register(role) => registry.register(actor, role),
            Op::Revoke => registry.revoke(actor),
        }
    }

    #[test]
    fn distinct_actor_operations_commute() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| {
                (0..128)
                    .map(|_| (ActorId::new(rng.next_u64()), ActorId::new(rng.next_u64())))
                    .collect::<Vec<_>>()
            },
            |pairs| {
                let ops = [
                    Op::Register(ActorRole::Human),
                    Op::Register(ActorRole::Agent),
                    Op::Revoke,
                ];
                for &(a, b) in pairs {
                    if a == b {
                        continue;
                    }
                    // Each prefix creates one of unknown, active-human, active-agent, revoked.
                    for initial_a in 0..4 {
                        for initial_b in 0..4 {
                            let mut base = ActorRegistry::new();
                            for (actor, state) in [(a, initial_a), (b, initial_b)] {
                                if state != 0 {
                                    base.register(
                                        actor,
                                        if state == 2 {
                                            ActorRole::Agent
                                        } else {
                                            ActorRole::Human
                                        },
                                    )
                                    .unwrap();
                                    if state == 3 {
                                        base.revoke(actor).unwrap();
                                    }
                                }
                            }
                            for &op_a in &ops {
                                for &op_b in &ops {
                                    let mut ab = base.clone();
                                    let result_a = apply(&mut ab, a, op_a);
                                    let result_b = apply(&mut ab, b, op_b);
                                    let mut ba = base.clone();
                                    assert_eq!(result_b, apply(&mut ba, b, op_b));
                                    assert_eq!(result_a, apply(&mut ba, a, op_a));
                                    assert_eq!(ab, ba);
                                }
                            }
                        }
                    }
                }
            },
        );
    }

    #[test]
    fn registration_and_revocation_laws() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| {
                (0..128)
                    .map(|_| ActorId::new(rng.next_u64()))
                    .chain([ActorId::new(0), ActorId::new(u64::MAX)])
                    .collect::<Vec<_>>()
            },
            |actors| {
                for &actor in actors {
                    for role in ActorRole::ALL {
                        let other = if role == ActorRole::Human {
                            ActorRole::Agent
                        } else {
                            ActorRole::Human
                        };
                        let mut registry = ActorRegistry::new();
                        assert_eq!(
                            registry.revoke(actor),
                            Err(RegistryError::UnknownActor(actor))
                        );
                        assert_eq!(registry, ActorRegistry::default());
                        assert_eq!(registry.register(actor, role), Ok(()));
                        let active = registry.clone();
                        assert_eq!(registry.register(actor, role), Ok(()));
                        assert_eq!(registry, active);
                        assert_eq!(
                            registry.register(actor, other),
                            Err(RegistryError::RoleConflict {
                                actor,
                                existing: role,
                                requested: other
                            })
                        );
                        assert_eq!(registry, active);
                        assert_eq!(registry.role_of(actor), Some(role));
                        assert!(registry.is_active(actor));
                        assert!(!registry.is_revoked(actor));
                        assert_eq!(registry.revoke(actor), Ok(()));
                        let revoked = registry.clone();
                        assert_eq!(registry.role_of(actor), None);
                        assert!(!registry.is_active(actor));
                        assert!(registry.is_revoked(actor));
                        for requested in ActorRole::ALL {
                            assert_eq!(
                                registry.register(actor, requested),
                                Err(RegistryError::ActorRevoked(actor))
                            );
                            assert_eq!(registry, revoked);
                        }
                        assert_eq!(
                            registry.revoke(actor),
                            Err(RegistryError::ActorRevoked(actor))
                        );
                        assert_eq!(registry, revoked);
                    }
                }
            },
        );
    }

    #[test]
    fn active_actors_are_sorted_and_counted() {
        let mut registry = ActorRegistry::new();
        let a = ActorId::new(0);
        let b = ActorId::new(1);
        let c = ActorId::new(u64::MAX);
        assert_eq!(registry.role_of(b), None);
        assert!(!registry.is_active(b));
        assert!(!registry.is_revoked(b));
        registry.register(c, ActorRole::Agent).unwrap();
        registry.register(a, ActorRole::Human).unwrap();
        registry.register(b, ActorRole::Agent).unwrap();
        registry.revoke(b).unwrap();
        assert_eq!(
            registry.active_actors(),
            vec![(a, ActorRole::Human), (c, ActorRole::Agent)]
        );
        assert_eq!(registry.len_active(), 2);
    }

    #[test]
    fn errors_implement_error_and_display() {
        fn assert_error<T: std::error::Error + Copy>() {}
        assert_error::<RegistryError>();
        let actor = ActorId::new(7);
        assert_eq!(
            RegistryError::UnknownActor(actor).to_string(),
            "unknown actor 7"
        );
        assert_eq!(
            RegistryError::ActorRevoked(actor).to_string(),
            "actor 7 is revoked"
        );
        assert_eq!(
            RegistryError::RoleConflict {
                actor,
                existing: ActorRole::Human,
                requested: ActorRole::Agent
            }
            .to_string(),
            "actor 7 has role human, not agent"
        );
    }
}
