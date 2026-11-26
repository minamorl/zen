//! Bounded, permission-scoped, single-snapshot workspace context extraction.
//!
//! Extraction reads exclusively from one immutable [`WorkspaceSnapshot`] (or one
//! [`WorkspaceSession`] pinned to its current snapshot), so every emitted
//! fragment comes from the exact same revision without tearing across edits.
//!
//! # Laws
//!
//! * **Single-snapshot consistency:** every emitted [`ContextFragment`] carries
//!   the exact [`Revision`] of the supplied snapshot or session, matching
//!   [`ContextPack::revision`].
//! * **Permission and scope containment:** every participant must be active in
//!   [`ActorRegistry`] and hold [`Capability::Read`] for every emitted buffer,
//!   and every emitted span must lie strictly within the intersection of all
//!   participants' [`ContextScope`]s (and session [`BufferView`]s when reading
//!   from a [`WorkspaceSession`]). Under [`ScopeMode::Strict`], unauthorized,
//!   unknown, closed, or out-of-scope targets fail with typed errors; under
//!   [`ScopeMode::Filter`], they are excluded or clipped without leaking
//!   unpermitted text.
//! * **Byte budget and UTF-8 boundary safety:** total emitted bytes across all
//!   fragments never exceed [`ContextBudget::max_total_bytes`], each fragment
//!   respects optional per-fragment limits, fragment counts respect optional
//!   count limits, and truncation always stops on a UTF-8 scalar boundary.
//! * **Non-interference:** extraction inspects only bytes inside the effective
//!   permitted spans of permitted buffers and never retains unpermitted source
//!   text. Modifying text strictly outside the permitted scope produces
//!   identical extracted fragments, spans, text, truncation flags, and metadata.
//! * **Deterministic canonicalization:** targets and participants are ordered
//!   canonically, overlapping or touching spans in the same buffer are merged
//!   into strictly separated fragments, and invalid budgets, stale views,
//!   closed buffers, and out-of-bounds ranges return typed errors.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::editor::{
    collab::{
        access::{AccessError, AccessPolicy, Capability},
        registry::ActorRegistry,
    },
    edit::ids::{ActorId, BufferId},
    text::{
        clip::{self, ClipError},
        lines::LineIndex,
        span::{ByteSpan, SpanError},
        utf8::Text,
    },
    workspace::{
        session::{SessionError, WorkspaceSession},
        snapshots::WorkspaceSnapshot,
        version::Revision,
        view::BufferView,
    },
};

/// A budget configuration failed validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetError {
    /// The total byte budget must be at least one byte.
    ZeroTotalBytes,
    /// A per-fragment byte limit must be at least one byte.
    ZeroFragmentBytes,
    /// A maximum fragment count must be at least one.
    ZeroFragmentLimit,
    /// The per-fragment byte limit exceeds the total byte budget.
    FragmentExceedsTotal {
        max_fragment_bytes: usize,
        max_total_bytes: usize,
    },
}

impl fmt::Display for BudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::ZeroTotalBytes => f.write_str("total byte budget must be at least 1"),
            Self::ZeroFragmentBytes => f.write_str("per-fragment byte limit must be at least 1"),
            Self::ZeroFragmentLimit => f.write_str("maximum fragment count must be at least 1"),
            Self::FragmentExceedsTotal {
                max_fragment_bytes,
                max_total_bytes,
            } => write!(
                f,
                "per-fragment limit {max_fragment_bytes} exceeds total budget {max_total_bytes}"
            ),
        }
    }
}

impl std::error::Error for BudgetError {}

/// Byte and fragment bounds for context extraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContextBudget {
    /// Maximum total UTF-8 bytes emitted across all fragments.
    pub max_total_bytes: usize,
    /// Optional maximum UTF-8 bytes emitted for any single fragment.
    pub max_fragment_bytes: Option<usize>,
    /// Optional maximum number of fragments emitted in the pack.
    pub max_fragments: Option<usize>,
}

impl ContextBudget {
    /// Creates a validated budget bounded only by total UTF-8 bytes.
    pub fn new(max_total_bytes: usize) -> Result<Self, BudgetError> {
        Self::with_limits(max_total_bytes, None, None)
    }

    /// Creates a validated budget with optional per-fragment and count limits.
    pub fn with_limits(
        max_total_bytes: usize,
        max_fragment_bytes: Option<usize>,
        max_fragments: Option<usize>,
    ) -> Result<Self, BudgetError> {
        let budget = Self {
            max_total_bytes,
            max_fragment_bytes,
            max_fragments,
        };
        budget.validate()?;
        Ok(budget)
    }

    /// Validates all configured limits.
    pub fn validate(&self) -> Result<(), BudgetError> {
        if self.max_total_bytes == 0 {
            return Err(BudgetError::ZeroTotalBytes);
        }
        if let Some(limit) = self.max_fragment_bytes {
            if limit == 0 {
                return Err(BudgetError::ZeroFragmentBytes);
            }
            if limit > self.max_total_bytes {
                return Err(BudgetError::FragmentExceedsTotal {
                    max_fragment_bytes: limit,
                    max_total_bytes: self.max_total_bytes,
                });
            }
        }
        if let Some(0) = self.max_fragments {
            return Err(BudgetError::ZeroFragmentLimit);
        }
        Ok(())
    }
}

/// Handling policy for unauthorized, closed, unknown, or out-of-scope targets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScopeMode {
    /// Fail with a typed error when any target violates access or scope bounds.
    #[default]
    Strict,
    /// Exclude unpermitted buffers and clip spans to the permitted scope.
    Filter,
}

fn merge_spans(mut spans: Vec<ByteSpan>) -> Vec<ByteSpan> {
    spans.sort_by_key(|span| (span.start(), span.end()));
    let mut merged: Vec<ByteSpan> = Vec::new();
    for span in spans {
        if let Some(last) = merged.last_mut() {
            if span.start() <= last.end() {
                let end = last.end().max(span.end());
                *last = ByteSpan::new(last.start(), end).expect("ordered endpoints");
                continue;
            }
        }
        merged.push(span);
    }
    merged
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum BufferScopeKind {
    Full,
    Spans(Vec<ByteSpan>),
}

/// Canonical permitted range set within a single buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferScope {
    kind: BufferScopeKind,
}

impl BufferScope {
    /// Permits the entire live buffer.
    pub fn full() -> Self {
        Self {
            kind: BufferScopeKind::Full,
        }
    }

    /// Permits nothing in the buffer.
    pub fn empty() -> Self {
        Self {
            kind: BufferScopeKind::Spans(Vec::new()),
        }
    }

    /// Permits a single span in the buffer.
    pub fn span(span: ByteSpan) -> Self {
        Self {
            kind: BufferScopeKind::Spans(vec![span]),
        }
    }

    /// Permits the canonical union of the supplied spans.
    pub fn spans(spans: impl IntoIterator<Item = ByteSpan>) -> Self {
        Self {
            kind: BufferScopeKind::Spans(merge_spans(spans.into_iter().collect())),
        }
    }

    /// Returns whether the entire buffer is permitted.
    pub fn is_full(&self) -> bool {
        matches!(self.kind, BufferScopeKind::Full)
    }

    /// Returns whether no spans are permitted.
    pub fn is_empty(&self) -> bool {
        matches!(&self.kind, BufferScopeKind::Spans(spans) if spans.is_empty())
    }

    /// Returns the canonical permitted spans, or `None` if the full buffer is permitted.
    pub fn as_spans(&self) -> Option<&[ByteSpan]> {
        match &self.kind {
            BufferScopeKind::Full => None,
            BufferScopeKind::Spans(spans) => Some(spans),
        }
    }

    /// Returns whether `span` is completely contained in this scope.
    pub fn contains_span(&self, span: ByteSpan) -> bool {
        match &self.kind {
            BufferScopeKind::Full => true,
            BufferScopeKind::Spans(spans) => {
                spans.iter().any(|allowed| allowed.contains_span(span))
            }
        }
    }

    /// Computes the union of two buffer scopes.
    pub fn union(&self, other: &Self) -> Self {
        match (&self.kind, &other.kind) {
            (BufferScopeKind::Full, _) | (_, BufferScopeKind::Full) => Self::full(),
            (BufferScopeKind::Spans(left), BufferScopeKind::Spans(right)) => {
                Self::spans(left.iter().chain(right.iter()).copied())
            }
        }
    }

    /// Computes the closed intersection of two buffer scopes.
    ///
    /// Closed intersection is commutative, associative, and idempotent, matching
    /// [`BufferView::narrow_intersect`].
    pub fn intersect(&self, other: &Self) -> Self {
        match (&self.kind, &other.kind) {
            (BufferScopeKind::Full, _) => other.clone(),
            (_, BufferScopeKind::Full) => self.clone(),
            (BufferScopeKind::Spans(left), BufferScopeKind::Spans(right)) => {
                let mut spans = Vec::new();
                for &a in left {
                    for &b in right {
                        if let Some(overlap) = a.intersect_closed(b) {
                            spans.push(overlap);
                        }
                    }
                }
                Self::spans(spans)
            }
        }
    }
}

/// Caller-permitted buffer and span scope layered on top of [`AccessPolicy`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextScope {
    buffers: Option<BTreeMap<BufferId, BufferScope>>,
}

impl ContextScope {
    /// Permits all live buffers readable under [`AccessPolicy`].
    pub fn all_readable() -> Self {
        Self { buffers: None }
    }

    /// Permits no buffers or spans.
    pub fn empty() -> Self {
        Self {
            buffers: Some(BTreeMap::new()),
        }
    }

    /// Permits only the listed buffers in full.
    pub fn from_buffers(buffers: impl IntoIterator<Item = BufferId>) -> Self {
        let map = buffers
            .into_iter()
            .map(|buffer| (buffer, BufferScope::full()))
            .collect();
        Self { buffers: Some(map) }
    }

    /// Permits only the visible spans of the supplied narrowed views.
    pub fn from_views(views: impl IntoIterator<Item = BufferView>) -> Self {
        let mut scope = Self::empty();
        for view in views {
            scope.allow_view(view);
        }
        scope
    }

    /// Adds a full buffer to a restricted scope. No-op if already `all_readable`.
    pub fn allow_buffer(&mut self, buffer: BufferId) -> &mut Self {
        if let Some(map) = &mut self.buffers {
            map.insert(buffer, BufferScope::full());
        }
        self
    }

    /// Adds a narrowed view's visible span to a restricted scope. No-op if already `all_readable`.
    pub fn allow_view(&mut self, view: BufferView) -> &mut Self {
        self.allow_span(view.buffer(), view.visible())
    }

    /// Adds a single span to a restricted scope. No-op if already `all_readable`.
    pub fn allow_span(&mut self, buffer: BufferId, span: ByteSpan) -> &mut Self {
        if let Some(map) = &mut self.buffers {
            let next = BufferScope::span(span);
            map.entry(buffer)
                .and_modify(|existing| *existing = existing.union(&next))
                .or_insert(next);
        }
        self
    }

    /// Returns whether no extra buffer/span narrowing is configured beyond [`AccessPolicy`].
    pub fn is_all_readable(&self) -> bool {
        self.buffers.is_none()
    }

    /// Returns whether no buffers are permitted.
    pub fn is_empty(&self) -> bool {
        matches!(&self.buffers, Some(map) if map.is_empty())
    }

    /// Returns whether `buffer` is included in this scope.
    pub fn allows_buffer(&self, buffer: BufferId) -> bool {
        match &self.buffers {
            None => true,
            Some(map) => map.contains_key(&buffer),
        }
    }

    /// Returns the permitted [`BufferScope`] for `buffer`, if permitted.
    pub fn buffer_scope(&self, buffer: BufferId) -> Option<BufferScope> {
        match &self.buffers {
            None => Some(BufferScope::full()),
            Some(map) => map.get(&buffer).cloned(),
        }
    }

    /// Returns explicitly scoped buffer identifiers in ascending order, or `None` if `all_readable`.
    pub fn scoped_buffers(&self) -> Option<Vec<BufferId>> {
        self.buffers
            .as_ref()
            .map(|map| map.keys().copied().collect())
    }

    /// Computes the intersection of two scopes.
    ///
    /// Intersection forms a bounded semilattice with [`ContextScope::all_readable`]
    /// as identity and [`ContextScope::empty`] as absorbing element.
    pub fn intersect(&self, other: &Self) -> Self {
        match (&self.buffers, &other.buffers) {
            (None, _) => other.clone(),
            (_, None) => self.clone(),
            (Some(left), Some(right)) => {
                let mut result = BTreeMap::new();
                for (&buffer, left_scope) in left {
                    if let Some(right_scope) = right.get(&buffer) {
                        let overlap = left_scope.intersect(right_scope);
                        if !overlap.is_empty() {
                            result.insert(buffer, overlap);
                        }
                    }
                }
                Self {
                    buffers: Some(result),
                }
            }
        }
    }
}

/// An actor and their permitted scope for a shared human/agent context pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParticipantScope {
    /// Active participant identifier.
    pub actor: ActorId,
    /// Scope restrictions for this participant.
    pub scope: ContextScope,
}

impl ParticipantScope {
    /// Pairs an actor with an explicit context scope.
    pub fn new(actor: ActorId, scope: ContextScope) -> Self {
        Self { actor, scope }
    }

    /// Pairs an actor with all policy-readable live buffers.
    pub fn all_readable(actor: ActorId) -> Self {
        Self {
            actor,
            scope: ContextScope::all_readable(),
        }
    }
}

/// A unit of context requested from the snapshot or session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextTarget {
    /// All live buffers permitted by the effective scope and policy.
    Workspace,
    /// All permitted spans of the given buffer.
    Buffer(BufferId),
    /// The visible range of a narrowed buffer view.
    View(BufferView),
    /// An explicit validated byte span in a buffer.
    Span { buffer: BufferId, span: ByteSpan },
    /// An explicit raw half-open byte range `[start, end)` in a buffer.
    ByteRange {
        buffer: BufferId,
        start: usize,
        end: usize,
    },
    /// A half-open line range `[start_line, end_line)` within a contiguous permitted buffer scope.
    LineRange {
        buffer: BufferId,
        start_line: usize,
        end_line: usize,
    },
}

/// Parameters for a context extraction query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextRequest {
    /// Requested targets; normalized deterministically before extraction.
    pub targets: Vec<ContextTarget>,
    /// Byte and fragment budget bounds.
    pub budget: ContextBudget,
    /// Strict failure or non-leaking filtering for unpermitted targets.
    pub mode: ScopeMode,
    /// Optional expected snapshot revision; mismatches fail before reading text.
    pub expected_revision: Option<Revision>,
}

impl ContextRequest {
    /// Creates a strict extraction request for the given targets and budget.
    pub fn new(targets: Vec<ContextTarget>, budget: ContextBudget) -> Self {
        Self {
            targets,
            budget,
            mode: ScopeMode::Strict,
            expected_revision: None,
        }
    }

    /// Creates a filtered whole-workspace request with the given budget.
    pub fn workspace(budget: ContextBudget) -> Self {
        Self {
            targets: vec![ContextTarget::Workspace],
            budget,
            mode: ScopeMode::Filter,
            expected_revision: None,
        }
    }

    /// Sets the scope handling mode.
    pub fn with_mode(mut self, mode: ScopeMode) -> Self {
        self.mode = mode;
        self
    }

    /// Pins extraction to an exact snapshot revision.
    pub fn at_revision(mut self, revision: Revision) -> Self {
        self.expected_revision = Some(revision);
        self
    }
}

/// One bounded, permission-checked text fragment from a single snapshot revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextFragment {
    /// Snapshot revision from which this fragment was read.
    pub revision: Revision,
    /// Buffer containing this fragment.
    pub buffer: BufferId,
    /// Exact emitted byte span in original buffer coordinates.
    pub span: ByteSpan,
    /// Extracted UTF-8 text; never retains bytes outside `span`.
    pub text: Text,
    /// Whether bytes were omitted from this fragment due to budget limits.
    pub truncated: bool,
    /// Number of permitted requested bytes omitted from the tail of this fragment.
    pub omitted_bytes: usize,
}

impl ContextFragment {
    /// Returns the emitted UTF-8 byte length.
    pub fn len_bytes(&self) -> usize {
        self.text.len_bytes()
    }

    /// Returns whether the emitted text is empty.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Borrows the emitted UTF-8 text.
    pub fn as_str(&self) -> &str {
        self.text.as_str()
    }
}

/// A deterministic, bounded, single-snapshot context pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextPack {
    /// Single snapshot revision shared by every emitted fragment.
    pub revision: Revision,
    /// Sorted, deduplicated active actors permitted to read the entire pack.
    pub participants: Vec<ActorId>,
    /// Budget bounds applied during extraction.
    pub budget: ContextBudget,
    /// Canonically ordered, non-overlapping, strictly separated fragments.
    pub fragments: Vec<ContextFragment>,
    /// Sum of emitted UTF-8 byte lengths across `fragments`.
    pub total_bytes: usize,
    /// Whether any fragment was truncated or omitted due to budget limits.
    pub truncated: bool,
    /// Number of permitted candidate fragments omitted due to budget limits.
    pub omitted_fragments: usize,
}

impl ContextPack {
    /// Returns whether no fragments were emitted.
    pub fn is_empty(&self) -> bool {
        self.fragments.is_empty()
    }

    /// Compares all extracted payload and metadata independent of snapshot revision.
    pub fn same_content(&self, other: &Self) -> bool {
        self.participants == other.participants
            && self.budget == other.budget
            && self.total_bytes == other.total_bytes
            && self.truncated == other.truncated
            && self.omitted_fragments == other.omitted_fragments
            && self.fragments.len() == other.fragments.len()
            && self
                .fragments
                .iter()
                .zip(&other.fragments)
                .all(|(left, right)| {
                    left.buffer == right.buffer
                        && left.span == right.span
                        && left.text == right.text
                        && left.truncated == right.truncated
                        && left.omitted_bytes == right.omitted_bytes
                })
    }
}

/// Context extraction failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextError {
    /// The supplied budget violates budget invariants.
    InvalidBudget(BudgetError),
    /// Shared extraction received no participants.
    EmptyParticipants,
    /// The snapshot revision did not match the pinned expected revision.
    RevisionMismatch {
        expected: Revision,
        actual: Revision,
    },
    /// Actor or buffer authorization failed.
    Access(AccessError),
    /// The requested buffer lies outside the caller-permitted scope.
    BufferOutsideScope { buffer: BufferId },
    /// The requested byte range is not contained in the permitted scope.
    SpanOutsideScope {
        buffer: BufferId,
        requested_start: usize,
        requested_end: usize,
    },
    /// A supplied or scoped view is no longer valid against the snapshot's text.
    StaleView {
        buffer: BufferId,
        span: ByteSpan,
        error: SpanError,
    },
    /// A requested byte span or byte range is invalid in the permitted text.
    InvalidSpan { buffer: BufferId, error: SpanError },
    /// A requested line range is inverted or exceeds the permitted line count.
    InvalidLineRange { buffer: BufferId, error: ClipError },
    /// A line range cannot be resolved unambiguously across multiple disjoint permitted spans.
    DisjointScopeForLineRange { buffer: BufferId },
    /// `ContextTarget::Workspace` with an unrestricted scope requires a snapshot rather than a session.
    WorkspaceTargetUnsupportedForSession,
}

impl fmt::Display for ContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBudget(error) => error.fmt(f),
            Self::EmptyParticipants => f.write_str("context extraction requires at least one actor"),
            Self::RevisionMismatch { expected, actual } => {
                write!(
                    f,
                    "snapshot revision {actual} does not match expected {expected}"
                )
            }
            Self::Access(error) => error.fmt(f),
            Self::BufferOutsideScope { buffer } => {
                write!(f, "buffer {buffer} is outside the permitted scope")
            }
            Self::SpanOutsideScope {
                buffer,
                requested_start,
                requested_end,
            } => write!(
                f,
                "range {requested_start}..{requested_end} in buffer {buffer} is outside the permitted scope"
            ),
            Self::StaleView {
                buffer,
                span,
                error,
            } => write!(f, "stale view {span:?} in buffer {buffer}: {error}"),
            Self::InvalidSpan { buffer, error } => {
                write!(f, "invalid span in buffer {buffer}: {error}")
            }
            Self::InvalidLineRange { buffer, error } => {
                write!(f, "invalid line range in buffer {buffer}: {error}")
            }
            Self::DisjointScopeForLineRange { buffer } => write!(
                f,
                "line range in buffer {buffer} requires a single contiguous permitted scope"
            ),
            Self::WorkspaceTargetUnsupportedForSession => {
                f.write_str("unscoped workspace target is not supported for session extraction")
            }
        }
    }
}

impl std::error::Error for ContextError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidBudget(error) => Some(error),
            Self::Access(error) => Some(error),
            Self::StaleView { error, .. } | Self::InvalidSpan { error, .. } => Some(error),
            Self::InvalidLineRange { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Extracts a bounded, permission-scoped context pack for one actor from a single snapshot.
pub fn extract_context(
    snapshot: &WorkspaceSnapshot,
    registry: &ActorRegistry,
    policy: &AccessPolicy,
    actor: ActorId,
    scope: &ContextScope,
    request: &ContextRequest,
) -> Result<ContextPack, ContextError> {
    extract_shared_context(
        snapshot,
        registry,
        policy,
        &[ParticipantScope::new(actor, scope.clone())],
        request,
    )
}

/// Extracts all permitted live workspace buffers for one actor within the given scope and budget.
pub fn extract_workspace_context(
    snapshot: &WorkspaceSnapshot,
    registry: &ActorRegistry,
    policy: &AccessPolicy,
    actor: ActorId,
    scope: &ContextScope,
    budget: ContextBudget,
) -> Result<ContextPack, ContextError> {
    extract_context(
        snapshot,
        registry,
        policy,
        actor,
        scope,
        &ContextRequest::workspace(budget),
    )
}

/// Extracts a shared human/agent context pack from a single snapshot.
///
/// Every participant must be active and hold [`Capability::Read`] for every
/// emitted buffer, and every emitted span lies strictly inside the intersection
/// of all participants' scopes.
pub fn extract_shared_context(
    snapshot: &WorkspaceSnapshot,
    registry: &ActorRegistry,
    policy: &AccessPolicy,
    participants: &[ParticipantScope],
    request: &ContextRequest,
) -> Result<ContextPack, ContextError> {
    let source = SnapshotSource {
        snapshot,
        registry,
        policy,
    };
    extract_from_source(&source, participants, request)
}

/// Extracts a bounded, permission-scoped context pack for one actor from a [`WorkspaceSession`].
///
/// Any narrowed [`BufferView`] stored in `session` for `actor` is intersected with `scope`.
pub fn extract_session_context(
    session: &WorkspaceSession,
    actor: ActorId,
    scope: &ContextScope,
    request: &ContextRequest,
) -> Result<ContextPack, ContextError> {
    extract_shared_session_context(
        session,
        &[ParticipantScope::new(actor, scope.clone())],
        request,
    )
}

/// Extracts a shared human/agent context pack from a [`WorkspaceSession`].
///
/// Every participant's session [`BufferView`] (if set) is intersected with their
/// [`ParticipantScope::scope`] and with every other participant's scope.
pub fn extract_shared_session_context(
    session: &WorkspaceSession,
    participants: &[ParticipantScope],
    request: &ContextRequest,
) -> Result<ContextPack, ContextError> {
    let source = SessionSource { session };
    extract_from_source(&source, participants, request)
}

trait ContextSource {
    fn revision(&self) -> Revision;
    fn check_active(&self, actors: &[ActorId]) -> Result<(), ContextError>;
    fn read_for_all<'a>(
        &'a self,
        actors: &[ActorId],
        buffer: BufferId,
    ) -> Result<&'a Text, AccessError>;
    fn all_readable_buffers(&self, actors: &[ActorId]) -> Result<Vec<BufferId>, ContextError>;
    fn session_buffer_scope(
        &self,
        actors: &[ActorId],
        buffer: BufferId,
    ) -> Result<BufferScope, ContextError>;
}

struct SnapshotSource<'a> {
    snapshot: &'a WorkspaceSnapshot,
    registry: &'a ActorRegistry,
    policy: &'a AccessPolicy,
}

impl ContextSource for SnapshotSource<'_> {
    fn revision(&self) -> Revision {
        self.snapshot.revision()
    }

    fn check_active(&self, actors: &[ActorId]) -> Result<(), ContextError> {
        for &actor in actors {
            self.policy
                .authorize(
                    self.registry,
                    self.snapshot.catalog(),
                    actor,
                    Capability::Read,
                    [],
                )
                .map_err(ContextError::Access)?;
        }
        Ok(())
    }

    fn read_for_all<'a>(
        &'a self,
        actors: &[ActorId],
        buffer: BufferId,
    ) -> Result<&'a Text, AccessError> {
        let mut text = None;
        for &actor in actors {
            text = Some(
                self.policy
                    .read(self.registry, self.snapshot.catalog(), actor, buffer)?,
            );
        }
        Ok(text.expect("at least one active participant"))
    }

    fn all_readable_buffers(&self, actors: &[ActorId]) -> Result<Vec<BufferId>, ContextError> {
        Ok(self
            .snapshot
            .catalog()
            .live_ids()
            .into_iter()
            .filter(|&buffer| {
                actors
                    .iter()
                    .all(|&actor| self.policy.is_granted(actor, buffer, Capability::Read))
            })
            .collect())
    }

    fn session_buffer_scope(
        &self,
        _actors: &[ActorId],
        _buffer: BufferId,
    ) -> Result<BufferScope, ContextError> {
        Ok(BufferScope::full())
    }
}

struct SessionSource<'a> {
    session: &'a WorkspaceSession,
}

impl ContextSource for SessionSource<'_> {
    fn revision(&self) -> Revision {
        self.session.revision()
    }

    fn check_active(&self, actors: &[ActorId]) -> Result<(), ContextError> {
        for &actor in actors {
            if let Err(SessionError::Access(error @ AccessError::InactiveActor(_))) =
                self.session.read(actor, BufferId::new(0))
            {
                return Err(ContextError::Access(error));
            }
        }
        Ok(())
    }

    fn read_for_all<'a>(
        &'a self,
        actors: &[ActorId],
        buffer: BufferId,
    ) -> Result<&'a Text, AccessError> {
        let mut text = None;
        for &actor in actors {
            match self.session.read(actor, buffer) {
                Ok(value) => text = Some(value),
                Err(SessionError::Access(error)) => return Err(error),
                Err(SessionError::InvalidState | SessionError::InvalidTransition) => {
                    unreachable!("session read only returns access errors")
                }
            }
        }
        Ok(text.expect("at least one active participant"))
    }

    fn all_readable_buffers(&self, _actors: &[ActorId]) -> Result<Vec<BufferId>, ContextError> {
        Err(ContextError::WorkspaceTargetUnsupportedForSession)
    }

    fn session_buffer_scope(
        &self,
        actors: &[ActorId],
        buffer: BufferId,
    ) -> Result<BufferScope, ContextError> {
        let mut combined = BufferScope::full();
        for &actor in actors {
            match self.session.view(actor, buffer) {
                Ok(Some(view)) => {
                    combined = combined.intersect(&BufferScope::span(view.visible()));
                }
                Ok(None) => {}
                Err(SessionError::Access(error)) => return Err(ContextError::Access(error)),
                Err(SessionError::InvalidState | SessionError::InvalidTransition) => {
                    unreachable!("session view lookup only returns access errors")
                }
            }
        }
        Ok(combined)
    }
}

#[derive(Clone, Debug)]
enum BufferTarget {
    Full,
    View(BufferView),
    Span(ByteSpan),
    ByteRange { start: usize, end: usize },
    LineRange { start_line: usize, end_line: usize },
}

fn buffer_target_sort_key(target: &BufferTarget) -> (u8, usize, usize) {
    match target {
        BufferTarget::Full => (0, 0, 0),
        BufferTarget::View(view) => (
            1,
            view.visible().start().as_usize(),
            view.visible().end().as_usize(),
        ),
        BufferTarget::Span(span) => (2, span.start().as_usize(), span.end().as_usize()),
        BufferTarget::ByteRange { start, end } => (3, *start, *end),
        BufferTarget::LineRange {
            start_line,
            end_line,
        } => (4, *start_line, *end_line),
    }
}

fn validate_subspan_in_slice(
    text: &Text,
    permitted: ByteSpan,
    permitted_slice: &str,
    abs_start: usize,
    abs_end: usize,
) -> Result<ByteSpan, SpanError> {
    let base = permitted.start().as_usize();
    let rel_start = abs_start - base;
    let rel_end = abs_end - base;
    if !permitted_slice.is_char_boundary(rel_start) {
        return Err(SpanError::NotCharBoundary { offset: abs_start });
    }
    if !permitted_slice.is_char_boundary(rel_end) {
        return Err(SpanError::NotCharBoundary { offset: abs_end });
    }
    ByteSpan::validate_in(text, abs_start, abs_end)
}

fn validate_permitted_spans<'a>(
    buffer: BufferId,
    text: &'a Text,
    scope: &BufferScope,
) -> Result<Vec<(ByteSpan, &'a str)>, ContextError> {
    match scope.as_spans() {
        None => {
            let full = ByteSpan::full_of(text);
            Ok(vec![(full, text.as_str())])
        }
        Some(spans) => {
            let mut validated = Vec::with_capacity(spans.len());
            for &span in spans {
                let slice = span
                    .slice_in(text)
                    .map_err(|error| ContextError::StaleView {
                        buffer,
                        span,
                        error,
                    })?;
                validated.push((span, slice));
            }
            Ok(validated)
        }
    }
}

struct PermittedBuffer<'a> {
    buffer: BufferId,
    text: &'a Text,
    scope_is_full: bool,
    spans: Vec<(ByteSpan, &'a str)>,
}

fn resolve_abs_range(
    permitted_buf: &PermittedBuffer<'_>,
    range: (usize, usize),
    mode: ScopeMode,
    stale_view: Option<ByteSpan>,
) -> Result<Vec<ByteSpan>, ContextError> {
    let buffer = permitted_buf.buffer;
    let text = permitted_buf.text;
    let (start, end) = range;
    if start > end {
        return Err(ContextError::InvalidSpan {
            buffer,
            error: SpanError::InvertedRange { start, end },
        });
    }
    let wrap_span_error = |error: SpanError| match stale_view {
        Some(span) => ContextError::StaleView {
            buffer,
            span,
            error,
        },
        None => ContextError::InvalidSpan { buffer, error },
    };

    match mode {
        ScopeMode::Strict => {
            if permitted_buf.scope_is_full {
                let (full_span, full_slice) = permitted_buf.spans[0];
                let len = full_span.len_bytes();
                if start > len {
                    return Err(wrap_span_error(SpanError::OutOfBounds {
                        offset: start,
                        len,
                    }));
                }
                if end > len {
                    return Err(wrap_span_error(SpanError::OutOfBounds { offset: end, len }));
                }
                let span = validate_subspan_in_slice(text, full_span, full_slice, start, end)
                    .map_err(wrap_span_error)?;
                Ok(vec![span])
            } else {
                let containing = permitted_buf.spans.iter().find(|(allowed, _)| {
                    allowed.start().as_usize() <= start && end <= allowed.end().as_usize()
                });
                let Some(&(allowed, slice)) = containing else {
                    return Err(ContextError::SpanOutsideScope {
                        buffer,
                        requested_start: start,
                        requested_end: end,
                    });
                };
                let span = validate_subspan_in_slice(text, allowed, slice, start, end)
                    .map_err(wrap_span_error)?;
                Ok(vec![span])
            }
        }
        ScopeMode::Filter => {
            let mut resolved = Vec::new();
            for &(allowed, slice) in &permitted_buf.spans {
                let a_start = allowed.start().as_usize();
                let a_end = allowed.end().as_usize();
                let clip_start = start.max(a_start);
                let clip_end = end.min(a_end);
                let included = if start == end {
                    a_start <= start && start <= a_end
                } else {
                    clip_start < clip_end
                };
                if included {
                    let span =
                        validate_subspan_in_slice(text, allowed, slice, clip_start, clip_end)
                            .map_err(wrap_span_error)?;
                    resolved.push(span);
                }
            }
            Ok(resolved)
        }
    }
}

fn resolve_line_range(
    buffer: BufferId,
    text: &Text,
    permitted: &[(ByteSpan, &str)],
    start_line: usize,
    end_line: usize,
) -> Result<ByteSpan, ContextError> {
    if start_line > end_line {
        return Err(ContextError::InvalidLineRange {
            buffer,
            error: ClipError::InvertedLines {
                start: start_line,
                end: end_line,
            },
        });
    }
    if permitted.len() != 1 {
        return Err(ContextError::DisjointScopeForLineRange { buffer });
    }
    let (allowed, slice) = permitted[0];
    let scoped_text = Text::from_str(slice);
    let index = LineIndex::from_text(&scoped_text);
    let clipped = clip::clip_lines(&scoped_text, &index, start_line..end_line)
        .map_err(|error| ContextError::InvalidLineRange { buffer, error })?;
    let base = allowed.start().as_usize();
    let abs_start = base + clipped.origin().start().as_usize();
    let abs_end = base + clipped.origin().end().as_usize();
    Ok(ByteSpan::validate_in(text, abs_start, abs_end)
        .expect("line boundaries inside permitted slice are valid buffer boundaries"))
}

fn extract_from_source(
    source: &impl ContextSource,
    participants: &[ParticipantScope],
    request: &ContextRequest,
) -> Result<ContextPack, ContextError> {
    request
        .budget
        .validate()
        .map_err(ContextError::InvalidBudget)?;
    if participants.is_empty() {
        return Err(ContextError::EmptyParticipants);
    }

    let mut actor_set = BTreeSet::new();
    let mut effective_scope = ContextScope::all_readable();
    for participant in participants {
        actor_set.insert(participant.actor);
        effective_scope = effective_scope.intersect(&participant.scope);
    }
    let ordered_actors: Vec<ActorId> = actor_set.into_iter().collect();
    source.check_active(&ordered_actors)?;

    let revision = source.revision();
    if let Some(expected) = request.expected_revision {
        if expected != revision {
            return Err(ContextError::RevisionMismatch {
                expected,
                actual: revision,
            });
        }
    }

    let mut targets_by_buffer: BTreeMap<BufferId, Vec<BufferTarget>> = BTreeMap::new();
    let mut include_workspace = false;
    for target in &request.targets {
        match target {
            ContextTarget::Workspace => include_workspace = true,
            ContextTarget::Buffer(buffer) => {
                targets_by_buffer
                    .entry(*buffer)
                    .or_default()
                    .push(BufferTarget::Full);
            }
            ContextTarget::View(view) => {
                targets_by_buffer
                    .entry(view.buffer())
                    .or_default()
                    .push(BufferTarget::View(*view));
            }
            ContextTarget::Span { buffer, span } => {
                targets_by_buffer
                    .entry(*buffer)
                    .or_default()
                    .push(BufferTarget::Span(*span));
            }
            ContextTarget::ByteRange { buffer, start, end } => {
                targets_by_buffer
                    .entry(*buffer)
                    .or_default()
                    .push(BufferTarget::ByteRange {
                        start: *start,
                        end: *end,
                    });
            }
            ContextTarget::LineRange {
                buffer,
                start_line,
                end_line,
            } => {
                targets_by_buffer
                    .entry(*buffer)
                    .or_default()
                    .push(BufferTarget::LineRange {
                        start_line: *start_line,
                        end_line: *end_line,
                    });
            }
        }
    }

    // Validate buffer-independent malformed ranges before checking access or text,
    // in deterministic buffer and target order.
    for (&buffer, targets) in &mut targets_by_buffer {
        targets.sort_by_key(buffer_target_sort_key);
        for target in targets.iter() {
            match *target {
                BufferTarget::ByteRange { start, end } if start > end => {
                    return Err(ContextError::InvalidSpan {
                        buffer,
                        error: SpanError::InvertedRange { start, end },
                    });
                }
                BufferTarget::LineRange {
                    start_line,
                    end_line,
                } if start_line > end_line => {
                    return Err(ContextError::InvalidLineRange {
                        buffer,
                        error: ClipError::InvertedLines {
                            start: start_line,
                            end: end_line,
                        },
                    });
                }
                _ => {}
            }
        }
    }

    if include_workspace {
        let workspace_buffers = match effective_scope.scoped_buffers() {
            Some(scoped) => {
                let mut permitted = Vec::new();
                for buffer in scoped {
                    if source.read_for_all(&ordered_actors, buffer).is_err() {
                        continue;
                    }
                    let Some(caller_scope) = effective_scope.buffer_scope(buffer) else {
                        continue;
                    };
                    let session_scope = source.session_buffer_scope(&ordered_actors, buffer)?;
                    if !caller_scope.intersect(&session_scope).is_empty() {
                        permitted.push(buffer);
                    }
                }
                permitted
            }
            None => source.all_readable_buffers(&ordered_actors)?,
        };
        for buffer in workspace_buffers {
            let entry = targets_by_buffer.entry(buffer).or_default();
            entry.push(BufferTarget::Full);
            entry.sort_by_key(buffer_target_sort_key);
        }
    }

    let mut candidates: Vec<(BufferId, &Text, ByteSpan)> = Vec::new();
    for (buffer, targets) in targets_by_buffer {
        let Some(caller_scope) = effective_scope.buffer_scope(buffer) else {
            match request.mode {
                ScopeMode::Strict => return Err(ContextError::BufferOutsideScope { buffer }),
                ScopeMode::Filter => continue,
            }
        };

        let text = match source.read_for_all(&ordered_actors, buffer) {
            Ok(text) => text,
            Err(error) => match request.mode {
                ScopeMode::Strict => return Err(ContextError::Access(error)),
                ScopeMode::Filter => continue,
            },
        };

        let session_scope = source.session_buffer_scope(&ordered_actors, buffer)?;
        let combined_scope = caller_scope.intersect(&session_scope);
        if combined_scope.is_empty() {
            match request.mode {
                ScopeMode::Strict => return Err(ContextError::BufferOutsideScope { buffer }),
                ScopeMode::Filter => continue,
            }
        }

        let permitted_buf = PermittedBuffer {
            buffer,
            text,
            scope_is_full: combined_scope.is_full(),
            spans: validate_permitted_spans(buffer, text, &combined_scope)?,
        };

        let mut resolved_spans = Vec::new();
        for target in targets {
            match target {
                BufferTarget::Full => {
                    resolved_spans.extend(permitted_buf.spans.iter().map(|&(span, _)| span));
                }
                BufferTarget::View(view) => {
                    let span = view.visible();
                    resolved_spans.extend(resolve_abs_range(
                        &permitted_buf,
                        (span.start().as_usize(), span.end().as_usize()),
                        request.mode,
                        Some(span),
                    )?);
                }
                BufferTarget::Span(span) => {
                    resolved_spans.extend(resolve_abs_range(
                        &permitted_buf,
                        (span.start().as_usize(), span.end().as_usize()),
                        request.mode,
                        None,
                    )?);
                }
                BufferTarget::ByteRange { start, end } => {
                    resolved_spans.extend(resolve_abs_range(
                        &permitted_buf,
                        (start, end),
                        request.mode,
                        None,
                    )?);
                }
                BufferTarget::LineRange {
                    start_line,
                    end_line,
                } => {
                    resolved_spans.push(resolve_line_range(
                        buffer,
                        text,
                        &permitted_buf.spans,
                        start_line,
                        end_line,
                    )?);
                }
            }
        }

        for span in merge_spans(resolved_spans) {
            candidates.push((buffer, text, span));
        }
    }

    let per_fragment_cap = request.budget.max_fragment_bytes.unwrap_or(usize::MAX);
    let mut remaining_total = request.budget.max_total_bytes;
    let mut total_bytes = 0usize;
    let mut truncated = false;
    let mut omitted_fragments = 0usize;
    let mut fragments = Vec::new();

    for (index, &(buffer, text, span)) in candidates.iter().enumerate() {
        if request
            .budget
            .max_fragments
            .is_some_and(|limit| fragments.len() >= limit)
        {
            omitted_fragments += candidates.len() - index;
            truncated = true;
            break;
        }

        if span.is_empty() {
            fragments.push(ContextFragment {
                revision,
                buffer,
                span,
                text: Text::empty(),
                truncated: false,
                omitted_bytes: 0,
            });
            continue;
        }

        let limit = remaining_total.min(per_fragment_cap);
        let (clipped, frag_truncated) =
            clip::clip_with_byte_limit(text, span, limit).expect("validated permitted span");
        let emitted_str = clipped.as_str();
        let emitted_len = emitted_str.len();
        let omitted_bytes = span.len_bytes() - emitted_len;

        let blocked_by_total = if frag_truncated {
            let full_target = span.slice_in(text).expect("validated permitted span");
            let next_scalar_len = full_target[emitted_len..]
                .chars()
                .next()
                .expect("truncated span has a following scalar")
                .len_utf8();
            let needed = emitted_len + next_scalar_len;
            needed > remaining_total && needed <= per_fragment_cap
        } else {
            false
        };

        if frag_truncated {
            truncated = true;
        }

        if emitted_len == 0 {
            omitted_fragments += 1;
        } else {
            fragments.push(ContextFragment {
                revision,
                buffer,
                span: clipped.origin(),
                text: Text::from_str(emitted_str),
                truncated: frag_truncated,
                omitted_bytes,
            });
            total_bytes += emitted_len;
            remaining_total -= emitted_len;
        }

        if blocked_by_total {
            omitted_fragments += candidates.len() - (index + 1);
            break;
        }
    }

    Ok(ContextPack {
        revision,
        participants: ordered_actors,
        budget: request.budget,
        fragments,
        total_bytes,
        truncated,
        omitted_fragments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::{delivery::DeliveryLog, role::ActorRole, sequencer::Sequencer},
        edit::{
            ids::Nonce, intent::EditIntent, patch::Patch, splice::Splice, transaction::Transaction,
        },
        test_support::{assert_law, texts, Rng},
        workspace::{catalog::BufferCatalog, snapshots::VersionedWorkspace},
    };
    use std::error::Error as StdError;

    fn utf8_exact_bytes(rng: &mut Rng, len: usize) -> String {
        let mut out = String::with_capacity(len);
        while out.len() < len {
            let rem = len - out.len();
            let mut choices: Vec<&str> = vec!["a", "z", "\n", "\r", " "];
            if rem >= 2 {
                choices.push("\u{00e9}");
            }
            if rem >= 3 {
                choices.push("界");
                choices.push("漢");
            }
            if rem >= 4 {
                choices.push("😀");
            }
            out.push_str(rng.choose(&choices).unwrap());
        }
        assert_eq!(out.len(), len);
        out
    }

    fn random_span(rng: &mut Rng, text: &Text) -> ByteSpan {
        let boundaries = text.char_boundaries();
        let a = rng.gen_range(0..boundaries.len());
        let b = rng.gen_range(0..boundaries.len());
        ByteSpan::new(boundaries[a.min(b)], boundaries[a.max(b)]).unwrap()
    }

    fn reference_extract_snapshot(
        snapshot: &WorkspaceSnapshot,
        registry: &ActorRegistry,
        policy: &AccessPolicy,
        participants: &[ParticipantScope],
        request: &ContextRequest,
    ) -> Result<ContextPack, ContextError> {
        request
            .budget
            .validate()
            .map_err(ContextError::InvalidBudget)?;
        if participants.is_empty() {
            return Err(ContextError::EmptyParticipants);
        }
        let ordered_actors: Vec<ActorId> = participants
            .iter()
            .map(|p| p.actor)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for &actor in &ordered_actors {
            if !registry.is_active(actor) {
                return Err(ContextError::Access(AccessError::InactiveActor(actor)));
            }
        }
        if let Some(expected) = request.expected_revision {
            if expected != snapshot.revision() {
                return Err(ContextError::RevisionMismatch {
                    expected,
                    actual: snapshot.revision(),
                });
            }
        }

        // Independent scope intersection per buffer.
        let mut effective_all = true;
        let mut scoped_map: BTreeMap<BufferId, Option<Vec<(usize, usize)>>> = BTreeMap::new();
        for p in participants {
            match p.scope.scoped_buffers() {
                None => {}
                Some(buffers) => {
                    let mut current: BTreeMap<BufferId, Option<Vec<(usize, usize)>>> =
                        BTreeMap::new();
                    for b in buffers {
                        let bs = p.scope.buffer_scope(b).unwrap();
                        let val = bs.as_spans().map(|spans| {
                            spans
                                .iter()
                                .map(|s| (s.start().as_usize(), s.end().as_usize()))
                                .collect::<Vec<_>>()
                        });
                        current.insert(b, val);
                    }
                    if effective_all {
                        effective_all = false;
                        scoped_map = current;
                    } else {
                        let mut next = BTreeMap::new();
                        for (b, lhs) in &scoped_map {
                            if let Some(rhs) = current.get(b) {
                                let merged = match (lhs, rhs) {
                                    (None, None) => Some(None),
                                    (None, Some(r)) => Some(Some(r.clone())),
                                    (Some(l), None) => Some(Some(l.clone())),
                                    (Some(l), Some(r)) => {
                                        let mut raw = Vec::new();
                                        for &(ls, le) in l {
                                            for &(rs, re) in r {
                                                let s = ls.max(rs);
                                                let e = le.min(re);
                                                if s <= e {
                                                    raw.push((s, e));
                                                }
                                            }
                                        }
                                        let m = merge_raw(raw);
                                        if m.is_empty() {
                                            None
                                        } else {
                                            Some(Some(m))
                                        }
                                    }
                                };
                                if let Some(val) = merged {
                                    next.insert(*b, val);
                                }
                            }
                        }
                        scoped_map = next;
                    }
                }
            }
        }

        let mut per_buffer: BTreeMap<BufferId, Vec<ContextTarget>> = BTreeMap::new();
        let mut want_ws = false;
        for t in &request.targets {
            match t {
                ContextTarget::Workspace => want_ws = true,
                ContextTarget::Buffer(b) => per_buffer.entry(*b).or_default().push(t.clone()),
                ContextTarget::View(v) => per_buffer.entry(v.buffer()).or_default().push(t.clone()),
                ContextTarget::Span { buffer, .. }
                | ContextTarget::ByteRange { buffer, .. }
                | ContextTarget::LineRange { buffer, .. } => {
                    per_buffer.entry(*buffer).or_default().push(t.clone())
                }
            }
        }
        for (&buffer, list) in &mut per_buffer {
            list.sort_by_key(|t| match t {
                ContextTarget::Buffer(_) => (0u8, 0usize, 0usize),
                ContextTarget::View(v) => (
                    1,
                    v.visible().start().as_usize(),
                    v.visible().end().as_usize(),
                ),
                ContextTarget::Span { span, .. } => {
                    (2, span.start().as_usize(), span.end().as_usize())
                }
                ContextTarget::ByteRange { start, end, .. } => (3, *start, *end),
                ContextTarget::LineRange {
                    start_line,
                    end_line,
                    ..
                } => (4, *start_line, *end_line),
                ContextTarget::Workspace => unreachable!(),
            });
            for t in list.iter() {
                match *t {
                    ContextTarget::ByteRange { start, end, .. } if start > end => {
                        return Err(ContextError::InvalidSpan {
                            buffer,
                            error: SpanError::InvertedRange { start, end },
                        });
                    }
                    ContextTarget::LineRange {
                        start_line,
                        end_line,
                        ..
                    } if start_line > end_line => {
                        return Err(ContextError::InvalidLineRange {
                            buffer,
                            error: ClipError::InvertedLines {
                                start: start_line,
                                end: end_line,
                            },
                        });
                    }
                    _ => {}
                }
            }
        }

        if want_ws {
            let ids: Vec<BufferId> = if effective_all {
                snapshot
                    .catalog()
                    .live_ids()
                    .into_iter()
                    .filter(|&b| {
                        ordered_actors
                            .iter()
                            .all(|&a| policy.is_granted(a, b, Capability::Read))
                    })
                    .collect()
            } else {
                scoped_map
                    .iter()
                    .filter(|(&b, spans)| {
                        !matches!(spans, Some(raw) if raw.is_empty())
                            && snapshot.catalog().contains_live(b)
                            && ordered_actors
                                .iter()
                                .all(|&a| policy.is_granted(a, b, Capability::Read))
                    })
                    .map(|(&b, _)| b)
                    .collect()
            };
            for b in ids {
                let entry = per_buffer.entry(b).or_default();
                entry.push(ContextTarget::Buffer(b));
                entry.sort_by_key(|t| match t {
                    ContextTarget::Buffer(_) => (0u8, 0usize, 0usize),
                    ContextTarget::View(v) => (
                        1,
                        v.visible().start().as_usize(),
                        v.visible().end().as_usize(),
                    ),
                    ContextTarget::Span { span, .. } => {
                        (2, span.start().as_usize(), span.end().as_usize())
                    }
                    ContextTarget::ByteRange { start, end, .. } => (3, *start, *end),
                    ContextTarget::LineRange {
                        start_line,
                        end_line,
                        ..
                    } => (4, *start_line, *end_line),
                    ContextTarget::Workspace => unreachable!(),
                });
            }
        }

        let mut all_candidates: Vec<(BufferId, usize, usize)> = Vec::new();
        for (buffer, targets) in per_buffer {
            let scope_entry = if effective_all {
                Some(None)
            } else {
                scoped_map.get(&buffer).cloned()
            };
            let Some(scope_spans_opt) = scope_entry else {
                if request.mode == ScopeMode::Strict {
                    return Err(ContextError::BufferOutsideScope { buffer });
                }
                continue;
            };
            let mut access_err = None;
            for &actor in &ordered_actors {
                if let Err(err) = policy.read(registry, snapshot.catalog(), actor, buffer) {
                    access_err = Some(err);
                    break;
                }
            }
            if let Some(err) = access_err {
                if request.mode == ScopeMode::Strict {
                    return Err(ContextError::Access(err));
                }
                continue;
            }
            let text = snapshot.catalog().get(buffer).unwrap();
            let s = text.as_str();
            let (is_full, allowed_raw): (bool, Vec<(usize, usize)>) = match scope_spans_opt {
                None => (true, vec![(0, s.len())]),
                Some(raw) => {
                    for &(a, b) in &raw {
                        if a > s.len() {
                            let span = ByteSpan::new(
                                Text::from_str(&"a".repeat(a.max(b)))
                                    .validate_offset(a)
                                    .unwrap(),
                                Text::from_str(&"a".repeat(a.max(b)))
                                    .validate_offset(b)
                                    .unwrap(),
                            )
                            .unwrap();
                            return Err(ContextError::StaleView {
                                buffer,
                                span,
                                error: SpanError::OutOfBounds {
                                    offset: a,
                                    len: s.len(),
                                },
                            });
                        }
                        if !s.is_char_boundary(a) {
                            return Err(ContextError::StaleView {
                                buffer,
                                span: make_span(a, b),
                                error: SpanError::NotCharBoundary { offset: a },
                            });
                        }
                        if b > s.len() {
                            return Err(ContextError::StaleView {
                                buffer,
                                span: make_span(a, b),
                                error: SpanError::OutOfBounds {
                                    offset: b,
                                    len: s.len(),
                                },
                            });
                        }
                        if !s.is_char_boundary(b) {
                            return Err(ContextError::StaleView {
                                buffer,
                                span: make_span(a, b),
                                error: SpanError::NotCharBoundary { offset: b },
                            });
                        }
                    }
                    (false, raw)
                }
            };

            let mut raw_ranges = Vec::new();
            for target in targets {
                match target {
                    ContextTarget::Buffer(_) => raw_ranges.extend(allowed_raw.iter().copied()),
                    ContextTarget::View(v) => {
                        let a = v.visible().start().as_usize();
                        let b = v.visible().end().as_usize();
                        ref_resolve_range(
                            buffer,
                            s,
                            is_full,
                            &allowed_raw,
                            a,
                            b,
                            request.mode,
                            Some(v.visible()),
                            &mut raw_ranges,
                        )?;
                    }
                    ContextTarget::Span { span, .. } => {
                        ref_resolve_range(
                            buffer,
                            s,
                            is_full,
                            &allowed_raw,
                            span.start().as_usize(),
                            span.end().as_usize(),
                            request.mode,
                            None,
                            &mut raw_ranges,
                        )?;
                    }
                    ContextTarget::ByteRange { start, end, .. } => {
                        ref_resolve_range(
                            buffer,
                            s,
                            is_full,
                            &allowed_raw,
                            start,
                            end,
                            request.mode,
                            None,
                            &mut raw_ranges,
                        )?;
                    }
                    ContextTarget::LineRange {
                        start_line,
                        end_line,
                        ..
                    } => {
                        if allowed_raw.len() != 1 {
                            return Err(ContextError::DisjointScopeForLineRange { buffer });
                        }
                        let (base, lim) = allowed_raw[0];
                        let sub = &s[base..lim];
                        let starts: Vec<usize> = std::iter::once(0)
                            .chain(sub.match_indices('\n').map(|(i, _)| i + 1))
                            .chain(std::iter::once(sub.len()))
                            .collect();
                        let line_count = starts.len() - 1;
                        if end_line > line_count {
                            return Err(ContextError::InvalidLineRange {
                                buffer,
                                error: ClipError::LineOutOfRange {
                                    line: end_line,
                                    line_count,
                                },
                            });
                        }
                        raw_ranges.push((base + starts[start_line], base + starts[end_line]));
                    }
                    ContextTarget::Workspace => unreachable!(),
                }
            }
            for (start, end) in merge_raw(raw_ranges) {
                all_candidates.push((buffer, start, end));
            }
        }

        let per_frag = request.budget.max_fragment_bytes.unwrap_or(usize::MAX);
        let mut remaining = request.budget.max_total_bytes;
        let mut total_bytes = 0usize;
        let mut truncated = false;
        let mut omitted_fragments = 0usize;
        let mut fragments = Vec::new();

        for (idx, &(buffer, start, end)) in all_candidates.iter().enumerate() {
            if request
                .budget
                .max_fragments
                .is_some_and(|lim| fragments.len() >= lim)
            {
                omitted_fragments += all_candidates.len() - idx;
                truncated = true;
                break;
            }
            let text = snapshot.catalog().get(buffer).unwrap();
            let raw_str = &text.as_str()[start..end];
            if raw_str.is_empty() {
                fragments.push(ContextFragment {
                    revision: snapshot.revision(),
                    buffer,
                    span: ByteSpan::validate_in(text, start, end).unwrap(),
                    text: Text::empty(),
                    truncated: false,
                    omitted_bytes: 0,
                });
                continue;
            }
            let cap = remaining.min(per_frag);
            let mut take = 0usize;
            for ch in raw_str.chars() {
                if take + ch.len_utf8() <= cap {
                    take += ch.len_utf8();
                } else {
                    break;
                }
            }
            let frag_trunc = take < raw_str.len();
            let blocked_by_total = if frag_trunc {
                let next_len = raw_str[take..].chars().next().unwrap().len_utf8();
                take + next_len > remaining && take + next_len <= per_frag
            } else {
                false
            };
            if frag_trunc {
                truncated = true;
            }
            if take == 0 {
                omitted_fragments += 1;
            } else {
                fragments.push(ContextFragment {
                    revision: snapshot.revision(),
                    buffer,
                    span: ByteSpan::validate_in(text, start, start + take).unwrap(),
                    text: Text::from_str(&raw_str[..take]),
                    truncated: frag_trunc,
                    omitted_bytes: raw_str.len() - take,
                });
                total_bytes += take;
                remaining -= take;
            }
            if blocked_by_total {
                omitted_fragments += all_candidates.len() - (idx + 1);
                break;
            }
        }

        Ok(ContextPack {
            revision: snapshot.revision(),
            participants: ordered_actors,
            budget: request.budget,
            fragments,
            total_bytes,
            truncated,
            omitted_fragments,
        })
    }

    fn make_span(a: usize, b: usize) -> ByteSpan {
        let pad = Text::from_str(&"a".repeat(a.max(b)));
        ByteSpan::validate_in(&pad, a, b).unwrap()
    }

    fn merge_raw(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
        ranges.sort();
        let mut out: Vec<(usize, usize)> = Vec::new();
        for (s, e) in ranges {
            if let Some(last) = out.last_mut() {
                if s <= last.1 {
                    last.1 = last.1.max(e);
                    continue;
                }
            }
            out.push((s, e));
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn ref_resolve_range(
        buffer: BufferId,
        s: &str,
        is_full: bool,
        allowed_raw: &[(usize, usize)],
        start: usize,
        end: usize,
        mode: ScopeMode,
        stale_view: Option<ByteSpan>,
        out: &mut Vec<(usize, usize)>,
    ) -> Result<(), ContextError> {
        let wrap = |error: SpanError| match stale_view {
            Some(span) => ContextError::StaleView {
                buffer,
                span,
                error,
            },
            None => ContextError::InvalidSpan { buffer, error },
        };
        match mode {
            ScopeMode::Strict => {
                if is_full {
                    if start > s.len() {
                        return Err(wrap(SpanError::OutOfBounds {
                            offset: start,
                            len: s.len(),
                        }));
                    }
                    if end > s.len() {
                        return Err(wrap(SpanError::OutOfBounds {
                            offset: end,
                            len: s.len(),
                        }));
                    }
                    if !s.is_char_boundary(start) {
                        return Err(wrap(SpanError::NotCharBoundary { offset: start }));
                    }
                    if !s.is_char_boundary(end) {
                        return Err(wrap(SpanError::NotCharBoundary { offset: end }));
                    }
                    out.push((start, end));
                } else {
                    if !allowed_raw.iter().any(|&(a, b)| a <= start && end <= b) {
                        return Err(ContextError::SpanOutsideScope {
                            buffer,
                            requested_start: start,
                            requested_end: end,
                        });
                    }
                    if !s.is_char_boundary(start) {
                        return Err(wrap(SpanError::NotCharBoundary { offset: start }));
                    }
                    if !s.is_char_boundary(end) {
                        return Err(wrap(SpanError::NotCharBoundary { offset: end }));
                    }
                    out.push((start, end));
                }
            }
            ScopeMode::Filter => {
                for &(a, b) in allowed_raw {
                    let cs = start.max(a);
                    let ce = end.min(b);
                    let keep = if start == end {
                        a <= start && start <= b
                    } else {
                        cs < ce
                    };
                    if keep {
                        if !s.is_char_boundary(cs) {
                            return Err(wrap(SpanError::NotCharBoundary { offset: cs }));
                        }
                        if !s.is_char_boundary(ce) {
                            return Err(wrap(SpanError::NotCharBoundary { offset: ce }));
                        }
                        out.push((cs, ce));
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn budget_validation_and_error_display() {
        assert_eq!(ContextBudget::new(0), Err(BudgetError::ZeroTotalBytes));
        assert_eq!(
            ContextBudget::with_limits(16, Some(0), None),
            Err(BudgetError::ZeroFragmentBytes)
        );
        assert_eq!(
            ContextBudget::with_limits(16, None, Some(0)),
            Err(BudgetError::ZeroFragmentLimit)
        );
        assert_eq!(
            ContextBudget::with_limits(16, Some(17), None),
            Err(BudgetError::FragmentExceedsTotal {
                max_fragment_bytes: 17,
                max_total_bytes: 16
            })
        );
        let ok = ContextBudget::with_limits(16, Some(16), Some(1)).unwrap();
        assert_eq!(ok.validate(), Ok(()));

        let errors: Vec<ContextError> = vec![
            ContextError::InvalidBudget(BudgetError::ZeroTotalBytes),
            ContextError::InvalidBudget(BudgetError::ZeroFragmentBytes),
            ContextError::InvalidBudget(BudgetError::ZeroFragmentLimit),
            ContextError::InvalidBudget(BudgetError::FragmentExceedsTotal {
                max_fragment_bytes: 10,
                max_total_bytes: 5,
            }),
            ContextError::EmptyParticipants,
            ContextError::RevisionMismatch {
                expected: Revision::ZERO,
                actual: Revision::new(3),
            },
            ContextError::Access(AccessError::InactiveActor(ActorId::new(1))),
            ContextError::BufferOutsideScope {
                buffer: BufferId::new(2),
            },
            ContextError::SpanOutsideScope {
                buffer: BufferId::new(2),
                requested_start: 1,
                requested_end: 9,
            },
            ContextError::StaleView {
                buffer: BufferId::new(2),
                span: make_span(1, 3),
                error: SpanError::NotCharBoundary { offset: 1 },
            },
            ContextError::InvalidSpan {
                buffer: BufferId::new(2),
                error: SpanError::OutOfBounds { offset: 9, len: 4 },
            },
            ContextError::InvalidLineRange {
                buffer: BufferId::new(2),
                error: ClipError::LineOutOfRange {
                    line: 5,
                    line_count: 2,
                },
            },
            ContextError::DisjointScopeForLineRange {
                buffer: BufferId::new(2),
            },
            ContextError::WorkspaceTargetUnsupportedForSession,
        ];
        for err in &errors {
            assert!(!err.to_string().is_empty());
        }
        assert!(errors[0].source().is_some());
        assert!(errors[4].source().is_none());
        assert!(errors[6].source().is_some());
        assert!(errors[9].source().is_some());
        assert!(errors[10].source().is_some());
        assert!(errors[11].source().is_some());
    }

    #[test]
    fn single_snapshot_consistency_and_revision_pin() {
        let actor = ActorId::new(1);
        let a = BufferId::new(1);
        let b = BufferId::new(2);
        let mut registry = ActorRegistry::new();
        registry.register(actor, ActorRole::Human).unwrap();
        let mut policy = AccessPolicy::new();
        policy.grant_actor(actor, Capability::Read);

        let mut workspace = VersionedWorkspace::new(BufferCatalog::new());
        workspace.register(a, Text::from_str("alpha")).unwrap();
        workspace.register(b, Text::from_str("beta")).unwrap();
        let snap_rev2 = workspace.capture();
        assert_eq!(snap_rev2.revision(), Revision::new(2));

        workspace.update(a, Text::from_str("ALPHA-NEW")).unwrap();
        workspace.remove(b).unwrap();
        let snap_rev4 = workspace.capture();
        assert_eq!(snap_rev4.revision(), Revision::new(4));

        let budget = ContextBudget::new(1024).unwrap();
        let pack_old = extract_context(
            &snap_rev2,
            &registry,
            &policy,
            actor,
            &ContextScope::all_readable(),
            &ContextRequest::new(
                vec![ContextTarget::Buffer(a), ContextTarget::Buffer(b)],
                budget,
            )
            .at_revision(Revision::new(2)),
        )
        .unwrap();
        assert_eq!(pack_old.revision, Revision::new(2));
        assert_eq!(pack_old.fragments.len(), 2);
        assert!(pack_old
            .fragments
            .iter()
            .all(|f| f.revision == Revision::new(2)));
        assert_eq!(pack_old.fragments[0].as_str(), "alpha");
        assert_eq!(pack_old.fragments[1].as_str(), "beta");

        assert_eq!(
            extract_context(
                &snap_rev4,
                &registry,
                &policy,
                actor,
                &ContextScope::all_readable(),
                &ContextRequest::new(vec![ContextTarget::Buffer(a)], budget)
                    .at_revision(Revision::new(2)),
            ),
            Err(ContextError::RevisionMismatch {
                expected: Revision::new(2),
                actual: Revision::new(4),
            })
        );
    }

    #[test]
    fn permission_scope_stale_views_and_error_contracts() {
        let actor = ActorId::new(1);
        let revoked = ActorId::new(2);
        let unknown_actor = ActorId::new(3);
        let mut registry = ActorRegistry::new();
        registry.register(actor, ActorRole::Agent).unwrap();
        registry.register(revoked, ActorRole::Human).unwrap();
        registry.revoke(revoked).unwrap();

        let live = BufferId::new(1);
        let secret = BufferId::new(2);
        let closed = BufferId::new(3);
        let unknown_buf = BufferId::new(4);

        let mut workspace = VersionedWorkspace::new(BufferCatalog::new());
        workspace
            .register(live, Text::from_str("HEAD_SECRET_allowed_TAIL_SECRET"))
            .unwrap();
        workspace
            .register(secret, Text::from_str("TOP_SECRET_OTHER_BUFFER"))
            .unwrap();
        workspace
            .register(closed, Text::from_str("CLOSED_SECRET"))
            .unwrap();
        let before_close = workspace.capture();
        let stale_view = BufferView::narrowed(
            before_close.catalog(),
            live,
            ByteSpan::validate_in(before_close.catalog().get(live).unwrap(), 1, 20).unwrap(),
        )
        .unwrap();
        workspace.remove(closed).unwrap();
        let snapshot = workspace.capture();

        let mut policy = AccessPolicy::new();
        policy.grant_buffer(actor, live, Capability::Read);
        policy.grant_buffer(actor, closed, Capability::Read);
        let budget = ContextBudget::new(256).unwrap();

        let live_text = snapshot.catalog().get(live).unwrap();
        let allowed_span = ByteSpan::validate_in(live_text, 12, 19).unwrap();
        assert_eq!(allowed_span.slice_in(live_text).unwrap(), "allowed");
        let narrow_view = BufferView::narrowed(snapshot.catalog(), live, allowed_span).unwrap();
        let narrow_scope = ContextScope::from_views([narrow_view]);

        // Inactive / revoked actors fail before reading anything.
        for bad_actor in [revoked, unknown_actor] {
            assert_eq!(
                extract_context(
                    &snapshot,
                    &registry,
                    &policy,
                    bad_actor,
                    &ContextScope::all_readable(),
                    &ContextRequest::new(vec![], budget),
                ),
                Err(ContextError::Access(AccessError::InactiveActor(bad_actor)))
            );
        }

        // Strict failures on unauthorized, closed, unknown, and out-of-scope targets.
        assert_eq!(
            extract_context(
                &snapshot,
                &registry,
                &policy,
                actor,
                &ContextScope::all_readable(),
                &ContextRequest::new(vec![ContextTarget::Buffer(secret)], budget),
            ),
            Err(ContextError::Access(AccessError::Denied {
                actor,
                buffer: secret,
                capability: Capability::Read,
            }))
        );
        assert_eq!(
            extract_context(
                &snapshot,
                &registry,
                &policy,
                actor,
                &ContextScope::all_readable(),
                &ContextRequest::new(vec![ContextTarget::Buffer(closed)], budget),
            ),
            Err(ContextError::Access(AccessError::BufferClosed(closed)))
        );
        assert_eq!(
            extract_context(
                &snapshot,
                &registry,
                &policy,
                actor,
                &ContextScope::all_readable(),
                &ContextRequest::new(vec![ContextTarget::Buffer(unknown_buf)], budget),
            ),
            Err(ContextError::Access(AccessError::UnknownBuffer(
                unknown_buf
            )))
        );
        assert_eq!(
            extract_context(
                &snapshot,
                &registry,
                &policy,
                actor,
                &narrow_scope,
                &ContextRequest::new(vec![ContextTarget::Buffer(secret)], budget),
            ),
            Err(ContextError::BufferOutsideScope { buffer: secret })
        );
        assert_eq!(
            extract_context(
                &snapshot,
                &registry,
                &policy,
                actor,
                &narrow_scope,
                &ContextRequest::new(
                    vec![ContextTarget::ByteRange {
                        buffer: live,
                        start: 10,
                        end: 19,
                    }],
                    budget,
                ),
            ),
            Err(ContextError::SpanOutsideScope {
                buffer: live,
                requested_start: 10,
                requested_end: 19,
            })
        );

        // Filter mode excludes unauthorized/closed/unknown buffers and clips to permitted scope,
        // without retaining or leaking any unpermitted text even in Debug output.
        let filtered = extract_context(
            &snapshot,
            &registry,
            &policy,
            actor,
            &narrow_scope,
            &ContextRequest::new(
                vec![
                    ContextTarget::Buffer(secret),
                    ContextTarget::Buffer(closed),
                    ContextTarget::Buffer(unknown_buf),
                    ContextTarget::ByteRange {
                        buffer: live,
                        start: 0,
                        end: 999,
                    },
                ],
                budget,
            )
            .with_mode(ScopeMode::Filter),
        )
        .unwrap();
        assert_eq!(filtered.fragments.len(), 1);
        assert_eq!(filtered.fragments[0].as_str(), "allowed");
        assert_eq!(filtered.fragments[0].span, allowed_span);
        let debug_dump = format!("{filtered:?}");
        assert!(!debug_dump.contains("HEAD_SECRET"));
        assert!(!debug_dump.contains("TAIL_SECRET"));
        assert!(!debug_dump.contains("TOP_SECRET"));
        assert!(!debug_dump.contains("CLOSED_SECRET"));

        // Stale view detection after buffer shrinks or changes UTF-8 boundaries.
        workspace.update(live, Text::from_str("界")).unwrap();
        let mutated = workspace.capture();
        assert_eq!(
            extract_context(
                &mutated,
                &registry,
                &policy,
                actor,
                &ContextScope::all_readable(),
                &ContextRequest::new(vec![ContextTarget::View(stale_view)], budget),
            ),
            Err(ContextError::StaleView {
                buffer: live,
                span: stale_view.visible(),
                error: SpanError::OutOfBounds { offset: 20, len: 3 },
            })
        );
        let mid_char_view = BufferView::narrowed(
            before_close.catalog(),
            live,
            ByteSpan::validate_in(before_close.catalog().get(live).unwrap(), 1, 2).unwrap(),
        )
        .unwrap();
        assert_eq!(
            extract_context(
                &mutated,
                &registry,
                &policy,
                actor,
                &ContextScope::from_views([mid_char_view]),
                &ContextRequest::new(vec![ContextTarget::Buffer(live)], budget),
            ),
            Err(ContextError::StaleView {
                buffer: live,
                span: mid_char_view.visible(),
                error: SpanError::NotCharBoundary { offset: 1 },
            })
        );

        // Invalid budget, inverted ranges, out-of-bounds ranges, invalid line ranges, and disjoint scope.
        let invalid_budget = ContextBudget {
            max_total_bytes: 0,
            max_fragment_bytes: None,
            max_fragments: None,
        };
        assert_eq!(
            extract_context(
                &snapshot,
                &registry,
                &policy,
                revoked,
                &ContextScope::all_readable(),
                &ContextRequest::new(vec![], invalid_budget),
            ),
            Err(ContextError::InvalidBudget(BudgetError::ZeroTotalBytes))
        );
        assert_eq!(
            extract_context(
                &snapshot,
                &registry,
                &policy,
                actor,
                &ContextScope::all_readable(),
                &ContextRequest::new(
                    vec![ContextTarget::ByteRange {
                        buffer: live,
                        start: 5,
                        end: 2,
                    }],
                    budget,
                ),
            ),
            Err(ContextError::InvalidSpan {
                buffer: live,
                error: SpanError::InvertedRange { start: 5, end: 2 },
            })
        );
        assert_eq!(
            extract_context(
                &snapshot,
                &registry,
                &policy,
                actor,
                &ContextScope::all_readable(),
                &ContextRequest::new(
                    vec![ContextTarget::LineRange {
                        buffer: live,
                        start_line: 3,
                        end_line: 1,
                    }],
                    budget,
                ),
            ),
            Err(ContextError::InvalidLineRange {
                buffer: live,
                error: ClipError::InvertedLines { start: 3, end: 1 },
            })
        );
        assert_eq!(
            extract_context(
                &snapshot,
                &registry,
                &policy,
                actor,
                &ContextScope::all_readable(),
                &ContextRequest::new(
                    vec![ContextTarget::LineRange {
                        buffer: live,
                        start_line: 0,
                        end_line: 5,
                    }],
                    budget,
                ),
            ),
            Err(ContextError::InvalidLineRange {
                buffer: live,
                error: ClipError::LineOutOfRange {
                    line: 5,
                    line_count: 1,
                },
            })
        );

        let mut disjoint_scope = ContextScope::empty();
        disjoint_scope.allow_span(live, ByteSpan::validate_in(live_text, 0, 4).unwrap());
        disjoint_scope.allow_span(live, ByteSpan::validate_in(live_text, 12, 19).unwrap());
        assert_eq!(
            extract_context(
                &snapshot,
                &registry,
                &policy,
                actor,
                &disjoint_scope,
                &ContextRequest::new(
                    vec![ContextTarget::LineRange {
                        buffer: live,
                        start_line: 0,
                        end_line: 1,
                    }],
                    budget,
                ),
            ),
            Err(ContextError::DisjointScopeForLineRange { buffer: live })
        );
    }

    #[test]
    fn shared_human_agent_context_packs_intersect_permissions_and_views() {
        let human = ActorId::new(10);
        let agent = ActorId::new(2);
        let mut registry = ActorRegistry::new();
        registry.register(human, ActorRole::Human).unwrap();
        registry.register(agent, ActorRole::Agent).unwrap();

        let shared_buf = BufferId::new(1);
        let human_only = BufferId::new(2);
        let agent_only = BufferId::new(3);
        let mut catalog = BufferCatalog::new();
        let text = Text::from_str("0123456789abcdef");
        catalog.register(shared_buf, text.clone()).unwrap();
        catalog
            .register(human_only, Text::from_str("human-private"))
            .unwrap();
        catalog
            .register(agent_only, Text::from_str("agent-private"))
            .unwrap();
        let snapshot = VersionedWorkspace::new(catalog).capture();

        let mut policy = AccessPolicy::new();
        policy.grant_buffer(human, shared_buf, Capability::Read);
        policy.grant_buffer(human, human_only, Capability::Read);
        policy.grant_buffer(agent, shared_buf, Capability::Read);
        policy.grant_buffer(agent, agent_only, Capability::Read);

        let human_view = BufferView::narrowed(
            snapshot.catalog(),
            shared_buf,
            ByteSpan::validate_in(&text, 2, 12).unwrap(),
        )
        .unwrap();
        let agent_view = BufferView::narrowed(
            snapshot.catalog(),
            shared_buf,
            ByteSpan::validate_in(&text, 6, 15).unwrap(),
        )
        .unwrap();

        let mut human_scope = ContextScope::from_views([human_view]);
        human_scope.allow_buffer(human_only);
        let mut agent_scope = ContextScope::from_views([agent_view]);
        agent_scope.allow_buffer(agent_only);

        let budget = ContextBudget::new(128).unwrap();
        let req = ContextRequest::workspace(budget);
        let p_human = ParticipantScope::new(human, human_scope);
        let p_agent = ParticipantScope::new(agent, agent_scope);

        let pack_ab = extract_shared_context(
            &snapshot,
            &registry,
            &policy,
            &[p_human.clone(), p_agent.clone()],
            &req,
        )
        .unwrap();
        let pack_ba = extract_shared_context(
            &snapshot,
            &registry,
            &policy,
            &[p_agent.clone(), p_human.clone(), p_agent.clone()],
            &req,
        )
        .unwrap();
        assert_eq!(pack_ab, pack_ba);
        assert_eq!(pack_ab.participants, vec![agent, human]);
        assert_eq!(pack_ab.fragments.len(), 1);
        assert_eq!(pack_ab.fragments[0].buffer, shared_buf);
        assert_eq!(pack_ab.fragments[0].as_str(), "6789ab");
        assert_eq!(
            pack_ab.fragments[0].span,
            ByteSpan::validate_in(&text, 6, 12).unwrap()
        );

        assert_eq!(
            extract_shared_context(&snapshot, &registry, &policy, &[], &req),
            Err(ContextError::EmptyParticipants)
        );

        // Revoking either participant fails the shared pack.
        let mut revoked_registry = registry.clone();
        revoked_registry.revoke(agent).unwrap();
        assert_eq!(
            extract_shared_context(
                &snapshot,
                &revoked_registry,
                &policy,
                &[p_human, p_agent],
                &req
            ),
            Err(ContextError::Access(AccessError::InactiveActor(agent)))
        );
    }

    #[test]
    fn budget_utf8_boundaries_and_truncation_semantics() {
        let actor = ActorId::new(1);
        let mut registry = ActorRegistry::new();
        registry.register(actor, ActorRole::Agent).unwrap();
        let mut policy = AccessPolicy::new();
        policy.grant_actor(actor, Capability::Read);

        let b1 = BufferId::new(1);
        let b2 = BufferId::new(2);
        let b3 = BufferId::new(3);
        let mut catalog = BufferCatalog::new();
        catalog.register(b1, Text::from_str("😀x")).unwrap(); // 4 + 1 bytes
        catalog.register(b2, Text::from_str("")).unwrap(); // 0 bytes
        catalog.register(b3, Text::from_str("a界b")).unwrap(); // 1 + 3 + 1 bytes
        let snapshot = VersionedWorkspace::new(catalog).capture();

        // Budget smaller than first 4-byte scalar: never splits mid-scalar, stops on total exhaustion.
        for small in 1..=3 {
            let pack = extract_workspace_context(
                &snapshot,
                &registry,
                &policy,
                actor,
                &ContextScope::all_readable(),
                ContextBudget::new(small).unwrap(),
            )
            .unwrap();
            assert!(pack.fragments.is_empty());
            assert_eq!(pack.total_bytes, 0);
            assert!(pack.truncated);
            assert_eq!(pack.omitted_fragments, 3);
        }

        // Budget = 4 fits "😀" exactly, omits "x" (1 byte) and stops before b2/b3.
        let pack4 = extract_workspace_context(
            &snapshot,
            &registry,
            &policy,
            actor,
            &ContextScope::all_readable(),
            ContextBudget::new(4).unwrap(),
        )
        .unwrap();
        assert_eq!(pack4.fragments.len(), 1);
        assert_eq!(pack4.fragments[0].as_str(), "😀");
        assert!(pack4.fragments[0].truncated);
        assert_eq!(pack4.fragments[0].omitted_bytes, 1);
        assert_eq!(pack4.total_bytes, 4);
        assert!(pack4.truncated);
        assert_eq!(pack4.omitted_fragments, 2);

        // Budget = 5 fits b1 completely ("😀x") AND empty b2 ("") with 0 bytes remaining,
        // then omits b3 because remaining total is 0.
        let pack5 = extract_workspace_context(
            &snapshot,
            &registry,
            &policy,
            actor,
            &ContextScope::all_readable(),
            ContextBudget::new(5).unwrap(),
        )
        .unwrap();
        assert_eq!(pack5.fragments.len(), 2);
        assert_eq!(pack5.fragments[0].as_str(), "😀x");
        assert!(!pack5.fragments[0].truncated);
        assert_eq!(pack5.fragments[1].as_str(), "");
        assert!(!pack5.fragments[1].truncated);
        assert_eq!(pack5.total_bytes, 5);
        assert!(pack5.truncated);
        assert_eq!(pack5.omitted_fragments, 1);

        // Per-fragment cap = 2 with total = 10 omits b1 ("😀" > 2), keeps b2 (""), and takes "a" from b3.
        let pack_frag = extract_workspace_context(
            &snapshot,
            &registry,
            &policy,
            actor,
            &ContextScope::all_readable(),
            ContextBudget::with_limits(10, Some(2), None).unwrap(),
        )
        .unwrap();
        assert_eq!(pack_frag.fragments.len(), 2);
        assert_eq!(pack_frag.fragments[0].buffer, b2);
        assert_eq!(pack_frag.fragments[0].as_str(), "");
        assert_eq!(pack_frag.fragments[1].buffer, b3);
        assert_eq!(pack_frag.fragments[1].as_str(), "a");
        assert!(pack_frag.fragments[1].truncated);
        assert_eq!(pack_frag.fragments[1].omitted_bytes, 4);
        assert_eq!(pack_frag.total_bytes, 1);
        assert!(pack_frag.truncated);
        assert_eq!(pack_frag.omitted_fragments, 1);

        // max_fragments = 1 stops after first fragment even when total bytes remain.
        let pack_count = extract_workspace_context(
            &snapshot,
            &registry,
            &policy,
            actor,
            &ContextScope::all_readable(),
            ContextBudget::with_limits(100, None, Some(1)).unwrap(),
        )
        .unwrap();
        assert_eq!(pack_count.fragments.len(), 1);
        assert_eq!(pack_count.fragments[0].buffer, b1);
        assert!(!pack_count.fragments[0].truncated);
        assert!(pack_count.truncated);
        assert_eq!(pack_count.omitted_fragments, 2);
    }

    #[test]
    fn ordering_deduplication_and_overlap_coalescing() {
        let actor = ActorId::new(1);
        let mut registry = ActorRegistry::new();
        registry.register(actor, ActorRole::Human).unwrap();
        let mut policy = AccessPolicy::new();
        policy.grant_actor(actor, Capability::Read);

        let b1 = BufferId::new(1);
        let b2 = BufferId::new(2);
        let t1 = Text::from_str("line0\nline1\nline2\n");
        let t2 = Text::from_str("hello world");
        let mut catalog = BufferCatalog::new();
        catalog.register(b2, t2.clone()).unwrap();
        catalog.register(b1, t1.clone()).unwrap();
        let snapshot = VersionedWorkspace::new(catalog).capture();

        let budget = ContextBudget::new(1024).unwrap();
        let req = ContextRequest::new(
            vec![
                ContextTarget::ByteRange {
                    buffer: b2,
                    start: 6,
                    end: 11,
                },
                ContextTarget::ByteRange {
                    buffer: b1,
                    start: 3,
                    end: 8,
                },
                ContextTarget::LineRange {
                    buffer: b1,
                    start_line: 0,
                    end_line: 1,
                },
                ContextTarget::ByteRange {
                    buffer: b1,
                    start: 12,
                    end: 18,
                },
                ContextTarget::ByteRange {
                    buffer: b2,
                    start: 6,
                    end: 11,
                },
                ContextTarget::ByteRange {
                    buffer: b1,
                    start: 8,
                    end: 8,
                },
            ],
            budget,
        );
        let pack = extract_context(
            &snapshot,
            &registry,
            &policy,
            actor,
            &ContextScope::all_readable(),
            &req,
        )
        .unwrap();
        assert_eq!(pack.fragments.len(), 3);
        assert_eq!(pack.fragments[0].buffer, b1);
        assert_eq!(pack.fragments[0].as_str(), "line0\nli");
        assert_eq!(pack.fragments[1].buffer, b1);
        assert_eq!(pack.fragments[1].as_str(), "line2\n");
        assert_eq!(pack.fragments[2].buffer, b2);
        assert_eq!(pack.fragments[2].as_str(), "world");
        assert_eq!(pack.total_bytes, 8 + 6 + 5);
        assert!(!pack.truncated);
    }

    #[test]
    fn non_interference_under_same_buffer_and_other_buffer_edits() {
        let actor = ActorId::new(1);
        let mut registry = ActorRegistry::new();
        registry.register(actor, ActorRole::Agent).unwrap();

        let b1 = BufferId::new(1);
        let b2 = BufferId::new(2);
        let b3 = BufferId::new(3);
        let mut policy = AccessPolicy::new();
        policy.grant_buffer(actor, b1, Capability::Read);

        // Layout in b1: [prefix: 6 bytes][perm1: "KEEP_1\n" (7 bytes)][gap: 6 bytes][perm2: "KEEP_2" (6 bytes)][suffix: variable]
        let initial_b1 = Text::from_str("abcdefKEEP_1\n123456KEEP_2_trailing_secret");
        let mut catalog1 = BufferCatalog::new();
        catalog1.register(b1, initial_b1.clone()).unwrap();
        catalog1
            .register(b2, Text::from_str("unpermitted-before"))
            .unwrap();
        let mut workspace = VersionedWorkspace::new(catalog1.clone());
        let snap1 = workspace.capture();

        let p1 = ByteSpan::validate_in(&initial_b1, 6, 13).unwrap();
        let p2 = ByteSpan::validate_in(&initial_b1, 19, 25).unwrap();
        let mut scope = ContextScope::empty();
        scope.allow_span(b1, p1);
        scope.allow_span(b1, p2);

        let budget = ContextBudget::new(100).unwrap();
        let req = ContextRequest::new(
            vec![ContextTarget::ByteRange {
                buffer: b1,
                start: 0,
                end: 1000,
            }],
            budget,
        )
        .with_mode(ScopeMode::Filter);

        let pack1 = extract_context(&snap1, &registry, &policy, actor, &scope, &req).unwrap();
        assert_eq!(pack1.fragments.len(), 2);
        assert_eq!(pack1.fragments[0].as_str(), "KEEP_1\n");
        assert_eq!(pack1.fragments[1].as_str(), "KEEP_2");

        // Mutate prefix (same 6 bytes with newlines and multibyte), gap (same 6 bytes),
        // suffix (completely different length & content), mutate & remove b2, add b3.
        let modified_b1 = Text::from_str("\n\n\n界KEEP_1\n😀\n\nKEEP_2");
        assert_eq!(&modified_b1.as_str()[6..13], "KEEP_1\n");
        assert_eq!(&modified_b1.as_str()[19..25], "KEEP_2");

        let mut catalog2 = catalog1;
        catalog2.update(b1, modified_b1.clone()).unwrap();
        catalog2
            .update(b2, Text::from_str("changed-unpermitted-secret"))
            .unwrap();
        catalog2.remove(b2).unwrap();
        catalog2
            .register(b3, Text::from_str("newly-added-unpermitted"))
            .unwrap();
        let snap2_same_rev = VersionedWorkspace::new(catalog2).capture();
        let pack2_same_rev =
            extract_context(&snap2_same_rev, &registry, &policy, actor, &scope, &req).unwrap();
        assert_eq!(pack1, pack2_same_rev);

        // Also test advancing live VersionedWorkspace revisions.
        workspace.update(b1, modified_b1).unwrap();
        workspace.remove(b2).unwrap();
        workspace
            .register(b3, Text::from_str("added-live-secret"))
            .unwrap();
        let snap_live = workspace.capture();
        let pack_live =
            extract_context(&snap_live, &registry, &policy, actor, &scope, &req).unwrap();
        assert_eq!(pack_live.revision, snap_live.revision());
        assert!(pack_live
            .fragments
            .iter()
            .all(|f| f.revision == snap_live.revision()));
        assert!(pack1.same_content(&pack_live));

        // LineRange on a single narrowed view is also invariant to prefix newline edits!
        let single_scope =
            ContextScope::from_views([BufferView::narrowed(snap1.catalog(), b1, p1).unwrap()]);
        let line_req = ContextRequest::new(
            vec![ContextTarget::LineRange {
                buffer: b1,
                start_line: 0,
                end_line: 1,
            }],
            budget,
        );
        let l1 =
            extract_context(&snap1, &registry, &policy, actor, &single_scope, &line_req).unwrap();
        let l2 = extract_context(
            &snap2_same_rev,
            &registry,
            &policy,
            actor,
            &single_scope,
            &line_req,
        )
        .unwrap();
        assert_eq!(l1, l2);
        assert_eq!(l1.fragments[0].as_str(), "KEEP_1\n");
    }

    #[test]
    fn session_integration_extracts_narrowed_and_shared_context() {
        let human = ActorId::new(1);
        let agent = ActorId::new(2);
        let mut registry = ActorRegistry::new();
        registry.register(human, ActorRole::Human).unwrap();
        registry.register(agent, ActorRole::Agent).unwrap();

        let b1 = BufferId::new(1);
        let b2 = BufferId::new(2);
        let mut catalog = BufferCatalog::new();
        catalog
            .register(b1, Text::from_str("aa_shared_zz"))
            .unwrap();
        catalog.register(b2, Text::from_str("hidden")).unwrap();

        let mut policy = AccessPolicy::new();
        policy.grant_buffer(human, b1, Capability::Read);
        policy.grant_buffer(human, b2, Capability::Read);
        policy.grant_buffer(agent, b1, Capability::Read);

        let mut seq = Sequencer::new(catalog, registry.clone());
        let mut log = DeliveryLog::new(seq.snapshot());
        let mut session = WorkspaceSession::new(seq.snapshot(), registry, policy);

        let text = session.read(human, b1).unwrap().clone();
        let h_view = BufferView::narrowed(
            seq.snapshot().catalog(),
            b1,
            ByteSpan::validate_in(&text, 1, 11).unwrap(),
        )
        .unwrap();
        let a_view = BufferView::narrowed(
            seq.snapshot().catalog(),
            b1,
            ByteSpan::validate_in(&text, 3, 9).unwrap(),
        )
        .unwrap();
        session.set_view(human, h_view).unwrap();
        session.set_view(agent, a_view).unwrap();

        let budget = ContextBudget::new(64).unwrap();
        // Unscoped Workspace target on session fails with typed error.
        assert_eq!(
            extract_session_context(
                &session,
                human,
                &ContextScope::all_readable(),
                &ContextRequest::workspace(budget),
            ),
            Err(ContextError::WorkspaceTargetUnsupportedForSession)
        );

        // Single-actor extraction respects session view narrowing.
        let human_pack = extract_session_context(
            &session,
            human,
            &ContextScope::all_readable(),
            &ContextRequest::new(vec![ContextTarget::Buffer(b1)], budget),
        )
        .unwrap();
        assert_eq!(human_pack.revision, session.revision());
        assert_eq!(human_pack.fragments[0].as_str(), "a_shared_z");

        // Shared extraction intersects both session views and access policies.
        let shared = extract_shared_session_context(
            &session,
            &[
                ParticipantScope::new(human, ContextScope::from_buffers([b1, b2])),
                ParticipantScope::new(agent, ContextScope::from_buffers([b1, b2])),
            ],
            &ContextRequest::workspace(budget),
        )
        .unwrap();
        assert_eq!(shared.revision, session.revision());
        assert_eq!(shared.participants, vec![human, agent]);
        assert_eq!(shared.fragments.len(), 1);
        assert_eq!(shared.fragments[0].buffer, b1);
        assert_eq!(shared.fragments[0].as_str(), "shared");

        // Advance session through a verified delivery editing b2 (unreadable by agent)
        // and inserting at offset 0 of b1 (projecting both views cleanly).
        let before = seq.snapshot();
        let b1_text = before.catalog().get(b1).unwrap();
        let b2_text = before.catalog().get(b2).unwrap();
        let txn = Transaction::new(
            EditIntent::for_actor(human, Nonce::new(0), before.revision()),
            vec![
                (
                    b1,
                    Patch::new(vec![Splice::insert(
                        b1_text.validate_offset(0).unwrap(),
                        Text::from_str("!!"),
                    )])
                    .unwrap(),
                ),
                (
                    b2,
                    Patch::new(vec![Splice::insert(
                        b2_text.validate_offset(0).unwrap(),
                        Text::from_str("SECRET"),
                    )])
                    .unwrap(),
                ),
            ],
        )
        .unwrap();
        let receipt = seq.submit(txn).committed_receipt().unwrap();
        log.record_workspace(&seq, receipt).unwrap();
        session.advance(log.events().last().unwrap()).unwrap();

        let shared_after = extract_shared_session_context(
            &session,
            &[
                ParticipantScope::new(human, ContextScope::from_buffers([b1, b2])),
                ParticipantScope::new(agent, ContextScope::from_buffers([b1, b2])),
            ],
            &ContextRequest::workspace(budget),
        )
        .unwrap();
        assert_eq!(shared_after.revision, session.revision());
        assert_eq!(shared_after.fragments.len(), 1);
        assert_eq!(shared_after.fragments[0].as_str(), "shared");
        assert_eq!(shared_after.fragments[0].span.start().as_usize(), 5);
        assert_eq!(shared_after.fragments[0].span.end().as_usize(), 11);

        // Inactive actor check works even on empty target list.
        assert_eq!(
            extract_session_context(
                &session,
                ActorId::new(99),
                &ContextScope::all_readable(),
                &ContextRequest::new(vec![], budget),
            ),
            Err(ContextError::Access(AccessError::InactiveActor(
                ActorId::new(99)
            )))
        );
    }

    #[test]
    fn inactive_and_revoked_actors_fail_before_revision_check() {
        let active = ActorId::new(1);
        let revoked = ActorId::new(2);
        let unknown = ActorId::new(3);
        let mut registry = ActorRegistry::new();
        registry.register(active, ActorRole::Human).unwrap();
        registry.register(revoked, ActorRole::Agent).unwrap();
        registry.revoke(revoked).unwrap();

        let b1 = BufferId::new(1);
        let mut workspace = VersionedWorkspace::new(BufferCatalog::new());
        workspace.register(b1, Text::from_str("hello")).unwrap();
        workspace.update(b1, Text::from_str("hello world")).unwrap();
        let snapshot = workspace.capture();
        assert_eq!(snapshot.revision(), Revision::new(2));

        let mut policy = AccessPolicy::new();
        policy.grant_default(Capability::Read);
        let session = WorkspaceSession::new(snapshot.clone(), registry.clone(), policy.clone());

        let budget = ContextBudget::new(64).unwrap();
        let wrong_rev = Revision::new(99);
        let req_target =
            ContextRequest::new(vec![ContextTarget::Buffer(b1)], budget).at_revision(wrong_rev);
        let req_empty = ContextRequest::new(vec![], budget).at_revision(wrong_rev);

        // Active actor still observes RevisionMismatch on both snapshot and session.
        let expected_mismatch = Err(ContextError::RevisionMismatch {
            expected: wrong_rev,
            actual: snapshot.revision(),
        });
        assert_eq!(
            extract_context(
                &snapshot,
                &registry,
                &policy,
                active,
                &ContextScope::all_readable(),
                &req_target,
            ),
            expected_mismatch
        );
        assert_eq!(
            reference_extract_snapshot(
                &snapshot,
                &registry,
                &policy,
                &[ParticipantScope::all_readable(active)],
                &req_target,
            ),
            expected_mismatch
        );
        assert_eq!(
            extract_session_context(&session, active, &ContextScope::all_readable(), &req_target),
            expected_mismatch
        );
        assert_eq!(
            extract_shared_session_context(
                &session,
                &[ParticipantScope::all_readable(active)],
                &req_empty,
            ),
            expected_mismatch
        );

        // Unknown and revoked actors fail with InactiveActor before expected_revision is checked.
        for bad in [revoked, unknown] {
            let expected_err = Err(ContextError::Access(AccessError::InactiveActor(bad)));
            for req in [&req_target, &req_empty] {
                assert_eq!(
                    extract_context(
                        &snapshot,
                        &registry,
                        &policy,
                        bad,
                        &ContextScope::all_readable(),
                        req,
                    ),
                    expected_err
                );
                assert_eq!(
                    reference_extract_snapshot(
                        &snapshot,
                        &registry,
                        &policy,
                        &[ParticipantScope::all_readable(bad)],
                        req,
                    ),
                    expected_err
                );
                let pair = [
                    ParticipantScope::all_readable(active),
                    ParticipantScope::all_readable(bad),
                ];
                assert_eq!(
                    extract_shared_context(&snapshot, &registry, &policy, &pair, req),
                    expected_err
                );
                assert_eq!(
                    reference_extract_snapshot(&snapshot, &registry, &policy, &pair, req),
                    expected_err
                );
                assert_eq!(
                    extract_session_context(&session, bad, &ContextScope::all_readable(), req),
                    expected_err
                );
                assert_eq!(
                    extract_shared_session_context(&session, &pair, req),
                    expected_err
                );
                let reversed = [
                    ParticipantScope::all_readable(bad),
                    ParticipantScope::all_readable(active),
                ];
                assert_eq!(
                    extract_shared_session_context(&session, &reversed, req),
                    expected_err
                );
            }
        }

        // Multiple inactive participants report the lowest ActorId deterministically.
        let both_bad = [
            ParticipantScope::all_readable(unknown),
            ParticipantScope::all_readable(revoked),
        ];
        let expected_first = Err(ContextError::Access(AccessError::InactiveActor(revoked)));
        assert_eq!(
            extract_shared_context(&snapshot, &registry, &policy, &both_bad, &req_target),
            expected_first
        );
        assert_eq!(
            reference_extract_snapshot(&snapshot, &registry, &policy, &both_bad, &req_target),
            expected_first
        );
        assert_eq!(
            extract_shared_session_context(&session, &both_bad, &req_target),
            expected_first
        );
    }

    #[test]
    fn workspace_target_under_restricted_scope_filters_unpermitted_and_closed_buffers() {
        let human = ActorId::new(1);
        let agent = ActorId::new(2);
        let mut registry = ActorRegistry::new();
        registry.register(human, ActorRole::Human).unwrap();
        registry.register(agent, ActorRole::Agent).unwrap();

        let b1 = BufferId::new(1);
        let b2 = BufferId::new(2);
        let mut policy = AccessPolicy::new();
        policy.grant_buffer(human, b1, Capability::Read);
        policy.grant_buffer(human, b2, Capability::Read);
        policy.grant_buffer(agent, b1, Capability::Read);

        let mut cat_live = BufferCatalog::new();
        cat_live
            .register(b1, Text::from_str("shared-content"))
            .unwrap();
        cat_live
            .register(b2, Text::from_str("HUMAN_ONLY_SECRET"))
            .unwrap();
        let snap_live = VersionedWorkspace::new(cat_live.clone()).capture();

        let mut cat_closed = cat_live;
        cat_closed.remove(b2).unwrap();
        let snap_closed = VersionedWorkspace::new(cat_closed).capture();

        let mut cat_omitted = BufferCatalog::new();
        cat_omitted
            .register(b1, Text::from_str("shared-content"))
            .unwrap();
        let snap_omitted = VersionedWorkspace::new(cat_omitted).capture();

        let sess_live = WorkspaceSession::new(snap_live.clone(), registry.clone(), policy.clone());
        let sess_closed =
            WorkspaceSession::new(snap_closed.clone(), registry.clone(), policy.clone());
        let sess_omitted =
            WorkspaceSession::new(snap_omitted.clone(), registry.clone(), policy.clone());

        let budget = ContextBudget::new(256).unwrap();
        let shared_participants = [
            ParticipantScope::new(human, ContextScope::from_buffers([b1, b2])),
            ParticipantScope::all_readable(agent),
        ];

        // (a) Shared pack where human has Read on {b1, b2} with from_buffers([b1, b2])
        // and agent has Read on {b1} with all_readable() returns {b1} cleanly in Strict and Filter.
        for mode in [ScopeMode::Strict, ScopeMode::Filter] {
            let ws_req =
                ContextRequest::new(vec![ContextTarget::Workspace], budget).with_mode(mode);
            let snap_pack = extract_shared_context(
                &snap_live,
                &registry,
                &policy,
                &shared_participants,
                &ws_req,
            )
            .unwrap();
            let ref_pack = reference_extract_snapshot(
                &snap_live,
                &registry,
                &policy,
                &shared_participants,
                &ws_req,
            )
            .unwrap();
            let sess_pack =
                extract_shared_session_context(&sess_live, &shared_participants, &ws_req).unwrap();

            assert_eq!(snap_pack, ref_pack);
            assert_eq!(snap_pack, sess_pack);
            assert_eq!(snap_pack.participants, vec![human, agent]);
            assert_eq!(snap_pack.fragments.len(), 1);
            assert_eq!(snap_pack.fragments[0].buffer, b1);
            assert_eq!(snap_pack.fragments[0].as_str(), "shared-content");
            assert!(!format!("{snap_pack:?}").contains("HUMAN_ONLY_SECRET"));
        }

        // (b) Closing or omitting unpermitted b2 under Workspace + Strict produces identical packs
        // and never leaks lifecycle changes through different errors.
        let strict_ws = ContextRequest::new(vec![ContextTarget::Workspace], budget)
            .with_mode(ScopeMode::Strict);
        let pack_live = extract_shared_context(
            &snap_live,
            &registry,
            &policy,
            &shared_participants,
            &strict_ws,
        )
        .unwrap();
        let pack_closed = extract_shared_context(
            &snap_closed,
            &registry,
            &policy,
            &shared_participants,
            &strict_ws,
        )
        .unwrap();
        let pack_omitted = extract_shared_context(
            &snap_omitted,
            &registry,
            &policy,
            &shared_participants,
            &strict_ws,
        )
        .unwrap();
        assert_eq!(pack_live, pack_closed);
        assert_eq!(pack_live, pack_omitted);
        assert_eq!(
            reference_extract_snapshot(
                &snap_closed,
                &registry,
                &policy,
                &shared_participants,
                &strict_ws,
            )
            .unwrap(),
            pack_live
        );
        assert_eq!(
            reference_extract_snapshot(
                &snap_omitted,
                &registry,
                &policy,
                &shared_participants,
                &strict_ws,
            )
            .unwrap(),
            pack_live
        );
        assert_eq!(
            extract_shared_session_context(&sess_closed, &shared_participants, &strict_ws).unwrap(),
            pack_live
        );
        assert_eq!(
            extract_shared_session_context(&sess_omitted, &shared_participants, &strict_ws)
                .unwrap(),
            pack_live
        );

        // Single-actor restricted scope under Workspace + Strict also hides b2 lifecycle changes.
        let scoped_b1_b2 = ContextScope::from_buffers([b1, b2]);
        let single_live = extract_context(
            &snap_live,
            &registry,
            &policy,
            agent,
            &scoped_b1_b2,
            &strict_ws,
        )
        .unwrap();
        let single_closed = extract_context(
            &snap_closed,
            &registry,
            &policy,
            agent,
            &scoped_b1_b2,
            &strict_ws,
        )
        .unwrap();
        let single_omitted = extract_context(
            &snap_omitted,
            &registry,
            &policy,
            agent,
            &scoped_b1_b2,
            &strict_ws,
        )
        .unwrap();
        assert_eq!(single_live, single_closed);
        assert_eq!(single_live, single_omitted);
        assert_eq!(
            extract_session_context(&sess_live, agent, &scoped_b1_b2, &strict_ws).unwrap(),
            single_live
        );
        assert_eq!(
            extract_session_context(&sess_closed, agent, &scoped_b1_b2, &strict_ws).unwrap(),
            single_live
        );
        assert_eq!(
            extract_session_context(&sess_omitted, agent, &scoped_b1_b2, &strict_ws).unwrap(),
            single_live
        );

        // Advancing a live workspace by closing unpermitted b2 preserves exact content.
        let mut live_ws = VersionedWorkspace::new(snap_live.catalog().clone());
        let before_close = live_ws.capture();
        live_ws.remove(b2).unwrap();
        let after_close = live_ws.capture();
        let before_pack = extract_shared_context(
            &before_close,
            &registry,
            &policy,
            &shared_participants,
            &strict_ws,
        )
        .unwrap();
        let after_pack = extract_shared_context(
            &after_close,
            &registry,
            &policy,
            &shared_participants,
            &strict_ws,
        )
        .unwrap();
        assert!(before_pack.same_content(&after_pack));

        // Explicit Buffer(b2) in Strict mode still returns typed access errors.
        let explicit_b2 = ContextRequest::new(vec![ContextTarget::Buffer(b2)], budget);
        assert_eq!(
            extract_shared_context(
                &snap_live,
                &registry,
                &policy,
                &shared_participants,
                &explicit_b2,
            ),
            Err(ContextError::Access(AccessError::Denied {
                actor: agent,
                buffer: b2,
                capability: Capability::Read,
            }))
        );
        assert_eq!(
            extract_shared_context(
                &snap_closed,
                &registry,
                &policy,
                &shared_participants,
                &explicit_b2,
            ),
            Err(ContextError::Access(AccessError::BufferClosed(b2)))
        );
        assert_eq!(
            extract_shared_context(
                &snap_omitted,
                &registry,
                &policy,
                &shared_participants,
                &explicit_b2,
            ),
            Err(ContextError::Access(AccessError::UnknownBuffer(b2)))
        );
    }

    #[test]
    fn session_check_active_handles_buffer_zero_states_and_revoked_actor() {
        let active = ActorId::new(1);
        let revoked = ActorId::new(2);
        let unknown = ActorId::new(3);
        let mut registry = ActorRegistry::new();
        registry.register(active, ActorRole::Human).unwrap();
        registry.register(revoked, ActorRole::Agent).unwrap();
        registry.revoke(revoked).unwrap();

        let b0 = BufferId::new(0);
        let b1 = BufferId::new(1);
        let budget = ContextBudget::new(128).unwrap();

        // 1. BufferId(0) open + readable.
        let mut cat_readable = BufferCatalog::new();
        cat_readable
            .register(b0, Text::from_str("zero-open"))
            .unwrap();
        cat_readable
            .register(b1, Text::from_str("buf-one"))
            .unwrap();
        let mut policy_readable = AccessPolicy::new();
        policy_readable.grant_default(Capability::Read);
        let sess_open_readable = WorkspaceSession::new(
            VersionedWorkspace::new(cat_readable).capture(),
            registry.clone(),
            policy_readable,
        );

        // 2. BufferId(0) open + denied (active actor may read b1 but not b0).
        let mut cat_denied = BufferCatalog::new();
        cat_denied
            .register(b0, Text::from_str("zero-denied"))
            .unwrap();
        cat_denied.register(b1, Text::from_str("buf-one")).unwrap();
        let mut policy_denied = AccessPolicy::new();
        policy_denied.grant_buffer(active, b1, Capability::Read);
        let sess_open_denied = WorkspaceSession::new(
            VersionedWorkspace::new(cat_denied).capture(),
            registry.clone(),
            policy_denied,
        );

        // 3. BufferId(0) closed (tombstoned in catalog).
        let mut cat_closed = BufferCatalog::new();
        cat_closed
            .register(b0, Text::from_str("zero-closed"))
            .unwrap();
        cat_closed.register(b1, Text::from_str("buf-one")).unwrap();
        cat_closed.remove(b0).unwrap();
        let mut policy_closed = AccessPolicy::new();
        policy_closed.grant_default(Capability::Read);
        let sess_closed = WorkspaceSession::new(
            VersionedWorkspace::new(cat_closed).capture(),
            registry,
            policy_closed,
        );

        for session in [&sess_open_readable, &sess_open_denied, &sess_closed] {
            let source = SessionSource { session };
            assert_eq!(source.check_active(&[active]), Ok(()));

            let empty_pack = extract_session_context(
                session,
                active,
                &ContextScope::all_readable(),
                &ContextRequest::new(vec![], budget),
            )
            .unwrap();
            assert!(empty_pack.fragments.is_empty());
            assert_eq!(empty_pack.revision, session.revision());

            let b1_pack = extract_session_context(
                session,
                active,
                &ContextScope::all_readable(),
                &ContextRequest::new(vec![ContextTarget::Buffer(b1)], budget),
            )
            .unwrap();
            assert_eq!(b1_pack.fragments.len(), 1);
            assert_eq!(b1_pack.fragments[0].as_str(), "buf-one");

            for bad in [revoked, unknown] {
                let expected_err = ContextError::Access(AccessError::InactiveActor(bad));
                assert_eq!(source.check_active(&[bad]), Err(expected_err.clone()));
                assert_eq!(
                    source.check_active(&[active, bad]),
                    Err(expected_err.clone())
                );
                assert_eq!(
                    extract_session_context(
                        session,
                        bad,
                        &ContextScope::all_readable(),
                        &ContextRequest::new(vec![], budget),
                    ),
                    Err(expected_err.clone())
                );
                assert_eq!(
                    extract_session_context(
                        session,
                        bad,
                        &ContextScope::all_readable(),
                        &ContextRequest::new(vec![ContextTarget::Buffer(b1)], budget)
                            .at_revision(Revision::new(77)),
                    ),
                    Err(expected_err.clone())
                );
                assert_eq!(
                    extract_shared_session_context(
                        session,
                        &[
                            ParticipantScope::all_readable(active),
                            ParticipantScope::all_readable(bad),
                        ],
                        &ContextRequest::new(vec![ContextTarget::Buffer(b1)], budget),
                    ),
                    Err(expected_err)
                );
            }
        }
    }

    #[test]
    fn session_context_errors_disjoint_views_and_stale_caller_scopes() {
        let human = ActorId::new(1);
        let agent = ActorId::new(2);
        let mut registry = ActorRegistry::new();
        registry.register(human, ActorRole::Human).unwrap();
        registry.register(agent, ActorRole::Agent).unwrap();

        let b1 = BufferId::new(1);
        let b_shared = BufferId::new(2);
        let secret = BufferId::new(3);
        let closed = BufferId::new(4);
        let unknown_buf = BufferId::new(5);

        let mut workspace = VersionedWorkspace::new(BufferCatalog::new());
        workspace
            .register(b1, Text::from_str("0123456789abcdef"))
            .unwrap();
        workspace
            .register(b_shared, Text::from_str("overlap_zone"))
            .unwrap();
        workspace
            .register(secret, Text::from_str("HUMAN_SECRET"))
            .unwrap();
        workspace
            .register(closed, Text::from_str("WAS_OPEN"))
            .unwrap();
        workspace.remove(closed).unwrap();
        let snap = workspace.capture();

        let mut policy = AccessPolicy::new();
        for buf in [b1, b_shared, secret, closed] {
            policy.grant_buffer(human, buf, Capability::Read);
        }
        for buf in [b1, b_shared, closed] {
            policy.grant_buffer(agent, buf, Capability::Read);
        }

        let mut session = WorkspaceSession::new(snap.clone(), registry.clone(), policy.clone());
        let b1_text = snap.catalog().get(b1).unwrap();
        let shared_text = snap.catalog().get(b_shared).unwrap();

        // Disjoint session views on b1: human [0..4], agent [8..12].
        let h_b1 = BufferView::narrowed(
            snap.catalog(),
            b1,
            ByteSpan::validate_in(b1_text, 0, 4).unwrap(),
        )
        .unwrap();
        let a_b1 = BufferView::narrowed(
            snap.catalog(),
            b1,
            ByteSpan::validate_in(b1_text, 8, 12).unwrap(),
        )
        .unwrap();
        // Overlapping session views on b_shared: human [0..10], agent [4..12] -> [4..10] = "lap_zo".
        let h_shared = BufferView::narrowed(
            snap.catalog(),
            b_shared,
            ByteSpan::validate_in(shared_text, 0, 10).unwrap(),
        )
        .unwrap();
        let a_shared = BufferView::narrowed(
            snap.catalog(),
            b_shared,
            ByteSpan::validate_in(shared_text, 4, 12).unwrap(),
        )
        .unwrap();
        session.set_view(human, h_b1).unwrap();
        session.set_view(agent, a_b1).unwrap();
        session.set_view(human, h_shared).unwrap();
        session.set_view(agent, a_shared).unwrap();

        let budget = ContextBudget::new(256).unwrap();
        let all = ContextScope::all_readable();
        let shared_all = [
            ParticipantScope::all_readable(human),
            ParticipantScope::all_readable(agent),
        ];

        // 1. expected_revision match and mismatch on single and shared session extraction.
        let matching = ContextRequest::new(vec![ContextTarget::Buffer(b_shared)], budget)
            .at_revision(session.revision());
        assert_eq!(
            extract_session_context(&session, human, &all, &matching)
                .unwrap()
                .fragments[0]
                .as_str(),
            "overlap_zo"
        );
        assert_eq!(
            extract_shared_session_context(&session, &shared_all, &matching)
                .unwrap()
                .fragments[0]
                .as_str(),
            "lap_zo"
        );

        let wrong_rev = Revision::new(999);
        let mismatched = ContextRequest::new(vec![ContextTarget::Buffer(b_shared)], budget)
            .at_revision(wrong_rev);
        let expected_rev_err = Err(ContextError::RevisionMismatch {
            expected: wrong_rev,
            actual: session.revision(),
        });
        assert_eq!(
            extract_session_context(&session, human, &all, &mismatched),
            expected_rev_err
        );
        assert_eq!(
            extract_shared_session_context(&session, &shared_all, &mismatched),
            expected_rev_err
        );

        // 2. Disjoint session views in Strict vs Filter mode (shared and single-actor).
        let strict_b1 = ContextRequest::new(vec![ContextTarget::Buffer(b1)], budget);
        let filter_b1 = strict_b1.clone().with_mode(ScopeMode::Filter);
        assert_eq!(
            extract_shared_session_context(&session, &shared_all, &strict_b1),
            Err(ContextError::BufferOutsideScope { buffer: b1 })
        );
        let filtered_shared =
            extract_shared_session_context(&session, &shared_all, &filter_b1).unwrap();
        assert!(filtered_shared.fragments.is_empty());
        assert_eq!(filtered_shared.total_bytes, 0);

        // Workspace + restricted scope excludes disjoint-view buffer even in Strict mode.
        let shared_both_bufs = [
            ParticipantScope::new(human, ContextScope::from_buffers([b1, b_shared])),
            ParticipantScope::all_readable(agent),
        ];
        for mode in [ScopeMode::Strict, ScopeMode::Filter] {
            let ws_pack = extract_shared_session_context(
                &session,
                &shared_both_bufs,
                &ContextRequest::new(vec![ContextTarget::Workspace], budget).with_mode(mode),
            )
            .unwrap();
            assert_eq!(ws_pack.fragments.len(), 1);
            assert_eq!(ws_pack.fragments[0].buffer, b_shared);
            assert_eq!(ws_pack.fragments[0].as_str(), "lap_zo");
        }

        // Single-actor caller scope disjoint from actor's own session view on b1.
        let mut disjoint_caller_scope = ContextScope::empty();
        disjoint_caller_scope.allow_span(b1, ByteSpan::validate_in(b1_text, 8, 12).unwrap());
        disjoint_caller_scope.allow_buffer(b_shared);
        assert_eq!(
            extract_session_context(&session, human, &disjoint_caller_scope, &strict_b1),
            Err(ContextError::BufferOutsideScope { buffer: b1 })
        );
        let single_filtered =
            extract_session_context(&session, human, &disjoint_caller_scope, &filter_b1).unwrap();
        assert!(single_filtered.fragments.is_empty());
        let single_ws = extract_session_context(
            &session,
            human,
            &disjoint_caller_scope,
            &ContextRequest::new(vec![ContextTarget::Workspace], budget)
                .with_mode(ScopeMode::Strict),
        )
        .unwrap();
        assert_eq!(single_ws.fragments.len(), 1);
        assert_eq!(single_ws.fragments[0].buffer, b_shared);
        assert_eq!(single_ws.fragments[0].as_str(), "overlap_zo");

        // 3. Strict access and scope errors on both single and shared session extraction.
        let req_secret = ContextRequest::new(vec![ContextTarget::Buffer(secret)], budget);
        let expected_denied = Err(ContextError::Access(AccessError::Denied {
            actor: agent,
            buffer: secret,
            capability: Capability::Read,
        }));
        assert_eq!(
            extract_session_context(&session, agent, &all, &req_secret),
            expected_denied
        );
        assert_eq!(
            extract_shared_session_context(&session, &shared_all, &req_secret),
            expected_denied
        );

        let req_closed = ContextRequest::new(vec![ContextTarget::Buffer(closed)], budget);
        let expected_closed = Err(ContextError::Access(AccessError::BufferClosed(closed)));
        assert_eq!(
            extract_session_context(&session, human, &all, &req_closed),
            expected_closed
        );
        assert_eq!(
            extract_shared_session_context(&session, &shared_all, &req_closed),
            expected_closed
        );

        let req_unknown = ContextRequest::new(vec![ContextTarget::Buffer(unknown_buf)], budget);
        let expected_unknown = Err(ContextError::Access(AccessError::UnknownBuffer(
            unknown_buf,
        )));
        assert_eq!(
            extract_session_context(&session, human, &all, &req_unknown),
            expected_unknown
        );
        assert_eq!(
            extract_shared_session_context(&session, &shared_all, &req_unknown),
            expected_unknown
        );

        let only_b1 = ContextScope::from_buffers([b1]);
        let req_b_shared = ContextRequest::new(vec![ContextTarget::Buffer(b_shared)], budget);
        assert_eq!(
            extract_session_context(&session, human, &only_b1, &req_b_shared),
            Err(ContextError::BufferOutsideScope { buffer: b_shared })
        );
        assert_eq!(
            extract_shared_session_context(
                &session,
                &[
                    ParticipantScope::new(human, only_b1),
                    ParticipantScope::all_readable(agent),
                ],
                &req_b_shared,
            ),
            Err(ContextError::BufferOutsideScope { buffer: b_shared })
        );

        let req_wide_single = ContextRequest::new(
            vec![ContextTarget::ByteRange {
                buffer: b_shared,
                start: 0,
                end: 12,
            }],
            budget,
        );
        assert_eq!(
            extract_session_context(&session, human, &all, &req_wide_single),
            Err(ContextError::SpanOutsideScope {
                buffer: b_shared,
                requested_start: 0,
                requested_end: 12,
            })
        );
        let req_wide_shared = ContextRequest::new(
            vec![ContextTarget::ByteRange {
                buffer: b_shared,
                start: 2,
                end: 10,
            }],
            budget,
        );
        assert_eq!(
            extract_shared_session_context(&session, &shared_all, &req_wide_shared),
            Err(ContextError::SpanOutsideScope {
                buffer: b_shared,
                requested_start: 2,
                requested_end: 10,
            })
        );
        let clipped_shared = extract_shared_session_context(
            &session,
            &shared_all,
            &req_wide_shared.with_mode(ScopeMode::Filter),
        )
        .unwrap();
        assert_eq!(clipped_shared.fragments.len(), 1);
        assert_eq!(clipped_shared.fragments[0].as_str(), "lap_zo");

        // 4. Stale caller scopes against a session after the buffer shrinks / changes UTF-8 boundaries.
        let b_edit = BufferId::new(10);
        let mut edit_ws = VersionedWorkspace::new(BufferCatalog::new());
        edit_ws
            .register(b_edit, Text::from_str("0123456789"))
            .unwrap();
        let before_snap = edit_ws.capture();
        let old_text = before_snap.catalog().get(b_edit).unwrap();
        let oob_span = ByteSpan::validate_in(old_text, 0, 8).unwrap();
        let mid_span = ByteSpan::validate_in(old_text, 1, 2).unwrap();
        let oob_view = BufferView::narrowed(before_snap.catalog(), b_edit, oob_span).unwrap();
        let mid_view = BufferView::narrowed(before_snap.catalog(), b_edit, mid_span).unwrap();
        let oob_scope = ContextScope::from_views([oob_view]);
        let mid_scope = ContextScope::from_views([mid_view]);

        let mut edit_policy = AccessPolicy::new();
        edit_policy.grant_default(Capability::Read);
        let mut edit_session = WorkspaceSession::new(before_snap, registry, edit_policy);
        edit_ws.update(b_edit, Text::from_str("界")).unwrap();
        let after_snap = edit_ws.capture();
        edit_session.update(after_snap.clone(), b_edit).unwrap();

        let req_edit = ContextRequest::new(vec![ContextTarget::Buffer(b_edit)], budget);
        let expected_oob = Err(ContextError::StaleView {
            buffer: b_edit,
            span: oob_span,
            error: SpanError::OutOfBounds { offset: 8, len: 3 },
        });
        assert_eq!(
            extract_session_context(&edit_session, human, &oob_scope, &req_edit),
            expected_oob
        );
        assert_eq!(
            extract_shared_session_context(
                &edit_session,
                &[
                    ParticipantScope::new(human, oob_scope.clone()),
                    ParticipantScope::all_readable(agent),
                ],
                &req_edit,
            ),
            expected_oob
        );

        let expected_mid = Err(ContextError::StaleView {
            buffer: b_edit,
            span: mid_span,
            error: SpanError::NotCharBoundary { offset: 1 },
        });
        assert_eq!(
            extract_session_context(&edit_session, human, &mid_scope, &req_edit),
            expected_mid
        );
        assert_eq!(
            extract_shared_session_context(
                &edit_session,
                &[
                    ParticipantScope::new(human, mid_scope.clone()),
                    ParticipantScope::all_readable(agent),
                ],
                &req_edit,
            ),
            expected_mid
        );

        // Even with a valid full session view set on the shrunk buffer, mid-scalar caller scope fails.
        let live_full_view = BufferView::full(after_snap.catalog(), b_edit).unwrap();
        edit_session.set_view(human, live_full_view).unwrap();
        edit_session.set_view(agent, live_full_view).unwrap();
        assert_eq!(
            extract_session_context(&edit_session, human, &mid_scope, &req_edit),
            expected_mid
        );
        assert_eq!(
            extract_shared_session_context(
                &edit_session,
                &[
                    ParticipantScope::new(human, mid_scope),
                    ParticipantScope::all_readable(agent),
                ],
                &req_edit,
            ),
            expected_mid
        );
    }

    #[test]
    fn law_scope_semilattice_algebra() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| {
                let sample = Text::from_str("a界b😀c\nd");
                let make_scope = |rng: &mut Rng| -> ContextScope {
                    match rng.gen_range(0..4) {
                        0 => ContextScope::all_readable(),
                        1 => ContextScope::empty(),
                        _ => {
                            let mut scope = ContextScope::empty();
                            for b in 0..4u64 {
                                match rng.gen_range(0..4) {
                                    0 => {}
                                    1 => {
                                        scope.allow_buffer(BufferId::new(b));
                                    }
                                    _ => {
                                        let count = rng.gen_range(1..4);
                                        for _ in 0..count {
                                            scope.allow_span(
                                                BufferId::new(b),
                                                random_span(rng, &sample),
                                            );
                                        }
                                    }
                                }
                            }
                            scope
                        }
                    }
                };
                (0..24)
                    .map(|_| (make_scope(rng), make_scope(rng), make_scope(rng)))
                    .collect::<Vec<_>>()
            },
            |triples| {
                let top = ContextScope::all_readable();
                let bot = ContextScope::empty();
                for (a, b, c) in triples {
                    assert_eq!(a.intersect(b), b.intersect(a));
                    assert_eq!(a.intersect(b).intersect(c), a.intersect(&b.intersect(c)));
                    assert_eq!(a.intersect(a), *a);
                    assert_eq!(a.intersect(&top), *a);
                    assert_eq!(top.intersect(a), *a);
                    assert_eq!(a.intersect(&bot), bot);
                    assert_eq!(bot.intersect(a), bot);
                }
            },
        );
    }

    #[test]
    fn law_independent_reference_model_single_and_shared() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| {
                let corpus = texts(rng);
                let mut cases = Vec::new();
                for _ in 0..32 {
                    let mut buf_texts = Vec::new();
                    for _ in 0..5 {
                        let s = rng.choose(&corpus).unwrap().clone();
                        buf_texts.push(s);
                    }
                    let closed_mask = rng.next_u64() & 0b11111;
                    let grants_a = rng.next_u64() & 0b11111;
                    let grants_b = rng.next_u64() & 0b11111;
                    let default_grant = rng.gen_bool() && rng.gen_bool();
                    let shared = rng.gen_bool();
                    let strict = rng.gen_bool();
                    let total_budget = rng.gen_range(1..48);
                    let per_frag = if rng.gen_bool() {
                        Some(rng.gen_range(1..total_budget + 1))
                    } else {
                        None
                    };
                    let max_frags = if rng.gen_bool() {
                        Some(rng.gen_range(1..6))
                    } else {
                        None
                    };
                    let seed_local = rng.next_u64();
                    cases.push((
                        buf_texts,
                        closed_mask,
                        grants_a,
                        grants_b,
                        default_grant,
                        shared,
                        strict,
                        total_budget,
                        per_frag,
                        max_frags,
                        seed_local,
                    ));
                }
                cases
            },
            |cases| {
                for (
                    buf_texts,
                    closed_mask,
                    grants_a,
                    grants_b,
                    default_grant,
                    shared,
                    strict,
                    total_budget,
                    per_frag,
                    max_frags,
                    seed_local,
                ) in cases
                {
                    let mut rng = Rng::new(*seed_local);
                    let human = ActorId::new(1);
                    let agent = ActorId::new(2);
                    let mut registry = ActorRegistry::new();
                    registry.register(human, ActorRole::Human).unwrap();
                    registry.register(agent, ActorRole::Agent).unwrap();

                    let mut order: Vec<usize> = (0..buf_texts.len()).collect();
                    rng.shuffle(&mut order);
                    let mut catalog = BufferCatalog::new();
                    let mut reversed_catalog = BufferCatalog::new();
                    for &idx in &order {
                        let id = BufferId::new(idx as u64);
                        catalog
                            .register(id, Text::from_str(&buf_texts[idx]))
                            .unwrap();
                        if closed_mask & (1 << idx) != 0 {
                            catalog.remove(id).unwrap();
                        }
                    }
                    for &idx in order.iter().rev() {
                        let id = BufferId::new(idx as u64);
                        reversed_catalog
                            .register(id, Text::from_str(&buf_texts[idx]))
                            .unwrap();
                        if closed_mask & (1 << idx) != 0 {
                            reversed_catalog.remove(id).unwrap();
                        }
                    }
                    let snapshot = VersionedWorkspace::new(catalog).capture();
                    let reversed_snapshot = VersionedWorkspace::new(reversed_catalog).capture();

                    let mut policy = AccessPolicy::new();
                    if *default_grant {
                        policy.grant_default(Capability::Read);
                    }
                    for idx in 0..buf_texts.len() {
                        let id = BufferId::new(idx as u64);
                        if grants_a & (1 << idx) != 0 {
                            policy.grant_buffer(human, id, Capability::Read);
                        }
                        if grants_b & (1 << idx) != 0 {
                            policy.grant_buffer(agent, id, Capability::Read);
                        }
                    }

                    let build_scope = |rng: &mut Rng| -> ContextScope {
                        if rng.gen_bool() {
                            ContextScope::all_readable()
                        } else {
                            let mut scope = ContextScope::empty();
                            for (idx, src) in buf_texts.iter().enumerate() {
                                let id = BufferId::new(idx as u64);
                                match rng.gen_range(0..4) {
                                    0 => {}
                                    1 => {
                                        scope.allow_buffer(id);
                                    }
                                    _ => {
                                        let text = Text::from_str(src);
                                        let count = rng.gen_range(1..3);
                                        for _ in 0..count {
                                            scope.allow_span(id, random_span(rng, &text));
                                        }
                                    }
                                }
                            }
                            scope
                        }
                    };

                    let scope_h = build_scope(&mut rng);
                    let scope_a = build_scope(&mut rng);
                    let mut participants = vec![ParticipantScope::new(human, scope_h)];
                    if *shared {
                        participants.push(ParticipantScope::new(agent, scope_a));
                    }

                    let mut targets = Vec::new();
                    if rng.gen_bool() {
                        targets.push(ContextTarget::Workspace);
                    }
                    let target_count = rng.gen_range(0..8);
                    for _ in 0..target_count {
                        let idx = rng.gen_range(0..buf_texts.len() + 1);
                        let id = BufferId::new(idx as u64);
                        match rng.gen_range(0..5) {
                            0 => targets.push(ContextTarget::Buffer(id)),
                            1 if idx < buf_texts.len() => {
                                let text = Text::from_str(&buf_texts[idx]);
                                let span = random_span(&mut rng, &text);
                                if let Ok(view) = BufferView::narrowed(snapshot.catalog(), id, span)
                                {
                                    targets.push(ContextTarget::View(view));
                                } else {
                                    targets.push(ContextTarget::Span { buffer: id, span });
                                }
                            }
                            2 if idx < buf_texts.len() => {
                                let text = Text::from_str(&buf_texts[idx]);
                                let span = random_span(&mut rng, &text);
                                targets.push(ContextTarget::Span { buffer: id, span });
                            }
                            3 => {
                                let max_len = if idx < buf_texts.len() {
                                    buf_texts[idx].len() + 3
                                } else {
                                    8
                                };
                                let a = rng.gen_range(0..max_len + 1);
                                let b = rng.gen_range(0..max_len + 1);
                                targets.push(ContextTarget::ByteRange {
                                    buffer: id,
                                    start: a.min(b),
                                    end: a.max(b),
                                });
                            }
                            _ => {
                                let a = rng.gen_range(0..4);
                                let b = rng.gen_range(a..5);
                                targets.push(ContextTarget::LineRange {
                                    buffer: id,
                                    start_line: a,
                                    end_line: b,
                                });
                            }
                        }
                    }

                    let budget =
                        ContextBudget::with_limits(*total_budget, *per_frag, *max_frags).unwrap();
                    let mode = if *strict {
                        ScopeMode::Strict
                    } else {
                        ScopeMode::Filter
                    };
                    let request = ContextRequest::new(targets.clone(), budget).with_mode(mode);

                    let actual = extract_shared_context(
                        &snapshot,
                        &registry,
                        &policy,
                        &participants,
                        &request,
                    );
                    let expected = reference_extract_snapshot(
                        &snapshot,
                        &registry,
                        &policy,
                        &participants,
                        &request,
                    );
                    assert_eq!(actual, expected);

                    // Registration order, target permutation/duplication, and participant order invariance.
                    assert_eq!(
                        actual,
                        extract_shared_context(
                            &reversed_snapshot,
                            &registry,
                            &policy,
                            &participants,
                            &request,
                        )
                    );
                    let mut permuted_targets = targets.clone();
                    rng.shuffle(&mut permuted_targets);
                    permuted_targets.extend(targets);
                    let permuted_req =
                        ContextRequest::new(permuted_targets, budget).with_mode(mode);
                    let mut permuted_participants = participants.clone();
                    permuted_participants.reverse();
                    if let Some(first) = participants.first() {
                        permuted_participants.push(first.clone());
                    }
                    assert_eq!(
                        actual,
                        extract_shared_context(
                            &snapshot,
                            &registry,
                            &policy,
                            &permuted_participants,
                            &permuted_req,
                        )
                    );

                    // Structural invariants on every Ok pack.
                    if let Ok(pack) = actual {
                        assert_eq!(pack.revision, snapshot.revision());
                        assert!(pack.total_bytes <= budget.max_total_bytes);
                        let sum: usize =
                            pack.fragments.iter().map(ContextFragment::len_bytes).sum();
                        assert_eq!(pack.total_bytes, sum);
                        if let Some(limit) = budget.max_fragments {
                            assert!(pack.fragments.len() <= limit);
                        }
                        assert_eq!(
                            pack.truncated,
                            pack.omitted_fragments > 0
                                || pack.fragments.iter().any(|f| f.truncated)
                        );
                        let combined_scope = participants
                            .iter()
                            .fold(ContextScope::all_readable(), |acc, p| {
                                acc.intersect(&p.scope)
                            });
                        for frag in &pack.fragments {
                            assert_eq!(frag.revision, snapshot.revision());
                            assert_eq!(frag.span.len_bytes(), frag.len_bytes());
                            assert_eq!(frag.truncated, frag.omitted_bytes > 0);
                            if let Some(limit) = budget.max_fragment_bytes {
                                assert!(frag.len_bytes() <= limit);
                            }
                            for &p_actor in &pack.participants {
                                assert!(policy.is_granted(p_actor, frag.buffer, Capability::Read));
                            }
                            let b_scope = combined_scope.buffer_scope(frag.buffer).unwrap();
                            assert!(b_scope.contains_span(frag.span));
                            let live_text = snapshot.catalog().get(frag.buffer).unwrap();
                            assert_eq!(frag.span.slice_in(live_text), Ok(frag.as_str()));
                        }
                        for pair in pack.fragments.windows(2) {
                            if pair[0].buffer == pair[1].buffer {
                                assert!(pair[0].span.end() < pair[1].span.start());
                            } else {
                                assert!(pair[0].buffer < pair[1].buffer);
                            }
                        }
                    }
                }
            },
        );
    }

    #[test]
    fn law_non_interference_under_arbitrary_outside_edits() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| (0..40).map(|_| rng.next_u64()).collect::<Vec<_>>(),
            |seeds| {
                for &seed in seeds {
                    let mut rng = Rng::new(seed);
                    let human = ActorId::new(1);
                    let agent = ActorId::new(2);
                    let mut registry = ActorRegistry::new();
                    registry.register(human, ActorRole::Human).unwrap();
                    registry.register(agent, ActorRole::Agent).unwrap();

                    let mut policy = AccessPolicy::new();
                    let mut catalog_a = BufferCatalog::new();
                    let mut catalog_b = BufferCatalog::new();
                    let mut scope_h = ContextScope::empty();
                    let mut scope_a = ContextScope::empty();
                    let mut filter_targets = Vec::new();
                    let mut strict_targets = Vec::new();
                    let mut permitted_texts_b = Vec::new();

                    // Build 4 permitted buffers with [prefix][perm1][gap][perm2][suffix]
                    // and unpermitted buffers differing arbitrarily in content and lifecycle.
                    for idx in 0..4u64 {
                        let id = BufferId::new(idx);
                        policy.grant_buffer(human, id, Capability::Read);
                        policy.grant_buffer(agent, id, Capability::Read);

                        let prefix_len = rng.gen_range(0..12);
                        let p1_len = rng.gen_range(0..12);
                        let gap_len = rng.gen_range(1..12);
                        let p2_len = rng.gen_range(0..12);
                        let suffix_a_len = rng.gen_range(0..16);
                        let suffix_b_len = rng.gen_range(0..32);

                        let p1 = utf8_exact_bytes(&mut rng, p1_len);
                        let p2 = utf8_exact_bytes(&mut rng, p2_len);
                        let pre_a = utf8_exact_bytes(&mut rng, prefix_len);
                        let pre_b = utf8_exact_bytes(&mut rng, prefix_len);
                        let gap_a = utf8_exact_bytes(&mut rng, gap_len);
                        let gap_b = utf8_exact_bytes(&mut rng, gap_len);
                        let suf_a = utf8_exact_bytes(&mut rng, suffix_a_len);
                        let suf_b = utf8_exact_bytes(&mut rng, suffix_b_len);

                        let text_a = Text::from_str(&format!("{pre_a}{p1}{gap_a}{p2}{suf_a}"));
                        let text_b = Text::from_str(&format!("{pre_b}{p1}{gap_b}{p2}{suf_b}"));
                        catalog_a.register(id, text_a.clone()).unwrap();
                        catalog_b.register(id, text_b.clone()).unwrap();
                        permitted_texts_b.push((id, text_b));

                        let span1 = ByteSpan::validate_in(&text_a, prefix_len, prefix_len + p1_len)
                            .unwrap();
                        let span2_start = prefix_len + p1_len + gap_len;
                        let span2 =
                            ByteSpan::validate_in(&text_a, span2_start, span2_start + p2_len)
                                .unwrap();

                        let single_span = idx < 2 || rng.gen_bool();
                        scope_h.allow_span(id, span1);
                        scope_a.allow_span(id, span1);
                        if !single_span {
                            scope_h.allow_span(id, span2);
                            scope_a.allow_span(id, span2);
                        } else {
                            // Only human has span2 -> outside shared scope.
                            scope_h.allow_span(id, span2);
                        }

                        let p1_text = Text::from_str(&p1);
                        let p1_bounds = p1_text.char_boundaries();
                        let sa = p1_bounds[rng.gen_range(0..p1_bounds.len())].as_usize();
                        let sb = p1_bounds[rng.gen_range(0..p1_bounds.len())].as_usize();
                        let sub_span1 = ByteSpan::validate_in(
                            &text_a,
                            prefix_len + sa.min(sb),
                            prefix_len + sa.max(sb),
                        )
                        .unwrap();
                        let view_inside = BufferView::narrowed(&catalog_a, id, sub_span1).unwrap();
                        let view_wide = BufferView::full(&catalog_a, id).unwrap();

                        let line_count = LineIndex::from_text(&p1_text).line_count();
                        let start_line = rng.gen_range(0..line_count + 1);
                        let end_line = rng.gen_range(start_line..line_count + 1);

                        match idx {
                            0 => {
                                filter_targets.push(ContextTarget::LineRange {
                                    buffer: id,
                                    start_line,
                                    end_line,
                                });
                                strict_targets.push(ContextTarget::LineRange {
                                    buffer: id,
                                    start_line,
                                    end_line,
                                });
                            }
                            1 => {
                                filter_targets.push(ContextTarget::View(if rng.gen_bool() {
                                    view_wide
                                } else {
                                    view_inside
                                }));
                                strict_targets.push(ContextTarget::View(view_inside));
                            }
                            _ => {
                                match rng.gen_range(0..5) {
                                    0 => filter_targets.push(ContextTarget::Buffer(id)),
                                    1 => filter_targets.push(ContextTarget::Span {
                                        buffer: id,
                                        span: span1,
                                    }),
                                    2 => filter_targets.push(ContextTarget::ByteRange {
                                        buffer: id,
                                        start: 0,
                                        end: 500,
                                    }),
                                    3 => filter_targets.push(ContextTarget::View(view_wide)),
                                    _ if single_span => {
                                        filter_targets.push(ContextTarget::LineRange {
                                            buffer: id,
                                            start_line,
                                            end_line,
                                        });
                                    }
                                    _ => filter_targets.push(ContextTarget::View(view_inside)),
                                }
                                match rng.gen_range(0..5) {
                                    0 => strict_targets.push(ContextTarget::Buffer(id)),
                                    1 => strict_targets.push(ContextTarget::View(view_inside)),
                                    2 => strict_targets.push(ContextTarget::Span {
                                        buffer: id,
                                        span: sub_span1,
                                    }),
                                    3 => strict_targets.push(ContextTarget::ByteRange {
                                        buffer: id,
                                        start: sub_span1.start().as_usize(),
                                        end: sub_span1.end().as_usize(),
                                    }),
                                    _ if single_span => {
                                        strict_targets.push(ContextTarget::LineRange {
                                            buffer: id,
                                            start_line,
                                            end_line,
                                        });
                                    }
                                    _ => strict_targets.push(ContextTarget::Buffer(id)),
                                }
                            }
                        }
                    }

                    // Unpermitted buffers differ arbitrarily between catalog_a and catalog_b,
                    // including buffers scoped by both participants where only human has Read.
                    let unperm1 = BufferId::new(10);
                    let unperm2 = BufferId::new(11);
                    let unperm3 = BufferId::new(12);
                    let human_only_live = BufferId::new(13);
                    let human_only_closed = BufferId::new(14);
                    policy.grant_buffer(human, human_only_live, Capability::Read);
                    policy.grant_buffer(human, human_only_closed, Capability::Read);
                    scope_h.allow_buffer(human_only_live);
                    scope_a.allow_buffer(human_only_live);
                    scope_h.allow_buffer(human_only_closed);
                    scope_a.allow_buffer(human_only_closed);

                    catalog_a
                        .register(unperm1, Text::from_str("CANARY_UNPERMITTED_A1"))
                        .unwrap();
                    catalog_a
                        .register(unperm2, Text::from_str("CANARY_UNPERMITTED_A2"))
                        .unwrap();
                    catalog_a
                        .register(human_only_live, Text::from_str("HUMAN_ONLY_SECRET_A"))
                        .unwrap();
                    catalog_a
                        .register(human_only_closed, Text::from_str("HUMAN_ONLY_SECRET_OPEN"))
                        .unwrap();

                    catalog_b
                        .register(unperm1, Text::from_str("DIFFERENT_SECRET_B1_LONGER"))
                        .unwrap();
                    catalog_b
                        .register(unperm2, Text::from_str("CANARY_UNPERMITTED_A2"))
                        .unwrap();
                    catalog_b.remove(unperm2).unwrap();
                    catalog_b
                        .register(unperm3, Text::from_str("NEW_SECRET_BUFFER_B3"))
                        .unwrap();
                    catalog_b
                        .register(human_only_live, Text::from_str("HUMAN_ONLY_SECRET_B"))
                        .unwrap();
                    catalog_b
                        .register(human_only_closed, Text::from_str("HUMAN_ONLY_SECRET_OPEN"))
                        .unwrap();
                    catalog_b.remove(human_only_closed).unwrap();

                    filter_targets.push(ContextTarget::Buffer(unperm1));
                    filter_targets.push(ContextTarget::Buffer(unperm2));
                    filter_targets.push(ContextTarget::Buffer(unperm3));
                    filter_targets.push(ContextTarget::Buffer(human_only_live));
                    filter_targets.push(ContextTarget::Buffer(human_only_closed));

                    let snap_a = VersionedWorkspace::new(catalog_a.clone()).capture();
                    let snap_b = VersionedWorkspace::new(catalog_b).capture();

                    let mut live_ws = VersionedWorkspace::new(catalog_a);
                    for (id, text_b) in permitted_texts_b {
                        live_ws.update(id, text_b).unwrap();
                    }
                    live_ws
                        .update(unperm1, Text::from_str("MUTATED_LIVE_UNPERMITTED"))
                        .unwrap();
                    live_ws.remove(unperm2).unwrap();
                    live_ws
                        .register(unperm3, Text::from_str("ADDED_LIVE_UNPERMITTED"))
                        .unwrap();
                    live_ws
                        .update(human_only_live, Text::from_str("HUMAN_ONLY_SECRET_LIVE"))
                        .unwrap();
                    live_ws.remove(human_only_closed).unwrap();
                    let snap_live = live_ws.capture();

                    let total = rng.gen_range(1..64);
                    let per_frag = rng.gen_bool().then(|| rng.gen_range(1..total + 1));
                    let max_frags = rng.gen_bool().then(|| rng.gen_range(1..6));
                    let budget = ContextBudget::with_limits(total, per_frag, max_frags).unwrap();
                    let participants = [
                        ParticipantScope::new(human, scope_h),
                        ParticipantScope::new(agent, scope_a),
                    ];

                    let filter_req =
                        ContextRequest::new(filter_targets, budget).with_mode(ScopeMode::Filter);
                    let strict_req =
                        ContextRequest::new(strict_targets, budget).with_mode(ScopeMode::Strict);
                    let ws_strict_req = ContextRequest::new(vec![ContextTarget::Workspace], budget)
                        .with_mode(ScopeMode::Strict);
                    let ws_filter_req = ContextRequest::new(vec![ContextTarget::Workspace], budget)
                        .with_mode(ScopeMode::Filter);

                    let mut ws_packs = Vec::new();
                    for (is_ws, req) in [
                        (false, &filter_req),
                        (false, &strict_req),
                        (true, &ws_strict_req),
                        (true, &ws_filter_req),
                    ] {
                        let pack_a =
                            extract_shared_context(&snap_a, &registry, &policy, &participants, req)
                                .unwrap();
                        let pack_b =
                            extract_shared_context(&snap_b, &registry, &policy, &participants, req)
                                .unwrap();
                        assert_eq!(pack_a, pack_b);
                        let dump = format!("{pack_a:?}");
                        assert!(!dump.contains("CANARY_UNPERMITTED"));
                        assert!(!dump.contains("DIFFERENT_SECRET"));
                        assert!(!dump.contains("NEW_SECRET_BUFFER"));
                        assert!(!dump.contains("HUMAN_ONLY_SECRET"));

                        let pack_live = extract_shared_context(
                            &snap_live,
                            &registry,
                            &policy,
                            &participants,
                            req,
                        )
                        .unwrap();
                        assert_eq!(pack_live.revision, snap_live.revision());
                        assert!(pack_a.same_content(&pack_live));
                        if is_ws {
                            ws_packs.push(pack_a);
                        }
                    }
                    assert_eq!(ws_packs[0], ws_packs[1]);

                    // Strict out-of-scope errors are also invariant to outside edits and lifecycle.
                    let strict_span_err_req = ContextRequest::new(
                        vec![ContextTarget::ByteRange {
                            buffer: BufferId::new(0),
                            start: 0,
                            end: 500,
                        }],
                        budget,
                    )
                    .with_mode(ScopeMode::Strict);
                    assert_eq!(
                        extract_shared_context(
                            &snap_a,
                            &registry,
                            &policy,
                            &participants,
                            &strict_span_err_req,
                        ),
                        extract_shared_context(
                            &snap_b,
                            &registry,
                            &policy,
                            &participants,
                            &strict_span_err_req,
                        )
                    );
                    let strict_unscoped_err_req =
                        ContextRequest::new(vec![ContextTarget::Buffer(unperm2)], budget)
                            .with_mode(ScopeMode::Strict);
                    assert_eq!(
                        extract_shared_context(
                            &snap_a,
                            &registry,
                            &policy,
                            &participants,
                            &strict_unscoped_err_req,
                        ),
                        extract_shared_context(
                            &snap_b,
                            &registry,
                            &policy,
                            &participants,
                            &strict_unscoped_err_req,
                        )
                    );
                }
            },
        );
    }

    #[test]
    fn law_error_handling_for_invalid_inputs() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            let actor = ActorId::new(1);
            let mut registry = ActorRegistry::new();
            registry.register(actor, ActorRole::Human).unwrap();
            let mut policy = AccessPolicy::new();
            policy.grant_actor(actor, Capability::Read);
            let budget = ContextBudget::new(64).unwrap();
            let buf = BufferId::new(1);

            for source in corpus {
                let text = Text::from_str(source);
                let mut catalog = BufferCatalog::new();
                catalog.register(buf, text.clone()).unwrap();
                let snapshot = VersionedWorkspace::new(catalog).capture();

                // Out-of-bounds byte range fails in Strict mode.
                assert_eq!(
                    extract_context(
                        &snapshot,
                        &registry,
                        &policy,
                        actor,
                        &ContextScope::all_readable(),
                        &ContextRequest::new(
                            vec![ContextTarget::ByteRange {
                                buffer: buf,
                                start: 0,
                                end: source.len() + 1,
                            }],
                            budget,
                        ),
                    ),
                    Err(ContextError::InvalidSpan {
                        buffer: buf,
                        error: SpanError::OutOfBounds {
                            offset: source.len() + 1,
                            len: source.len(),
                        },
                    })
                );

                // Mid-scalar offsets fail in both Strict and Filter modes.
                for offset in 0..source.len() {
                    if !source.is_char_boundary(offset) {
                        for mode in [ScopeMode::Strict, ScopeMode::Filter] {
                            assert_eq!(
                                extract_context(
                                    &snapshot,
                                    &registry,
                                    &policy,
                                    actor,
                                    &ContextScope::all_readable(),
                                    &ContextRequest::new(
                                        vec![ContextTarget::ByteRange {
                                            buffer: buf,
                                            start: 0,
                                            end: offset,
                                        }],
                                        budget,
                                    )
                                    .with_mode(mode),
                                ),
                                Err(ContextError::InvalidSpan {
                                    buffer: buf,
                                    error: SpanError::NotCharBoundary { offset },
                                })
                            );
                        }
                    }
                }

                // Line range beyond line_count fails.
                let line_count = LineIndex::from_text(&text).line_count();
                assert_eq!(
                    extract_context(
                        &snapshot,
                        &registry,
                        &policy,
                        actor,
                        &ContextScope::all_readable(),
                        &ContextRequest::new(
                            vec![ContextTarget::LineRange {
                                buffer: buf,
                                start_line: 0,
                                end_line: line_count + 1,
                            }],
                            budget,
                        ),
                    ),
                    Err(ContextError::InvalidLineRange {
                        buffer: buf,
                        error: ClipError::LineOutOfRange {
                            line: line_count + 1,
                            line_count,
                        },
                    })
                );
            }
        });
    }
}
