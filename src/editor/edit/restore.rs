//! Prepare historical text as a fresh, atomic edit proposal.

use std::collections::BTreeSet;
use std::fmt;

use crate::editor::edit::{
    ids::{ActorId, BufferId, Nonce},
    intent::EditIntent,
    patch::Patch,
    splice::Splice,
    transaction::Transaction,
};
use crate::editor::text::{diff::diff, utf8::Text};
use crate::editor::workspace::snapshots::WorkspaceSnapshot;

/// A restoration either needs no commit or is represented by one transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// Every target already has the requested content.
    NoChange,
    /// A transaction containing only changed buffers.
    Transaction(Transaction),
}

/// An invalid restoration target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestoreError {
    /// A target occurs more than once, including unchanged targets.
    DuplicateBufferId(BufferId),
    /// The target was never registered.
    UnknownBuffer(BufferId),
    /// The target was removed.
    BufferClosed(BufferId),
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateBufferId(id) => write!(f, "duplicate buffer {id}"),
            Self::UnknownBuffer(id) => write!(f, "unknown buffer {id}"),
            Self::BufferClosed(id) => write!(f, "buffer {id} is closed"),
        }
    }
}

impl std::error::Error for RestoreError {}

/// Prepares a restoration of one live buffer against a snapshot.
pub fn restore(
    snapshot: &WorkspaceSnapshot,
    buffer: BufferId,
    target: Text,
    actor: ActorId,
    nonce: Nonce,
) -> Result<RestoreOutcome, RestoreError> {
    restore_many(snapshot, [(buffer, target)], actor, nonce)
}

/// Prepares an atomic restoration from a borrowed slice of targets.
pub fn restore_slice(
    snapshot: &WorkspaceSnapshot,
    targets: &[(BufferId, Text)],
    actor: ActorId,
    nonce: Nonce,
) -> Result<RestoreOutcome, RestoreError> {
    restore_many(snapshot, targets.iter().cloned(), actor, nonce)
}

/// Prepares one atomic restoration; duplicate and non-live targets are rejected.
/// An empty target set is a no-change outcome and requires no workspace commit.
pub fn restore_many(
    snapshot: &WorkspaceSnapshot,
    targets: impl IntoIterator<Item = (BufferId, Text)>,
    actor: ActorId,
    nonce: Nonce,
) -> Result<RestoreOutcome, RestoreError> {
    let mut seen = BTreeSet::new();
    let mut patches = Vec::new();
    for (id, target) in targets {
        if !seen.insert(id) {
            return Err(RestoreError::DuplicateBufferId(id));
        }
        let current = snapshot.catalog().get(id).ok_or_else(|| {
            if snapshot.catalog().is_tombstoned(id) {
                RestoreError::BufferClosed(id)
            } else {
                RestoreError::UnknownBuffer(id)
            }
        })?;
        if let Some(change) = diff(current, &target) {
            let splice = Splice::new(change.span, change.replacement);
            let patch = Patch::new(vec![splice]).expect("one splice cannot overlap itself");
            patches.push((id, patch));
        }
    }
    if patches.is_empty() {
        return Ok(RestoreOutcome::NoChange);
    }
    let intent = EditIntent::for_actor(actor, nonce, snapshot.revision());
    Ok(RestoreOutcome::Transaction(
        Transaction::new(intent, patches).expect("target IDs were checked for duplicates"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};
    use crate::editor::workspace::{catalog::BufferCatalog, snapshots::VersionedWorkspace};

    const A: BufferId = BufferId::new(1);
    const B: BufferId = BufferId::new(2);
    const OTHER: BufferId = BufferId::new(3);
    const DEAD: BufferId = BufferId::new(4);
    const MISSING: BufferId = BufferId::new(5);
    const ACTOR: ActorId = ActorId::new(7);
    const NONCE: Nonce = Nonce::new(9);

    fn workspace(a: &str, b: &str) -> VersionedWorkspace {
        let mut catalog = BufferCatalog::new();
        for (id, text) in [(A, a), (B, b), (OTHER, "untouched"), (DEAD, "removed")] {
            catalog.register(id, Text::from_str(text)).unwrap();
        }
        catalog.remove(DEAD).unwrap();
        VersionedWorkspace::new(catalog)
    }

    #[test]
    fn restoration_laws() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for (before, target) in corpus.iter().zip(corpus.iter().rev()) {
                let mut workspace = workspace(before, "second");
                let snapshot = workspace.capture();
                let target = Text::from_str(target);
                let second = Text::from_str(before);
                let outcome = restore_many(
                    &snapshot,
                    [(A, target.clone()), (B, second.clone())],
                    ACTOR,
                    NONCE,
                )
                .unwrap();
                let RestoreOutcome::Transaction(txn) = outcome else {
                    assert_eq!(snapshot.catalog().get(A), Some(&target));
                    assert_eq!(snapshot.catalog().get(B), Some(&second));
                    assert_eq!(workspace.capture(), snapshot);
                    continue;
                };
                assert_eq!(
                    txn.intent(),
                    EditIntent::for_actor(ACTOR, NONCE, snapshot.revision())
                );
                assert_eq!(
                    txn.patches().len(),
                    usize::from(before != target.as_str()) + usize::from(before != "second")
                );
                let result = txn.apply_with_inverse(&snapshot).unwrap();
                assert_eq!(result.catalog().get(A), Some(&target));
                assert_eq!(result.catalog().get(B), Some(&second));
                assert_eq!(result.catalog().get(OTHER), snapshot.catalog().get(OTHER));
                assert_eq!(
                    result.catalog().tombstoned_ids(),
                    snapshot.catalog().tombstoned_ids()
                );
                let updates = txn
                    .patches()
                    .iter()
                    .map(|(id, _)| (*id, result.catalog().get(*id).unwrap().clone()))
                    .collect::<Vec<_>>();
                workspace.update_batch(updates).unwrap();
                assert!(workspace.revision() > snapshot.revision());
                assert_eq!(workspace.catalog(), result.catalog());
                let inverse = result
                    .inverse()
                    .apply_with_inverse(&workspace.capture())
                    .unwrap();
                assert_eq!(inverse.catalog(), snapshot.catalog());
            }
        });
    }

    #[test]
    fn single_and_unchanged_laws() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for before in corpus {
                let workspace = workspace(before, "second");
                let snapshot = workspace.capture();
                assert_eq!(
                    restore(&snapshot, A, Text::from_str(before), ACTOR, NONCE),
                    Ok(RestoreOutcome::NoChange)
                );
                assert_eq!(
                    restore_many(&snapshot, [], ACTOR, NONCE),
                    Ok(RestoreOutcome::NoChange)
                );
                assert_eq!(
                    restore_many(
                        &snapshot,
                        [(A, Text::from_str(before)), (B, Text::from_str("second"))],
                        ACTOR,
                        NONCE,
                    ),
                    Ok(RestoreOutcome::NoChange)
                );
                assert_eq!(workspace.capture(), snapshot);
                let target = Text::from_str(&format!("{before}界"));
                let RestoreOutcome::Transaction(txn) =
                    restore(&snapshot, A, target.clone(), ACTOR, NONCE).unwrap()
                else {
                    panic!("changed text")
                };
                let result = txn.apply_with_inverse(&snapshot).unwrap();
                assert_eq!(result.catalog().get(A), Some(&target));
                assert_eq!(result.catalog().get(B), snapshot.catalog().get(B));
                assert_eq!(
                    result.inverse().apply_to_catalog(result.catalog()).unwrap(),
                    *snapshot.catalog()
                );
            }
        });
    }

    #[test]
    fn borrowed_slice_targets() {
        let workspace = workspace("old", "second");
        let snapshot = workspace.capture();
        let targets = [(A, Text::from_str("新")), (B, Text::from_str("second"))];
        let RestoreOutcome::Transaction(txn) =
            restore_slice(&snapshot, &targets, ACTOR, NONCE).unwrap()
        else {
            panic!("changed text");
        };
        assert_eq!(txn.patches().len(), 1);
        assert_eq!(
            txn.apply_with_inverse(&snapshot).unwrap().catalog().get(A),
            Some(&targets[0].1)
        );
        assert_eq!(workspace.capture(), snapshot);
    }

    #[test]
    fn invalid_targets_leave_workspace_unchanged() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for text in corpus {
                let workspace = workspace(text, "second");
                let snapshot = workspace.capture();
                for (id, error) in [
                    (MISSING, RestoreError::UnknownBuffer(MISSING)),
                    (DEAD, RestoreError::BufferClosed(DEAD)),
                    (A, RestoreError::DuplicateBufferId(A)),
                ] {
                    assert_eq!(
                        restore_many(
                            &snapshot,
                            [
                                (A, Text::from_str(&format!("{text}界"))),
                                (id, Text::empty())
                            ],
                            ACTOR,
                            NONCE,
                        ),
                        Err(error)
                    );
                    assert_eq!(workspace.capture(), snapshot);
                }
            }
        });
        let workspace = workspace("old", "second");
        let snapshot = workspace.capture();
        for (id, error) in [
            (MISSING, RestoreError::UnknownBuffer(MISSING)),
            (DEAD, RestoreError::BufferClosed(DEAD)),
            (A, RestoreError::DuplicateBufferId(A)),
        ] {
            assert_eq!(
                restore_many(
                    &snapshot,
                    [(A, Text::from_str("new")), (id, Text::empty())],
                    ACTOR,
                    NONCE
                ),
                Err(error)
            );
            assert_eq!(workspace.capture(), snapshot);
        }
        assert_eq!(
            restore_many(
                &snapshot,
                [(A, Text::from_str("old")), (A, Text::from_str("old"))],
                ACTOR,
                NONCE
            ),
            Err(RestoreError::DuplicateBufferId(A))
        );
        assert_eq!(
            restore(&snapshot, DEAD, Text::empty(), ACTOR, NONCE),
            Err(RestoreError::BufferClosed(DEAD))
        );
        assert_eq!(
            restore(&snapshot, MISSING, Text::empty(), ACTOR, NONCE),
            Err(RestoreError::UnknownBuffer(MISSING))
        );
    }
}
