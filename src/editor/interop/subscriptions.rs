//! Revisioned, per-buffer notification streams.
use super::{
    delta::{self, DeltaFormats},
    error::{BoundaryError, ErrorCode},
    snapshot::{self, AgentSnapshot, BufferSnapshot, EmacsSnapshot, NeovimSnapshot},
};
use crate::editor::{
    collab::{receipt::CommitReceipt, sequencer::Sequencer},
    edit::{
        ids::{ActorId, BufferId, Nonce},
        intent::EditIntent,
        patch::Patch,
        splice::Splice,
        transaction::Transaction,
    },
    text::{span::ByteSpan, utf8::Text},
    workspace::{
        buffer_lifecycle::{BufferLifecycle, LifecycleEvent, LifecycleKind},
        events::WorkspaceEvent,
        snapshots::{SnapshotStore, WorkspaceSnapshot},
        version::Revision,
    },
};

/// Initial projections for all supported frontends.
#[derive(Clone, PartialEq, Eq)]
pub struct InitialSnapshots {
    emacs: EmacsSnapshot,
    neovim: NeovimSnapshot,
    agent: AgentSnapshot,
    closed: bool,
    anchor: WorkspaceSnapshot,
}

impl std::fmt::Debug for InitialSnapshots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InitialSnapshots")
            .field("buffer", &self.emacs.buffer)
            .field("revision", &self.emacs.revision)
            .field("closed", &self.closed)
            .finish()
    }
}

impl InitialSnapshots {
    /// Observe the Emacs projection, rejecting a closed subscription.
    pub fn emacs(&self) -> Result<&EmacsSnapshot, BoundaryError> {
        if self.closed {
            return Err(BoundaryError::new(
                ErrorCode::BufferClosed,
                "buffer is closed",
            ));
        }
        Ok(&self.emacs)
    }
    /// Observe the Neovim projection, rejecting a closed subscription.
    pub fn neovim(&self) -> Result<&NeovimSnapshot, BoundaryError> {
        if self.closed {
            return Err(BoundaryError::new(
                ErrorCode::BufferClosed,
                "buffer is closed",
            ));
        }
        Ok(&self.neovim)
    }
    /// Observe the Agent projection, rejecting a closed subscription.
    pub fn agent(&self) -> Result<&AgentSnapshot, BoundaryError> {
        if self.closed {
            return Err(BoundaryError::new(
                ErrorCode::BufferClosed,
                "buffer is closed",
            ));
        }
        Ok(&self.agent)
    }
}

/// A change for each frontend, or its single terminal notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notification {
    Change(DeltaFormats),
    /// Emacs kill-buffer, Neovim detach, and Agent closed.
    Closed {
        buffer: BufferId,
        revision: Revision,
    },
}

/// One consumed revision, including silent revisions with no buffer notification.
#[derive(Clone, PartialEq, Eq)]
pub struct Update {
    previous: Revision,
    revision: Revision,
    notification: Option<Notification>,
    buffer: BufferId,
    before: WorkspaceSnapshot,
    after: WorkspaceSnapshot,
}

impl std::fmt::Debug for Update {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Update")
            .field("previous", &self.previous)
            .field("revision", &self.revision)
            .field("buffer", &self.buffer)
            .field("has_notification", &self.notification.is_some())
            .finish()
    }
}

impl Update {
    /// The workspace revision before this update.
    pub fn previous(&self) -> Revision {
        self.previous
    }
    /// The workspace revision after this update.
    pub fn revision(&self) -> Revision {
        self.revision
    }
    /// The buffer notification, absent when this buffer did not change.
    pub fn notification(&self) -> Option<&Notification> {
        self.notification.as_ref()
    }
    /// Apply a revision atomically to all three frontend projections.
    pub fn apply(&self, snapshots: &mut InitialSnapshots) -> Result<(), BoundaryError> {
        let mut next = snapshots.clone();
        if next.anchor != self.before
            || next.emacs.buffer != self.buffer
            || next.emacs.buffer != next.neovim.buffer
            || next.emacs.buffer != next.agent.buffer
            || next.emacs.revision != self.previous
            || next.neovim.revision != self.previous
            || next.agent.revision != self.previous
            || self.previous.next().ok() != Some(self.revision)
        {
            return Err(mismatch("projection revision or buffer mismatch"));
        }
        if next.closed && self.notification.is_some() {
            return Err(mismatch("notification after close"));
        }
        if let Some(Notification::Change(formats)) = &self.notification {
            delta::apply_emacs(&mut next.emacs, &formats.emacs)?;
            delta::apply_neovim(&mut next.neovim, &formats.neovim)?;
            delta::apply_agent(&mut next.agent, &formats.agent)?;
        } else {
            if let Some(Notification::Closed { buffer, revision }) = &self.notification {
                if *buffer != next.emacs.buffer || *revision != self.revision {
                    return Err(mismatch("invalid close"));
                }
                next.closed = true;
            }
            next.emacs.revision = self.revision;
            next.neovim.revision = self.revision;
            next.agent.revision = self.revision;
        }
        if next.emacs.revision != self.revision
            || next.neovim.revision != self.revision
            || next.agent.revision != self.revision
        {
            return Err(mismatch("change revision mismatch"));
        }
        next.anchor = self.after.clone();
        *snapshots = next;
        Ok(())
    }
}

/// A stream bound to one workspace and one buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subscription {
    buffer: BufferId,
    current: WorkspaceSnapshot,
    origins: SnapshotStore,
    closed: bool,
}

fn mismatch(message: &str) -> BoundaryError {
    BoundaryError::new(
        ErrorCode::RevisionMismatch,
        format!("resync required: {message}"),
    )
}

impl Subscription {
    /// Subscribe to a live buffer and obtain all initial frontend projections.
    pub fn subscribe(
        initial: &WorkspaceSnapshot,
        buffer: BufferId,
    ) -> Result<(Self, InitialSnapshots), BoundaryError> {
        let projections = InitialSnapshots {
            emacs: snapshot::encode_emacs(initial, buffer)?,
            neovim: snapshot::encode_neovim(initial, buffer)?,
            agent: snapshot::encode_agent(initial, buffer)?,
            closed: false,
            anchor: initial.clone(),
        };
        let mut origins = SnapshotStore::new();
        origins
            .record(initial.clone())
            .map_err(|_| mismatch("invalid initial snapshot"))?;
        Ok((
            Self {
                buffer,
                current: initial.clone(),
                origins,
                closed: false,
            },
            projections,
        ))
    }

    /// Whether the terminal notification has been emitted.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    fn validate_next(
        &self,
        previous: Revision,
        revision: Revision,
        next: &WorkspaceSnapshot,
    ) -> Result<SnapshotStore, BoundaryError> {
        if previous != self.current.revision()
            || previous.next().ok() != Some(revision)
            || revision != next.revision()
        {
            return Err(mismatch("non-contiguous event or snapshot"));
        }
        let mut origins = self.origins.clone();
        origins
            .record(next.clone())
            .map_err(|_| mismatch("different workspace or revision"))?;
        origins.prune_before(revision);
        Ok(origins)
    }

    /// Consume an edit and its resulting capture atomically.
    pub fn workspace_event(
        &mut self,
        sequencer: &Sequencer,
        next: &WorkspaceSnapshot,
    ) -> Result<Update, BoundaryError> {
        if sequencer.snapshots().get(next.revision()) != Some(next) {
            return Err(mismatch("snapshot is not from this sequencer"));
        }
        let receipt = *sequencer
            .journal()
            .lookup_revision(next.revision())
            .ok_or_else(|| mismatch("missing receipt"))?;
        let event = WorkspaceEvent::from_receipt(sequencer, receipt)
            .ok_or_else(|| mismatch("missing event"))?;
        self.apply_workspace_event(&event, next)
    }

    /// Consume a saved edit event and its resulting snapshot atomically.
    pub fn apply_workspace_event(
        &mut self,
        event: &WorkspaceEvent,
        next: &WorkspaceSnapshot,
    ) -> Result<Update, BoundaryError> {
        let receipt = event.receipt();
        let origins = self.validate_next(receipt.previous(), receipt.committed(), next)?;
        let expected = event
            .apply_to_catalog(self.current.catalog())
            .map_err(|_| mismatch("event cannot apply"))?;
        if &expected != next.catalog() {
            return Err(mismatch("event and snapshot differ"));
        }
        let mut notification = None;
        if !self.closed
            && self.current.catalog().get(self.buffer) != next.catalog().get(self.buffer)
        {
            let base = BufferSnapshot {
                buffer: self.buffer,
                revision: self.current.revision(),
                text: self
                    .current
                    .catalog()
                    .get(self.buffer)
                    .ok_or_else(|| mismatch("buffer missing"))?
                    .clone(),
            };
            let intent = receipt.intent().with_base(self.current.revision());
            let transaction = Transaction::new(intent, event.patches().to_vec())
                .map_err(|_| mismatch("invalid event"))?;
            let projected_receipt = CommitReceipt::new(intent, self.current.revision())
                .map_err(|_| mismatch("revision overflow"))?;
            let mut formats = delta::emit_receipt(&projected_receipt, &transaction, &base)?;
            formats.emacs.version.target_revision = receipt.committed();
            formats.neovim.version.target_revision = receipt.committed();
            formats.agent.version.target_revision = receipt.committed();
            notification = Some(Notification::Change(formats));
        }
        let before = std::mem::replace(&mut self.current, next.clone());
        self.origins = origins;
        Ok(Update {
            previous: receipt.previous(),
            revision: receipt.committed(),
            notification,
            buffer: self.buffer,
            before,
            after: next.clone(),
        })
    }

    /// Consume an open or close transition and its resulting capture atomically.
    pub fn lifecycle_event(
        &mut self,
        lifecycle: &BufferLifecycle,
    ) -> Result<Update, BoundaryError> {
        let next = lifecycle.snapshot();
        let event = lifecycle
            .events()
            .iter()
            .find(|event| event.revision == next.revision())
            .ok_or_else(|| mismatch("missing lifecycle event"))?;
        self.apply_lifecycle_event(event, &next)
    }

    /// Consume a saved lifecycle event and its resulting snapshot atomically.
    pub fn apply_lifecycle_event(
        &mut self,
        event: &LifecycleEvent,
        next: &WorkspaceSnapshot,
    ) -> Result<Update, BoundaryError> {
        let origins = self.validate_next(event.previous, event.revision, next)?;
        let mut expected = self.current.catalog().clone();
        match event.kind {
            LifecycleKind::Opened => {
                let text = next
                    .catalog()
                    .get(event.buffer)
                    .ok_or_else(|| mismatch("opened buffer missing"))?;
                expected
                    .register(event.buffer, text.clone())
                    .map_err(|_| mismatch("invalid open"))?;
            }
            LifecycleKind::Closed => {
                expected
                    .remove(event.buffer)
                    .map_err(|_| mismatch("invalid close"))?;
            }
        }
        if &expected != next.catalog() {
            return Err(mismatch("lifecycle and snapshot differ"));
        }
        let notification =
            if !self.closed && event.buffer == self.buffer && event.kind == LifecycleKind::Closed {
                Some(Notification::Closed {
                    buffer: self.buffer,
                    revision: event.revision,
                })
            } else {
                None
            };
        let before = std::mem::replace(&mut self.current, next.clone());
        self.origins = origins;
        if notification.is_some() {
            self.closed = true;
        }
        Ok(Update {
            previous: event.previous,
            revision: event.revision,
            notification,
            buffer: self.buffer,
            before,
            after: next.clone(),
        })
    }

    /// Consume a direct catalog removal without a lifecycle event.
    pub fn removal(&mut self, next: &WorkspaceSnapshot) -> Result<Update, BoundaryError> {
        let previous = self.current.revision();
        let origins = self.validate_next(previous, next.revision(), next)?;
        let mut expected = self.current.catalog().clone();
        let removed: Vec<_> = expected
            .live_ids()
            .into_iter()
            .filter(|id| !next.catalog().contains_live(*id))
            .collect();
        if removed.len() != 1 {
            return Err(mismatch("invalid removal"));
        }
        expected
            .remove(removed[0])
            .map_err(|_| mismatch("invalid removal"))?;
        if &expected != next.catalog() {
            return Err(mismatch("removal and snapshot differ"));
        }
        let notification =
            (!self.closed && removed[0] == self.buffer).then_some(Notification::Closed {
                buffer: self.buffer,
                revision: next.revision(),
            });
        let before = std::mem::replace(&mut self.current, next.clone());
        self.origins = origins;
        if notification.is_some() {
            self.closed = true;
        }
        Ok(Update {
            previous,
            revision: next.revision(),
            notification,
            buffer: self.buffer,
            before,
            after: next.clone(),
        })
    }

    /// Consume an update captured by a lifecycle workspace, preserving its origin.
    /// The snapshot-only lifecycle update API has no splice history, so a full-buffer
    /// replacement is used when the subscribed content changed.
    pub fn lifecycle_update(
        &mut self,
        lifecycle: &BufferLifecycle,
    ) -> Result<Update, BoundaryError> {
        let next = lifecycle.snapshot();
        self.apply_lifecycle_update(&next)
    }

    /// Consume a saved snapshot captured by a lifecycle update.
    pub fn apply_lifecycle_update(
        &mut self,
        next: &WorkspaceSnapshot,
    ) -> Result<Update, BoundaryError> {
        let previous = self.current.revision();
        let origins = self.validate_next(previous, next.revision(), next)?;
        let mut expected = self.current.catalog().clone();
        let mut changes = Vec::new();
        for id in expected.live_ids() {
            let old = expected.get(id).expect("live").clone();
            let new = next
                .catalog()
                .get(id)
                .ok_or_else(|| mismatch("missing buffer"))?;
            if &old != new {
                changes.push((id, old, new.clone()));
            }
        }
        if changes.is_empty() && expected == *next.catalog() {
            let before = std::mem::replace(&mut self.current, next.clone());
            self.origins = origins;
            return Ok(Update {
                previous,
                revision: next.revision(),
                notification: None,
                buffer: self.buffer,
                before,
                after: next.clone(),
            });
        }
        if changes.len() != 1 {
            return Err(mismatch("expected one buffer update"));
        }
        let (id, old, new) = &changes[0];
        expected
            .update(*id, new.clone())
            .map_err(|_| mismatch("invalid update"))?;
        if &expected != next.catalog() {
            return Err(mismatch("update and snapshot differ"));
        }
        let notification = if !self.closed && *id == self.buffer {
            let span = ByteSpan::validate_in(old, 0, old.len_bytes())
                .map_err(|_| mismatch("invalid span"))?;
            let patch = Patch::new(vec![Splice::new(span, Text::from_str(new.as_str()))])
                .map_err(|_| mismatch("invalid patch"))?;
            let intent =
                EditIntent::for_actor(ActorId::new(0), Nonce::new(previous.raw()), previous);
            let txn = Transaction::new(intent, vec![(*id, patch)])
                .map_err(|_| mismatch("invalid transaction"))?;
            let receipt =
                CommitReceipt::new(intent, previous).map_err(|_| mismatch("revision overflow"))?;
            let base = BufferSnapshot {
                buffer: *id,
                revision: previous,
                text: old.clone(),
            };
            Some(Notification::Change(delta::emit_receipt(
                &receipt, &txn, &base,
            )?))
        } else {
            None
        };
        let before = std::mem::replace(&mut self.current, next.clone());
        self.origins = origins;
        Ok(Update {
            previous,
            revision: next.revision(),
            notification,
            buffer: self.buffer,
            before,
            after: next.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::{registry::ActorRegistry, role::ActorRole, sequencer::Sequencer},
        edit::{
            ids::{ActorId, Nonce},
            intent::EditIntent,
            patch::Patch,
            splice::Splice,
        },
        test_support::{assert_law, texts},
        text::{span::ByteSpan, utf8::Text},
        workspace::{
            buffer_lifecycle::{BufferLifecycle, ClosePolicy},
            catalog::BufferCatalog,
            events::submit_with_event,
        },
    };

    fn edit(sequencer: &mut Sequencer, id: BufferId, value: &str, nonce: u64) -> WorkspaceEvent {
        let before = sequencer.snapshot();
        let text = before.catalog().get(id).unwrap();
        let span = ByteSpan::validate_in(text, 0, text.len_bytes()).unwrap();
        let transaction = Transaction::new(
            EditIntent::for_actor(ActorId::new(1), Nonce::new(nonce), before.revision()),
            vec![(
                id,
                Patch::new(vec![Splice::new(span, Text::from_str(value))]).unwrap(),
            )],
        )
        .unwrap();
        submit_with_event(sequencer, transaction).1.unwrap()
    }

    fn assert_projections(
        projections: &InitialSnapshots,
        snapshot: &WorkspaceSnapshot,
        id: BufferId,
    ) {
        assert_eq!(
            projections.emacs(),
            Ok(&snapshot::encode_emacs(snapshot, id).unwrap())
        );
        assert_eq!(
            projections.neovim(),
            Ok(&snapshot::encode_neovim(snapshot, id).unwrap())
        );
        assert_eq!(
            projections.agent(),
            Ok(&snapshot::encode_agent(snapshot, id).unwrap())
        );
    }

    #[test]
    fn saved_workspace_event_survives_snapshot_pruning() {
        let id = BufferId::new(1);
        let mut catalog = BufferCatalog::new();
        catalog.register(id, Text::from_str("before")).unwrap();
        let mut actors = ActorRegistry::new();
        actors.register(ActorId::new(1), ActorRole::Human).unwrap();
        let mut sequencer = Sequencer::new(catalog, actors);
        let (mut stream, mut projections) =
            Subscription::subscribe(&sequencer.snapshot(), id).unwrap();
        let event = edit(&mut sequencer, id, "界😀", 1);
        let saved_snapshot = sequencer.snapshot();
        edit(&mut sequencer, id, "later", 2);
        sequencer.prune_before(sequencer.revision());
        assert!(sequencer
            .snapshots()
            .get(saved_snapshot.revision())
            .is_none());
        let before = stream.clone();
        assert!(stream
            .apply_workspace_event(&event, &sequencer.snapshot())
            .is_err());
        assert_eq!(stream, before);
        let update = stream
            .apply_workspace_event(&event, &saved_snapshot)
            .unwrap();
        assert!(matches!(
            update.notification(),
            Some(Notification::Change(_))
        ));
        update.apply(&mut projections).unwrap();
        assert_projections(&projections, &saved_snapshot, id);
    }

    #[test]
    fn saved_events_share_lifecycle_origin_through_close() {
        let target = BufferId::new(1);
        let other = BufferId::new(2);
        let mut life = BufferLifecycle::new();
        life.open(target, Text::from_str("start")).unwrap();
        life.open(other, Text::from_str("other")).unwrap();
        let initial = life.snapshot();
        let (mut stream, mut projections) = Subscription::subscribe(&initial, target).unwrap();
        let mut actors = ActorRegistry::new();
        actors.register(ActorId::new(1), ActorRole::Human).unwrap();
        let mut sequencer = Sequencer::new(initial.catalog().clone(), actors);
        edit(&mut sequencer, target, "temporary", 0);
        edit(&mut sequencer, target, "start", 1);
        let edit_event = edit(&mut sequencer, target, "new界😀", 2);
        life.update(target, Text::from_str("new界😀")).unwrap();
        let edited = life.snapshot();
        life.close(target, ClosePolicy::Discard).unwrap();
        let close_event = *life.events().last().unwrap();
        let closed = life.snapshot();
        edit(&mut sequencer, target, "advance", 3);
        let other_event = edit(&mut sequencer, other, "other edited", 4);
        life.update(other, Text::from_str("other edited")).unwrap();
        let other_edited = life.snapshot();
        life.update(other, Text::from_str("other again")).unwrap();
        let other_updated = life.snapshot();
        life.close(other, ClosePolicy::Discard).unwrap();
        let other_close = *life.events().last().unwrap();
        let final_snapshot = life.snapshot();

        let before = stream.clone();
        assert!(stream.apply_workspace_event(&edit_event, &closed).is_err());
        assert!(stream.apply_lifecycle_event(&close_event, &closed).is_err());
        assert_eq!(stream, before);
        let update = stream.apply_workspace_event(&edit_event, &edited).unwrap();
        assert!(matches!(
            update.notification(),
            Some(Notification::Change(_))
        ));
        update.apply(&mut projections).unwrap();
        assert_projections(&projections, &edited, target);
        let before = stream.clone();
        assert!(stream
            .apply_lifecycle_event(&close_event, &other_edited)
            .is_err());
        let wrong_close = LifecycleEvent {
            buffer: other,
            ..close_event
        };
        assert!(stream.apply_lifecycle_event(&wrong_close, &closed).is_err());
        assert!(stream.apply_lifecycle_update(&closed).is_err());
        assert_eq!(stream, before);
        let update = stream.apply_lifecycle_event(&close_event, &closed).unwrap();
        assert_eq!(
            update.notification(),
            Some(&Notification::Closed {
                buffer: target,
                revision: closed.revision()
            })
        );
        update.apply(&mut projections).unwrap();
        assert!(projections.emacs().is_err());
        assert!(projections.neovim().is_err());
        assert!(projections.agent().is_err());
        assert!(stream.is_closed());
        let before = stream.clone();
        assert!(stream.apply_lifecycle_event(&close_event, &closed).is_err());
        assert!(stream.apply_lifecycle_update(&other_updated).is_err());
        assert_eq!(stream, before);
        for update in [
            stream
                .apply_workspace_event(&other_event, &other_edited)
                .unwrap(),
            stream.apply_lifecycle_update(&other_updated).unwrap(),
            stream
                .apply_lifecycle_event(&other_close, &final_snapshot)
                .unwrap(),
        ] {
            assert_eq!(update.notification(), None);
            update.apply(&mut projections).unwrap();
            assert!(projections.emacs().is_err());
            assert!(projections.neovim().is_err());
            assert!(projections.agent().is_err());
        }
    }

    #[test]
    fn generated_streams_replay_and_reject_atomically() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |cases| {
            for source in cases {
                let target = BufferId::new(1);
                let other = BufferId::new(2);
                let mut catalog = BufferCatalog::new();
                catalog.register(target, Text::from_str(source)).unwrap();
                catalog.register(other, Text::from_str("other")).unwrap();
                let mut actors = ActorRegistry::new();
                actors.register(ActorId::new(1), ActorRole::Human).unwrap();
                let mut sequencer = Sequencer::new(catalog, actors);
                let (mut stream, initial) =
                    Subscription::subscribe(&sequencer.snapshot(), target).unwrap();
                let mut projections = initial;
                for nonce in 0..6 {
                    let before = sequencer.snapshot();
                    let id = if nonce % 2 == 0 { other } else { target };
                    let text = before.catalog().get(id).unwrap();
                    let splice = Splice::new(
                        ByteSpan::validate_in(text, 0, 0).unwrap(),
                        Text::from_str(if nonce % 3 == 0 { "界\n" } else { "😀" }),
                    );
                    let mut splices = vec![splice];
                    if id == target && !text.as_str().is_empty() {
                        splices.push(Splice::new(
                            ByteSpan::validate_in(text, text.len_bytes(), text.len_bytes())
                                .unwrap(),
                            Text::from_str("z"),
                        ));
                    }
                    let transaction = Transaction::new(
                        EditIntent::for_actor(
                            ActorId::new(1),
                            Nonce::new(nonce),
                            before.revision(),
                        ),
                        vec![(id, Patch::new(splices).unwrap())],
                    )
                    .unwrap();
                    let (_, event) = submit_with_event(&mut sequencer, transaction);
                    event.unwrap();
                    let after = sequencer.snapshot();
                    let saved = stream.clone();
                    assert!(stream.workspace_event(&sequencer, &before).is_err());
                    assert_eq!(stream, saved);
                    let update = stream.workspace_event(&sequencer, &after).unwrap();
                    if id == other {
                        assert_eq!(update.notification, None);
                    } else {
                        assert!(matches!(update.notification, Some(Notification::Change(_))));
                    }
                    update.apply(&mut projections).unwrap();
                    let direct = snapshot::encode_emacs(&after, target).unwrap();
                    assert_eq!(projections.emacs.text, direct.text);
                    assert_eq!(
                        snapshot::decode_neovim(&projections.neovim)
                            .unwrap()
                            .text
                            .as_str(),
                        direct.text
                    );
                    assert_eq!(projections.agent.text, direct.text);
                    {
                        assert_eq!(projections.emacs, direct);
                        assert_eq!(
                            projections.neovim,
                            snapshot::encode_neovim(&after, target).unwrap()
                        );
                        assert_eq!(
                            projections.agent,
                            snapshot::encode_agent(&after, target).unwrap()
                        );
                    }
                    let saved = stream.clone();
                    assert!(stream.workspace_event(&sequencer, &after).is_err());
                    assert_eq!(stream, saved);
                }
            }
        });
    }

    #[test]
    fn lifecycle_closes_once_and_rejects_mismatches() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |cases| {
            for value in cases {
                let id = BufferId::new(1);
                let other = BufferId::new(2);
                let mut life = BufferLifecycle::new();
                life.open(id, Text::from_str(value)).unwrap();
                let initial = life.snapshot();
                assert!(Subscription::subscribe(&initial, other).is_err());
                let (mut stream, mut projections) = Subscription::subscribe(&initial, id).unwrap();
                life.open(other, Text::empty()).unwrap();
                let next = life.snapshot();
                let mut alien = BufferLifecycle::new();
                alien.open(id, Text::from_str(value)).unwrap();
                alien.open(other, Text::empty()).unwrap();
                let saved = stream.clone();
                assert!(stream.lifecycle_event(&alien).is_err());
                assert_eq!(stream, saved);
                let update = stream.lifecycle_event(&life).unwrap();
                assert_eq!(update.notification, None);
                update.apply(&mut projections).unwrap();
                assert_eq!(stream.current, next);
                let (_, mut other_projection) = Subscription::subscribe(&next, other).unwrap();
                let (_, mut alien_projection) =
                    Subscription::subscribe(&alien.snapshot(), id).unwrap();
                life.update(other, Text::empty()).unwrap();
                let silent = stream.lifecycle_update(&life).unwrap();
                assert_eq!(silent.notification, None);
                let other_before = other_projection.clone();
                let alien_before = alien_projection.clone();
                assert!(silent.apply(&mut other_projection).is_err());
                assert_eq!(other_projection, other_before);
                assert!(silent.apply(&mut alien_projection).is_err());
                assert_eq!(alien_projection, alien_before);
                silent.apply(&mut projections).unwrap();
                life.update(id, Text::from_str("編集😀")).unwrap();
                let update = stream.lifecycle_update(&life).unwrap();
                assert!(matches!(update.notification, Some(Notification::Change(_))));
                update.apply(&mut projections).unwrap();
                assert_eq!(
                    projections.emacs,
                    snapshot::encode_emacs(&life.snapshot(), id).unwrap()
                );
                assert_eq!(
                    projections.neovim,
                    snapshot::encode_neovim(&life.snapshot(), id).unwrap()
                );
                assert_eq!(
                    projections.agent,
                    snapshot::encode_agent(&life.snapshot(), id).unwrap()
                );
                life.close(id, ClosePolicy::Discard).unwrap();
                let closed = life.snapshot();
                alien.update(other, Text::empty()).unwrap();
                alien.update(id, Text::from_str("編集😀")).unwrap();
                alien.close(id, ClosePolicy::Discard).unwrap();
                let saved = stream.clone();
                assert!(stream.lifecycle_event(&alien).is_err());
                assert_eq!(stream, saved);
                let update = stream.lifecycle_event(&life).unwrap();
                assert_eq!(
                    update.notification,
                    Some(Notification::Closed {
                        buffer: id,
                        revision: closed.revision()
                    })
                );
                let debug = format!("{update:?}");
                assert!(!debug.contains("編集😀"));
                update.apply(&mut projections).unwrap();
                assert!(!format!("{projections:?}").contains("編集😀"));
                assert_eq!(
                    projections.emacs().unwrap_err().code(),
                    snapshot::encode_emacs(&closed, id).unwrap_err().code()
                );
                assert_eq!(
                    projections.neovim().unwrap_err().code(),
                    snapshot::encode_neovim(&closed, id).unwrap_err().code()
                );
                assert_eq!(
                    projections.agent().unwrap_err().code(),
                    snapshot::encode_agent(&closed, id).unwrap_err().code()
                );
                assert!(stream.is_closed());
                assert!(Subscription::subscribe(&closed, id).is_err());
                let saved = stream.clone();
                assert!(stream.lifecycle_event(&life).is_err());
                assert_eq!(stream, saved);
                life.close(other, ClosePolicy::Discard).unwrap();
                assert_eq!(stream.lifecycle_event(&life).unwrap().notification, None);
            }
        });
    }

    #[test]
    fn direct_removal_and_unchanged_target() {
        use crate::editor::workspace::snapshots::VersionedWorkspace;
        let id = BufferId::new(1);
        let other = BufferId::new(2);
        let mut catalog = BufferCatalog::new();
        catalog.register(id, Text::from_str("界")).unwrap();
        catalog.register(other, Text::from_str("a")).unwrap();
        let mut workspace = VersionedWorkspace::new(catalog.clone());
        let (mut stream, _) = Subscription::subscribe(&workspace.capture(), id).unwrap();
        workspace.remove(other).unwrap();
        assert_eq!(
            stream.removal(&workspace.capture()).unwrap().notification,
            None
        );
        workspace.remove(id).unwrap();
        let next = workspace.capture();
        assert_eq!(
            stream.removal(&next).unwrap().notification,
            Some(Notification::Closed {
                buffer: id,
                revision: next.revision()
            })
        );
        let saved = stream.clone();
        assert!(stream.removal(&next).is_err());
        assert_eq!(stream, saved);

        let mut actors = ActorRegistry::new();
        actors.register(ActorId::new(1), ActorRole::Human).unwrap();
        let mut sequencer = Sequencer::new(catalog.clone(), actors.clone());
        let mut foreign = Sequencer::new(catalog, actors);
        let (mut stream, _) = Subscription::subscribe(&sequencer.snapshot(), id).unwrap();
        let span = ByteSpan::validate_in(&Text::from_str("界"), 0, 3).unwrap();
        let other_span = ByteSpan::validate_in(&Text::from_str("a"), 0, 1).unwrap();
        let txn = Transaction::new(
            EditIntent::for_actor(ActorId::new(1), Nonce::new(1), Revision::ZERO),
            vec![
                (
                    id,
                    Patch::new(vec![Splice::new(span, Text::from_str("界"))]).unwrap(),
                ),
                (
                    other,
                    Patch::new(vec![Splice::new(other_span, Text::from_str("b"))]).unwrap(),
                ),
            ],
        )
        .unwrap();
        submit_with_event(&mut sequencer, txn.clone());
        submit_with_event(&mut foreign, txn);
        let saved = stream.clone();
        assert!(stream
            .workspace_event(&foreign, &sequencer.snapshot())
            .is_err());
        assert_eq!(stream, saved);
        assert_eq!(
            stream
                .workspace_event(&sequencer, &sequencer.snapshot())
                .unwrap()
                .notification,
            None
        );
    }
}
