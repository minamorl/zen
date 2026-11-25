//! Atomic multi-buffer edit preparation.
//!
//! A transaction is evaluated against a snapshot and returns a new catalog;
//! it does not mutate a workspace or advance its revision. A caller committing
//! a changed catalog must atomically publish it at `base.next()`. The current
//! workspace API does not expose such a commit operation.
//!
//! # Laws
//!
//! * **Atomicity:** the base is checked first, then every live buffer and patch
//!   result and inverse is computed before a cloned catalog is updated. An
//!   error therefore leaves the snapshot and its workspace unchanged.
//! * **Commutativity:** construction sorts distinct IDs. Each patch reads only
//!   its own original text, and updates to distinct catalog entries commute;
//!   every input permutation gives the same transaction and catalog.
//! * **Inverse:** each patch inverse restores its original text. Distinct IDs
//!   cannot interfere, so applying all inverses with `apply_to_catalog` restores
//!   every affected text, even when another revision cannot be committed.
//! * **No-op:** equality of all computed texts to their inputs defines a no-op,
//!   including replacements with identical text. It needs no revision advance;
//!   a changed result requires `Revision::next` and fails on overflow.

use std::fmt;

use crate::editor::edit::{
    ids::BufferId,
    intent::EditIntent,
    patch::{Patch, PatchError},
};
use crate::editor::workspace::{
    catalog::BufferCatalog, snapshots::WorkspaceSnapshot, version::Revision,
};

/// A canonical set of patches associated with one editing intent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    intent: EditIntent,
    patches: Vec<(BufferId, Patch)>,
}

/// A transaction could not be constructed or evaluated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransactionError {
    /// An identifier occurs more than once.
    DuplicateBufferId(BufferId),
    /// The intent does not refer to the snapshot's revision.
    RevisionMismatch {
        expected: Revision,
        actual: Revision,
    },
    /// The buffer has never been registered.
    UnknownBuffer(BufferId),
    /// The buffer was removed.
    BufferClosed(BufferId),
    /// A patch is invalid for its buffer's current text.
    Patch { buffer: BufferId, error: PatchError },
    /// A changed result cannot be assigned a next revision.
    RevisionOverflow,
}

impl fmt::Display for TransactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateBufferId(id) => write!(f, "duplicate buffer {id}"),
            Self::RevisionMismatch { expected, actual } => {
                write!(f, "expected revision {expected}, got {actual}")
            }
            Self::UnknownBuffer(id) => write!(f, "unknown buffer {id}"),
            Self::BufferClosed(id) => write!(f, "buffer {id} is closed"),
            Self::Patch { buffer, error } => write!(f, "buffer {buffer}: {error}"),
            Self::RevisionOverflow => f.write_str("workspace revision overflow"),
        }
    }
}

impl std::error::Error for TransactionError {}

/// An evaluated catalog, its inverse, and whether any text changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionResult {
    catalog: BufferCatalog,
    inverse: Transaction,
    is_noop: bool,
}

impl TransactionResult {
    /// Returns the evaluated catalog.
    pub fn catalog(&self) -> &BufferCatalog {
        &self.catalog
    }
    /// Returns the inverse, based at the revision a caller must publish.
    pub fn inverse(&self) -> &Transaction {
        &self.inverse
    }
    /// Returns whether all resulting texts equal their inputs.
    pub fn is_noop(&self) -> bool {
        self.is_noop
    }
    /// Consumes the result into its catalog and inverse.
    pub fn into_parts(self) -> (BufferCatalog, Transaction) {
        (self.catalog, self.inverse)
    }
}

impl Transaction {
    /// Sorts entries by ID and rejects duplicates.
    pub fn new(
        intent: EditIntent,
        mut patches: Vec<(BufferId, Patch)>,
    ) -> Result<Self, TransactionError> {
        patches.sort_by_key(|(id, _)| *id);
        for pair in patches.windows(2) {
            if pair[0].0 == pair[1].0 {
                return Err(TransactionError::DuplicateBufferId(pair[0].0));
            }
        }
        Ok(Self { intent, patches })
    }

    /// Returns the intent.
    pub fn intent(&self) -> EditIntent {
        self.intent
    }
    /// Returns entries in ascending buffer order.
    pub fn patches(&self) -> &[(BufferId, Patch)] {
        &self.patches
    }

    /// Applies to a catalog without checking or advancing a revision.
    ///
    /// This is useful for applying an inverse to the catalog returned by
    /// `apply_with_inverse`, including when the resulting revision is MAX.
    /// The caller is responsible for matching the catalog to the intent's base.
    pub fn apply_to_catalog(
        &self,
        catalog: &BufferCatalog,
    ) -> Result<BufferCatalog, TransactionError> {
        let mut computed = Vec::with_capacity(self.patches.len());
        for (id, patch) in &self.patches {
            let text = catalog.get(*id).ok_or_else(|| {
                if catalog.is_tombstoned(*id) {
                    TransactionError::BufferClosed(*id)
                } else {
                    TransactionError::UnknownBuffer(*id)
                }
            })?;
            let (next, _) = patch
                .apply_with_inverse(text)
                .map_err(|error| TransactionError::Patch { buffer: *id, error })?;
            computed.push((*id, next));
        }
        let mut result = catalog.clone();
        for (id, text) in computed {
            result.update(id, text).expect("validated live buffer");
        }
        Ok(result)
    }

    /// Evaluates against a snapshot, returning a new catalog and exact inverse.
    ///
    /// The inverse has the same transaction ID and base `snapshot.revision()`
    /// for a no-op, or `snapshot.revision().next()` for a changed result.
    /// This method does not commit or alter the snapshot or its workspace.
    pub fn apply_with_inverse(
        &self,
        snapshot: &WorkspaceSnapshot,
    ) -> Result<TransactionResult, TransactionError> {
        let actual = snapshot.revision();
        if self.intent.base() != actual {
            return Err(TransactionError::RevisionMismatch {
                expected: self.intent.base(),
                actual,
            });
        }
        let mut computed = Vec::with_capacity(self.patches.len());
        let mut is_noop = true;
        for (id, patch) in &self.patches {
            let text = snapshot.catalog().get(*id).ok_or_else(|| {
                if snapshot.catalog().is_tombstoned(*id) {
                    TransactionError::BufferClosed(*id)
                } else {
                    TransactionError::UnknownBuffer(*id)
                }
            })?;
            let (next, inverse) = patch
                .apply_with_inverse(text)
                .map_err(|error| TransactionError::Patch { buffer: *id, error })?;
            is_noop &= next == *text;
            computed.push((*id, next, inverse));
        }
        let next_revision = if is_noop {
            actual
        } else {
            actual
                .next()
                .map_err(|_| TransactionError::RevisionOverflow)?
        };
        let mut catalog = snapshot.catalog().clone();
        let mut inverses = Vec::with_capacity(computed.len());
        for (id, next, inverse) in computed {
            // Each ID was checked live above; only texts are updated here.
            catalog.update(id, next).expect("validated live buffer");
            inverses.push((id, inverse));
        }
        Ok(TransactionResult {
            catalog,
            inverse: Self {
                intent: self.intent.with_base(next_revision),
                patches: inverses,
            },
            is_noop,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::edit::{
        ids::{ActorId, Nonce},
        splice::{Splice, SpliceError},
    };
    use crate::editor::text::{span::ByteSpan, utf8::Text};
    use crate::editor::workspace::snapshots::VersionedWorkspace;

    fn intent(base: Revision) -> EditIntent {
        EditIntent::for_actor(ActorId::new(1), Nonce::new(2), base)
    }
    fn replace(source: &str, replacement: &str) -> Patch {
        Patch::new(vec![Splice::new(
            ByteSpan::full_of(&Text::from_str(source)),
            Text::from_str(replacement),
        )])
        .unwrap()
    }
    fn workspace(texts: &[&str]) -> VersionedWorkspace {
        let mut catalog = BufferCatalog::new();
        for (i, text) in texts.iter().enumerate() {
            catalog
                .register(BufferId::new(i as u64), Text::from_str(text))
                .unwrap();
        }
        VersionedWorkspace::new(catalog)
    }
    fn permutations(
        entries: &mut [(BufferId, Patch)],
        at: usize,
        expected: &Transaction,
        snapshot: &WorkspaceSnapshot,
        catalog: &BufferCatalog,
    ) {
        if at == entries.len() {
            let txn = Transaction::new(expected.intent(), entries.to_vec()).unwrap();
            assert_eq!(&txn, expected);
            assert_eq!(txn.apply_with_inverse(snapshot).unwrap().catalog(), catalog);
        } else {
            for i in at..entries.len() {
                entries.swap(at, i);
                permutations(entries, at + 1, expected, snapshot, catalog);
                entries.swap(at, i);
            }
        }
    }

    #[test]
    fn exhaustive_permutations_inverse_and_noop() {
        for a in ["", "a", "界"] {
            for b in ["", "b", "😀"] {
                for c in ["", "c", "e\u{301}"] {
                    for replacement in ["", "x", "界"] {
                        let workspace = workspace(&[a, b, c]);
                        let snapshot = workspace.capture();
                        let mut entries = vec![
                            (BufferId::new(0), replace(a, replacement)),
                            (BufferId::new(1), replace(b, replacement)),
                            (BufferId::new(2), replace(c, replacement)),
                        ];
                        let txn =
                            Transaction::new(intent(Revision::ZERO), entries.clone()).unwrap();
                        let result = txn.apply_with_inverse(&snapshot).unwrap();
                        for id in 0..3 {
                            assert_eq!(
                                result.catalog().get(BufferId::new(id)).unwrap().as_str(),
                                replacement
                            );
                        }
                        assert_eq!(
                            result.is_noop(),
                            a == replacement && b == replacement && c == replacement
                        );
                        permutations(&mut entries, 0, &txn, &snapshot, result.catalog());
                        let mut after = VersionedWorkspace::new(result.catalog().clone());
                        if !result.is_noop() {
                            let text = after.catalog().get(BufferId::new(0)).unwrap().clone();
                            after.update(BufferId::new(0), text).unwrap();
                        }
                        let restored = result
                            .inverse()
                            .apply_with_inverse(&after.capture())
                            .unwrap();
                        assert_eq!(restored.catalog(), snapshot.catalog());
                        assert_eq!(
                            &result.inverse().apply_to_catalog(result.catalog()).unwrap(),
                            snapshot.catalog()
                        );
                        assert_eq!(workspace.capture(), snapshot);
                    }
                }
            }
        }
        let single = workspace(&["abc"]);
        let empty = Transaction::new(intent(Revision::ZERO), vec![])
            .unwrap()
            .apply_with_inverse(&single.capture())
            .unwrap();
        assert!(empty.is_noop());
        assert_eq!(empty.inverse().intent().base(), Revision::ZERO);
        let same = Transaction::new(
            intent(Revision::ZERO),
            vec![(BufferId::new(0), replace("abc", "abc"))],
        )
        .unwrap()
        .apply_with_inverse(&single.capture())
        .unwrap();
        assert!(same.is_noop());
        let different = Transaction::new(
            intent(Revision::ZERO),
            vec![
                (BufferId::new(0), replace("abc", "A")),
                (BufferId::new(1), replace("界", "B")),
                (BufferId::new(2), replace("😀", "C")),
            ],
        )
        .unwrap();
        let result = different
            .apply_with_inverse(&workspace(&["abc", "界", "😀"]).capture())
            .unwrap();
        for (id, expected) in [(0, "A"), (1, "B"), (2, "C")] {
            assert_eq!(
                result.catalog().get(BufferId::new(id)).unwrap().as_str(),
                expected
            );
        }
    }

    #[test]
    fn failures_leave_workspace_and_snapshot_unchanged() {
        let mut workspace = workspace(&["ok", "界", "closed"]);
        workspace.remove(BufferId::new(2)).unwrap();
        let snapshot = workspace.capture();
        let valid = (BufferId::new(0), replace("ok", "new"));
        let bad_span = ByteSpan::validate_in(&Text::from_str("abc"), 1, 2).unwrap();
        let invalid = Patch::new(vec![Splice::delete(bad_span)]).unwrap();
        for (id, patch, expected) in [
            (
                1,
                invalid,
                TransactionError::Patch {
                    buffer: BufferId::new(1),
                    error: PatchError::InvalidSplice(SpliceError::NotCharBoundary { offset: 1 }),
                },
            ),
            (
                2,
                Patch::default(),
                TransactionError::BufferClosed(BufferId::new(2)),
            ),
            (
                3,
                Patch::default(),
                TransactionError::UnknownBuffer(BufferId::new(3)),
            ),
        ] {
            let txn = Transaction::new(
                intent(snapshot.revision()),
                vec![valid.clone(), (BufferId::new(id), patch)],
            )
            .unwrap();
            assert_eq!(txn.apply_with_inverse(&snapshot), Err(expected));
            assert_eq!(workspace.capture(), snapshot);
        }
        let stale = Transaction::new(intent(Revision::ZERO), vec![valid.clone()]).unwrap();
        assert_eq!(
            stale.apply_with_inverse(&snapshot),
            Err(TransactionError::RevisionMismatch {
                expected: Revision::ZERO,
                actual: snapshot.revision()
            })
        );
        assert_eq!(workspace.capture(), snapshot);
        assert_eq!(
            Transaction::new(intent(Revision::ZERO), vec![valid.clone(), valid]),
            Err(TransactionError::DuplicateBufferId(BufferId::new(0)))
        );
    }
}
