//! Private actor state synchronized with verified workspace deliveries.

use std::collections::{BTreeMap, BTreeSet};

use crate::editor::{
    collab::{
        access::{AccessError, AccessPolicy, Capability},
        delivery::{replay, DeliveryEvent},
        registry::ActorRegistry,
    },
    edit::{
        ids::{ActorId, BufferId},
        patch::Patch,
        splice::Splice,
    },
    text::{
        anchors::{Affinity, ReplacementEffect},
        position_map::{ByteReplacement, PositionMap},
        utf8::Text,
    },
    workspace::{
        annotation_mapping::RevisionedMap,
        annotations::{Annotation, AnnotationKey, AnnotationStore},
        bookmarks::{Bookmark, BookmarkKey, BookmarkName, BookmarkStore},
        registers::{KillRing, RegisterEntry, RegisterName, RegisterStore},
        selections::Selection,
        snapshots::{SnapshotStore, WorkspaceSnapshot},
        version::Revision,
        view::BufferView,
        view_mapping::{clip_selection, map_selection, map_view},
    },
};

/// An operation was rejected without changing session state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionError {
    Access(AccessError),
    InvalidState,
    InvalidTransition,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::{
            checkpoints::{CheckpointOp, CheckpointState},
            delivery::DeliveryLog,
            proposal::RequestId,
            role::ActorRole,
            sequencer::Sequencer,
        },
        edit::{
            ids::Nonce, intent::EditIntent, patch::Patch, splice::Splice, transaction::Transaction,
        },
        test_support::assert_law,
        text::{anchors::Anchor, span::ByteSpan},
        workspace::{
            annotations::{AnnotationId, CollapsePolicy, NamespaceId},
            catalog::BufferCatalog,
            registers::RegisterKind,
            snapshots::VersionedWorkspace,
        },
    };

    fn actors() -> ActorRegistry {
        let mut registry = ActorRegistry::new();
        for n in 1..=2 {
            registry
                .register(ActorId::new(n), ActorRole::Human)
                .unwrap();
        }
        registry
    }

    fn catalog() -> BufferCatalog {
        let mut catalog = BufferCatalog::new();
        for n in 0..2 {
            catalog
                .register(BufferId::new(n), Text::from_str("abcd"))
                .unwrap();
        }
        catalog
    }

    fn policy() -> AccessPolicy {
        let mut policy = AccessPolicy::new();
        policy.grant_default(Capability::Read);
        policy
    }

    fn populate(session: &mut WorkspaceSession, actor: ActorId, buffer: BufferId, offset: usize) {
        let text = session.read(actor, buffer).unwrap().clone();
        let point = text.validate_offset(offset).unwrap();
        let view = BufferView::full(session.snapshot.catalog(), buffer).unwrap();
        let selection =
            Selection::new(session.snapshot.catalog(), &view, buffer, point, point).unwrap();
        session.set_view(actor, view).unwrap();
        session.set_selection(actor, selection).unwrap();
        session
            .set_bookmark(
                actor,
                mark(buffer),
                Bookmark {
                    buffer,
                    anchor: Anchor {
                        offset: point,
                        affinity: Affinity::Right,
                    },
                    revision: session.revision(),
                },
            )
            .unwrap();
        session
            .set_overlay(
                actor,
                Annotation {
                    key: AnnotationKey {
                        buffer,
                        namespace: NamespaceId::new(1),
                        id: AnnotationId::new(1),
                    },
                    owner: actor,
                    span: ByteSpan::empty_at(point),
                    start_affinity: Affinity::Right,
                    end_affinity: Affinity::Right,
                    priority: 0,
                    collapse: CollapsePolicy::RetainEmpty,
                    label: "private".into(),
                },
            )
            .unwrap();
        let entry = RegisterEntry {
            text: Text::from_str(if actor.raw() == 1 { "one" } else { "two" }),
            kind: RegisterKind::Charwise,
        };
        session
            .put_register(actor, RegisterName::new("a").unwrap(), entry.clone())
            .unwrap();
        session.push_kill(actor, entry).unwrap();
    }

    fn mark(buffer: BufferId) -> BookmarkName {
        BookmarkName::new(format!("mark{}", buffer.raw())).unwrap()
    }

    fn key(buffer: BufferId) -> AnnotationKey {
        AnnotationKey {
            buffer,
            namespace: NamespaceId::new(1),
            id: AnnotationId::new(1),
        }
    }

    fn assert_denied(session: &WorkspaceSession, actor: ActorId, buffer: BufferId) {
        assert!(session.read(actor, buffer).is_err());
        assert!(session.view(actor, buffer).is_err());
        assert!(session.selection(actor, buffer).is_err());
        assert!(session.bookmark(actor, buffer, &mark(buffer)).is_err());
        assert!(session.overlay(actor, key(buffer)).is_err());
    }

    fn commit(
        seq: &mut Sequencer,
        log: &mut DeliveryLog,
        nonce: u64,
        buffer: BufferId,
        insert: &str,
    ) -> DeliveryEvent {
        let snapshot = seq.snapshot();
        let text = snapshot.catalog().get(buffer).unwrap();
        let patch = Patch::new(vec![Splice::insert(
            text.validate_offset(0).unwrap(),
            Text::from_str(insert),
        )])
        .unwrap();
        let transaction = Transaction::new(
            EditIntent::for_actor(ActorId::new(1), Nonce::new(nonce), snapshot.revision()),
            vec![(buffer, patch)],
        )
        .unwrap();
        let receipt = seq.submit(transaction).committed_receipt().unwrap();
        log.record_workspace(seq, receipt).unwrap();
        log.events().last().unwrap().clone()
    }

    #[test]
    fn actor_operations_commute() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| (rng.next_u64(), rng.next_u64()),
            |&(a, b)| {
                let snapshot = VersionedWorkspace::new(catalog()).capture();
                let mut left = WorkspaceSession::new(snapshot, actors(), policy());
                let mut right = left.clone();
                let first = ActorId::new(1);
                let second = ActorId::new(2);
                populate(&mut left, first, BufferId::new(a % 2), (a % 5) as usize);
                populate(&mut left, second, BufferId::new(b % 2), (b % 5) as usize);
                populate(&mut right, second, BufferId::new(b % 2), (b % 5) as usize);
                populate(&mut right, first, BufferId::new(a % 2), (a % 5) as usize);
                assert_eq!(left, right);
                let name = RegisterName::new("a").unwrap();
                for (actor, value) in [(first, "one"), (second, "two")] {
                    assert_eq!(left.register(actor, &name).unwrap().text.as_str(), value);
                    assert_eq!(left.current_kill(actor).unwrap().text.as_str(), value);
                }
            },
        );
    }

    #[test]
    fn multiple_splices_project_both_actors_and_buffers() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| rng.next_u64(),
            |&choice| {
                let mut seq = Sequencer::new(catalog(), actors());
                let mut log = DeliveryLog::new(seq.snapshot());
                let mut session = WorkspaceSession::new(seq.snapshot(), actors(), policy());
                for actor in [ActorId::new(1), ActorId::new(2)] {
                    for buffer in [BufferId::new(0), BufferId::new(1)] {
                        populate(&mut session, actor, buffer, 2);
                    }
                }
                let before = seq.snapshot();
                let first = if choice & 1 == 0 { "x" } else { "yz" };
                let last = if choice & 2 == 0 { "p" } else { "qr" };
                let patches = [BufferId::new(0), BufferId::new(1)]
                    .into_iter()
                    .map(|buffer| {
                        let text = before.catalog().get(buffer).unwrap();
                        (
                            buffer,
                            Patch::new(vec![
                                Splice::insert(
                                    text.validate_offset(0).unwrap(),
                                    Text::from_str(first),
                                ),
                                Splice::insert(
                                    text.validate_offset(4).unwrap(),
                                    Text::from_str(last),
                                ),
                            ])
                            .unwrap(),
                        )
                    })
                    .collect();
                let txn = Transaction::new(
                    EditIntent::for_actor(ActorId::new(1), Nonce::new(0), before.revision()),
                    patches,
                )
                .unwrap();
                let receipt = seq.submit(txn).committed_receipt().unwrap();
                log.record_workspace(&seq, receipt).unwrap();
                session.advance(log.events().last().unwrap()).unwrap();
                for actor in [ActorId::new(1), ActorId::new(2)] {
                    for buffer in [BufferId::new(0), BufferId::new(1)] {
                        assert_eq!(
                            session
                                .view(actor, buffer)
                                .unwrap()
                                .unwrap()
                                .visible()
                                .end()
                                .as_usize(),
                            4 + first.len() + last.len()
                        );
                        assert_eq!(
                            session
                                .selection(actor, buffer)
                                .unwrap()
                                .unwrap()
                                .head()
                                .as_usize(),
                            2 + first.len()
                        );
                        assert_eq!(
                            session
                                .bookmark(actor, buffer, &mark(buffer))
                                .unwrap()
                                .unwrap()
                                .anchor
                                .offset
                                .as_usize(),
                            2 + first.len()
                        );
                        assert_eq!(
                            session
                                .overlay(actor, key(buffer))
                                .unwrap()
                                .unwrap()
                                .span
                                .start()
                                .as_usize(),
                            2 + first.len()
                        );
                    }
                }
            },
        );
    }

    #[test]
    fn collapsed_overlays_follow_policy() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| rng.next_u64(),
            |&choice| {
                let mut seq = Sequencer::new(catalog(), actors());
                let mut log = DeliveryLog::new(seq.snapshot());
                let mut session = WorkspaceSession::new(seq.snapshot(), actors(), policy());
                let buffer = BufferId::new(choice % 2);
                let actor = ActorId::new(1);
                let text = session.read(actor, buffer).unwrap().clone();
                for (id, collapse) in [(1, CollapsePolicy::Evict), (2, CollapsePolicy::RetainEmpty)]
                {
                    session
                        .set_overlay(
                            actor,
                            Annotation {
                                key: AnnotationKey {
                                    buffer,
                                    namespace: NamespaceId::new(9),
                                    id: AnnotationId::new(id),
                                },
                                owner: actor,
                                span: ByteSpan::validate_in(&text, 1, 2).unwrap(),
                                start_affinity: Affinity::Left,
                                end_affinity: Affinity::Right,
                                priority: 0,
                                collapse,
                                label: "span".into(),
                            },
                        )
                        .unwrap();
                }
                let patch = Patch::new(vec![Splice::delete(
                    ByteSpan::validate_in(&text, 1, 2).unwrap(),
                )])
                .unwrap();
                let txn = Transaction::new(
                    EditIntent::for_actor(actor, Nonce::new(0), seq.revision()),
                    vec![(buffer, patch)],
                )
                .unwrap();
                let receipt = seq.submit(txn).committed_receipt().unwrap();
                log.record_workspace(&seq, receipt).unwrap();
                session.advance(log.events().last().unwrap()).unwrap();
                let key = |id| AnnotationKey {
                    buffer,
                    namespace: NamespaceId::new(9),
                    id: AnnotationId::new(id),
                };
                assert_eq!(session.overlay(actor, key(1)).unwrap(), None);
                assert!(session
                    .overlay(actor, key(2))
                    .unwrap()
                    .unwrap()
                    .span
                    .is_empty());
            },
        );
    }

    #[test]
    fn hidden_content_changes_do_not_change_visible_state() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| rng.next_u64(),
            |&choice| {
                let actor = ActorId::new(1);
                let hidden = BufferId::new(choice % 2);
                let visible = BufferId::new(1 - choice % 2);
                let mut observations = Vec::new();
                for text in ["secret", "different hidden contents"] {
                    let mut seq = Sequencer::new(catalog(), actors());
                    let mut log = DeliveryLog::new(seq.snapshot());
                    let mut session = WorkspaceSession::new(seq.snapshot(), actors(), policy());
                    populate(&mut session, actor, visible, 2);
                    populate(&mut session, actor, hidden, 1);
                    let mut restricted = AccessPolicy::new();
                    restricted.grant_buffer(actor, visible, Capability::Read);
                    restricted.grant_actor(ActorId::new(2), Capability::Read);
                    session.set_policy(restricted);
                    let event = commit(&mut seq, &mut log, 0, hidden, text);
                    session.advance(&event).unwrap();
                    assert_denied(&session, actor, hidden);
                    observations.push((
                        session.revision(),
                        session.read(actor, visible).unwrap().clone(),
                        session.view(actor, visible).unwrap(),
                        session.selection(actor, visible).unwrap(),
                        session.bookmark(actor, visible, &mark(visible)).unwrap(),
                        session.overlay(actor, key(visible)).unwrap().cloned(),
                    ));
                }
                assert_eq!(observations[0], observations[1]);
            },
        );
    }

    #[test]
    fn identity_replacement_in_mixed_edit_projects_positions() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| rng.next_u64(),
            |&choice| {
                let mut seq = Sequencer::new(catalog(), actors());
                let mut log = DeliveryLog::new(seq.snapshot());
                let mut session = WorkspaceSession::new(seq.snapshot(), actors(), policy());
                let actor = ActorId::new(1);
                let visible = BufferId::new(choice % 2);
                let hidden = BufferId::new(1 - choice % 2);
                populate(&mut session, actor, visible, 2);
                let before = seq.snapshot();
                let text = before.catalog().get(visible).unwrap();
                let narrowed = BufferView::narrowed(
                    before.catalog(),
                    visible,
                    ByteSpan::validate_in(text, 1, 3).unwrap(),
                )
                .unwrap();
                session.set_view(actor, narrowed).unwrap();
                let mut restricted = AccessPolicy::new();
                restricted.grant_buffer(actor, visible, Capability::Read);
                session.set_policy(restricted);
                let same =
                    Patch::new(vec![Splice::new(ByteSpan::full_of(text), text.clone())]).unwrap();
                let other = before.catalog().get(hidden).unwrap();
                let changed = Patch::new(vec![Splice::insert(
                    other.validate_offset(0).unwrap(),
                    Text::from_str("hidden"),
                )])
                .unwrap();
                let txn = Transaction::new(
                    EditIntent::for_actor(actor, Nonce::new(0), before.revision()),
                    vec![(visible, same), (hidden, changed)],
                )
                .unwrap();
                let receipt = seq.submit(txn).committed_receipt().unwrap();
                log.record_workspace(&seq, receipt).unwrap();
                session.advance(log.events().last().unwrap()).unwrap();
                assert_eq!(
                    session.view(actor, visible).unwrap().unwrap().visible(),
                    ByteSpan::full_of(text)
                );
                assert_eq!(
                    session
                        .selection(actor, visible)
                        .unwrap()
                        .unwrap()
                        .head()
                        .as_usize(),
                    4
                );
                let after_bookmark = session
                    .bookmark(actor, visible, &mark(visible))
                    .unwrap()
                    .unwrap();
                assert_eq!(after_bookmark.anchor.offset.as_usize(), 4);
                assert_eq!(after_bookmark.revision, session.revision());
                assert_eq!(
                    session
                        .overlay(actor, key(visible))
                        .unwrap()
                        .unwrap()
                        .span
                        .start()
                        .as_usize(),
                    4
                );
                assert_denied(&session, actor, hidden);
            },
        );
    }

    #[test]
    fn equal_final_text_keeps_distinct_edit_geometry() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| rng.next_u64(),
            |&choice| {
                let buffer = BufferId::new(choice % 2);
                let other = BufferId::new(1 - choice % 2);
                let mut catalog = catalog();
                catalog.update(buffer, Text::from_str("aaaa")).unwrap();
                let mut seq = Sequencer::new(catalog, actors());
                let mut log = DeliveryLog::new(seq.snapshot());
                let mut session = WorkspaceSession::new(seq.snapshot(), actors(), policy());
                for actor in [ActorId::new(1), ActorId::new(2)] {
                    populate(&mut session, actor, buffer, 2);
                    let view = BufferView::narrowed(
                        session.snapshot.catalog(),
                        buffer,
                        ByteSpan::validate_in(session.read(actor, buffer).unwrap(), 1, 3).unwrap(),
                    )
                    .unwrap();
                    session.set_view(actor, view).unwrap();
                }
                let before = seq.snapshot();
                let text = before.catalog().get(buffer).unwrap();
                let patch = Patch::new(vec![
                    Splice::delete(ByteSpan::validate_in(text, 0, 1).unwrap()),
                    Splice::insert(text.validate_offset(4).unwrap(), Text::from_str("a")),
                ])
                .unwrap();
                let text = before.catalog().get(other).unwrap();
                let other_patch = Patch::new(vec![Splice::insert(
                    text.validate_offset(0).unwrap(),
                    Text::from_str("x"),
                )])
                .unwrap();
                let txn = Transaction::new(
                    EditIntent::for_actor(ActorId::new(1), Nonce::new(0), before.revision()),
                    vec![(buffer, patch), (other, other_patch)],
                )
                .unwrap();
                let receipt = seq.submit(txn).committed_receipt().unwrap();
                log.record_workspace(&seq, receipt).unwrap();
                session.advance(log.events().last().unwrap()).unwrap();
                for actor in [ActorId::new(1), ActorId::new(2)] {
                    let span = session.view(actor, buffer).unwrap().unwrap().visible();
                    assert_eq!((span.start().as_usize(), span.end().as_usize()), (0, 2));
                    assert_eq!(
                        session
                            .selection(actor, buffer)
                            .unwrap()
                            .unwrap()
                            .head()
                            .as_usize(),
                        1
                    );
                    assert_eq!(
                        session
                            .bookmark(actor, buffer, &mark(buffer))
                            .unwrap()
                            .unwrap()
                            .anchor
                            .offset
                            .as_usize(),
                        1
                    );
                    assert_eq!(
                        session
                            .overlay(actor, key(buffer))
                            .unwrap()
                            .unwrap()
                            .span
                            .start()
                            .as_usize(),
                        1
                    );
                }
            },
        );
    }

    #[test]
    fn empty_splice_does_not_evict_overlay() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| rng.next_u64(),
            |&choice| {
                let buffer = BufferId::new(choice % 2);
                let actor = ActorId::new(1);
                let mut results = Vec::new();
                for extra in [false, true] {
                    let mut seq = Sequencer::new(catalog(), actors());
                    let mut log = DeliveryLog::new(seq.snapshot());
                    let mut session = WorkspaceSession::new(seq.snapshot(), actors(), policy());
                    let text = session.read(actor, buffer).unwrap().clone();
                    let point = text.validate_offset(1).unwrap();
                    let annotation = Annotation {
                        key: key(buffer),
                        owner: actor,
                        span: ByteSpan::empty_at(point),
                        start_affinity: Affinity::Left,
                        end_affinity: Affinity::Right,
                        priority: 0,
                        collapse: CollapsePolicy::Evict,
                        label: "mark".into(),
                    };
                    session.set_overlay(actor, annotation).unwrap();
                    let mut splices = vec![Splice::insert(point, Text::from_str("x"))];
                    if extra {
                        splices.push(Splice::insert(
                            text.validate_offset(4).unwrap(),
                            Text::empty(),
                        ));
                    }
                    let patch = Patch::new(splices).unwrap();
                    let txn = Transaction::new(
                        EditIntent::for_actor(actor, Nonce::new(0), seq.revision()),
                        vec![(buffer, patch)],
                    )
                    .unwrap();
                    let receipt = seq.submit(txn).committed_receipt().unwrap();
                    log.record_workspace(&seq, receipt).unwrap();
                    session.advance(log.events().last().unwrap()).unwrap();
                    let span = session.overlay(actor, key(buffer)).unwrap().unwrap().span;
                    results.push((span.start().as_usize(), span.end().as_usize()));
                }
                assert_eq!(results, [(1, 2), (1, 2)]);
            },
        );
    }

    #[test]
    fn lifecycle_updates_then_closes() {
        use crate::editor::workspace::buffer_lifecycle::{BufferLifecycle, ClosePolicy};
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| rng.next_u64(),
            |&choice| {
                let mut source = BufferLifecycle::new();
                let mut session = WorkspaceSession::new(source.snapshot(), actors(), policy());
                let buffer = BufferId::new(choice);
                source.open(buffer, Text::from_str("initial")).unwrap();
                session.open(source.snapshot(), buffer).unwrap();
                populate(&mut session, ActorId::new(1), buffer, 2);
                source
                    .update(buffer, Text::from_str("new initial"))
                    .unwrap();
                session.update(source.snapshot(), buffer).unwrap();
                assert_eq!(
                    session.read(ActorId::new(1), buffer).unwrap().as_str(),
                    "new initial"
                );
                assert_eq!(
                    session
                        .selection(ActorId::new(1), buffer)
                        .unwrap()
                        .unwrap()
                        .head()
                        .as_usize(),
                    6
                );
                source.close(buffer, ClosePolicy::Discard).unwrap();
                session.close(source.snapshot(), buffer).unwrap();
                assert_denied(&session, ActorId::new(1), buffer);
                assert_eq!(session.revision(), source.snapshot().revision());
            },
        );
    }

    #[test]
    fn continuous_bookmark_and_overlay_projection() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| rng.next_u64(),
            |&choice| {
                let mut seq = Sequencer::new(catalog(), actors());
                let mut log = DeliveryLog::new(seq.snapshot());
                let mut session = WorkspaceSession::new(seq.snapshot(), actors(), policy());
                let buffer = BufferId::new(choice % 2);
                let mut references = Vec::new();
                for actor in [ActorId::new(1), ActorId::new(2)] {
                    populate(&mut session, actor, buffer, 2);
                    let text = session.read(actor, buffer).unwrap();
                    let affinity = if actor.raw() == 1 {
                        Affinity::Left
                    } else {
                        Affinity::Right
                    };
                    session
                        .set_bookmark(
                            actor,
                            mark(buffer),
                            Bookmark {
                                buffer,
                                anchor: Anchor {
                                    offset: text.validate_offset(2).unwrap(),
                                    affinity,
                                },
                                revision: session.revision(),
                            },
                        )
                        .unwrap();
                    let state = session.actors.get(&actor).unwrap();
                    references.push((state.bookmarks.clone(), state.overlays.clone()));
                }
                for n in 0..4 {
                    let before = seq.snapshot();
                    let source = before.catalog().get(buffer).unwrap().clone();
                    let replacement = if (choice >> n) & 1 == 0 { "x" } else { "yz" };
                    let event = commit(&mut seq, &mut log, n, buffer, replacement);
                    let after = seq.snapshot();
                    let map = PositionMap::from_replacement(
                        source.len_bytes(),
                        ByteReplacement {
                            start: 0,
                            end: 0,
                            inserted_len: replacement.len(),
                        },
                    )
                    .unwrap();
                    let revisioned = RevisionedMap {
                        map: &map,
                        source: before.revision(),
                        target: after.revision(),
                    };
                    for (bookmarks, overlays) in &mut references {
                        bookmarks
                            .advance(&before, &after, buffer, &revisioned)
                            .unwrap();
                        overlays
                            .transport(
                                buffer,
                                ReplacementEffect {
                                    span: ByteSpan::empty_at(source.validate_offset(0).unwrap()),
                                    inserted_len: replacement.len(),
                                },
                                &source,
                                after.catalog().get(buffer).unwrap(),
                            )
                            .unwrap();
                    }
                    session.advance(&event).unwrap();
                    for (index, actor) in [ActorId::new(1), ActorId::new(2)].into_iter().enumerate()
                    {
                        let state = session.actors.get(&actor).unwrap();
                        assert_eq!(state.bookmarks, references[index].0);
                        assert_eq!(state.overlays, references[index].1);
                    }
                }
            },
        );
    }

    #[test]
    fn committed_identity_replacement_projects_published_spans() {
        let mut seq = Sequencer::new(catalog(), actors());
        let mut log = DeliveryLog::new(seq.snapshot());
        let mut session = WorkspaceSession::new(seq.snapshot(), actors(), policy());
        let actor = ActorId::new(1);
        let buffer = BufferId::new(0);
        populate(&mut session, actor, buffer, 2);
        let before = seq.snapshot();
        let source = before.catalog().get(buffer).unwrap();
        let patch = Patch::new(vec![
            Splice::new(
                ByteSpan::validate_in(source, 1, 3).unwrap(),
                Text::from_str("bc"),
            ),
            Splice::insert(source.validate_offset(4).unwrap(), Text::from_str("!")),
        ])
        .unwrap();
        let mut anchor = Anchor {
            offset: source.validate_offset(2).unwrap(),
            affinity: Affinity::Right,
        };
        let mut overlays = session.actors[&actor].overlays.clone();
        let mut text = source.clone();
        for splice in patch.splices().iter().rev() {
            let target = splice.apply(&text).unwrap();
            let effect = ReplacementEffect {
                span: splice.span(),
                inserted_len: splice.replacement().len_bytes(),
            };
            anchor = effect.transport(anchor, &text, &target).unwrap();
            overlays.transport(buffer, effect, &text, &target).unwrap();
            text = target;
        }
        let txn = Transaction::new(
            EditIntent::for_actor(actor, Nonce::new(0), before.revision()),
            vec![(buffer, patch)],
        )
        .unwrap();
        let receipt = seq.submit(txn).committed_receipt().unwrap();
        log.record_workspace(&seq, receipt).unwrap();
        session.advance(log.events().last().unwrap()).unwrap();
        let selection = session.selection(actor, buffer).unwrap().unwrap();
        assert_eq!(anchor.offset.as_usize(), 3);
        assert_eq!(selection.anchor(), anchor.offset);
        assert_eq!(selection.head(), anchor.offset);
        assert_eq!(
            session
                .bookmark(actor, buffer, &mark(buffer))
                .unwrap()
                .unwrap()
                .anchor,
            anchor
        );
        assert_eq!(session.actors[&actor].overlays, overlays);
        assert_eq!(
            session
                .overlay(actor, key(buffer))
                .unwrap()
                .unwrap()
                .span
                .start(),
            anchor.offset
        );
    }

    #[test]
    fn batch_updates_project_all_buffers_and_reject_invalid_transitions_atomically() {
        let mut source = VersionedWorkspace::new(catalog());
        let mut session = WorkspaceSession::new(source.capture(), actors(), policy());
        let actor = ActorId::new(1);
        let a = BufferId::new(0);
        let b = BufferId::new(1);
        for buffer in [a, b] {
            populate(&mut session, actor, buffer, 2);
        }
        source
            .update_batch([(a, Text::from_str("Xabcd")), (b, Text::from_str("YYabcd"))])
            .unwrap();
        let after = source.capture();
        let original = session.clone();
        for buffers in [vec![a], vec![a, a, b], vec![a, b, BufferId::new(99)]] {
            assert_eq!(
                session.update_batch(after.clone(), buffers),
                Err(SessionError::InvalidTransition)
            );
            assert_eq!(session, original);
        }
        let mut foreign = VersionedWorkspace::new(catalog());
        foreign
            .update_batch([(a, Text::from_str("Xabcd")), (b, Text::from_str("YYabcd"))])
            .unwrap();
        assert_eq!(
            session.update_batch(foreign.capture(), [a, b]),
            Err(SessionError::InvalidTransition)
        );
        assert_eq!(session, original);
        source.update_batch([]).unwrap();
        assert_eq!(
            session.update_batch(source.capture(), [a, b]),
            Err(SessionError::InvalidTransition)
        );
        assert_eq!(session, original);
        session.update_batch(after, [a, b]).unwrap();
        for (buffer, offset, len) in [(a, 3, 5), (b, 4, 6)] {
            let selection = session.selection(actor, buffer).unwrap().unwrap();
            assert_eq!(selection.anchor().as_usize(), offset);
            assert_eq!(selection.head().as_usize(), offset);
            assert_eq!(
                session
                    .view(actor, buffer)
                    .unwrap()
                    .unwrap()
                    .visible()
                    .end()
                    .as_usize(),
                len
            );
            assert_eq!(
                session
                    .bookmark(actor, buffer, &mark(buffer))
                    .unwrap()
                    .unwrap()
                    .anchor
                    .offset
                    .as_usize(),
                offset
            );
            assert_eq!(
                session
                    .overlay(actor, key(buffer))
                    .unwrap()
                    .unwrap()
                    .span
                    .start()
                    .as_usize(),
                offset
            );
        }
        session.update_batch(source.capture(), []).unwrap();
        assert_eq!(session.revision(), source.capture().revision());

        let mut closed_source = VersionedWorkspace::new(catalog());
        closed_source.remove(b).unwrap();
        let mut closed_session = WorkspaceSession::new(closed_source.capture(), actors(), policy());
        closed_source.update_batch([]).unwrap();
        let original = closed_session.clone();
        assert_eq!(
            closed_session.update_batch(closed_source.capture(), [b]),
            Err(SessionError::InvalidTransition)
        );
        assert_eq!(closed_session, original);
    }

    #[test]
    fn deliveries_track_all_private_positions() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| (rng.next_u64() % 5, rng.next_u64()),
            |&(start, choice)| {
                let mut seq = Sequencer::new(catalog(), actors());
                let mut log = DeliveryLog::new(seq.snapshot());
                let mut session = WorkspaceSession::new(seq.snapshot(), actors(), policy());
                let buffer = BufferId::new(0);
                for actor in [ActorId::new(1), ActorId::new(2)] {
                    populate(&mut session, actor, buffer, start as usize);
                }
                let key = AnnotationKey {
                    buffer,
                    namespace: NamespaceId::new(1),
                    id: AnnotationId::new(1),
                };
                let name = mark(buffer);
                let mut expected = start as usize;
                for n in 0..8 {
                    let insert = if (choice >> n) & 1 == 0 { "x" } else { "yz" };
                    let event = commit(&mut seq, &mut log, n, buffer, insert);
                    session.advance(&event).unwrap();
                    expected += insert.len();
                    assert_eq!(session.revision(), seq.revision());
                    for actor in [ActorId::new(1), ActorId::new(2)] {
                        assert_eq!(
                            session
                                .selection(actor, buffer)
                                .unwrap()
                                .unwrap()
                                .head()
                                .as_usize(),
                            expected
                        );
                        assert_eq!(
                            session
                                .bookmark(actor, buffer, &name)
                                .unwrap()
                                .unwrap()
                                .anchor
                                .offset
                                .as_usize(),
                            expected
                        );
                        assert_eq!(
                            session
                                .bookmark(actor, buffer, &name)
                                .unwrap()
                                .unwrap()
                                .revision,
                            seq.revision()
                        );
                        assert_eq!(
                            session
                                .overlay(actor, key)
                                .unwrap()
                                .unwrap()
                                .span
                                .start()
                                .as_usize(),
                            expected
                        );
                        assert_eq!(
                            session
                                .view(actor, buffer)
                                .unwrap()
                                .unwrap()
                                .visible()
                                .end()
                                .as_usize(),
                            seq.snapshot().catalog().get(buffer).unwrap().len_bytes()
                        );
                    }
                    let before = session.clone();
                    assert_eq!(
                        session.advance(&event),
                        Err(SessionError::InvalidTransition)
                    );
                    assert_eq!(session, before);
                }
                let mut foreign = Sequencer::new(catalog(), actors());
                let mut foreign_log = DeliveryLog::new(foreign.snapshot());
                let foreign_event = commit(&mut foreign, &mut foreign_log, 0, buffer, "x");
                let before = session.clone();
                assert_eq!(
                    session.advance(&foreign_event),
                    Err(SessionError::InvalidTransition)
                );
                assert_eq!(session, before);
            },
        );
    }

    #[test]
    fn closure_and_revocation_isolate_observation() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| rng.next_u64(),
            |&choice| {
                let mut workspace = VersionedWorkspace::new(catalog());
                let mut session = WorkspaceSession::new(workspace.capture(), actors(), policy());
                let a = ActorId::new(1);
                let b = ActorId::new(2);
                let hidden = BufferId::new(choice % 2);
                let other = BufferId::new(1 - choice % 2);
                for actor in [a, b] {
                    populate(&mut session, actor, hidden, 1);
                }
                populate(&mut session, a, other, 2);
                let b_view = session.view(b, hidden).unwrap();
                let other_view = session.view(a, other).unwrap();
                let mut restricted = AccessPolicy::new();
                restricted.grant_buffer(a, other, Capability::Read);
                restricted.grant_actor(b, Capability::Read);
                session.set_policy(restricted);
                assert_denied(&session, a, hidden);
                assert_eq!(session.view(b, hidden).unwrap(), b_view);
                assert_eq!(session.view(a, other).unwrap(), other_view);
                workspace.remove(hidden).unwrap();
                session.close(workspace.capture(), hidden).unwrap();
                assert_denied(&session, b, hidden);
                assert_eq!(session.view(a, other).unwrap(), other_view);
            },
        );
    }

    #[test]
    fn unreadable_edits_and_lifecycle_follow_verified_transitions() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| rng.next_u64(),
            |&choice| {
                let mut authority = CheckpointState::from_catalog(catalog(), actors());
                let mut session = WorkspaceSession::new(authority.snapshot(), actors(), policy());
                let a = ActorId::new(1);
                let b = ActorId::new(2);
                let hidden = BufferId::new(choice % 2);
                let other = BufferId::new(1 - choice % 2);
                for actor in [a, b] {
                    populate(&mut session, actor, hidden, 1);
                }
                populate(&mut session, a, other, 2);
                let other_selection = session.selection(a, other).unwrap();
                let other_mark = session
                    .bookmark(a, other, &mark(other))
                    .unwrap()
                    .unwrap()
                    .anchor;
                let mut restricted = AccessPolicy::new();
                restricted.grant_buffer(a, other, Capability::Read);
                restricted.grant_actor(b, Capability::Read);
                session.set_policy(restricted);
                assert_denied(&session, a, hidden);
                let text = authority.snapshot().catalog().get(hidden).unwrap().clone();
                let patch = Patch::new(vec![Splice::insert(
                    text.validate_offset(0).unwrap(),
                    Text::from_str("secret-change"),
                )])
                .unwrap();
                let txn = Transaction::new(
                    EditIntent::for_actor(a, Nonce::new(10), authority.revision()),
                    vec![(hidden, patch)],
                )
                .unwrap();
                authority
                    .apply(CheckpointOp::Edit {
                        request: RequestId::new(10),
                        txn,
                    })
                    .unwrap();
                session
                    .advance(authority.delivery().events().last().unwrap())
                    .unwrap();
                assert_denied(&session, a, hidden);
                assert!(!format!("{session:?}").contains("secret-change"));
                assert_eq!(session.selection(a, other).unwrap(), other_selection);
                assert_eq!(
                    session
                        .bookmark(a, other, &mark(other))
                        .unwrap()
                        .unwrap()
                        .anchor,
                    other_mark
                );
                assert_eq!(
                    session
                        .selection(b, hidden)
                        .unwrap()
                        .unwrap()
                        .head()
                        .as_usize(),
                    14
                );
                authority
                    .apply(CheckpointOp::Close {
                        request: RequestId::new(11),
                        intent: EditIntent::for_actor(a, Nonce::new(11), authority.revision()),
                        buffer: hidden,
                        policy: crate::editor::workspace::buffer_lifecycle::ClosePolicy::Discard,
                    })
                    .unwrap();
                session.close(authority.snapshot(), hidden).unwrap();
                assert_denied(&session, b, hidden);
                assert_eq!(session.selection(a, other).unwrap(), other_selection);
                let opened = authority
                    .apply(CheckpointOp::Open {
                        request: RequestId::new(12),
                        intent: EditIntent::for_actor(a, Nonce::new(12), authority.revision()),
                        name: "new".into(),
                        text: Text::from_str("hello"),
                    })
                    .unwrap()
                    .buffer
                    .unwrap();
                session.open(authority.snapshot(), opened).unwrap();
                let snapshot = authority.snapshot();
                let patch = Patch::new(vec![Splice::insert(
                    snapshot
                        .catalog()
                        .get(opened)
                        .unwrap()
                        .validate_offset(0)
                        .unwrap(),
                    Text::from_str("x"),
                )])
                .unwrap();
                let txn = Transaction::new(
                    EditIntent::for_actor(a, Nonce::new(13), authority.revision()),
                    vec![(opened, patch)],
                )
                .unwrap();
                authority
                    .apply(CheckpointOp::Edit {
                        request: RequestId::new(13),
                        txn,
                    })
                    .unwrap();
                session
                    .advance(authority.delivery().events().last().unwrap())
                    .unwrap();
                assert_eq!(session.read(b, opened).unwrap().as_str(), "xhello");
                assert_eq!(session.revision(), authority.revision());
            },
        );
    }

    #[test]
    fn mixed_edits_match_continuous_projection_and_reject_wrong_history() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| rng.next_u64(),
            |&choice| {
                let mut seq = Sequencer::new(catalog(), actors());
                let mut log = DeliveryLog::new(seq.snapshot());
                let mut session = WorkspaceSession::new(seq.snapshot(), actors(), policy());
                let actor = ActorId::new(1);
                for buffer in [BufferId::new(0), BufferId::new(1)] {
                    populate(&mut session, actor, buffer, 2);
                    let text = session.read(actor, buffer).unwrap();
                    let view = BufferView::narrowed(
                        session.snapshot.catalog(),
                        buffer,
                        ByteSpan::validate_in(text, 1, 3).unwrap(),
                    )
                    .unwrap();
                    let selection = Selection::new(
                        session.snapshot.catalog(),
                        &view,
                        buffer,
                        text.validate_offset(1).unwrap(),
                        text.validate_offset(3).unwrap(),
                    )
                    .unwrap();
                    session.set_view(actor, view).unwrap();
                    session.set_selection(actor, selection).unwrap();
                }
                let mut foreign = Sequencer::new(catalog(), actors());
                let mut foreign_log = DeliveryLog::new(foreign.snapshot());
                let foreign_event =
                    commit(&mut foreign, &mut foreign_log, 0, BufferId::new(0), "x");
                let original = session.clone();
                assert_eq!(
                    session.advance(&foreign_event),
                    Err(SessionError::InvalidTransition)
                );
                assert_eq!(session, original);
                let mut views: Vec<_> = [0, 1]
                    .into_iter()
                    .map(|n| session.view(actor, BufferId::new(n)).unwrap().unwrap())
                    .collect();
                let mut selections: Vec<_> = [0, 1]
                    .into_iter()
                    .map(|n| session.selection(actor, BufferId::new(n)).unwrap().unwrap())
                    .collect();
                for n in 0..2 {
                    let before = seq.snapshot();
                    let mut patches = Vec::new();
                    let mut maps = Vec::new();
                    for buffer in [BufferId::new(0), BufferId::new(1)] {
                        let text = before.catalog().get(buffer).unwrap();
                        let replacement = if (choice >> (n + buffer.raw())) & 1 == 0 {
                            ""
                        } else {
                            "xyz"
                        };
                        let span = ByteSpan::validate_in(text, 0, 1).unwrap();
                        patches.push((
                            buffer,
                            Patch::new(vec![Splice::new(span, Text::from_str(replacement))])
                                .unwrap(),
                        ));
                        maps.push(
                            PositionMap::from_replacement(
                                text.len_bytes(),
                                ByteReplacement {
                                    start: 0,
                                    end: 1,
                                    inserted_len: replacement.len(),
                                },
                            )
                            .unwrap(),
                        );
                    }
                    let txn = Transaction::new(
                        EditIntent::for_actor(actor, Nonce::new(n), seq.revision()),
                        patches,
                    )
                    .unwrap();
                    let receipt = seq.submit(txn).committed_receipt().unwrap();
                    log.record_workspace(&seq, receipt).unwrap();
                    let event = log.events().last().unwrap();
                    let after = seq.snapshot();
                    for (index, buffer) in
                        [BufferId::new(0), BufferId::new(1)].into_iter().enumerate()
                    {
                        let len = before.catalog().get(buffer).unwrap().len_bytes();
                        views[index] = map_view(
                            after.catalog(),
                            &views[index],
                            &maps[index],
                            len,
                            Affinity::Left,
                            Affinity::Right,
                        )
                        .unwrap();
                        let mapped = map_selection(
                            after.catalog(),
                            selections[index],
                            &maps[index],
                            len,
                            Affinity::Right,
                            Affinity::Right,
                        )
                        .unwrap();
                        selections[index] =
                            clip_selection(after.catalog(), mapped, &views[index]).unwrap();
                    }
                    session.advance(event).unwrap();
                    for (index, buffer) in
                        [BufferId::new(0), BufferId::new(1)].into_iter().enumerate()
                    {
                        assert_eq!(session.view(actor, buffer).unwrap(), Some(views[index]));
                        assert_eq!(
                            session.selection(actor, buffer).unwrap(),
                            Some(selections[index])
                        );
                    }
                    if n == 1 {
                        let mut behind = original.clone();
                        assert_eq!(behind.advance(event), Err(SessionError::InvalidTransition));
                        assert_eq!(behind, original);
                    }
                }
            },
        );
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct PrivateState {
    views: BTreeMap<BufferId, BufferView>,
    selections: BTreeMap<BufferId, Selection>,
    bookmarks: BookmarkStore,
    overlays: AnnotationStore,
    registers: RegisterStore,
    ring: KillRing,
}

/// A shared capture and isolated actor-owned state.
#[derive(Clone, PartialEq, Eq)]
pub struct WorkspaceSession {
    snapshot: WorkspaceSnapshot,
    registry: ActorRegistry,
    policy: AccessPolicy,
    actors: BTreeMap<ActorId, PrivateState>,
}

impl std::fmt::Debug for WorkspaceSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceSession")
            .field("revision", &self.revision())
            .finish_non_exhaustive()
    }
}

impl WorkspaceSession {
    pub fn new(snapshot: WorkspaceSnapshot, registry: ActorRegistry, policy: AccessPolicy) -> Self {
        let actors = registry
            .active_actors()
            .into_iter()
            .map(|(id, _)| (id, PrivateState::default()))
            .collect();
        Self {
            snapshot,
            registry,
            policy,
            actors,
        }
    }

    pub fn revision(&self) -> Revision {
        self.snapshot.revision()
    }

    fn check(&self, actor: ActorId, buffer: BufferId) -> Result<(), SessionError> {
        self.policy
            .authorize(
                &self.registry,
                self.snapshot.catalog(),
                actor,
                Capability::Read,
                [buffer],
            )
            .map_err(SessionError::Access)
    }

    pub fn read(&self, actor: ActorId, buffer: BufferId) -> Result<&Text, SessionError> {
        self.policy
            .read(&self.registry, self.snapshot.catalog(), actor, buffer)
            .map_err(SessionError::Access)
    }

    pub fn view(
        &self,
        actor: ActorId,
        buffer: BufferId,
    ) -> Result<Option<BufferView>, SessionError> {
        self.check(actor, buffer)?;
        Ok(self
            .actors
            .get(&actor)
            .and_then(|state| state.views.get(&buffer).copied()))
    }

    pub fn set_view(&mut self, actor: ActorId, view: BufferView) -> Result<(), SessionError> {
        let buffer = view.buffer();
        self.check(actor, buffer)?;
        BufferView::narrowed(self.snapshot.catalog(), buffer, view.visible())
            .map_err(|_| SessionError::InvalidState)?;
        let state = self
            .actors
            .get_mut(&actor)
            .ok_or(SessionError::InvalidState)?;
        if let Some(selection) = state.selections.get(&buffer) {
            selection
                .validate(self.snapshot.catalog(), &view)
                .map_err(|_| SessionError::InvalidState)?;
        }
        state.views.insert(buffer, view);
        Ok(())
    }

    pub fn selection(
        &self,
        actor: ActorId,
        buffer: BufferId,
    ) -> Result<Option<Selection>, SessionError> {
        self.check(actor, buffer)?;
        Ok(self
            .actors
            .get(&actor)
            .and_then(|state| state.selections.get(&buffer).copied()))
    }

    pub fn set_selection(
        &mut self,
        actor: ActorId,
        selection: Selection,
    ) -> Result<(), SessionError> {
        let buffer = selection.buffer();
        self.check(actor, buffer)?;
        let state = self
            .actors
            .get_mut(&actor)
            .ok_or(SessionError::InvalidState)?;
        let view = state.views.get(&buffer).ok_or(SessionError::InvalidState)?;
        selection
            .validate(self.snapshot.catalog(), view)
            .map_err(|_| SessionError::InvalidState)?;
        state.selections.insert(buffer, selection);
        Ok(())
    }

    pub fn set_bookmark(
        &mut self,
        actor: ActorId,
        name: BookmarkName,
        bookmark: Bookmark,
    ) -> Result<(), SessionError> {
        self.check(actor, bookmark.buffer)?;
        self.actors
            .get_mut(&actor)
            .ok_or(SessionError::InvalidState)?
            .bookmarks
            .set(&self.snapshot, BookmarkKey { actor, name }, bookmark)
            .map_err(|_| SessionError::InvalidState)
    }

    pub fn bookmark(
        &self,
        actor: ActorId,
        buffer: BufferId,
        name: &BookmarkName,
    ) -> Result<Option<Bookmark>, SessionError> {
        self.check(actor, buffer)?;
        Ok(self
            .actors
            .get(&actor)
            .and_then(|state| {
                state.bookmarks.get(&BookmarkKey {
                    actor,
                    name: name.clone(),
                })
            })
            .filter(|b| b.buffer == buffer)
            .copied())
    }

    pub fn set_overlay(
        &mut self,
        actor: ActorId,
        annotation: Annotation,
    ) -> Result<(), SessionError> {
        self.check(actor, annotation.key.buffer)?;
        if annotation.owner != actor {
            return Err(SessionError::InvalidState);
        }
        self.actors
            .get_mut(&actor)
            .ok_or(SessionError::InvalidState)?
            .overlays
            .set(self.snapshot.catalog(), annotation)
            .map_err(|_| SessionError::InvalidState)
    }

    pub fn overlay(
        &self,
        actor: ActorId,
        key: AnnotationKey,
    ) -> Result<Option<&Annotation>, SessionError> {
        self.check(actor, key.buffer)?;
        Ok(self
            .actors
            .get(&actor)
            .and_then(|state| state.overlays.get(key)))
    }

    pub fn put_register(
        &mut self,
        actor: ActorId,
        name: RegisterName,
        entry: RegisterEntry,
    ) -> Result<(), SessionError> {
        let state = self
            .actors
            .get_mut(&actor)
            .ok_or(SessionError::InvalidState)?;
        state.registers.put(actor, name, entry);
        Ok(())
    }

    pub fn register(&self, actor: ActorId, name: &RegisterName) -> Option<&RegisterEntry> {
        self.actors
            .get(&actor)
            .and_then(|state| state.registers.get(actor, name))
    }

    pub fn push_kill(&mut self, actor: ActorId, entry: RegisterEntry) -> Result<(), SessionError> {
        self.actors
            .get_mut(&actor)
            .ok_or(SessionError::InvalidState)?
            .ring
            .push(entry);
        Ok(())
    }

    pub fn current_kill(&self, actor: ActorId) -> Option<&RegisterEntry> {
        self.actors
            .get(&actor)
            .and_then(|state| state.ring.current())
    }

    /// Advance an origin-bound, single-buffer open transition.
    pub fn open(&mut self, after: WorkspaceSnapshot, buffer: BufferId) -> Result<(), SessionError> {
        if self.revision().next().ok() != Some(after.revision()) {
            return Err(SessionError::InvalidTransition);
        }
        let mut store = SnapshotStore::new();
        store
            .record(self.snapshot.clone())
            .map_err(|_| SessionError::InvalidTransition)?;
        store
            .record(after.clone())
            .map_err(|_| SessionError::InvalidTransition)?;
        let mut expected = self.snapshot.catalog().clone();
        let text = after
            .catalog()
            .get(buffer)
            .ok_or(SessionError::InvalidTransition)?;
        expected
            .register(buffer, text.clone())
            .map_err(|_| SessionError::InvalidTransition)?;
        if &expected != after.catalog() {
            return Err(SessionError::InvalidTransition);
        }
        let mut actors = self.actors.clone();
        for state in actors.values_mut() {
            state
                .bookmarks
                .advance_batch(&self.snapshot, &after, &[])
                .map_err(|_| SessionError::InvalidTransition)?;
        }
        self.actors = actors;
        self.snapshot = after;
        Ok(())
    }

    /// Replace grants and discard newly inaccessible positional state.
    pub fn set_policy(&mut self, policy: AccessPolicy) {
        self.policy = policy;
        let catalog = self.snapshot.catalog();
        for (&actor, state) in &mut self.actors {
            let allowed = |buffer| {
                self.policy
                    .authorize(&self.registry, catalog, actor, Capability::Read, [buffer])
                    .is_ok()
            };
            state.views.retain(|&buffer, _| allowed(buffer));
            state.selections.retain(|&buffer, _| allowed(buffer));
            let keys: Vec<_> = state
                .bookmarks
                .iter()
                .filter(|(_, b)| !allowed(b.buffer))
                .map(|(k, _)| k.clone())
                .collect();
            for key in keys {
                state.bookmarks.remove(&key);
            }
            let keys: Vec<_> = state
                .overlays
                .iter()
                .filter(|a| !allowed(a.key.buffer))
                .map(|a| a.key)
                .collect();
            for key in keys {
                state.overlays.remove(key);
            }
        }
    }

    /// Advance only through a delivery bound to this exact workspace capture.
    pub fn advance(&mut self, event: &DeliveryEvent) -> Result<(), SessionError> {
        replay(&self.snapshot, std::slice::from_ref(event))
            .map_err(|_| SessionError::InvalidTransition)?;
        self.project(event.after(), event.workspace_event().patches())
    }

    /// Advance a single snapshot replacement using its minimal text difference.
    pub fn update(
        &mut self,
        after: WorkspaceSnapshot,
        buffer: BufferId,
    ) -> Result<(), SessionError> {
        self.update_batch(after, [buffer])
    }

    /// Advance a snapshot replacement affecting distinct live buffers.
    pub fn update_batch(
        &mut self,
        after: WorkspaceSnapshot,
        buffers: impl IntoIterator<Item = BufferId>,
    ) -> Result<(), SessionError> {
        if self.revision().next().ok() != Some(after.revision()) {
            return Err(SessionError::InvalidTransition);
        }
        let mut store = SnapshotStore::new();
        store
            .record(self.snapshot.clone())
            .map_err(|_| SessionError::InvalidTransition)?;
        store
            .record(after.clone())
            .map_err(|_| SessionError::InvalidTransition)?;
        let mut expected = self.snapshot.catalog().clone();
        let mut seen = BTreeSet::new();
        let mut patches = Vec::new();
        for buffer in buffers {
            if !seen.insert(buffer) {
                return Err(SessionError::InvalidTransition);
            }
            let old = self
                .snapshot
                .catalog()
                .get(buffer)
                .ok_or(SessionError::InvalidTransition)?;
            let new = after
                .catalog()
                .get(buffer)
                .ok_or(SessionError::InvalidTransition)?;
            expected
                .update(buffer, new.clone())
                .map_err(|_| SessionError::InvalidTransition)?;
            let splices = crate::editor::text::diff::diff(old, new)
                .map(|d| vec![Splice::new(d.span, d.replacement)])
                .unwrap_or_default();
            let patch = Patch::new(splices).map_err(|_| SessionError::InvalidTransition)?;
            patches.push((buffer, patch));
        }
        if &expected != after.catalog() {
            return Err(SessionError::InvalidTransition);
        }
        self.project(&after, &patches)
    }

    fn project(
        &mut self,
        after: &WorkspaceSnapshot,
        patches: &[(BufferId, Patch)],
    ) -> Result<(), SessionError> {
        let before = &self.snapshot;
        let mut maps = BTreeMap::new();
        for (buffer, patch) in patches {
            let text = before
                .catalog()
                .get(*buffer)
                .ok_or(SessionError::InvalidTransition)?;
            let mut map = PositionMap::identity(text.len_bytes());
            for splice in patch.splices().iter().rev() {
                let span = splice.span();
                if span.is_empty() && splice.replacement().is_empty() {
                    continue;
                }
                let next = PositionMap::from_replacement(
                    map.target_len(),
                    ByteReplacement {
                        start: span.start().as_usize(),
                        end: span.end().as_usize(),
                        inserted_len: splice.replacement().len_bytes(),
                    },
                )
                .map_err(|_| SessionError::InvalidTransition)?;
                map = map
                    .compose(&next)
                    .map_err(|_| SessionError::InvalidTransition)?;
            }
            maps.insert(*buffer, map);
        }
        let versioned: Vec<_> = maps
            .iter()
            .map(|(&id, map)| {
                (
                    id,
                    RevisionedMap {
                        map,
                        source: before.revision(),
                        target: after.revision(),
                    },
                )
            })
            .collect();
        let refs: Vec<_> = versioned.iter().map(|(id, map)| (*id, map)).collect();
        let mut actors = self.actors.clone();
        for state in actors.values_mut() {
            state
                .bookmarks
                .advance_batch(before, after, &refs)
                .map_err(|_| SessionError::InvalidTransition)?;
            for (&buffer, map) in &maps {
                let old = before
                    .catalog()
                    .get(buffer)
                    .ok_or(SessionError::InvalidTransition)?;
                let len = old.len_bytes();
                if let Some(view) = state.views.get_mut(&buffer) {
                    *view = map_view(
                        after.catalog(),
                        view,
                        map,
                        len,
                        Affinity::Left,
                        Affinity::Right,
                    )
                    .map_err(|_| SessionError::InvalidTransition)?;
                }
                if let Some(selection) = state.selections.get_mut(&buffer) {
                    let mapped = map_selection(
                        after.catalog(),
                        *selection,
                        map,
                        len,
                        Affinity::Right,
                        Affinity::Right,
                    )
                    .map_err(|_| SessionError::InvalidTransition)?;
                    *selection = clip_selection(
                        after.catalog(),
                        mapped,
                        state.views.get(&buffer).ok_or(SessionError::InvalidState)?,
                    )
                    .map_err(|_| SessionError::InvalidTransition)?;
                }
                let patch = patches
                    .iter()
                    .find(|(id, _)| *id == buffer)
                    .ok_or(SessionError::InvalidTransition)?;
                let mut text = old.clone();
                for splice in patch.1.splices().iter().rev() {
                    if splice.span().is_empty() && splice.replacement().is_empty() {
                        continue;
                    }
                    let target = splice
                        .apply(&text)
                        .map_err(|_| SessionError::InvalidTransition)?;
                    state
                        .overlays
                        .transport(
                            buffer,
                            ReplacementEffect {
                                span: splice.span(),
                                inserted_len: splice.replacement().len_bytes(),
                            },
                            &text,
                            &target,
                        )
                        .map_err(|_| SessionError::InvalidTransition)?;
                    text = target;
                }
            }
        }
        self.actors = actors;
        self.snapshot = after.clone();
        Ok(())
    }

    /// Advance a single close transition with an origin-bound capture.
    pub fn close(
        &mut self,
        after: WorkspaceSnapshot,
        buffer: BufferId,
    ) -> Result<(), SessionError> {
        if self.revision().next().ok() != Some(after.revision()) {
            return Err(SessionError::InvalidTransition);
        }
        let mut store = SnapshotStore::new();
        store
            .record(self.snapshot.clone())
            .map_err(|_| SessionError::InvalidTransition)?;
        store
            .record(after.clone())
            .map_err(|_| SessionError::InvalidTransition)?;
        let mut expected = self.snapshot.catalog().clone();
        expected
            .remove(buffer)
            .map_err(|_| SessionError::InvalidTransition)?;
        if &expected != after.catalog() {
            return Err(SessionError::InvalidTransition);
        }
        let mut actors = self.actors.clone();
        for state in actors.values_mut() {
            state.views.remove(&buffer);
            state.selections.remove(&buffer);
            let keys: Vec<_> = state
                .bookmarks
                .iter()
                .filter(|(_, b)| b.buffer == buffer)
                .map(|(k, _)| k.clone())
                .collect();
            for key in keys {
                state.bookmarks.remove(&key);
            }
            let keys: Vec<_> = state
                .overlays
                .iter()
                .filter(|a| a.key.buffer == buffer)
                .map(|a| a.key)
                .collect();
            for key in keys {
                state.overlays.remove(key);
            }
            state
                .bookmarks
                .advance_batch(&self.snapshot, &after, &[])
                .map_err(|_| SessionError::InvalidTransition)?;
        }
        self.actors = actors;
        self.snapshot = after;
        Ok(())
    }
}
