//! Revisioned buffer lifecycle, with permanent tombstones and dirty-close protection.
use super::{
    catalog::{BufferCatalog, CatalogError},
    dirty::{DirtyTracker, SaveReceipt},
    snapshots::{VersionedWorkspace, WorkspaceError, WorkspaceSnapshot},
    version::Revision,
};
use crate::editor::{edit::ids::BufferId, text::utf8::Text};
use std::{collections::BTreeMap, fmt};

/// Policy for unsaved content when closing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClosePolicy {
    RequireClean,
    Discard,
}

/// A lifecycle transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleKind {
    Opened,
    Closed,
}

/// A lifecycle transition and its workspace revisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecycleEvent {
    pub buffer: BufferId,
    pub kind: LifecycleKind,
    pub previous: Revision,
    pub revision: Revision,
}

/// A rejected operation, which leaves the lifecycle unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleError {
    DirtyBuffer(BufferId),
    Workspace(WorkspaceError),
}
impl fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DirtyBuffer(id) => write!(f, "buffer {id} has unsaved changes"),
            Self::Workspace(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for LifecycleError {}

/// Owns the versioned catalog and one baseline tracker per live buffer.
#[derive(Debug)]
pub struct BufferLifecycle {
    workspace: VersionedWorkspace,
    dirty: BTreeMap<BufferId, DirtyTracker>,
    events: Vec<LifecycleEvent>,
}
impl Default for BufferLifecycle {
    fn default() -> Self {
        Self::new()
    }
}
impl BufferLifecycle {
    /// Creates an empty workspace.
    pub fn new() -> Self {
        Self {
            workspace: VersionedWorkspace::new(BufferCatalog::new()),
            dirty: BTreeMap::new(),
            events: Vec::new(),
        }
    }
    /// Captures the catalog and revision together.
    pub fn snapshot(&self) -> WorkspaceSnapshot {
        self.workspace.capture()
    }
    /// Returns lifecycle events in publication order.
    pub fn events(&self) -> &[LifecycleEvent] {
        &self.events
    }
    /// Opens a fresh buffer with a clean baseline.
    pub fn open(&mut self, id: BufferId, text: Text) -> Result<(), LifecycleError> {
        let previous = self.workspace.revision();
        self.workspace
            .register(id, text.clone())
            .map_err(LifecycleError::Workspace)?;
        let revision = self.workspace.revision();
        let mut tracker = DirtyTracker::new();
        tracker
            .acknowledge(SaveReceipt {
                buffer: id,
                version: revision,
                content: text,
            })
            .expect("fresh tracker accepts first receipt");
        self.dirty.insert(id, tracker);
        self.events.push(LifecycleEvent {
            buffer: id,
            kind: LifecycleKind::Opened,
            previous,
            revision,
        });
        Ok(())
    }
    fn live_text(&self, id: BufferId) -> Result<&Text, LifecycleError> {
        self.workspace.catalog().get(id).ok_or_else(|| {
            let error = if self.workspace.catalog().is_tombstoned(id) {
                CatalogError::TombstonedBufferId(id)
            } else {
                CatalogError::UnknownBuffer(id)
            };
            LifecycleError::Workspace(WorkspaceError::Catalog(error))
        })
    }
    /// Checks a live buffer against its saved content.
    pub fn is_dirty(&self, id: BufferId) -> Result<bool, LifecycleError> {
        let text = self.live_text(id)?;
        Ok(self.dirty[&id].is_dirty(id, text))
    }
    /// Updates a live buffer's content.
    pub fn update(&mut self, id: BufferId, text: Text) -> Result<(), LifecycleError> {
        self.live_text(id)?;
        self.workspace
            .update(id, text)
            .map_err(LifecycleError::Workspace)
    }
    /// Acknowledges the current content as saved.
    pub fn mark_saved(&mut self, id: BufferId) -> Result<(), LifecycleError> {
        let content = self.live_text(id)?.clone();
        self.dirty
            .get_mut(&id)
            .expect("live tracker")
            .acknowledge(SaveReceipt {
                buffer: id,
                version: self.workspace.revision(),
                content,
            })
            .expect("monotonic workspace revision");
        Ok(())
    }
    /// Closes a buffer, requiring explicit discard for unsaved content.
    pub fn close(&mut self, id: BufferId, policy: ClosePolicy) -> Result<Text, LifecycleError> {
        if self.is_dirty(id)? && policy == ClosePolicy::RequireClean {
            return Err(LifecycleError::DirtyBuffer(id));
        }
        let previous = self.workspace.revision();
        let text = self
            .workspace
            .remove(id)
            .map_err(LifecycleError::Workspace)?;
        self.dirty.remove(&id);
        self.events.push(LifecycleEvent {
            buffer: id,
            kind: LifecycleKind::Closed,
            previous,
            revision: self.workspace.revision(),
        });
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts, Rng};

    #[test]
    fn lifecycle_laws() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| texts(rng),
            |cases| {
                let mut lifecycle = BufferLifecycle::new();
                lifecycle
                    .open(BufferId::new(0), Text::from_str("keeper"))
                    .unwrap();
                for (index, value) in cases.iter().enumerate() {
                    let id = BufferId::new(index as u64 + 1);
                    let live = lifecycle.snapshot().catalog().live_ids();
                    let mut tombstones = lifecycle.snapshot().catalog().tombstoned_ids();
                    let previous = lifecycle.snapshot().revision();
                    let event_count = lifecycle.events().len();
                    lifecycle.open(id, Text::from_str(value)).unwrap();
                    assert_eq!(lifecycle.events().len(), event_count + 1);
                    assert!(!lifecycle.is_dirty(id).unwrap());
                    let opened = lifecycle.snapshot();
                    assert_eq!(opened.revision(), previous.next().unwrap());
                    assert_eq!(
                        lifecycle.events().last(),
                        Some(&LifecycleEvent {
                            buffer: id,
                            kind: LifecycleKind::Opened,
                            previous,
                            revision: opened.revision()
                        })
                    );
                    let opened_dirty = lifecycle.dirty.clone();
                    let events = lifecycle.events().to_vec();
                    assert_eq!(
                        lifecycle.open(id, Text::empty()),
                        Err(LifecycleError::Workspace(WorkspaceError::Catalog(
                            CatalogError::DuplicateBufferId(id)
                        )))
                    );
                    assert_eq!(lifecycle.snapshot(), opened);
                    assert_eq!(lifecycle.dirty, opened_dirty);
                    assert_eq!(lifecycle.events(), events);
                    lifecycle
                        .update(id, Text::from_str(&(value.clone() + "x")))
                        .unwrap();
                    let dirty_snapshot = lifecycle.snapshot();
                    let dirty = lifecycle.dirty.clone();
                    let events = lifecycle.events().to_vec();
                    assert_eq!(
                        lifecycle.close(id, ClosePolicy::RequireClean),
                        Err(LifecycleError::DirtyBuffer(id))
                    );
                    assert_eq!(lifecycle.snapshot(), dirty_snapshot);
                    assert_eq!(lifecycle.dirty, dirty);
                    assert_eq!(lifecycle.events(), events);
                    lifecycle.close(id, ClosePolicy::Discard).unwrap();
                    assert_eq!(lifecycle.events().len(), event_count + 2);
                    assert_eq!(&lifecycle.events()[..events.len()], events);
                    let closed = lifecycle.snapshot();
                    assert_eq!(closed.catalog().live_ids(), live);
                    tombstones.push(id);
                    tombstones.sort();
                    assert_eq!(closed.catalog().tombstoned_ids(), tombstones);
                    assert_eq!(closed.revision(), dirty_snapshot.revision().next().unwrap());
                    assert!(!lifecycle.dirty.contains_key(&id));
                    let mut expected_dirty = dirty.clone();
                    expected_dirty.remove(&id);
                    assert_eq!(lifecycle.dirty, expected_dirty);
                    let closed_dirty = lifecycle.dirty.clone();
                    let closed_events = lifecycle.events().to_vec();
                    let stale = LifecycleError::Workspace(WorkspaceError::Catalog(
                        CatalogError::TombstonedBufferId(id),
                    ));
                    assert_eq!(lifecycle.open(id, Text::empty()), Err(stale));
                    assert_eq!(lifecycle.is_dirty(id), Err(stale));
                    assert_eq!(lifecycle.update(id, Text::empty()), Err(stale));
                    assert_eq!(lifecycle.mark_saved(id), Err(stale));
                    assert_eq!(lifecycle.close(id, ClosePolicy::Discard), Err(stale));
                    assert_eq!(lifecycle.snapshot(), closed);
                    assert_eq!(lifecycle.dirty, closed_dirty);
                    assert_eq!(lifecycle.events(), closed_events);
                    let tail = &lifecycle.events()[lifecycle.events().len() - 2..];
                    assert_eq!(
                        tail[0],
                        LifecycleEvent {
                            buffer: id,
                            kind: LifecycleKind::Opened,
                            previous,
                            revision: previous.next().unwrap()
                        }
                    );
                    assert_eq!(
                        tail[1],
                        LifecycleEvent {
                            buffer: id,
                            kind: LifecycleKind::Closed,
                            previous: dirty_snapshot.revision(),
                            revision: dirty_snapshot.revision().next().unwrap()
                        }
                    );
                    assert!(tail[1].revision > tail[0].revision);
                    assert!(lifecycle
                        .events()
                        .windows(2)
                        .all(|pair| pair[0].revision < pair[1].revision));
                }
                let id = BufferId::new(1000);
                let previous = lifecycle.snapshot().revision();
                let events = lifecycle.events().to_vec();
                lifecycle.open(id, Text::empty()).unwrap();
                assert_eq!(lifecycle.snapshot().revision(), previous.next().unwrap());
                assert_eq!(lifecycle.events().len(), events.len() + 1);
                assert_eq!(&lifecycle.events()[..events.len()], events);
                assert_eq!(
                    lifecycle.events().last(),
                    Some(&LifecycleEvent {
                        buffer: id,
                        kind: LifecycleKind::Opened,
                        previous,
                        revision: previous.next().unwrap()
                    })
                );
                let opened_events = lifecycle.events().to_vec();
                lifecycle.update(id, Text::from_str("saved")).unwrap();
                lifecycle.mark_saved(id).unwrap();
                assert_eq!(lifecycle.events(), opened_events);
                assert!(!lifecycle.is_dirty(id).unwrap());
                let before = lifecycle.snapshot();
                let count = lifecycle.events().len();
                let mut expected_dirty = lifecycle.dirty.clone();
                expected_dirty.remove(&id);
                lifecycle.close(id, ClosePolicy::RequireClean).unwrap();
                assert_eq!(&lifecycle.events()[..opened_events.len()], opened_events);
                assert_eq!(lifecycle.dirty, expected_dirty);
                let after = lifecycle.snapshot();
                let mut expected_live = before.catalog().live_ids();
                expected_live.retain(|&buffer| buffer != id);
                let mut expected_tombstones = before.catalog().tombstoned_ids();
                expected_tombstones.push(id);
                expected_tombstones.sort();
                assert_eq!(after.catalog().live_ids(), expected_live);
                assert_eq!(after.catalog().tombstoned_ids(), expected_tombstones);
                assert!(!lifecycle.dirty.contains_key(&id));
                assert_eq!(after.revision(), before.revision().next().unwrap());
                assert_eq!(lifecycle.events().len(), count + 1);
                assert_eq!(
                    lifecycle.events().last(),
                    Some(&LifecycleEvent {
                        buffer: id,
                        kind: LifecycleKind::Closed,
                        previous: before.revision(),
                        revision: after.revision()
                    })
                );
                assert!(lifecycle
                    .events()
                    .windows(2)
                    .all(|pair| pair[0].revision < pair[1].revision));
            },
        );
    }
}
