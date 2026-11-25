//! Composable edits expressed in the coordinates of one workspace revision.
//!
//! Composition unions splices in canonical buffer and span order. `Patch`
//! rejects closed-interval contact, making disjoint union commutative and
//! associative; the empty map is its identity. Evaluation never mutates the
//! snapshot and validates every target before returning a result.

use std::collections::BTreeMap;
use std::fmt;

use crate::editor::edit::ids::{ActorId, BufferId, Nonce};
use crate::editor::edit::intent::EditIntent;
use crate::editor::edit::patch::{Patch, PatchError};
use crate::editor::edit::transaction::{Transaction, TransactionError, TransactionResult};
use crate::editor::workspace::snapshots::WorkspaceSnapshot;
use crate::editor::workspace::version::Revision;

/// An immutable, canonically ordered plan in base-revision coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceEdit {
    actor: ActorId,
    base: Revision,
    patches: BTreeMap<BufferId, Patch>,
}

/// A plan could not be combined or evaluated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceEditError {
    /// Plans belong to different actors.
    ActorMismatch { left: ActorId, right: ActorId },
    /// Plans refer to different base revisions.
    BaseMismatch { left: Revision, right: Revision },
    /// Splices for the same buffer conflict.
    Patch { buffer: BufferId, error: PatchError },
    /// The transaction or snapshot rejected the plan.
    Transaction(TransactionError),
}

impl fmt::Display for WorkspaceEditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ActorMismatch { left, right } => write!(f, "actor mismatch: {left} and {right}"),
            Self::BaseMismatch { left, right } => write!(f, "base mismatch: {left} and {right}"),
            Self::Patch { buffer, error } => write!(f, "buffer {buffer}: {error}"),
            Self::Transaction(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for WorkspaceEditError {}

impl From<TransactionError> for WorkspaceEditError {
    fn from(error: TransactionError) -> Self {
        Self::Transaction(error)
    }
}

impl WorkspaceEdit {
    /// Creates an identity plan for an actor and revision.
    pub fn empty(actor: ActorId, base: Revision) -> Self {
        Self {
            actor,
            base,
            patches: BTreeMap::new(),
        }
    }

    /// Constructs and validates a plan against a snapshot. Duplicate buffer IDs
    /// are rejected; use composition to combine distinct patches for one buffer.
    pub fn new(
        actor: ActorId,
        base: Revision,
        patches: Vec<(BufferId, Patch)>,
        snapshot: &WorkspaceSnapshot,
    ) -> Result<Self, WorkspaceEditError> {
        let txn = Transaction::new(EditIntent::for_actor(actor, Nonce::new(0), base), patches)?;
        txn.apply_with_inverse(snapshot)?;
        Ok(Self {
            actor,
            base,
            patches: txn.patches().iter().cloned().collect(),
        })
    }

    /// Returns the actor.
    pub fn actor(&self) -> ActorId {
        self.actor
    }
    /// Returns the base revision.
    pub fn base(&self) -> Revision {
        self.base
    }
    /// Returns patches in ascending buffer order.
    pub fn patches(&self) -> &BTreeMap<BufferId, Patch> {
        &self.patches
    }

    /// Unions compatible plans without modifying either input.
    pub fn compose(&self, other: &Self) -> Result<Self, WorkspaceEditError> {
        if self.actor != other.actor {
            return Err(WorkspaceEditError::ActorMismatch {
                left: self.actor,
                right: other.actor,
            });
        }
        if self.base != other.base {
            return Err(WorkspaceEditError::BaseMismatch {
                left: self.base,
                right: other.base,
            });
        }
        let mut patches = self.patches.clone();
        for (buffer, patch) in &other.patches {
            if let Some(existing) = patches.get(buffer) {
                let splices = existing
                    .splices()
                    .iter()
                    .chain(patch.splices())
                    .cloned()
                    .collect();
                let merged = Patch::new(splices).map_err(|error| WorkspaceEditError::Patch {
                    buffer: *buffer,
                    error,
                })?;
                patches.insert(*buffer, merged);
            } else {
                patches.insert(*buffer, patch.clone());
            }
        }
        Ok(Self {
            actor: self.actor,
            base: self.base,
            patches,
        })
    }

    /// Converts to a transaction after validating the supplied snapshot.
    pub fn to_transaction(
        &self,
        nonce: Nonce,
        snapshot: &WorkspaceSnapshot,
    ) -> Result<Transaction, WorkspaceEditError> {
        let txn = Transaction::new(
            EditIntent::for_actor(self.actor, nonce, self.base),
            self.patches
                .iter()
                .map(|(id, patch)| (*id, patch.clone()))
                .collect(),
        )?;
        txn.apply_with_inverse(snapshot)?;
        Ok(txn)
    }

    /// Evaluates atomically, returning the resulting catalog and inverse.
    pub fn apply(
        &self,
        nonce: Nonce,
        snapshot: &WorkspaceSnapshot,
    ) -> Result<TransactionResult, WorkspaceEditError> {
        self.to_transaction(nonce, snapshot)?
            .apply_with_inverse(snapshot)
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::edit::splice::{Splice, SpliceError};
    use crate::editor::test_support::assert_law;
    use crate::editor::text::{span::ByteSpan, utf8::Text};
    use crate::editor::workspace::{catalog::BufferCatalog, snapshots::VersionedWorkspace};

    fn patch(source: &str, start: usize, end: usize, replacement: &str) -> Patch {
        let text = Text::from_str(source);
        Patch::new(vec![Splice::new(
            ByteSpan::validate_in(&text, start, end).unwrap(),
            Text::from_str(replacement),
        )])
        .unwrap()
    }

    fn snapshot() -> WorkspaceSnapshot {
        let mut catalog = BufferCatalog::new();
        for id in 0..3 {
            catalog
                .register(BufferId::new(id), Text::from_str("a界b😀c"))
                .unwrap();
        }
        catalog
            .register(BufferId::new(3), Text::from_str("closed"))
            .unwrap();
        catalog.remove(BufferId::new(3)).unwrap();
        VersionedWorkspace::new(catalog).capture()
    }

    fn plan(
        snapshot: &WorkspaceSnapshot,
        id: u64,
        start: usize,
        end: usize,
        replacement: &str,
    ) -> WorkspaceEdit {
        WorkspaceEdit::new(
            ActorId::new(1),
            snapshot.revision(),
            vec![(BufferId::new(id), patch("a界b😀c", start, end, replacement))],
            snapshot,
        )
        .unwrap()
    }

    #[test]
    fn composition_laws_and_application() {
        assert_law(
            &[0, 1, 42, 99, u64::MAX],
            |rng| {
                (
                    rng.gen_range(0..3) as u64,
                    rng.gen_range(0..3) as u64,
                    rng.gen_range(0..3) as u64,
                    rng.gen_bool(),
                )
            },
            |&(a, b, c, flag)| {
                let snapshot = snapshot();
                let first = plan(&snapshot, a, 0, 1, if flag { "界" } else { "" });
                let second = plan(&snapshot, b, 4, 5, "😀");
                let third = plan(&snapshot, c, 9, 10, "z");
                let identity = WorkspaceEdit::empty(first.actor(), first.base());
                assert_eq!(first.compose(&identity).unwrap(), first);
                assert_eq!(identity.compose(&first).unwrap(), first);
                assert_eq!(first.compose(&second), second.compose(&first));
                let combined = first.compose(&second).unwrap().compose(&third).unwrap();
                assert_eq!(
                    combined,
                    first.compose(&second.compose(&third).unwrap()).unwrap()
                );
                let nonce = Nonce::new(7);
                let result = combined.apply(nonce, &snapshot).unwrap();
                let txn = combined.to_transaction(nonce, &snapshot).unwrap();
                assert_eq!(result, txn.apply_with_inverse(&snapshot).unwrap());
                // Apply individual splices in descending base coordinates so offsets remain valid.
                let mut expected = snapshot.catalog().clone();
                for component in [&third, &second, &first] {
                    for (id, p) in component.patches() {
                        let next = p.apply(expected.get(*id).unwrap()).unwrap();
                        expected.update(*id, next).unwrap();
                    }
                }
                assert_eq!(result.catalog(), &expected);
            },
        );
    }

    #[test]
    fn rejects_conflicts_and_invalid_targets_atomically() {
        let snapshot = snapshot();
        let first = plan(&snapshot, 0, 0, 1, "x");
        let before = first.clone();
        let actor = WorkspaceEdit::empty(ActorId::new(2), snapshot.revision());
        let base = WorkspaceEdit::empty(first.actor(), Revision::new(1));
        assert!(matches!(
            first.compose(&actor),
            Err(WorkspaceEditError::ActorMismatch { .. })
        ));
        assert!(matches!(
            first.compose(&base),
            Err(WorkspaceEditError::BaseMismatch { .. })
        ));
        for (a, b, c, d) in [(0, 0, 0, 0), (0, 4, 1, 5), (0, 1, 1, 4)] {
            let x = plan(&snapshot, 0, a, b, "x");
            let y = plan(&snapshot, 0, c, d, "y");
            let inputs = (x.clone(), y.clone(), snapshot.clone());
            assert!(matches!(
                x.compose(&y),
                Err(WorkspaceEditError::Patch {
                    error: PatchError::Contact { .. },
                    ..
                })
            ));
            assert_eq!((x, y, snapshot.clone()), inputs);
        }
        for (id, p, expected) in [
            (
                0,
                patch("abcdefghijk", 2, 2, ""),
                TransactionError::Patch {
                    buffer: BufferId::new(0),
                    error: PatchError::InvalidSplice(SpliceError::NotCharBoundary { offset: 2 }),
                },
            ),
            (
                0,
                patch("abcdefghijkl", 11, 11, ""),
                TransactionError::Patch {
                    buffer: BufferId::new(0),
                    error: PatchError::InvalidSplice(SpliceError::OutOfBounds {
                        offset: 11,
                        len: 10,
                    }),
                },
            ),
            (
                3,
                Patch::default(),
                TransactionError::BufferClosed(BufferId::new(3)),
            ),
            (
                4,
                Patch::default(),
                TransactionError::UnknownBuffer(BufferId::new(4)),
            ),
        ] {
            assert_eq!(
                WorkspaceEdit::new(
                    first.actor(),
                    first.base(),
                    vec![
                        (BufferId::new(1), patch("a界b😀c", 0, 1, "x")),
                        (BufferId::new(id), p)
                    ],
                    &snapshot
                ),
                Err(WorkspaceEditError::Transaction(expected))
            );
            assert_eq!(first, before);
        }
        assert!(matches!(
            base.apply(Nonce::new(0), &snapshot),
            Err(WorkspaceEditError::Transaction(
                TransactionError::RevisionMismatch { .. }
            ))
        ));
        assert_eq!(first, before);
    }

    #[test]
    fn generated_rejections_preserve_inputs() {
        assert_law(
            &[0, 1, 2, 3, 42, 99, u64::MAX],
            |rng| (rng.next_u64(), rng.gen_range(0..3), rng.gen_bool()),
            |&(value, choice, flag)| {
                let snapshot = snapshot();
                let saved = snapshot.clone();
                let first = plan(&snapshot, 0, 0, 1, "x");
                let saved_plan = first.clone();
                let other_actor = WorkspaceEdit::empty(ActorId::new(value | 2), first.base());
                assert!(matches!(
                    first.compose(&other_actor),
                    Err(WorkspaceEditError::ActorMismatch { .. })
                ));
                let other_base = WorkspaceEdit::empty(first.actor(), Revision::new(value | 1));
                assert!(matches!(
                    first.compose(&other_base),
                    Err(WorkspaceEditError::BaseMismatch { .. })
                ));
                assert!(matches!(
                    other_base.to_transaction(Nonce::new(value), &snapshot),
                    Err(WorkspaceEditError::Transaction(
                        TransactionError::RevisionMismatch { .. }
                    ))
                ));

                let (a, b, c, d) = [(0, 0, 0, 0), (0, 4, 1, 5), (0, 1, 1, 4)][choice];
                let x = plan(&snapshot, 1, a, b, "x");
                let y = plan(&snapshot, 1, c, d, "y");
                let pair = (x.clone(), y.clone());
                assert!(matches!(
                    x.compose(&y),
                    Err(WorkspaceEditError::Patch {
                        error: PatchError::Contact { .. },
                        ..
                    })
                ));
                assert_eq!((x, y), pair);

                let offset = if flag { 2 } else { 11 + (value % 8) as usize };
                let invalid = patch(&"a".repeat(offset), offset, offset, "x");
                let expected = if flag {
                    SpliceError::NotCharBoundary { offset }
                } else {
                    SpliceError::OutOfBounds { offset, len: 10 }
                };
                let valid = (BufferId::new(0), patch("a界b😀c", 0, 1, "x"));
                assert_eq!(
                    WorkspaceEdit::new(
                        first.actor(),
                        first.base(),
                        vec![valid.clone(), (BufferId::new(1), invalid)],
                        &snapshot
                    ),
                    Err(WorkspaceEditError::Transaction(TransactionError::Patch {
                        buffer: BufferId::new(1),
                        error: PatchError::InvalidSplice(expected)
                    }))
                );
                let id = if flag {
                    BufferId::new(3)
                } else {
                    BufferId::new(4 + value % 100)
                };
                let expected = if flag {
                    TransactionError::BufferClosed(id)
                } else {
                    TransactionError::UnknownBuffer(id)
                };
                assert_eq!(
                    WorkspaceEdit::new(
                        first.actor(),
                        first.base(),
                        vec![valid, (id, Patch::default())],
                        &snapshot
                    ),
                    Err(WorkspaceEditError::Transaction(expected))
                );
                assert_eq!(snapshot, saved);
                assert_eq!(first, saved_plan);
            },
        );
    }
}
