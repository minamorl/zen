//! End-to-end interoperability conformance harness and suite across the
//! Emacs, Neovim, and Agent typed adapters.
//!
//! # Conformance Laws
//!
//! Given the exact same initial workspace and coordinator state, semantically
//! equivalent operation sequences translated into the three typed adapter
//! models must agree identically after every step on:
//!
//! 1. **Buffer text across all buffers:** authoritative catalog text,
//!    tombstone sets, incrementally updated adapter-native snapshots
//!    (`EmacsSnapshot`, `NeovimSnapshot`, `AgentSnapshot`), and recovered
//!    lagging `Resync` client snapshots agree byte-for-byte across ASCII,
//!    multi-byte UTF-8, CJK, emoji, LF, and CRLF edge cases.
//! 2. **Normalized cursor, selection, and annotation state:** selections and
//!    overlays projected by `WorkspaceSession` roundtrip through each
//!    adapter's native coordinate system (`EmacsCharPos`, `LineColByte`,
//!    `OneBasedLineColByte`, `AgentPosition`, `EmacsOverlay`, `NeovimExtmark`,
//!    `AgentAnnotation`) to identical normalized byte spans and directions.
//! 3. **Revisions, receipts, and change groups:** workspace revisions,
//!    `CommitReceipt` sequences, and `GroupPlan` change-group identities agree
//!    identically.
//! 4. **Exact rejection reasons with zero state drift:** invalid, stale,
//!    conflicting, compacted, or unauthorized operations reject with identical
//!    normalized [`RejectionReason`] values and leave coordinator checkpoints,
//!    session state, frontend snapshots, resync clients, and group plans
//!    completely unchanged.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use super::{
    agent_position::{self, AgentPosition},
    annotations::{self, AgentAnnotation, EmacsOverlay, NeovimExtmark},
    commands::{self, AgentCommand, Context, EmacsCommand, NeovimCommand},
    coordinates::{BytePos, CoordKind, EmacsCharPos, LineColByte, LineColUtf16},
    delta, emacs_position,
    error::{BoundaryError, ErrorCode},
    groups::{GroupPlan, SequenceStep},
    neovim_position,
    region::{EndpointMode, OrientedRegion, RegionCoord},
    requests::{
        BatchEdit, Envelope, Input, InputOperation, Operation as RequestOperation, Recovery,
        Replacement, Request, RequestError, Target,
    },
    resync::{Resync, ResyncError},
    snapshot::{self, AgentSnapshot, EmacsSnapshot, NeovimSnapshot},
    subscriptions::{InitialSnapshots, Notification, Subscription},
    undo::{
        self, CoreRequest, FrontendCommand, InteropError as UndoInteropError, Target as UndoTarget,
    },
};
use crate::editor::{
    collab::{
        access::{AccessError, AccessPolicy, Capability},
        checkpoints::{CheckpointError, CheckpointOutcome},
        coordinator::{
            Coordinator, CoordinatorCheckpoint, CoordinatorError, Operation as CoordinatorOperation,
        },
        delivery::{CatchUp, DeliveryError},
        proposal::{ProposalError, RequestId},
        rebase::RebaseError,
        receipt::{CommitReceipt, RejectReason, SubmitOutcome},
        redo::RedoError,
        registry::{ActorRegistry, RegistryError},
        retention::RetentionError,
        role::ActorRole,
        selective_undo::UndoError,
    },
    edit::{
        ids::{ActorId, BufferId, Nonce, TxnId},
        intent::EditIntent,
        transaction::{Transaction, TransactionError},
    },
    text::{anchors::Affinity, lines::LineIndex, span::ByteSpan, utf8::Text},
    workspace::{
        annotations::{Annotation, AnnotationKey, CollapsePolicy},
        buffer_lifecycle::ClosePolicy,
        selections::{Direction, Selection},
        session::{SessionError, WorkspaceSession},
        snapshots::WorkspaceSnapshot,
        version::Revision,
        view::BufferView,
    },
};

/// One of the three typed frontend adapters exercised by the conformance suite.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AdapterKind {
    /// Emacs point/mark/region/overlay/change-group adapter.
    Emacs,
    /// Neovim cursor/extmark/line-array/undo-block adapter.
    Neovim,
    /// Agent typed request/proposal/batch adapter.
    Agent,
}

impl AdapterKind {
    /// All three adapters in canonical order.
    pub const ALL: [Self; 3] = [Self::Emacs, Self::Neovim, Self::Agent];

    /// Stable human-readable adapter name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Emacs => "emacs",
            Self::Neovim => "neovim",
            Self::Agent => "agent",
        }
    }
}

impl fmt::Display for AdapterKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Normalized, adapter-agnostic rejection classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RejectionReason {
    InvalidUtf8,
    InvalidBoundary,
    InvalidUtf16Boundary,
    OutOfRange,
    InvalidSpan,
    InvalidCoordinate,
    InvalidLineEndings,
    CorruptSequence,
    UnsupportedOperation,
    RevisionMismatch,
    RevisionOverflow,
    BaseExpired,
    FutureRevision,
    Conflict,
    UnknownBuffer,
    BufferClosed,
    DuplicateBufferId,
    NameInUse,
    DirtyBuffer,
    IdExhausted,
    UnknownActor,
    RevokedActor,
    PermissionDenied,
    RequestReuse,
    TxnIdReuse,
    MissingProposal,
    UnapprovedProposal,
    StaleApproval,
    NotHumanReviewer,
    EmptyProposal,
    ApprovalMismatch,
    UnknownUndoTarget,
    NotUndoOwner,
    AlreadyUndone,
    NotUndone,
    AmbiguousUndoTarget,
    InvalidUndoTransaction,
    NoChange,
    DifferentWorkspace,
    StaleSnapshot,
    ResyncGap,
    InvalidEvent,
    InvalidSessionState,
    EmptyPattern,
}

impl fmt::Display for RejectionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Normalized selection state in byte offsets after adapter coordinate roundtrip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NormalizedSelection {
    pub anchor: usize,
    pub head: usize,
    pub direction: Direction,
}

/// Normalized annotation state in byte offsets after adapter projection roundtrip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedAnnotation {
    pub start: usize,
    pub end: usize,
    pub start_affinity: Affinity,
    pub end_affinity: Affinity,
    pub priority: u32,
    pub collapse: CollapsePolicy,
    pub label: String,
}

/// Normalized outcome of executing one step through an adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepOutcome {
    /// A workspace mutation committed with its receipt and optional target buffer.
    Committed {
        receipt: CommitReceipt,
        buffer: Option<BufferId>,
    },
    /// A proposal was stored awaiting human review.
    Proposed { request: RequestId },
    /// A proposal received a human approval.
    Approved {
        request: RequestId,
        reviewer: ActorId,
    },
    /// A selection/cursor update succeeded without advancing workspace revision.
    Selected {
        actor: ActorId,
        buffer: BufferId,
        selection: NormalizedSelection,
    },
    /// An annotation/overlay/extmark update succeeded without advancing workspace revision.
    Annotated {
        actor: ActorId,
        key: AnnotationKey,
        annotation: NormalizedAnnotation,
    },
    /// A read, view update, recovery, policy change, actor change, or compaction succeeded.
    Applied { revision: Revision },
    /// The operation was rejected atomically with zero state drift.
    Rejected {
        reason: RejectionReason,
        revision: Revision,
    },
}

/// Canonical observable state extracted from one adapter harness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterObservation {
    pub adapter: AdapterKind,
    pub revision: Revision,
    pub live_buffers: BTreeMap<BufferId, String>,
    pub tombstoned_buffers: BTreeSet<BufferId>,
    pub projected_buffers: BTreeMap<BufferId, (Revision, String)>,
    pub lagging_buffers: BTreeMap<BufferId, (Revision, String)>,
    pub selections: BTreeMap<(ActorId, BufferId), NormalizedSelection>,
    pub annotations: BTreeMap<(ActorId, AnnotationKey), NormalizedAnnotation>,
    pub receipts: Vec<CommitReceipt>,
    pub change_groups: Vec<CommitReceipt>,
    pub outcomes: Vec<StepOutcome>,
}

/// Specific divergence detected by the conformance comparator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConformanceDivergence {
    BufferText {
        left: AdapterKind,
        right: AdapterKind,
        buffer: BufferId,
        left_text: Option<String>,
        right_text: Option<String>,
    },
    ProjectedBufferText {
        left: AdapterKind,
        right: AdapterKind,
        buffer: BufferId,
        left_snapshot: Option<(Revision, String)>,
        right_snapshot: Option<(Revision, String)>,
    },
    LaggingBufferText {
        left: AdapterKind,
        right: AdapterKind,
        buffer: BufferId,
        left_snapshot: Option<(Revision, String)>,
        right_snapshot: Option<(Revision, String)>,
    },
    Tombstones {
        left: AdapterKind,
        right: AdapterKind,
        left_tombstones: BTreeSet<BufferId>,
        right_tombstones: BTreeSet<BufferId>,
    },
    Revision {
        left: AdapterKind,
        right: AdapterKind,
        left_revision: Revision,
        right_revision: Revision,
    },
    Selection {
        left: AdapterKind,
        right: AdapterKind,
        actor: ActorId,
        buffer: BufferId,
        left_selection: Option<NormalizedSelection>,
        right_selection: Option<NormalizedSelection>,
    },
    Annotation {
        left: AdapterKind,
        right: AdapterKind,
        actor: ActorId,
        key: AnnotationKey,
        left_annotation: Option<NormalizedAnnotation>,
        right_annotation: Option<NormalizedAnnotation>,
    },
    Receipt {
        left: AdapterKind,
        right: AdapterKind,
        index: usize,
        left_receipt: Option<CommitReceipt>,
        right_receipt: Option<CommitReceipt>,
    },
    ChangeGroups {
        left: AdapterKind,
        right: AdapterKind,
        left_groups: Vec<CommitReceipt>,
        right_groups: Vec<CommitReceipt>,
    },
    StepOutcome {
        left: AdapterKind,
        right: AdapterKind,
        step: usize,
        left_outcome: Option<StepOutcome>,
        right_outcome: Option<StepOutcome>,
    },
    RejectionReason {
        left: AdapterKind,
        right: AdapterKind,
        step: usize,
        left_reason: RejectionReason,
        right_reason: RejectionReason,
    },
    RejectionStateDrift {
        adapter: AdapterKind,
        step: usize,
    },
}

impl fmt::Display for ConformanceDivergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Failure containing every divergence observed across adapters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConformanceFailure {
    pub divergences: Vec<ConformanceDivergence>,
}

impl fmt::Display for ConformanceFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "interop conformance failure ({} divergence(s)):",
            self.divergences.len()
        )?;
        for divergence in &self.divergences {
            writeln!(f, "  - {divergence}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConformanceFailure {}

/// Compares two adapter observations across every conformance axis.
pub fn compare_pair(
    left: &AdapterObservation,
    right: &AdapterObservation,
) -> Vec<ConformanceDivergence> {
    let mut divergences = Vec::new();
    let (l, r) = (left.adapter, right.adapter);

    if left.revision != right.revision {
        divergences.push(ConformanceDivergence::Revision {
            left: l,
            right: r,
            left_revision: left.revision,
            right_revision: right.revision,
        });
    }

    let buffer_ids: BTreeSet<BufferId> = left
        .live_buffers
        .keys()
        .chain(right.live_buffers.keys())
        .copied()
        .collect();
    for buffer in buffer_ids {
        let lt = left.live_buffers.get(&buffer);
        let rt = right.live_buffers.get(&buffer);
        if lt != rt {
            divergences.push(ConformanceDivergence::BufferText {
                left: l,
                right: r,
                buffer,
                left_text: lt.cloned(),
                right_text: rt.cloned(),
            });
        }
    }

    let projected_ids: BTreeSet<BufferId> = left
        .projected_buffers
        .keys()
        .chain(right.projected_buffers.keys())
        .copied()
        .collect();
    for buffer in projected_ids {
        let lp = left.projected_buffers.get(&buffer);
        let rp = right.projected_buffers.get(&buffer);
        if lp != rp {
            divergences.push(ConformanceDivergence::ProjectedBufferText {
                left: l,
                right: r,
                buffer,
                left_snapshot: lp.cloned(),
                right_snapshot: rp.cloned(),
            });
        }
    }

    let lagging_ids: BTreeSet<BufferId> = left
        .lagging_buffers
        .keys()
        .chain(right.lagging_buffers.keys())
        .copied()
        .collect();
    for buffer in lagging_ids {
        let ll = left.lagging_buffers.get(&buffer);
        let rl = right.lagging_buffers.get(&buffer);
        if ll != rl {
            divergences.push(ConformanceDivergence::LaggingBufferText {
                left: l,
                right: r,
                buffer,
                left_snapshot: ll.cloned(),
                right_snapshot: rl.cloned(),
            });
        }
    }

    if left.tombstoned_buffers != right.tombstoned_buffers {
        divergences.push(ConformanceDivergence::Tombstones {
            left: l,
            right: r,
            left_tombstones: left.tombstoned_buffers.clone(),
            right_tombstones: right.tombstoned_buffers.clone(),
        });
    }

    let sel_keys: BTreeSet<(ActorId, BufferId)> = left
        .selections
        .keys()
        .chain(right.selections.keys())
        .copied()
        .collect();
    for (actor, buffer) in sel_keys {
        let ls = left.selections.get(&(actor, buffer)).copied();
        let rs = right.selections.get(&(actor, buffer)).copied();
        if ls != rs {
            divergences.push(ConformanceDivergence::Selection {
                left: l,
                right: r,
                actor,
                buffer,
                left_selection: ls,
                right_selection: rs,
            });
        }
    }

    let ann_keys: BTreeSet<(ActorId, AnnotationKey)> = left
        .annotations
        .keys()
        .chain(right.annotations.keys())
        .copied()
        .collect();
    for (actor, key) in ann_keys {
        let la = left.annotations.get(&(actor, key));
        let ra = right.annotations.get(&(actor, key));
        if la != ra {
            divergences.push(ConformanceDivergence::Annotation {
                left: l,
                right: r,
                actor,
                key,
                left_annotation: la.cloned(),
                right_annotation: ra.cloned(),
            });
        }
    }

    let max_receipts = left.receipts.len().max(right.receipts.len());
    for index in 0..max_receipts {
        let lr = left.receipts.get(index).copied();
        let rr = right.receipts.get(index).copied();
        if lr != rr {
            divergences.push(ConformanceDivergence::Receipt {
                left: l,
                right: r,
                index,
                left_receipt: lr,
                right_receipt: rr,
            });
        }
    }

    if left.change_groups != right.change_groups {
        divergences.push(ConformanceDivergence::ChangeGroups {
            left: l,
            right: r,
            left_groups: left.change_groups.clone(),
            right_groups: right.change_groups.clone(),
        });
    }

    let max_outcomes = left.outcomes.len().max(right.outcomes.len());
    for step in 0..max_outcomes {
        let lo = left.outcomes.get(step);
        let ro = right.outcomes.get(step);
        if lo != ro {
            match (lo, ro) {
                (
                    Some(StepOutcome::Rejected {
                        reason: left_reason,
                        revision: left_rev,
                    }),
                    Some(StepOutcome::Rejected {
                        reason: right_reason,
                        revision: right_rev,
                    }),
                ) if left_reason != right_reason => {
                    divergences.push(ConformanceDivergence::RejectionReason {
                        left: l,
                        right: r,
                        step,
                        left_reason: *left_reason,
                        right_reason: *right_reason,
                    });
                    if left_rev != right_rev {
                        divergences.push(ConformanceDivergence::StepOutcome {
                            left: l,
                            right: r,
                            step,
                            left_outcome: lo.cloned(),
                            right_outcome: ro.cloned(),
                        });
                    }
                }
                _ => {
                    divergences.push(ConformanceDivergence::StepOutcome {
                        left: l,
                        right: r,
                        step,
                        left_outcome: lo.cloned(),
                        right_outcome: ro.cloned(),
                    });
                }
            }
        }
    }

    divergences
}

/// Verifies full pairwise conformance across all three adapter observations.
pub fn verify_conformance(
    observations: &[AdapterObservation; 3],
) -> Result<(), ConformanceFailure> {
    let mut divergences = Vec::new();
    for (i, j) in [(0, 1), (0, 2), (1, 2)] {
        divergences.extend(compare_pair(&observations[i], &observations[j]));
    }
    if divergences.is_empty() {
        Ok(())
    } else {
        Err(ConformanceFailure { divergences })
    }
}

/// Initial workspace and policy configuration shared by all three adapters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitialWorkspace {
    pub actors: Vec<(ActorId, ActorRole)>,
    pub default_capabilities: Vec<Capability>,
    pub actor_grants: Vec<(ActorId, Capability)>,
    pub buffer_grants: Vec<(ActorId, BufferId, Capability)>,
    pub initial_buffers: Vec<(String, String)>,
    pub agent_coord_kind: CoordKind,
}

impl InitialWorkspace {
    /// Standard collaborative setup with one human actor (1), one agent actor (2),
    /// full default capabilities, and the supplied named buffers.
    pub fn standard(buffers: &[(&str, &str)], agent_coord_kind: CoordKind) -> Self {
        Self {
            actors: vec![
                (ActorId::new(1), ActorRole::Human),
                (ActorId::new(2), ActorRole::Agent),
            ],
            default_capabilities: Capability::ALL.to_vec(),
            actor_grants: Vec::new(),
            buffer_grants: Vec::new(),
            initial_buffers: buffers
                .iter()
                .map(|(name, text)| ((*name).to_owned(), (*text).to_owned()))
                .collect(),
            agent_coord_kind,
        }
    }
}

/// One intermediate edit inside a grouped command sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupEdit {
    pub start_byte: usize,
    pub end_byte: usize,
    pub text: String,
}

/// One buffer span edit inside an atomic multi-buffer batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpanEdit {
    pub buffer: BufferId,
    pub start_byte: usize,
    pub end_byte: usize,
    pub text: String,
}

/// Access policy mutations applied identically across adapters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyChange {
    GrantDefault(Capability),
    RevokeDefault(Capability),
    GrantActor(ActorId, Capability),
    RevokeActor(ActorId, Capability),
    GrantBuffer(ActorId, BufferId, Capability),
    RevokeBuffer(ActorId, BufferId, Capability),
}

/// Invalid operations guaranteed to produce identical rejections across all three adapters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InvalidOp {
    OutOfRangePosition {
        actor: ActorId,
        buffer: BufferId,
    },
    UnsupportedCommand {
        actor: ActorId,
        buffer: BufferId,
        name: String,
    },
    NestedChangeGroup {
        actor: ActorId,
        buffer: BufferId,
    },
    UnclosedChangeGroup {
        actor: ActorId,
        buffer: BufferId,
        text: String,
    },
    EmptyChangeGroup {
        actor: ActorId,
        buffer: BufferId,
    },
    EndWithoutBegin {
        actor: ActorId,
        buffer: BufferId,
    },
    SelectInsideGroup {
        actor: ActorId,
        buffer: BufferId,
    },
    OverlappingBatch {
        actor: ActorId,
        buffer: BufferId,
        first: (usize, usize),
        second: (usize, usize),
    },
    RequestIdReuse {
        actor: ActorId,
        buffer: BufferId,
        reuse_tag: String,
    },
    TxnIdReuse {
        actor: ActorId,
        buffer: BufferId,
        reuse_tag: String,
    },
    StaleEnvelope {
        tag: String,
        actor: ActorId,
        buffer: BufferId,
        start_byte: usize,
        end_byte: usize,
        text: String,
    },
    MissingActorEnvelope {
        buffer: BufferId,
    },
}

/// Semantically equivalent scenario operation translated into each adapter model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScenarioOp {
    OpenBuffer {
        actor: ActorId,
        name: String,
        text: String,
        base_override: Option<Revision>,
        tag: Option<String>,
    },
    CloseBuffer {
        actor: ActorId,
        buffer: BufferId,
        policy: ClosePolicy,
        base_override: Option<Revision>,
        tag: Option<String>,
    },
    ReadBuffer {
        actor: ActorId,
        buffer: BufferId,
    },
    Insert {
        actor: ActorId,
        buffer: BufferId,
        byte_offset: usize,
        text: String,
        tag: Option<String>,
    },
    Replace {
        actor: ActorId,
        buffer: BufferId,
        start_byte: usize,
        end_byte: usize,
        text: String,
        reverse_emacs_mark: bool,
        tag: Option<String>,
    },
    ChangeGroup {
        actor: ActorId,
        buffer: BufferId,
        edits: Vec<GroupEdit>,
        sequence_step: Option<SequenceStep>,
        tag: Option<String>,
    },
    MultiBufferBatch {
        actor: ActorId,
        primary: BufferId,
        edits: Vec<SpanEdit>,
        mode: EndpointMode,
        tag: Option<String>,
    },
    NarrowView {
        actor: ActorId,
        buffer: BufferId,
        start_byte: usize,
        end_byte: usize,
    },
    Select {
        actor: ActorId,
        buffer: BufferId,
        anchor_byte: usize,
        head_byte: usize,
        mode: EndpointMode,
    },
    SetAnnotation {
        actor: ActorId,
        key: AnnotationKey,
        start_byte: usize,
        end_byte: usize,
        start_affinity: Affinity,
        end_affinity: Affinity,
        priority: u32,
        collapse: CollapsePolicy,
        label: String,
    },
    SaveSnapshot {
        tag: String,
    },
    EditAtSavedBase {
        base_tag: String,
        actor: ActorId,
        buffer: BufferId,
        start_byte: usize,
        end_byte: usize,
        text: String,
        tag: Option<String>,
    },
    ProposeEdit {
        proposal_tag: String,
        actor: ActorId,
        buffer: BufferId,
        start_byte: usize,
        end_byte: usize,
        text: String,
    },
    ApproveProposal {
        proposal_tag: String,
        reviewer: ActorId,
    },
    CommitProposal {
        proposal_tag: String,
        tag: Option<String>,
    },
    Undo {
        actor: ActorId,
        target_tag: String,
        use_resolved_candidate: bool,
        tag: Option<String>,
    },
    Redo {
        actor: ActorId,
        target_tag: String,
        use_resolved_candidate: bool,
        tag: Option<String>,
    },
    RetryCommitted {
        tag: String,
    },
    PauseLagging {
        buffer: BufferId,
    },
    DeliverLatestToLagging {
        buffer: BufferId,
    },
    Compact {
        floor_offset_from_tip: u64,
    },
    CatchUpLagging {
        buffer: BufferId,
    },
    RecoverViaRequest {
        actor: ActorId,
        buffer: BufferId,
        observed_tag: String,
    },
    UpdatePolicy(PolicyChange),
    RegisterActor {
        actor: ActorId,
        role: ActorRole,
    },
    RevokeActor {
        actor: ActorId,
    },
    Invalid(InvalidOp),
}

/// Successful conformance run report containing all three final observations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConformanceReport {
    pub observations: [AdapterObservation; 3],
    pub step_count: usize,
}

impl ConformanceReport {
    pub fn observation(&self, adapter: AdapterKind) -> &AdapterObservation {
        match adapter {
            AdapterKind::Emacs => &self.observations[0],
            AdapterKind::Neovim => &self.observations[1],
            AdapterKind::Agent => &self.observations[2],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NativeSnapshot {
    Emacs(EmacsSnapshot),
    Neovim(NeovimSnapshot),
    Agent(AgentSnapshot),
}

impl NativeSnapshot {
    fn encode(
        adapter: AdapterKind,
        snapshot: &WorkspaceSnapshot,
        buffer: BufferId,
    ) -> Result<Self, BoundaryError> {
        match adapter {
            AdapterKind::Emacs => snapshot::encode_emacs(snapshot, buffer).map(Self::Emacs),
            AdapterKind::Neovim => snapshot::encode_neovim(snapshot, buffer).map(Self::Neovim),
            AdapterKind::Agent => snapshot::encode_agent(snapshot, buffer).map(Self::Agent),
        }
    }

    fn decode(&self) -> Result<(Revision, String), BoundaryError> {
        match self {
            Self::Emacs(s) => {
                let decoded = snapshot::decode_emacs(s);
                Ok((decoded.revision, decoded.text.as_str().to_owned()))
            }
            Self::Neovim(s) => {
                let decoded = snapshot::decode_neovim(s)?;
                Ok((decoded.revision, decoded.text.as_str().to_owned()))
            }
            Self::Agent(s) => {
                let decoded = snapshot::decode_agent(s);
                Ok((decoded.revision, decoded.text.as_str().to_owned()))
            }
        }
    }

    fn bump_revision(&mut self, revision: Revision) {
        match self {
            Self::Emacs(s) => s.revision = revision,
            Self::Neovim(s) => s.revision = revision,
            Self::Agent(s) => s.revision = revision,
        }
    }

    fn apply_change(&mut self, formats: &delta::DeltaFormats) -> Result<(), BoundaryError> {
        match self {
            Self::Emacs(s) => delta::apply_emacs(s, &formats.emacs),
            Self::Neovim(s) => delta::apply_neovim(s, &formats.neovim),
            Self::Agent(s) => delta::apply_agent(s, &formats.agent),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BufferClient {
    resync: Resync,
    native: Option<NativeSnapshot>,
    lagging: Resync,
    lagging_projections: InitialSnapshots,
    lagging_paused: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RecordedCommit {
    request: RequestId,
    operation: CoordinatorOperation,
    txn: TxnId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StateStamp {
    checkpoint: CoordinatorCheckpoint,
    policy: AccessPolicy,
    session: WorkspaceSession,
    clients: BTreeMap<BufferId, BufferClient>,
    group_plan: GroupPlan,
    receipts: Vec<CommitReceipt>,
    outcomes: Vec<StepOutcome>,
    saved_snapshots: BTreeMap<String, WorkspaceSnapshot>,
    tagged_commits: BTreeMap<String, RecordedCommit>,
    proposal_requests: BTreeMap<String, RequestId>,
    annotation_keys: BTreeSet<(ActorId, AnnotationKey)>,
}

#[cfg(test)]
type AfterRejectHook = Box<dyn FnOnce(&mut AdapterHarness)>;

struct AdapterHarness {
    adapter: AdapterKind,
    agent_coord_kind: CoordKind,
    coordinator: Coordinator,
    policy: AccessPolicy,
    session: WorkspaceSession,
    clients: BTreeMap<BufferId, BufferClient>,
    group_plan: GroupPlan,
    receipts: Vec<CommitReceipt>,
    outcomes: Vec<StepOutcome>,
    saved_snapshots: BTreeMap<String, WorkspaceSnapshot>,
    tagged_commits: BTreeMap<String, RecordedCommit>,
    proposal_requests: BTreeMap<String, RequestId>,
    annotation_keys: BTreeSet<(ActorId, AnnotationKey)>,
    #[cfg(test)]
    after_reject_hook: Option<AfterRejectHook>,
}

fn classify_boundary_error(error: &BoundaryError) -> RejectionReason {
    match error.code() {
        ErrorCode::InvalidUtf8 => RejectionReason::InvalidUtf8,
        ErrorCode::InvalidBoundary => RejectionReason::InvalidBoundary,
        ErrorCode::InvalidUtf16Boundary => RejectionReason::InvalidUtf16Boundary,
        ErrorCode::OutOfRange => RejectionReason::OutOfRange,
        ErrorCode::InvalidSpan => RejectionReason::InvalidSpan,
        ErrorCode::RevisionOverflow => RejectionReason::RevisionOverflow,
        ErrorCode::RevisionMismatch => RejectionReason::RevisionMismatch,
        ErrorCode::BaseExpired => RejectionReason::BaseExpired,
        ErrorCode::UnknownBuffer => RejectionReason::UnknownBuffer,
        ErrorCode::BufferClosed => RejectionReason::BufferClosed,
        ErrorCode::DuplicateBufferId => RejectionReason::DuplicateBufferId,
        ErrorCode::UnknownActor => RejectionReason::UnknownActor,
        ErrorCode::RevokedActor => RejectionReason::RevokedActor,
        ErrorCode::Conflict => RejectionReason::Conflict,
        ErrorCode::TxnIdReuse => RejectionReason::TxnIdReuse,
        ErrorCode::EmptyPattern => RejectionReason::EmptyPattern,
        ErrorCode::PermissionDenied => RejectionReason::PermissionDenied,
        ErrorCode::StaleApproval => RejectionReason::StaleApproval,
        ErrorCode::UnsupportedOperation => RejectionReason::UnsupportedOperation,
        ErrorCode::InvalidCoordinate => RejectionReason::InvalidCoordinate,
        ErrorCode::InvalidLineEndings => RejectionReason::InvalidLineEndings,
        ErrorCode::CorruptSequence => RejectionReason::CorruptSequence,
    }
}

fn classify_access_error(error: &AccessError, registry: &ActorRegistry) -> RejectionReason {
    match error {
        AccessError::InactiveActor(actor) => {
            if registry.is_revoked(*actor) {
                RejectionReason::RevokedActor
            } else {
                RejectionReason::UnknownActor
            }
        }
        AccessError::UnknownBuffer(_) => RejectionReason::UnknownBuffer,
        AccessError::BufferClosed(_) => RejectionReason::BufferClosed,
        AccessError::Denied { .. } => RejectionReason::PermissionDenied,
        AccessError::Transaction(err) => classify_transaction_error(err),
    }
}

fn classify_transaction_error(error: &TransactionError) -> RejectionReason {
    match error {
        TransactionError::RevisionOverflow => RejectionReason::RevisionOverflow,
        TransactionError::RevisionMismatch { .. } => RejectionReason::RevisionMismatch,
        TransactionError::UnknownBuffer(_) => RejectionReason::UnknownBuffer,
        TransactionError::BufferClosed(_) => RejectionReason::BufferClosed,
        TransactionError::DuplicateBufferId(_) => RejectionReason::DuplicateBufferId,
        TransactionError::Patch { .. } => RejectionReason::InvalidSpan,
    }
}

fn classify_rebase_error(error: &RebaseError) -> RejectionReason {
    match error {
        RebaseError::RevisionMismatch { .. } => RejectionReason::RevisionMismatch,
        RebaseError::RevisionOverflow => RejectionReason::RevisionOverflow,
        RebaseError::Conflict(_) => RejectionReason::Conflict,
        RebaseError::Transaction(err) => classify_transaction_error(err),
        RebaseError::TargetMismatch | RebaseError::DisconnectedChain => {
            RejectionReason::InvalidEvent
        }
        RebaseError::Anchor(_) | RebaseError::Span(_) | RebaseError::Patch { .. } => {
            RejectionReason::InvalidSpan
        }
    }
}

fn classify_retention_error(error: &RetentionError) -> RejectionReason {
    match error {
        RetentionError::BaseExpired { .. } => RejectionReason::BaseExpired,
        RetentionError::FutureRevision { .. } => RejectionReason::FutureRevision,
        RetentionError::TxnIdReuse(_) => RejectionReason::TxnIdReuse,
        RetentionError::RequestIdReuse(_) => RejectionReason::RequestReuse,
        RetentionError::InvalidCommit => RejectionReason::InvalidEvent,
        RetentionError::Rebase(err) => classify_rebase_error(err),
    }
}

fn classify_delivery_error(error: &DeliveryError) -> RejectionReason {
    match error {
        DeliveryError::FutureRevision { .. } => RejectionReason::FutureRevision,
        DeliveryError::BaseExpired { .. } => RejectionReason::BaseExpired,
        DeliveryError::NonContiguous { .. } => RejectionReason::ResyncGap,
        DeliveryError::InvalidEvent => RejectionReason::InvalidEvent,
    }
}

fn classify_checkpoint_error(error: &CheckpointError) -> RejectionReason {
    match error {
        CheckpointError::InconsistentState => RejectionReason::InvalidEvent,
        CheckpointError::InactiveActor => RejectionReason::UnknownActor,
        CheckpointError::Gone(_) => RejectionReason::BufferClosed,
        CheckpointError::UnknownBuffer(_) => RejectionReason::UnknownBuffer,
        CheckpointError::NameInUse => RejectionReason::NameInUse,
        CheckpointError::Dirty(_) => RejectionReason::DirtyBuffer,
        CheckpointError::IdExhausted => RejectionReason::IdExhausted,
        CheckpointError::BaseExpired { .. } => RejectionReason::BaseExpired,
        CheckpointError::FutureRevision { .. } => RejectionReason::FutureRevision,
        CheckpointError::RequestIdReuse(_) => RejectionReason::RequestReuse,
        CheckpointError::TxnIdReuse(_) => RejectionReason::TxnIdReuse,
        CheckpointError::Delivery(err) => classify_delivery_error(err),
        CheckpointError::Retention(err) => classify_retention_error(err),
        CheckpointError::Workspace(_) => RejectionReason::RevisionOverflow,
        CheckpointError::Transaction(err) => classify_transaction_error(err),
        CheckpointError::Rebase(err) => classify_rebase_error(err),
        CheckpointError::Journal(_) => RejectionReason::InvalidEvent,
    }
}

fn classify_proposal_error(error: &ProposalError, registry: &ActorRegistry) -> RejectionReason {
    match error {
        ProposalError::Access(err) => classify_access_error(err, registry),
        ProposalError::Transaction(err) => classify_transaction_error(err),
        ProposalError::NotHuman(_) => RejectionReason::NotHumanReviewer,
        ProposalError::EmptyProposal => RejectionReason::EmptyProposal,
        ProposalError::ApprovalMismatch => RejectionReason::ApprovalMismatch,
        ProposalError::RevisionMismatch { .. } => RejectionReason::RevisionMismatch,
    }
}

fn classify_coordinator_error(
    error: &CoordinatorError,
    registry: &ActorRegistry,
) -> RejectionReason {
    match error {
        CoordinatorError::Access(err) => classify_access_error(err, registry),
        CoordinatorError::Checkpoint(err) => classify_checkpoint_error(err),
        CoordinatorError::Proposal(err) => classify_proposal_error(err, registry),
        CoordinatorError::Register(_) => RejectionReason::InvalidSpan,
        CoordinatorError::RequestReuse(_) => RejectionReason::RequestReuse,
        CoordinatorError::TransactionReuse(_) => RejectionReason::TxnIdReuse,
        CoordinatorError::MissingProposal(_) => RejectionReason::MissingProposal,
        CoordinatorError::Unapproved(_) => RejectionReason::UnapprovedProposal,
        CoordinatorError::StaleApproval(_) => RejectionReason::StaleApproval,
        CoordinatorError::RevisionMismatch { .. } => RejectionReason::RevisionMismatch,
        CoordinatorError::UnknownTarget(_) => RejectionReason::UnknownUndoTarget,
        CoordinatorError::NotOwner(_) => RejectionReason::NotUndoOwner,
        CoordinatorError::InvalidTransaction(_) => RejectionReason::InvalidUndoTransaction,
        CoordinatorError::AlreadyUndone(_) => RejectionReason::AlreadyUndone,
        CoordinatorError::NotUndone(_) => RejectionReason::NotUndone,
        CoordinatorError::NoChange => RejectionReason::NoChange,
    }
}

fn classify_resync_error(error: &ResyncError) -> RejectionReason {
    match error {
        ResyncError::DifferentWorkspace => RejectionReason::DifferentWorkspace,
        ResyncError::StaleSnapshot => RejectionReason::StaleSnapshot,
        ResyncError::Conflict => RejectionReason::Conflict,
        ResyncError::Gap { .. } => RejectionReason::ResyncGap,
        ResyncError::InvalidEvent => RejectionReason::InvalidEvent,
        ResyncError::Projection(err) => classify_boundary_error(err),
    }
}

fn classify_request_error(error: &RequestError, registry: &ActorRegistry) -> RejectionReason {
    match error {
        RequestError::Access(err) => classify_access_error(err, registry),
        RequestError::Recovery(err) => classify_resync_error(err),
        RequestError::Lifecycle(_) => RejectionReason::InvalidEvent,
        RequestError::DifferentWorkspace => RejectionReason::DifferentWorkspace,
        RequestError::InvalidOperation | RequestError::OpenRequiresLifecycle => {
            RejectionReason::UnsupportedOperation
        }
        RequestError::InvalidRecovery => RejectionReason::InvalidEvent,
    }
}

fn classify_session_error(error: &SessionError, registry: &ActorRegistry) -> RejectionReason {
    match error {
        SessionError::Access(err) => classify_access_error(err, registry),
        SessionError::InvalidState | SessionError::InvalidTransition => {
            RejectionReason::InvalidSessionState
        }
    }
}

fn classify_reject_reason(reason: RejectReason) -> RejectionReason {
    match reason {
        RejectReason::Conflict => RejectionReason::Conflict,
        RejectReason::RevisionMismatch => RejectionReason::RevisionMismatch,
        RejectReason::BaseExpired => RejectionReason::BaseExpired,
        RejectReason::TxnIdReuse => RejectionReason::TxnIdReuse,
        RejectReason::PermissionDenied => RejectionReason::PermissionDenied,
        RejectReason::UnknownBuffer => RejectionReason::UnknownBuffer,
        RejectReason::BufferClosed => RejectionReason::BufferClosed,
        RejectReason::UnknownActor => RejectionReason::UnknownActor,
        RejectReason::RevokedActor => RejectionReason::RevokedActor,
        RejectReason::InvalidInput => RejectionReason::InvalidCoordinate,
    }
}

fn classify_undo_error(error: &UndoInteropError) -> RejectionReason {
    match error {
        UndoInteropError::AmbiguousTarget => RejectionReason::AmbiguousUndoTarget,
        UndoInteropError::InvalidOperationActor => RejectionReason::InvalidUndoTransaction,
        UndoInteropError::TargetActorMismatch { .. } => RejectionReason::NotUndoOwner,
        UndoInteropError::Undo(err) => match err {
            UndoError::UnknownTarget(_) => RejectionReason::UnknownUndoTarget,
            UndoError::NotOwner { .. } => RejectionReason::NotUndoOwner,
            UndoError::AlreadyUndone { .. } => RejectionReason::AlreadyUndone,
            UndoError::CompensationTarget { .. } | UndoError::InvalidCompensationActor => {
                RejectionReason::InvalidUndoTransaction
            }
            UndoError::CompensationIdReuse(_) => RejectionReason::TxnIdReuse,
            UndoError::Rejected(reason) => classify_reject_reason(*reason),
            UndoError::NoOp => RejectionReason::NoChange,
        },
        UndoInteropError::Redo(err) => match err {
            RedoError::UnknownTarget(_) => RejectionReason::UnknownUndoTarget,
            RedoError::NotOwner { .. } => RejectionReason::NotUndoOwner,
            RedoError::NotUndone(_) => RejectionReason::NotUndone,
            RedoError::AlreadyRedone { .. } => RejectionReason::AlreadyUndone,
            RedoError::InvalidRedoActor => RejectionReason::InvalidUndoTransaction,
            RedoError::RedoIdReuse(_) => RejectionReason::TxnIdReuse,
            RedoError::Rejected(reason) => classify_reject_reason(*reason),
            RedoError::NoOp => RejectionReason::NoChange,
        },
    }
}

fn classify_registry_error(error: &RegistryError) -> RejectionReason {
    match error {
        RegistryError::RoleConflict { .. } => RejectionReason::Conflict,
        RegistryError::ActorRevoked(_) => RejectionReason::RevokedActor,
        RegistryError::UnknownActor(_) => RejectionReason::UnknownActor,
    }
}

fn build_initial_checkpoint(
    initial: &InitialWorkspace,
) -> Result<(CoordinatorCheckpoint, AccessPolicy), RejectionReason> {
    let mut actors = ActorRegistry::new();
    let bootstrap_actor = ActorId::new(u64::MAX - 1);
    let mut has_bootstrap = false;
    for &(actor, role) in &initial.actors {
        actors
            .register(actor, role)
            .map_err(|e| classify_registry_error(&e))?;
    }
    let opener = if let Some(&(actor, _)) = initial.actors.first() {
        actor
    } else {
        actors
            .register(bootstrap_actor, ActorRole::Human)
            .map_err(|e| classify_registry_error(&e))?;
        has_bootstrap = true;
        bootstrap_actor
    };

    let mut setup_policy = AccessPolicy::new();
    for cap in Capability::ALL {
        setup_policy.grant_default(cap);
    }

    let mut coordinator = Coordinator::new(actors, setup_policy);
    for (idx, (name, text)) in initial.initial_buffers.iter().enumerate() {
        let rev = coordinator.snapshot().revision();
        let req_id = RequestId::new(idx as u64 + 1);
        let intent = EditIntent::for_actor(opener, Nonce::new(idx as u64 + 1), rev);
        coordinator
            .apply(
                req_id,
                CoordinatorOperation::Open {
                    intent,
                    name: name.clone(),
                    text: Text::from_str(text),
                },
            )
            .map_err(|e| classify_coordinator_error(&e, coordinator.workspace().actors()))?;
    }

    if has_bootstrap {
        let _ = coordinator.actors_mut().revoke(bootstrap_actor);
    }

    let mut policy = AccessPolicy::new();
    for &cap in &initial.default_capabilities {
        policy.grant_default(cap);
    }
    for &(actor, cap) in &initial.actor_grants {
        policy.grant_actor(actor, cap);
    }
    for &(actor, buffer, cap) in &initial.buffer_grants {
        policy.grant_buffer(actor, buffer, cap);
    }
    *coordinator.access_mut() = policy.clone();
    Ok((coordinator.checkpoint(), policy))
}

impl AdapterHarness {
    fn new(
        adapter: AdapterKind,
        agent_coord_kind: CoordKind,
        checkpoint: &CoordinatorCheckpoint,
        policy: AccessPolicy,
    ) -> Self {
        let coordinator = Coordinator::restore(checkpoint).expect("valid initial checkpoint");
        let snapshot = coordinator.snapshot();
        let session = WorkspaceSession::new(
            snapshot.clone(),
            coordinator.workspace().actors().clone(),
            policy.clone(),
        );
        let mut clients = BTreeMap::new();
        for buffer in snapshot.catalog().live_ids() {
            let resync = Resync::new(&snapshot, buffer).expect("live initial buffer");
            let native =
                NativeSnapshot::encode(adapter, &snapshot, buffer).expect("encode initial buffer");
            let lagging = Resync::new(&snapshot, buffer).expect("live lagging client");
            let (_, lagging_projections) =
                Subscription::subscribe(&snapshot, buffer).expect("subscribe lagging client");
            clients.insert(
                buffer,
                BufferClient {
                    resync,
                    native: Some(native),
                    lagging,
                    lagging_projections,
                    lagging_paused: false,
                },
            );
        }
        Self {
            adapter,
            agent_coord_kind,
            coordinator,
            policy,
            session,
            clients,
            group_plan: GroupPlan::new(),
            receipts: Vec::new(),
            outcomes: Vec::new(),
            saved_snapshots: BTreeMap::new(),
            tagged_commits: BTreeMap::new(),
            proposal_requests: BTreeMap::new(),
            annotation_keys: BTreeSet::new(),
            #[cfg(test)]
            after_reject_hook: None,
        }
    }

    #[cfg(test)]
    fn set_after_reject_hook(&mut self, hook: impl FnOnce(&mut Self) + 'static) {
        self.after_reject_hook = Some(Box::new(hook));
    }

    fn coord_kind(&self) -> CoordKind {
        match self.adapter {
            AdapterKind::Emacs => CoordKind::EmacsChar,
            AdapterKind::Neovim => CoordKind::LineColByte,
            AdapterKind::Agent => self.agent_coord_kind,
        }
    }

    fn envelope(&self, input: Input) -> Envelope {
        match self.adapter {
            AdapterKind::Emacs => Envelope::Command(input),
            AdapterKind::Neovim => Envelope::BufferOperation(input),
            AdapterKind::Agent => Envelope::AgentProposal(input),
        }
    }

    fn stamp(&self) -> StateStamp {
        StateStamp {
            checkpoint: self.coordinator.checkpoint(),
            policy: self.policy.clone(),
            session: self.session.clone(),
            clients: self.clients.clone(),
            group_plan: self.group_plan.clone(),
            receipts: self.receipts.clone(),
            outcomes: self.outcomes.clone(),
            saved_snapshots: self.saved_snapshots.clone(),
            tagged_commits: self.tagged_commits.clone(),
            proposal_requests: self.proposal_requests.clone(),
            annotation_keys: self.annotation_keys.clone(),
        }
    }

    fn encode_coord(&self, text: &Text, byte_offset: usize) -> Result<RegionCoord, BoundaryError> {
        let offset = text.validate_offset(byte_offset).map_err(|e| {
            let code = match e {
                crate::editor::text::utf8::Utf8Error::NotCharBoundary { .. } => {
                    ErrorCode::InvalidBoundary
                }
                _ => ErrorCode::OutOfRange,
            };
            BoundaryError::new(code, e.to_string())
        })?;
        match self.adapter {
            AdapterKind::Emacs => {
                emacs_position::byte_to_emacs(text, offset).map(RegionCoord::Emacs)
            }
            AdapterKind::Neovim => {
                // Also validate 1-based mark roundtrip on Neovim coordinates.
                let mark = neovim_position::byte_to_mark(text, offset)?;
                let back = neovim_position::mark_to_byte(text, mark)?;
                assert_eq!(back, offset);
                neovim_position::byte_to_line_col(text, offset).map(RegionCoord::Neovim)
            }
            AdapterKind::Agent => {
                agent_position::byte_to_agent_position(text, offset, self.agent_coord_kind)
                    .map(RegionCoord::Agent)
            }
        }
    }

    fn encode_region(
        &self,
        snapshot: &WorkspaceSnapshot,
        buffer: BufferId,
        anchor_byte: usize,
        head_byte: usize,
        mode: EndpointMode,
    ) -> Result<OrientedRegion, BoundaryError> {
        let catalog = snapshot.catalog();
        let Some(text) = catalog.get(buffer) else {
            let code = if catalog.is_tombstoned(buffer) {
                ErrorCode::BufferClosed
            } else {
                ErrorCode::UnknownBuffer
            };
            return Err(BoundaryError::new(code, "buffer is not live"));
        };
        let view = BufferView::full(catalog, buffer)
            .map_err(|e| BoundaryError::new(ErrorCode::UnknownBuffer, e.to_string()))?;
        let anchor = text.validate_offset(anchor_byte).map_err(|e| {
            let code = match e {
                crate::editor::text::utf8::Utf8Error::NotCharBoundary { .. } => {
                    ErrorCode::InvalidBoundary
                }
                _ => ErrorCode::OutOfRange,
            };
            BoundaryError::new(code, e.to_string())
        })?;
        let head = text.validate_offset(head_byte).map_err(|e| {
            let code = match e {
                crate::editor::text::utf8::Utf8Error::NotCharBoundary { .. } => {
                    ErrorCode::InvalidBoundary
                }
                _ => ErrorCode::OutOfRange,
            };
            BoundaryError::new(code, e.to_string())
        })?;
        let selection = Selection::new(catalog, &view, buffer, anchor, head)
            .map_err(|e| BoundaryError::new(ErrorCode::OutOfRange, e.to_string()))?;
        OrientedRegion::from_selection(catalog, &view, selection, self.coord_kind(), mode)
    }

    fn out_of_range_coord(&self, text: &Text) -> RegionCoord {
        match self.adapter {
            AdapterKind::Emacs => {
                RegionCoord::Emacs(EmacsCharPos::new(text.len_scalars() + 2).expect("nonzero"))
            }
            AdapterKind::Neovim => {
                let line_count = LineIndex::from_text(text).line_count();
                RegionCoord::Neovim(LineColByte::new(line_count, 0))
            }
            AdapterKind::Agent => match self.agent_coord_kind {
                CoordKind::LineColUtf16 => {
                    let line_count = LineIndex::from_text(text).line_count();
                    RegionCoord::Agent(AgentPosition::LineColUtf16(LineColUtf16::new(
                        line_count, 0,
                    )))
                }
                _ => RegionCoord::Agent(AgentPosition::Byte(BytePos::new(text.len_bytes() + 1))),
            },
        }
    }

    fn fallback_zero_coord(&self) -> RegionCoord {
        match self.adapter {
            AdapterKind::Emacs => RegionCoord::Emacs(emacs_position::point_min()),
            AdapterKind::Neovim => RegionCoord::Neovim(LineColByte::new(0, 0)),
            AdapterKind::Agent => match self.agent_coord_kind {
                CoordKind::LineColUtf16 => {
                    RegionCoord::Agent(AgentPosition::LineColUtf16(LineColUtf16::new(0, 0)))
                }
                _ => RegionCoord::Agent(AgentPosition::Byte(BytePos::new(0))),
            },
        }
    }

    fn lower_single_insert(
        &self,
        snapshot: &WorkspaceSnapshot,
        context: Context,
        byte_offset: usize,
        value: &str,
    ) -> Result<Vec<Request>, BoundaryError> {
        let coord = match snapshot.catalog().get(context.buffer) {
            Some(text) => self.encode_coord(text, byte_offset)?,
            None => self.fallback_zero_coord(),
        };
        let text = Text::from_str(value);
        match (self.adapter, coord) {
            (AdapterKind::Emacs, RegionCoord::Emacs(pos)) => commands::lower_emacs(
                snapshot,
                context,
                &[EmacsCommand::Point(pos), EmacsCommand::Insert(text)],
            ),
            (AdapterKind::Neovim, RegionCoord::Neovim(pos)) => commands::lower_neovim(
                snapshot,
                context,
                &[NeovimCommand::Cursor(pos), NeovimCommand::Insert(text)],
            ),
            (AdapterKind::Agent, RegionCoord::Agent(pos)) => commands::lower_agent(
                snapshot,
                context,
                &[AgentCommand::Replace {
                    start: pos,
                    end: pos,
                    text,
                }],
            ),
            _ => unreachable!("coord matches adapter"),
        }
    }

    fn lower_single_replace(
        &self,
        snapshot: &WorkspaceSnapshot,
        context: Context,
        start_byte: usize,
        end_byte: usize,
        value: &str,
        reverse_emacs_mark: bool,
    ) -> Result<Vec<Request>, BoundaryError> {
        let (start, end) = match snapshot.catalog().get(context.buffer) {
            Some(text) => (
                self.encode_coord(text, start_byte)?,
                self.encode_coord(text, end_byte)?,
            ),
            None => (self.fallback_zero_coord(), self.fallback_zero_coord()),
        };
        let text = Text::from_str(value);
        match (self.adapter, start, end) {
            (AdapterKind::Emacs, RegionCoord::Emacs(a), RegionCoord::Emacs(b)) => {
                let (mark, point) = if reverse_emacs_mark { (b, a) } else { (a, b) };
                commands::lower_emacs(
                    snapshot,
                    context,
                    &[
                        EmacsCommand::Mark(mark),
                        EmacsCommand::Point(point),
                        EmacsCommand::ReplaceRegion(text),
                    ],
                )
            }
            (AdapterKind::Neovim, RegionCoord::Neovim(a), RegionCoord::Neovim(b)) => {
                commands::lower_neovim(
                    snapshot,
                    context,
                    &[NeovimCommand::SetText {
                        start: a,
                        end: b,
                        text,
                    }],
                )
            }
            (AdapterKind::Agent, RegionCoord::Agent(a), RegionCoord::Agent(b)) => {
                commands::lower_agent(
                    snapshot,
                    context,
                    &[AgentCommand::Replace {
                        start: a,
                        end: b,
                        text,
                    }],
                )
            }
            _ => unreachable!("coords match adapter"),
        }
    }

    fn lower_change_group(
        &self,
        snapshot: &WorkspaceSnapshot,
        context: Context,
        edits: &[GroupEdit],
    ) -> Result<Vec<Request>, BoundaryError> {
        let mut sim = snapshot
            .catalog()
            .get(context.buffer)
            .cloned()
            .unwrap_or_else(Text::empty);
        match self.adapter {
            AdapterKind::Emacs => {
                let mut cmds = vec![EmacsCommand::BeginChangeGroup];
                for edit in edits {
                    let RegionCoord::Emacs(a) = self.encode_coord(&sim, edit.start_byte)? else {
                        unreachable!()
                    };
                    let RegionCoord::Emacs(b) = self.encode_coord(&sim, edit.end_byte)? else {
                        unreachable!()
                    };
                    if edit.start_byte == edit.end_byte {
                        cmds.push(EmacsCommand::Point(a));
                        cmds.push(EmacsCommand::Insert(Text::from_str(&edit.text)));
                    } else {
                        cmds.push(EmacsCommand::Mark(a));
                        cmds.push(EmacsCommand::Point(b));
                        cmds.push(EmacsCommand::ReplaceRegion(Text::from_str(&edit.text)));
                    }
                    if edit.start_byte <= edit.end_byte {
                        let mut next = sim.as_str().to_owned();
                        next.replace_range(edit.start_byte..edit.end_byte, &edit.text);
                        sim = Text::from_str(&next);
                    }
                }
                cmds.push(EmacsCommand::EndChangeGroup);
                commands::lower_emacs(snapshot, context, &cmds)
            }
            AdapterKind::Neovim => {
                let mut cmds = vec![NeovimCommand::BeginUndoBlock];
                for edit in edits {
                    let RegionCoord::Neovim(a) = self.encode_coord(&sim, edit.start_byte)? else {
                        unreachable!()
                    };
                    let RegionCoord::Neovim(b) = self.encode_coord(&sim, edit.end_byte)? else {
                        unreachable!()
                    };
                    if edit.start_byte == edit.end_byte {
                        cmds.push(NeovimCommand::Cursor(a));
                        cmds.push(NeovimCommand::Insert(Text::from_str(&edit.text)));
                    } else {
                        cmds.push(NeovimCommand::SetText {
                            start: a,
                            end: b,
                            text: Text::from_str(&edit.text),
                        });
                    }
                    if edit.start_byte <= edit.end_byte {
                        let mut next = sim.as_str().to_owned();
                        next.replace_range(edit.start_byte..edit.end_byte, &edit.text);
                        sim = Text::from_str(&next);
                    }
                }
                cmds.push(NeovimCommand::EndUndoBlock);
                commands::lower_neovim(snapshot, context, &cmds)
            }
            AdapterKind::Agent => {
                let mut cmds = vec![AgentCommand::BeginBatch];
                for edit in edits {
                    let RegionCoord::Agent(a) = self.encode_coord(&sim, edit.start_byte)? else {
                        unreachable!()
                    };
                    let RegionCoord::Agent(b) = self.encode_coord(&sim, edit.end_byte)? else {
                        unreachable!()
                    };
                    cmds.push(AgentCommand::Replace {
                        start: a,
                        end: b,
                        text: Text::from_str(&edit.text),
                    });
                    if edit.start_byte <= edit.end_byte {
                        let mut next = sim.as_str().to_owned();
                        next.replace_range(edit.start_byte..edit.end_byte, &edit.text);
                        sim = Text::from_str(&next);
                    }
                }
                cmds.push(AgentCommand::EndBatch);
                commands::lower_agent(snapshot, context, &cmds)
            }
        }
    }

    fn lower_exclusive_select(
        &self,
        snapshot: &WorkspaceSnapshot,
        context: Context,
        anchor_byte: usize,
        head_byte: usize,
    ) -> Result<Vec<Request>, BoundaryError> {
        let (anchor, head) = match snapshot.catalog().get(context.buffer) {
            Some(text) => (
                self.encode_coord(text, anchor_byte)?,
                self.encode_coord(text, head_byte)?,
            ),
            None => (self.fallback_zero_coord(), self.fallback_zero_coord()),
        };
        match (self.adapter, anchor, head) {
            (AdapterKind::Emacs, RegionCoord::Emacs(a), RegionCoord::Emacs(h)) => {
                commands::lower_emacs(
                    snapshot,
                    context,
                    &[
                        EmacsCommand::Mark(a),
                        EmacsCommand::Point(h),
                        EmacsCommand::SelectRegion,
                    ],
                )
            }
            (AdapterKind::Neovim, RegionCoord::Neovim(a), RegionCoord::Neovim(h)) => {
                commands::lower_neovim(
                    snapshot,
                    context,
                    &[NeovimCommand::Select { anchor: a, head: h }],
                )
            }
            (AdapterKind::Agent, RegionCoord::Agent(a), RegionCoord::Agent(h)) => {
                commands::lower_agent(
                    snapshot,
                    context,
                    &[AgentCommand::Select { anchor: a, head: h }],
                )
            }
            _ => unreachable!("coords match adapter"),
        }
    }

    fn after_edit_commit(&mut self, receipt: CommitReceipt, sequence_step: Option<SequenceStep>) {
        let after = self.coordinator.snapshot();
        let event = self
            .coordinator
            .workspace()
            .delivery()
            .events()
            .last()
            .expect("edit appends delivery event")
            .clone();
        self.session
            .advance(&event)
            .expect("session advances with committed delivery event");
        let group = self
            .group_plan
            .project(SubmitOutcome::Committed(receipt), sequence_step)
            .expect("valid group projection")
            .expect("committed outcome creates change group");
        // Verify adapter-specific group plans are populated consistently.
        match self.adapter {
            AdapterKind::Emacs => {
                let plan = group.emacs();
                assert!(
                    plan.boundary_before
                        && plan.atomic_change_group
                        && plan.prepare_each_buffer_and_combine
                        && plan.amalgamate_before_accept
                        && plan.boundary_after
                );
            }
            AdapterKind::Neovim => {
                let plan = group.neovim();
                assert!(
                    plan.new_undo_block_per_buffer
                        && plan.join_subsequent_edits_in_same_buffer
                        && plan.map_undo_blocks_to_group_id
                        && plan.coordinate_multi_buffer_undo_redo
                        && plan.restore_endofline_on_undo_redo
                );
            }
            AdapterKind::Agent => {
                assert_eq!(group.id, receipt);
            }
        }
        self.receipts.push(receipt);

        for client in self.clients.values_mut() {
            let notification = client
                .resync
                .delivery(&event)
                .expect("continuous resync consumes committed event");
            match notification {
                Some(Notification::Change(formats)) => {
                    if let Some(native) = &mut client.native {
                        native
                            .apply_change(&formats)
                            .expect("adapter delta applies to native snapshot");
                    }
                }
                None => {
                    if let Some(native) = &mut client.native {
                        native.bump_revision(after.revision());
                    }
                }
                Some(Notification::Closed { .. }) => {
                    client.native = None;
                }
            }
            if !client.lagging_paused {
                client
                    .lagging
                    .catch_up_projected(
                        &CatchUp::Events(vec![event.clone()]),
                        &mut client.lagging_projections,
                    )
                    .expect("unpaused lagging client advances");
            }
        }
    }

    fn after_open_commit(&mut self, receipt: CommitReceipt, buffer: BufferId) {
        let after = self.coordinator.snapshot();
        self.session
            .open(after.clone(), buffer)
            .expect("session opens buffer");
        self.group_plan
            .project(SubmitOutcome::Committed(receipt), None)
            .expect("open group projection");
        self.receipts.push(receipt);

        for client in self.clients.values_mut() {
            client
                .resync
                .resync(&after)
                .expect("continuous client resyncs across open");
            if let Some(native) = &mut client.native {
                native.bump_revision(after.revision());
            }
            if !client.lagging_paused {
                client
                    .lagging
                    .catch_up_projected(
                        &CatchUp::Snapshot(after.clone()),
                        &mut client.lagging_projections,
                    )
                    .expect("unpaused lagging client catches up across open");
            }
        }
        let resync = Resync::new(&after, buffer).expect("resync for opened buffer");
        let native =
            NativeSnapshot::encode(self.adapter, &after, buffer).expect("encode opened buffer");
        let lagging = Resync::new(&after, buffer).expect("lagging client for opened buffer");
        let (_, lagging_projections) =
            Subscription::subscribe(&after, buffer).expect("subscribe opened buffer");
        self.clients.insert(
            buffer,
            BufferClient {
                resync,
                native: Some(native),
                lagging,
                lagging_projections,
                lagging_paused: false,
            },
        );
    }

    fn after_close_commit(&mut self, receipt: CommitReceipt, buffer: BufferId) {
        let after = self.coordinator.snapshot();
        self.session
            .close(after.clone(), buffer)
            .expect("session closes buffer");
        self.annotation_keys.retain(|(_, key)| key.buffer != buffer);
        self.group_plan
            .project(SubmitOutcome::Committed(receipt), None)
            .expect("close group projection");
        self.receipts.push(receipt);

        for (&id, client) in &mut self.clients {
            client
                .resync
                .resync(&after)
                .expect("continuous client resyncs across close");
            if id == buffer {
                assert!(client.resync.is_closed());
                client.native = None;
            } else if let Some(native) = &mut client.native {
                native.bump_revision(after.revision());
            }
            if !client.lagging_paused {
                client
                    .lagging
                    .catch_up_projected(
                        &CatchUp::Snapshot(after.clone()),
                        &mut client.lagging_projections,
                    )
                    .expect("unpaused lagging client catches up across close");
            }
        }
    }

    fn rebuild_session_for_registry(&mut self) {
        let snapshot = self.coordinator.snapshot();
        let registry = self.coordinator.workspace().actors().clone();
        let mut next =
            WorkspaceSession::new(snapshot.clone(), registry.clone(), self.policy.clone());
        let live = snapshot.catalog().live_ids();
        for (actor, _) in registry.active_actors() {
            for &buffer in &live {
                if let Ok(Some(view)) = self.session.view(actor, buffer) {
                    let _ = next.set_view(actor, view);
                }
                if let Ok(Some(selection)) = self.session.selection(actor, buffer) {
                    let _ = next.set_selection(actor, selection);
                }
            }
        }
        let active_keys: Vec<_> = self.annotation_keys.iter().copied().collect();
        for (actor, key) in active_keys {
            if registry.is_active(actor) {
                if let Ok(Some(annotation)) = self.session.overlay(actor, key) {
                    let _ = next.set_overlay(actor, annotation.clone());
                }
            }
        }
        self.session = next;
    }

    fn execute_edit_request(
        &mut self,
        request: &Request,
        req_id: RequestId,
        nonce: Nonce,
        authorize_against_current: bool,
        sequence_step: Option<SequenceStep>,
        tag: Option<&str>,
    ) -> Result<StepOutcome, RejectionReason> {
        let current = self.coordinator.snapshot();
        if authorize_against_current {
            request
                .authorize(
                    &self.policy,
                    self.coordinator.workspace().actors(),
                    &current,
                )
                .map_err(|e| classify_request_error(&e, self.coordinator.workspace().actors()))?;
        }
        let (_, crate::editor::collab::lifecycle::Operation::Edit(txn)) = request
            .lifecycle_operation(nonce)
            .ok_or(RejectionReason::UnsupportedOperation)?
        else {
            return Err(RejectionReason::UnsupportedOperation);
        };
        let txn_id = txn.intent().txn();
        let op = CoordinatorOperation::Edit(txn);
        let outcome = self
            .coordinator
            .apply(req_id, op.clone())
            .map_err(|e| classify_coordinator_error(&e, self.coordinator.workspace().actors()))?;
        let receipt = outcome.receipt.expect("edit produces receipt");
        self.after_edit_commit(receipt, sequence_step);
        if let Some(tag) = tag {
            self.tagged_commits.insert(
                tag.to_owned(),
                RecordedCommit {
                    request: req_id,
                    operation: op,
                    txn: txn_id,
                },
            );
        }
        Ok(StepOutcome::Committed {
            receipt,
            buffer: outcome.buffer,
        })
    }

    fn run_step(&mut self, step: usize, op: &ScenarioOp) -> Result<(), ConformanceFailure> {
        let before = self.stamp();
        let req_id = RequestId::new(1000 + step as u64);
        let nonce = Nonce::new(1000 + step as u64);
        let result = self.execute_op(op, req_id, nonce);
        match result {
            Ok(outcome) => {
                self.outcomes.push(outcome);
                Ok(())
            }
            Err(reason) => {
                #[cfg(test)]
                if let Some(hook) = self.after_reject_hook.take() {
                    hook(self);
                }
                let after = self.stamp();
                if after != before {
                    return Err(ConformanceFailure {
                        divergences: vec![ConformanceDivergence::RejectionStateDrift {
                            adapter: self.adapter,
                            step,
                        }],
                    });
                }
                self.outcomes.push(StepOutcome::Rejected {
                    reason,
                    revision: self.coordinator.snapshot().revision(),
                });
                Ok(())
            }
        }
    }

    fn execute_op(
        &mut self,
        op: &ScenarioOp,
        req_id: RequestId,
        nonce: Nonce,
    ) -> Result<StepOutcome, RejectionReason> {
        match op {
            ScenarioOp::OpenBuffer {
                actor,
                name,
                text,
                base_override,
                tag,
            } => {
                let current = self.coordinator.snapshot();
                let requested_rev = base_override.unwrap_or_else(|| current.revision());
                let envelope = self.envelope(Input {
                    actor: Some(*actor),
                    revision: Some(requested_rev),
                    target: Some(Target::Name(name.clone())),
                    operation: InputOperation::Open(Text::from_str(text)),
                });
                let request = envelope
                    .normalize(&current)
                    .map_err(|e| classify_boundary_error(&e))?;
                let (intent, crate::editor::collab::lifecycle::Operation::Open { name, text }) =
                    request
                        .lifecycle_operation(nonce)
                        .ok_or(RejectionReason::UnsupportedOperation)?
                else {
                    return Err(RejectionReason::UnsupportedOperation);
                };
                let coord_op = CoordinatorOperation::Open { intent, name, text };
                let outcome = self
                    .coordinator
                    .apply(req_id, coord_op.clone())
                    .map_err(|e| {
                        classify_coordinator_error(&e, self.coordinator.workspace().actors())
                    })?;
                let receipt = outcome.receipt.expect("open produces receipt");
                let buffer = outcome.buffer.expect("open produces buffer id");
                self.after_open_commit(receipt, buffer);
                if let Some(tag) = tag {
                    self.tagged_commits.insert(
                        tag.clone(),
                        RecordedCommit {
                            request: req_id,
                            operation: coord_op,
                            txn: intent.txn(),
                        },
                    );
                }
                Ok(StepOutcome::Committed {
                    receipt,
                    buffer: Some(buffer),
                })
            }

            ScenarioOp::CloseBuffer {
                actor,
                buffer,
                policy,
                base_override,
                tag,
            } => {
                let current = self.coordinator.snapshot();
                let requested_rev = base_override.unwrap_or_else(|| current.revision());
                let envelope = self.envelope(Input {
                    actor: Some(*actor),
                    revision: Some(requested_rev),
                    target: Some(Target::Buffer(*buffer)),
                    operation: InputOperation::Close(*policy),
                });
                let request = envelope
                    .normalize(&current)
                    .map_err(|e| classify_boundary_error(&e))?;
                let (
                    intent,
                    crate::editor::collab::lifecycle::Operation::Close {
                        buffer: target_buf,
                        policy: close_policy,
                    },
                ) = request
                    .lifecycle_operation(nonce)
                    .ok_or(RejectionReason::UnsupportedOperation)?
                else {
                    return Err(RejectionReason::UnsupportedOperation);
                };
                let coord_op = CoordinatorOperation::Close {
                    intent,
                    buffer: target_buf,
                    policy: close_policy,
                };
                let outcome = self
                    .coordinator
                    .apply(req_id, coord_op.clone())
                    .map_err(|e| {
                        classify_coordinator_error(&e, self.coordinator.workspace().actors())
                    })?;
                let receipt = outcome.receipt.expect("close produces receipt");
                self.after_close_commit(receipt, target_buf);
                if let Some(tag) = tag {
                    self.tagged_commits.insert(
                        tag.clone(),
                        RecordedCommit {
                            request: req_id,
                            operation: coord_op,
                            txn: intent.txn(),
                        },
                    );
                }
                Ok(StepOutcome::Committed {
                    receipt,
                    buffer: Some(target_buf),
                })
            }

            ScenarioOp::ReadBuffer { actor, buffer } => {
                let current = self.coordinator.snapshot();
                let envelope = self.envelope(Input {
                    actor: Some(*actor),
                    revision: Some(current.revision()),
                    target: Some(Target::Buffer(*buffer)),
                    operation: InputOperation::Read,
                });
                let request = envelope
                    .normalize(&current)
                    .map_err(|e| classify_boundary_error(&e))?;
                request
                    .authorize(
                        &self.policy,
                        self.coordinator.workspace().actors(),
                        &current,
                    )
                    .map_err(|e| {
                        classify_request_error(&e, self.coordinator.workspace().actors())
                    })?;
                let session_text = self.session.read(*actor, *buffer).map_err(|e| {
                    classify_session_error(&e, self.coordinator.workspace().actors())
                })?;
                let encoded = NativeSnapshot::encode(self.adapter, &current, *buffer)
                    .map_err(|e| classify_boundary_error(&e))?;
                let (rev, decoded) = encoded.decode().map_err(|e| classify_boundary_error(&e))?;
                assert_eq!(rev, current.revision());
                assert_eq!(decoded, session_text.as_str());
                Ok(StepOutcome::Applied { revision: rev })
            }

            ScenarioOp::Insert {
                actor,
                buffer,
                byte_offset,
                text,
                tag,
            } => {
                let current = self.coordinator.snapshot();
                let requests = self
                    .lower_single_insert(
                        &current,
                        Context {
                            actor: *actor,
                            buffer: *buffer,
                        },
                        *byte_offset,
                        text,
                    )
                    .map_err(|e| classify_boundary_error(&e))?;
                assert_eq!(requests.len(), 1);
                self.execute_edit_request(&requests[0], req_id, nonce, true, None, tag.as_deref())
            }

            ScenarioOp::Replace {
                actor,
                buffer,
                start_byte,
                end_byte,
                text,
                reverse_emacs_mark,
                tag,
            } => {
                let current = self.coordinator.snapshot();
                let requests = self
                    .lower_single_replace(
                        &current,
                        Context {
                            actor: *actor,
                            buffer: *buffer,
                        },
                        *start_byte,
                        *end_byte,
                        text,
                        *reverse_emacs_mark,
                    )
                    .map_err(|e| classify_boundary_error(&e))?;
                assert_eq!(requests.len(), 1);
                self.execute_edit_request(&requests[0], req_id, nonce, true, None, tag.as_deref())
            }

            ScenarioOp::ChangeGroup {
                actor,
                buffer,
                edits,
                sequence_step,
                tag,
            } => {
                let current = self.coordinator.snapshot();
                let requests = self
                    .lower_change_group(
                        &current,
                        Context {
                            actor: *actor,
                            buffer: *buffer,
                        },
                        edits,
                    )
                    .map_err(|e| classify_boundary_error(&e))?;
                assert_eq!(requests.len(), 1);
                self.execute_edit_request(
                    &requests[0],
                    req_id,
                    nonce,
                    true,
                    *sequence_step,
                    tag.as_deref(),
                )
            }

            ScenarioOp::MultiBufferBatch {
                actor,
                primary,
                edits,
                mode,
                tag,
            } => {
                let current = self.coordinator.snapshot();
                let mut batch = Vec::with_capacity(edits.len());
                for edit in edits {
                    let region = self
                        .encode_region(&current, edit.buffer, edit.start_byte, edit.end_byte, *mode)
                        .map_err(|e| classify_boundary_error(&e))?;
                    batch.push(BatchEdit {
                        buffer: edit.buffer,
                        replacement: Replacement {
                            region,
                            text: Text::from_str(&edit.text),
                        },
                    });
                }
                let envelope = self.envelope(Input {
                    actor: Some(*actor),
                    revision: Some(current.revision()),
                    target: Some(Target::Buffer(*primary)),
                    operation: InputOperation::MultiBatch(batch),
                });
                let request = envelope
                    .normalize(&current)
                    .map_err(|e| classify_boundary_error(&e))?;
                self.execute_edit_request(&request, req_id, nonce, true, None, tag.as_deref())
            }

            ScenarioOp::NarrowView {
                actor,
                buffer,
                start_byte,
                end_byte,
            } => {
                let current = self.coordinator.snapshot();
                let region = self
                    .encode_region(
                        &current,
                        *buffer,
                        *start_byte,
                        *end_byte,
                        EndpointMode::Exclusive,
                    )
                    .map_err(|e| classify_boundary_error(&e))?;
                let envelope = self.envelope(Input {
                    actor: Some(*actor),
                    revision: Some(current.revision()),
                    target: Some(Target::Buffer(*buffer)),
                    operation: InputOperation::Region(region),
                });
                let request = envelope
                    .normalize(&current)
                    .map_err(|e| classify_boundary_error(&e))?;
                request
                    .authorize(
                        &self.policy,
                        self.coordinator.workspace().actors(),
                        &current,
                    )
                    .map_err(|e| {
                        classify_request_error(&e, self.coordinator.workspace().actors())
                    })?;
                let RequestOperation::Region(sel) = request.operation() else {
                    return Err(RejectionReason::UnsupportedOperation);
                };
                let view = BufferView::narrowed(current.catalog(), *buffer, sel.range())
                    .map_err(|_| RejectionReason::InvalidSpan)?;
                self.session.set_view(*actor, view).map_err(|e| {
                    classify_session_error(&e, self.coordinator.workspace().actors())
                })?;
                Ok(StepOutcome::Applied {
                    revision: current.revision(),
                })
            }

            ScenarioOp::Select {
                actor,
                buffer,
                anchor_byte,
                head_byte,
                mode,
            } => {
                let current = self.coordinator.snapshot();
                let request = match mode {
                    EndpointMode::Exclusive => {
                        let requests = self
                            .lower_exclusive_select(
                                &current,
                                Context {
                                    actor: *actor,
                                    buffer: *buffer,
                                },
                                *anchor_byte,
                                *head_byte,
                            )
                            .map_err(|e| classify_boundary_error(&e))?;
                        assert_eq!(requests.len(), 1);
                        requests.into_iter().next().expect("one selection request")
                    }
                    EndpointMode::Inclusive => {
                        let region = self
                            .encode_region(&current, *buffer, *anchor_byte, *head_byte, *mode)
                            .map_err(|e| classify_boundary_error(&e))?;
                        let envelope = self.envelope(Input {
                            actor: Some(*actor),
                            revision: Some(current.revision()),
                            target: Some(Target::Buffer(*buffer)),
                            operation: InputOperation::Region(region),
                        });
                        envelope
                            .normalize(&current)
                            .map_err(|e| classify_boundary_error(&e))?
                    }
                };
                request
                    .authorize(
                        &self.policy,
                        self.coordinator.workspace().actors(),
                        &current,
                    )
                    .map_err(|e| {
                        classify_request_error(&e, self.coordinator.workspace().actors())
                    })?;
                let RequestOperation::Region(selection) = request.operation() else {
                    return Err(RejectionReason::UnsupportedOperation);
                };
                let existing_view = self.session.view(*actor, *buffer).map_err(|e| {
                    classify_session_error(&e, self.coordinator.workspace().actors())
                })?;
                let view = if let Some(view) = existing_view {
                    view
                } else {
                    let full = BufferView::full(current.catalog(), *buffer)
                        .map_err(|_| RejectionReason::UnknownBuffer)?;
                    self.session.set_view(*actor, full).map_err(|e| {
                        classify_session_error(&e, self.coordinator.workspace().actors())
                    })?;
                    full
                };
                // Validate against active view (which may be narrowed) through adapter's OrientedRegion.
                let encoded = OrientedRegion::from_selection(
                    current.catalog(),
                    &view,
                    *selection,
                    self.coord_kind(),
                    EndpointMode::Exclusive,
                )
                .map_err(|e| classify_boundary_error(&e))?;
                let roundtripped = encoded
                    .to_selection(current.catalog(), &view, *buffer)
                    .map_err(|e| classify_boundary_error(&e))?;
                self.session
                    .set_selection(*actor, roundtripped)
                    .map_err(|e| {
                        classify_session_error(&e, self.coordinator.workspace().actors())
                    })?;
                Ok(StepOutcome::Selected {
                    actor: *actor,
                    buffer: *buffer,
                    selection: NormalizedSelection {
                        anchor: roundtripped.anchor().as_usize(),
                        head: roundtripped.head().as_usize(),
                        direction: roundtripped.direction(),
                    },
                })
            }

            ScenarioOp::SetAnnotation {
                actor,
                key,
                start_byte,
                end_byte,
                start_affinity,
                end_affinity,
                priority,
                collapse,
                label,
            } => {
                let current = self.coordinator.snapshot();
                self.policy
                    .authorize(
                        self.coordinator.workspace().actors(),
                        current.catalog(),
                        *actor,
                        Capability::Read,
                        [key.buffer],
                    )
                    .map_err(|e| {
                        classify_access_error(&e, self.coordinator.workspace().actors())
                    })?;
                let text = current
                    .catalog()
                    .get(key.buffer)
                    .ok_or(RejectionReason::UnknownBuffer)?;
                let span =
                    ByteSpan::validate_in(text, *start_byte, *end_byte).map_err(|e| match e {
                        crate::editor::text::span::SpanError::NotCharBoundary { .. } => {
                            RejectionReason::InvalidBoundary
                        }
                        crate::editor::text::span::SpanError::OutOfBounds { .. } => {
                            RejectionReason::OutOfRange
                        }
                        crate::editor::text::span::SpanError::InvertedRange { .. } => {
                            RejectionReason::InvalidSpan
                        }
                    })?;
                let core = Annotation {
                    key: *key,
                    owner: *actor,
                    span,
                    start_affinity: *start_affinity,
                    end_affinity: *end_affinity,
                    priority: *priority,
                    collapse: *collapse,
                    label: label.clone(),
                };
                // Roundtrip through the real adapter-specific overlay/extmark/annotation type.
                let roundtripped = self
                    .roundtrip_annotation(text, &core)
                    .map_err(|e| classify_boundary_error(&e))?;
                self.session
                    .set_overlay(*actor, roundtripped.clone())
                    .map_err(|e| {
                        classify_session_error(&e, self.coordinator.workspace().actors())
                    })?;
                self.annotation_keys.insert((*actor, *key));
                Ok(StepOutcome::Annotated {
                    actor: *actor,
                    key: *key,
                    annotation: NormalizedAnnotation {
                        start: roundtripped.span.start().as_usize(),
                        end: roundtripped.span.end().as_usize(),
                        start_affinity: roundtripped.start_affinity,
                        end_affinity: roundtripped.end_affinity,
                        priority: roundtripped.priority,
                        collapse: roundtripped.collapse,
                        label: roundtripped.label,
                    },
                })
            }

            ScenarioOp::SaveSnapshot { tag } => {
                let snap = self.coordinator.snapshot();
                let rev = snap.revision();
                self.saved_snapshots.insert(tag.clone(), snap);
                Ok(StepOutcome::Applied { revision: rev })
            }

            ScenarioOp::EditAtSavedBase {
                base_tag,
                actor,
                buffer,
                start_byte,
                end_byte,
                text,
                tag,
            } => {
                let base_snapshot = self
                    .saved_snapshots
                    .get(base_tag)
                    .cloned()
                    .ok_or(RejectionReason::InvalidCoordinate)?;
                let requests = self
                    .lower_single_replace(
                        &base_snapshot,
                        Context {
                            actor: *actor,
                            buffer: *buffer,
                        },
                        *start_byte,
                        *end_byte,
                        text,
                        false,
                    )
                    .map_err(|e| classify_boundary_error(&e))?;
                assert_eq!(requests.len(), 1);
                // Let Coordinator enforce authorization, retention floor, and rebase conflict checks.
                self.execute_edit_request(&requests[0], req_id, nonce, false, None, tag.as_deref())
            }

            ScenarioOp::ProposeEdit {
                proposal_tag,
                actor,
                buffer,
                start_byte,
                end_byte,
                text,
            } => {
                let current = self.coordinator.snapshot();
                let region = self
                    .encode_region(
                        &current,
                        *buffer,
                        *start_byte,
                        *end_byte,
                        EndpointMode::Exclusive,
                    )
                    .map_err(|e| classify_boundary_error(&e))?;
                let envelope = self.envelope(Input {
                    actor: Some(*actor),
                    revision: Some(current.revision()),
                    target: Some(Target::Buffer(*buffer)),
                    operation: InputOperation::Edit(Replacement {
                        region,
                        text: Text::from_str(text),
                    }),
                });
                let request = envelope
                    .normalize(&current)
                    .map_err(|e| classify_boundary_error(&e))?;
                let proposal = request
                    .proposal(req_id, nonce)
                    .ok_or(RejectionReason::EmptyProposal)?;
                self.coordinator.propose(proposal).map_err(|e| {
                    classify_coordinator_error(&e, self.coordinator.workspace().actors())
                })?;
                self.proposal_requests.insert(proposal_tag.clone(), req_id);
                Ok(StepOutcome::Proposed { request: req_id })
            }

            ScenarioOp::ApproveProposal {
                proposal_tag,
                reviewer,
            } => {
                let request = self
                    .proposal_requests
                    .get(proposal_tag)
                    .copied()
                    .unwrap_or(req_id);
                self.coordinator.approve(request, *reviewer).map_err(|e| {
                    classify_coordinator_error(&e, self.coordinator.workspace().actors())
                })?;
                Ok(StepOutcome::Approved {
                    request,
                    reviewer: *reviewer,
                })
            }

            ScenarioOp::CommitProposal { proposal_tag, tag } => {
                let request = self
                    .proposal_requests
                    .get(proposal_tag)
                    .copied()
                    .unwrap_or(req_id);
                let outcome: CheckpointOutcome =
                    self.coordinator.commit_proposal(request).map_err(|e| {
                        classify_coordinator_error(&e, self.coordinator.workspace().actors())
                    })?;
                let receipt = outcome
                    .receipt
                    .expect("committed proposal produces receipt");
                // Avoid double-advancing on idempotent commit_proposal retries.
                if !self.receipts.contains(&receipt) {
                    self.after_edit_commit(receipt, None);
                }
                if let Some(tag) = tag {
                    self.tagged_commits.insert(
                        tag.clone(),
                        RecordedCommit {
                            request,
                            operation: CoordinatorOperation::Edit(
                                Transaction::new(receipt.intent(), Vec::new())
                                    .expect("empty placeholder"),
                            ),
                            txn: receipt.txn(),
                        },
                    );
                }
                Ok(StepOutcome::Committed {
                    receipt,
                    buffer: outcome.buffer,
                })
            }

            ScenarioOp::Undo {
                actor,
                target_tag,
                use_resolved_candidate,
                tag,
            } => {
                let target_txn = self
                    .tagged_commits
                    .get(target_tag)
                    .map(|c| c.txn)
                    .unwrap_or_else(|| TxnId::new(*actor, Nonce::new(999_999)));
                let op_txn = TxnId::new(*actor, nonce);
                let candidates = [target_txn];
                let target_ref = if *use_resolved_candidate {
                    UndoTarget::Resolved(&candidates)
                } else {
                    UndoTarget::Explicit(target_txn)
                };
                let cmd = match self.adapter {
                    AdapterKind::Emacs => FrontendCommand::EmacsUndoOnly(target_ref),
                    AdapterKind::Neovim => FrontendCommand::NeovimUndo(target_ref),
                    AdapterKind::Agent => FrontendCommand::AgentUndo(target_txn),
                };
                let CoreRequest::Undo(core_req) =
                    undo::normalize(*actor, op_txn, cmd).map_err(|e| classify_undo_error(&e))?
                else {
                    return Err(RejectionReason::InvalidUndoTransaction);
                };
                let coord_op = CoordinatorOperation::Undo {
                    actor: core_req.actor,
                    target: core_req.target,
                    transaction: core_req.operation,
                };
                let outcome = self
                    .coordinator
                    .apply(req_id, coord_op.clone())
                    .map_err(|e| {
                        classify_coordinator_error(&e, self.coordinator.workspace().actors())
                    })?;
                let receipt = outcome.receipt.expect("undo produces receipt");
                self.after_edit_commit(receipt, None);
                if let Some(tag) = tag {
                    self.tagged_commits.insert(
                        tag.clone(),
                        RecordedCommit {
                            request: req_id,
                            operation: coord_op,
                            txn: op_txn,
                        },
                    );
                }
                Ok(StepOutcome::Committed {
                    receipt,
                    buffer: outcome.buffer,
                })
            }

            ScenarioOp::Redo {
                actor,
                target_tag,
                use_resolved_candidate,
                tag,
            } => {
                let target_txn = self
                    .tagged_commits
                    .get(target_tag)
                    .map(|c| c.txn)
                    .unwrap_or_else(|| TxnId::new(*actor, Nonce::new(999_999)));
                let op_txn = TxnId::new(*actor, nonce);
                let candidates = [target_txn];
                let target_ref = if *use_resolved_candidate {
                    UndoTarget::Resolved(&candidates)
                } else {
                    UndoTarget::Explicit(target_txn)
                };
                let cmd = match self.adapter {
                    AdapterKind::Emacs => FrontendCommand::EmacsUndoRedo(target_ref),
                    AdapterKind::Neovim => FrontendCommand::NeovimRedo(target_ref),
                    AdapterKind::Agent => FrontendCommand::AgentRedo(target_txn),
                };
                let CoreRequest::Redo(core_req) =
                    undo::normalize(*actor, op_txn, cmd).map_err(|e| classify_undo_error(&e))?
                else {
                    return Err(RejectionReason::InvalidUndoTransaction);
                };
                let coord_op = CoordinatorOperation::Redo {
                    actor: core_req.actor,
                    target: core_req.target,
                    transaction: core_req.operation,
                };
                let outcome = self
                    .coordinator
                    .apply(req_id, coord_op.clone())
                    .map_err(|e| {
                        classify_coordinator_error(&e, self.coordinator.workspace().actors())
                    })?;
                let receipt = outcome.receipt.expect("redo produces receipt");
                self.after_edit_commit(receipt, None);
                if let Some(tag) = tag {
                    self.tagged_commits.insert(
                        tag.clone(),
                        RecordedCommit {
                            request: req_id,
                            operation: coord_op,
                            txn: op_txn,
                        },
                    );
                }
                Ok(StepOutcome::Committed {
                    receipt,
                    buffer: outcome.buffer,
                })
            }

            ScenarioOp::RetryCommitted { tag } => {
                let recorded = self
                    .tagged_commits
                    .get(tag)
                    .cloned()
                    .ok_or(RejectionReason::InvalidCoordinate)?;
                let outcome = self
                    .coordinator
                    .apply(recorded.request, recorded.operation)
                    .map_err(|e| {
                        classify_coordinator_error(&e, self.coordinator.workspace().actors())
                    })?;
                let receipt = outcome.receipt.expect("retry returns original receipt");
                // Idempotent redelivery through GroupPlan leaves plan unchanged.
                let group = self
                    .group_plan
                    .project(SubmitOutcome::Committed(receipt), None)
                    .expect("idempotent group projection")
                    .expect("existing group");
                assert_eq!(group.id, receipt);
                Ok(StepOutcome::Committed {
                    receipt,
                    buffer: outcome.buffer,
                })
            }

            ScenarioOp::PauseLagging { buffer } => {
                let client = self
                    .clients
                    .get_mut(buffer)
                    .ok_or(RejectionReason::UnknownBuffer)?;
                client.lagging_paused = true;
                Ok(StepOutcome::Applied {
                    revision: self.coordinator.snapshot().revision(),
                })
            }

            ScenarioOp::DeliverLatestToLagging { buffer } => {
                let latest = self
                    .coordinator
                    .workspace()
                    .delivery()
                    .events()
                    .last()
                    .cloned()
                    .ok_or(RejectionReason::InvalidEvent)?;
                let client = self
                    .clients
                    .get_mut(buffer)
                    .ok_or(RejectionReason::UnknownBuffer)?;
                client
                    .lagging
                    .catch_up_projected(
                        &CatchUp::Events(vec![latest]),
                        &mut client.lagging_projections,
                    )
                    .map_err(|e| classify_resync_error(&e))?;
                Ok(StepOutcome::Applied {
                    revision: client.lagging.revision(),
                })
            }

            ScenarioOp::Compact {
                floor_offset_from_tip,
            } => {
                let tip = self.coordinator.snapshot().revision().raw();
                let floor = Revision::new(tip.saturating_sub(*floor_offset_from_tip));
                self.coordinator.compact(floor).map_err(|e| {
                    classify_coordinator_error(&e, self.coordinator.workspace().actors())
                })?;
                Ok(StepOutcome::Applied {
                    revision: self.coordinator.snapshot().revision(),
                })
            }

            ScenarioOp::CatchUpLagging { buffer } => {
                let lagging_rev = self
                    .clients
                    .get(buffer)
                    .ok_or(RejectionReason::UnknownBuffer)?
                    .lagging
                    .revision();
                let catch_up = self
                    .coordinator
                    .workspace()
                    .delivery()
                    .catch_up(lagging_rev)
                    .map_err(|e| classify_delivery_error(&e))?;
                let client = self
                    .clients
                    .get_mut(buffer)
                    .ok_or(RejectionReason::UnknownBuffer)?;
                client
                    .lagging
                    .catch_up_projected(&catch_up, &mut client.lagging_projections)
                    .map_err(|e| classify_resync_error(&e))?;
                client.lagging_paused = false;
                Ok(StepOutcome::Applied {
                    revision: client.lagging.revision(),
                })
            }

            ScenarioOp::RecoverViaRequest {
                actor,
                buffer,
                observed_tag,
            } => {
                let observed = self
                    .saved_snapshots
                    .get(observed_tag)
                    .cloned()
                    .ok_or(RejectionReason::InvalidCoordinate)?;
                let current = self.coordinator.snapshot();
                let envelope = self.envelope(Input {
                    actor: Some(*actor),
                    revision: Some(observed.revision()),
                    target: Some(Target::Buffer(*buffer)),
                    operation: InputOperation::Resync,
                });
                let request = envelope
                    .normalize(&observed)
                    .map_err(|e| classify_boundary_error(&e))?;
                let recovery = request
                    .recover(
                        &self.policy,
                        self.coordinator.workspace().actors(),
                        &observed,
                        &current,
                    )
                    .map_err(|e| {
                        classify_request_error(&e, self.coordinator.workspace().actors())
                    })?;
                match recovery {
                    Recovery::Live(agent_snap) => {
                        assert_eq!(agent_snap.revision, current.revision());
                        assert_eq!(
                            agent_snap.text,
                            current
                                .catalog()
                                .get(*buffer)
                                .expect("live buffer")
                                .as_str()
                        );
                        Ok(StepOutcome::Applied {
                            revision: agent_snap.revision,
                        })
                    }
                    Recovery::Closed {
                        buffer: closed_buf,
                        revision,
                    } => {
                        assert_eq!(closed_buf, *buffer);
                        assert_eq!(revision, current.revision());
                        Ok(StepOutcome::Applied { revision })
                    }
                }
            }

            ScenarioOp::UpdatePolicy(change) => {
                match *change {
                    PolicyChange::GrantDefault(cap) => {
                        self.coordinator.access_mut().grant_default(cap);
                        self.policy.grant_default(cap);
                    }
                    PolicyChange::RevokeDefault(cap) => {
                        self.coordinator.access_mut().revoke_default(cap);
                        self.policy.revoke_default(cap);
                    }
                    PolicyChange::GrantActor(actor, cap) => {
                        self.coordinator.access_mut().grant_actor(actor, cap);
                        self.policy.grant_actor(actor, cap);
                    }
                    PolicyChange::RevokeActor(actor, cap) => {
                        self.coordinator.access_mut().revoke_actor(actor, cap);
                        self.policy.revoke_actor(actor, cap);
                    }
                    PolicyChange::GrantBuffer(actor, buffer, cap) => {
                        self.coordinator
                            .access_mut()
                            .grant_buffer(actor, buffer, cap);
                        self.policy.grant_buffer(actor, buffer, cap);
                    }
                    PolicyChange::RevokeBuffer(actor, buffer, cap) => {
                        self.coordinator
                            .access_mut()
                            .revoke_buffer(actor, buffer, cap);
                        self.policy.revoke_buffer(actor, buffer, cap);
                    }
                }
                self.session.set_policy(self.policy.clone());
                Ok(StepOutcome::Applied {
                    revision: self.coordinator.snapshot().revision(),
                })
            }

            ScenarioOp::RegisterActor { actor, role } => {
                self.coordinator
                    .actors_mut()
                    .register(*actor, *role)
                    .map_err(|e| classify_registry_error(&e))?;
                self.rebuild_session_for_registry();
                Ok(StepOutcome::Applied {
                    revision: self.coordinator.snapshot().revision(),
                })
            }

            ScenarioOp::RevokeActor { actor } => {
                self.coordinator
                    .actors_mut()
                    .revoke(*actor)
                    .map_err(|e| classify_registry_error(&e))?;
                self.rebuild_session_for_registry();
                Ok(StepOutcome::Applied {
                    revision: self.coordinator.snapshot().revision(),
                })
            }

            ScenarioOp::Invalid(invalid) => self.execute_invalid(invalid, req_id, nonce),
        }
    }

    fn execute_invalid(
        &mut self,
        invalid: &InvalidOp,
        req_id: RequestId,
        nonce: Nonce,
    ) -> Result<StepOutcome, RejectionReason> {
        let current = self.coordinator.snapshot();
        match invalid {
            InvalidOp::OutOfRangePosition { actor, buffer } => {
                let text = current
                    .catalog()
                    .get(*buffer)
                    .ok_or(RejectionReason::UnknownBuffer)?;
                let bad = self.out_of_range_coord(text);
                let ctx = Context {
                    actor: *actor,
                    buffer: *buffer,
                };
                let err = match (self.adapter, bad) {
                    (AdapterKind::Emacs, RegionCoord::Emacs(pos)) => {
                        commands::lower_emacs(&current, ctx, &[EmacsCommand::Point(pos)])
                            .unwrap_err()
                    }
                    (AdapterKind::Neovim, RegionCoord::Neovim(pos)) => {
                        commands::lower_neovim(&current, ctx, &[NeovimCommand::Cursor(pos)])
                            .unwrap_err()
                    }
                    (AdapterKind::Agent, RegionCoord::Agent(pos)) => commands::lower_agent(
                        &current,
                        ctx,
                        &[AgentCommand::Select {
                            anchor: pos,
                            head: pos,
                        }],
                    )
                    .unwrap_err(),
                    _ => unreachable!(),
                };
                Err(classify_boundary_error(&err))
            }

            InvalidOp::UnsupportedCommand {
                actor,
                buffer,
                name,
            } => {
                let ctx = Context {
                    actor: *actor,
                    buffer: *buffer,
                };
                let res = match self.adapter {
                    AdapterKind::Emacs => commands::lower_emacs(
                        &current,
                        ctx,
                        &[EmacsCommand::Unsupported(name.clone())],
                    ),
                    AdapterKind::Neovim => commands::lower_neovim(
                        &current,
                        ctx,
                        &[NeovimCommand::Unsupported(name.clone())],
                    ),
                    AdapterKind::Agent => commands::lower_agent(
                        &current,
                        ctx,
                        &[AgentCommand::Unsupported(name.clone())],
                    ),
                };
                Err(classify_boundary_error(&res.unwrap_err()))
            }

            InvalidOp::NestedChangeGroup { actor, buffer } => {
                let ctx = Context {
                    actor: *actor,
                    buffer: *buffer,
                };
                let res = match self.adapter {
                    AdapterKind::Emacs => commands::lower_emacs(
                        &current,
                        ctx,
                        &[
                            EmacsCommand::BeginChangeGroup,
                            EmacsCommand::BeginChangeGroup,
                        ],
                    ),
                    AdapterKind::Neovim => commands::lower_neovim(
                        &current,
                        ctx,
                        &[NeovimCommand::BeginUndoBlock, NeovimCommand::BeginUndoBlock],
                    ),
                    AdapterKind::Agent => commands::lower_agent(
                        &current,
                        ctx,
                        &[AgentCommand::BeginBatch, AgentCommand::BeginBatch],
                    ),
                };
                Err(classify_boundary_error(&res.unwrap_err()))
            }

            InvalidOp::UnclosedChangeGroup {
                actor,
                buffer,
                text,
            } => {
                let ctx = Context {
                    actor: *actor,
                    buffer: *buffer,
                };
                let value = Text::from_str(text);
                let zero = self.fallback_zero_coord();
                let res = match (self.adapter, zero) {
                    (AdapterKind::Emacs, RegionCoord::Emacs(pos)) => commands::lower_emacs(
                        &current,
                        ctx,
                        &[
                            EmacsCommand::BeginChangeGroup,
                            EmacsCommand::Point(pos),
                            EmacsCommand::Insert(value),
                        ],
                    ),
                    (AdapterKind::Neovim, RegionCoord::Neovim(pos)) => commands::lower_neovim(
                        &current,
                        ctx,
                        &[
                            NeovimCommand::BeginUndoBlock,
                            NeovimCommand::Cursor(pos),
                            NeovimCommand::Insert(value),
                        ],
                    ),
                    (AdapterKind::Agent, RegionCoord::Agent(pos)) => commands::lower_agent(
                        &current,
                        ctx,
                        &[
                            AgentCommand::BeginBatch,
                            AgentCommand::Replace {
                                start: pos,
                                end: pos,
                                text: value,
                            },
                        ],
                    ),
                    _ => unreachable!(),
                };
                Err(classify_boundary_error(&res.unwrap_err()))
            }

            InvalidOp::EmptyChangeGroup { actor, buffer } => {
                let ctx = Context {
                    actor: *actor,
                    buffer: *buffer,
                };
                let res = match self.adapter {
                    AdapterKind::Emacs => commands::lower_emacs(
                        &current,
                        ctx,
                        &[EmacsCommand::BeginChangeGroup, EmacsCommand::EndChangeGroup],
                    ),
                    AdapterKind::Neovim => commands::lower_neovim(
                        &current,
                        ctx,
                        &[NeovimCommand::BeginUndoBlock, NeovimCommand::EndUndoBlock],
                    ),
                    AdapterKind::Agent => commands::lower_agent(
                        &current,
                        ctx,
                        &[AgentCommand::BeginBatch, AgentCommand::EndBatch],
                    ),
                };
                Err(classify_boundary_error(&res.unwrap_err()))
            }

            InvalidOp::EndWithoutBegin { actor, buffer } => {
                let ctx = Context {
                    actor: *actor,
                    buffer: *buffer,
                };
                let res = match self.adapter {
                    AdapterKind::Emacs => {
                        commands::lower_emacs(&current, ctx, &[EmacsCommand::EndChangeGroup])
                    }
                    AdapterKind::Neovim => {
                        commands::lower_neovim(&current, ctx, &[NeovimCommand::EndUndoBlock])
                    }
                    AdapterKind::Agent => {
                        commands::lower_agent(&current, ctx, &[AgentCommand::EndBatch])
                    }
                };
                Err(classify_boundary_error(&res.unwrap_err()))
            }

            InvalidOp::SelectInsideGroup { actor, buffer } => {
                let ctx = Context {
                    actor: *actor,
                    buffer: *buffer,
                };
                let zero = self.fallback_zero_coord();
                let res = match (self.adapter, zero) {
                    (AdapterKind::Emacs, RegionCoord::Emacs(pos)) => commands::lower_emacs(
                        &current,
                        ctx,
                        &[
                            EmacsCommand::BeginChangeGroup,
                            EmacsCommand::Mark(pos),
                            EmacsCommand::Point(pos),
                            EmacsCommand::SelectRegion,
                        ],
                    ),
                    (AdapterKind::Neovim, RegionCoord::Neovim(pos)) => commands::lower_neovim(
                        &current,
                        ctx,
                        &[
                            NeovimCommand::BeginUndoBlock,
                            NeovimCommand::Select {
                                anchor: pos,
                                head: pos,
                            },
                        ],
                    ),
                    (AdapterKind::Agent, RegionCoord::Agent(pos)) => commands::lower_agent(
                        &current,
                        ctx,
                        &[
                            AgentCommand::BeginBatch,
                            AgentCommand::Select {
                                anchor: pos,
                                head: pos,
                            },
                        ],
                    ),
                    _ => unreachable!(),
                };
                Err(classify_boundary_error(&res.unwrap_err()))
            }

            InvalidOp::OverlappingBatch {
                actor,
                buffer,
                first,
                second,
            } => {
                let r1 = self
                    .encode_region(&current, *buffer, first.0, first.1, EndpointMode::Exclusive)
                    .map_err(|e| classify_boundary_error(&e))?;
                let r2 = self
                    .encode_region(
                        &current,
                        *buffer,
                        second.0,
                        second.1,
                        EndpointMode::Exclusive,
                    )
                    .map_err(|e| classify_boundary_error(&e))?;
                let envelope = self.envelope(Input {
                    actor: Some(*actor),
                    revision: Some(current.revision()),
                    target: Some(Target::Buffer(*buffer)),
                    operation: InputOperation::Batch(vec![
                        Replacement {
                            region: r1,
                            text: Text::from_str("A"),
                        },
                        Replacement {
                            region: r2,
                            text: Text::from_str("B"),
                        },
                    ]),
                });
                let err = envelope.normalize(&current).unwrap_err();
                Err(classify_boundary_error(&err))
            }

            InvalidOp::RequestIdReuse {
                actor,
                buffer,
                reuse_tag,
            } => {
                let reused = self
                    .tagged_commits
                    .get(reuse_tag)
                    .ok_or(RejectionReason::InvalidCoordinate)?
                    .request;
                let requests = self
                    .lower_single_insert(
                        &current,
                        Context {
                            actor: *actor,
                            buffer: *buffer,
                        },
                        0,
                        "conflicting-reuse",
                    )
                    .map_err(|e| classify_boundary_error(&e))?;
                self.execute_edit_request(&requests[0], reused, nonce, true, None, None)
            }

            InvalidOp::TxnIdReuse {
                actor,
                buffer,
                reuse_tag,
            } => {
                let reused_txn = self
                    .tagged_commits
                    .get(reuse_tag)
                    .ok_or(RejectionReason::InvalidCoordinate)?
                    .txn;
                let requests = self
                    .lower_single_insert(
                        &current,
                        Context {
                            actor: *actor,
                            buffer: *buffer,
                        },
                        0,
                        "conflicting-txn-reuse",
                    )
                    .map_err(|e| classify_boundary_error(&e))?;
                self.execute_edit_request(
                    &requests[0],
                    req_id,
                    reused_txn.nonce(),
                    true,
                    None,
                    None,
                )
            }

            InvalidOp::StaleEnvelope {
                tag,
                actor,
                buffer,
                start_byte,
                end_byte,
                text,
            } => {
                let saved = self
                    .saved_snapshots
                    .get(tag)
                    .ok_or(RejectionReason::InvalidCoordinate)?;
                let region = self
                    .encode_region(
                        saved,
                        *buffer,
                        *start_byte,
                        *end_byte,
                        EndpointMode::Exclusive,
                    )
                    .map_err(|e| classify_boundary_error(&e))?;
                let envelope = self.envelope(Input {
                    actor: Some(*actor),
                    revision: Some(saved.revision()),
                    target: Some(Target::Buffer(*buffer)),
                    operation: InputOperation::Edit(Replacement {
                        region,
                        text: Text::from_str(text),
                    }),
                });
                let err = envelope.normalize(&current).unwrap_err();
                Err(classify_boundary_error(&err))
            }

            InvalidOp::MissingActorEnvelope { buffer } => {
                let envelope = self.envelope(Input {
                    actor: None,
                    revision: Some(current.revision()),
                    target: Some(Target::Buffer(*buffer)),
                    operation: InputOperation::Read,
                });
                let err = envelope.normalize(&current).unwrap_err();
                Err(classify_boundary_error(&err))
            }
        }
    }

    fn roundtrip_annotation(
        &self,
        text: &Text,
        annotation: &Annotation,
    ) -> Result<Annotation, BoundaryError> {
        match self.adapter {
            AdapterKind::Emacs => {
                let overlay: EmacsOverlay = annotations::project_emacs(text, annotation)?;
                annotations::unproject_emacs(text, &overlay)
            }
            AdapterKind::Neovim => {
                let extmark: NeovimExtmark = annotations::project_neovim(text, annotation)?;
                annotations::unproject_neovim(text, &extmark)
            }
            AdapterKind::Agent => {
                let ann: AgentAnnotation =
                    annotations::project_agent(text, annotation, self.agent_coord_kind)?;
                annotations::unproject_agent(text, &ann)
            }
        }
    }

    fn observe(&self) -> AdapterObservation {
        let snapshot = self.coordinator.snapshot();
        let catalog = snapshot.catalog();
        let mut live_buffers = BTreeMap::new();
        for id in catalog.live_ids() {
            live_buffers.insert(
                id,
                catalog.get(id).expect("live buffer").as_str().to_owned(),
            );
        }
        let tombstoned_buffers: BTreeSet<BufferId> = catalog.tombstoned_ids().into_iter().collect();

        let mut projected_buffers = BTreeMap::new();
        let mut lagging_buffers = BTreeMap::new();
        for (&id, client) in &self.clients {
            if let Some(native) = &client.native {
                let decoded = native.decode().expect("valid native projection");
                projected_buffers.insert(id, decoded);
            }
            if !client.lagging.is_closed() {
                let decoded = match self.adapter {
                    AdapterKind::Emacs => {
                        let s = client
                            .lagging_projections
                            .emacs()
                            .expect("live lagging emacs projection");
                        let d = snapshot::decode_emacs(s);
                        (d.revision, d.text.as_str().to_owned())
                    }
                    AdapterKind::Neovim => {
                        let s = client
                            .lagging_projections
                            .neovim()
                            .expect("live lagging neovim projection");
                        let d = snapshot::decode_neovim(s).expect("canonical neovim projection");
                        (d.revision, d.text.as_str().to_owned())
                    }
                    AdapterKind::Agent => {
                        let s = client
                            .lagging_projections
                            .agent()
                            .expect("live lagging agent projection");
                        let d = snapshot::decode_agent(s);
                        (d.revision, d.text.as_str().to_owned())
                    }
                };
                lagging_buffers.insert(id, decoded);
            }
        }

        let mut selections = BTreeMap::new();
        let mut annotations_map = BTreeMap::new();
        let registry = self.coordinator.workspace().actors();
        for (actor, _) in registry.active_actors() {
            for &buffer in live_buffers.keys() {
                if let Ok(Some(selection)) = self.session.selection(actor, buffer) {
                    let view = self
                        .session
                        .view(actor, buffer)
                        .expect("readable view")
                        .expect("selection has view");
                    // Project through the adapter's native coordinate representation and decode back.
                    let encoded = OrientedRegion::from_selection(
                        catalog,
                        &view,
                        selection,
                        self.coord_kind(),
                        EndpointMode::Exclusive,
                    )
                    .expect("valid projected selection");
                    let decoded = encoded
                        .to_selection(catalog, &view, buffer)
                        .expect("roundtrip projected selection");
                    selections.insert(
                        (actor, buffer),
                        NormalizedSelection {
                            anchor: decoded.anchor().as_usize(),
                            head: decoded.head().as_usize(),
                            direction: decoded.direction(),
                        },
                    );
                }
            }
        }
        for &(actor, key) in &self.annotation_keys {
            if !registry.is_active(actor) {
                continue;
            }
            if let Ok(Some(annotation)) = self.session.overlay(actor, key) {
                if let Some(text) = catalog.get(key.buffer) {
                    let roundtripped = self
                        .roundtrip_annotation(text, annotation)
                        .expect("roundtrip projected annotation");
                    annotations_map.insert(
                        (actor, key),
                        NormalizedAnnotation {
                            start: roundtripped.span.start().as_usize(),
                            end: roundtripped.span.end().as_usize(),
                            start_affinity: roundtripped.start_affinity,
                            end_affinity: roundtripped.end_affinity,
                            priority: roundtripped.priority,
                            collapse: roundtripped.collapse,
                            label: roundtripped.label,
                        },
                    );
                }
            }
        }

        AdapterObservation {
            adapter: self.adapter,
            revision: snapshot.revision(),
            live_buffers,
            tombstoned_buffers,
            projected_buffers,
            lagging_buffers,
            selections,
            annotations: annotations_map,
            receipts: self.receipts.clone(),
            change_groups: self.group_plan.groups().iter().map(|g| g.id).collect(),
            outcomes: self.outcomes.clone(),
        }
    }
}

/// Runs a semantic operation sequence across the Emacs, Neovim, and Agent
/// adapters from the exact same initial checkpoint, verifying conformance after
/// every step.
pub fn run_conformance(
    initial: &InitialWorkspace,
    ops: &[ScenarioOp],
) -> Result<ConformanceReport, ConformanceFailure> {
    let (checkpoint, policy) =
        build_initial_checkpoint(initial).map_err(|_| ConformanceFailure {
            divergences: vec![ConformanceDivergence::RejectionStateDrift {
                adapter: AdapterKind::Emacs,
                step: 0,
            }],
        })?;
    let mut harnesses = [
        AdapterHarness::new(
            AdapterKind::Emacs,
            initial.agent_coord_kind,
            &checkpoint,
            policy.clone(),
        ),
        AdapterHarness::new(
            AdapterKind::Neovim,
            initial.agent_coord_kind,
            &checkpoint,
            policy.clone(),
        ),
        AdapterHarness::new(
            AdapterKind::Agent,
            initial.agent_coord_kind,
            &checkpoint,
            policy,
        ),
    ];

    run_conformance_harnesses(&mut harnesses, ops)
}

fn run_conformance_harnesses(
    harnesses: &mut [AdapterHarness; 3],
    ops: &[ScenarioOp],
) -> Result<ConformanceReport, ConformanceFailure> {
    let initial_obs = [
        harnesses[0].observe(),
        harnesses[1].observe(),
        harnesses[2].observe(),
    ];
    verify_conformance(&initial_obs)?;

    for (step, op) in ops.iter().enumerate() {
        let mut drift = Vec::new();
        for harness in harnesses.iter_mut() {
            if let Err(err) = harness.run_step(step, op) {
                drift.extend(err.divergences);
            }
        }
        if !drift.is_empty() {
            return Err(ConformanceFailure { divergences: drift });
        }
        let obs = [
            harnesses[0].observe(),
            harnesses[1].observe(),
            harnesses[2].observe(),
        ];
        verify_conformance(&obs)?;
    }

    let observations = [
        harnesses[0].observe(),
        harnesses[1].observe(),
        harnesses[2].observe(),
    ];
    Ok(ConformanceReport {
        observations,
        step_count: ops.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::{redo::Redo, selective_undo::SelectiveUndo, sequencer::Sequencer},
        edit::rectangle_edit::{self, Rectangle, RectangleOperation, ShortLinePolicy},
        test_support::{assert_law, texts, Rng},
        text::{
            columns::VisualPosition,
            position_map::{ByteReplacement, PositionMap},
            rectangle::Endpoint,
        },
        workspace::{
            annotation_mapping::{self, MappingRevisions, RevisionedMap},
            annotations::{AnnotationId, NamespaceId},
            catalog::BufferCatalog,
            snapshots::VersionedWorkspace,
        },
    };

    const AGENT_MODES: [CoordKind; 2] = [CoordKind::Byte, CoordKind::LineColUtf16];

    fn run_both_agent_modes(
        buffers: &[(&str, &str)],
        ops: &[ScenarioOp],
    ) -> [ConformanceReport; 2] {
        AGENT_MODES.map(|kind| {
            let initial = InitialWorkspace::standard(buffers, kind);
            run_conformance(&initial, ops).unwrap_or_else(|err| {
                panic!("conformance failed in agent mode {kind}: {err}");
            })
        })
    }

    fn ann_key(buffer: u64, id: u64) -> AnnotationKey {
        AnnotationKey {
            buffer: BufferId::new(buffer),
            namespace: NamespaceId::new(1),
            id: AnnotationId::new(id),
        }
    }

    #[test]
    fn unicode_crlf_insertions_deletions_replacements_and_selections() {
        let a1 = ActorId::new(1);
        let a2 = ActorId::new(2);
        let b0 = BufferId::new(0);
        let b1 = BufferId::new(1);
        let b2 = BufferId::new(2);

        // Buffer 0: mixed ASCII, CJK, combining mark, emoji, CRLF, trailing LF.
        // Buffer 1: empty buffer edge case.
        // Buffer 2: single LF and CRLF lines without trailing LF.
        let src0 = "a界e\u{0301}😀\r\n終😀\n";
        let src1 = "";
        let src2 = "\n\r\nx";

        // Byte boundaries in src0:
        // "a"(0..1) "界"(1..4) "e"(4..5) "\u{0301}"(5..7) "😀"(7..11) "\r"(11..12) "\n"(12..13)
        // "終"(13..16) "😀"(16..20) "\n"(20..21)
        let ops = vec![
            // Forward and backward selections before edits.
            ScenarioOp::Select {
                actor: a1,
                buffer: b0,
                anchor_byte: 1,
                head_byte: 11,
                mode: EndpointMode::Exclusive,
            },
            ScenarioOp::Select {
                actor: a2,
                buffer: b0,
                anchor_byte: 20,
                head_byte: 13,
                mode: EndpointMode::Inclusive,
            },
            // Empty selection at EOF in inclusive mode (allowed).
            ScenarioOp::Select {
                actor: a1,
                buffer: b1,
                anchor_byte: 0,
                head_byte: 0,
                mode: EndpointMode::Inclusive,
            },
            // Empty selection before EOF in inclusive mode is rejected identically.
            ScenarioOp::Select {
                actor: a1,
                buffer: b0,
                anchor_byte: 4,
                head_byte: 4,
                mode: EndpointMode::Inclusive,
            },
            // Annotation across emoji + CRLF.
            ScenarioOp::SetAnnotation {
                actor: a1,
                key: ann_key(0, 1),
                start_byte: 7,
                end_byte: 16,
                start_affinity: Affinity::Left,
                end_affinity: Affinity::Right,
                priority: 100,
                collapse: CollapsePolicy::RetainEmpty,
                label: "emoji-crlf".into(),
            },
            // Insert into empty buffer (toggles Neovim empty line-array -> non-empty with LF).
            ScenarioOp::Insert {
                actor: a1,
                buffer: b1,
                byte_offset: 0,
                text: "日本語\r\n😀\n".into(),
                tag: Some("b1_init".into()),
            },
            // Replace across combining accent + emoji using reversed Emacs mark/point.
            ScenarioOp::Replace {
                actor: a2,
                buffer: b0,
                start_byte: 4,
                end_byte: 11,
                text: "漢字𐀀".into(),
                reverse_emacs_mark: true,
                tag: Some("b0_replace".into()),
            },
            // Delete trailing LF in b0 (tests Neovim endofline true -> false transition).
            ScenarioOp::Replace {
                actor: a1,
                buffer: b0,
                start_byte: "a界漢字𐀀\r\n終😀\n".len() - 1,
                end_byte: "a界漢字𐀀\r\n終😀\n".len(),
                text: String::new(),
                reverse_emacs_mark: false,
                tag: Some("b0_strip_lf".into()),
            },
            // Insert trailing LF on b2 (tests Neovim endofline false -> true transition).
            ScenarioOp::Insert {
                actor: a2,
                buffer: b2,
                byte_offset: src2.len(),
                text: "\n".into(),
                tag: Some("b2_add_lf".into()),
            },
            // Read all three buffers through adapter snapshot encode/decode.
            ScenarioOp::ReadBuffer {
                actor: a1,
                buffer: b0,
            },
            ScenarioOp::ReadBuffer {
                actor: a2,
                buffer: b1,
            },
            ScenarioOp::ReadBuffer {
                actor: a1,
                buffer: b2,
            },
            // No-op replacement rejects with NoChange and zero drift.
            ScenarioOp::Replace {
                actor: a1,
                buffer: b2,
                start_byte: 0,
                end_byte: 1,
                text: "\n".into(),
                reverse_emacs_mark: false,
                tag: None,
            },
        ];

        let reports =
            run_both_agent_modes(&[("zero", src0), ("empty", src1), ("lines", src2)], &ops);
        for report in &reports {
            let obs = report.observation(AdapterKind::Emacs);
            assert_eq!(obs.live_buffers[&b0], "a界漢字𐀀\r\n終😀");
            assert_eq!(obs.live_buffers[&b1], "日本語\r\n😀\n");
            assert_eq!(obs.live_buffers[&b2], "\n\r\nx\n");
            assert_eq!(obs.projected_buffers[&b0].1, obs.live_buffers[&b0]);
            assert_eq!(obs.projected_buffers[&b1].1, obs.live_buffers[&b1]);
            assert_eq!(obs.projected_buffers[&b2].1, obs.live_buffers[&b2]);
            // Inclusive empty before EOF rejected at step 3; no-op rejected at final step.
            assert!(matches!(
                obs.outcomes[3],
                StepOutcome::Rejected {
                    reason: RejectionReason::UnsupportedOperation,
                    ..
                }
            ));
            assert!(matches!(
                obs.outcomes.last(),
                Some(StepOutcome::Rejected {
                    reason: RejectionReason::NoChange,
                    ..
                })
            ));
            // Selections were projected forward across edits and agree across all 3 adapters.
            assert!(obs.selections.contains_key(&(a1, b0)));
            assert!(obs.selections.contains_key(&(a2, b0)));
            assert_eq!(obs.selections[&(a2, b0)].direction, Direction::Backward);
            assert!(obs.annotations.contains_key(&(a1, ann_key(0, 1))));
        }
    }

    #[test]
    fn change_groups_undo_redo_and_group_plan_equivalence() {
        let a1 = ActorId::new(1);
        let a2 = ActorId::new(2);
        let b0 = BufferId::new(0);
        let seq_id = TxnId::new(a1, Nonce::new(77));

        let ops = vec![
            // Multi-edit change group with intermediate coordinates (insert, then replace).
            ScenarioOp::ChangeGroup {
                actor: a1,
                buffer: b0,
                edits: vec![
                    GroupEdit {
                        start_byte: 0,
                        end_byte: 0,
                        text: "界😀".into(),
                    },
                    GroupEdit {
                        start_byte: "界😀a".len(),
                        end_byte: "界😀ab".len(),
                        text: "XYZ".into(),
                    },
                ],
                sequence_step: Some(SequenceStep {
                    sequence: seq_id,
                    index: 0,
                }),
                tag: Some("group1".into()),
            },
            // Second actor makes a disjoint edit at the tail.
            ScenarioOp::Insert {
                actor: a2,
                buffer: b0,
                byte_offset: "界😀aXYZcdef".len(),
                text: "\n終".into(),
                tag: Some("tail_edit".into()),
            },
            // Undo group1 via resolved single candidate (Emacs/Neovim Resolved, Agent Explicit).
            ScenarioOp::Undo {
                actor: a1,
                target_tag: "group1".into(),
                use_resolved_candidate: true,
                tag: Some("undo_group1".into()),
            },
            // Duplicate undo of already-undone target rejects with AlreadyUndone.
            ScenarioOp::Undo {
                actor: a1,
                target_tag: "group1".into(),
                use_resolved_candidate: false,
                tag: None,
            },
            // Wrong actor trying to undo tail_edit rejects with NotUndoOwner.
            ScenarioOp::Undo {
                actor: a1,
                target_tag: "tail_edit".into(),
                use_resolved_candidate: false,
                tag: None,
            },
            // Redo group1 cleanly across the disjoint tail edit.
            ScenarioOp::Redo {
                actor: a1,
                target_tag: "group1".into(),
                use_resolved_candidate: true,
                tag: Some("redo_group1".into()),
            },
            // Redo again on an already-redone target rejects with NotUndone.
            ScenarioOp::Redo {
                actor: a1,
                target_tag: "group1".into(),
                use_resolved_candidate: false,
                tag: None,
            },
            // Second undo cycle targeting root after redo, then redo targeting compensation.
            ScenarioOp::Undo {
                actor: a1,
                target_tag: "group1".into(),
                use_resolved_candidate: false,
                tag: Some("undo2".into()),
            },
            ScenarioOp::Redo {
                actor: a1,
                target_tag: "undo2".into(),
                use_resolved_candidate: false,
                tag: Some("redo2".into()),
            },
            // Idempotent retry of group1 returns exact original receipt with zero drift.
            ScenarioOp::RetryCommitted {
                tag: "group1".into(),
            },
            // Conflicting edit on group1's region blocks subsequent undo with Conflict.
            ScenarioOp::Replace {
                actor: a2,
                buffer: b0,
                start_byte: 0,
                end_byte: "界".len(),
                text: "衝突".into(),
                reverse_emacs_mark: false,
                tag: Some("conflict_edit".into()),
            },
            ScenarioOp::Undo {
                actor: a1,
                target_tag: "group1".into(),
                use_resolved_candidate: false,
                tag: None,
            },
        ];

        let reports = run_both_agent_modes(&[("doc", "abcdef")], &ops);
        let obs = reports[0].observation(AdapterKind::Neovim);
        assert_eq!(obs.live_buffers[&b0], "衝突😀aXYZcdef\n終");
        assert!(matches!(
            obs.outcomes[3],
            StepOutcome::Rejected {
                reason: RejectionReason::AlreadyUndone,
                ..
            }
        ));
        assert!(matches!(
            obs.outcomes[4],
            StepOutcome::Rejected {
                reason: RejectionReason::NotUndoOwner,
                ..
            }
        ));
        assert!(matches!(
            obs.outcomes[6],
            StepOutcome::Rejected {
                reason: RejectionReason::NotUndone,
                ..
            }
        ));
        assert!(matches!(
            obs.outcomes[11],
            StepOutcome::Rejected {
                reason: RejectionReason::Conflict,
                ..
            }
        ));
        assert_eq!(obs.change_groups, obs.receipts);

        // Also verify standalone undo::submit equivalence across all 3 FrontendCommand variants.
        let target = TxnId::new(a1, Nonce::new(1));
        let undo_id = TxnId::new(a1, Nonce::new(2));
        let redo_id = TxnId::new(a1, Nonce::new(3));
        let cands = [target];
        let undo_cmds = [
            FrontendCommand::EmacsUndoOnly(UndoTarget::Resolved(&cands)),
            FrontendCommand::NeovimUndo(UndoTarget::Resolved(&cands)),
            FrontendCommand::AgentUndo(target),
        ];
        let redo_cmds = [
            FrontendCommand::EmacsUndoRedo(UndoTarget::Explicit(target)),
            FrontendCommand::NeovimRedo(UndoTarget::Resolved(&cands)),
            FrontendCommand::AgentRedo(target),
        ];
        let mut standalone = Vec::new();
        for idx in 0..3 {
            let mut cat = BufferCatalog::new();
            cat.register(b0, Text::from_str("a界😀z")).unwrap();
            let mut reg = ActorRegistry::new();
            reg.register(a1, ActorRole::Human).unwrap();
            let mut engine = Redo::new(SelectiveUndo::new(Sequencer::new(cat, reg)));
            let span = ByteSpan::validate_in(&Text::from_str("a界😀z"), 1, 4).unwrap();
            let patch = crate::editor::edit::patch::Patch::new(vec![
                crate::editor::edit::splice::Splice::new(span, Text::from_str("改")),
            ])
            .unwrap();
            let txn = Transaction::new(EditIntent::new(target, Revision::ZERO), vec![(b0, patch)])
                .unwrap();
            assert!(engine.submit(txn).is_committed());
            let u = undo::submit(&mut engine, a1, undo_id, undo_cmds[idx]).unwrap();
            let r = undo::submit(&mut engine, a1, redo_id, redo_cmds[idx]).unwrap();
            let r_retry = undo::submit(&mut engine, a1, redo_id, redo_cmds[idx]).unwrap();
            assert_eq!(r, r_retry);
            standalone.push((
                u,
                r,
                engine.sequencer().snapshot().revision(),
                engine
                    .sequencer()
                    .snapshot()
                    .catalog()
                    .get(b0)
                    .unwrap()
                    .as_str()
                    .to_owned(),
            ));
        }
        assert_eq!(standalone[0], standalone[1]);
        assert_eq!(standalone[1], standalone[2]);
        assert_eq!(standalone[0].3, "a改😀z");
    }

    #[test]
    fn disjoint_concurrent_edits_and_overlapping_conflicts() {
        let a1 = ActorId::new(1);
        let a2 = ActorId::new(2);
        let b0 = BufferId::new(0);
        // "head-界-middle-😀-tail\r\n"
        // "head-" len 5; "界" len 3 (5..8); "-middle-" len 8 (8..16); "😀" len 4 (16..20); "-tail\r\n" (20..27)
        let source = "head-界-middle-😀-tail\r\n";

        let ops = vec![
            ScenarioOp::Select {
                actor: a1,
                buffer: b0,
                anchor_byte: 8,
                head_byte: 16,
                mode: EndpointMode::Exclusive,
            },
            ScenarioOp::SetAnnotation {
                actor: a2,
                key: ann_key(0, 10),
                start_byte: 8,
                end_byte: 16,
                start_affinity: Affinity::Left,
                end_affinity: Affinity::Right,
                priority: 5,
                collapse: CollapsePolicy::RetainEmpty,
                label: "mid-touching".into(),
            },
            ScenarioOp::SetAnnotation {
                actor: a2,
                key: ann_key(0, 11),
                start_byte: 8,
                end_byte: 15,
                start_affinity: Affinity::Left,
                end_affinity: Affinity::Left,
                priority: 6,
                collapse: CollapsePolicy::RetainEmpty,
                label: "mid-interior".into(),
            },
            ScenarioOp::SaveSnapshot {
                tag: "fork_base".into(),
            },
            // Actor 1 edits prefix [0..5].
            ScenarioOp::Replace {
                actor: a1,
                buffer: b0,
                start_byte: 0,
                end_byte: 5,
                text: "BEGIN:".into(),
                reverse_emacs_mark: false,
                tag: Some("prefix_edit".into()),
            },
            // Actor 2 submits a stale-base disjoint edit at [16..20] ("😀" -> "🎉") against fork_base.
            ScenarioOp::EditAtSavedBase {
                base_tag: "fork_base".into(),
                actor: a2,
                buffer: b0,
                start_byte: 16,
                end_byte: 20,
                text: "🎉".into(),
                tag: Some("disjoint_rebase".into()),
            },
            // Actor 2 submits a stale-base overlapping edit touching [0..8] against fork_base -> Conflict!
            ScenarioOp::EditAtSavedBase {
                base_tag: "fork_base".into(),
                actor: a2,
                buffer: b0,
                start_byte: 4,
                end_byte: 8,
                text: "CLASH".into(),
                tag: None,
            },
            // Touching closed-footprint boundary at byte 5 against fork_base -> Conflict!
            ScenarioOp::EditAtSavedBase {
                base_tag: "fork_base".into(),
                actor: a2,
                buffer: b0,
                start_byte: 5,
                end_byte: 8,
                text: "TOUCH".into(),
                tag: None,
            },
            // Envelope-level stale revision rejection -> RevisionMismatch!
            ScenarioOp::Invalid(InvalidOp::StaleEnvelope {
                tag: "fork_base".into(),
                actor: a2,
                buffer: b0,
                start_byte: 20,
                end_byte: 25,
                text: "stale".into(),
            }),
        ];

        let reports = run_both_agent_modes(&[("shared", source)], &ops);
        let obs = reports[0].observation(AdapterKind::Agent);
        assert_eq!(obs.live_buffers[&b0], "BEGIN:界-middle-🎉-tail\r\n");
        // Selection on "-middle-" shifted by +1 from prefix edit ("head-" -> "BEGIN:"),
        // and its right-affinity head at the start of replaced "😀" (17..21) advances to 21.
        assert_eq!(
            obs.selections[&(a1, b0)],
            NormalizedSelection {
                anchor: 9,
                head: 21,
                direction: Direction::Forward,
            }
        );
        // Right-affinity annotation endpoint at byte 17 also advances to 21, while
        // the strictly interior annotation (8..15) shifts uniformly by +1 to (9..16).
        assert_eq!(obs.annotations[&(a2, ann_key(0, 10))].start, 9);
        assert_eq!(obs.annotations[&(a2, ann_key(0, 10))].end, 21);
        assert_eq!(obs.annotations[&(a2, ann_key(0, 11))].start, 9);
        assert_eq!(obs.annotations[&(a2, ann_key(0, 11))].end, 16);
        assert!(matches!(
            obs.outcomes[6],
            StepOutcome::Rejected {
                reason: RejectionReason::Conflict,
                ..
            }
        ));
        assert!(matches!(
            obs.outcomes[7],
            StepOutcome::Rejected {
                reason: RejectionReason::Conflict,
                ..
            }
        ));
        assert!(matches!(
            obs.outcomes[8],
            StepOutcome::Rejected {
                reason: RejectionReason::RevisionMismatch,
                ..
            }
        ));
    }

    #[test]
    fn stale_base_after_compaction_and_resync_catch_up() {
        let a1 = ActorId::new(1);
        let a2 = ActorId::new(2);
        let b0 = BufferId::new(0);
        let b1 = BufferId::new(1);

        let ops = vec![
            ScenarioOp::SaveSnapshot {
                tag: "rev_initial".into(),
            },
            // Pause lagging client on b0 to simulate dropped deliveries.
            ScenarioOp::PauseLagging { buffer: b0 },
            ScenarioOp::Insert {
                actor: a1,
                buffer: b0,
                byte_offset: 0,
                text: "一行\n".into(),
                tag: Some("e1".into()),
            },
            // Silent edit on b1 while b0 client is lagging.
            ScenarioOp::Insert {
                actor: a2,
                buffer: b1,
                byte_offset: 0,
                text: "other😀".into(),
                tag: Some("e2".into()),
            },
            ScenarioOp::Insert {
                actor: a1,
                buffer: b0,
                byte_offset: "一行\n".len(),
                text: "界😀\r\n".into(),
                tag: Some("e3".into()),
            },
            // Delivering latest event directly across skipped events hits ResyncGap atomically.
            ScenarioOp::DeliverLatestToLagging { buffer: b0 },
            // Catch up via incremental CatchUp::Events before compaction.
            ScenarioOp::CatchUpLagging { buffer: b0 },
            // Now pause again, produce more edits, and compact so events are evicted.
            ScenarioOp::PauseLagging { buffer: b0 },
            ScenarioOp::Replace {
                actor: a1,
                buffer: b0,
                start_byte: 0,
                end_byte: "一行".len(),
                text: "更新".into(),
                reverse_emacs_mark: false,
                tag: Some("e4".into()),
            },
            ScenarioOp::Insert {
                actor: a2,
                buffer: b0,
                byte_offset: 0,
                text: ">>".into(),
                tag: Some("e5".into()),
            },
            // Compact to tip: retained history floor advances past rev_initial and lagging rev.
            ScenarioOp::Compact {
                floor_offset_from_tip: 0,
            },
            // Stale-base edit against compacted rev_initial rejects with BaseExpired!
            ScenarioOp::EditAtSavedBase {
                base_tag: "rev_initial".into(),
                actor: a2,
                buffer: b0,
                start_byte: 0,
                end_byte: 0,
                text: "too-old".into(),
                tag: None,
            },
            // Undo of compacted edit e1 rejects with BaseExpired!
            ScenarioOp::Undo {
                actor: a1,
                target_tag: "e1".into(),
                use_resolved_candidate: false,
                tag: None,
            },
            // Catch up via CatchUp::Snapshot after compaction recovers lagging client identically!
            ScenarioOp::CatchUpLagging { buffer: b0 },
            // Request::recover recovers from rev_initial to tip for live buffer b0.
            ScenarioOp::RecoverViaRequest {
                actor: a1,
                buffer: b0,
                observed_tag: "rev_initial".into(),
            },
            // Close b1 and verify closed-buffer recovery via Request::recover.
            ScenarioOp::CloseBuffer {
                actor: a1,
                buffer: b1,
                policy: ClosePolicy::Discard,
                base_override: None,
                tag: Some("close_b1".into()),
            },
            ScenarioOp::RecoverViaRequest {
                actor: a1,
                buffer: b1,
                observed_tag: "rev_initial".into(),
            },
            // Post-recovery edit confirms resumed incremental delta sync.
            ScenarioOp::Insert {
                actor: a1,
                buffer: b0,
                byte_offset: 0,
                text: "終".into(),
                tag: Some("after_recovery".into()),
            },
        ];

        let reports = run_both_agent_modes(&[("main", "base"), ("aux", "aux")], &ops);
        let obs = reports[0].observation(AdapterKind::Emacs);
        assert_eq!(obs.live_buffers[&b0], "終>>更新\n界😀\r\nbase");
        assert_eq!(obs.lagging_buffers[&b0].1, obs.live_buffers[&b0]);
        assert_eq!(obs.lagging_buffers[&b0].0, obs.revision);
        assert!(obs.tombstoned_buffers.contains(&b1));
        assert!(matches!(
            obs.outcomes[5],
            StepOutcome::Rejected {
                reason: RejectionReason::ResyncGap,
                ..
            }
        ));
        assert!(matches!(
            obs.outcomes[11],
            StepOutcome::Rejected {
                reason: RejectionReason::BaseExpired,
                ..
            }
        ));
        assert!(matches!(
            obs.outcomes[12],
            StepOutcome::Rejected {
                reason: RejectionReason::BaseExpired,
                ..
            }
        ));
    }

    #[test]
    fn multi_buffer_batches_proposals_and_permission_boundaries() {
        let human = ActorId::new(1);
        let agent = ActorId::new(2);
        let outsider = ActorId::new(99);
        let b0 = BufferId::new(0);
        let b1 = BufferId::new(1);

        let ops = vec![
            // Multi-buffer atomic batch across b0 and b1 with inclusive endpoints.
            ScenarioOp::MultiBufferBatch {
                actor: human,
                primary: b0,
                edits: vec![
                    SpanEdit {
                        buffer: b0,
                        start_byte: 0,
                        end_byte: "界".len(),
                        text: "新".into(),
                    },
                    SpanEdit {
                        buffer: b1,
                        start_byte: 0,
                        end_byte: "😀".len(),
                        text: "🚀".into(),
                    },
                ],
                mode: EndpointMode::Inclusive,
                tag: Some("multi_inc".into()),
            },
            // Restrict default Commit and Read; grant selectively.
            ScenarioOp::UpdatePolicy(PolicyChange::RevokeDefault(Capability::Commit)),
            ScenarioOp::UpdatePolicy(PolicyChange::RevokeDefault(Capability::Read)),
            ScenarioOp::UpdatePolicy(PolicyChange::GrantBuffer(human, b0, Capability::Read)),
            ScenarioOp::UpdatePolicy(PolicyChange::GrantBuffer(human, b0, Capability::Commit)),
            ScenarioOp::UpdatePolicy(PolicyChange::GrantBuffer(human, b1, Capability::Read)),
            ScenarioOp::UpdatePolicy(PolicyChange::GrantBuffer(agent, b0, Capability::Read)),
            ScenarioOp::UpdatePolicy(PolicyChange::GrantBuffer(agent, b0, Capability::Propose)),
            // Unauthorized read by agent on b1 -> PermissionDenied.
            ScenarioOp::ReadBuffer {
                actor: agent,
                buffer: b1,
            },
            // Unauthorized direct commit by agent on b0 -> PermissionDenied.
            ScenarioOp::Insert {
                actor: agent,
                buffer: b0,
                byte_offset: 0,
                text: "denied".into(),
                tag: None,
            },
            // Multi-buffer batch where human lacks Commit on b1 -> atomic PermissionDenied!
            ScenarioOp::MultiBufferBatch {
                actor: human,
                primary: b0,
                edits: vec![
                    SpanEdit {
                        buffer: b0,
                        start_byte: 0,
                        end_byte: 0,
                        text: "ok".into(),
                    },
                    SpanEdit {
                        buffer: b1,
                        start_byte: 0,
                        end_byte: 0,
                        text: "blocked".into(),
                    },
                ],
                mode: EndpointMode::Exclusive,
                tag: None,
            },
            // Unknown actor -> UnknownActor.
            ScenarioOp::Insert {
                actor: outsider,
                buffer: b0,
                byte_offset: 0,
                text: "ghost".into(),
                tag: None,
            },
            // Agent proposes an edit on b0; unapproved commit -> UnapprovedProposal.
            ScenarioOp::ProposeEdit {
                proposal_tag: "p1".into(),
                actor: agent,
                buffer: b0,
                start_byte: 0,
                end_byte: "新".len(),
                text: "提".into(),
            },
            ScenarioOp::CommitProposal {
                proposal_tag: "p1".into(),
                tag: None,
            },
            // Agent cannot self-approve -> NotHumanReviewer.
            ScenarioOp::ApproveProposal {
                proposal_tag: "p1".into(),
                reviewer: agent,
            },
            // Human approves p1, then commit needs Commit on proposer; grant and commit.
            ScenarioOp::ApproveProposal {
                proposal_tag: "p1".into(),
                reviewer: human,
            },
            // Without Commit on agent, commit_proposal rejects with PermissionDenied.
            ScenarioOp::CommitProposal {
                proposal_tag: "p1".into(),
                tag: None,
            },
            ScenarioOp::UpdatePolicy(PolicyChange::GrantBuffer(agent, b0, Capability::Commit)),
            ScenarioOp::CommitProposal {
                proposal_tag: "p1".into(),
                tag: Some("p1_committed".into()),
            },
            // Propose p2, approve it, then advance workspace so approval becomes stale -> StaleApproval!
            ScenarioOp::ProposeEdit {
                proposal_tag: "p2".into(),
                actor: agent,
                buffer: b0,
                start_byte: 0,
                end_byte: "提".len(),
                text: "古".into(),
            },
            ScenarioOp::ApproveProposal {
                proposal_tag: "p2".into(),
                reviewer: human,
            },
            ScenarioOp::Insert {
                actor: human,
                buffer: b0,
                byte_offset: "提abc".len(),
                text: "!".into(),
                tag: Some("intervening".into()),
            },
            ScenarioOp::CommitProposal {
                proposal_tag: "p2".into(),
                tag: None,
            },
            // Dirty buffer close with RequireClean rejects with DirtyBuffer.
            ScenarioOp::CloseBuffer {
                actor: human,
                buffer: b0,
                policy: ClosePolicy::RequireClean,
                base_override: None,
                tag: None,
            },
            // Revoke agent and verify RevokedActor rejection with zero state drift.
            ScenarioOp::RevokeActor { actor: agent },
            ScenarioOp::Insert {
                actor: agent,
                buffer: b0,
                byte_offset: 0,
                text: "after-revoke".into(),
                tag: None,
            },
            // Register a fresh human actor and open a new buffer.
            ScenarioOp::RegisterActor {
                actor: ActorId::new(3),
                role: ActorRole::Human,
            },
            ScenarioOp::UpdatePolicy(PolicyChange::GrantActor(
                ActorId::new(3),
                Capability::Commit,
            )),
            ScenarioOp::UpdatePolicy(PolicyChange::GrantActor(ActorId::new(3), Capability::Read)),
            ScenarioOp::OpenBuffer {
                actor: ActorId::new(3),
                name: "fresh".into(),
                text: "hello 界\n".into(),
                base_override: None,
                tag: Some("open_fresh".into()),
            },
            // Narrow view on fresh buffer and reject selection outside narrowed range.
            ScenarioOp::NarrowView {
                actor: ActorId::new(3),
                buffer: BufferId::new(2),
                start_byte: 0,
                end_byte: 5,
            },
            ScenarioOp::Select {
                actor: ActorId::new(3),
                buffer: BufferId::new(2),
                anchor_byte: 0,
                head_byte: 9,
                mode: EndpointMode::Exclusive,
            },
            ScenarioOp::Select {
                actor: ActorId::new(3),
                buffer: BufferId::new(2),
                anchor_byte: 1,
                head_byte: 5,
                mode: EndpointMode::Exclusive,
            },
        ];

        let reports = run_both_agent_modes(&[("a", "界abc"), ("b", "😀xyz")], &ops);
        let obs = reports[0].observation(AdapterKind::Emacs);
        assert_eq!(obs.live_buffers[&b0], "提abc!");
        assert_eq!(obs.live_buffers[&b1], "🚀xyz");
        assert_eq!(obs.live_buffers[&BufferId::new(2)], "hello 界\n");
        assert!(matches!(
            obs.outcomes[8],
            StepOutcome::Rejected {
                reason: RejectionReason::PermissionDenied,
                ..
            }
        ));
        assert!(matches!(
            obs.outcomes[11],
            StepOutcome::Rejected {
                reason: RejectionReason::UnknownActor,
                ..
            }
        ));
        assert!(matches!(
            obs.outcomes[13],
            StepOutcome::Rejected {
                reason: RejectionReason::UnapprovedProposal,
                ..
            }
        ));
        assert!(matches!(
            obs.outcomes[14],
            StepOutcome::Rejected {
                reason: RejectionReason::NotHumanReviewer,
                ..
            }
        ));
        assert!(matches!(
            obs.outcomes[22],
            StepOutcome::Rejected {
                reason: RejectionReason::StaleApproval,
                ..
            }
        ));
        assert!(matches!(
            obs.outcomes[23],
            StepOutcome::Rejected {
                reason: RejectionReason::DirtyBuffer,
                ..
            }
        ));
        assert!(matches!(
            obs.outcomes[25],
            StepOutcome::Rejected {
                reason: RejectionReason::RevokedActor,
                ..
            }
        ));
        assert!(matches!(
            obs.outcomes[31],
            StepOutcome::Rejected {
                reason: RejectionReason::OutOfRange,
                ..
            }
        ));
        assert_eq!(
            obs.selections[&(ActorId::new(3), BufferId::new(2))],
            NormalizedSelection {
                anchor: 1,
                head: 5,
                direction: Direction::Forward,
            }
        );
    }

    #[test]
    fn invalid_commands_malformed_groups_and_id_reuse_rejections() {
        let a1 = ActorId::new(1);
        let b0 = BufferId::new(0);
        let b1 = BufferId::new(1);
        let unknown_buf = BufferId::new(99);

        let ops = vec![
            ScenarioOp::Insert {
                actor: a1,
                buffer: b0,
                byte_offset: 0,
                text: "seed".into(),
                tag: Some("seed_edit".into()),
            },
            ScenarioOp::CloseBuffer {
                actor: a1,
                buffer: b1,
                policy: ClosePolicy::Discard,
                base_override: None,
                tag: Some("close_b1".into()),
            },
            ScenarioOp::Invalid(InvalidOp::OutOfRangePosition {
                actor: a1,
                buffer: b0,
            }),
            ScenarioOp::Invalid(InvalidOp::UnsupportedCommand {
                actor: a1,
                buffer: b0,
                name: "custom-macro".into(),
            }),
            ScenarioOp::Invalid(InvalidOp::NestedChangeGroup {
                actor: a1,
                buffer: b0,
            }),
            ScenarioOp::Invalid(InvalidOp::UnclosedChangeGroup {
                actor: a1,
                buffer: b0,
                text: "unclosed".into(),
            }),
            ScenarioOp::Invalid(InvalidOp::EmptyChangeGroup {
                actor: a1,
                buffer: b0,
            }),
            ScenarioOp::Invalid(InvalidOp::EndWithoutBegin {
                actor: a1,
                buffer: b0,
            }),
            ScenarioOp::Invalid(InvalidOp::SelectInsideGroup {
                actor: a1,
                buffer: b0,
            }),
            ScenarioOp::Invalid(InvalidOp::OverlappingBatch {
                actor: a1,
                buffer: b0,
                first: (0, 2),
                second: (2, 4),
            }),
            ScenarioOp::Invalid(InvalidOp::RequestIdReuse {
                actor: a1,
                buffer: b0,
                reuse_tag: "seed_edit".into(),
            }),
            ScenarioOp::Invalid(InvalidOp::TxnIdReuse {
                actor: a1,
                buffer: b0,
                reuse_tag: "seed_edit".into(),
            }),
            ScenarioOp::Invalid(InvalidOp::MissingActorEnvelope { buffer: b0 }),
            ScenarioOp::Insert {
                actor: a1,
                buffer: unknown_buf,
                byte_offset: 0,
                text: "x".into(),
                tag: None,
            },
            ScenarioOp::Insert {
                actor: a1,
                buffer: b1,
                byte_offset: 0,
                text: "x".into(),
                tag: None,
            },
            ScenarioOp::Undo {
                actor: a1,
                target_tag: "missing_tag".into(),
                use_resolved_candidate: false,
                tag: None,
            },
            ScenarioOp::OpenBuffer {
                actor: a1,
                name: "main".into(),
                text: "dup".into(),
                base_override: None,
                tag: None,
            },
        ];

        let reports = run_both_agent_modes(&[("main", "a界😀z"), ("temp", "bye")], &ops);
        let obs = reports[0].observation(AdapterKind::Neovim);
        let expected_reasons = [
            (2, RejectionReason::OutOfRange),
            (3, RejectionReason::UnsupportedOperation),
            (4, RejectionReason::CorruptSequence),
            (5, RejectionReason::CorruptSequence),
            (6, RejectionReason::CorruptSequence),
            (7, RejectionReason::CorruptSequence),
            (8, RejectionReason::UnsupportedOperation),
            (9, RejectionReason::Conflict),
            (10, RejectionReason::RequestReuse),
            (11, RejectionReason::TxnIdReuse),
            (12, RejectionReason::UnknownActor),
            (13, RejectionReason::UnknownBuffer),
            (14, RejectionReason::BufferClosed),
            (15, RejectionReason::UnknownUndoTarget),
            (16, RejectionReason::NameInUse),
        ];
        for (idx, expected) in expected_reasons {
            assert_eq!(
                obs.outcomes[idx],
                StepOutcome::Rejected {
                    reason: expected,
                    revision: obs.revision,
                },
                "unexpected outcome at step {idx}"
            );
        }
    }

    #[test]
    fn rectangle_edits_and_annotation_mapping_conformance() {
        // Verify visual-column rectangular edits translated into all 3 typed adapters agree
        // identically with rectangle_edit::plan_transaction across tabs, CJK, emoji, and CRLF.
        let source = "\t界😀\r\nabcd\r\n12\n終😀z";
        let mut catalog = BufferCatalog::new();
        let b0 = BufferId::new(0);
        catalog.register(b0, Text::from_str(source)).unwrap();
        let workspace = VersionedWorkspace::new(catalog);
        let snap = workspace.capture();
        let actor = ActorId::new(1);
        let rect = Rectangle {
            first: VisualPosition { line: 0, column: 1 },
            second: VisualPosition { line: 3, column: 4 },
            endpoint: Endpoint::Exclusive,
            tab_width: 4,
        };
        let rows = ["A", "B界", "C😀", "D"];
        for op in [
            RectangleOperation::Delete,
            RectangleOperation::Uniform("!"),
            RectangleOperation::Insert(">"),
            RectangleOperation::Replace(&rows),
        ] {
            let intent = EditIntent::for_actor(actor, Nonce::new(1), snap.revision());
            let planned = rectangle_edit::plan_transaction(
                &snap,
                b0,
                rect,
                ShortLinePolicy::Clamp,
                op,
                intent,
            )
            .unwrap();
            let expected = planned
                .apply_with_inverse(&snap)
                .unwrap()
                .catalog()
                .get(b0)
                .unwrap()
                .as_str()
                .to_owned();
            let span_edits: Vec<SpanEdit> = planned.patches()[0]
                .1
                .splices()
                .iter()
                .map(|s| SpanEdit {
                    buffer: b0,
                    start_byte: s.span().start().as_usize(),
                    end_byte: s.span().end().as_usize(),
                    text: s.replacement().as_str().to_owned(),
                })
                .collect();
            let reports = run_both_agent_modes(
                &[("rect", source)],
                &[ScenarioOp::MultiBufferBatch {
                    actor,
                    primary: b0,
                    edits: span_edits,
                    mode: EndpointMode::Exclusive,
                    tag: Some("rect_op".into()),
                }],
            );
            for report in &reports {
                assert_eq!(
                    report.observation(AdapterKind::Emacs).live_buffers[&b0],
                    expected
                );
                assert_eq!(
                    report.observation(AdapterKind::Neovim).live_buffers[&b0],
                    expected
                );
                assert_eq!(
                    report.observation(AdapterKind::Agent).live_buffers[&b0],
                    expected
                );
            }
        }

        // Verify annotations::map_emacs / map_neovim / map_agent commute identically on collapse.
        let before = Text::from_str("a😀z");
        let after = Text::from_str("a界z");
        let pos_map = PositionMap::from_replacement(
            before.len_bytes(),
            ByteReplacement {
                start: 1,
                end: 5,
                inserted_len: "界".len(),
            },
        )
        .unwrap();
        let rev_map = RevisionedMap {
            map: &pos_map,
            source: Revision::ZERO,
            target: Revision::new(1),
        };
        let revs = MappingRevisions {
            base: Revision::ZERO,
            target: Revision::new(1),
        };
        let core_ann = Annotation {
            key: ann_key(0, 42),
            owner: actor,
            span: ByteSpan::validate_in(&before, 1, 5).unwrap(),
            start_affinity: Affinity::Right,
            end_affinity: Affinity::Left,
            priority: 65535,
            collapse: CollapsePolicy::RetainEmpty,
            label: "collapse".into(),
        };
        let expected_mapped =
            annotation_mapping::map_annotation(&core_ann, &before, &after, &rev_map, revs).unwrap();
        let em = annotations::map_emacs(
            &annotations::project_emacs(&before, &core_ann).unwrap(),
            &before,
            &after,
            &rev_map,
            revs,
        )
        .unwrap();
        let nv = annotations::map_neovim(
            &annotations::project_neovim(&before, &core_ann).unwrap(),
            &before,
            &after,
            &rev_map,
            revs,
        )
        .unwrap();
        assert_eq!(
            annotations::unproject_emacs(&after, &em).unwrap(),
            expected_mapped
        );
        assert_eq!(
            annotations::unproject_neovim(&after, &nv).unwrap(),
            expected_mapped
        );
        for kind in AGENT_MODES {
            let ag = annotations::map_agent(
                &annotations::project_agent(&before, &core_ann, kind).unwrap(),
                &before,
                &after,
                &rev_map,
                revs,
            )
            .unwrap();
            assert_eq!(
                annotations::unproject_agent(&after, &ag).unwrap(),
                expected_mapped
            );
        }
    }

    fn random_boundary(rng: &mut Rng, text: &str) -> usize {
        let boundaries: Vec<usize> = text
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(text.len()))
            .collect();
        *rng.choose(&boundaries).expect("at least one boundary")
    }

    fn random_replacement(rng: &mut Rng) -> String {
        let choices = [
            "",
            "x",
            "abc",
            "\n",
            "\r\n",
            "界",
            "日本語\n",
            "😀",
            "e\u{0301}",
            "界😀\r\n",
        ];
        (*rng.choose(&choices).expect("nonempty")).to_owned()
    }

    #[test]
    fn generated_randomized_editing_selection_undo_conformance() {
        assert_law(
            &[0, 1, 7, 42, 99, u64::MAX],
            |rng: &mut Rng| {
                let corpus = texts(rng);
                let init0 = rng.choose(&corpus).expect("corpus").clone();
                let init1 = rng.choose(&corpus).expect("corpus").clone();
                let mut shadow = [init0.clone(), init1.clone()];
                let mut ops = Vec::new();
                let mut latest_undoable: Option<(ActorId, String, [String; 2])> = None;
                let mut latest_redoable: Option<(ActorId, String, [String; 2])> = None;

                for step in 0..24 {
                    let actor = if rng.gen_bool() {
                        ActorId::new(1)
                    } else {
                        ActorId::new(2)
                    };
                    let buf_idx = rng.gen_range(0..2);
                    let buffer = BufferId::new(buf_idx as u64);
                    let kind = rng.gen_range(0..8);
                    match kind {
                        0 => {
                            let pos = random_boundary(rng, &shadow[buf_idx]);
                            let text = random_replacement(rng);
                            let tag = format!("ins_{step}");
                            if !text.is_empty() {
                                let prev = shadow.clone();
                                shadow[buf_idx].insert_str(pos, &text);
                                latest_undoable = Some((actor, tag.clone(), prev));
                                latest_redoable = None;
                            }
                            ops.push(ScenarioOp::Insert {
                                actor,
                                buffer,
                                byte_offset: pos,
                                text,
                                tag: Some(tag),
                            });
                        }
                        1 => {
                            let a = random_boundary(rng, &shadow[buf_idx]);
                            let b = random_boundary(rng, &shadow[buf_idx]);
                            let (s, e) = (a.min(b), a.max(b));
                            let text = random_replacement(rng);
                            let tag = format!("rep_{step}");
                            let changed = shadow[buf_idx][s..e] != text;
                            if changed {
                                let prev = shadow.clone();
                                shadow[buf_idx].replace_range(s..e, &text);
                                latest_undoable = Some((actor, tag.clone(), prev));
                                latest_redoable = None;
                            }
                            ops.push(ScenarioOp::Replace {
                                actor,
                                buffer,
                                start_byte: s,
                                end_byte: e,
                                text,
                                reverse_emacs_mark: rng.gen_bool(),
                                tag: Some(tag),
                            });
                        }
                        2 => {
                            let mut group_edits = Vec::new();
                            let mut sim = shadow[buf_idx].clone();
                            let count = rng.gen_range(1..4);
                            for _ in 0..count {
                                let a = random_boundary(rng, &sim);
                                let b = random_boundary(rng, &sim);
                                let (s, e) = (a.min(b), a.max(b));
                                let text = random_replacement(rng);
                                sim.replace_range(s..e, &text);
                                group_edits.push(GroupEdit {
                                    start_byte: s,
                                    end_byte: e,
                                    text,
                                });
                            }
                            let tag = format!("grp_{step}");
                            if sim != shadow[buf_idx] {
                                let prev = shadow.clone();
                                shadow[buf_idx] = sim;
                                latest_undoable = Some((actor, tag.clone(), prev));
                                latest_redoable = None;
                            }
                            ops.push(ScenarioOp::ChangeGroup {
                                actor,
                                buffer,
                                edits: group_edits,
                                sequence_step: None,
                                tag: Some(tag),
                            });
                        }
                        3 => {
                            let a0 = random_boundary(rng, &shadow[0]);
                            let b0 = random_boundary(rng, &shadow[0]);
                            let (s0, e0) = (a0.min(b0), a0.max(b0));
                            let t0 = random_replacement(rng);
                            let a1 = random_boundary(rng, &shadow[1]);
                            let b1 = random_boundary(rng, &shadow[1]);
                            let (s1, e1) = (a1.min(b1), a1.max(b1));
                            let t1 = random_replacement(rng);
                            let changed = shadow[0][s0..e0] != t0 || shadow[1][s1..e1] != t1;
                            let tag = format!("mb_{step}");
                            if changed {
                                let prev = shadow.clone();
                                shadow[0].replace_range(s0..e0, &t0);
                                shadow[1].replace_range(s1..e1, &t1);
                                latest_undoable = Some((actor, tag.clone(), prev));
                                latest_redoable = None;
                            }
                            ops.push(ScenarioOp::MultiBufferBatch {
                                actor,
                                primary: BufferId::new(0),
                                edits: vec![
                                    SpanEdit {
                                        buffer: BufferId::new(0),
                                        start_byte: s0,
                                        end_byte: e0,
                                        text: t0,
                                    },
                                    SpanEdit {
                                        buffer: BufferId::new(1),
                                        start_byte: s1,
                                        end_byte: e1,
                                        text: t1,
                                    },
                                ],
                                mode: EndpointMode::Exclusive,
                                tag: Some(tag),
                            });
                        }
                        4 => {
                            let anchor = random_boundary(rng, &shadow[buf_idx]);
                            let head = random_boundary(rng, &shadow[buf_idx]);
                            let mode = if rng.gen_bool()
                                && (anchor != head || head == shadow[buf_idx].len())
                            {
                                EndpointMode::Inclusive
                            } else {
                                EndpointMode::Exclusive
                            };
                            ops.push(ScenarioOp::Select {
                                actor,
                                buffer,
                                anchor_byte: anchor,
                                head_byte: head,
                                mode,
                            });
                        }
                        5 => {
                            let a = random_boundary(rng, &shadow[buf_idx]);
                            let b = random_boundary(rng, &shadow[buf_idx]);
                            ops.push(ScenarioOp::SetAnnotation {
                                actor,
                                key: ann_key(buf_idx as u64, (step % 3) as u64 + 1),
                                start_byte: a.min(b),
                                end_byte: a.max(b),
                                start_affinity: if rng.gen_bool() {
                                    Affinity::Left
                                } else {
                                    Affinity::Right
                                },
                                end_affinity: if rng.gen_bool() {
                                    Affinity::Left
                                } else {
                                    Affinity::Right
                                },
                                priority: rng.gen_range(0..65536) as u32,
                                collapse: if rng.gen_bool() {
                                    CollapsePolicy::RetainEmpty
                                } else {
                                    CollapsePolicy::Evict
                                },
                                label: format!("lbl-{step}"),
                            });
                        }
                        6 => {
                            if let Some((owner, target_tag, prev_shadow)) = latest_undoable.take() {
                                let after_shadow = shadow.clone();
                                shadow = prev_shadow;
                                ops.push(ScenarioOp::Undo {
                                    actor: owner,
                                    target_tag: target_tag.clone(),
                                    use_resolved_candidate: rng.gen_bool(),
                                    tag: Some(format!("undo_{step}")),
                                });
                                latest_redoable = Some((owner, target_tag, after_shadow));
                            } else {
                                ops.push(ScenarioOp::ReadBuffer { actor, buffer });
                            }
                        }
                        _ => {
                            if let Some((owner, target_tag, next_shadow)) = latest_redoable.take() {
                                let prev_shadow = shadow.clone();
                                shadow = next_shadow;
                                ops.push(ScenarioOp::Redo {
                                    actor: owner,
                                    target_tag: target_tag.clone(),
                                    use_resolved_candidate: rng.gen_bool(),
                                    tag: Some(format!("redo_{step}")),
                                });
                                latest_undoable = Some((owner, target_tag, prev_shadow));
                            } else if let Some((_, ref last_tag, _)) = latest_undoable {
                                ops.push(ScenarioOp::RetryCommitted {
                                    tag: last_tag.clone(),
                                });
                            } else {
                                ops.push(ScenarioOp::ReadBuffer { actor, buffer });
                            }
                        }
                    }
                }
                (init0, init1, shadow, ops)
            },
            |(init0, init1, expected_shadow, ops)| {
                let reports = run_both_agent_modes(&[("b0", init0), ("b1", init1)], ops);
                for report in &reports {
                    assert_eq!(report.step_count, ops.len());
                    let obs = report.observation(AdapterKind::Emacs);
                    assert_eq!(obs.live_buffers[&BufferId::new(0)], expected_shadow[0]);
                    assert_eq!(obs.live_buffers[&BufferId::new(1)], expected_shadow[1]);
                    assert_eq!(
                        obs.projected_buffers[&BufferId::new(0)].1,
                        expected_shadow[0]
                    );
                    assert_eq!(
                        obs.projected_buffers[&BufferId::new(1)].1,
                        expected_shadow[1]
                    );
                }
            },
        );
    }

    #[test]
    fn generated_randomized_concurrent_conflict_compaction_resync_conformance() {
        let mut commits = 0usize;
        let mut conflicts = 0usize;
        let mut base_expired = 0usize;
        let mut resync_gaps = 0usize;
        let mut permission_denied = 0usize;
        let mut undo_commits = 0usize;
        let mut redo_commits = 0usize;

        assert_law(
            &[0, 1, 7, 42, 99, u64::MAX],
            |rng: &mut Rng| {
                let mut ops = Vec::new();
                let a1 = ActorId::new(1);
                let a2 = ActorId::new(2);
                let b0 = BufferId::new(0);
                let b1 = BufferId::new(1);

                // Fixed structure with randomized choices so every branch is hit deterministically
                // while coordinates remain valid across UTF-8/CJK/emoji/CRLF contents.
                let prefix_val = random_replacement(rng);
                let prefix_val = if prefix_val.is_empty() {
                    "界".to_owned()
                } else {
                    prefix_val
                };
                let suffix_val = random_replacement(rng);
                let suffix_val = if suffix_val.is_empty() {
                    "😀".to_owned()
                } else {
                    suffix_val
                };

                // Initial text on b0: "AA-界-BB-😀-CC\r\n" (len: 2+1+3+1+2+1+4+1+2+2 = 19)
                ops.push(ScenarioOp::Select {
                    actor: a1,
                    buffer: b0,
                    anchor_byte: 3,
                    head_byte: 10,
                    mode: EndpointMode::Exclusive,
                });
                ops.push(ScenarioOp::SaveSnapshot {
                    tag: "base0".into(),
                });
                ops.push(ScenarioOp::PauseLagging { buffer: b0 });

                // Edit 1 at prefix [0..2]
                ops.push(ScenarioOp::Replace {
                    actor: a1,
                    buffer: b0,
                    start_byte: 0,
                    end_byte: 2,
                    text: prefix_val.clone(),
                    reverse_emacs_mark: rng.gen_bool(),
                    tag: Some("e_prefix".into()),
                });
                // Disjoint concurrent edit at suffix [15..17] ("CC") from base0 -> rebases & commits!
                ops.push(ScenarioOp::EditAtSavedBase {
                    base_tag: "base0".into(),
                    actor: a2,
                    buffer: b0,
                    start_byte: 15,
                    end_byte: 17,
                    text: suffix_val.clone(),
                    tag: Some("e_disjoint".into()),
                });
                // Overlapping concurrent edit at [0..3] from base0 -> Conflict!
                ops.push(ScenarioOp::EditAtSavedBase {
                    base_tag: "base0".into(),
                    actor: a2,
                    buffer: b0,
                    start_byte: 0,
                    end_byte: 3,
                    text: "X".into(),
                    tag: None,
                });
                // Deliver latest to paused lagging client -> ResyncGap!
                ops.push(ScenarioOp::DeliverLatestToLagging { buffer: b0 });
                // Catch up via Events!
                ops.push(ScenarioOp::CatchUpLagging { buffer: b0 });

                // Undo and redo e_disjoint!
                ops.push(ScenarioOp::Undo {
                    actor: a2,
                    target_tag: "e_disjoint".into(),
                    use_resolved_candidate: rng.gen_bool(),
                    tag: Some("u_disjoint".into()),
                });
                ops.push(ScenarioOp::Redo {
                    actor: a2,
                    target_tag: "e_disjoint".into(),
                    use_resolved_candidate: rng.gen_bool(),
                    tag: Some("r_disjoint".into()),
                });

                // Pause lagging again, edit b1 and b0, compact, hit BaseExpired, catch up via Snapshot!
                ops.push(ScenarioOp::PauseLagging { buffer: b0 });
                ops.push(ScenarioOp::Insert {
                    actor: a1,
                    buffer: b1,
                    byte_offset: 0,
                    text: prefix_val,
                    tag: Some("b1_edit".into()),
                });
                ops.push(ScenarioOp::Insert {
                    actor: a1,
                    buffer: b0,
                    byte_offset: 0,
                    text: suffix_val,
                    tag: Some("b0_late".into()),
                });
                ops.push(ScenarioOp::Compact {
                    floor_offset_from_tip: 0,
                });
                ops.push(ScenarioOp::EditAtSavedBase {
                    base_tag: "base0".into(),
                    actor: a2,
                    buffer: b0,
                    start_byte: 7,
                    end_byte: 9,
                    text: "expired".into(),
                    tag: None,
                });
                ops.push(ScenarioOp::CatchUpLagging { buffer: b0 });

                // Permission denial boundary + recovery.
                ops.push(ScenarioOp::UpdatePolicy(PolicyChange::RevokeDefault(
                    Capability::Commit,
                )));
                ops.push(ScenarioOp::Insert {
                    actor: a2,
                    buffer: b0,
                    byte_offset: 0,
                    text: "blocked".into(),
                    tag: None,
                });
                ops.push(ScenarioOp::UpdatePolicy(PolicyChange::GrantDefault(
                    Capability::Commit,
                )));
                ops.push(ScenarioOp::RecoverViaRequest {
                    actor: a1,
                    buffer: b0,
                    observed_tag: "base0".into(),
                });
                ops
            },
            |ops| {
                let reports =
                    run_both_agent_modes(&[("b0", "AA-界-BB-😀-CC\r\n"), ("b1", "tail\n")], ops);
                let obs = reports[0].observation(AdapterKind::Emacs);
                for (idx, outcome) in obs.outcomes.iter().enumerate() {
                    match outcome {
                        StepOutcome::Committed { .. } => {
                            commits += 1;
                            if idx == 8 {
                                undo_commits += 1;
                            }
                            if idx == 9 {
                                redo_commits += 1;
                            }
                        }
                        StepOutcome::Rejected {
                            reason: RejectionReason::Conflict,
                            ..
                        } => conflicts += 1,
                        StepOutcome::Rejected {
                            reason: RejectionReason::BaseExpired,
                            ..
                        } => base_expired += 1,
                        StepOutcome::Rejected {
                            reason: RejectionReason::ResyncGap,
                            ..
                        } => resync_gaps += 1,
                        StepOutcome::Rejected {
                            reason: RejectionReason::PermissionDenied,
                            ..
                        } => permission_denied += 1,
                        _ => {}
                    }
                }
            },
        );

        assert!(commits > 0, "expected committed steps");
        assert!(conflicts > 0, "expected conflict rejections");
        assert!(base_expired > 0, "expected base-expired rejections");
        assert!(resync_gaps > 0, "expected resync-gap rejections");
        assert!(
            permission_denied > 0,
            "expected permission-denied rejections"
        );
        assert!(undo_commits > 0, "expected undo commits");
        assert!(redo_commits > 0, "expected redo commits");
    }

    #[test]
    fn negative_comparator_catches_text_position_revision_and_rejection_divergences() {
        let a1 = ActorId::new(1);
        let b0 = BufferId::new(0);
        let initial = InitialWorkspace::standard(&[("doc", "a界😀z\r\n")], CoordKind::LineColUtf16);
        let report = run_conformance(
            &initial,
            &[
                ScenarioOp::Select {
                    actor: a1,
                    buffer: b0,
                    anchor_byte: 1,
                    head_byte: 8,
                    mode: EndpointMode::Exclusive,
                },
                ScenarioOp::SetAnnotation {
                    actor: a1,
                    key: ann_key(0, 1),
                    start_byte: 1,
                    end_byte: 8,
                    start_affinity: Affinity::Left,
                    end_affinity: Affinity::Right,
                    priority: 7,
                    collapse: CollapsePolicy::RetainEmpty,
                    label: "tag".into(),
                },
                ScenarioOp::Insert {
                    actor: a1,
                    buffer: b0,
                    byte_offset: 0,
                    text: "prefix".into(),
                    tag: Some("ins".into()),
                },
                ScenarioOp::Invalid(InvalidOp::OutOfRangePosition {
                    actor: a1,
                    buffer: b0,
                }),
            ],
        )
        .expect("baseline conforms");
        assert!(verify_conformance(&report.observations).is_ok());

        // 1. Text divergence (authoritative buffer text).
        let mut text_bad = report.observations.clone();
        text_bad[1]
            .live_buffers
            .insert(b0, "prefixa界😀z\n".to_owned());
        let err = verify_conformance(&text_bad).unwrap_err();
        assert!(err.divergences.iter().any(|d| matches!(
            d,
            ConformanceDivergence::BufferText {
                left: AdapterKind::Emacs,
                right: AdapterKind::Neovim,
                buffer,
                ..
            } if *buffer == b0
        )));

        // 2. Projected native snapshot text divergence.
        let mut proj_bad = report.observations.clone();
        proj_bad[2]
            .projected_buffers
            .get_mut(&b0)
            .unwrap()
            .1
            .push('!');
        let err = verify_conformance(&proj_bad).unwrap_err();
        assert!(err.divergences.iter().any(|d| matches!(
            d,
            ConformanceDivergence::ProjectedBufferText {
                right: AdapterKind::Agent,
                buffer,
                ..
            } if *buffer == b0
        )));

        // 3. Lagging resync snapshot divergence.
        let mut lag_bad = report.observations.clone();
        lag_bad[0].lagging_buffers.get_mut(&b0).unwrap().1 = "stale".to_owned();
        let err = verify_conformance(&lag_bad).unwrap_err();
        assert!(err.divergences.iter().any(|d| matches!(
            d,
            ConformanceDivergence::LaggingBufferText {
                left: AdapterKind::Emacs,
                buffer,
                ..
            } if *buffer == b0
        )));

        // 4. Tombstone set divergence.
        let mut tomb_bad = report.observations.clone();
        tomb_bad[1].tombstoned_buffers.insert(BufferId::new(9));
        let err = verify_conformance(&tomb_bad).unwrap_err();
        assert!(err
            .divergences
            .iter()
            .any(|d| matches!(d, ConformanceDivergence::Tombstones { .. })));

        // 5. Cursor / selection position divergence.
        let mut pos_bad = report.observations.clone();
        pos_bad[2].selections.get_mut(&(a1, b0)).unwrap().head += 1;
        let err = verify_conformance(&pos_bad).unwrap_err();
        assert!(err.divergences.iter().any(|d| matches!(
            d,
            ConformanceDivergence::Selection {
                right: AdapterKind::Agent,
                actor,
                buffer,
                ..
            } if *actor == a1 && *buffer == b0
        )));

        // 6. Annotation span divergence.
        let mut ann_bad = report.observations.clone();
        ann_bad[1]
            .annotations
            .get_mut(&(a1, ann_key(0, 1)))
            .unwrap()
            .end -= 1;
        let err = verify_conformance(&ann_bad).unwrap_err();
        assert!(err.divergences.iter().any(|d| matches!(
            d,
            ConformanceDivergence::Annotation {
                right: AdapterKind::Neovim,
                ..
            }
        )));

        // 7. Revision number divergence.
        let mut rev_bad = report.observations.clone();
        rev_bad[0].revision = rev_bad[0].revision.next().unwrap();
        let err = verify_conformance(&rev_bad).unwrap_err();
        assert!(err.divergences.iter().any(|d| matches!(
            d,
            ConformanceDivergence::Revision {
                left: AdapterKind::Emacs,
                right: AdapterKind::Neovim,
                ..
            }
        )));

        // 8. Receipt and change-group divergence.
        let mut receipt_bad = report.observations.clone();
        let altered_intent =
            EditIntent::for_actor(ActorId::new(2), Nonce::new(999), Revision::new(1));
        let altered_receipt = CommitReceipt::new(altered_intent, Revision::new(1)).unwrap();
        receipt_bad[1].receipts[0] = altered_receipt;
        receipt_bad[1].change_groups[0] = altered_receipt;
        let err = verify_conformance(&receipt_bad).unwrap_err();
        assert!(err
            .divergences
            .iter()
            .any(|d| matches!(d, ConformanceDivergence::Receipt { index: 0, .. })));
        assert!(err
            .divergences
            .iter()
            .any(|d| matches!(d, ConformanceDivergence::ChangeGroups { .. })));

        // 9. Rejection-reason divergence (both rejected, different reasons).
        let mut rej_bad = report.observations.clone();
        rej_bad[2].outcomes[3] = StepOutcome::Rejected {
            reason: RejectionReason::InvalidCoordinate,
            revision: rej_bad[2].revision,
        };
        let err = verify_conformance(&rej_bad).unwrap_err();
        assert!(err.divergences.iter().any(|d| matches!(
            d,
            ConformanceDivergence::RejectionReason {
                right: AdapterKind::Agent,
                step: 3,
                left_reason: RejectionReason::OutOfRange,
                right_reason: RejectionReason::InvalidCoordinate,
                ..
            }
        )));

        // 10. Accept-vs-reject outcome divergence.
        let mut outcome_bad = report.observations.clone();
        outcome_bad[0].outcomes[3] = StepOutcome::Applied {
            revision: outcome_bad[0].revision,
        };
        let err = verify_conformance(&outcome_bad).unwrap_err();
        assert!(err.divergences.iter().any(|d| matches!(
            d,
            ConformanceDivergence::StepOutcome {
                left: AdapterKind::Emacs,
                step: 3,
                ..
            }
        )));

        // 11. End-to-end real adapter divergence: establish a shared selection, then run Emacs
        // with different edits and an extra commit compared to Neovim/Agent, confirming
        // verify_conformance catches text, projected text, selection, and revision divergences
        // together.
        let (cp, pol) = build_initial_checkpoint(&initial).unwrap();
        let mut em = AdapterHarness::new(AdapterKind::Emacs, CoordKind::Byte, &cp, pol.clone());
        let mut nv = AdapterHarness::new(AdapterKind::Neovim, CoordKind::Byte, &cp, pol.clone());
        let mut ag = AdapterHarness::new(AdapterKind::Agent, CoordKind::Byte, &cp, pol);
        let shared_select = ScenarioOp::Select {
            actor: a1,
            buffer: b0,
            anchor_byte: 1,
            head_byte: 8,
            mode: EndpointMode::Exclusive,
        };
        em.run_step(0, &shared_select).unwrap();
        nv.run_step(0, &shared_select).unwrap();
        ag.run_step(0, &shared_select).unwrap();
        em.run_step(
            1,
            &ScenarioOp::Insert {
                actor: a1,
                buffer: b0,
                byte_offset: 0,
                text: "EMACS_ONLY".into(),
                tag: None,
            },
        )
        .unwrap();
        nv.run_step(
            1,
            &ScenarioOp::Insert {
                actor: a1,
                buffer: b0,
                byte_offset: 0,
                text: "OTHER".into(),
                tag: None,
            },
        )
        .unwrap();
        ag.run_step(
            1,
            &ScenarioOp::Insert {
                actor: a1,
                buffer: b0,
                byte_offset: 0,
                text: "OTHER".into(),
                tag: None,
            },
        )
        .unwrap();
        // Advance Emacs by one additional revision while Neovim and Agent perform a read-only op.
        em.run_step(
            2,
            &ScenarioOp::Insert {
                actor: a1,
                buffer: b0,
                byte_offset: 0,
                text: "!".into(),
                tag: None,
            },
        )
        .unwrap();
        let read_op = ScenarioOp::ReadBuffer {
            actor: a1,
            buffer: b0,
        };
        nv.run_step(2, &read_op).unwrap();
        ag.run_step(2, &read_op).unwrap();
        let err = verify_conformance(&[em.observe(), nv.observe(), ag.observe()]).unwrap_err();
        assert!(err.divergences.iter().any(|d| matches!(
            d,
            ConformanceDivergence::BufferText {
                left: AdapterKind::Emacs,
                right: AdapterKind::Neovim,
                buffer,
                ..
            } if *buffer == b0
        )));
        assert!(err.divergences.iter().any(|d| matches!(
            d,
            ConformanceDivergence::ProjectedBufferText {
                left: AdapterKind::Emacs,
                right: AdapterKind::Agent,
                buffer,
                ..
            } if *buffer == b0
        )));
        assert!(err.divergences.iter().any(|d| matches!(
            d,
            ConformanceDivergence::Selection {
                left: AdapterKind::Emacs,
                right: AdapterKind::Neovim,
                actor,
                buffer,
                ..
            } if *actor == a1 && *buffer == b0
        )));
        assert!(err.divergences.iter().any(|d| matches!(
            d,
            ConformanceDivergence::Selection {
                left: AdapterKind::Emacs,
                right: AdapterKind::Agent,
                actor,
                buffer,
                ..
            } if *actor == a1 && *buffer == b0
        )));
        assert!(err.divergences.iter().any(|d| matches!(
            d,
            ConformanceDivergence::Revision {
                left: AdapterKind::Emacs,
                right: AdapterKind::Neovim,
                left_revision,
                right_revision,
            } if left_revision != right_revision
        )));
        assert!(err.divergences.iter().any(|d| matches!(
            d,
            ConformanceDivergence::Revision {
                left: AdapterKind::Emacs,
                right: AdapterKind::Agent,
                left_revision,
                right_revision,
            } if left_revision != right_revision
        )));
        assert!(err.to_string().contains("interop conformance failure"));
    }

    #[test]
    fn negative_rejection_state_drift_detector_trips_on_mutation() {
        let a1 = ActorId::new(1);
        let b0 = BufferId::new(0);
        let initial = InitialWorkspace::standard(&[("doc", "abc")], CoordKind::Byte);
        let (cp, pol) = build_initial_checkpoint(&initial).unwrap();
        let rejecting_op = ScenarioOp::Invalid(InvalidOp::OutOfRangePosition {
            actor: a1,
            buffer: b0,
        });

        // Clean rejection with no injected mutation must succeed (catches inverted drift checks).
        let mut clean = AdapterHarness::new(AdapterKind::Emacs, CoordKind::Byte, &cp, pol.clone());
        clean
            .run_step(0, &rejecting_op)
            .expect("clean rejection must not report drift");
        assert!(matches!(
            clean.outcomes.as_slice(),
            [StepOutcome::Rejected {
                reason: RejectionReason::OutOfRange,
                ..
            }]
        ));

        let mut clean_harnesses = [
            AdapterHarness::new(AdapterKind::Emacs, CoordKind::Byte, &cp, pol.clone()),
            AdapterHarness::new(AdapterKind::Neovim, CoordKind::Byte, &cp, pol.clone()),
            AdapterHarness::new(AdapterKind::Agent, CoordKind::Byte, &cp, pol.clone()),
        ];
        let clean_ops = [
            ScenarioOp::Insert {
                actor: a1,
                buffer: b0,
                byte_offset: 0,
                text: "ok".into(),
                tag: None,
            },
            rejecting_op.clone(),
        ];
        let clean_report = run_conformance_harnesses(&mut clean_harnesses, &clean_ops)
            .expect("clean conformance run with rejection must pass");
        assert_eq!(clean_report.step_count, 2);

        fn synthetic_receipt(nonce: u64) -> CommitReceipt {
            let intent =
                EditIntent::for_actor(ActorId::new(1), Nonce::new(nonce), Revision::new(1));
            CommitReceipt::new(intent, Revision::new(1)).expect("valid synthetic receipt")
        }

        // Exercise isolated state drift across every harness state component via the production
        // AdapterHarness::run_step drift detector.
        type DriftMutator = fn(&mut AdapterHarness);
        let cases: [(&str, AdapterKind, usize, DriftMutator); 12] = [
            ("coordinator", AdapterKind::Emacs, 0, |h| {
                h.coordinator.access_mut().revoke_default(Capability::Read);
            }),
            ("policy", AdapterKind::Neovim, 1, |h| {
                h.policy.revoke_default(Capability::Read);
            }),
            ("session", AdapterKind::Agent, 2, |h| {
                let snap = h.coordinator.snapshot();
                let text = snap.catalog().get(BufferId::new(0)).expect("live buffer");
                let span = ByteSpan::validate_in(text, 0, 1).expect("valid span");
                let view =
                    BufferView::narrowed(snap.catalog(), BufferId::new(0), span).expect("view");
                h.session.set_view(ActorId::new(1), view).expect("set view");
            }),
            ("clients.lagging_paused", AdapterKind::Emacs, 3, |h| {
                h.clients
                    .get_mut(&BufferId::new(0))
                    .expect("buffer client")
                    .lagging_paused = true;
            }),
            ("clients.native", AdapterKind::Neovim, 4, |h| {
                h.clients
                    .get_mut(&BufferId::new(0))
                    .expect("buffer client")
                    .native = None;
            }),
            ("group_plan", AdapterKind::Agent, 5, |h| {
                h.group_plan
                    .project(SubmitOutcome::Committed(synthetic_receipt(701)), None)
                    .expect("project synthetic receipt");
            }),
            ("receipts", AdapterKind::Emacs, 6, |h| {
                h.receipts.push(synthetic_receipt(702));
            }),
            ("outcomes", AdapterKind::Neovim, 7, |h| {
                let rev = h.coordinator.snapshot().revision();
                h.outcomes.push(StepOutcome::Applied { revision: rev });
            }),
            ("saved_snapshots", AdapterKind::Agent, 8, |h| {
                let snap = h.coordinator.snapshot();
                h.saved_snapshots.insert("leaked".into(), snap);
            }),
            ("tagged_commits", AdapterKind::Emacs, 9, |h| {
                let intent =
                    EditIntent::for_actor(ActorId::new(1), Nonce::new(703), Revision::new(1));
                let txn = Transaction::new(intent, Vec::new()).expect("empty txn");
                h.tagged_commits.insert(
                    "leaked".into(),
                    RecordedCommit {
                        request: RequestId::new(703),
                        operation: CoordinatorOperation::Edit(txn),
                        txn: intent.txn(),
                    },
                );
            }),
            ("proposal_requests", AdapterKind::Neovim, 10, |h| {
                h.proposal_requests
                    .insert("leaked".into(), RequestId::new(704));
            }),
            ("annotation_keys", AdapterKind::Agent, 11, |h| {
                h.annotation_keys.insert((ActorId::new(1), ann_key(0, 99)));
            }),
        ];

        for (component, adapter, step, mutate) in cases {
            let mut harness = AdapterHarness::new(adapter, CoordKind::Byte, &cp, pol.clone());
            harness.set_after_reject_hook(mutate);
            let Err(err) = harness.run_step(step, &rejecting_op) else {
                panic!("injected rejection drift in {component} must fail run_step");
            };
            assert_eq!(
                err.divergences,
                vec![ConformanceDivergence::RejectionStateDrift { adapter, step }],
                "unexpected divergence for {component}"
            );
            assert!(err.to_string().contains("RejectionStateDrift"));
            assert!(err.to_string().contains("interop conformance failure"));
        }

        // End-to-end conformance runner path: hooks do not fire on accepted step 0, then trip
        // RejectionStateDrift at step 1 across each mutated adapter.
        let mut harnesses = [
            AdapterHarness::new(AdapterKind::Emacs, CoordKind::Byte, &cp, pol.clone()),
            AdapterHarness::new(AdapterKind::Neovim, CoordKind::Byte, &cp, pol.clone()),
            AdapterHarness::new(AdapterKind::Agent, CoordKind::Byte, &cp, pol),
        ];
        harnesses[1].set_after_reject_hook(|h| {
            h.coordinator.access_mut().revoke_default(Capability::Read);
        });
        harnesses[2].set_after_reject_hook(move |h| {
            h.clients
                .get_mut(&b0)
                .expect("buffer client")
                .lagging_paused = true;
        });
        let err = run_conformance_harnesses(&mut harnesses, &clean_ops)
            .expect_err("injected rejection drift must fail conformance run");
        assert_eq!(
            err.divergences,
            vec![
                ConformanceDivergence::RejectionStateDrift {
                    adapter: AdapterKind::Neovim,
                    step: 1,
                },
                ConformanceDivergence::RejectionStateDrift {
                    adapter: AdapterKind::Agent,
                    step: 1,
                },
            ]
        );
        assert!(err.to_string().contains("RejectionStateDrift"));
        assert!(err.to_string().contains("interop conformance failure"));
    }
}
