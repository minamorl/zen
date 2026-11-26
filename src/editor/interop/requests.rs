//! Transport-neutral, validated requests over a workspace snapshot.
//!
//! All envelopes use the same normalization path, resolving coordinates and
//! edit order against the supplied revision before a request exists.

use super::{
    batch::{decode_batch, ExternalEdit},
    error::{BoundaryError, ErrorCode},
    region::OrientedRegion,
    resync::{Resync, ResyncError},
    snapshot::AgentSnapshot,
};
use crate::editor::{
    collab::{
        access::{AccessError, AccessPolicy, Capability},
        lifecycle,
        proposal::{Proposal, RequestId},
        registry::ActorRegistry,
    },
    edit::{
        ids::{ActorId, BufferId, Nonce},
        intent::EditIntent,
        patch::Patch,
        transaction::Transaction,
    },
    text::utf8::Text,
    workspace::{
        buffer_lifecycle::ClosePolicy,
        selections::Selection,
        snapshots::{SnapshotError, SnapshotStore, WorkspaceSnapshot},
        version::Revision,
        view::BufferView,
    },
};

/// A target as supplied by a transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Buffer(BufferId),
    Name(String),
}

/// One replacement expressed in external coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replacement {
    pub region: OrientedRegion,
    pub text: Text,
}

/// An edit and its explicit buffer target in a multi-buffer batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchEdit {
    pub buffer: BufferId,
    pub replacement: Replacement,
}

/// An operation supplied by a transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputOperation {
    Open(Text),
    Close(ClosePolicy),
    Read,
    Edit(Replacement),
    Batch(Vec<Replacement>),
    MultiBatch(Vec<BatchEdit>),
    Region(OrientedRegion),
    Resync,
}

/// Untrusted fields; missing values are rejected by normalization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub actor: Option<ActorId>,
    pub revision: Option<Revision>,
    pub target: Option<Target>,
    pub operation: InputOperation,
}

/// Envelope variants do not change the meaning of their contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Envelope {
    Command(Input),
    BufferOperation(Input),
    AgentProposal(Input),
}

/// An operation with coordinates resolved to the base snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    Open(Text),
    Close(ClosePolicy),
    Read,
    Edit(Vec<(BufferId, Patch)>),
    Region(Selection),
    Resync,
}

/// Canonical request. Its fields can only be produced by validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    origin_snapshot: WorkspaceSnapshot,
    actor: ActorId,
    revision: Revision,
    target: Target,
    operation: Operation,
}

/// Authorization or recovery failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestError {
    Access(AccessError),
    Recovery(ResyncError),
    Lifecycle(lifecycle::LifecycleError),
    DifferentWorkspace,
    InvalidOperation,
    /// Opening is authorized by the lifecycle authority after allocating an ID.
    OpenRequiresLifecycle,
    /// The observed capture does not match the request or operation.
    InvalidRecovery,
}

/// Recovery exposes only the authorized buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recovery {
    Live(AgentSnapshot),
    Closed {
        buffer: BufferId,
        revision: Revision,
    },
}

impl From<AccessError> for RequestError {
    fn from(value: AccessError) -> Self {
        Self::Access(value)
    }
}

fn invalid(code: ErrorCode, message: &str) -> BoundaryError {
    BoundaryError::new(code, message)
}

fn same_workspace(a: &WorkspaceSnapshot, b: &WorkspaceSnapshot) -> bool {
    let mut store = SnapshotStore::new();
    store.record(a.clone()).expect("empty snapshot store");
    !matches!(
        store.record(b.clone()),
        Err(SnapshotError::DifferentWorkspace)
    )
}

impl Envelope {
    /// Validate required fields and resolve the payload against `snapshot`.
    pub fn normalize(self, snapshot: &WorkspaceSnapshot) -> Result<Request, BoundaryError> {
        let input = match self {
            Self::Command(input) | Self::BufferOperation(input) | Self::AgentProposal(input) => {
                input
            }
        };
        let actor = input
            .actor
            .ok_or_else(|| invalid(ErrorCode::UnknownActor, "missing actor"))?;
        let revision = input
            .revision
            .ok_or_else(|| invalid(ErrorCode::RevisionMismatch, "missing revision"))?;
        if revision != snapshot.revision() {
            return Err(invalid(
                ErrorCode::RevisionMismatch,
                "revision does not match snapshot",
            ));
        }
        let target = input
            .target
            .ok_or_else(|| invalid(ErrorCode::InvalidCoordinate, "missing target"))?;
        let operation = match (input.operation, &target) {
            (InputOperation::Open(text), Target::Name(name))
                if !name.trim().is_empty() && !name.contains('\0') =>
            {
                Operation::Open(text)
            }
            (InputOperation::Open(_), _) => {
                return Err(invalid(ErrorCode::InvalidCoordinate, "invalid open target"))
            }
            (operation, Target::Buffer(buffer)) => {
                let view = BufferView::full(snapshot.catalog(), *buffer).map_err(|_| {
                    invalid(
                        if snapshot.catalog().is_tombstoned(*buffer) {
                            ErrorCode::BufferClosed
                        } else {
                            ErrorCode::UnknownBuffer
                        },
                        "target is not live",
                    )
                })?;
                match operation {
                    InputOperation::Close(policy) => Operation::Close(policy),
                    InputOperation::Read => Operation::Read,
                    InputOperation::Resync => Operation::Resync,
                    InputOperation::Region(region) => Operation::Region(region.to_selection(
                        snapshot.catalog(),
                        &view,
                        *buffer,
                    )?),
                    InputOperation::Edit(edit) => compile_edits(
                        snapshot,
                        actor,
                        revision,
                        vec![BatchEdit {
                            buffer: *buffer,
                            replacement: edit,
                        }],
                    )?,
                    InputOperation::Batch(edits) => compile_edits(
                        snapshot,
                        actor,
                        revision,
                        edits
                            .into_iter()
                            .map(|replacement| BatchEdit {
                                buffer: *buffer,
                                replacement,
                            })
                            .collect(),
                    )?,
                    InputOperation::MultiBatch(edits) => {
                        if !edits.is_empty() && !edits.iter().any(|edit| edit.buffer == *buffer) {
                            return Err(invalid(
                                ErrorCode::InvalidCoordinate,
                                "batch does not include target",
                            ));
                        }
                        compile_edits(snapshot, actor, revision, edits)?
                    }
                    InputOperation::Open(_) => unreachable!(),
                }
            }
            (_, _) => {
                return Err(invalid(
                    ErrorCode::InvalidCoordinate,
                    "expected buffer target",
                ))
            }
        };
        Ok(Request {
            origin_snapshot: snapshot.clone(),
            actor,
            revision,
            target,
            operation,
        })
    }
}

fn compile_edits(
    snapshot: &WorkspaceSnapshot,
    actor: ActorId,
    revision: Revision,
    edits: Vec<BatchEdit>,
) -> Result<Operation, BoundaryError> {
    let intent = EditIntent::for_actor(actor, Nonce::new(0), revision);
    let external: Vec<_> = edits
        .into_iter()
        .map(|edit| ExternalEdit {
            buffer: edit.buffer,
            intent,
            region: edit.replacement.region,
            replacement: edit.replacement.text,
        })
        .collect();
    let transaction = decode_batch(snapshot, intent, &external)?;
    Ok(Operation::Edit(transaction.patches().to_vec()))
}

impl Request {
    pub(crate) fn rebind_origin(mut self, origin_snapshot: WorkspaceSnapshot) -> Self {
        self.origin_snapshot = origin_snapshot;
        self
    }

    pub fn actor(&self) -> ActorId {
        self.actor
    }
    pub fn revision(&self) -> Revision {
        self.revision
    }
    pub fn target(&self) -> &Target {
        &self.target
    }
    pub fn operation(&self) -> &Operation {
        &self.operation
    }

    /// Required capability. Use `authorize` to handle closed-buffer resync.
    pub fn capability(&self) -> Capability {
        match self.operation {
            Operation::Read | Operation::Region(_) | Operation::Resync => Capability::Read,
            _ => Capability::Commit,
        }
    }

    /// Authorize against current registry, policy and catalog before execution.
    /// A closed-buffer resync still requires an active actor and a current Read
    /// grant; only the usual live-buffer requirement is relaxed. Authorization
    /// must be coordinated with execution as policy and workspace can change.
    pub fn authorize(
        &self,
        policy: &AccessPolicy,
        registry: &ActorRegistry,
        snapshot: &WorkspaceSnapshot,
    ) -> Result<(), RequestError> {
        if !same_workspace(&self.origin_snapshot, snapshot) {
            return Err(RequestError::DifferentWorkspace);
        }
        let catalog = snapshot.catalog();
        match self.target {
            Target::Buffer(buffer)
                if matches!(self.operation, Operation::Resync) && catalog.is_tombstoned(buffer) =>
            {
                policy.authorize(registry, catalog, self.actor, Capability::Read, [])?;
                if !policy.is_granted(self.actor, buffer, Capability::Read) {
                    return Err(AccessError::Denied {
                        actor: self.actor,
                        buffer,
                        capability: Capability::Read,
                    }
                    .into());
                }
                Ok(())
            }
            Target::Buffer(buffer) => {
                let targets: Vec<BufferId> = match &self.operation {
                    Operation::Edit(patches) if !patches.is_empty() => {
                        patches.iter().map(|(id, _)| *id).collect()
                    }
                    _ => vec![buffer],
                };
                Ok(policy.authorize(registry, catalog, self.actor, self.capability(), targets)?)
            }
            Target::Name(_) => Err(RequestError::OpenRequiresLifecycle),
        }
    }

    /// Authorize and recover from the request's observed capture to current state.
    /// Both captures must belong to the request's originating workspace.
    pub fn recover(
        &self,
        policy: &AccessPolicy,
        registry: &ActorRegistry,
        observed: &WorkspaceSnapshot,
        current: &WorkspaceSnapshot,
    ) -> Result<Recovery, RequestError> {
        if !same_workspace(&self.origin_snapshot, observed)
            || !same_workspace(&self.origin_snapshot, current)
        {
            return Err(RequestError::DifferentWorkspace);
        }
        let buffer = match (&self.target, &self.operation) {
            (Target::Buffer(buffer), Operation::Resync) if observed.revision() == self.revision => {
                *buffer
            }
            _ => return Err(RequestError::InvalidRecovery),
        };
        self.authorize(policy, registry, current)?;
        let mut client = Resync::new(observed, buffer).map_err(RequestError::Recovery)?;
        client.resync(current).map_err(RequestError::Recovery)?;
        if client.is_closed() {
            Ok(Recovery::Closed {
                buffer,
                revision: client.revision(),
            })
        } else {
            Ok(Recovery::Live(client.agent().map_err(|error| {
                RequestError::Recovery(ResyncError::Projection(error))
            })?))
        }
    }

    /// Construct a lifecycle operation with a caller-supplied transaction nonce.
    pub fn lifecycle_operation(&self, nonce: Nonce) -> Option<(EditIntent, lifecycle::Operation)> {
        let intent = EditIntent::for_actor(self.actor, nonce, self.revision);
        let operation = match (&self.target, &self.operation) {
            (Target::Name(name), Operation::Open(text)) => lifecycle::Operation::Open {
                name: name.clone(),
                text: text.clone(),
            },
            (Target::Buffer(buffer), Operation::Close(policy)) => lifecycle::Operation::Close {
                buffer: *buffer,
                policy: *policy,
            },
            (Target::Buffer(_), Operation::Edit(patches)) => {
                lifecycle::Operation::Edit(Transaction::new(intent, patches.clone()).ok()?)
            }
            _ => return None,
        };
        Some((intent, operation))
    }

    /// Execute a lifecycle operation against its originating workspace.
    pub fn execute(
        &self,
        authority: &mut lifecycle::CollaborativeLifecycle,
        request: RequestId,
        nonce: Nonce,
    ) -> Result<crate::editor::collab::receipt::CommitReceipt, RequestError> {
        if !same_workspace(&self.origin_snapshot, &authority.snapshot()) {
            return Err(RequestError::DifferentWorkspace);
        }
        let (intent, operation) = self
            .lifecycle_operation(nonce)
            .ok_or(RequestError::InvalidOperation)?;
        authority
            .submit(request, intent, operation)
            .map_err(RequestError::Lifecycle)
    }

    /// Produce a reviewable proposal for an edit.
    pub fn proposal(&self, request: RequestId, nonce: Nonce) -> Option<Proposal> {
        match self.lifecycle_operation(nonce)?.1 {
            lifecycle::Operation::Edit(transaction) if !transaction.patches().is_empty() => {
                Some(Proposal::new(request, transaction))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        interop::{coordinates::CoordKind, region::EndpointMode},
        test_support::{assert_law, Rng},
        workspace::{catalog::BufferCatalog, snapshots::VersionedWorkspace},
    };

    fn fixture() -> WorkspaceSnapshot {
        let mut catalog = BufferCatalog::new();
        catalog
            .register(BufferId::new(1), Text::from_str("a界😀z"))
            .unwrap();
        VersionedWorkspace::new(catalog).capture()
    }

    fn input(snapshot: &WorkspaceSnapshot, operation: InputOperation) -> Input {
        Input {
            actor: Some(ActorId::new(7)),
            revision: Some(snapshot.revision()),
            target: Some(Target::Buffer(BufferId::new(1))),
            operation,
        }
    }

    #[test]
    fn foreign_workspace_is_rejected_at_each_boundary() {
        use crate::editor::collab::{lifecycle::CollaborativeLifecycle, role::ActorRole};
        let actor = ActorId::new(7);
        let buffer = BufferId::new(0);
        let mut registry = ActorRegistry::new();
        registry.register(actor, ActorRole::Human).unwrap();
        let mut policy = AccessPolicy::new();
        policy.grant_buffer(actor, buffer, Capability::Commit);
        policy.grant_buffer(actor, buffer, Capability::Read);
        let mut first = CollaborativeLifecycle::new(registry.clone(), policy.clone());
        let mut second = CollaborativeLifecycle::new(registry.clone(), policy.clone());
        for authority in [&mut first, &mut second] {
            let intent = EditIntent::for_actor(actor, Nonce::new(1), Revision::ZERO);
            authority
                .submit(
                    RequestId::new(1),
                    intent,
                    lifecycle::Operation::Open {
                        name: "file".into(),
                        text: Text::from_str("abc"),
                    },
                )
                .unwrap();
        }
        let origin = first.snapshot();
        let foreign = second.snapshot();
        assert_eq!(origin.revision(), foreign.revision());
        let view = BufferView::full(origin.catalog(), buffer).unwrap();
        let offsets = origin.catalog().get(buffer).unwrap().char_boundaries();
        let selection =
            Selection::new(origin.catalog(), &view, buffer, offsets[0], offsets[1]).unwrap();
        let region = OrientedRegion::from_selection(
            origin.catalog(),
            &view,
            selection,
            CoordKind::Byte,
            EndpointMode::Exclusive,
        )
        .unwrap();
        let mut raw = input(
            &origin,
            InputOperation::Edit(Replacement {
                region,
                text: Text::from_str("x"),
            }),
        );
        raw.target = Some(Target::Buffer(buffer));
        let request = Envelope::Command(raw).normalize(&origin).unwrap();
        assert_eq!(
            request.authorize(&policy, &registry, &foreign),
            Err(RequestError::DifferentWorkspace)
        );
        assert_eq!(
            request.execute(&mut second, RequestId::new(2), Nonce::new(2)),
            Err(RequestError::DifferentWorkspace)
        );
        assert_eq!(second.snapshot(), foreign);
        let mut raw = input(&origin, InputOperation::Resync);
        raw.target = Some(Target::Buffer(buffer));
        let request = Envelope::Command(raw).normalize(&origin).unwrap();
        for (observed, current) in [
            (&foreign, &origin),
            (&origin, &foreign),
            (&foreign, &foreign),
        ] {
            assert_eq!(
                request.recover(&policy, &registry, observed, current),
                Err(RequestError::DifferentWorkspace)
            );
        }
    }

    #[test]
    fn nonempty_batch_can_be_approved() {
        use crate::editor::collab::role::ActorRole;
        let snapshot = fixture();
        let buffer = BufferId::new(1);
        let actor = ActorId::new(7);
        let view = BufferView::full(snapshot.catalog(), buffer).unwrap();
        let text = snapshot.catalog().get(buffer).unwrap();
        let offsets = text.char_boundaries();
        let selection =
            Selection::new(snapshot.catalog(), &view, buffer, offsets[0], offsets[1]).unwrap();
        let region = OrientedRegion::from_selection(
            snapshot.catalog(),
            &view,
            selection,
            CoordKind::Byte,
            EndpointMode::Exclusive,
        )
        .unwrap();
        let request = Envelope::Command(input(
            &snapshot,
            InputOperation::Batch(vec![Replacement {
                region,
                text: Text::from_str("x"),
            }]),
        ))
        .normalize(&snapshot)
        .unwrap();
        let proposal = request.proposal(RequestId::new(1), Nonce::new(1)).unwrap();
        assert!(!proposal.transaction().patches().is_empty());
        let mut registry = ActorRegistry::new();
        registry.register(actor, ActorRole::Human).unwrap();
        let mut policy = AccessPolicy::new();
        policy.grant_buffer(actor, buffer, Capability::Review);
        assert!(proposal
            .approve(&snapshot, &registry, &policy, actor)
            .is_ok());
    }

    #[test]
    fn generated_invalid_fields_never_normalize() {
        use crate::editor::interop::{
            agent_position::AgentPosition, coordinates::BytePos, region::RegionCoord,
        };
        use crate::editor::workspace::selections::Direction;
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| rng.next_u64(),
            |value| {
                let snapshot = fixture();
                let invalid_region = OrientedRegion {
                    anchor: RegionCoord::Agent(AgentPosition::Byte(BytePos::new(2))),
                    head: RegionCoord::Agent(AgentPosition::Byte(BytePos::new(2))),
                    direction: Direction::Forward,
                    mode: EndpointMode::Exclusive,
                };
                let valid_view = BufferView::full(snapshot.catalog(), BufferId::new(1)).unwrap();
                let offsets = snapshot
                    .catalog()
                    .get(BufferId::new(1))
                    .unwrap()
                    .char_boundaries();
                let selection = Selection::new(
                    snapshot.catalog(),
                    &valid_view,
                    BufferId::new(1),
                    offsets[0],
                    offsets[1],
                )
                .unwrap();
                let region = OrientedRegion::from_selection(
                    snapshot.catalog(),
                    &valid_view,
                    selection,
                    CoordKind::Byte,
                    EndpointMode::Exclusive,
                )
                .unwrap();
                let replacement = Replacement {
                    region,
                    text: Text::from_str("x"),
                };
                let operations = [
                    InputOperation::Read,
                    InputOperation::Open(Text::from_str("x")),
                    InputOperation::Close(ClosePolicy::Discard),
                    InputOperation::Edit(replacement.clone()),
                    InputOperation::Batch(vec![replacement.clone()]),
                    InputOperation::MultiBatch(vec![BatchEdit {
                        buffer: BufferId::new(1),
                        replacement,
                    }]),
                    InputOperation::Region(region),
                    InputOperation::Resync,
                ];
                for operation in operations {
                    let mut base = input(&snapshot, operation.clone());
                    if matches!(operation, InputOperation::Open(_)) {
                        base.target = Some(Target::Name("file".into()));
                    }
                    for defect in 0..5 {
                        let mut raw = base.clone();
                        match defect {
                            0 => raw.actor = None,
                            1 => raw.revision = None,
                            2 => raw.target = None,
                            3 => {
                                raw.target = Some(if matches!(operation, InputOperation::Open(_)) {
                                    Target::Buffer(BufferId::new(1))
                                } else {
                                    Target::Name("file".into())
                                })
                            }
                            _ => match &mut raw.operation {
                                InputOperation::Edit(edit) => edit.region = invalid_region,
                                InputOperation::Batch(edits) => edits[0].region = invalid_region,
                                InputOperation::MultiBatch(edits) => {
                                    edits[0].replacement.region = invalid_region
                                }
                                InputOperation::Region(region) => *region = invalid_region,
                                InputOperation::Open(_) => {
                                    raw.target = Some(Target::Name(format!("bad\0{value}")))
                                }
                                _ => raw.revision = Some(Revision::MAX),
                            },
                        }
                        for envelope in [
                            Envelope::Command(raw.clone()),
                            Envelope::BufferOperation(raw.clone()),
                            Envelope::AgentProposal(raw.clone()),
                        ] {
                            assert!(envelope.normalize(&snapshot).is_err(), "accepted {raw:?}");
                        }
                    }
                }
            },
        );
    }

    #[test]
    fn generated_encodings_normalize_to_one_request() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| (rng.next_u64(), rng.next_u64(), rng.next_u64()),
            |(actor, value, other)| {
                let snapshot = fixture();
                let buffer = BufferId::new(1);
                let text = snapshot.catalog().get(buffer).unwrap();
                let offsets = text.char_boundaries();
                let start = offsets[*value as usize % offsets.len()];
                let end = offsets[*other as usize % offsets.len()];
                let (start, end) = (start.min(end), start.max(end));
                let view = BufferView::full(snapshot.catalog(), buffer).unwrap();
                let selection =
                    Selection::new(snapshot.catalog(), &view, buffer, start, end).unwrap();
                let mut results = Vec::new();
                for kind in [
                    CoordKind::Byte,
                    CoordKind::EmacsChar,
                    CoordKind::LineColByte,
                    CoordKind::LineColUtf16,
                ] {
                    let region = OrientedRegion::from_selection(
                        snapshot.catalog(),
                        &view,
                        selection,
                        kind,
                        EndpointMode::Exclusive,
                    )
                    .unwrap();
                    let replacement = Replacement {
                        region,
                        text: Text::from_str("replacement"),
                    };
                    let mut raw = input(&snapshot, InputOperation::Edit(replacement.clone()));
                    raw.actor = Some(ActorId::new(*actor));
                    results.push(Envelope::Command(raw.clone()).normalize(&snapshot).unwrap());
                    results.push(
                        Envelope::AgentProposal(raw.clone())
                            .normalize(&snapshot)
                            .unwrap(),
                    );
                    raw.operation = InputOperation::Batch(vec![replacement]);
                    results.push(Envelope::BufferOperation(raw).normalize(&snapshot).unwrap());
                }
                assert!(results.iter().all(|result| result == &results[0]));
                let proposal = results[0]
                    .proposal(RequestId::new(3), Nonce::new(5))
                    .unwrap();
                assert_eq!(
                    proposal.transaction().intent().actor(),
                    ActorId::new(*actor)
                );
                assert_eq!(proposal.transaction().intent().base(), snapshot.revision());
                let result = proposal
                    .transaction()
                    .apply_with_inverse(&snapshot)
                    .unwrap();
                let source = text.as_str();
                let expected = format!(
                    "{}replacement{}",
                    &source[..start.as_usize()],
                    &source[end.as_usize()..]
                );
                assert_eq!(result.catalog().get(buffer).unwrap().as_str(), expected);
            },
        );
    }

    #[test]
    fn required_fields_and_payloads_are_rejected() {
        let snapshot = fixture();
        let base = input(&snapshot, InputOperation::Read);
        let mut cases = Vec::new();
        let mut raw = base.clone();
        raw.actor = None;
        cases.push((raw, ErrorCode::UnknownActor));
        let mut raw = base.clone();
        raw.revision = None;
        cases.push((raw, ErrorCode::RevisionMismatch));
        let mut raw = base.clone();
        raw.revision = Some(Revision::MAX);
        cases.push((raw, ErrorCode::RevisionMismatch));
        let mut raw = base.clone();
        raw.target = None;
        cases.push((raw, ErrorCode::InvalidCoordinate));
        let mut raw = base.clone();
        raw.target = Some(Target::Buffer(BufferId::new(99)));
        cases.push((raw, ErrorCode::UnknownBuffer));
        let mut raw = base.clone();
        raw.target = Some(Target::Name(String::new()));
        cases.push((raw, ErrorCode::InvalidCoordinate));
        let mut raw = base;
        raw.target = Some(Target::Name("  ".into()));
        raw.operation = InputOperation::Open(Text::from_str(""));
        cases.push((raw, ErrorCode::InvalidCoordinate));
        for (raw, code) in cases {
            for envelope in [
                Envelope::Command(raw.clone()),
                Envelope::BufferOperation(raw.clone()),
                Envelope::AgentProposal(raw.clone()),
            ] {
                assert_eq!(envelope.normalize(&snapshot).unwrap_err().code(), code);
            }
        }
    }

    #[test]
    fn batch_permutations_and_invalid_coordinates() {
        use crate::editor::interop::{
            agent_position::AgentPosition, coordinates::BytePos, region::RegionCoord,
        };
        use crate::editor::workspace::selections::Direction;
        let snapshot = fixture();
        let region = |a, b| OrientedRegion {
            anchor: RegionCoord::Agent(AgentPosition::Byte(BytePos::new(a))),
            head: RegionCoord::Agent(AgentPosition::Byte(BytePos::new(b))),
            direction: Direction::Forward,
            mode: EndpointMode::Exclusive,
        };
        let first = Replacement {
            region: region(0, 1),
            text: Text::from_str("X"),
        };
        let second = Replacement {
            region: region(4, 8),
            text: Text::from_str("Y"),
        };
        let forward = input(
            &snapshot,
            InputOperation::Batch(vec![first.clone(), second.clone()]),
        );
        let reverse = input(
            &snapshot,
            InputOperation::Batch(vec![second, first.clone()]),
        );
        let expected = Envelope::Command(forward).normalize(&snapshot).unwrap();
        assert_eq!(
            expected,
            Envelope::BufferOperation(reverse)
                .normalize(&snapshot)
                .unwrap()
        );
        let result = expected
            .proposal(RequestId::new(1), Nonce::new(1))
            .unwrap()
            .transaction()
            .apply_with_inverse(&snapshot)
            .unwrap();
        assert_eq!(
            result.catalog().get(BufferId::new(1)).unwrap().as_str(),
            "X界Yz"
        );
        for operation in [
            InputOperation::Batch(vec![first.clone(), first]),
            InputOperation::Edit(Replacement {
                region: region(2, 2),
                text: Text::from_str(""),
            }),
        ] {
            let raw = input(&snapshot, operation);
            for envelope in [
                Envelope::Command(raw.clone()),
                Envelope::BufferOperation(raw.clone()),
                Envelope::AgentProposal(raw.clone()),
            ] {
                assert!(envelope.normalize(&snapshot).is_err());
            }
        }
    }

    #[test]
    fn resync_accepts_observed_older_revision() {
        let mut workspace = VersionedWorkspace::new(fixture().catalog().clone());
        let observed = workspace.capture();
        workspace
            .update(BufferId::new(1), Text::from_str("new"))
            .unwrap();
        let snapshot = workspace.capture();
        let mut raw = input(&snapshot, InputOperation::Resync);
        raw.revision = None;
        for envelope in [
            Envelope::Command(raw.clone()),
            Envelope::BufferOperation(raw.clone()),
            Envelope::AgentProposal(raw.clone()),
        ] {
            assert_eq!(
                envelope.normalize(&snapshot).unwrap_err().code(),
                ErrorCode::RevisionMismatch
            );
        }
        raw.revision = Some(Revision::ZERO);
        let request = Envelope::Command(raw.clone()).normalize(&observed).unwrap();
        assert_eq!(request.revision(), Revision::ZERO);
        assert_eq!(request.operation(), &Operation::Resync);
        assert_eq!(request.target(), &Target::Buffer(BufferId::new(1)));
        raw.revision = Some(Revision::MAX);
        assert_eq!(
            Envelope::Command(raw)
                .normalize(&snapshot)
                .unwrap_err()
                .code(),
            ErrorCode::RevisionMismatch
        );
    }

    #[test]
    fn empty_batch_requires_permission_and_has_no_proposal() {
        use crate::editor::collab::role::ActorRole;
        let snapshot = fixture();
        let raw = input(&snapshot, InputOperation::Batch(vec![]));
        let actor = ActorId::new(7);
        let mut registry = ActorRegistry::new();
        registry.register(actor, ActorRole::Human).unwrap();
        let policy = AccessPolicy::new();
        for envelope in [
            Envelope::Command(raw.clone()),
            Envelope::BufferOperation(raw.clone()),
            Envelope::AgentProposal(raw),
        ] {
            let request = envelope.normalize(&snapshot).unwrap();
            assert!(matches!(
                request.authorize(&policy, &registry, &snapshot),
                Err(RequestError::Access(AccessError::Denied { .. }))
            ));
            assert!(request.proposal(RequestId::new(1), Nonce::new(9)).is_none());
        }
    }

    #[test]
    fn open_authorization_is_decided_by_lifecycle() {
        use crate::editor::collab::{
            lifecycle::{CollaborativeLifecycle, LifecycleError},
            role::ActorRole,
        };
        let actor = ActorId::new(7);
        let mut registry = ActorRegistry::new();
        registry.register(actor, ActorRole::Human).unwrap();
        let policy = AccessPolicy::new();
        let mut authority = CollaborativeLifecycle::new(registry.clone(), policy.clone());
        let snapshot = authority.snapshot();
        let mut raw = input(&snapshot, InputOperation::Open(Text::from_str("content")));
        raw.target = Some(Target::Name("file".into()));
        let request = Envelope::Command(raw).normalize(&snapshot).unwrap();
        assert_eq!(
            request.authorize(&policy, &registry, &snapshot),
            Err(RequestError::OpenRequiresLifecycle)
        );
        let (intent, operation) = request.lifecycle_operation(Nonce::new(1)).unwrap();
        assert!(matches!(
            authority.submit(RequestId::new(1), intent, operation.clone()),
            Err(LifecycleError::Access(AccessError::Denied { .. }))
        ));
        authority
            .access_mut()
            .grant_buffer(actor, BufferId::new(0), Capability::Commit);
        assert!(authority
            .submit(RequestId::new(1), intent, operation)
            .is_ok());
    }

    #[test]
    fn closed_resync_checks_current_grants_and_recovers() {
        use crate::editor::{
            collab::role::ActorRole, workspace::buffer_lifecycle::BufferLifecycle,
        };
        let buffer = BufferId::new(1);
        let actor = ActorId::new(7);
        let mut lifecycle = BufferLifecycle::new();
        lifecycle.open(buffer, Text::from_str("secret")).unwrap();
        let initial = lifecycle.snapshot();
        lifecycle.close(buffer, ClosePolicy::Discard).unwrap();
        let closed = lifecycle.snapshot();
        let mut raw = input(&closed, InputOperation::Resync);
        raw.revision = Some(initial.revision());
        let mut registry = ActorRegistry::new();
        registry.register(actor, ActorRole::Human).unwrap();
        let mut policy = AccessPolicy::new();
        for envelope in [
            Envelope::Command(raw.clone()),
            Envelope::BufferOperation(raw.clone()),
            Envelope::AgentProposal(raw),
        ] {
            let request = envelope.normalize(&initial).unwrap();
            assert!(matches!(
                request.authorize(&policy, &registry, &closed),
                Err(RequestError::Access(AccessError::Denied { .. }))
            ));
            assert!(matches!(
                request.recover(&policy, &registry, &initial, &closed),
                Err(RequestError::Access(AccessError::Denied { .. }))
            ));
            policy.grant_buffer(actor, buffer, Capability::Read);
            assert_eq!(request.authorize(&policy, &registry, &closed), Ok(()));
            let recovered = request
                .recover(&policy, &registry, &initial, &closed)
                .unwrap();
            assert_eq!(
                recovered,
                Recovery::Closed {
                    buffer,
                    revision: closed.revision()
                }
            );
            assert_eq!(
                request.recover(&policy, &registry, &closed, &closed),
                Err(RequestError::InvalidRecovery)
            );
            policy.revoke_buffer(actor, buffer, Capability::Read);
        }
        policy.grant_buffer(actor, buffer, Capability::Read);
        let unknown = VersionedWorkspace::new(BufferCatalog::new()).capture();
        let request = Envelope::Command(input(&initial, InputOperation::Resync))
            .normalize(&initial)
            .unwrap();
        assert_eq!(
            request.authorize(&policy, &registry, &unknown),
            Err(RequestError::DifferentWorkspace)
        );
        registry.revoke(actor).unwrap();
        assert_eq!(
            request.authorize(&policy, &registry, &closed),
            Err(RequestError::Access(AccessError::InactiveActor(actor)))
        );
    }

    #[test]
    fn resync_validates_observed_target_and_limits_projection() {
        use crate::editor::collab::role::ActorRole;
        let mut workspace = VersionedWorkspace::new(BufferCatalog::new());
        let absent = workspace.capture();
        let buffer = BufferId::new(1);
        let secret = BufferId::new(2);
        workspace
            .register(buffer, Text::from_str("visible"))
            .unwrap();
        workspace
            .register(secret, Text::from_str("secret"))
            .unwrap();
        let observed = workspace.capture();
        let raw = input(&absent, InputOperation::Resync);
        for envelope in [
            Envelope::Command(raw.clone()),
            Envelope::BufferOperation(raw.clone()),
            Envelope::AgentProposal(raw),
        ] {
            assert_eq!(
                envelope.normalize(&absent).unwrap_err().code(),
                ErrorCode::UnknownBuffer
            );
        }
        let request = Envelope::Command(input(&observed, InputOperation::Resync))
            .normalize(&observed)
            .unwrap();
        workspace.update(buffer, Text::from_str("updated")).unwrap();
        let current = workspace.capture();
        let mut registry = ActorRegistry::new();
        registry
            .register(ActorId::new(7), ActorRole::Human)
            .unwrap();
        let mut policy = AccessPolicy::new();
        assert!(matches!(
            request.recover(&policy, &registry, &observed, &current),
            Err(RequestError::Access(AccessError::Denied { .. }))
        ));
        policy.grant_buffer(ActorId::new(7), buffer, Capability::Read);
        assert!(!policy.is_granted(ActorId::new(7), secret, Capability::Read));
        assert_eq!(
            request.recover(&policy, &registry, &observed, &current),
            Ok(Recovery::Live(AgentSnapshot {
                buffer,
                revision: current.revision(),
                text: "updated".into()
            }))
        );
    }

    #[test]
    fn multi_buffer_batch_preserves_all_targets_and_authorization() {
        use crate::editor::{
            collab::role::ActorRole,
            interop::{agent_position::AgentPosition, coordinates::BytePos, region::RegionCoord},
            workspace::selections::Direction,
        };
        let mut catalog = BufferCatalog::new();
        for id in [0, 1, 2] {
            catalog
                .register(BufferId::new(id), Text::from_str("abc"))
                .unwrap();
        }
        let snapshot = VersionedWorkspace::new(catalog).capture();
        let region = OrientedRegion {
            anchor: RegionCoord::Agent(AgentPosition::Byte(BytePos::new(0))),
            head: RegionCoord::Agent(AgentPosition::Byte(BytePos::new(1))),
            direction: Direction::Forward,
            mode: EndpointMode::Exclusive,
        };
        let edit = |id, value| BatchEdit {
            buffer: BufferId::new(id),
            replacement: Replacement {
                region,
                text: Text::from_str(value),
            },
        };
        let first = edit(1, "X");
        let second = edit(2, "Y");
        let raw = input(
            &snapshot,
            InputOperation::MultiBatch(vec![first.clone(), second.clone()]),
        );
        let mut mismatched = raw.clone();
        mismatched.target = Some(Target::Buffer(BufferId::new(0)));
        for envelope in [
            Envelope::Command(mismatched.clone()),
            Envelope::BufferOperation(mismatched.clone()),
            Envelope::AgentProposal(mismatched),
        ] {
            assert_eq!(
                envelope.normalize(&snapshot).unwrap_err().code(),
                ErrorCode::InvalidCoordinate
            );
        }
        let reverse = input(&snapshot, InputOperation::MultiBatch(vec![second, first]));
        let requests = [
            Envelope::Command(raw.clone()),
            Envelope::BufferOperation(reverse),
            Envelope::AgentProposal(raw),
        ];
        let requests: Vec<_> = requests
            .into_iter()
            .map(|e| e.normalize(&snapshot).unwrap())
            .collect();
        assert!(requests.iter().all(|r| r == &requests[0]));
        let proposal = requests[0]
            .proposal(RequestId::new(1), Nonce::new(2))
            .unwrap();
        assert_eq!(proposal.transaction().patches().len(), 2);
        let result = proposal
            .transaction()
            .apply_with_inverse(&snapshot)
            .unwrap();
        assert_eq!(
            result.catalog().get(BufferId::new(1)).unwrap().as_str(),
            "Xbc"
        );
        assert_eq!(
            result.catalog().get(BufferId::new(2)).unwrap().as_str(),
            "Ybc"
        );
        let mut registry = ActorRegistry::new();
        registry
            .register(ActorId::new(7), ActorRole::Human)
            .unwrap();
        let mut policy = AccessPolicy::new();
        policy.grant_buffer(ActorId::new(7), BufferId::new(1), Capability::Commit);
        assert!(
            matches!(requests[0].authorize(&policy, &registry, &snapshot), Err(RequestError::Access(AccessError::Denied { buffer, .. })) if buffer == BufferId::new(2))
        );
        policy.grant_buffer(ActorId::new(7), BufferId::new(2), Capability::Commit);
        assert_eq!(requests[0].authorize(&policy, &registry, &snapshot), Ok(()));
    }

    #[test]
    fn generated_batch_order_and_region_orientation() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| (rng.next_u64(), rng.next_u64()),
            |(a, b)| {
                let snapshot = fixture();
                let buffer = BufferId::new(1);
                let text = snapshot.catalog().get(buffer).unwrap();
                let offsets = text.char_boundaries();
                let view = BufferView::full(snapshot.catalog(), buffer).unwrap();
                let region = |start, end| {
                    let selection = Selection::new(
                        snapshot.catalog(),
                        &view,
                        buffer,
                        offsets[start],
                        offsets[end],
                    )
                    .unwrap();
                    OrientedRegion::from_selection(
                        snapshot.catalog(),
                        &view,
                        selection,
                        CoordKind::Byte,
                        EndpointMode::Exclusive,
                    )
                    .unwrap()
                };
                let left = format!("L{a}");
                let right = format!("R{b}");
                let first = Replacement {
                    region: region(0, 1),
                    text: Text::from_str(&left),
                };
                let second = Replacement {
                    region: region(2, 3),
                    text: Text::from_str(&right),
                };
                let forward = input(
                    &snapshot,
                    InputOperation::Batch(vec![first.clone(), second.clone()]),
                );
                let reverse = input(&snapshot, InputOperation::Batch(vec![second, first]));
                let requests = [
                    Envelope::Command(forward.clone()),
                    Envelope::BufferOperation(reverse),
                    Envelope::AgentProposal(forward),
                ];
                let requests: Vec<_> = requests
                    .into_iter()
                    .map(|e| e.normalize(&snapshot).unwrap())
                    .collect();
                assert!(requests.iter().all(|r| r == &requests[0]));
                let proposal = requests[0]
                    .proposal(RequestId::new(1), Nonce::new(1))
                    .unwrap();
                let result = proposal
                    .transaction()
                    .apply_with_inverse(&snapshot)
                    .unwrap();
                assert_eq!(
                    result.catalog().get(buffer).unwrap().as_str(),
                    format!("{left}界{right}z")
                );
                let selected =
                    Selection::new(snapshot.catalog(), &view, buffer, offsets[3], offsets[1])
                        .unwrap();
                let raw = input(&snapshot, InputOperation::Region(region(3, 1)));
                for envelope in [
                    Envelope::Command(raw.clone()),
                    Envelope::BufferOperation(raw.clone()),
                    Envelope::AgentProposal(raw),
                ] {
                    assert_eq!(
                        envelope.normalize(&snapshot).unwrap().operation(),
                        &Operation::Region(selected)
                    );
                }
            },
        );
    }

    #[test]
    fn lifecycle_and_read_operations_preserve_payloads() {
        let snapshot = fixture();
        for operation in [
            InputOperation::Read,
            InputOperation::Resync,
            InputOperation::Close(ClosePolicy::Discard),
        ] {
            let request = Envelope::Command(input(&snapshot, operation.clone()))
                .normalize(&snapshot)
                .unwrap();
            assert_eq!(
                request.capability(),
                if matches!(operation, InputOperation::Close(_)) {
                    Capability::Commit
                } else {
                    Capability::Read
                }
            );
            assert_eq!(
                request.lifecycle_operation(Nonce::new(4)).is_some(),
                matches!(operation, InputOperation::Close(_))
            );
        }
        let mut raw = input(&snapshot, InputOperation::Open(Text::from_str("hello")));
        raw.target = Some(Target::Name("file".into()));
        let request = Envelope::Command(raw).normalize(&snapshot).unwrap();
        assert!(
            matches!(request.lifecycle_operation(Nonce::new(4)).unwrap().1, lifecycle::Operation::Open { name, .. } if name == "file")
        );
    }
}
