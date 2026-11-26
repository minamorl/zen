//! Actor-private named anchors in revisioned workspace snapshots.
//!
//! Maps describe byte geometry, not edit provenance. The caller must supply
//! the map for the actual edit history; intermediate UTF-8 boundaries can only
//! be checked when the intermediate snapshots are supplied.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use crate::editor::edit::ids::{ActorId, BufferId};
use crate::editor::text::anchors::Anchor;
use crate::editor::text::position_map::PositionMapError;
use crate::editor::text::utf8::{Text, Utf8Error};
use crate::editor::workspace::annotation_mapping::RevisionedMap;
use crate::editor::workspace::buffer_lifecycle::{LifecycleEvent, LifecycleKind};
use crate::editor::workspace::snapshots::WorkspaceSnapshot;
use crate::editor::workspace::version::Revision;

/// A nonempty bookmark name, scoped to an actor.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct BookmarkName(String);

impl BookmarkName {
    /// Creates a name; empty names are rejected.
    pub fn new(name: impl Into<String>) -> Result<Self, BookmarkError> {
        let name = name.into();
        if name.is_empty() {
            return Err(BookmarkError::InvalidName);
        }
        Ok(Self(name))
    }

    /// Returns the name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};
    use crate::editor::text::anchors::Affinity;
    use crate::editor::text::position_map::{ByteReplacement, PositionMap};
    use crate::editor::workspace::buffer_lifecycle::{BufferLifecycle, ClosePolicy};
    use crate::editor::workspace::catalog::BufferCatalog;
    use crate::editor::workspace::snapshots::VersionedWorkspace;

    fn key(actor: u64, name: &str) -> BookmarkKey {
        BookmarkKey {
            actor: ActorId::new(actor),
            name: BookmarkName::new(name).unwrap(),
        }
    }

    fn bookmark(
        snapshot: &WorkspaceSnapshot,
        buffer: BufferId,
        offset: usize,
        affinity: Affinity,
    ) -> Bookmark {
        Bookmark {
            buffer,
            anchor: Anchor {
                offset: snapshot
                    .catalog()
                    .get(buffer)
                    .unwrap()
                    .validate_offset(offset)
                    .unwrap(),
                affinity,
            },
            revision: snapshot.revision(),
        }
    }

    #[test]
    fn distinct_keys_commute() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| (rng.next_u64(), rng.next_u64()),
            |&(a, b)| {
                let id = BufferId::new(1);
                let mut catalog = BufferCatalog::new();
                catalog.register(id, Text::from_str("abc")).unwrap();
                let snapshot = VersionedWorkspace::new(catalog).capture();
                let keys = [key(a, "one"), key(b, "two")];
                let values = [
                    bookmark(&snapshot, id, 0, Affinity::Left),
                    bookmark(&snapshot, id, 3, Affinity::Right),
                ];
                let mut left = BookmarkStore::new();
                let mut right = BookmarkStore::new();
                for i in [0, 1] {
                    left.set(&snapshot, keys[i].clone(), values[i]).unwrap();
                }
                for i in [1, 0] {
                    right.set(&snapshot, keys[i].clone(), values[i]).unwrap();
                }
                assert_eq!(left, right);
                for i in [0, 1] {
                    left.set(&snapshot, keys[i].clone(), values[1 - i]).unwrap();
                }
                for i in [1, 0] {
                    right
                        .set(&snapshot, keys[i].clone(), values[1 - i])
                        .unwrap();
                }
                assert_eq!(left, right);
                left.remove(&keys[0]);
                left.remove(&keys[1]);
                right.remove(&keys[1]);
                right.remove(&keys[0]);
                assert_eq!(left, right);
                assert!(left.get(&keys[0]).is_none());
                assert!(left.get(&keys[1]).is_none());
                let keys = [key(0, "same"), key(1, "same")];
                for i in [0, 1] {
                    left.set(&snapshot, keys[i].clone(), values[i]).unwrap();
                }
                for i in [1, 0] {
                    right.set(&snapshot, keys[i].clone(), values[i]).unwrap();
                }
                assert_eq!(left, right);
                assert_eq!(left.get(&keys[0]), Some(&values[0]));
                assert_eq!(left.get(&keys[1]), Some(&values[1]));
                for i in [0, 1] {
                    left.set(&snapshot, keys[i].clone(), values[1 - i]).unwrap();
                }
                for i in [1, 0] {
                    right
                        .set(&snapshot, keys[i].clone(), values[1 - i])
                        .unwrap();
                }
                assert_eq!(left, right);
                assert_eq!(left.get(&keys[0]), Some(&values[1]));
                assert_eq!(left.get(&keys[1]), Some(&values[0]));
                for i in [0, 1] {
                    left.remove(&keys[i]);
                }
                for i in [1, 0] {
                    right.remove(&keys[i]);
                }
                assert_eq!(left, right);
                assert!(left.get(&keys[0]).is_none() && left.get(&keys[1]).is_none());
            },
        );
    }

    #[test]
    fn tracking_composes() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let id = BufferId::new(1);
                let other = BufferId::new(2);
                let text = Text::from_str(source);
                let bounds = text.char_boundaries();
                for (index, start) in bounds.iter().enumerate() {
                    let end = bounds[(index + 1).min(bounds.len() - 1)].as_usize();
                    let start = start.as_usize();
                    for inserted in ["", "x", "界"] {
                        let mut catalog = BufferCatalog::new();
                        catalog.register(id, Text::from_str(source)).unwrap();
                        catalog.register(other, Text::from_str("other")).unwrap();
                        let mut workspace = VersionedWorkspace::new(catalog);
                        let before = workspace.capture();
                        let middle = format!("{}{}{}", &source[..start], inserted, &source[end..]);
                        let first = PositionMap::from_replacement(
                            source.len(),
                            ByteReplacement {
                                start,
                                end,
                                inserted_len: inserted.len(),
                            },
                        )
                        .unwrap();
                        workspace.update(id, Text::from_str(&middle)).unwrap();
                        let mid = workspace.capture();
                        let second = PositionMap::from_replacement(
                            middle.len(),
                            ByteReplacement {
                                start: 0,
                                end: 0,
                                inserted_len: "😀".len(),
                            },
                        )
                        .unwrap();
                        workspace
                            .update(id, Text::from_str(&format!("😀{middle}")))
                            .unwrap();
                        let after = workspace.capture();
                        let m1 = RevisionedMap {
                            map: &first,
                            source: before.revision(),
                            target: mid.revision(),
                        };
                        let m2 = RevisionedMap {
                            map: &second,
                            source: mid.revision(),
                            target: after.revision(),
                        };
                        let composed = m1.compose(&m2).unwrap();
                        for offset in &bounds {
                            for affinity in [Affinity::Left, Affinity::Right] {
                                let mut sequential = BookmarkStore::new();
                                sequential
                                    .set(
                                        &before,
                                        key(0, "target"),
                                        bookmark(&before, id, offset.as_usize(), affinity),
                                    )
                                    .unwrap();
                                sequential
                                    .set(
                                        &before,
                                        key(0, "other"),
                                        bookmark(&before, other, 2, affinity),
                                    )
                                    .unwrap();
                                let mut direct = sequential.clone();
                                sequential.advance(&before, &mid, id, &m1).unwrap();
                                let p = offset.as_usize();
                                let expected = if p < start {
                                    p
                                } else if p > end {
                                    p - (end - start) + inserted.len()
                                } else if affinity == Affinity::Left {
                                    start
                                } else {
                                    start + inserted.len()
                                };
                                assert_eq!(
                                    sequential
                                        .get(&key(0, "target"))
                                        .unwrap()
                                        .anchor
                                        .offset
                                        .as_usize(),
                                    expected
                                );
                                sequential.advance(&mid, &after, id, &m2).unwrap();
                                let expected = if expected == 0 && affinity == Affinity::Left {
                                    0
                                } else {
                                    expected + "😀".len()
                                };
                                assert_eq!(
                                    sequential
                                        .get(&key(0, "target"))
                                        .unwrap()
                                        .anchor
                                        .offset
                                        .as_usize(),
                                    expected
                                );
                                direct
                                    .advance(&before, &after, id, &composed.as_revisioned())
                                    .unwrap();
                                assert_eq!(sequential, direct);
                                for store in [&sequential, &direct] {
                                    let unchanged = store.get(&key(0, "other")).unwrap();
                                    assert_eq!(unchanged.anchor.offset.as_usize(), 2);
                                    assert_eq!(unchanged.revision, after.revision());
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn closure_and_invalid_targets_are_atomic() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| rng.next_u64(),
            |&actor| {
                let mut lifecycle = BufferLifecycle::new();
                let (a, b, missing) = (BufferId::new(1), BufferId::new(2), BufferId::new(3));
                lifecycle.open(a, Text::from_str("界")).unwrap();
                lifecycle.open(b, Text::from_str("live")).unwrap();
                let before = lifecycle.snapshot();
                let mut store = BookmarkStore::new();
                let ka = key(actor, "a");
                let kb = key(actor, "b");
                store
                    .set(&before, ka.clone(), bookmark(&before, a, 0, Affinity::Left))
                    .unwrap();
                store
                    .set(
                        &before,
                        kb.clone(),
                        bookmark(&before, b, 1, Affinity::Right),
                    )
                    .unwrap();
                let saved = store.clone();
                let foreign = Text::from_str("abcd");
                for (buffer, offset) in [(missing, 0), (a, 1), (a, 4)] {
                    let invalid = Bookmark {
                        buffer,
                        anchor: Anchor {
                            offset: foreign.validate_offset(offset).unwrap(),
                            affinity: Affinity::Left,
                        },
                        revision: before.revision(),
                    };
                    assert!(store.set(&before, ka.clone(), invalid).is_err());
                    assert_eq!(store, saved);
                }
                let mut stale = bookmark(&before, a, 0, Affinity::Left);
                stale.revision = Revision::ZERO;
                assert!(matches!(
                    store.set(&before, ka.clone(), stale),
                    Err(BookmarkError::RevisionMismatch { .. })
                ));
                assert_eq!(store, saved);
                // A bookmark may predate the close even if other keys are current.
                lifecycle.update(b, Text::from_str("live")).unwrap();
                let newer = lifecycle.snapshot();
                store
                    .set(&newer, kb.clone(), bookmark(&newer, b, 1, Affinity::Right))
                    .unwrap();
                lifecycle.close(a, ClosePolicy::RequireClean).unwrap();
                let after = lifecycle.snapshot();
                store
                    .close(&newer, &after, *lifecycle.events().last().unwrap())
                    .unwrap();
                assert_eq!(store.get(&ka), None);
                assert_eq!(store.get(&kb).unwrap().revision, after.revision());
                let saved = store.clone();
                let mut closed = bookmark(&after, b, 0, Affinity::Left);
                closed.buffer = a;
                assert_eq!(
                    store.set(&after, ka, closed),
                    Err(BookmarkError::BufferClosed(a))
                );
                assert_eq!(store, saved);
            },
        );
    }

    #[test]
    fn batch_maps_multiple_buffers_atomically() {
        let (a, b) = (BufferId::new(1), BufferId::new(2));
        let mut catalog = BufferCatalog::new();
        for id in [a, b] {
            catalog.register(id, Text::from_str("ab")).unwrap();
        }
        let mut workspace = VersionedWorkspace::new(catalog);
        let before = workspace.capture();
        let mut store = BookmarkStore::new();
        for (id, name) in [(a, "a"), (b, "b")] {
            store
                .set(
                    &before,
                    key(0, name),
                    bookmark(&before, id, 1, Affinity::Right),
                )
                .unwrap();
        }
        workspace
            .update_batch([(a, Text::from_str("aXb")), (b, Text::from_str("aYb"))])
            .unwrap();
        let after = workspace.capture();
        let geometry = PositionMap::from_replacement(
            2,
            ByteReplacement {
                start: 1,
                end: 1,
                inserted_len: 1,
            },
        )
        .unwrap();
        let map = RevisionedMap {
            map: &geometry,
            source: before.revision(),
            target: after.revision(),
        };
        let original = store.clone();
        assert!(store.advance(&before, &after, a, &map).is_err());
        assert_eq!(store, original);
        store
            .advance_batch(&before, &after, &[(a, &map), (b, &map)])
            .unwrap();
        for name in ["a", "b"] {
            let actual = store.get(&key(0, name)).unwrap();
            assert_eq!(actual.anchor.offset.as_usize(), 2);
            assert_eq!(actual.revision, after.revision());
        }
    }

    #[test]
    fn composed_history_advances_intermediate_bookmark() {
        let id = BufferId::new(1);
        let other = BufferId::new(2);
        let mut catalog = BufferCatalog::new();
        catalog.register(id, Text::from_str("ab")).unwrap();
        catalog.register(other, Text::from_str("other")).unwrap();
        let mut workspace = VersionedWorkspace::new(catalog);
        let before = workspace.capture();
        workspace.update(id, Text::from_str("Xab")).unwrap();
        let middle = workspace.capture();
        workspace.update(id, Text::from_str("YXab")).unwrap();
        let after = workspace.capture();
        let first = PositionMap::from_replacement(
            2,
            ByteReplacement {
                start: 0,
                end: 0,
                inserted_len: 1,
            },
        )
        .unwrap();
        let second = PositionMap::from_replacement(
            3,
            ByteReplacement {
                start: 0,
                end: 0,
                inserted_len: 1,
            },
        )
        .unwrap();
        let composed = first.compose(&second).unwrap();
        let map = RevisionedMap {
            map: &composed,
            source: before.revision(),
            target: after.revision(),
        };
        let mut store = BookmarkStore::new();
        store
            .set(
                &middle,
                key(0, "mid"),
                bookmark(&middle, id, 1, Affinity::Right),
            )
            .unwrap();
        store
            .set(
                &before,
                key(0, "base"),
                bookmark(&before, id, 1, Affinity::Right),
            )
            .unwrap();
        let m1 = RevisionedMap {
            map: &first,
            source: before.revision(),
            target: middle.revision(),
        };
        let m2 = RevisionedMap {
            map: &second,
            source: middle.revision(),
            target: after.revision(),
        };
        let mut sequential = store.clone();
        sequential.advance(&before, &middle, id, &m1).unwrap();
        sequential.advance(&middle, &after, id, &m2).unwrap();
        let maps1 = [(id, &m1)];
        let maps2 = [(id, &m2)];
        let steps = [
            BookmarkStep {
                before: &before,
                after: &middle,
                maps: &maps1,
            },
            BookmarkStep {
                before: &middle,
                after: &after,
                maps: &maps2,
            },
        ];
        store.advance_sequence(&steps).unwrap();
        assert_eq!(store, sequential);
        assert_eq!(
            store.get(&key(0, "mid")).unwrap().anchor.offset.as_usize(),
            2
        );
        assert_eq!(
            store.get(&key(0, "base")).unwrap().anchor.offset.as_usize(),
            3
        );
        store.remove(&key(0, "mid"));
        store.remove(&key(0, "base"));
        store
            .set(
                &before,
                key(0, "edit"),
                bookmark(&before, id, 1, Affinity::Right),
            )
            .unwrap();
        store
            .set(
                &middle,
                key(0, "other"),
                bookmark(&middle, other, 3, Affinity::Right),
            )
            .unwrap();
        let mut sequential = store.clone();
        sequential.advance(&before, &middle, id, &m1).unwrap();
        sequential.advance(&middle, &after, id, &m2).unwrap();
        store.advance(&before, &after, id, &map).unwrap();
        assert_eq!(store, sequential);
        let unchanged = store.get(&key(0, "other")).unwrap();
        assert_eq!(unchanged.anchor.offset.as_usize(), 3);
        assert_eq!(unchanged.revision, after.revision());
    }

    #[test]
    fn close_invalidates_with_stale_survivor() {
        let mut lifecycle = BufferLifecycle::new();
        let (a, b) = (BufferId::new(1), BufferId::new(2));
        lifecycle.open(a, Text::from_str("abc")).unwrap();
        lifecycle.open(b, Text::from_str("abc")).unwrap();
        let initial = lifecycle.snapshot();
        let mut store = BookmarkStore::new();
        for (id, name) in [(a, "a"), (b, "b")] {
            store
                .set(
                    &initial,
                    key(0, name),
                    bookmark(&initial, id, 1, Affinity::Right),
                )
                .unwrap();
        }
        lifecycle.open(BufferId::new(3), Text::empty()).unwrap();
        let before = lifecycle.snapshot();
        lifecycle.close(a, ClosePolicy::RequireClean).unwrap();
        let after = lifecycle.snapshot();
        store
            .close(&before, &after, *lifecycle.events().last().unwrap())
            .unwrap();
        assert!(store.get(&key(0, "a")).is_none());
        let saved = store.clone();
        assert_eq!(
            store.set(
                &initial,
                key(0, "a"),
                bookmark(&initial, a, 1, Affinity::Right)
            ),
            Err(BookmarkError::BufferClosed(a))
        );
        assert_eq!(store, saved);
        assert_eq!(
            *store.get(&key(0, "b")).unwrap(),
            bookmark(&initial, b, 1, Affinity::Right)
        );
        let c = BufferId::new(3);
        store
            .set(&after, key(0, "c"), bookmark(&after, c, 0, Affinity::Right))
            .unwrap();
        lifecycle.update(c, Text::from_str("x")).unwrap();
        let updated = lifecycle.snapshot();
        let geometry = PositionMap::from_replacement(
            0,
            ByteReplacement {
                start: 0,
                end: 0,
                inserted_len: 1,
            },
        )
        .unwrap();
        let map = RevisionedMap {
            map: &geometry,
            source: after.revision(),
            target: updated.revision(),
        };
        store.advance(&after, &updated, c, &map).unwrap();
        assert_eq!(store.get(&key(0, "c")).unwrap().anchor.offset.as_usize(), 1);
        assert_eq!(
            store.get(&key(0, "c")).unwrap().revision,
            updated.revision()
        );
        assert_eq!(
            *store.get(&key(0, "b")).unwrap(),
            bookmark(&updated, b, 1, Affinity::Right)
        );
    }

    #[test]
    fn stale_unchanged_bookmark_advances_and_tombstone_removes_it() {
        let (edited, untouched, opened) = (BufferId::new(1), BufferId::new(2), BufferId::new(3));
        let mut catalog = BufferCatalog::new();
        catalog.register(edited, Text::from_str("ab")).unwrap();
        catalog.register(untouched, Text::from_str("xyz")).unwrap();
        let mut workspace = VersionedWorkspace::new(catalog);
        let initial = workspace.capture();
        let mut store = BookmarkStore::new();
        let mark = key(0, "untouched");
        store
            .set(
                &initial,
                mark.clone(),
                bookmark(&initial, untouched, 1, Affinity::Right),
            )
            .unwrap();
        workspace.register(opened, Text::empty()).unwrap();
        let before = workspace.capture();
        workspace.update(edited, Text::from_str("Xab")).unwrap();
        let after = workspace.capture();
        let geometry = PositionMap::from_replacement(
            2,
            ByteReplacement {
                start: 0,
                end: 0,
                inserted_len: 1,
            },
        )
        .unwrap();
        let map = RevisionedMap {
            map: &geometry,
            source: before.revision(),
            target: after.revision(),
        };
        store.advance(&before, &after, edited, &map).unwrap();
        assert_eq!(
            *store.get(&mark).unwrap(),
            bookmark(&after, untouched, 1, Affinity::Right)
        );
        workspace.remove(untouched).unwrap();
        let closed = workspace.capture();
        store.advance_batch(&after, &closed, &[]).unwrap();
        assert!(store.get(&mark).is_none());
        assert_eq!(
            store.set(
                &initial,
                mark,
                bookmark(&initial, untouched, 1, Affinity::Right)
            ),
            Err(BookmarkError::BufferClosed(untouched))
        );
    }

    #[test]
    fn composed_map_advances_buffer_opened_at_middle_revision() {
        let (edited, opened) = (BufferId::new(1), BufferId::new(2));
        let mut catalog = BufferCatalog::new();
        catalog.register(edited, Text::from_str("ab")).unwrap();
        let mut workspace = VersionedWorkspace::new(catalog);
        let before = workspace.capture();
        workspace.register(opened, Text::from_str("a界")).unwrap();
        let middle = workspace.capture();
        workspace.update(edited, Text::from_str("Xab")).unwrap();
        let after = workspace.capture();
        let first = PositionMap::identity(2);
        let second = PositionMap::from_replacement(
            2,
            ByteReplacement {
                start: 0,
                end: 0,
                inserted_len: 1,
            },
        )
        .unwrap();
        let m1 = RevisionedMap {
            map: &first,
            source: before.revision(),
            target: middle.revision(),
        };
        let m2 = RevisionedMap {
            map: &second,
            source: middle.revision(),
            target: after.revision(),
        };
        let composed = m1.compose(&m2).unwrap();
        let mut direct = BookmarkStore::new();
        direct
            .set(
                &middle,
                key(0, "opened"),
                bookmark(&middle, opened, 1, Affinity::Right),
            )
            .unwrap();
        let mut sequential = direct.clone();
        sequential.advance(&before, &middle, edited, &m1).unwrap();
        sequential.advance(&middle, &after, edited, &m2).unwrap();
        direct
            .advance(&before, &after, edited, &composed.as_revisioned())
            .unwrap();
        assert_eq!(direct, sequential);
        let result = direct.get(&key(0, "opened")).unwrap();
        assert_eq!(
            (result.revision, result.anchor.offset.as_usize()),
            (after.revision(), 1)
        );
    }

    #[test]
    fn composed_maps_advance_older_bookmark_in_mapped_buffer() {
        let (a, b) = (BufferId::new(1), BufferId::new(2));
        let mut catalog = BufferCatalog::new();
        catalog.register(a, Text::from_str("ab")).unwrap();
        let mut workspace = VersionedWorkspace::new(catalog);
        let initial = workspace.capture();
        let mark = key(0, "a");
        let mut direct = BookmarkStore::new();
        direct
            .set(
                &initial,
                mark.clone(),
                bookmark(&initial, a, 1, Affinity::Right),
            )
            .unwrap();
        workspace.register(b, Text::from_str("ab")).unwrap();
        let before = workspace.capture();
        workspace.update(b, Text::from_str("Xab")).unwrap();
        let middle = workspace.capture();
        workspace.update(a, Text::from_str("Xab")).unwrap();
        let after = workspace.capture();
        let insertion = PositionMap::from_replacement(
            2,
            ByteReplacement {
                start: 0,
                end: 0,
                inserted_len: 1,
            },
        )
        .unwrap();
        let first = RevisionedMap {
            map: &insertion,
            source: before.revision(),
            target: middle.revision(),
        };
        let second = RevisionedMap {
            map: &insertion,
            source: middle.revision(),
            target: after.revision(),
        };
        let a_identity = PositionMap::identity(2);
        let b_identity = PositionMap::identity(3);
        let a_first = RevisionedMap {
            map: &a_identity,
            source: before.revision(),
            target: middle.revision(),
        };
        let b_second = RevisionedMap {
            map: &b_identity,
            source: middle.revision(),
            target: after.revision(),
        };
        let a_composed = a_first.compose(&second).unwrap();
        let b_composed = first.compose(&b_second).unwrap();
        let saved = direct.clone();
        assert_eq!(
            direct.advance_batch(&before, &after, &[(a, &a_composed.as_revisioned())]),
            Err(BookmarkError::UnmappedBufferChanged(b))
        );
        assert_eq!(direct, saved);
        let mut sequential = direct.clone();
        sequential.advance(&before, &middle, b, &first).unwrap();
        sequential.advance(&middle, &after, a, &second).unwrap();
        direct
            .advance_batch(
                &before,
                &after,
                &[
                    (a, &a_composed.as_revisioned()),
                    (b, &b_composed.as_revisioned()),
                ],
            )
            .unwrap();
        let expected = bookmark(&after, a, 2, Affinity::Right);
        assert_eq!(sequential.get(&mark), Some(&expected));
        assert_eq!(direct.get(&mark), Some(&expected));
        assert_eq!(direct, sequential);
    }

    #[test]
    fn reverse_snapshot_order_is_rejected_atomically() {
        let (a, b) = (BufferId::new(1), BufferId::new(2));
        let mut workspace = VersionedWorkspace::new(BufferCatalog::new());
        workspace.register(a, Text::from_str("a")).unwrap();
        let rev0 = workspace.capture();
        workspace.register(b, Text::from_str("b")).unwrap();
        let rev1 = workspace.capture();
        let mut store = BookmarkStore::new();
        store
            .set(&rev1, key(0, "a"), bookmark(&rev1, a, 0, Affinity::Left))
            .unwrap();
        let saved = store.clone();
        assert_eq!(
            store.advance_batch(&rev1, &rev0, &[]),
            Err(BookmarkError::RevisionMismatch {
                expected: rev1.revision(),
                actual: rev0.revision(),
            })
        );
        assert_eq!(store, saved);
    }

    #[test]
    fn set_rejects_older_snapshot_for_existing_key_atomically() {
        let id = BufferId::new(1);
        let mut catalog = BufferCatalog::new();
        catalog.register(id, Text::from_str("abc")).unwrap();
        let mut workspace = VersionedWorkspace::new(catalog);
        let before = workspace.capture();
        let mark = key(0, "target");
        let stale = bookmark(&before, id, 1, Affinity::Right);
        let mut store = BookmarkStore::new();
        store.set(&before, mark.clone(), stale).unwrap();
        workspace.update(id, Text::from_str("界")).unwrap();
        let after = workspace.capture();
        let geometry = PositionMap::from_replacement(
            3,
            ByteReplacement {
                start: 0,
                end: 3,
                inserted_len: 3,
            },
        )
        .unwrap();
        let map = RevisionedMap {
            map: &geometry,
            source: before.revision(),
            target: after.revision(),
        };
        store.advance(&before, &after, id, &map).unwrap();
        let saved = store.clone();
        assert_eq!(
            store.set(&before, mark.clone(), stale),
            Err(BookmarkError::RevisionMismatch {
                expected: after.revision(),
                actual: before.revision()
            })
        );
        assert_eq!(
            store.get(&mark),
            Some(&bookmark(&after, id, 3, Affinity::Right))
        );
        assert_eq!(store, saved);
    }
}

/// The complete, actor-scoped identity of a bookmark.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct BookmarkKey {
    pub actor: ActorId,
    pub name: BookmarkName,
}

/// A live target at a particular workspace revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bookmark {
    pub buffer: BufferId,
    pub anchor: Anchor,
    pub revision: Revision,
}

/// A rejected bookmark operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BookmarkError {
    InvalidName,
    UnknownBuffer(BufferId),
    BufferClosed(BufferId),
    InvalidOffset(Utf8Error),
    RevisionMismatch {
        expected: Revision,
        actual: Revision,
    },
    LengthMismatch {
        expected: usize,
        actual: usize,
    },
    UnmappedBufferChanged(BufferId),
    InvalidLifecycleEvent,
    InvalidHistory,
    DuplicateMap(BufferId),
    PositionMap(PositionMapError),
}

impl fmt::Display for BookmarkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bookmark error: {self:?}")
    }
}
impl std::error::Error for BookmarkError {}

fn live_text(snapshot: &WorkspaceSnapshot, id: BufferId) -> Result<&Text, BookmarkError> {
    snapshot.catalog().get(id).ok_or_else(|| {
        if snapshot.catalog().is_tombstoned(id) {
            BookmarkError::BufferClosed(id)
        } else {
            BookmarkError::UnknownBuffer(id)
        }
    })
}

fn check_revision(expected: Revision, actual: Revision) -> Result<(), BookmarkError> {
    if expected != actual {
        return Err(BookmarkError::RevisionMismatch { expected, actual });
    }
    Ok(())
}

/// One revision interval and the maps for its edited buffers.
pub struct BookmarkStep<'a> {
    pub before: &'a WorkspaceSnapshot,
    pub after: &'a WorkspaceSnapshot,
    pub maps: &'a [(BufferId, &'a RevisionedMap<'a>)],
}

/// A deterministic bookmark table; changes to distinct keys commute.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BookmarkStore {
    entries: BTreeMap<BookmarkKey, Bookmark>,
    texts: BTreeMap<BookmarkKey, Text>,
    closed: BTreeSet<BufferId>,
}

impl BookmarkStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Validates and inserts or replaces a bookmark atomically.
    pub fn set(
        &mut self,
        snapshot: &WorkspaceSnapshot,
        key: BookmarkKey,
        bookmark: Bookmark,
    ) -> Result<(), BookmarkError> {
        if self.closed.contains(&bookmark.buffer) {
            return Err(BookmarkError::BufferClosed(bookmark.buffer));
        }
        check_revision(snapshot.revision(), bookmark.revision)?;
        if let Some(existing) = self.entries.get(&key) {
            if existing.revision > bookmark.revision {
                return Err(BookmarkError::RevisionMismatch {
                    expected: existing.revision,
                    actual: bookmark.revision,
                });
            }
        }
        let text = live_text(snapshot, bookmark.buffer)?;
        text.validate_offset(bookmark.anchor.offset.as_usize())
            .map_err(BookmarkError::InvalidOffset)?;
        self.texts.insert(key.clone(), text.clone());
        self.entries.insert(key, bookmark);
        Ok(())
    }

    /// Removes a key if present.
    pub fn remove(&mut self, key: &BookmarkKey) -> Option<Bookmark> {
        self.texts.remove(key);
        self.entries.remove(key)
    }

    /// Looks up a key.
    pub fn get(&self, key: &BookmarkKey) -> Option<&Bookmark> {
        self.entries.get(key)
    }

    /// Iterates in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&BookmarkKey, &Bookmark)> {
        self.entries.iter()
    }

    /// Advances entries through a map for one buffer, atomically.
    /// Older entries in unchanged buffers also advance when their stored text matches.
    /// Entries strictly inside the interval for a mapped buffer are rejected:
    /// a composed map does not expose revision boundaries of component edits.
    /// Every other live buffer present in both snapshots must be unchanged unless mapped.
    pub fn advance(
        &mut self,
        before: &WorkspaceSnapshot,
        after: &WorkspaceSnapshot,
        buffer: BufferId,
        map: &RevisionedMap<'_>,
    ) -> Result<(), BookmarkError> {
        self.advance_batch(before, after, &[(buffer, map)])
    }

    /// Advances maps for distinct buffers changed in the same revision.
    /// Every other live buffer present in both snapshots must be unchanged unless mapped.
    pub fn advance_batch(
        &mut self,
        before: &WorkspaceSnapshot,
        after: &WorkspaceSnapshot,
        maps: &[(BufferId, &RevisionedMap<'_>)],
    ) -> Result<(), BookmarkError> {
        if after.revision() < before.revision() {
            return Err(BookmarkError::RevisionMismatch {
                expected: before.revision(),
                actual: after.revision(),
            });
        }
        let mut by_buffer = BTreeMap::new();
        for &(buffer, map) in maps {
            if by_buffer.insert(buffer, map).is_some() {
                return Err(BookmarkError::DuplicateMap(buffer));
            }
            check_revision(map.source, before.revision())?;
            check_revision(map.target, after.revision())?;
            let source = live_text(before, buffer)?;
            let target = live_text(after, buffer)?;
            for (expected, actual) in [
                (map.map.source_len(), source.len_bytes()),
                (map.map.target_len(), target.len_bytes()),
            ] {
                if expected != actual {
                    return Err(BookmarkError::LengthMismatch { expected, actual });
                }
            }
        }
        for buffer in before.catalog().live_ids() {
            if !by_buffer.contains_key(&buffer)
                && after
                    .catalog()
                    .get(buffer)
                    .is_some_and(|after_text| before.catalog().get(buffer) != Some(after_text))
            {
                return Err(BookmarkError::UnmappedBufferChanged(buffer));
            }
        }
        let mut next = self.clone();
        let tombstones = after.catalog().tombstoned_ids();
        next.entries.retain(|_, bookmark| {
            bookmark.revision > after.revision() || !after.catalog().is_tombstoned(bookmark.buffer)
        });
        next.texts.retain(|key, _| next.entries.contains_key(key));
        next.closed.extend(tombstones);
        for (key, bookmark) in &mut next.entries {
            if bookmark.revision != before.revision() && bookmark.revision < after.revision() {
                if by_buffer.contains_key(&bookmark.buffer) {
                    if bookmark.revision > before.revision() {
                        return Err(BookmarkError::RevisionMismatch {
                            expected: before.revision(),
                            actual: bookmark.revision,
                        });
                    }
                    if next.texts[key] != *live_text(before, bookmark.buffer)? {
                        return Err(BookmarkError::UnmappedBufferChanged(bookmark.buffer));
                    }
                } else {
                    let old = &next.texts[key];
                    if bookmark.revision < before.revision()
                        && old != live_text(before, bookmark.buffer)?
                    {
                        return Err(BookmarkError::UnmappedBufferChanged(bookmark.buffer));
                    }
                    let new = live_text(after, bookmark.buffer)?;
                    if old != new {
                        return Err(BookmarkError::UnmappedBufferChanged(bookmark.buffer));
                    }
                    new.validate_offset(bookmark.anchor.offset.as_usize())
                        .map_err(BookmarkError::InvalidOffset)?;
                    bookmark.revision = after.revision();
                    next.texts.insert(key.clone(), new.clone());
                    continue;
                }
            }
            if bookmark.revision != before.revision()
                && (bookmark.revision > before.revision() || bookmark.revision >= after.revision())
            {
                continue;
            }
            let old = live_text(before, bookmark.buffer)?;
            old.validate_offset(bookmark.anchor.offset.as_usize())
                .map_err(BookmarkError::InvalidOffset)?;
            let new = live_text(after, bookmark.buffer)?;
            let offset = if let Some(map) = by_buffer.get(&bookmark.buffer) {
                map.map
                    .map_offset(bookmark.anchor.offset.as_usize(), bookmark.anchor.affinity)
                    .map_err(BookmarkError::PositionMap)?
            } else {
                if old != new {
                    return Err(BookmarkError::UnmappedBufferChanged(bookmark.buffer));
                }
                bookmark.anchor.offset.as_usize()
            };
            bookmark.anchor.offset = new
                .validate_offset(offset)
                .map_err(BookmarkError::InvalidOffset)?;
            bookmark.revision = after.revision();
            next.texts.insert(key.clone(), new.clone());
        }
        *self = next;
        Ok(())
    }

    /// Atomically applies a contiguous history, retaining revision boundaries
    /// so entries registered between edits use only their subsequent maps.
    pub fn advance_sequence(&mut self, steps: &[BookmarkStep<'_>]) -> Result<(), BookmarkError> {
        let mut next = self.clone();
        for (index, step) in steps.iter().enumerate() {
            if index > 0 && steps[index - 1].after != step.before {
                return Err(BookmarkError::InvalidHistory);
            }
            next.advance_batch(step.before, step.after, step.maps)?;
        }
        *self = next;
        Ok(())
    }

    /// Applies a close event; current surviving entries advance, stale ones retain their revision.
    pub fn close(
        &mut self,
        before: &WorkspaceSnapshot,
        after: &WorkspaceSnapshot,
        event: LifecycleEvent,
    ) -> Result<(), BookmarkError> {
        if after.revision() < before.revision() {
            return Err(BookmarkError::RevisionMismatch {
                expected: before.revision(),
                actual: after.revision(),
            });
        }
        check_revision(event.previous, before.revision())?;
        check_revision(event.revision, after.revision())?;
        if event.kind != LifecycleKind::Closed || !after.catalog().is_tombstoned(event.buffer) {
            return Err(BookmarkError::InvalidLifecycleEvent);
        }
        live_text(before, event.buffer)?;
        let mut next = self.clone();
        next.entries
            .retain(|_, bookmark| bookmark.buffer != event.buffer);
        next.texts.retain(|key, _| next.entries.contains_key(key));
        next.closed.insert(event.buffer);
        for (key, bookmark) in &mut next.entries {
            if bookmark.revision != before.revision() {
                continue;
            }
            let old = live_text(before, bookmark.buffer)?;
            old.validate_offset(bookmark.anchor.offset.as_usize())
                .map_err(BookmarkError::InvalidOffset)?;
            if old != live_text(after, bookmark.buffer)? {
                return Err(BookmarkError::UnmappedBufferChanged(bookmark.buffer));
            }
            bookmark.revision = after.revision();
            next.texts.insert(key.clone(), old.clone());
        }
        *self = next;
        Ok(())
    }
}
