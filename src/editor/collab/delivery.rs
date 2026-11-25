//! Verified, ordered delivery over a bounded retained history.

use std::{fmt, sync::Arc};

use crate::editor::{
    collab::{
        proposal::RequestId, receipt::CommitReceipt, retention::RetainedHistory,
        sequencer::Sequencer,
    },
    edit::transaction::Transaction,
    workspace::{
        catalog::BufferCatalog,
        events::WorkspaceEvent,
        snapshots::{SnapshotStore, WorkspaceSnapshot},
        version::Revision,
    },
};

/// A rejected delivery operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryError {
    /// The requested revision is beyond the published tip.
    FutureRevision { revision: Revision, tip: Revision },
    /// The requested revision has been compacted away.
    BaseExpired { revision: Revision, floor: Revision },
    /// An event does not follow the previous revision.
    NonContiguous {
        expected: Revision,
        previous: Revision,
    },
    /// The event, its publication evidence, or its workspace is inconsistent.
    InvalidEvent,
}

impl fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FutureRevision { revision, tip } => {
                write!(f, "revision {revision} is beyond tip {tip}")
            }
            Self::BaseExpired { revision, floor } => {
                write!(f, "revision {revision} is below floor {floor}")
            }
            Self::NonContiguous { expected, previous } => {
                write!(f, "expected previous revision {expected}, got {previous}")
            }
            Self::InvalidEvent => f.write_str("event does not match its published transition"),
        }
    }
}
impl std::error::Error for DeliveryError {}

/// An immutable published event, bound to its exact before and after captures.
///
/// Only a delivery log can construct one, after confirming its receipt and
/// transaction against the retained publication record. Cloning preserves
/// that evidence; no mutable access to the payload is exposed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryEvent {
    receipt: CommitReceipt,
    forward: Transaction,
    published: Arc<Transaction>,
    workspace_event: WorkspaceEvent,
    before: WorkspaceSnapshot,
    after: WorkspaceSnapshot,
}

impl DeliveryEvent {
    pub fn receipt(&self) -> CommitReceipt {
        self.receipt
    }
    pub fn transaction(&self) -> &Transaction {
        &self.forward
    }
    /// Returns the event published by the sequencer.
    pub fn workspace_event(&self) -> &WorkspaceEvent {
        &self.workspace_event
    }
    pub fn before(&self) -> &WorkspaceSnapshot {
        &self.before
    }
    pub fn after(&self) -> &WorkspaceSnapshot {
        &self.after
    }
}

/// Incremental events, or a complete current snapshot after expiration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatchUp {
    Events(Vec<DeliveryEvent>),
    Snapshot(WorkspaceSnapshot),
}

/// Replays a delivered sequence without the sender's log and without mutation.
pub fn replay(
    base: &WorkspaceSnapshot,
    events: &[DeliveryEvent],
) -> Result<(BufferCatalog, Revision), DeliveryError> {
    let mut current = base;
    for event in events {
        let receipt = event.receipt;
        if receipt.previous() != current.revision()
            || current.revision().next().ok() != Some(receipt.committed())
        {
            return Err(DeliveryError::NonContiguous {
                expected: current.revision(),
                previous: receipt.previous(),
            });
        }
        if &event.before != current
            || event.after.revision() != receipt.committed()
            || event.forward.intent() != receipt.intent().with_base(receipt.previous())
            || event.forward != *event.published
            || event.workspace_event.receipt() != receipt
            || event.workspace_event.patches() != event.forward.patches()
        {
            return Err(DeliveryError::InvalidEvent);
        }
        let mut origin = SnapshotStore::new();
        origin.record(current.clone()).expect("empty store");
        origin
            .record(event.after.clone())
            .map_err(|_| DeliveryError::InvalidEvent)?;
        let result = event
            .forward
            .apply_with_inverse(current)
            .map_err(|_| DeliveryError::InvalidEvent)?;
        if result.catalog() != event.after.catalog() {
            return Err(DeliveryError::InvalidEvent);
        }
        current = &event.after;
    }
    Ok((current.catalog().clone(), current.revision()))
}

/// A contiguous stream and retained captures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryLog {
    floor: Revision,
    snapshots: SnapshotStore,
    events: Vec<DeliveryEvent>,
}

impl DeliveryLog {
    pub fn new(base: WorkspaceSnapshot) -> Self {
        let floor = base.revision();
        let mut snapshots = SnapshotStore::new();
        snapshots.record(base).expect("empty store");
        Self {
            floor,
            snapshots,
            events: Vec::new(),
        }
    }
    pub fn floor(&self) -> Revision {
        self.floor
    }
    pub fn tip(&self) -> Revision {
        self.tip_snapshot().revision()
    }
    pub fn base_snapshot(&self) -> &WorkspaceSnapshot {
        self.snapshots.get(self.floor).expect("retained floor")
    }
    pub fn tip_snapshot(&self) -> &WorkspaceSnapshot {
        self.snapshots.latest().expect("retained tip")
    }
    pub fn events(&self) -> &[DeliveryEvent] {
        &self.events
    }

    /// Records a published transition, checking its full identity against C12.
    ///
    /// The history's retry validation compares the request, original and
    /// applied transactions, receipt, snapshot content and snapshot origin.
    /// A caller must pass an already published record, not a fresh proposal.
    pub fn record(
        &mut self,
        history: &RetainedHistory,
        request: RequestId,
        original: Transaction,
        event: WorkspaceEvent,
        after: WorkspaceSnapshot,
    ) -> Result<(), DeliveryError> {
        let receipt = event.receipt();
        let forward = Transaction::new(
            receipt.intent().with_base(receipt.previous()),
            event.patches().to_vec(),
        )
        .map_err(|_| DeliveryError::InvalidEvent)?;
        let tip = self.tip();
        if receipt.previous() != tip || tip.next().ok() != Some(receipt.committed()) {
            return Err(DeliveryError::NonContiguous {
                expected: tip,
                previous: receipt.previous(),
            });
        }
        if history.receipt_by_request(request) != Some(receipt)
            || history.receipt_by_txn(receipt.txn()) != Some(receipt)
        {
            return Err(DeliveryError::InvalidEvent);
        }
        let mut checked = history.clone();
        checked
            .record(request, original, forward.clone(), receipt, after.clone())
            .map_err(|_| DeliveryError::InvalidEvent)?;
        let event = DeliveryEvent {
            receipt,
            published: Arc::new(forward.clone()),
            workspace_event: event,
            forward,
            before: self.tip_snapshot().clone(),
            after: after.clone(),
        };
        self.append(event)
    }

    /// Projects an event directly from the sequencer that published it.
    /// The event is obtained from the authority rather than supplied by a caller.
    pub fn record_workspace(
        &mut self,
        sequencer: &Sequencer,
        receipt: CommitReceipt,
    ) -> Result<(), DeliveryError> {
        let event =
            WorkspaceEvent::from_receipt(sequencer, receipt).ok_or(DeliveryError::InvalidEvent)?;
        let after = sequencer
            .snapshots()
            .get(receipt.committed())
            .ok_or(DeliveryError::InvalidEvent)?
            .clone();
        let forward = sequencer
            .history()
            .get_by_revision(receipt.committed())
            .ok_or(DeliveryError::InvalidEvent)?
            .forward()
            .clone();
        self.append(DeliveryEvent {
            receipt,
            published: Arc::new(forward.clone()),
            workspace_event: event,
            forward,
            before: self.tip_snapshot().clone(),
            after,
        })
    }

    fn append(&mut self, event: DeliveryEvent) -> Result<(), DeliveryError> {
        replay(self.tip_snapshot(), std::slice::from_ref(&event))?;
        let mut snapshots = self.snapshots.clone();
        snapshots
            .record(event.after.clone())
            .map_err(|_| DeliveryError::InvalidEvent)?;
        self.snapshots = snapshots;
        self.events.push(event);
        Ok(())
    }

    pub fn catch_up(&self, since: Revision) -> Result<CatchUp, DeliveryError> {
        if since > self.tip() {
            return Err(DeliveryError::FutureRevision {
                revision: since,
                tip: self.tip(),
            });
        }
        if since < self.floor {
            return Ok(CatchUp::Snapshot(self.tip_snapshot().clone()));
        }
        let offset = usize::try_from(since.steps_since(self.floor).expect("checked floor"))
            .expect("bounded by event count");
        Ok(CatchUp::Events(self.events[offset..].to_vec()))
    }
    pub fn compact(&mut self, floor: Revision) -> Result<(), DeliveryError> {
        if floor < self.floor {
            return Err(DeliveryError::BaseExpired {
                revision: floor,
                floor: self.floor,
            });
        }
        if floor > self.tip() {
            return Err(DeliveryError::FutureRevision {
                revision: floor,
                tip: self.tip(),
            });
        }
        let count = usize::try_from(floor.steps_since(self.floor).expect("checked floor"))
            .expect("bounded by event count");
        self.events.drain(..count);
        self.snapshots.prune_before(floor);
        self.floor = floor;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::{registry::ActorRegistry, role::ActorRole},
        edit::{
            ids::{ActorId, BufferId, Nonce},
            intent::EditIntent,
            patch::Patch,
            splice::Splice,
        },
        test_support::assert_law,
        text::{span::ByteSpan, utf8::Text},
        workspace::{events::submit_with_event, snapshots::VersionedWorkspace},
    };

    #[test]
    fn retained_delivery_laws() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| (0..8).map(|_| rng.next_u64()).collect::<Vec<_>>(),
            |choices| {
                let mut catalog = BufferCatalog::new();
                for id in 0..2 {
                    catalog
                        .register(BufferId::new(id), Text::from_str("abc"))
                        .unwrap();
                }
                let mut actors = ActorRegistry::new();
                actors.register(ActorId::new(1), ActorRole::Human).unwrap();
                let mut workspace = Sequencer::new(catalog, actors);
                let base = workspace.snapshot();
                let mut history = RetainedHistory::new(base.clone());
                let mut log = DeliveryLog::new(base.clone());
                let mut captures = vec![base.clone()];
                for (i, choice) in choices.iter().enumerate() {
                    let before = workspace.snapshot();
                    let mut patches = Vec::new();
                    for id in 0..2 {
                        let id = BufferId::new(id);
                        let old = before.catalog().get(id).unwrap();
                        let value = Text::from_str(&format!("{choice}-{i}"));
                        patches.push((
                            id,
                            Patch::new(vec![Splice::new(ByteSpan::full_of(old), value.clone())])
                                .unwrap(),
                        ));
                    }
                    let intent = EditIntent::for_actor(
                        ActorId::new(1),
                        Nonce::new(i as u64),
                        before.revision(),
                    );
                    let txn = Transaction::new(intent, patches).unwrap();
                    let (_, event) = submit_with_event(&mut workspace, txn.clone());
                    let event = event.unwrap();
                    let receipt = event.receipt();
                    let after = workspace.snapshot();
                    let request = RequestId::new(i as u64);
                    history
                        .record(request, txn.clone(), txn.clone(), receipt, after.clone())
                        .unwrap();
                    log.record(&history, request, txn, event, after.clone())
                        .unwrap();
                    captures.push(after);
                }
                let all = log.events().to_vec();
                let saved = log.clone();
                for bad in [
                    vec![all[1].clone()],
                    vec![all[0].clone(), all[0].clone()],
                    vec![all[1].clone(), all[0].clone()],
                ] {
                    assert!(replay(&base, &bad).is_err());
                    assert_eq!(log, saved);
                }
                let mut corrupt = all[0].clone();
                let altered_patch = Patch::new(vec![Splice::new(
                    ByteSpan::full_of(base.catalog().get(BufferId::new(0)).unwrap()),
                    Text::from_str("changed"),
                )])
                .unwrap();
                corrupt.forward = Transaction::new(
                    corrupt.forward.intent(),
                    vec![(BufferId::new(0), altered_patch)],
                )
                .unwrap();
                assert_eq!(replay(&base, &[corrupt]), Err(DeliveryError::InvalidEvent));
                let mut equivalent = all[0].clone();
                let source = base.catalog().get(BufferId::new(0)).unwrap();
                let no_change = Patch::new(vec![Splice::new(
                    ByteSpan::validate_in(source, 0, 1).unwrap(),
                    Text::from_str("a"),
                )])
                .unwrap();
                equivalent.forward = Transaction::new(
                    equivalent.forward.intent(),
                    vec![(BufferId::new(0), no_change)],
                )
                .unwrap();
                assert_eq!(
                    replay(&base, &[equivalent]),
                    Err(DeliveryError::InvalidEvent)
                );
                let mut foreign = VersionedWorkspace::new(base.catalog().clone());
                let foreign_base = foreign.capture();
                let mut foreign_history = RetainedHistory::new(foreign_base.clone());
                let original = all[0].forward.clone();
                let receipt = all[0].receipt;
                let foreign_catalog = original.apply_to_catalog(foreign_base.catalog()).unwrap();
                let updates = (0..2)
                    .map(|id| {
                        (
                            BufferId::new(id),
                            foreign_catalog.get(BufferId::new(id)).unwrap().clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                foreign.update_batch(updates).unwrap();
                let foreign_after = foreign.capture();
                foreign_history
                    .record(
                        RequestId::new(0),
                        original.clone(),
                        original.clone(),
                        receipt,
                        foreign_after.clone(),
                    )
                    .unwrap();
                let mut local_receiver = DeliveryLog::new(base.clone());
                let unchanged = local_receiver.clone();
                assert_eq!(
                    local_receiver.record(
                        &foreign_history,
                        RequestId::new(0),
                        original.clone(),
                        all[0].workspace_event.clone(),
                        captures[1].clone()
                    ),
                    Err(DeliveryError::InvalidEvent)
                );
                assert_eq!(local_receiver, unchanged);
                let mut other = DeliveryLog::new(foreign_base);
                other
                    .record(
                        &foreign_history,
                        RequestId::new(0),
                        original,
                        all[0].workspace_event.clone(),
                        foreign_after,
                    )
                    .unwrap();
                assert_eq!(
                    replay(&base, other.events()),
                    Err(DeliveryError::InvalidEvent)
                );
                for floor in 0..=choices.len() {
                    log.compact(Revision::new(floor as u64)).unwrap();
                    for capture in captures.iter().skip(floor) {
                        let CatchUp::Events(events) = log.catch_up(capture.revision()).unwrap()
                        else {
                            panic!("expected events")
                        };
                        assert_eq!(
                            replay(capture, &events).unwrap(),
                            (workspace.snapshot().catalog().clone(), workspace.revision())
                        );
                    }
                    if floor > 0 {
                        assert_eq!(
                            log.catch_up(Revision::ZERO),
                            Ok(CatchUp::Snapshot(workspace.snapshot()))
                        );
                    }
                    assert_eq!(log.catch_up(log.tip()), Ok(CatchUp::Events(vec![])));
                    assert!(matches!(
                        log.catch_up(log.tip().next().unwrap()),
                        Err(DeliveryError::FutureRevision { .. })
                    ));
                }
            },
        );
    }

    #[test]
    fn sequencer_workspace_events_are_delivered() {
        let id = BufferId::new(0);
        let mut catalog = BufferCatalog::new();
        catalog.register(id, Text::from_str("abc")).unwrap();
        let mut actors = ActorRegistry::new();
        actors.register(ActorId::new(1), ActorRole::Human).unwrap();
        let mut sequencer = Sequencer::new(catalog, actors);
        let base = sequencer.snapshot();
        let patch = Patch::new(vec![Splice::new(
            ByteSpan::full_of(base.catalog().get(id).unwrap()),
            Text::from_str("xyz"),
        )])
        .unwrap();
        let intent = EditIntent::for_actor(ActorId::new(1), Nonce::new(1), base.revision());
        let transaction = Transaction::new(intent, vec![(id, patch)]).unwrap();
        let (_, projected) = submit_with_event(&mut sequencer, transaction);
        let projected = projected.unwrap();
        let mut log = DeliveryLog::new(base.clone());
        log.record_workspace(&sequencer, projected.receipt())
            .unwrap();
        let CatchUp::Events(events) = log.catch_up(base.revision()).unwrap() else {
            panic!("expected events")
        };
        assert_eq!(events[0].workspace_event(), &projected);
        assert_eq!(
            replay(&base, &events).unwrap(),
            (sequencer.snapshot().catalog().clone(), sequencer.revision())
        );
    }

    #[test]
    fn both_delivery_paths_preserve_published_events_after_compaction() {
        let id = BufferId::new(0);
        let mut catalog = BufferCatalog::new();
        catalog.register(id, Text::from_str("start")).unwrap();
        let mut actors = ActorRegistry::new();
        actors.register(ActorId::new(1), ActorRole::Human).unwrap();
        let mut sequencer = Sequencer::new(catalog, actors);
        let base = sequencer.snapshot();
        let mut history = RetainedHistory::new(base.clone());
        let mut retained_log = DeliveryLog::new(base.clone());
        let mut sequencer_log = DeliveryLog::new(base.clone());
        let mut captures = vec![base];
        let mut published = Vec::new();
        for (i, value) in ["first", "second", "third"].iter().enumerate() {
            let before = sequencer.snapshot();
            let patch = Patch::new(vec![Splice::new(
                ByteSpan::full_of(before.catalog().get(id).unwrap()),
                Text::from_str(value),
            )])
            .unwrap();
            let intent =
                EditIntent::for_actor(ActorId::new(1), Nonce::new(i as u64), before.revision());
            let original = Transaction::new(intent, vec![(id, patch)]).unwrap();
            let (_, event) = submit_with_event(&mut sequencer, original.clone());
            let event = event.unwrap();
            let receipt = event.receipt();
            let after = sequencer.snapshot();
            let request = RequestId::new(i as u64);
            history
                .record(
                    request,
                    original.clone(),
                    original.clone(),
                    receipt,
                    after.clone(),
                )
                .unwrap();
            retained_log
                .record(&history, request, original, event.clone(), after.clone())
                .unwrap();
            sequencer_log.record_workspace(&sequencer, receipt).unwrap();
            captures.push(after);
            published.push(event);
        }
        for (floor, base) in captures.iter().enumerate() {
            retained_log.compact(base.revision()).unwrap();
            sequencer_log.compact(base.revision()).unwrap();
            for (offset, capture) in captures.iter().enumerate().skip(floor) {
                let CatchUp::Events(retained) = retained_log.catch_up(capture.revision()).unwrap()
                else {
                    panic!("expected retained events")
                };
                let CatchUp::Events(projected) =
                    sequencer_log.catch_up(capture.revision()).unwrap()
                else {
                    panic!("expected sequencer events")
                };
                assert_eq!(retained.len(), published.len() - offset);
                assert_eq!(projected.len(), retained.len());
                for ((left, right), expected) in
                    retained.iter().zip(&projected).zip(&published[offset..])
                {
                    assert_eq!(left.workspace_event(), expected);
                    assert_eq!(right.workspace_event(), expected);
                }
                let expected = (sequencer.snapshot().catalog().clone(), sequencer.revision());
                assert_eq!(replay(capture, &retained).unwrap(), expected);
                assert_eq!(replay(capture, &projected).unwrap(), expected);
            }
        }
    }
}
