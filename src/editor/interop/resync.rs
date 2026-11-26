//! Revision-aware delivery and snapshot recovery for external clients.

use std::collections::BTreeMap;

use super::{
    error::BoundaryError,
    snapshot,
    subscriptions::{InitialSnapshots, Notification, Subscription},
};
use crate::editor::{
    collab::delivery::{CatchUp, DeliveryEvent},
    edit::ids::BufferId,
    workspace::{
        buffer_lifecycle::{BufferLifecycle, LifecycleEvent, LifecycleKind},
        events::WorkspaceEvent,
        snapshots::{SnapshotError, SnapshotStore, WorkspaceSnapshot},
        version::Revision,
    },
};

/// A rejected transition; rejection leaves the receiver unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResyncError {
    DifferentWorkspace,
    StaleSnapshot,
    Conflict,
    Gap {
        observed: Revision,
        previous: Revision,
    },
    InvalidEvent,
    Projection(BoundaryError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::{
            delivery::DeliveryLog, proposal::RequestId, registry::ActorRegistry,
            retention::RetainedHistory, role::ActorRole, sequencer::Sequencer,
        },
        edit::{
            ids::{ActorId, Nonce},
            intent::EditIntent,
            patch::Patch,
            splice::Splice,
            transaction::Transaction,
        },
        test_support::assert_law,
        text::{span::ByteSpan, utf8::Text},
        workspace::{
            buffer_lifecycle::{BufferLifecycle, ClosePolicy},
            catalog::BufferCatalog,
            events::submit_with_event,
            snapshots::VersionedWorkspace,
        },
    };

    #[test]
    fn delivery_recovery_law() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| (0..8).map(|_| rng.next_u64()).collect::<Vec<_>>(),
            |values| {
                let id = BufferId::new(1);
                let mut catalog = BufferCatalog::new();
                catalog.register(id, Text::from_str("initial")).unwrap();
                let mut actors = ActorRegistry::new();
                actors.register(ActorId::new(1), ActorRole::Human).unwrap();
                let mut source = Sequencer::new(catalog, actors);
                let base = source.snapshot();
                let mut log = DeliveryLog::new(base.clone());
                for (i, value) in values.iter().enumerate() {
                    let before = source.snapshot();
                    let old = before.catalog().get(id).unwrap();
                    let patch = Patch::new(vec![Splice::new(
                        ByteSpan::full_of(old),
                        Text::from_str(&format!("{value}\n界")),
                    )])
                    .unwrap();
                    let intent = EditIntent::for_actor(
                        ActorId::new(1),
                        Nonce::new(i as u64),
                        before.revision(),
                    );
                    let txn = Transaction::new(intent, vec![(id, patch)]).unwrap();
                    let (_, event) = submit_with_event(&mut source, txn);
                    log.record_workspace(&source, event.unwrap().receipt())
                        .unwrap();
                }
                let mut continuous = Resync::new(&base, id).unwrap();
                for event in log.events() {
                    assert!(continuous.delivery(event).unwrap().is_some());
                    let saved = continuous.clone();
                    assert_eq!(continuous.delivery(event), Ok(None));
                    assert_eq!(continuous, saved);
                }
                let mut recovered = Resync::new(&base, id).unwrap();
                let before = recovered.clone();
                assert!(matches!(
                    recovered.delivery(&log.events()[1 + (values[0] as usize % 7)]),
                    Err(ResyncError::Gap { .. })
                ));
                assert_eq!(recovered, before);
                recovered
                    .catch_up(&log.catch_up(recovered.revision()).unwrap())
                    .unwrap();
                assert_eq!(recovered.snapshot(), continuous.snapshot());
                assert_eq!(recovered.emacs(), continuous.emacs());
                assert_eq!(recovered.neovim(), continuous.neovim());
                assert_eq!(recovered.agent(), continuous.agent());
                let mut evicted = Resync::new(&base, id).unwrap();
                log.compact(Revision::new(1 + values[1] % 7)).unwrap();
                assert!(matches!(
                    log.catch_up(Revision::ZERO),
                    Ok(CatchUp::Snapshot(_))
                ));
                evicted
                    .catch_up(&log.catch_up(Revision::ZERO).unwrap())
                    .unwrap();
                assert_eq!(evicted.snapshot(), continuous.snapshot());
                assert_eq!(evicted.neovim(), continuous.neovim());
                assert_eq!(evicted.delivery(log.events().last().unwrap()), Ok(None));
                let saved = evicted.clone();
                assert_eq!(evicted.resync(&base), Err(ResyncError::StaleSnapshot));
                assert_eq!(evicted, saved);
                let foreign = VersionedWorkspace::new(base.catalog().clone()).capture();
                assert_eq!(
                    evicted.resync(&foreign),
                    Err(ResyncError::DifferentWorkspace)
                );
                assert_eq!(evicted, saved);
                let oldest = log.events()[0].clone();
                source.prune_before(Revision::new(8));
                assert_eq!(continuous.delivery(&oldest), Ok(None));
                let mut emacs = evicted.emacs().unwrap();
                let mut neovim = evicted.neovim().unwrap();
                let before = source.snapshot();
                let old = before.catalog().get(id).unwrap();
                let patch = Patch::new(vec![Splice::new(
                    ByteSpan::full_of(old),
                    Text::from_str("new\n界😀"),
                )])
                .unwrap();
                let intent =
                    EditIntent::for_actor(ActorId::new(1), Nonce::new(100), before.revision());
                let txn = Transaction::new(intent, vec![(id, patch)]).unwrap();
                let (_, event) = submit_with_event(&mut source, txn);
                log.record_workspace(&source, event.unwrap().receipt())
                    .unwrap();
                let notification = evicted
                    .delivery(log.events().last().unwrap())
                    .unwrap()
                    .unwrap();
                let Notification::Change(formats) = notification else {
                    panic!("expected edit");
                };
                super::super::delta::apply_emacs(&mut emacs, &formats.emacs).unwrap();
                super::super::delta::apply_neovim(&mut neovim, &formats.neovim).unwrap();
                assert_eq!(emacs, evicted.emacs().unwrap());
                assert_eq!(neovim, evicted.neovim().unwrap());
            },
        );
    }

    #[test]
    fn silent_revision_advances_all_projections() {
        let id = BufferId::new(1);
        let other = BufferId::new(2);
        let mut catalog = BufferCatalog::new();
        catalog.register(id, Text::from_str("old")).unwrap();
        catalog.register(other, Text::from_str("other")).unwrap();
        let mut actors = ActorRegistry::new();
        actors.register(ActorId::new(1), ActorRole::Human).unwrap();
        let mut source = Sequencer::new(catalog, actors);
        let base = source.snapshot();
        let mut log = DeliveryLog::new(base.clone());
        for (index, buffer) in [other, id].into_iter().enumerate() {
            let before = source.snapshot();
            let patch = Patch::new(vec![Splice::new(
                ByteSpan::full_of(before.catalog().get(buffer).unwrap()),
                Text::from_str("new\n界"),
            )])
            .unwrap();
            let intent =
                EditIntent::for_actor(ActorId::new(1), Nonce::new(index as u64), before.revision());
            let (_, event) = submit_with_event(
                &mut source,
                Transaction::new(intent, vec![(buffer, patch)]).unwrap(),
            );
            log.record_workspace(&source, event.unwrap().receipt())
                .unwrap();
        }
        let mut client = Resync::new(&base, id).unwrap();
        let mut projections = Subscription::subscribe(&base, id).unwrap().1;
        let before = client.clone();
        let before_projections = projections.clone();
        let gap = CatchUp::Events(vec![log.events()[1].clone()]);
        assert!(matches!(
            client.catch_up_projected(&gap, &mut projections),
            Err(ResyncError::Gap { .. })
        ));
        assert_eq!(client, before);
        assert_eq!(projections, before_projections);
        source.prune_before(Revision::new(2));
        let changes = client
            .catch_up_projected(&log.catch_up(base.revision()).unwrap(), &mut projections)
            .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(projections.emacs(), Ok(&client.emacs().unwrap()));
        assert_eq!(projections.neovim(), Ok(&client.neovim().unwrap()));
        assert_eq!(projections.agent(), Ok(&client.agent().unwrap()));
        let mut reloaded = Resync::new(&base, id).unwrap();
        reloaded.resync(&source.snapshot()).unwrap();
        assert_eq!(client.delivery(&log.events()[0]), Ok(None));
        assert_eq!(reloaded.delivery(&log.events()[0]), Ok(None));
    }

    #[test]
    fn rejects_same_revision_different_payload() {
        let id = BufferId::new(1);
        let mut catalog = BufferCatalog::new();
        catalog.register(id, Text::from_str("abc")).unwrap();
        let mut actors = ActorRegistry::new();
        actors.register(ActorId::new(1), ActorRole::Human).unwrap();
        let mut source = Sequencer::new(catalog.clone(), actors.clone());
        let mut foreign = Sequencer::new(catalog, actors);
        let base = source.snapshot();
        let intent = EditIntent::for_actor(ActorId::new(1), Nonce::new(1), base.revision());
        let text = base.catalog().get(id).unwrap();
        let full = Patch::new(vec![Splice::new(
            ByteSpan::full_of(text),
            Text::from_str("abX"),
        )])
        .unwrap();
        let partial = Patch::new(vec![Splice::new(
            ByteSpan::validate_in(text, 2, 3).unwrap(),
            Text::from_str("X"),
        )])
        .unwrap();
        let original = Transaction::new(intent, vec![(id, full)]).unwrap();
        let altered = Transaction::new(intent, vec![(id, partial)]).unwrap();
        let (_, published) = submit_with_event(&mut source, original);
        let (_, forged) = submit_with_event(&mut foreign, altered.clone());
        let mut actual = DeliveryLog::new(base.clone());
        actual
            .record_workspace(&source, published.unwrap().receipt())
            .unwrap();
        let forged = forged.unwrap();
        let mut history = RetainedHistory::new(base.clone());
        let after = source.snapshot();
        history
            .record(
                RequestId::new(1),
                altered.clone(),
                altered.clone(),
                forged.receipt(),
                after.clone(),
            )
            .unwrap();
        let mut other_log = DeliveryLog::new(base.clone());
        other_log
            .record(&history, RequestId::new(1), altered, forged, after)
            .unwrap();
        let mut client = Resync::new(&base, id).unwrap();
        client.delivery(&actual.events()[0]).unwrap();
        let saved = client.clone();
        assert_eq!(
            client.delivery(&other_log.events()[0]),
            Err(ResyncError::Conflict)
        );
        assert_eq!(client, saved);
        let mut recovered = Resync::new(&base, id).unwrap();
        recovered.resync(&source.snapshot()).unwrap();
        let saved = recovered.clone();
        assert_eq!(recovered.delivery(&other_log.events()[0]), Ok(None));
        assert_eq!(recovered, saved);
    }

    #[test]
    fn lifecycle_update_rejects_initial_revision_atomically() {
        let id = BufferId::new(1);
        let mut source = BufferLifecycle::new();
        let initial = source.snapshot();
        source.open(id, Text::from_str("open")).unwrap();
        let mut client = Resync::new(&source.snapshot(), id).unwrap();
        let saved = client.clone();
        assert_eq!(
            client.lifecycle_update(&source, &initial),
            Err(ResyncError::InvalidEvent)
        );
        assert_eq!(client, saved);
    }

    #[test]
    fn retained_and_sequenced_catch_up_recover_all_surfaces() {
        use crate::editor::collab::delivery::replay;
        use crate::editor::interop::delta::{apply_emacs, apply_neovim};

        let id = BufferId::new(1);
        let mut catalog = BufferCatalog::new();
        catalog.register(id, Text::from_str("始まり\n😀")).unwrap();
        let mut actors = ActorRegistry::new();
        actors.register(ActorId::new(1), ActorRole::Human).unwrap();
        let mut source = Sequencer::new(catalog, actors);
        let base = source.snapshot();
        let mut history = RetainedHistory::new(base.clone());
        let mut retained = DeliveryLog::new(base.clone());
        let mut sequenced = DeliveryLog::new(base.clone());
        for (i, value) in ["一行\n界", "二行\n😀終\n", "続きé\n猫"].iter().enumerate() {
            let before = source.snapshot();
            let text = before.catalog().get(id).unwrap();
            let patch = Patch::new(vec![Splice::new(
                ByteSpan::full_of(text),
                Text::from_str(value),
            )])
            .unwrap();
            let intent =
                EditIntent::for_actor(ActorId::new(1), Nonce::new(i as u64), before.revision());
            let original = Transaction::new(intent, vec![(id, patch)]).unwrap();
            let (_, event) = submit_with_event(&mut source, original.clone());
            let event = event.unwrap();
            let after = source.snapshot();
            let request = RequestId::new(i as u64);
            history
                .record(
                    request,
                    original.clone(),
                    original.clone(),
                    event.receipt(),
                    after.clone(),
                )
                .unwrap();
            retained
                .record(&history, request, original, event.clone(), after)
                .unwrap();
            sequenced
                .record_workspace(&source, event.receipt())
                .unwrap();
        }

        let mut continuous = Resync::new(&base, id).unwrap();
        let mut emacs = continuous.emacs().unwrap();
        let mut neovim = continuous.neovim().unwrap();
        let mut reconnected = Resync::new(&base, id).unwrap();
        let mut reconnect_emacs = reconnected.emacs().unwrap();
        let mut reconnect_neovim = reconnected.neovim().unwrap();
        for (i, event) in sequenced.events().iter().enumerate() {
            let Notification::Change(delta) = continuous.delivery(event).unwrap().unwrap() else {
                panic!("expected change")
            };
            apply_emacs(&mut emacs, &delta.emacs).unwrap();
            apply_neovim(&mut neovim, &delta.neovim).unwrap();
            if i == 0 {
                let Notification::Change(delta) = reconnected.delivery(event).unwrap().unwrap()
                else {
                    panic!("expected change")
                };
                apply_emacs(&mut reconnect_emacs, &delta.emacs).unwrap();
                apply_neovim(&mut reconnect_neovim, &delta.neovim).unwrap();
            } else if i == 2 {
                let saved = reconnected.clone();
                assert!(matches!(
                    reconnected.delivery(event),
                    Err(ResyncError::Gap { .. })
                ));
                assert_eq!(reconnected, saved);
                reconnected.resync(event.before()).unwrap();
                reconnect_emacs = reconnected.emacs().unwrap();
                reconnect_neovim = reconnected.neovim().unwrap();
                let Notification::Change(delta) = reconnected.delivery(event).unwrap().unwrap()
                else {
                    panic!("expected change")
                };
                apply_emacs(&mut reconnect_emacs, &delta.emacs).unwrap();
                apply_neovim(&mut reconnect_neovim, &delta.neovim).unwrap();
            }
        }
        assert_eq!(reconnect_emacs, emacs);
        assert_eq!(reconnect_emacs.text.as_bytes(), emacs.text.as_bytes());
        assert!(reconnect_emacs.text.chars().eq(emacs.text.chars()));
        assert_eq!(reconnect_neovim, neovim);
        assert_eq!(
            snapshot::decode_neovim(&neovim).unwrap().text,
            snapshot::decode_emacs(&emacs).text
        );

        let foreign = VersionedWorkspace::new(base.catalog().clone()).capture();
        let mut guarded = Resync::new(&base, id).unwrap();
        let saved = guarded.clone();
        assert_eq!(
            guarded.catch_up(&CatchUp::Snapshot(foreign)),
            Err(ResyncError::DifferentWorkspace)
        );
        assert_eq!(guarded, saved);
        let mut foreign_catalog = BufferCatalog::new();
        foreign_catalog
            .register(id, Text::from_str("foreign"))
            .unwrap();
        let mut foreign_actors = ActorRegistry::new();
        foreign_actors
            .register(ActorId::new(1), ActorRole::Human)
            .unwrap();
        let mut foreign_source = Sequencer::new(foreign_catalog, foreign_actors);
        let foreign_base = foreign_source.snapshot();
        let foreign_txn = Transaction::new(
            EditIntent::for_actor(ActorId::new(1), Nonce::new(99), Revision::ZERO),
            vec![(
                id,
                Patch::new(vec![Splice::new(
                    ByteSpan::full_of(foreign_base.catalog().get(id).unwrap()),
                    Text::from_str("other"),
                )])
                .unwrap(),
            )],
        )
        .unwrap();
        let (_, foreign_event) = submit_with_event(&mut foreign_source, foreign_txn);
        let mut foreign_log = DeliveryLog::new(foreign_base);
        foreign_log
            .record_workspace(&foreign_source, foreign_event.unwrap().receipt())
            .unwrap();
        assert_eq!(
            guarded.delivery(&foreign_log.events()[0]),
            Err(ResyncError::DifferentWorkspace)
        );
        assert_eq!(guarded, saved);
        let broken = CatchUp::Events(vec![
            sequenced.events()[0].clone(),
            sequenced.events()[2].clone(),
        ]);
        let mut projections = Subscription::subscribe(&base, id).unwrap().1;
        let saved_projections = projections.clone();
        assert!(matches!(
            guarded.catch_up_projected(&broken, &mut projections),
            Err(ResyncError::Gap { .. })
        ));
        assert_eq!(guarded, saved);
        assert_eq!(projections, saved_projections);

        let mut recovered_projections = Vec::new();
        for log in [&mut retained, &mut sequenced] {
            let response = log.catch_up(Revision::ZERO).unwrap();
            let CatchUp::Events(events) = &response else {
                panic!("expected events")
            };
            let (catalog, revision) = replay(&base, events).unwrap();
            let mut client = Resync::new(&base, id).unwrap();
            let mut projected_client = client.clone();
            let mut projections = Subscription::subscribe(&base, id).unwrap().1;
            client.catch_up(&response).unwrap();
            projected_client
                .catch_up_projected(&response, &mut projections)
                .unwrap();
            assert_eq!(client.snapshot().catalog(), &catalog);
            assert_eq!(client.revision(), revision);
            assert_eq!(client.snapshot(), continuous.snapshot());
            assert_eq!(projections.agent(), Ok(&client.agent().unwrap()));
            assert_eq!(projections.emacs(), Ok(&emacs));
            assert_eq!(projections.neovim(), Ok(&neovim));
            log.compact(Revision::new(2)).unwrap();
            let response = log.catch_up(Revision::ZERO).unwrap();
            assert!(matches!(response, CatchUp::Snapshot(_)));
            let mut client = Resync::new(&base, id).unwrap();
            let mut projected_client = client.clone();
            let mut projections = Subscription::subscribe(&base, id).unwrap().1;
            client.catch_up(&response).unwrap();
            projected_client
                .catch_up_projected(&response, &mut projections)
                .unwrap();
            assert_eq!(client.snapshot(), continuous.snapshot());
            assert_eq!(projected_client.snapshot(), continuous.snapshot());
            assert_eq!(projections.emacs(), Ok(&continuous.emacs().unwrap()));
            assert_eq!(projections.neovim(), Ok(&continuous.neovim().unwrap()));
            assert_eq!(projections.agent(), Ok(&continuous.agent().unwrap()));
            recovered_projections.push((projected_client, projections));
        }
        let before = source.snapshot();
        let patch = Patch::new(vec![Splice::new(
            ByteSpan::full_of(before.catalog().get(id).unwrap()),
            Text::from_str("after recovery"),
        )])
        .unwrap();
        let intent = EditIntent::for_actor(ActorId::new(1), Nonce::new(50), before.revision());
        let (_, event) = submit_with_event(
            &mut source,
            Transaction::new(intent, vec![(id, patch)]).unwrap(),
        );
        sequenced
            .record_workspace(&source, event.unwrap().receipt())
            .unwrap();
        let event = sequenced.events().last().unwrap();
        continuous.delivery(event).unwrap();
        for (client, projections) in &mut recovered_projections {
            client
                .catch_up_projected(&CatchUp::Events(vec![event.clone()]), projections)
                .unwrap();
            assert_eq!(projections.emacs(), Ok(&continuous.emacs().unwrap()));
            assert_eq!(projections.neovim(), Ok(&continuous.neovim().unwrap()));
            assert_eq!(projections.agent(), Ok(&continuous.agent().unwrap()));
        }
    }

    #[test]
    fn projected_snapshot_recovery_after_close() {
        let id = BufferId::new(1);
        let other = BufferId::new(2);
        let mut source = BufferLifecycle::new();
        source.open(id, Text::from_str("open")).unwrap();
        source.open(other, Text::from_str("other")).unwrap();
        let base = source.snapshot();
        let mut continuous = Resync::new(&base, id).unwrap();
        let mut recovered = continuous.clone();
        let mut projected = Subscription::subscribe(&base, id).unwrap().1;
        let saved = projected.clone();
        source.close(id, ClosePolicy::Discard).unwrap();
        let closed = source.snapshot();
        let revision = closed.revision();
        assert!(matches!(
            continuous.lifecycle_event(&source, revision, &closed),
            Ok(Some(Notification::Closed { .. }))
        ));
        assert_eq!(
            recovered.catch_up_projected(&CatchUp::Snapshot(closed), &mut projected),
            Ok(vec![])
        );
        assert!(recovered.is_closed());
        assert_eq!(projected, saved);
        assert_eq!(recovered.snapshot(), continuous.snapshot());
        source.update(other, Text::from_str("later")).unwrap();
        let after = source.snapshot();
        assert_eq!(continuous.lifecycle_update(&source, &after), Ok(None));
        assert_eq!(recovered.lifecycle_update(&source, &after), Ok(None));
        assert_eq!(recovered.snapshot(), continuous.snapshot());
        assert_eq!(recovered.emacs(), continuous.emacs());
        assert_eq!(recovered.neovim(), continuous.neovim());
        assert_eq!(recovered.agent(), continuous.agent());
    }

    #[test]
    fn lifecycle_recovery_law() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| (0..6).map(|_| rng.next_u64()).collect::<Vec<_>>(),
            |values| {
                let id = BufferId::new(1);
                let other = BufferId::new(2);
                let mut source = BufferLifecycle::new();
                source.open(id, Text::from_str("first")).unwrap();
                let base = source.snapshot();
                let mut continuous = Resync::new(&base, id).unwrap();
                source.open(other, Text::from_str("other")).unwrap();
                let open = *source.events().last().unwrap();
                let opened = source.snapshot();
                continuous
                    .lifecycle_event(&source, open.revision, &opened)
                    .unwrap();
                for value in values {
                    source
                        .update(id, Text::from_str(&format!("{value}😀\n")))
                        .unwrap();
                    continuous
                        .lifecycle_update(&source, &source.snapshot())
                        .unwrap();
                }
                let mut recovered = Resync::new(&base, id).unwrap();
                recovered.resync(&source.snapshot()).unwrap();
                assert_eq!(recovered.emacs(), continuous.emacs());
                assert_eq!(recovered.neovim(), continuous.neovim());
                source.close(id, ClosePolicy::Discard).unwrap();
                let close = *source.events().last().unwrap();
                let closed = source.snapshot();
                assert!(matches!(
                    continuous.lifecycle_event(&source, close.revision, &closed),
                    Ok(Some(Notification::Closed { .. }))
                ));
                assert!(matches!(
                    recovered.lifecycle_event(&source, close.revision, &closed),
                    Ok(Some(Notification::Closed { .. }))
                ));
                assert_eq!(
                    recovered.lifecycle_event(&source, close.revision, &closed),
                    Ok(None)
                );
                let saved = recovered.clone();
                let foreign = BufferLifecycle::new();
                assert_eq!(
                    recovered.lifecycle_event(&foreign, close.revision, &closed),
                    Err(ResyncError::DifferentWorkspace)
                );
                assert_eq!(recovered, saved);
                let mut skipped = Resync::new(&base, id).unwrap();
                skipped.resync(&closed).unwrap();
                assert_eq!(
                    skipped.lifecycle_event(&source, close.revision, &closed),
                    Ok(None)
                );
                assert!(skipped.is_closed());
                assert_eq!(skipped.emacs(), continuous.emacs());
                assert_eq!(skipped.neovim(), continuous.neovim());
                assert_eq!(skipped.agent(), continuous.agent());
                source.update(other, Text::from_str("after close")).unwrap();
                let after = source.snapshot();
                assert_eq!(continuous.lifecycle_update(&source, &after), Ok(None));
                assert_eq!(skipped.lifecycle_update(&source, &after), Ok(None));
                assert_eq!(skipped.snapshot(), continuous.snapshot());
                let mut reloaded = Resync::new(&base, id).unwrap();
                reloaded.resync(&after).unwrap();
                assert_eq!(reloaded.lifecycle_update(&source, &after), Ok(None));
            },
        );
    }
}

impl std::fmt::Display for ResyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ResyncError {}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    Edit(WorkspaceEvent),
    Lifecycle(LifecycleEvent),
    Update,
}

/// One buffer's observed workspace state and accepted event identities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resync {
    buffer: BufferId,
    current: WorkspaceSnapshot,
    origins: SnapshotStore,
    subscription: Option<Subscription>,
    accepted: BTreeMap<Revision, (Event, WorkspaceSnapshot)>,
}

impl Resync {
    /// Start from a live buffer capture.
    pub fn new(initial: &WorkspaceSnapshot, buffer: BufferId) -> Result<Self, ResyncError> {
        let (subscription, _) =
            Subscription::subscribe(initial, buffer).map_err(ResyncError::Projection)?;
        let mut origins = SnapshotStore::new();
        origins.record(initial.clone()).expect("empty store");
        Ok(Self {
            buffer,
            current: initial.clone(),
            origins,
            subscription: Some(subscription),
            accepted: BTreeMap::new(),
        })
    }

    pub fn snapshot(&self) -> &WorkspaceSnapshot {
        &self.current
    }
    pub fn revision(&self) -> Revision {
        self.current.revision()
    }
    pub fn is_closed(&self) -> bool {
        self.current.catalog().is_tombstoned(self.buffer)
    }
    pub fn emacs(&self) -> Result<snapshot::EmacsSnapshot, BoundaryError> {
        snapshot::encode_emacs(&self.current, self.buffer)
    }
    pub fn neovim(&self) -> Result<snapshot::NeovimSnapshot, BoundaryError> {
        snapshot::encode_neovim(&self.current, self.buffer)
    }
    pub fn agent(&self) -> Result<snapshot::AgentSnapshot, BoundaryError> {
        snapshot::encode_agent(&self.current, self.buffer)
    }

    // Check workspace identity even when a capture is stale.
    fn check_origin(&self, next: &WorkspaceSnapshot) -> Result<(), ResyncError> {
        let mut origins = self.origins.clone();
        match origins.record(next.clone()) {
            Err(SnapshotError::DifferentWorkspace) => Err(ResyncError::DifferentWorkspace),
            _ => Ok(()),
        }
    }

    /// Replace observed state with a newer capture without replaying notifications.
    pub fn resync(&mut self, next: &WorkspaceSnapshot) -> Result<(), ResyncError> {
        self.check_origin(next)?;
        if next.revision() < self.revision() {
            return Err(ResyncError::StaleSnapshot);
        }
        if next.revision() == self.revision() {
            return if next == &self.current {
                Ok(())
            } else {
                Err(ResyncError::Conflict)
            };
        }
        if self.is_closed() && !next.catalog().is_tombstoned(self.buffer) {
            return Err(ResyncError::Conflict);
        }
        let subscription = if next.catalog().is_tombstoned(self.buffer) {
            None
        } else {
            Some(
                Subscription::subscribe(next, self.buffer)
                    .map_err(ResyncError::Projection)?
                    .0,
            )
        };
        let mut origins = self.origins.clone();
        origins
            .record(next.clone())
            .map_err(|_| ResyncError::Conflict)?;
        origins.prune_before(next.revision());
        self.origins = origins;
        self.current = next.clone();
        self.subscription = subscription;
        Ok(())
    }

    fn apply(
        &mut self,
        event: Event,
        previous: Revision,
        revision: Revision,
        next: &WorkspaceSnapshot,
    ) -> Result<Option<Notification>, ResyncError> {
        self.check_origin(next)?;
        if revision <= self.revision() {
            return match self.accepted.get(&revision) {
                Some((old, capture)) if old == &event && capture == next => Ok(None),
                None if revision < self.revision() || next == &self.current => Ok(None),
                _ => Err(ResyncError::Conflict),
            };
        }
        if previous != self.revision() {
            return Err(ResyncError::Gap {
                observed: self.revision(),
                previous,
            });
        }
        if previous.next().ok() != Some(revision) || next.revision() != revision {
            return Err(ResyncError::InvalidEvent);
        }
        let mut subscription = self.subscription.clone();
        let notification = if let Some(stream) = &mut subscription {
            let update = match &event {
                Event::Edit(edit) => stream.apply_workspace_event(edit, next),
                Event::Lifecycle(lifecycle) => stream.apply_lifecycle_event(lifecycle, next),
                Event::Update => stream.apply_lifecycle_update(next),
            }
            .map_err(|_| ResyncError::InvalidEvent)?;
            update.notification().cloned()
        } else {
            // A snapshot may have skipped the close; continue validating the whole catalog.
            let mut expected = self.current.catalog().clone();
            match &event {
                Event::Edit(edit) => {
                    expected = edit
                        .apply_to_catalog(&expected)
                        .map_err(|_| ResyncError::InvalidEvent)?
                }
                Event::Lifecycle(lifecycle) => match lifecycle.kind {
                    LifecycleKind::Opened => expected
                        .register(
                            lifecycle.buffer,
                            next.catalog()
                                .get(lifecycle.buffer)
                                .ok_or(ResyncError::InvalidEvent)?
                                .clone(),
                        )
                        .map_err(|_| ResyncError::InvalidEvent)?,
                    LifecycleKind::Closed => {
                        expected
                            .remove(lifecycle.buffer)
                            .map_err(|_| ResyncError::InvalidEvent)?;
                    }
                },
                Event::Update => {
                    let mut changed = 0;
                    for id in expected.live_ids() {
                        let new = next.catalog().get(id).ok_or(ResyncError::InvalidEvent)?;
                        if expected.get(id) != Some(new) {
                            expected
                                .update(id, new.clone())
                                .map_err(|_| ResyncError::InvalidEvent)?;
                            changed += 1;
                        }
                    }
                    if changed > 1 {
                        return Err(ResyncError::InvalidEvent);
                    }
                }
            }
            if &expected != next.catalog() {
                return Err(ResyncError::InvalidEvent);
            }
            None
        };
        let mut origins = self.origins.clone();
        origins
            .record(next.clone())
            .map_err(|_| ResyncError::InvalidEvent)?;
        origins.prune_before(revision);
        self.origins = origins;
        self.subscription = subscription;
        self.current = next.clone();
        self.accepted.insert(revision, (event, next.clone()));
        Ok(notification)
    }

    fn workspace_event(
        &mut self,
        event: &WorkspaceEvent,
        next: &WorkspaceSnapshot,
    ) -> Result<Option<Notification>, ResyncError> {
        let receipt = event.receipt();
        self.apply(
            Event::Edit(event.clone()),
            receipt.previous(),
            receipt.committed(),
            next,
        )
    }
    /// Obtain the event from its workspace authority, rather than an unbound copy.
    pub fn lifecycle_event(
        &mut self,
        source: &BufferLifecycle,
        revision: Revision,
        next: &WorkspaceSnapshot,
    ) -> Result<Option<Notification>, ResyncError> {
        self.check_origin(&source.snapshot())?;
        self.check_origin(next)?;
        let event = source
            .events()
            .iter()
            .find(|event| event.revision == revision)
            .ok_or(ResyncError::InvalidEvent)?;
        if next.revision() != revision {
            return Err(ResyncError::InvalidEvent);
        }
        if revision <= self.revision() && !self.accepted.contains_key(&revision) {
            return if revision == self.revision() && next != &self.current {
                Err(ResyncError::Conflict)
            } else {
                Ok(None)
            };
        }
        self.apply(
            Event::Lifecycle(*event),
            event.previous,
            event.revision,
            next,
        )
    }
    pub fn lifecycle_update(
        &mut self,
        source: &BufferLifecycle,
        next: &WorkspaceSnapshot,
    ) -> Result<Option<Notification>, ResyncError> {
        self.check_origin(&source.snapshot())?;
        self.check_origin(next)?;
        if source
            .events()
            .iter()
            .any(|event| event.revision == next.revision())
        {
            return Err(ResyncError::Conflict);
        }
        if next.revision() > source.snapshot().revision() {
            return Err(ResyncError::InvalidEvent);
        }
        let previous = Revision::new(
            next.revision()
                .raw()
                .checked_sub(1)
                .ok_or(ResyncError::InvalidEvent)?,
        );
        if next.revision() <= self.revision() && !self.accepted.contains_key(&next.revision()) {
            return if next.revision() == self.revision() && next != &self.current {
                Err(ResyncError::Conflict)
            } else {
                Ok(None)
            };
        }
        self.apply(Event::Update, previous, next.revision(), next)
    }
    pub fn delivery(&mut self, event: &DeliveryEvent) -> Result<Option<Notification>, ResyncError> {
        self.check_origin(event.before())?;
        self.check_origin(event.after())?;
        if let Some((accepted, after)) = self.accepted.get(&event.receipt().committed()) {
            return if accepted == &Event::Edit(event.workspace_event().clone())
                && after == event.after()
            {
                Ok(None)
            } else {
                Err(ResyncError::Conflict)
            };
        }
        if crate::editor::collab::delivery::replay(event.before(), std::slice::from_ref(event))
            .is_err()
        {
            return Err(ResyncError::InvalidEvent);
        }
        if event.receipt().committed() <= self.revision()
            && !self.accepted.contains_key(&event.receipt().committed())
        {
            return if event.receipt().committed() == self.revision()
                && event.after() != &self.current
            {
                Err(ResyncError::Conflict)
            } else {
                Ok(None)
            };
        }
        if event.receipt().committed() > self.revision() && event.before() != &self.current {
            self.check_origin(event.before())?;
            if event.receipt().previous() == self.revision() {
                return Err(ResyncError::Conflict);
            }
        }
        self.workspace_event(event.workspace_event(), event.after())
    }
    /// Apply a catch-up response atomically; duplicates produce no notifications.
    pub fn catch_up(&mut self, response: &CatchUp) -> Result<Vec<Notification>, ResyncError> {
        let mut candidate = self.clone();
        let mut notifications = Vec::new();
        match response {
            CatchUp::Snapshot(snapshot) => candidate.resync(snapshot)?,
            CatchUp::Events(events) => {
                for event in events {
                    if let Some(notification) = candidate.delivery(event)? {
                        notifications.push(notification);
                    }
                }
            }
        }
        *self = candidate;
        Ok(notifications)
    }

    /// Apply every revision to frontend projections, including silent revisions.
    pub fn catch_up_projected(
        &mut self,
        response: &CatchUp,
        projections: &mut InitialSnapshots,
    ) -> Result<Vec<Notification>, ResyncError> {
        let mut candidate = self.clone();
        let mut projected = projections.clone();
        let mut notifications = Vec::new();
        match response {
            CatchUp::Snapshot(snapshot) => {
                candidate.resync(snapshot)?;
                if !candidate.is_closed() {
                    projected = Subscription::subscribe(snapshot, candidate.buffer)
                        .map_err(ResyncError::Projection)?
                        .1;
                }
            }
            CatchUp::Events(events) => {
                for event in events {
                    let previous_revision = candidate.revision();
                    let stream = candidate.subscription.clone();
                    let notification = candidate.delivery(event)?;
                    if let Some(mut stream) =
                        stream.filter(|_| event.receipt().committed() > previous_revision)
                    {
                        let update = stream
                            .apply_workspace_event(event.workspace_event(), event.after())
                            .map_err(|_| ResyncError::InvalidEvent)?;
                        update
                            .apply(&mut projected)
                            .map_err(ResyncError::Projection)?;
                    }
                    if let Some(notification) = notification {
                        notifications.push(notification);
                    }
                }
            }
        }
        *self = candidate;
        *projections = projected;
        Ok(notifications)
    }
}
