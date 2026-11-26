//! Immutable workspace captures and their revision-ordered history.
//!
//! # Laws
//!
//! * **Immutability:** capture clones the entire catalog, including tombstones.
//!   The snapshot exposes only shared references, so later live catalog mutations
//!   and history operations cannot change the captured value.
//! * **Atomic pairing:** `VersionedWorkspace` owns both values. It begins at
//!   revision zero, advances once on each successful mutation, and never exposes
//!   mutable access to its catalog. Capture clones this pair together; callers
//!   cannot attach an earlier revision to a later catalog through the public API.
//!   A store binds to its first capture's workspace, rejecting other origins even
//!   after pruning, so histories cannot mix independently numbered workspaces.
//! * **History:** recording accepts only revisions greater than every previously
//!   recorded revision, even after pruning. Consequently the last retained entry
//!   is the greatest retained revision, and lookup returns exactly the stored
//!   capture for its key. Rejected records do not change the store.
//! * **Pruning:** `prune_before(floor)` removes precisely entries with revisions
//!   below `floor`; it does not modify any retained capture or the high-water mark.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use super::{
    catalog::{BufferCatalog, CatalogError},
    version::Revision,
};
use crate::editor::{edit::ids::BufferId, text::utf8::Text};

/// A workspace that advances its revision with every successful catalog mutation.
///
/// Captures cannot be constructed by pairing a caller-selected revision with a
/// catalog from another moment:
///
/// ```compile_fail
/// use zen::editor::workspace::{catalog::BufferCatalog, snapshots::WorkspaceSnapshot, version::Revision};
/// let _ = WorkspaceSnapshot::capture(Revision::ZERO, &BufferCatalog::new());
/// ```
#[derive(Debug)]
pub struct VersionedWorkspace {
    revision: Revision,
    catalog: BufferCatalog,
    origin: Arc<()>,
}

/// A workspace mutation failed without changing either revision or catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceError {
    /// The revision cannot advance further.
    RevisionOverflow,
    /// The catalog rejected the operation.
    Catalog(CatalogError),
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RevisionOverflow => f.write_str("workspace revision overflow"),
            Self::Catalog(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for WorkspaceError {}

impl VersionedWorkspace {
    pub(crate) fn resume_from_snapshot(snapshot: &WorkspaceSnapshot) -> Self {
        Self {
            revision: snapshot.revision,
            catalog: snapshot.catalog.clone(),
            origin: Arc::new(()),
        }
    }

    /// Takes ownership of an initial catalog at revision zero.
    pub fn new(catalog: BufferCatalog) -> Self {
        Self {
            revision: Revision::ZERO,
            catalog,
            origin: Arc::new(()),
        }
    }

    /// Returns the current revision.
    pub fn revision(&self) -> Revision {
        self.revision
    }

    /// Returns the current catalog without permitting mutation.
    pub fn catalog(&self) -> &BufferCatalog {
        &self.catalog
    }

    /// Captures the current revision and catalog together.
    pub fn capture(&self) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            revision: self.revision,
            catalog: self.catalog.clone(),
            origin: Arc::clone(&self.origin),
        }
    }

    /// Registers a buffer and advances the revision on success.
    pub fn register(&mut self, id: BufferId, text: Text) -> Result<(), WorkspaceError> {
        let next = self
            .revision
            .next()
            .map_err(|_| WorkspaceError::RevisionOverflow)?;
        self.catalog
            .register(id, text)
            .map_err(WorkspaceError::Catalog)?;
        self.revision = next;
        Ok(())
    }

    /// Updates a buffer and advances the revision on success.
    pub fn update(&mut self, id: BufferId, text: Text) -> Result<(), WorkspaceError> {
        let next = self
            .revision
            .next()
            .map_err(|_| WorkspaceError::RevisionOverflow)?;
        self.catalog
            .update(id, text)
            .map_err(WorkspaceError::Catalog)?;
        self.revision = next;
        Ok(())
    }

    /// Atomically replaces distinct live buffers and advances exactly once.
    pub fn update_batch(
        &mut self,
        updates: impl IntoIterator<Item = (BufferId, Text)>,
    ) -> Result<(), WorkspaceError> {
        let next = self
            .revision
            .next()
            .map_err(|_| WorkspaceError::RevisionOverflow)?;
        let updates: Vec<_> = updates.into_iter().collect();
        let mut seen = BTreeSet::new();
        for (id, _) in &updates {
            if !seen.insert(*id) {
                return Err(WorkspaceError::Catalog(CatalogError::DuplicateBufferId(
                    *id,
                )));
            }
            if !self.catalog.contains_live(*id) {
                let error = if self.catalog.is_tombstoned(*id) {
                    CatalogError::TombstonedBufferId(*id)
                } else {
                    CatalogError::UnknownBuffer(*id)
                };
                return Err(WorkspaceError::Catalog(error));
            }
        }
        for (id, text) in updates {
            self.catalog
                .update(id, text)
                .expect("validated live buffer");
        }
        self.revision = next;
        Ok(())
    }

    /// Removes a buffer and advances the revision on success.
    pub fn remove(&mut self, id: BufferId) -> Result<Text, WorkspaceError> {
        let next = self
            .revision
            .next()
            .map_err(|_| WorkspaceError::RevisionOverflow)?;
        let text = self.catalog.remove(id).map_err(WorkspaceError::Catalog)?;
        self.revision = next;
        Ok(text)
    }
}

/// A revision and a private, immutable copy of its catalog.
#[derive(Clone, Debug)]
pub struct WorkspaceSnapshot {
    revision: Revision,
    catalog: BufferCatalog,
    origin: Arc<()>,
}

impl PartialEq for WorkspaceSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.revision == other.revision
            && self.catalog == other.catalog
            && Arc::ptr_eq(&self.origin, &other.origin)
    }
}

impl Eq for WorkspaceSnapshot {}

impl WorkspaceSnapshot {
    /// Returns the revision captured with this catalog.
    pub fn revision(&self) -> Revision {
        self.revision
    }

    /// Returns the catalog captured with this snapshot's revision.
    pub fn catalog(&self) -> &BufferCatalog {
        &self.catalog
    }
}

/// A snapshot cannot be recorded at or below the last recorded revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotError {
    /// The snapshot belongs to a different workspace than this history.
    DifferentWorkspace,
    /// The proposed revision is duplicate or regressive.
    NonIncreasingRevision {
        /// Highest revision ever recorded in this store.
        last: Revision,
        /// Revision of the rejected snapshot.
        attempted: Revision,
    },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DifferentWorkspace => f.write_str("snapshot belongs to a different workspace"),
            Self::NonIncreasingRevision { last, attempted } => {
                write!(
                    f,
                    "snapshot revision {attempted} is not greater than {last}"
                )
            }
        }
    }
}

impl std::error::Error for SnapshotError {}

/// Revision-ordered snapshots with an enduring recording high-water mark.
#[derive(Clone, Debug, Default)]
pub struct SnapshotStore {
    snapshots: BTreeMap<Revision, WorkspaceSnapshot>,
    last_recorded: Option<Revision>,
    origin: Option<Arc<()>>,
}

impl PartialEq for SnapshotStore {
    fn eq(&self, other: &Self) -> bool {
        let same_origin = match (&self.origin, &other.origin) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        same_origin
            && self.last_recorded == other.last_recorded
            && self.snapshots == other.snapshots
    }
}

impl Eq for SnapshotStore {}

impl SnapshotStore {
    /// Creates an empty history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a capture from this history's workspace at a strictly newer revision.
    pub fn record(&mut self, snapshot: WorkspaceSnapshot) -> Result<(), SnapshotError> {
        if self
            .origin
            .as_ref()
            .is_some_and(|origin| !Arc::ptr_eq(origin, &snapshot.origin))
        {
            return Err(SnapshotError::DifferentWorkspace);
        }
        let attempted = snapshot.revision();
        if let Some(last) = self.last_recorded {
            if attempted <= last {
                return Err(SnapshotError::NonIncreasingRevision { last, attempted });
            }
        }
        if self.origin.is_none() {
            self.origin = Some(Arc::clone(&snapshot.origin));
        }
        self.snapshots.insert(attempted, snapshot);
        self.last_recorded = Some(attempted);
        Ok(())
    }

    /// Returns the highest retained snapshot, or `None` when empty.
    pub fn latest(&self) -> Option<&WorkspaceSnapshot> {
        self.snapshots
            .last_key_value()
            .map(|(_, snapshot)| snapshot)
    }

    /// Returns the retained snapshot at exactly `revision`.
    pub fn get(&self, revision: Revision) -> Option<&WorkspaceSnapshot> {
        self.snapshots.get(&revision)
    }

    /// Removes snapshots strictly older than `floor`.
    pub fn prune_before(&mut self, floor: Revision) {
        self.snapshots = self.snapshots.split_off(&floor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::edit::ids::BufferId;
    use crate::editor::text::utf8::Text;

    #[test]
    fn batch_is_atomic_and_preserves_tombstones() {
        let a = BufferId::new(1);
        let b = BufferId::new(2);
        let dead = BufferId::new(3);
        let mut catalog = BufferCatalog::new();
        for id in [a, b, dead] {
            catalog.register(id, Text::from_str("old")).unwrap();
        }
        catalog.remove(dead).unwrap();
        let mut workspace = VersionedWorkspace::new(catalog);
        let before = workspace.capture();
        for invalid in [dead, BufferId::new(4), a] {
            assert!(workspace
                .update_batch([(a, Text::from_str("new")), (invalid, Text::empty())])
                .is_err());
            assert_eq!(workspace.capture(), before);
        }
        workspace
            .update_batch([(a, Text::from_str("A")), (b, Text::from_str("B"))])
            .unwrap();
        assert_eq!(workspace.revision(), Revision::new(1));
        assert_eq!(workspace.catalog().get(a).unwrap().as_str(), "A");
        assert_eq!(workspace.catalog().get(b).unwrap().as_str(), "B");
        assert!(workspace.catalog().is_tombstoned(dead));
        workspace.revision = Revision::MAX;
        let before = workspace.capture();
        assert_eq!(
            workspace.update_batch([(a, Text::empty())]),
            Err(WorkspaceError::RevisionOverflow)
        );
        assert_eq!(workspace.capture(), before);
    }

    #[test]
    fn exhaustive_catalog_and_history_sequences() {
        // Each base-8 sequence explores mutations, saved captures and pruning.
        let mut regressions = 0;
        let mut partial_prunes = 0;
        for mut sequence in 0..8_u32.pow(6) {
            let mut workspace = VersionedWorkspace::new(BufferCatalog::new());
            let mut store = SnapshotStore::new();
            let mut expected = BTreeMap::new();
            let mut high = None;
            let mut expected_revision = Revision::ZERO;
            let mut saved = (
                workspace.capture(),
                expected_revision,
                workspace.catalog().clone(),
            );
            let id = BufferId::new(1);
            for _ in 0..6 {
                let op = sequence % 8;
                sequence /= 8;
                let before = workspace.capture();
                match op {
                    0 => {
                        if workspace.register(id, Text::from_str("first")).is_ok() {
                            expected_revision = expected_revision.next().unwrap();
                        } else {
                            assert_eq!(workspace.capture(), before);
                        }
                    }
                    1 => {
                        if workspace.update(id, Text::from_str("updated")).is_ok() {
                            expected_revision = expected_revision.next().unwrap();
                        } else {
                            assert_eq!(workspace.capture(), before);
                        }
                    }
                    2 => {
                        if workspace.remove(id).is_ok() {
                            expected_revision = expected_revision.next().unwrap();
                        } else {
                            assert_eq!(workspace.capture(), before);
                        }
                    }
                    3 | 5 => {
                        let (snapshot, revision, catalog) = if op == 3 {
                            (
                                workspace.capture(),
                                expected_revision,
                                workspace.catalog().clone(),
                            )
                        } else {
                            saved.clone()
                        };
                        assert_eq!(snapshot.revision(), revision);
                        let before = store.clone();
                        let result = store.record(snapshot);
                        if let Some(last) = high.filter(|last| revision <= *last) {
                            if revision < last {
                                regressions += 1;
                            }
                            assert_eq!(
                                result,
                                Err(SnapshotError::NonIncreasingRevision {
                                    last,
                                    attempted: revision
                                })
                            );
                            assert_eq!(store, before);
                        } else {
                            assert_eq!(result, Ok(()));
                            high = Some(revision);
                            expected.insert(revision, catalog);
                        }
                    }
                    4 => {
                        saved = (
                            workspace.capture(),
                            expected_revision,
                            workspace.catalog().clone(),
                        )
                    }
                    _ => {
                        let floor = if op == 6 {
                            Revision::new(workspace.revision().raw() / 2)
                        } else {
                            workspace.revision()
                        };
                        if expected.keys().any(|r| *r < floor)
                            && expected.keys().any(|r| *r >= floor)
                        {
                            partial_prunes += 1;
                        }
                        store.prune_before(floor);
                        expected.retain(|revision, _| *revision >= floor);
                    }
                }
                assert_eq!(workspace.revision(), expected_revision);
                assert_eq!(workspace.capture().revision(), expected_revision);
                assert_eq!(
                    store.latest().map(WorkspaceSnapshot::revision),
                    expected.last_key_value().map(|(revision, _)| *revision)
                );
                assert_eq!(
                    store.latest().map(WorkspaceSnapshot::catalog),
                    expected.last_key_value().map(|(_, catalog)| catalog)
                );
                for n in 0..=7 {
                    let revision = Revision::new(n);
                    let actual = store.get(revision);
                    assert_eq!(
                        actual.map(WorkspaceSnapshot::revision),
                        expected.get(&revision).map(|_| revision)
                    );
                    assert_eq!(
                        actual.map(WorkspaceSnapshot::catalog),
                        expected.get(&revision)
                    );
                }
            }
        }
        assert!(regressions > 0);
        assert!(partial_prunes > 0);
    }

    #[test]
    fn old_revision_cannot_capture_new_catalog() {
        let id = BufferId::new(0);
        let mut catalog = BufferCatalog::new();
        catalog.register(id, Text::from_str("old")).unwrap();
        let mut workspace = VersionedWorkspace::new(catalog);
        let old = workspace.capture();
        workspace.update(id, Text::from_str("new")).unwrap();
        let new = workspace.capture();
        let mut store = SnapshotStore::new();
        store.record(new).unwrap();
        assert_eq!(store.get(Revision::ZERO), None);
        assert_eq!(old.catalog().get(id).unwrap().as_str(), "old");
        assert_eq!(
            store
                .get(Revision::new(1))
                .unwrap()
                .catalog()
                .get(id)
                .unwrap()
                .as_str(),
            "new"
        );
        assert_eq!(
            store.record(old),
            Err(SnapshotError::NonIncreasingRevision {
                last: Revision::new(1),
                attempted: Revision::ZERO
            })
        );
    }

    #[test]
    fn store_rejects_foreign_workspace_even_after_pruning() {
        let id = BufferId::new(0);
        let mut catalog = BufferCatalog::new();
        catalog.register(id, Text::from_str("old")).unwrap();
        let mut original = VersionedWorkspace::new(catalog);
        let zero = original.capture();
        original.update(id, Text::from_str("old")).unwrap();
        let one = original.capture();
        original.update(id, Text::from_str("new")).unwrap();
        let mut foreign = VersionedWorkspace::new(original.catalog().clone());
        foreign.update(id, Text::from_str("new")).unwrap();
        let mut store = SnapshotStore::new();
        store.record(zero).unwrap();
        assert_eq!(
            store.record(foreign.capture()),
            Err(SnapshotError::DifferentWorkspace)
        );
        store.record(one).unwrap();
        store.record(original.capture()).unwrap();
        assert_eq!(
            store
                .get(Revision::new(1))
                .unwrap()
                .catalog()
                .get(id)
                .unwrap()
                .as_str(),
            "old"
        );
        store.prune_before(Revision::MAX);
        assert_eq!(
            store.record(foreign.capture()),
            Err(SnapshotError::DifferentWorkspace)
        );
    }

    #[test]
    fn equality_preserves_workspace_identity_after_pruning() {
        let a = VersionedWorkspace::new(BufferCatalog::new());
        let b = VersionedWorkspace::new(BufferCatalog::new());
        let mut first = SnapshotStore::new();
        let mut second = SnapshotStore::new();
        first.record(a.capture()).unwrap();
        second.record(b.capture()).unwrap();
        assert_ne!(a.capture(), b.capture());
        assert_ne!(first, second);
        assert_eq!(first, first.clone());
        first.prune_before(Revision::new(1));
        second.prune_before(Revision::new(1));
        assert_ne!(first, second);
        assert_eq!(first, first.clone());
    }

    #[test]
    fn pruning_preserves_floor_and_newer_captures() {
        let id = BufferId::new(0);
        let mut catalog = BufferCatalog::new();
        catalog.register(id, Text::from_str("zero")).unwrap();
        let mut workspace = VersionedWorkspace::new(catalog);
        let mut store = SnapshotStore::new();
        store.record(workspace.capture()).unwrap();
        workspace.update(id, Text::from_str("one")).unwrap();
        let one = workspace.capture();
        store.record(one.clone()).unwrap();
        workspace.update(id, Text::from_str("two")).unwrap();
        let two = workspace.capture();
        store.record(two.clone()).unwrap();
        store.prune_before(Revision::new(1));
        assert_eq!(store.get(Revision::ZERO), None);
        assert_eq!(store.get(Revision::new(1)), Some(&one));
        assert_eq!(store.get(Revision::new(2)), Some(&two));
        assert_eq!(store.latest(), Some(&two));
    }

    #[test]
    fn overflow_leaves_workspace_unchanged() {
        let id = BufferId::new(0);
        let mut catalog = BufferCatalog::new();
        catalog.register(id, Text::from_str("old")).unwrap();
        let mut workspace = VersionedWorkspace {
            revision: Revision::MAX,
            catalog,
            origin: Arc::new(()),
        };
        let before = workspace.capture();
        assert_eq!(
            workspace.update(id, Text::from_str("new")),
            Err(WorkspaceError::RevisionOverflow)
        );
        assert_eq!(workspace.remove(id), Err(WorkspaceError::RevisionOverflow));
        assert_eq!(
            workspace.register(BufferId::new(1), Text::empty()),
            Err(WorkspaceError::RevisionOverflow)
        );
        assert_eq!(workspace.capture(), before);
    }

    #[test]
    fn capture_preserves_live_content_and_tombstones() {
        let mut initial = BufferCatalog::new();
        let a = BufferId::new(0);
        let b = BufferId::new(1);
        initial.register(a, Text::from_str("old")).unwrap();
        let mut live = VersionedWorkspace::new(initial);
        let first = live.capture();
        live.update(a, Text::from_str("new")).unwrap();
        live.register(b, Text::from_str("other")).unwrap();
        let second = live.capture();
        live.remove(a).unwrap();
        let third = live.capture();
        live.remove(b).unwrap();
        assert_eq!(first.catalog().get(a).unwrap().as_str(), "old");
        assert!(!first.catalog().contains_live(b));
        assert_eq!(second.catalog().get(a).unwrap().as_str(), "new");
        assert!(second.catalog().contains_live(b));
        assert!(third.catalog().is_tombstoned(a));
        assert!(third.catalog().contains_live(b));
        assert_eq!(
            (first.revision(), second.revision(), third.revision()),
            (Revision::ZERO, Revision::new(2), Revision::new(3))
        );
    }

    #[test]
    fn maximum_revision_and_pruning_all_preserve_high_water() {
        let catalog = BufferCatalog::new();
        let mut workspace = VersionedWorkspace {
            revision: Revision::new(3),
            catalog,
            origin: Arc::new(()),
        };
        let mut store = SnapshotStore::new();
        store.record(workspace.capture()).unwrap();
        store.prune_before(Revision::MAX);
        assert_eq!(store.latest(), None);
        assert_eq!(
            store.record(workspace.capture()),
            Err(SnapshotError::NonIncreasingRevision {
                last: Revision::new(3),
                attempted: Revision::new(3)
            })
        );
        workspace.revision = Revision::MAX;
        store.record(workspace.capture()).unwrap();
        store.prune_before(Revision::MAX);
        assert_eq!(store.latest().unwrap().revision(), Revision::MAX);
        assert_eq!(
            store.record(workspace.capture()),
            Err(SnapshotError::NonIncreasingRevision {
                last: Revision::MAX,
                attempted: Revision::MAX
            })
        );
    }
}
