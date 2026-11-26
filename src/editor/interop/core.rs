//! Unified public editing core facade across Emacs, Neovim, and Agent hosts.
//!
//! [`EditorCore`] exposes a single strictly typed, thread-safe public boundary
//! integrating collaborative coordination, capability-based access control,
//! per-actor workspace sessions, multi-format snapshot and delta projections,
//! subscription delivery and reconnect/resync, selective undo/redo, proposals,
//! navigation, search, rectangle and indentation edits, and bounded
//! permission-scoped context extraction.
//!
//! Every rejected public operation returns a typed [`CoreError`] and leaves all
//! observable core state unchanged.

mod facade {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fmt;
    use std::sync::{Arc, RwLock};

    use crate::editor::{
        collab::{
            access::{AccessError, AccessPolicy, Capability},
            checkpoints::{CheckpointError, CheckpointOutcome},
            concurrent::{AccessChange, ConcurrentCoordinator, ConcurrentError},
            coordinator::{
                Coordinator, CoordinatorCheckpoint, CoordinatorError,
                Operation as CoordinatorOperation,
            },
            delivery::{CatchUp, DeliveryError, DeliveryEvent},
            proposal::{Proposal, ProposalError, RequestId},
            rebase::RebaseError,
            receipt::{CommitReceipt, RejectReason, SubmitOutcome},
            redo::RedoError,
            registry::{ActorRegistry, RegistryError},
            retention::{self, RetentionError},
            role::ActorRole,
            selective_undo::UndoError,
        },
        edit::{
            ids::{ActorId, BufferId, Nonce, TxnId},
            indent_region::{self, IndentPolicy, IndentRegionError},
            intent::EditIntent,
            navigation::{self, NavigationError, SelectionMotion, WordMotion},
            patch::Patch,
            rectangle_edit::{
                self, Rectangle, RectangleEditError, RectangleOperation, ShortLinePolicy,
            },
            transaction::{Transaction, TransactionError},
        },
        interop::{
            agent_position::{self, AgentPosition},
            annotations::{self, AgentAnnotation, AnnotationMetadata, EmacsOverlay, NeovimExtmark},
            commands::{
                self, AgentCommand, Context as CommandContext, EmacsCommand, NeovimCommand,
            },
            coordinates::{CoordKind, EmacsCharPos, LineColByte},
            delta::{self, AgentDelta, DeltaFormats, DeltaVersion, EmacsDelta, NeovimDelta},
            emacs_position,
            error::{BoundaryError, ErrorCode},
            groups::{ChangeGroup, GroupError, GroupPlan, SequenceStep},
            neovim_position,
            region::{EndpointMode, OrientedRegion, RegionCoord},
            requests::{
                BatchEdit, Envelope, Input, InputOperation, Operation as RequestOperation,
                Replacement, Request, RequestError, Target,
            },
            resync::{Resync, ResyncError},
            snapshot::{self, AgentSnapshot, BufferSnapshot, EmacsSnapshot, NeovimSnapshot},
            subscriptions::{InitialSnapshots, Notification, Subscription},
            undo::{
                self, CoreRequest, FrontendCommand, InteropError as UndoInteropError,
                Target as UndoTarget,
            },
        },
        text::{
            anchors::Affinity,
            columns::{ColumnError, Rounding},
            delimiters::DelimiterTable,
            indent::{self, IndentError, IndentStyle},
            lines::LineIndex,
            position_map::{ByteReplacement, PositionMap},
            rectangle::{self, Endpoint},
            span::SpanError,
            utf8::{ByteOffset, Text, Utf8Error},
        },
        workspace::{
            annotations::{Annotation, AnnotationKey},
            buffer_lifecycle::ClosePolicy,
            context::{
                self, BufferScope, ContextError, ContextPack, ContextRequest, ContextScope,
                ContextTarget, ParticipantScope,
            },
            search::{self, WorkspaceSearchError},
            selections::{Direction, Selection, SelectionError},
            session::{SessionError, WorkspaceSession},
            snapshots::WorkspaceSnapshot,
            version::Revision,
            view::{BufferView, ViewError},
            view_mapping,
        },
    };

    /// Agent coordinate representation used by the public editing core.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum AgentCoordKind {
        /// Zero-based UTF-8 byte offsets.
        Byte,
        /// Zero-based line and UTF-16 code unit column.
        LineColUtf16,
    }

    impl AgentCoordKind {
        /// Converts into the underlying interop coordinate kind.
        pub const fn to_coord_kind(self) -> CoordKind {
            match self {
                Self::Byte => CoordKind::Byte,
                Self::LineColUtf16 => CoordKind::LineColUtf16,
            }
        }
    }

    /// Host frontend representation supported by [`EditorCore`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum HostFormat {
        /// Emacs one-based scalar positions, overlays, and full-text snapshots.
        Emacs,
        /// Neovim zero-based line/byte columns, extmarks, and line-array snapshots.
        Neovim,
        /// Agent typed byte or UTF-16 positions, annotations, and full-text snapshots.
        Agent(AgentCoordKind),
    }

    impl HostFormat {
        /// Agent format using UTF-8 byte offsets.
        pub const AGENT_BYTE: Self = Self::Agent(AgentCoordKind::Byte);
        /// Agent format using UTF-16 line/column offsets.
        pub const AGENT_UTF16: Self = Self::Agent(AgentCoordKind::LineColUtf16);

        /// Stable human-readable format tag.
        pub const fn as_str(self) -> &'static str {
            match self {
                Self::Emacs => "emacs",
                Self::Neovim => "neovim",
                Self::Agent(AgentCoordKind::Byte) => "agent_byte",
                Self::Agent(AgentCoordKind::LineColUtf16) => "agent_utf16",
            }
        }

        /// Coordinate kind corresponding to this host format.
        pub const fn coord_kind(self) -> CoordKind {
            match self {
                Self::Emacs => CoordKind::EmacsChar,
                Self::Neovim => CoordKind::LineColByte,
                Self::Agent(kind) => kind.to_coord_kind(),
            }
        }
    }

    impl fmt::Display for HostFormat {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.as_str())
        }
    }

    /// Opaque identifier for a subscribed frontend client stream.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct ClientId(u64);

    impl ClientId {
        /// Creates a client identifier from its raw numeric value.
        pub const fn new(value: u64) -> Self {
            Self(value)
        }

        /// Returns the raw numeric value.
        pub const fn raw(self) -> u64 {
            self.0
        }
    }

    impl fmt::Display for ClientId {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fmt(f)
        }
    }

    /// A buffer position expressed in one of the supported host formats.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum HostPosition {
        /// Emacs one-based Unicode scalar position.
        Emacs(EmacsCharPos),
        /// Neovim zero-based line and UTF-8 byte column.
        Neovim(LineColByte),
        /// Agent byte or UTF-16 line/column position.
        Agent(AgentPosition),
    }

    impl HostPosition {
        /// Returns the host format corresponding to this position variant.
        pub const fn format(self) -> HostFormat {
            match self {
                Self::Emacs(_) => HostFormat::Emacs,
                Self::Neovim(_) => HostFormat::Neovim,
                Self::Agent(AgentPosition::Byte(_)) => HostFormat::AGENT_BYTE,
                Self::Agent(AgentPosition::LineColUtf16(_)) => HostFormat::AGENT_UTF16,
            }
        }

        /// Converts this position into a [`RegionCoord`].
        pub const fn to_region_coord(self) -> RegionCoord {
            match self {
                Self::Emacs(pos) => RegionCoord::Emacs(pos),
                Self::Neovim(pos) => RegionCoord::Neovim(pos),
                Self::Agent(pos) => RegionCoord::Agent(pos),
            }
        }

        /// Converts a [`RegionCoord`] into a [`HostPosition`].
        pub const fn from_region_coord(coord: RegionCoord) -> Self {
            match coord {
                RegionCoord::Emacs(pos) => Self::Emacs(pos),
                RegionCoord::Neovim(pos) => Self::Neovim(pos),
                RegionCoord::Agent(pos) => Self::Agent(pos),
            }
        }
    }

    /// Normalized error classification returned by [`EditorCore`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub enum CoreErrorKind {
        InvalidUtf8,
        InvalidBoundary,
        InvalidUtf16Boundary,
        OutOfRange,
        InvalidSpan,
        InvalidCoordinate,
        InvalidLineEndings,
        CorruptSequence,
        UnsupportedOperation,
        EmptyPattern,
        InvalidBudget,
        InvalidLineRange,
        DisjointScope,
        InvalidRectangle,
        InvalidIndent,
        InvalidNavigation,
        RevisionMismatch,
        RevisionOverflow,
        BaseExpired,
        FutureRevision,
        Conflict,
        RequestReuse,
        TxnIdReuse,
        NoChange,
        GroupConflict,
        UnknownBuffer,
        BufferClosed,
        DuplicateBufferId,
        NameInUse,
        DirtyBuffer,
        IdExhausted,
        UnknownActor,
        RevokedActor,
        RoleConflict,
        PermissionDenied,
        OutsideScope,
        EmptyParticipants,
        MissingProposal,
        UnapprovedProposal,
        StaleApproval,
        NotHumanReviewer,
        EmptyProposal,
        ApprovalMismatch,
        ProposalRejected,
        ProposalAlreadyCommitted,
        UnknownUndoTarget,
        NotUndoOwner,
        AlreadyUndone,
        NotUndone,
        AmbiguousUndoTarget,
        InvalidUndoTransaction,
        UnknownClient,
        ResyncRequired,
        DifferentWorkspace,
        StaleSnapshot,
        ResyncGap,
        InvalidEvent,
        InvalidSessionState,
        LockPoisoned,
    }

    impl fmt::Display for CoreErrorKind {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{self:?}")
        }
    }

    /// Typed public error returned by every [`EditorCore`] operation.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct CoreError {
        kind: CoreErrorKind,
        message: String,
    }

    impl CoreError {
        /// Constructs a typed core error.
        pub fn new(kind: CoreErrorKind, message: impl Into<String>) -> Self {
            Self {
                kind,
                message: message.into(),
            }
        }

        /// Returns the normalized classification of this error.
        pub const fn kind(&self) -> CoreErrorKind {
            self.kind
        }

        /// Returns the human-readable diagnostic message.
        pub fn message(&self) -> &str {
            &self.message
        }
    }

    impl fmt::Display for CoreError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{}: {}", self.kind, self.message)
        }
    }

    impl std::error::Error for CoreError {}

    /// A buffer snapshot encoded in one of the three host representations.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum HostSnapshot {
        Emacs(EmacsSnapshot),
        Neovim(NeovimSnapshot),
        Agent(AgentSnapshot),
    }

    impl HostSnapshot {
        /// Returns the buffer identifier carried by this snapshot.
        pub const fn buffer(&self) -> BufferId {
            match self {
                Self::Emacs(s) => s.buffer,
                Self::Neovim(s) => s.buffer,
                Self::Agent(s) => s.buffer,
            }
        }

        /// Returns the workspace revision carried by this snapshot.
        pub const fn revision(&self) -> Revision {
            match self {
                Self::Emacs(s) => s.revision,
                Self::Neovim(s) => s.revision,
                Self::Agent(s) => s.revision,
            }
        }

        /// Decodes the host snapshot into its canonical [`BufferSnapshot`].
        pub fn decode(&self) -> Result<BufferSnapshot, CoreError> {
            match self {
                Self::Emacs(s) => Ok(snapshot::decode_emacs(s)),
                Self::Neovim(s) => snapshot::decode_neovim(s).map_err(map_boundary_error),
                Self::Agent(s) => Ok(snapshot::decode_agent(s)),
            }
        }

        /// Applies a matching [`HostDelta`] atomically to this host snapshot.
        pub fn apply_delta(&mut self, delta: &HostDelta) -> Result<(), CoreError> {
            match (self, delta) {
                (Self::Emacs(s), HostDelta::Emacs(d)) => {
                    delta::apply_emacs(s, d).map_err(map_boundary_error)
                }
                (Self::Neovim(s), HostDelta::Neovim(d)) => {
                    delta::apply_neovim(s, d).map_err(map_boundary_error)
                }
                (Self::Agent(s), HostDelta::Agent(d)) => {
                    delta::apply_agent(s, d).map_err(map_boundary_error)
                }
                _ => Err(CoreError::new(
                    CoreErrorKind::UnsupportedOperation,
                    "delta format does not match snapshot format",
                )),
            }
        }

        /// Applies an [`IncrementalDelivery`] batch atomically to this host snapshot.
        ///
        /// Returns `Ok(true)` if the buffer remains live, or `Ok(false)` if the
        /// delivery contained a terminal close notification for this buffer.
        pub fn apply_delivery(
            &mut self,
            delivery: &IncrementalDelivery,
        ) -> Result<bool, CoreError> {
            if self.buffer() != delivery.buffer || self.revision() != delivery.previous_revision {
                return Err(CoreError::new(
                    CoreErrorKind::RevisionMismatch,
                    "delivery base does not match local host snapshot",
                ));
            }
            let mut candidate = self.clone();
            let mut live = true;
            for notification in &delivery.notifications {
                match notification {
                    HostNotification::Change(delta) => {
                        if !live {
                            return Err(CoreError::new(
                                CoreErrorKind::InvalidEvent,
                                "change notification after buffer close",
                            ));
                        }
                        candidate.apply_delta(delta)?;
                    }
                    HostNotification::RevisionAdvanced {
                        buffer,
                        previous_revision,
                        revision,
                    } => {
                        if *buffer != candidate.buffer()
                            || *previous_revision != candidate.revision()
                            || previous_revision.next().ok() != Some(*revision)
                        {
                            return Err(CoreError::new(
                                CoreErrorKind::RevisionMismatch,
                                "non-contiguous revision advancement",
                            ));
                        }
                        candidate.set_revision(*revision);
                    }
                    HostNotification::Closed { buffer, revision } => {
                        if *buffer != candidate.buffer()
                            || candidate.revision().next().ok() != Some(*revision)
                        {
                            return Err(CoreError::new(
                                CoreErrorKind::InvalidEvent,
                                "invalid close notification",
                            ));
                        }
                        candidate.set_revision(*revision);
                        live = false;
                    }
                }
            }
            if (!live || (delivery.closed && delivery.notifications.is_empty()))
                && delivery.revision >= candidate.revision()
            {
                candidate.set_revision(delivery.revision);
                live = false;
            }
            if candidate.revision() != delivery.revision {
                return Err(CoreError::new(
                    CoreErrorKind::RevisionMismatch,
                    "delivery final revision mismatch",
                ));
            }
            *self = candidate;
            Ok(live && !delivery.closed)
        }

        /// Applies a [`ReconnectOutcome`] atomically to this host snapshot.
        ///
        /// Returns `Ok(true)` if the buffer remains live, or `Ok(false)` if closed.
        pub fn apply_reconnect(&mut self, outcome: &ReconnectOutcome) -> Result<bool, CoreError> {
            match outcome {
                ReconnectOutcome::Deltas(delivery) => self.apply_delivery(delivery),
                ReconnectOutcome::Snapshot {
                    buffer,
                    previous_revision,
                    revision,
                    snapshot,
                    ..
                } => {
                    if self.buffer() != *buffer
                        || self.revision() != *previous_revision
                        || snapshot.buffer() != *buffer
                        || snapshot.revision() != *revision
                    {
                        return Err(CoreError::new(
                            CoreErrorKind::RevisionMismatch,
                            "reconnect snapshot does not match local host snapshot",
                        ));
                    }
                    if std::mem::discriminant(self) != std::mem::discriminant(snapshot) {
                        return Err(CoreError::new(
                            CoreErrorKind::UnsupportedOperation,
                            "reconnect snapshot format mismatch",
                        ));
                    }
                    *self = snapshot.clone();
                    Ok(true)
                }
                ReconnectOutcome::Closed {
                    buffer,
                    previous_revision,
                    revision,
                    ..
                } => {
                    if self.buffer() != *buffer || self.revision() != *previous_revision {
                        return Err(CoreError::new(
                            CoreErrorKind::RevisionMismatch,
                            "reconnect close does not match local host snapshot",
                        ));
                    }
                    self.set_revision(*revision);
                    Ok(false)
                }
            }
        }

        fn set_revision(&mut self, revision: Revision) {
            match self {
                Self::Emacs(s) => s.revision = revision,
                Self::Neovim(s) => s.revision = revision,
                Self::Agent(s) => s.revision = revision,
            }
        }

        fn encode(
            format: HostFormat,
            snapshot: &WorkspaceSnapshot,
            buffer: BufferId,
        ) -> Result<Self, CoreError> {
            match format {
                HostFormat::Emacs => snapshot::encode_emacs(snapshot, buffer)
                    .map(Self::Emacs)
                    .map_err(map_boundary_error),
                HostFormat::Neovim => snapshot::encode_neovim(snapshot, buffer)
                    .map(Self::Neovim)
                    .map_err(map_boundary_error),
                HostFormat::Agent(_) => snapshot::encode_agent(snapshot, buffer)
                    .map(Self::Agent)
                    .map_err(map_boundary_error),
            }
        }
    }

    /// A versioned incremental delta encoded in one of the three host formats.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum HostDelta {
        Emacs(EmacsDelta),
        Neovim(NeovimDelta),
        Agent(AgentDelta),
    }

    impl HostDelta {
        /// Returns the delta's version metadata.
        pub const fn version(&self) -> DeltaVersion {
            match self {
                Self::Emacs(d) => d.version,
                Self::Neovim(d) => d.version,
                Self::Agent(d) => d.version,
            }
        }

        fn from_formats(format: HostFormat, formats: DeltaFormats) -> Self {
            match format {
                HostFormat::Emacs => Self::Emacs(formats.emacs),
                HostFormat::Neovim => Self::Neovim(formats.neovim),
                HostFormat::Agent(_) => Self::Agent(formats.agent),
            }
        }
    }

    /// A single revision notification delivered to a subscribed client.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum HostNotification {
        /// The subscribed buffer's text changed.
        Change(HostDelta),
        /// Another buffer changed; the subscribed buffer text is unchanged at `revision`.
        RevisionAdvanced {
            buffer: BufferId,
            previous_revision: Revision,
            revision: Revision,
        },
        /// The subscribed buffer was closed.
        Closed {
            buffer: BufferId,
            revision: Revision,
        },
    }

    /// A projected narrowed or full buffer view.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct HostView {
        pub buffer: BufferId,
        pub revision: Revision,
        pub start: HostPosition,
        pub end: HostPosition,
        pub start_byte: usize,
        pub end_byte: usize,
    }

    /// A projected actor selection with both host coordinates and byte offsets.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct HostSelection {
        pub buffer: BufferId,
        pub revision: Revision,
        pub region: OrientedRegion,
        pub anchor_byte: usize,
        pub head_byte: usize,
        pub direction: Direction,
    }

    /// An annotation projected into a host's native overlay/extmark/annotation type.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum HostAnnotation {
        Emacs(EmacsOverlay),
        Neovim(NeovimExtmark),
        Agent(AgentAnnotation),
    }

    impl HostAnnotation {
        /// Returns the annotation key.
        pub const fn key(&self) -> AnnotationKey {
            self.metadata().key
        }

        /// Returns the metadata sidecar shared by all host annotation formats.
        pub const fn metadata(&self) -> &AnnotationMetadata {
            match self {
                Self::Emacs(a) => &a.metadata,
                Self::Neovim(a) => &a.metadata,
                Self::Agent(a) => &a.metadata,
            }
        }
    }

    /// One search match projected into a host's coordinate format.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct HostSearchHit {
        pub revision: Revision,
        pub buffer: BufferId,
        pub start: HostPosition,
        pub end: HostPosition,
        pub start_byte: usize,
        pub end_byte: usize,
    }

    /// Bounded, permission-scoped context pack paired with host-projected fragment spans.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct HostContextPack {
        pub pack: ContextPack,
        pub format: HostFormat,
        pub projected_spans: Vec<(HostPosition, HostPosition)>,
    }

    /// Request to open a new named buffer.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct OpenBufferRequest {
        pub request: RequestId,
        pub actor: ActorId,
        pub nonce: Nonce,
        pub base: Revision,
        pub name: String,
        pub text: Text,
    }

    /// Request to close an existing live buffer.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct CloseBufferRequest {
        pub request: RequestId,
        pub actor: ActorId,
        pub nonce: Nonce,
        pub base: Revision,
        pub buffer: BufferId,
        pub policy: ClosePolicy,
    }

    /// One replacement edit expressed in a host's [`OrientedRegion`] coordinates.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct EditSpec {
        pub buffer: BufferId,
        pub region: OrientedRegion,
        pub replacement: Text,
    }

    /// Direct single-buffer edit request.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct EditRequest {
        pub request: RequestId,
        pub actor: ActorId,
        pub nonce: Nonce,
        pub base: Revision,
        pub edit: EditSpec,
        pub sequence_step: Option<SequenceStep>,
    }

    /// Atomic multi-buffer edit batch request.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct MultiBufferBatchRequest {
        pub request: RequestId,
        pub actor: ActorId,
        pub nonce: Nonce,
        pub base: Revision,
        pub primary: BufferId,
        pub edits: Vec<EditSpec>,
        pub sequence_step: Option<SequenceStep>,
    }

    /// Native command sequence for one of the three supported hosts.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum HostCommands {
        Emacs(Vec<EmacsCommand>),
        Neovim(Vec<NeovimCommand>),
        Agent(Vec<AgentCommand>),
    }

    impl HostCommands {
        /// Returns the host format of this command sequence.
        pub const fn default_format(&self) -> HostFormat {
            match self {
                Self::Emacs(_) => HostFormat::Emacs,
                Self::Neovim(_) => HostFormat::Neovim,
                Self::Agent(_) => HostFormat::AGENT_BYTE,
            }
        }
    }

    /// Native host command execution request (supporting direct edits, change groups, and selections).
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct CommandRequest {
        pub request: RequestId,
        pub actor: ActorId,
        pub nonce: Nonce,
        pub base: Revision,
        pub buffer: BufferId,
        pub commands: HostCommands,
        pub sequence_step: Option<SequenceStep>,
    }

    /// Geometry specification for a rectangular edit.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum RectangleSpec {
        /// Direct visual line/column rectangle.
        Visual(Rectangle),
        /// Rectangle defined by two host-format buffer positions (CRLF-aware).
        Positions {
            first: HostPosition,
            second: HostPosition,
            endpoint: Endpoint,
            tab_width: usize,
        },
    }

    /// Owned rectangular edit operation.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum RectangleEditOp {
        Delete,
        Replace(Vec<String>),
        Uniform(String),
        Insert(String),
    }

    /// Request to apply a rectangular edit transaction.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct RectangleEditRequest {
        pub request: RequestId,
        pub actor: ActorId,
        pub nonce: Nonce,
        pub base: Revision,
        pub buffer: BufferId,
        pub rectangle: RectangleSpec,
        pub policy: ShortLinePolicy,
        pub operation: RectangleEditOp,
        pub sequence_step: Option<SequenceStep>,
    }

    /// Indentation adjustment applied to each line in a half-open line range.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum IndentAction {
        /// Set every line's leading indentation to an exact visual column.
        SetColumn(usize),
        /// Increase each line's leading indentation by `columns` visual columns.
        IndentBy(usize),
        /// Decrease each line's leading indentation by `columns` visual columns (saturating at 0).
        DedentBy(usize),
    }

    /// Parameters for an indentation/dedentation operation over `[start_line, end_line)`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct IndentSpec {
        pub start_line: usize,
        pub end_line: usize,
        pub tab_width: usize,
        pub style: IndentStyle,
        pub action: IndentAction,
    }

    /// Request to apply an indentation or dedentation edit transaction.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct IndentRegionRequest {
        pub request: RequestId,
        pub actor: ActorId,
        pub nonce: Nonce,
        pub base: Revision,
        pub buffer: BufferId,
        pub spec: IndentSpec,
        pub sequence_step: Option<SequenceStep>,
    }

    /// Request to move vertically by visual column with optional sticky goal column.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct VisualMoveRequest {
        pub actor: ActorId,
        pub buffer: BufferId,
        pub position: HostPosition,
        pub delta_lines: isize,
        pub goal_column: Option<usize>,
        pub tab_width: usize,
        pub rounding: Rounding,
    }

    /// Edit payload carried by a reviewable proposal.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum ProposalPayload {
        /// Single-buffer replacement edit.
        Edit(EditSpec),
        /// Multi-buffer atomic batch.
        MultiBatch {
            primary: BufferId,
            edits: Vec<EditSpec>,
        },
        /// Native host command sequence (producing one edit request).
        Commands {
            buffer: BufferId,
            commands: HostCommands,
        },
        /// Rectangular edit.
        Rectangle {
            buffer: BufferId,
            rectangle: RectangleSpec,
            policy: ShortLinePolicy,
            operation: RectangleEditOp,
        },
        /// Indentation or dedentation edit.
        Indent { buffer: BufferId, spec: IndentSpec },
    }

    /// Request to submit a reviewable edit proposal.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct ProposalRequest {
        pub request: RequestId,
        pub actor: ActorId,
        pub nonce: Nonce,
        pub base: Revision,
        pub payload: ProposalPayload,
    }

    /// Lifecycle state of a tracked proposal.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum ProposalStatus {
        /// Submitted and awaiting human review.
        Pending,
        /// Approved by at least one human reviewer at `approved_revision`.
        Approved {
            reviewers: BTreeSet<ActorId>,
            approved_revision: Revision,
        },
        /// Permanently rejected by a human reviewer.
        Rejected {
            reviewer: ActorId,
            reason: Option<String>,
        },
        /// Approved and committed to the authoritative workspace.
        Committed { receipt: CommitReceipt },
    }

    /// Observable summary of a proposal.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct ProposalInfo {
        pub request: RequestId,
        pub proposer: ActorId,
        pub txn: TxnId,
        pub base_revision: Revision,
        pub buffers: Vec<BufferId>,
        pub status: ProposalStatus,
    }

    /// Result of opening a buffer.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct OpenOutcome {
        pub buffer: BufferId,
        pub receipt: CommitReceipt,
        pub change_group: ChangeGroup,
    }

    /// Result of committing a workspace mutation.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct CommitOutcome {
        pub receipt: CommitReceipt,
        pub buffer: Option<BufferId>,
        pub change_group: ChangeGroup,
    }

    /// Result of executing a native host command sequence.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct CommandOutcome {
        pub commit: Option<CommitOutcome>,
        pub selection: Option<HostSelection>,
        pub revision: Revision,
    }

    /// Incremental notifications delivered to a subscribed client.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct IncrementalDelivery {
        pub client: ClientId,
        pub buffer: BufferId,
        pub previous_revision: Revision,
        pub revision: Revision,
        pub notifications: Vec<HostNotification>,
        pub closed: bool,
    }

    /// Result of reconnecting or resynchronizing a subscribed client.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum ReconnectOutcome {
        /// Incremental delta catch-up succeeded without needing a full snapshot.
        Deltas(IncrementalDelivery),
        /// Compaction or lifecycle boundaries required full snapshot recovery.
        Snapshot {
            client: ClientId,
            buffer: BufferId,
            previous_revision: Revision,
            revision: Revision,
            snapshot: HostSnapshot,
        },
        /// The subscribed buffer has been closed.
        Closed {
            client: ClientId,
            buffer: BufferId,
            previous_revision: Revision,
            revision: Revision,
        },
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ProposalEntry {
        info: ProposalInfo,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum ClientTrack {
        Live {
            resync: Box<Resync>,
            projections: Box<InitialSnapshots>,
        },
        Restored {
            revision: Revision,
            closed: bool,
        },
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ClientRecord {
        actor: ActorId,
        buffer: BufferId,
        format: HostFormat,
        track: ClientTrack,
    }

    impl ClientRecord {
        fn revision(&self) -> Revision {
            match &self.track {
                ClientTrack::Live { resync, .. } => resync.revision(),
                ClientTrack::Restored { revision, .. } => *revision,
            }
        }

        fn is_closed(&self) -> bool {
            match &self.track {
                ClientTrack::Live { resync, .. } => resync.is_closed(),
                ClientTrack::Restored { closed, .. } => *closed,
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct SavedClientMeta {
        actor: ActorId,
        buffer: BufferId,
        format: HostFormat,
        revision: Revision,
        closed: bool,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct SavedSessionState {
        views: BTreeMap<(ActorId, BufferId), BufferView>,
        selections: BTreeMap<(ActorId, BufferId), Selection>,
        annotations: BTreeMap<(ActorId, AnnotationKey), Annotation>,
    }

    /// Complete, restorable checkpoint of an [`EditorCore`] instance.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct CoreCheckpoint {
        coordinator: CoordinatorCheckpoint,
        actors: ActorRegistry,
        policy: AccessPolicy,
        proposals: BTreeMap<RequestId, ProposalEntry>,
        group_plan: GroupPlan,
        request_outcomes: BTreeMap<RequestId, ([u8; 32], CommitOutcome)>,
        command_outcomes: BTreeMap<RequestId, CommandOutcome>,
        saved_session: SavedSessionState,
        retained_snapshots: BTreeMap<Revision, WorkspaceSnapshot>,
        clients: BTreeMap<ClientId, SavedClientMeta>,
        next_client_id: u64,
    }

    #[derive(Debug)]
    struct CoreState {
        coordinator: ConcurrentCoordinator,
        actors: ActorRegistry,
        policy: AccessPolicy,
        session: WorkspaceSession,
        proposals: BTreeMap<RequestId, ProposalEntry>,
        group_plan: GroupPlan,
        request_outcomes: BTreeMap<RequestId, ([u8; 32], CommitOutcome)>,
        command_outcomes: BTreeMap<RequestId, CommandOutcome>,
        annotation_keys: BTreeSet<(ActorId, AnnotationKey)>,
        retained_snapshots: BTreeMap<Revision, WorkspaceSnapshot>,
        clients: BTreeMap<ClientId, ClientRecord>,
        next_client_id: u64,
    }

    /// Unified, thread-safe public editing core facade.
    ///
    /// Cloning an [`EditorCore`] yields a handle sharing the same underlying
    /// collaborative workspace, sessions, and subscriptions across threads.
    #[derive(Clone, Debug)]
    pub struct EditorCore {
        state: Arc<RwLock<CoreState>>,
    }

    impl Default for EditorCore {
        fn default() -> Self {
            Self::new()
        }
    }

    impl EditorCore {
        /// Creates an empty editing core with no registered actors or grants.
        pub fn new() -> Self {
            Self::with_initial(ActorRegistry::new(), AccessPolicy::new())
        }

        /// Creates an editing core initialized with the given actor registry and access policy.
        pub fn with_initial(actors: ActorRegistry, policy: AccessPolicy) -> Self {
            let coordinator =
                ConcurrentCoordinator::without_recording(actors.clone(), policy.clone());
            let snapshot = coordinator
                .snapshot()
                .expect("fresh coordinator snapshot cannot fail");
            let session = WorkspaceSession::new(snapshot.clone(), actors.clone(), policy.clone());
            let mut retained_snapshots = BTreeMap::new();
            retained_snapshots.insert(snapshot.revision(), snapshot);
            Self {
                state: Arc::new(RwLock::new(CoreState {
                    coordinator,
                    actors,
                    policy,
                    session,
                    proposals: BTreeMap::new(),
                    group_plan: GroupPlan::new(),
                    request_outcomes: BTreeMap::new(),
                    command_outcomes: BTreeMap::new(),
                    annotation_keys: BTreeSet::new(),
                    retained_snapshots,
                    clients: BTreeMap::new(),
                    next_client_id: 1,
                })),
            }
        }

        /// Returns whether any internal synchronization lock has been poisoned.
        pub fn is_poisoned(&self) -> bool {
            if self.state.is_poisoned() {
                return true;
            }
            match self.state.read() {
                Ok(guard) => guard.coordinator.is_poisoned(),
                Err(_) => true,
            }
        }

        /// Captures a complete, restorable checkpoint of the editing core.
        pub fn checkpoint(&self) -> Result<CoreCheckpoint, CoreError> {
            let guard = self.read_guard()?;
            let coord_checkpoint = guard
                .coordinator
                .checkpoint()
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            let snapshot = guard
                .coordinator
                .snapshot()
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            let live = snapshot.catalog().live_ids();
            let mut views = BTreeMap::new();
            let mut selections = BTreeMap::new();
            for (actor, _) in guard.actors.active_actors() {
                for &buffer in &live {
                    if let Ok(Some(view)) = guard.session.view(actor, buffer) {
                        views.insert((actor, buffer), view);
                    }
                    if let Ok(Some(sel)) = guard.session.selection(actor, buffer) {
                        selections.insert((actor, buffer), sel);
                    }
                }
            }
            let mut annotations = BTreeMap::new();
            for &(actor, key) in &guard.annotation_keys {
                if guard.actors.is_active(actor) {
                    if let Ok(Some(ann)) = guard.session.overlay(actor, key) {
                        annotations.insert((actor, key), ann.clone());
                    }
                }
            }
            let clients = guard
                .clients
                .iter()
                .map(|(&id, rec)| {
                    (
                        id,
                        SavedClientMeta {
                            actor: rec.actor,
                            buffer: rec.buffer,
                            format: rec.format,
                            revision: rec.revision(),
                            closed: rec.is_closed(),
                        },
                    )
                })
                .collect();
            let floor = guard
                .coordinator
                .floor()
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            let retained_snapshots = guard
                .retained_snapshots
                .iter()
                .filter(|(&rev, _)| rev >= floor)
                .map(|(&rev, snap)| (rev, snap.clone()))
                .collect();
            Ok(CoreCheckpoint {
                coordinator: coord_checkpoint,
                actors: guard.actors.clone(),
                policy: guard.policy.clone(),
                proposals: guard.proposals.clone(),
                group_plan: guard.group_plan.clone(),
                request_outcomes: guard.request_outcomes.clone(),
                command_outcomes: guard.command_outcomes.clone(),
                saved_session: SavedSessionState {
                    views,
                    selections,
                    annotations,
                },
                retained_snapshots,
                clients,
                next_client_id: guard.next_client_id,
            })
        }

        /// Restores an [`EditorCore`] from a previously captured [`CoreCheckpoint`].
        pub fn restore(checkpoint: &CoreCheckpoint) -> Result<Self, CoreError> {
            let coord = Coordinator::restore(&checkpoint.coordinator)
                .map_err(|e| map_coordinator_error(&e, &checkpoint.actors))?;
            let current = coord.snapshot();
            let floor = coord.workspace().floor();
            let delivery = coord.workspace().delivery();
            let delivery_floor = delivery.floor();

            // Collect every origin-bound snapshot directly available on the restored coordinator
            // (history base, delivery base, all retained delivery event endpoints, and tip)
            // before wrapping in `ConcurrentCoordinator`.
            let mut origin_snapshots: BTreeMap<Revision, WorkspaceSnapshot> = BTreeMap::new();
            origin_snapshots.insert(current.revision(), current.clone());
            let history_base = coord.workspace().history().base_snapshot();
            origin_snapshots.insert(history_base.revision(), history_base.clone());
            let delivery_base = delivery.base_snapshot();
            origin_snapshots.insert(delivery_base.revision(), delivery_base.clone());
            for ev in delivery.events() {
                origin_snapshots.insert(ev.before().revision(), ev.before().clone());
                origin_snapshots.insert(ev.after().revision(), ev.after().clone());
            }

            let coordinator = ConcurrentCoordinator::from_coordinator(coord);
            coordinator
                .set_recording(false)
                .map_err(|e| map_concurrent_error(&e, &checkpoint.actors))?;
            let restored_actors = coordinator
                .active_actors()
                .map_err(|e| map_concurrent_error(&e, &checkpoint.actors))?;
            if restored_actors != checkpoint.actors.active_actors() {
                return Err(CoreError::new(
                    CoreErrorKind::InvalidEvent,
                    "checkpoint actor registry mismatch",
                ));
            }

            let mut session = WorkspaceSession::new(
                current.clone(),
                checkpoint.actors.clone(),
                checkpoint.policy.clone(),
            );
            for (&(actor, _), &view) in &checkpoint.saved_session.views {
                if checkpoint.actors.is_active(actor) {
                    let _ = session.set_view(actor, view);
                }
            }
            for (&(actor, _), &sel) in &checkpoint.saved_session.selections {
                if checkpoint.actors.is_active(actor) {
                    let _ = session.set_selection(actor, sel);
                }
            }
            let mut annotation_keys = BTreeSet::new();
            for (&(actor, key), ann) in &checkpoint.saved_session.annotations {
                if checkpoint.actors.is_active(actor)
                    && session.set_overlay(actor, ann.clone()).is_ok()
                {
                    annotation_keys.insert((actor, key));
                }
            }

            // Prefer restored-origin snapshots, then fill any older >= floor snapshots retained
            // for coordinate decoding and base recovery.
            let mut retained_snapshots = origin_snapshots.clone();
            for (&rev, snap) in &checkpoint.retained_snapshots {
                if rev >= floor {
                    retained_snapshots
                        .entry(rev)
                        .or_insert_with(|| snap.clone());
                }
            }

            let mut clients = BTreeMap::new();
            for (&id, meta) in &checkpoint.clients {
                let track = if !meta.closed && meta.revision >= delivery_floor {
                    if let Some(snap) = origin_snapshots.get(&meta.revision) {
                        if let Ok(resync) = Resync::new(snap, meta.buffer) {
                            if let Ok((_, projections)) = Subscription::subscribe(snap, meta.buffer)
                            {
                                ClientTrack::Live {
                                    resync: Box::new(resync),
                                    projections: Box::new(projections),
                                }
                            } else {
                                ClientTrack::Restored {
                                    revision: meta.revision,
                                    closed: meta.closed,
                                }
                            }
                        } else {
                            ClientTrack::Restored {
                                revision: meta.revision,
                                closed: meta.closed,
                            }
                        }
                    } else {
                        ClientTrack::Restored {
                            revision: meta.revision,
                            closed: meta.closed,
                        }
                    }
                } else {
                    ClientTrack::Restored {
                        revision: meta.revision,
                        closed: meta.closed,
                    }
                };
                clients.insert(
                    id,
                    ClientRecord {
                        actor: meta.actor,
                        buffer: meta.buffer,
                        format: meta.format,
                        track,
                    },
                );
            }

            Ok(Self {
                state: Arc::new(RwLock::new(CoreState {
                    coordinator,
                    actors: checkpoint.actors.clone(),
                    policy: checkpoint.policy.clone(),
                    session,
                    proposals: checkpoint.proposals.clone(),
                    group_plan: checkpoint.group_plan.clone(),
                    request_outcomes: checkpoint.request_outcomes.clone(),
                    command_outcomes: checkpoint.command_outcomes.clone(),
                    annotation_keys,
                    retained_snapshots,
                    clients,
                    next_client_id: checkpoint.next_client_id,
                })),
            })
        }

        // ------------------------------------------------------------------
        // Actor & access control management
        // ------------------------------------------------------------------

        /// Registers an actor with a role. Idempotent if already active with the same role.
        pub fn register_actor(&self, actor: ActorId, role: ActorRole) -> Result<(), CoreError> {
            let mut guard = self.write_guard()?;
            let mut next_actors = guard.actors.clone();
            next_actors
                .register(actor, role)
                .map_err(map_registry_error)?;
            guard
                .coordinator
                .register_actor(actor, role)
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            guard.actors = next_actors;
            guard.rebuild_session_for_registry()?;
            Ok(())
        }

        /// Permanently revokes an active actor and drops their session state.
        pub fn revoke_actor(&self, actor: ActorId) -> Result<(), CoreError> {
            let mut guard = self.write_guard()?;
            let mut next_actors = guard.actors.clone();
            next_actors.revoke(actor).map_err(map_registry_error)?;
            guard
                .coordinator
                .revoke_actor(actor)
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            guard.actors = next_actors;
            guard.annotation_keys.retain(|&(owner, _)| owner != actor);
            guard.rebuild_session_for_registry()?;
            Ok(())
        }

        /// Returns whether `actor` is currently registered and active.
        pub fn is_actor_active(&self, actor: ActorId) -> Result<bool, CoreError> {
            let guard = self.read_guard()?;
            Ok(guard.actors.is_active(actor))
        }

        /// Returns whether `actor` has been permanently revoked.
        pub fn is_actor_revoked(&self, actor: ActorId) -> Result<bool, CoreError> {
            let guard = self.read_guard()?;
            Ok(guard.actors.is_revoked(actor))
        }

        /// Returns the role of an active actor, or `None` if unknown or revoked.
        pub fn actor_role(&self, actor: ActorId) -> Result<Option<ActorRole>, CoreError> {
            let guard = self.read_guard()?;
            Ok(guard.actors.role_of(actor))
        }

        /// Returns all active actors and their roles in ascending identifier order.
        pub fn active_actors(&self) -> Result<Vec<(ActorId, ActorRole)>, CoreError> {
            let guard = self.read_guard()?;
            Ok(guard.actors.active_actors())
        }

        /// Applies an atomic capability grant or revocation to the workspace access policy.
        pub fn update_access(&self, change: AccessChange) -> Result<(), CoreError> {
            let mut guard = self.write_guard()?;
            guard
                .coordinator
                .update_access(change)
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            apply_access_change(change, &mut guard.policy);
            let policy = guard.policy.clone();
            guard.session.set_policy(policy);
            guard.prune_inaccessible_annotation_keys();
            Ok(())
        }

        /// Grants a workspace-default capability.
        pub fn grant_default(&self, capability: Capability) -> Result<(), CoreError> {
            self.update_access(AccessChange::GrantDefault(capability))
        }

        /// Revokes a workspace-default capability.
        pub fn revoke_default(&self, capability: Capability) -> Result<(), CoreError> {
            self.update_access(AccessChange::RevokeDefault(capability))
        }

        /// Grants an actor-wide capability across all buffers.
        pub fn grant_actor(&self, actor: ActorId, capability: Capability) -> Result<(), CoreError> {
            self.update_access(AccessChange::GrantActor(actor, capability))
        }

        /// Revokes an actor-wide capability.
        pub fn revoke_actor_capability(
            &self,
            actor: ActorId,
            capability: Capability,
        ) -> Result<(), CoreError> {
            self.update_access(AccessChange::RevokeActor(actor, capability))
        }

        /// Grants a capability to an actor for a specific buffer.
        pub fn grant_buffer(
            &self,
            actor: ActorId,
            buffer: BufferId,
            capability: Capability,
        ) -> Result<(), CoreError> {
            self.update_access(AccessChange::GrantBuffer(actor, buffer, capability))
        }

        /// Revokes a buffer-scoped capability from an actor.
        pub fn revoke_buffer(
            &self,
            actor: ActorId,
            buffer: BufferId,
            capability: Capability,
        ) -> Result<(), CoreError> {
            self.update_access(AccessChange::RevokeBuffer(actor, buffer, capability))
        }

        /// Returns whether `actor` currently holds `capability` on `buffer` (requires active actor).
        pub fn has_capability(
            &self,
            actor: ActorId,
            buffer: BufferId,
            capability: Capability,
        ) -> Result<bool, CoreError> {
            let guard = self.read_guard()?;
            Ok(guard.actors.is_active(actor) && guard.policy.is_granted(actor, buffer, capability))
        }

        // ------------------------------------------------------------------
        // Workspace metadata, buffer lifecycle, and multi-format reads
        // ------------------------------------------------------------------

        /// Returns the current workspace revision.
        pub fn revision(&self) -> Result<Revision, CoreError> {
            let guard = self.read_guard()?;
            guard
                .coordinator
                .revision()
                .map_err(|e| map_concurrent_error(&e, &guard.actors))
        }

        /// Returns the current retention/compaction floor revision.
        pub fn floor(&self) -> Result<Revision, CoreError> {
            let guard = self.read_guard()?;
            guard
                .coordinator
                .floor()
                .map_err(|e| map_concurrent_error(&e, &guard.actors))
        }

        /// Returns live buffer identifiers in ascending order.
        pub fn live_buffers(&self) -> Result<Vec<BufferId>, CoreError> {
            let guard = self.read_guard()?;
            let snapshot = guard
                .coordinator
                .snapshot()
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            Ok(snapshot.catalog().live_ids())
        }

        /// Returns closed (tombstoned) buffer identifiers in ascending order.
        pub fn tombstoned_buffers(&self) -> Result<Vec<BufferId>, CoreError> {
            let guard = self.read_guard()?;
            let snapshot = guard
                .coordinator
                .snapshot()
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            Ok(snapshot.catalog().tombstoned_ids())
        }

        /// Returns the mapping from live buffer names to buffer identifiers.
        pub fn buffer_names(&self) -> Result<BTreeMap<String, BufferId>, CoreError> {
            let guard = self.read_guard()?;
            guard
                .coordinator
                .buffer_names()
                .map_err(|e| map_concurrent_error(&e, &guard.actors))
        }

        /// Returns whether a live buffer currently has unsaved modifications.
        pub fn is_buffer_dirty(&self, buffer: BufferId) -> Result<bool, CoreError> {
            let guard = self.read_guard()?;
            guard
                .coordinator
                .is_buffer_dirty(buffer)
                .map_err(|e| map_concurrent_error(&e, &guard.actors))
        }

        /// Looks up a committed receipt by request identifier.
        pub fn receipt_by_request(
            &self,
            request: RequestId,
        ) -> Result<Option<CommitReceipt>, CoreError> {
            let guard = self.read_guard()?;
            guard
                .coordinator
                .receipt_by_request(request)
                .map_err(|e| map_concurrent_error(&e, &guard.actors))
        }

        /// Looks up a committed receipt by transaction identifier.
        pub fn receipt_by_txn(&self, txn: TxnId) -> Result<Option<CommitReceipt>, CoreError> {
            let guard = self.read_guard()?;
            guard
                .coordinator
                .receipt_by_txn(txn)
                .map_err(|e| map_concurrent_error(&e, &guard.actors))
        }

        /// Looks up a committed receipt by committed revision.
        pub fn receipt_by_revision(
            &self,
            revision: Revision,
        ) -> Result<Option<CommitReceipt>, CoreError> {
            let guard = self.read_guard()?;
            guard
                .coordinator
                .receipt_by_revision(revision)
                .map_err(|e| map_concurrent_error(&e, &guard.actors))
        }

        /// Returns all projected change groups in commit order.
        pub fn change_groups(&self) -> Result<Vec<ChangeGroup>, CoreError> {
            let guard = self.read_guard()?;
            Ok(guard.group_plan.groups().to_vec())
        }

        /// Opens a new named buffer atomically.
        pub fn open_buffer(&self, req: &OpenBufferRequest) -> Result<OpenOutcome, CoreError> {
            let mut guard = self.write_guard()?;
            let digest = digest_open(req);
            if let Some(existing) = guard.check_request_retry(req.request, digest)? {
                return Ok(OpenOutcome {
                    buffer: existing.buffer.expect("open outcome includes buffer"),
                    receipt: existing.receipt,
                    change_group: existing.change_group,
                });
            }
            guard.ensure_actor_active(req.actor)?;
            let current = guard.current_snapshot()?;
            let next_id = current
                .catalog()
                .live_ids()
                .into_iter()
                .chain(current.catalog().tombstoned_ids())
                .map(|id| id.raw())
                .max()
                .map_or(Some(0), |id| id.checked_add(1))
                .ok_or_else(|| CoreError::new(CoreErrorKind::IdExhausted, "buffer id exhausted"))?;
            guard.ensure_actor_capability(
                req.actor,
                Capability::Commit,
                [BufferId::new(next_id)],
            )?;
            let envelope = Envelope::BufferOperation(Input {
                actor: Some(req.actor),
                revision: Some(req.base),
                target: Some(Target::Name(req.name.clone())),
                operation: InputOperation::Open(req.text.clone()),
            });
            let normalized = envelope.normalize(&current).map_err(map_boundary_error)?;
            let (intent, crate::editor::collab::lifecycle::Operation::Open { name, text }) =
                normalized.lifecycle_operation(req.nonce).ok_or_else(|| {
                    CoreError::new(
                        CoreErrorKind::UnsupportedOperation,
                        "open request failed lifecycle lowering",
                    )
                })?
            else {
                return Err(CoreError::new(
                    CoreErrorKind::UnsupportedOperation,
                    "expected open lifecycle operation",
                ));
            };
            let coord_op = CoordinatorOperation::Open { intent, name, text };
            let outcome = guard
                .coordinator
                .apply(req.request, coord_op)
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            let receipt = outcome.receipt.expect("open produces receipt");
            let buffer = outcome.buffer.expect("open produces buffer id");
            let commit_outcome = guard.record_open_commit(req.request, digest, receipt, buffer)?;
            Ok(OpenOutcome {
                buffer,
                receipt: commit_outcome.receipt,
                change_group: commit_outcome.change_group,
            })
        }

        /// Closes a live buffer according to `req.policy`.
        pub fn close_buffer(&self, req: &CloseBufferRequest) -> Result<CommitOutcome, CoreError> {
            let mut guard = self.write_guard()?;
            let digest = digest_close(req);
            if let Some(existing) = guard.check_request_retry(req.request, digest)? {
                return Ok(existing);
            }
            guard.ensure_actor_capability(req.actor, Capability::Commit, [req.buffer])?;
            let current = guard.current_snapshot()?;
            let envelope = Envelope::BufferOperation(Input {
                actor: Some(req.actor),
                revision: Some(req.base),
                target: Some(Target::Buffer(req.buffer)),
                operation: InputOperation::Close(req.policy),
            });
            let normalized = envelope.normalize(&current).map_err(map_boundary_error)?;
            let (
                intent,
                crate::editor::collab::lifecycle::Operation::Close {
                    buffer: target_buf,
                    policy: close_policy,
                },
            ) = normalized.lifecycle_operation(req.nonce).ok_or_else(|| {
                CoreError::new(
                    CoreErrorKind::UnsupportedOperation,
                    "close request failed lifecycle lowering",
                )
            })?
            else {
                return Err(CoreError::new(
                    CoreErrorKind::UnsupportedOperation,
                    "expected close lifecycle operation",
                ));
            };
            let coord_op = CoordinatorOperation::Close {
                intent,
                buffer: target_buf,
                policy: close_policy,
            };
            let outcome = guard
                .coordinator
                .apply(req.request, coord_op)
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            let receipt = outcome.receipt.expect("close produces receipt");
            guard.record_close_commit(req.request, digest, receipt, target_buf)
        }

        /// Reads a live buffer as an Emacs snapshot after checking read authorization.
        pub fn read_emacs(
            &self,
            actor: ActorId,
            buffer: BufferId,
        ) -> Result<EmacsSnapshot, CoreError> {
            match self.read_snapshot(actor, buffer, HostFormat::Emacs)? {
                HostSnapshot::Emacs(snapshot) => Ok(snapshot),
                _ => unreachable!("Emacs format yields EmacsSnapshot"),
            }
        }

        /// Reads a live buffer as a canonical Neovim line-array snapshot after checking read authorization.
        pub fn read_neovim(
            &self,
            actor: ActorId,
            buffer: BufferId,
        ) -> Result<NeovimSnapshot, CoreError> {
            match self.read_snapshot(actor, buffer, HostFormat::Neovim)? {
                HostSnapshot::Neovim(snapshot) => Ok(snapshot),
                _ => unreachable!("Neovim format yields NeovimSnapshot"),
            }
        }

        /// Reads a live buffer as an Agent snapshot after checking read authorization.
        pub fn read_agent(
            &self,
            actor: ActorId,
            buffer: BufferId,
        ) -> Result<AgentSnapshot, CoreError> {
            match self.read_snapshot(actor, buffer, HostFormat::AGENT_BYTE)? {
                HostSnapshot::Agent(snapshot) => Ok(snapshot),
                _ => unreachable!("Agent format yields AgentSnapshot"),
            }
        }

        /// Reads a live buffer snapshot in the requested [`HostFormat`].
        pub fn read_snapshot(
            &self,
            actor: ActorId,
            buffer: BufferId,
            format: HostFormat,
        ) -> Result<HostSnapshot, CoreError> {
            let guard = self.read_guard()?;
            let current = guard.current_snapshot()?;
            guard
                .policy
                .authorize(
                    &guard.actors,
                    current.catalog(),
                    actor,
                    Capability::Read,
                    [buffer],
                )
                .map_err(|e| map_access_error(&e, &guard.actors))?;
            HostSnapshot::encode(format, &current, buffer)
        }

        // ------------------------------------------------------------------
        // Views, selections, and annotations
        // ------------------------------------------------------------------

        /// Narrows `actor`'s private view on `buffer` to the span of `region`.
        pub fn narrow_view(
            &self,
            actor: ActorId,
            buffer: BufferId,
            region: OrientedRegion,
        ) -> Result<HostView, CoreError> {
            let mut guard = self.write_guard()?;
            let current = guard.current_snapshot()?;
            guard
                .policy
                .authorize(
                    &guard.actors,
                    current.catalog(),
                    actor,
                    Capability::Read,
                    [buffer],
                )
                .map_err(|e| map_access_error(&e, &guard.actors))?;
            let full = BufferView::full(current.catalog(), buffer).map_err(map_view_error)?;
            let selection = region
                .to_selection(current.catalog(), &full, buffer)
                .map_err(map_boundary_error)?;
            let view = BufferView::narrowed(current.catalog(), buffer, selection.range())
                .map_err(map_view_error)?;
            guard
                .session
                .set_view(actor, view)
                .map_err(|e| map_session_error(&e, &guard.actors))?;
            let format = HostPosition::from_region_coord(region.anchor).format();
            let text = current
                .catalog()
                .get(buffer)
                .expect("authorized live buffer");
            encode_host_view(text, current.revision(), view, format)
        }

        /// Widens `actor`'s private view on `buffer` back to the full buffer span.
        pub fn widen_view(
            &self,
            actor: ActorId,
            buffer: BufferId,
            format: HostFormat,
        ) -> Result<HostView, CoreError> {
            let mut guard = self.write_guard()?;
            let current = guard.current_snapshot()?;
            guard
                .policy
                .authorize(
                    &guard.actors,
                    current.catalog(),
                    actor,
                    Capability::Read,
                    [buffer],
                )
                .map_err(|e| map_access_error(&e, &guard.actors))?;
            let full = BufferView::full(current.catalog(), buffer).map_err(map_view_error)?;
            guard
                .session
                .set_view(actor, full)
                .map_err(|e| map_session_error(&e, &guard.actors))?;
            let text = current
                .catalog()
                .get(buffer)
                .expect("authorized live buffer");
            encode_host_view(text, current.revision(), full, format)
        }

        /// Reads `actor`'s current private view on `buffer` projected into `format`.
        pub fn view(
            &self,
            actor: ActorId,
            buffer: BufferId,
            format: HostFormat,
        ) -> Result<Option<HostView>, CoreError> {
            let guard = self.read_guard()?;
            let current = guard.current_snapshot()?;
            let maybe = guard
                .session
                .view(actor, buffer)
                .map_err(|e| map_session_error(&e, &guard.actors))?;
            match maybe {
                Some(view) => {
                    let text = current
                        .catalog()
                        .get(buffer)
                        .expect("authorized live buffer");
                    Ok(Some(encode_host_view(
                        text,
                        current.revision(),
                        view,
                        format,
                    )?))
                }
                None => Ok(None),
            }
        }

        /// Sets `actor`'s oriented selection on `buffer` atomically.
        pub fn set_selection(
            &self,
            actor: ActorId,
            buffer: BufferId,
            region: OrientedRegion,
        ) -> Result<HostSelection, CoreError> {
            let mut guard = self.write_guard()?;
            let current = guard.current_snapshot()?;
            guard
                .policy
                .authorize(
                    &guard.actors,
                    current.catalog(),
                    actor,
                    Capability::Read,
                    [buffer],
                )
                .map_err(|e| map_access_error(&e, &guard.actors))?;
            let existing_view = guard
                .session
                .view(actor, buffer)
                .map_err(|e| map_session_error(&e, &guard.actors))?;
            let active_view = match existing_view {
                Some(v) => v,
                None => BufferView::full(current.catalog(), buffer).map_err(map_view_error)?,
            };
            // Validate and decode BEFORE any mutation to guarantee atomicity on rejection.
            let selection = region
                .to_selection(current.catalog(), &active_view, buffer)
                .map_err(map_boundary_error)?;
            let format = HostPosition::from_region_coord(region.anchor).format();
            let projected = encode_host_selection(
                current.catalog(),
                &active_view,
                current.revision(),
                selection,
                format,
            )?;
            if existing_view.is_none() {
                guard
                    .session
                    .set_view(actor, active_view)
                    .map_err(|e| map_session_error(&e, &guard.actors))?;
            }
            guard
                .session
                .set_selection(actor, selection)
                .map_err(|e| map_session_error(&e, &guard.actors))?;
            Ok(projected)
        }

        /// Reads `actor`'s current selection on `buffer` projected into `format`.
        pub fn selection(
            &self,
            actor: ActorId,
            buffer: BufferId,
            format: HostFormat,
        ) -> Result<Option<HostSelection>, CoreError> {
            let guard = self.read_guard()?;
            let current = guard.current_snapshot()?;
            let maybe = guard
                .session
                .selection(actor, buffer)
                .map_err(|e| map_session_error(&e, &guard.actors))?;
            match maybe {
                Some(selection) => {
                    let view = guard
                        .session
                        .view(actor, buffer)
                        .map_err(|e| map_session_error(&e, &guard.actors))?
                        .unwrap_or_else(|| {
                            BufferView::full(current.catalog(), buffer)
                                .expect("authorized live buffer")
                        });
                    Ok(Some(encode_host_selection(
                        current.catalog(),
                        &view,
                        current.revision(),
                        selection,
                        format,
                    )?))
                }
                None => Ok(None),
            }
        }

        /// Sets an Emacs overlay annotation for `actor`.
        pub fn set_emacs_overlay(
            &self,
            actor: ActorId,
            overlay: &EmacsOverlay,
        ) -> Result<EmacsOverlay, CoreError> {
            match self.set_annotation(actor, &HostAnnotation::Emacs(overlay.clone()))? {
                HostAnnotation::Emacs(out) => Ok(out),
                _ => unreachable!("Emacs annotation returns EmacsOverlay"),
            }
        }

        /// Sets a Neovim extmark annotation for `actor`.
        pub fn set_neovim_extmark(
            &self,
            actor: ActorId,
            extmark: &NeovimExtmark,
        ) -> Result<NeovimExtmark, CoreError> {
            match self.set_annotation(actor, &HostAnnotation::Neovim(extmark.clone()))? {
                HostAnnotation::Neovim(out) => Ok(out),
                _ => unreachable!("Neovim annotation returns NeovimExtmark"),
            }
        }

        /// Sets an Agent annotation for `actor`.
        pub fn set_agent_annotation(
            &self,
            actor: ActorId,
            annotation: &AgentAnnotation,
        ) -> Result<AgentAnnotation, CoreError> {
            match self.set_annotation(actor, &HostAnnotation::Agent(annotation.clone()))? {
                HostAnnotation::Agent(out) => Ok(out),
                _ => unreachable!("Agent annotation returns AgentAnnotation"),
            }
        }

        /// Sets a host-typed annotation for `actor` atomically.
        pub fn set_annotation(
            &self,
            actor: ActorId,
            annotation: &HostAnnotation,
        ) -> Result<HostAnnotation, CoreError> {
            let mut guard = self.write_guard()?;
            let current = guard.current_snapshot()?;
            let key = annotation.key();
            guard
                .policy
                .authorize(
                    &guard.actors,
                    current.catalog(),
                    actor,
                    Capability::Read,
                    [key.buffer],
                )
                .map_err(|e| map_access_error(&e, &guard.actors))?;
            let text = current
                .catalog()
                .get(key.buffer)
                .expect("authorized live buffer");
            let core_ann = match annotation {
                HostAnnotation::Emacs(ov) => {
                    annotations::unproject_emacs(text, ov).map_err(map_boundary_error)?
                }
                HostAnnotation::Neovim(em) => {
                    annotations::unproject_neovim(text, em).map_err(map_boundary_error)?
                }
                HostAnnotation::Agent(ag) => {
                    annotations::unproject_agent(text, ag).map_err(map_boundary_error)?
                }
            };
            let reprojected = match annotation {
                HostAnnotation::Emacs(_) => HostAnnotation::Emacs(
                    annotations::project_emacs(text, &core_ann).map_err(map_boundary_error)?,
                ),
                HostAnnotation::Neovim(_) => HostAnnotation::Neovim(
                    annotations::project_neovim(text, &core_ann).map_err(map_boundary_error)?,
                ),
                HostAnnotation::Agent(ag) => {
                    let kind = match ag.start {
                        AgentPosition::Byte(_) => CoordKind::Byte,
                        AgentPosition::LineColUtf16(_) => CoordKind::LineColUtf16,
                    };
                    HostAnnotation::Agent(
                        annotations::project_agent(text, &core_ann, kind)
                            .map_err(map_boundary_error)?,
                    )
                }
            };
            guard
                .session
                .set_overlay(actor, core_ann)
                .map_err(|e| map_session_error(&e, &guard.actors))?;
            guard.annotation_keys.insert((actor, key));
            Ok(reprojected)
        }

        /// Reads one annotation for `actor` projected into `format`.
        pub fn annotation(
            &self,
            actor: ActorId,
            key: AnnotationKey,
            format: HostFormat,
        ) -> Result<Option<HostAnnotation>, CoreError> {
            let guard = self.read_guard()?;
            let current = guard.current_snapshot()?;
            let maybe = guard
                .session
                .overlay(actor, key)
                .map_err(|e| map_session_error(&e, &guard.actors))?;
            match maybe {
                Some(ann) => {
                    let text = current
                        .catalog()
                        .get(key.buffer)
                        .expect("authorized live buffer");
                    Ok(Some(project_host_annotation(text, ann, format)?))
                }
                None => Ok(None),
            }
        }

        /// Reads all live annotations owned by `actor` in `buffer`, projected into `format`.
        pub fn annotations_in_buffer(
            &self,
            actor: ActorId,
            buffer: BufferId,
            format: HostFormat,
        ) -> Result<Vec<HostAnnotation>, CoreError> {
            let guard = self.read_guard()?;
            let current = guard.current_snapshot()?;
            guard
                .policy
                .authorize(
                    &guard.actors,
                    current.catalog(),
                    actor,
                    Capability::Read,
                    [buffer],
                )
                .map_err(|e| map_access_error(&e, &guard.actors))?;
            let text = current
                .catalog()
                .get(buffer)
                .expect("authorized live buffer");
            let mut result = Vec::new();
            for &(owner, key) in &guard.annotation_keys {
                if owner == actor && key.buffer == buffer {
                    if let Some(ann) = guard
                        .session
                        .overlay(actor, key)
                        .map_err(|e| map_session_error(&e, &guard.actors))?
                    {
                        result.push(project_host_annotation(text, ann, format)?);
                    }
                }
            }
            Ok(result)
        }

        // ------------------------------------------------------------------
        // Navigation, search, and bounded permission-scoped context extraction
        // ------------------------------------------------------------------

        /// Computes a word motion from `position` within `actor`'s active view on `buffer`.
        pub fn move_word(
            &self,
            actor: ActorId,
            buffer: BufferId,
            position: HostPosition,
            motion: WordMotion,
        ) -> Result<HostPosition, CoreError> {
            let guard = self.read_guard()?;
            let current = guard.current_snapshot()?;
            let (text, view) = guard.authorized_text_and_view(&current, actor, buffer)?;
            let point = host_position_to_byte(text, position)?;
            let next = navigation::move_word(current.catalog(), &view, point, motion)
                .map_err(map_navigation_error)?;
            byte_to_host_position(text, next, position.format())
        }

        /// Finds the matching delimiter at `position` within `actor`'s active view on `buffer`.
        pub fn matching_delimiter(
            &self,
            actor: ActorId,
            buffer: BufferId,
            position: HostPosition,
            table: &DelimiterTable,
        ) -> Result<Option<HostPosition>, CoreError> {
            let guard = self.read_guard()?;
            let current = guard.current_snapshot()?;
            let (text, view) = guard.authorized_text_and_view(&current, actor, buffer)?;
            let point = host_position_to_byte(text, position)?;
            let found = navigation::matching_delimiter(current.catalog(), &view, point, table)
                .map_err(map_navigation_error)?;
            match found {
                Some(offset) => Ok(Some(byte_to_host_position(
                    text,
                    offset,
                    position.format(),
                )?)),
                None => Ok(None),
            }
        }

        /// Traverses the innermost enclosing delimiter pair within `actor`'s active view.
        pub fn traverse_delimiter_pair(
            &self,
            actor: ActorId,
            buffer: BufferId,
            position: HostPosition,
            table: &DelimiterTable,
            forward: bool,
        ) -> Result<Option<HostPosition>, CoreError> {
            let guard = self.read_guard()?;
            let current = guard.current_snapshot()?;
            let (text, view) = guard.authorized_text_and_view(&current, actor, buffer)?;
            let point = host_position_to_byte(text, position)?;
            let found = navigation::traverse_pair(current.catalog(), &view, point, table, forward)
                .map_err(map_navigation_error)?;
            match found {
                Some(offset) => Ok(Some(byte_to_host_position(
                    text,
                    offset,
                    position.format(),
                )?)),
                None => Ok(None),
            }
        }

        /// Moves vertically by visual column and returns `(new_position, sticky_goal_column)`.
        pub fn move_visual(
            &self,
            req: &VisualMoveRequest,
        ) -> Result<(HostPosition, usize), CoreError> {
            let guard = self.read_guard()?;
            let current = guard.current_snapshot()?;
            let (text, view) = guard.authorized_text_and_view(&current, req.actor, req.buffer)?;
            let point = host_position_to_byte(text, req.position)?;
            let (next, goal) = navigation::move_visual(
                current.catalog(),
                &view,
                point,
                req.delta_lines,
                req.goal_column,
                req.tab_width,
                req.rounding,
            )
            .map_err(map_navigation_error)?;
            let out = byte_to_host_position(text, next, req.position.format())?;
            Ok((out, goal))
        }

        /// Applies a word motion to `actor`'s current selection on `buffer` atomically.
        pub fn move_selection_word(
            &self,
            actor: ActorId,
            buffer: BufferId,
            motion: WordMotion,
            mode: SelectionMotion,
            format: HostFormat,
        ) -> Result<HostSelection, CoreError> {
            let mut guard = self.write_guard()?;
            let current = guard.current_snapshot()?;
            let (_, view) = guard.authorized_text_and_view(&current, actor, buffer)?;
            let existing = guard
                .session
                .selection(actor, buffer)
                .map_err(|e| map_session_error(&e, &guard.actors))?
                .ok_or_else(|| {
                    CoreError::new(
                        CoreErrorKind::InvalidSessionState,
                        "actor has no active selection in buffer",
                    )
                })?;
            let moved =
                navigation::move_selections(current.catalog(), &view, &[existing], motion, mode)
                    .map_err(map_navigation_error)?;
            let next = moved[0];
            let projected =
                encode_host_selection(current.catalog(), &view, current.revision(), next, format)?;
            guard
                .session
                .set_selection(actor, next)
                .map_err(|e| map_session_error(&e, &guard.actors))?;
            Ok(projected)
        }

        /// Searches all live buffers readable by `actor` and projects hits into `format`.
        pub fn search_workspace(
            &self,
            actor: ActorId,
            pattern: &str,
            format: HostFormat,
        ) -> Result<Vec<HostSearchHit>, CoreError> {
            let guard = self.read_guard()?;
            let current = guard.current_snapshot()?;
            let matches =
                search::search_workspace(&current, &guard.actors, &guard.policy, actor, pattern)
                    .map_err(|e| map_search_error(&e, &guard.actors))?;
            project_search_hits(&current, &matches, format)
        }

        /// Searches readable live buffers among `buffers` and projects hits into `format`.
        pub fn search_buffers(
            &self,
            actor: ActorId,
            buffers: &[BufferId],
            pattern: &str,
            format: HostFormat,
        ) -> Result<Vec<HostSearchHit>, CoreError> {
            let guard = self.read_guard()?;
            let current = guard.current_snapshot()?;
            let matches = search::search_buffers(
                &current,
                &guard.actors,
                &guard.policy,
                actor,
                buffers.iter().copied(),
                pattern,
            )
            .map_err(|e| map_search_error(&e, &guard.actors))?;
            project_search_hits(&current, &matches, format)
        }

        /// Extracts a bounded, permission-scoped, single-snapshot context pack for one actor.
        pub fn extract_context(
            &self,
            actor: ActorId,
            scope: &ContextScope,
            request: &ContextRequest,
            format: HostFormat,
        ) -> Result<HostContextPack, CoreError> {
            self.extract_shared_context(
                &[ParticipantScope::new(actor, scope.clone())],
                request,
                format,
            )
        }

        /// Extracts a bounded, permission-scoped, single-snapshot shared human/agent context pack.
        ///
        /// Respects `AccessPolicy`, each participant's [`ContextScope`], and any
        /// narrowed session [`BufferView`] configured on participating actors.
        pub fn extract_shared_context(
            &self,
            participants: &[ParticipantScope],
            request: &ContextRequest,
            format: HostFormat,
        ) -> Result<HostContextPack, CoreError> {
            let guard = self.read_guard()?;
            let current = guard.current_snapshot()?;

            // Validate budget and participants early, matching workspace::context precedence.
            request
                .budget
                .validate()
                .map_err(|e| map_context_error(&ContextError::InvalidBudget(e), &guard.actors))?;
            if participants.is_empty() {
                return Err(map_context_error(
                    &ContextError::EmptyParticipants,
                    &guard.actors,
                ));
            }
            let ordered_actors: Vec<ActorId> = participants
                .iter()
                .map(|p| p.actor)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            for &actor in &ordered_actors {
                guard
                    .policy
                    .authorize(
                        &guard.actors,
                        current.catalog(),
                        actor,
                        Capability::Read,
                        [],
                    )
                    .map_err(|e| map_access_error(&e, &guard.actors))?;
            }

            let mut effective_scope = ContextScope::all_readable();
            for participant in participants {
                effective_scope = effective_scope.intersect(&participant.scope);
            }

            let has_workspace_target = request
                .targets
                .iter()
                .any(|t| matches!(t, ContextTarget::Workspace));
            let pack = if has_workspace_target && effective_scope.is_all_readable() {
                // Expand Workspace across all live buffers readable by all participants whose
                // intersected session views are non-empty, then delegate to session extraction.
                let mut expanded_targets = Vec::with_capacity(request.targets.len());
                for target in &request.targets {
                    if matches!(target, ContextTarget::Workspace) {
                        for buffer in current.catalog().live_ids() {
                            let readable = ordered_actors.iter().all(|&actor| {
                                guard.policy.is_granted(actor, buffer, Capability::Read)
                            });
                            if !readable {
                                continue;
                            }
                            let mut combined = BufferScope::full();
                            for &actor in &ordered_actors {
                                if let Ok(Some(view)) = guard.session.view(actor, buffer) {
                                    combined =
                                        combined.intersect(&BufferScope::span(view.visible()));
                                }
                            }
                            if !combined.is_empty() {
                                expanded_targets.push(ContextTarget::Buffer(buffer));
                            }
                        }
                    } else {
                        expanded_targets.push(target.clone());
                    }
                }
                let expanded_request = ContextRequest {
                    targets: expanded_targets,
                    budget: request.budget,
                    mode: request.mode,
                    expected_revision: request.expected_revision,
                };
                context::extract_shared_session_context(
                    &guard.session,
                    participants,
                    &expanded_request,
                )
                .map_err(|e| map_context_error(&e, &guard.actors))?
            } else {
                context::extract_shared_session_context(&guard.session, participants, request)
                    .map_err(|e| map_context_error(&e, &guard.actors))?
            };

            let mut projected_spans = Vec::with_capacity(pack.fragments.len());
            for frag in &pack.fragments {
                let text = current
                    .catalog()
                    .get(frag.buffer)
                    .expect("emitted fragment buffer is live");
                let start = byte_to_host_position(text, frag.span.start(), format)?;
                let end = byte_to_host_position(text, frag.span.end(), format)?;
                projected_spans.push((start, end));
            }
            Ok(HostContextPack {
                pack,
                format,
                projected_spans,
            })
        }

        // ------------------------------------------------------------------
        // Direct edits, batches, commands/change groups, rectangle, indent
        // ------------------------------------------------------------------

        /// Applies a direct single-buffer edit expressed in any host coordinate system.
        pub fn apply_edit(&self, req: &EditRequest) -> Result<CommitOutcome, CoreError> {
            let mut guard = self.write_guard()?;
            let digest = digest_edit_request(req);
            if let Some(existing) = guard.check_request_retry(req.request, digest)? {
                return Ok(existing);
            }
            guard.ensure_actor_capability(req.actor, Capability::Commit, [req.edit.buffer])?;
            let base_snapshot = guard.snapshot_for_base(req.base)?;
            let txn =
                compile_single_edit(&base_snapshot, req.actor, req.nonce, req.base, &req.edit)?;
            guard.commit_edit_transaction(req.request, digest, txn, req.sequence_step)
        }

        /// Applies an atomic multi-buffer batch edit expressed in host coordinates.
        pub fn apply_multi_buffer_batch(
            &self,
            req: &MultiBufferBatchRequest,
        ) -> Result<CommitOutcome, CoreError> {
            let mut guard = self.write_guard()?;
            let digest = digest_multi_batch_request(req);
            if let Some(existing) = guard.check_request_retry(req.request, digest)? {
                return Ok(existing);
            }
            guard.ensure_actor_capability(
                req.actor,
                Capability::Commit,
                std::iter::once(req.primary).chain(req.edits.iter().map(|e| e.buffer)),
            )?;
            let base_snapshot = guard.snapshot_for_base(req.base)?;
            let txn = compile_multi_batch(
                &base_snapshot,
                req.actor,
                req.nonce,
                req.base,
                req.primary,
                &req.edits,
            )?;
            guard.commit_edit_transaction(req.request, digest, txn, req.sequence_step)
        }

        /// Executes a native Emacs, Neovim, or Agent command sequence (including change groups and selections).
        pub fn execute_commands(&self, req: &CommandRequest) -> Result<CommandOutcome, CoreError> {
            let mut guard = self.write_guard()?;
            let digest = digest_command_request(req);
            if let Some(existing) = guard.check_request_retry(req.request, digest)? {
                if let Some(cmd_out) = guard.command_outcomes.get(&req.request) {
                    return Ok(cmd_out.clone());
                }
                return Ok(CommandOutcome {
                    commit: Some(existing),
                    selection: None,
                    revision: existing.receipt.committed(),
                });
            }
            guard.ensure_actor_active(req.actor)?;
            let (needs_commit, needs_read) = command_capabilities(&req.commands);
            if needs_commit {
                guard.ensure_actor_capability(req.actor, Capability::Commit, [req.buffer])?;
            }
            if needs_read {
                guard.ensure_actor_capability(req.actor, Capability::Read, [req.buffer])?;
            }
            let current = guard.current_snapshot()?;
            let base_snapshot = guard.snapshot_for_base(req.base)?;
            let lowered =
                lower_host_commands(&base_snapshot, req.actor, req.buffer, &req.commands)?;

            let mut edit_req: Option<&Request> = None;
            let mut region_sel: Option<Selection> = None;
            for item in &lowered {
                match item.operation() {
                    RequestOperation::Edit(_) => {
                        if edit_req.is_some() {
                            return Err(CoreError::new(
                                CoreErrorKind::UnsupportedOperation,
                                "multiple ungrouped edit requests require a change group",
                            ));
                        }
                        edit_req = Some(item);
                    }
                    RequestOperation::Region(sel) => {
                        region_sel = Some(*sel);
                    }
                    _ => {
                        return Err(CoreError::new(
                            CoreErrorKind::UnsupportedOperation,
                            "unsupported lowered command operation",
                        ));
                    }
                }
            }

            let format = req.commands.default_format();
            match (edit_req, region_sel) {
                (Some(edit), maybe_sel) => {
                    if maybe_sel.is_some() && req.base != current.revision() {
                        return Err(CoreError::new(
                            CoreErrorKind::RevisionMismatch,
                            "selection combined with edit requires current revision",
                        ));
                    }
                    if maybe_sel.is_some() {
                        guard
                            .policy
                            .authorize(
                                &guard.actors,
                                current.catalog(),
                                req.actor,
                                Capability::Read,
                                [req.buffer],
                            )
                            .map_err(|e| map_access_error(&e, &guard.actors))?;
                    }
                    let (_, crate::editor::collab::lifecycle::Operation::Edit(txn)) =
                        edit.lifecycle_operation(req.nonce).ok_or_else(|| {
                            CoreError::new(
                                CoreErrorKind::CorruptSequence,
                                "failed to build edit transaction from commands",
                            )
                        })?
                    else {
                        return Err(CoreError::new(
                            CoreErrorKind::CorruptSequence,
                            "expected edit operation",
                        ));
                    };
                    // If a post-edit selection was requested, pre-validate against the actor's
                    // mapped view on simulated post-edit state before committing anything.
                    if let Some(sel) = maybe_sel {
                        let applied = txn
                            .apply_with_inverse(&current)
                            .map_err(map_transaction_error)?;
                        let target_view = match guard
                            .session
                            .view(req.actor, req.buffer)
                            .map_err(|e| map_session_error(&e, &guard.actors))?
                        {
                            Some(existing) => {
                                let old_text =
                                    current.catalog().get(req.buffer).expect("validated buffer");
                                let mut map = PositionMap::identity(old_text.len_bytes());
                                if let Some((_, patch)) =
                                    txn.patches().iter().find(|(id, _)| *id == req.buffer)
                                {
                                    for splice in patch.splices().iter().rev() {
                                        if splice.span().is_empty()
                                            && splice.replacement().is_empty()
                                        {
                                            continue;
                                        }
                                        let next = PositionMap::from_replacement(
                                            map.target_len(),
                                            ByteReplacement {
                                                start: splice.span().start().as_usize(),
                                                end: splice.span().end().as_usize(),
                                                inserted_len: splice.replacement().len_bytes(),
                                            },
                                        )
                                        .map_err(|_| {
                                            CoreError::new(
                                                CoreErrorKind::InvalidSessionState,
                                                "failed to map view across edit",
                                            )
                                        })?;
                                        map = map.compose(&next).map_err(|_| {
                                            CoreError::new(
                                                CoreErrorKind::InvalidSessionState,
                                                "failed to compose view map across edit",
                                            )
                                        })?;
                                    }
                                }
                                view_mapping::map_view(
                                    applied.catalog(),
                                    &existing,
                                    &map,
                                    old_text.len_bytes(),
                                    Affinity::Left,
                                    Affinity::Right,
                                )
                                .map_err(|_| {
                                    CoreError::new(
                                        CoreErrorKind::InvalidSessionState,
                                        "failed to project view across edit",
                                    )
                                })?
                            }
                            None => BufferView::full(applied.catalog(), req.buffer)
                                .map_err(map_view_error)?,
                        };
                        Selection::new(
                            applied.catalog(),
                            &target_view,
                            req.buffer,
                            sel.anchor(),
                            sel.head(),
                        )
                        .map_err(map_selection_error)?;
                    }
                    let commit = guard.commit_edit_transaction(
                        req.request,
                        digest,
                        txn,
                        req.sequence_step,
                    )?;
                    let after = guard.current_snapshot()?;
                    let projected_sel = if let Some(sel) = maybe_sel {
                        let existing_view = guard
                            .session
                            .view(req.actor, req.buffer)
                            .map_err(|e| map_session_error(&e, &guard.actors))?;
                        let active_view = match existing_view {
                            Some(v) => v,
                            None => {
                                let full = BufferView::full(after.catalog(), req.buffer)
                                    .map_err(map_view_error)?;
                                guard
                                    .session
                                    .set_view(req.actor, full)
                                    .map_err(|e| map_session_error(&e, &guard.actors))?;
                                full
                            }
                        };
                        let validated = Selection::new(
                            after.catalog(),
                            &active_view,
                            req.buffer,
                            sel.anchor(),
                            sel.head(),
                        )
                        .map_err(map_selection_error)?;
                        guard
                            .session
                            .set_selection(req.actor, validated)
                            .map_err(|e| map_session_error(&e, &guard.actors))?;
                        Some(encode_host_selection(
                            after.catalog(),
                            &active_view,
                            after.revision(),
                            validated,
                            format,
                        )?)
                    } else {
                        None
                    };
                    let command_outcome = CommandOutcome {
                        commit: Some(commit),
                        selection: projected_sel,
                        revision: after.revision(),
                    };
                    guard
                        .command_outcomes
                        .insert(req.request, command_outcome.clone());
                    Ok(command_outcome)
                }
                (None, Some(sel)) => {
                    if req.base != current.revision() {
                        return Err(CoreError::new(
                            CoreErrorKind::RevisionMismatch,
                            "selection requires current revision",
                        ));
                    }
                    guard
                        .policy
                        .authorize(
                            &guard.actors,
                            current.catalog(),
                            req.actor,
                            Capability::Read,
                            [req.buffer],
                        )
                        .map_err(|e| map_access_error(&e, &guard.actors))?;
                    let existing_view = guard
                        .session
                        .view(req.actor, req.buffer)
                        .map_err(|e| map_session_error(&e, &guard.actors))?;
                    let active_view = match existing_view {
                        Some(v) => v,
                        None => BufferView::full(current.catalog(), req.buffer)
                            .map_err(map_view_error)?,
                    };
                    // Validate selection BEFORE mutating view to preserve atomicity.
                    let validated = Selection::new(
                        current.catalog(),
                        &active_view,
                        req.buffer,
                        sel.anchor(),
                        sel.head(),
                    )
                    .map_err(map_selection_error)?;
                    let projected = encode_host_selection(
                        current.catalog(),
                        &active_view,
                        current.revision(),
                        validated,
                        format,
                    )?;
                    if existing_view.is_none() {
                        guard
                            .session
                            .set_view(req.actor, active_view)
                            .map_err(|e| map_session_error(&e, &guard.actors))?;
                    }
                    guard
                        .session
                        .set_selection(req.actor, validated)
                        .map_err(|e| map_session_error(&e, &guard.actors))?;
                    Ok(CommandOutcome {
                        commit: None,
                        selection: Some(projected),
                        revision: current.revision(),
                    })
                }
                (None, None) => {
                    guard
                        .policy
                        .authorize(
                            &guard.actors,
                            current.catalog(),
                            req.actor,
                            Capability::Read,
                            [req.buffer],
                        )
                        .map_err(|e| map_access_error(&e, &guard.actors))?;
                    Ok(CommandOutcome {
                        commit: None,
                        selection: None,
                        revision: current.revision(),
                    })
                }
            }
        }

        /// Applies an atomic rectangular edit transaction.
        pub fn apply_rectangle_edit(
            &self,
            req: &RectangleEditRequest,
        ) -> Result<CommitOutcome, CoreError> {
            let mut guard = self.write_guard()?;
            let digest = digest_rectangle_request(req);
            if let Some(existing) = guard.check_request_retry(req.request, digest)? {
                return Ok(existing);
            }
            guard.ensure_actor_capability(req.actor, Capability::Commit, [req.buffer])?;
            let base_snapshot = guard.snapshot_for_base(req.base)?;
            let txn = compile_rectangle_edit(
                &base_snapshot,
                req.actor,
                req.nonce,
                req.base,
                req.buffer,
                req.rectangle,
                req.policy,
                &req.operation,
            )?;
            guard.commit_edit_transaction(req.request, digest, txn, req.sequence_step)
        }

        /// Applies an atomic indentation or dedentation edit over a half-open line range.
        pub fn apply_indent_region(
            &self,
            req: &IndentRegionRequest,
        ) -> Result<CommitOutcome, CoreError> {
            let mut guard = self.write_guard()?;
            let digest = digest_indent_request(req);
            if let Some(existing) = guard.check_request_retry(req.request, digest)? {
                return Ok(existing);
            }
            guard.ensure_actor_capability(req.actor, Capability::Commit, [req.buffer])?;
            let base_snapshot = guard.snapshot_for_base(req.base)?;
            let txn = compile_indent_region(
                &base_snapshot,
                req.actor,
                req.nonce,
                req.base,
                req.buffer,
                req.spec,
            )?;
            guard.commit_edit_transaction(req.request, digest, txn, req.sequence_step)
        }

        // ------------------------------------------------------------------
        // Proposals: propose -> approve / reject -> commit
        // ------------------------------------------------------------------

        /// Submits a reviewable edit proposal without advancing workspace revision.
        pub fn propose(&self, req: &ProposalRequest) -> Result<ProposalInfo, CoreError> {
            let mut guard = self.write_guard()?;
            let targets = proposal_target_buffers(&req.payload);
            guard.ensure_actor_capability(req.actor, Capability::Propose, targets)?;
            if let Some(entry) = guard.proposals.get(&req.request) {
                if matches!(entry.info.status, ProposalStatus::Rejected { .. }) {
                    return Err(CoreError::new(
                        CoreErrorKind::ProposalRejected,
                        format!("proposal {} has already been rejected", req.request.raw()),
                    ));
                }
            } else if guard.request_outcomes.contains_key(&req.request) {
                return Err(CoreError::new(
                    CoreErrorKind::RequestReuse,
                    format!("request {} has already been used", req.request.raw()),
                ));
            }
            let current = guard.current_snapshot()?;
            if req.base != current.revision() {
                return Err(CoreError::new(
                    CoreErrorKind::RevisionMismatch,
                    format!(
                        "proposal base revision {} does not match current {}",
                        req.base,
                        current.revision()
                    ),
                ));
            }
            let txn = match &req.payload {
                ProposalPayload::Edit(edit) => {
                    compile_single_edit(&current, req.actor, req.nonce, req.base, edit)?
                }
                ProposalPayload::MultiBatch { primary, edits } => {
                    compile_multi_batch(&current, req.actor, req.nonce, req.base, *primary, edits)?
                }
                ProposalPayload::Commands { buffer, commands } => {
                    let lowered = lower_host_commands(&current, req.actor, *buffer, commands)?;
                    let mut edits = Vec::new();
                    for item in lowered {
                        match item.operation() {
                            RequestOperation::Edit(_) => edits.push(item),
                            _ => {
                                return Err(CoreError::new(
                                    CoreErrorKind::UnsupportedOperation,
                                    "proposals only support edit commands",
                                ));
                            }
                        }
                    }
                    if edits.is_empty() {
                        return Err(CoreError::new(
                            CoreErrorKind::EmptyProposal,
                            "command sequence produced no edits",
                        ));
                    }
                    if edits.len() > 1 {
                        return Err(CoreError::new(
                            CoreErrorKind::UnsupportedOperation,
                            "proposal command sequence must produce a single grouped edit",
                        ));
                    }
                    let (_, crate::editor::collab::lifecycle::Operation::Edit(txn)) =
                        edits[0].lifecycle_operation(req.nonce).ok_or_else(|| {
                            CoreError::new(
                                CoreErrorKind::CorruptSequence,
                                "failed to build proposal transaction",
                            )
                        })?
                    else {
                        return Err(CoreError::new(
                            CoreErrorKind::CorruptSequence,
                            "expected edit operation",
                        ));
                    };
                    txn
                }
                ProposalPayload::Rectangle {
                    buffer,
                    rectangle,
                    policy,
                    operation,
                } => compile_rectangle_edit(
                    &current, req.actor, req.nonce, req.base, *buffer, *rectangle, *policy,
                    operation,
                )?,
                ProposalPayload::Indent { buffer, spec } => {
                    compile_indent_region(&current, req.actor, req.nonce, req.base, *buffer, *spec)?
                }
            };
            if txn.patches().is_empty() {
                return Err(CoreError::new(
                    CoreErrorKind::EmptyProposal,
                    "proposal has no target buffers",
                ));
            }
            let buffers: Vec<BufferId> = txn.patches().iter().map(|(id, _)| *id).collect();
            let txn_id = txn.intent().txn();
            let proposal = Proposal::new(req.request, txn);
            guard
                .coordinator
                .propose(proposal)
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            let entry = guard
                .proposals
                .entry(req.request)
                .or_insert_with(|| ProposalEntry {
                    info: ProposalInfo {
                        request: req.request,
                        proposer: req.actor,
                        txn: txn_id,
                        base_revision: req.base,
                        buffers,
                        status: ProposalStatus::Pending,
                    },
                });
            Ok(entry.info.clone())
        }

        /// Records a human reviewer's approval on a pending proposal.
        pub fn approve_proposal(
            &self,
            request: RequestId,
            reviewer: ActorId,
        ) -> Result<ProposalInfo, CoreError> {
            let mut guard = self.write_guard()?;
            if let Some(entry) = guard.proposals.get(&request) {
                if matches!(entry.info.status, ProposalStatus::Rejected { .. }) {
                    return Err(CoreError::new(
                        CoreErrorKind::ProposalRejected,
                        format!("proposal {} was rejected", request.raw()),
                    ));
                }
            }
            guard
                .coordinator
                .approve(request, reviewer)
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            let current_rev = guard.current_snapshot()?.revision();
            let entry = guard.proposals.get_mut(&request).ok_or_else(|| {
                CoreError::new(
                    CoreErrorKind::MissingProposal,
                    format!("unknown proposal {}", request.raw()),
                )
            })?;
            match &mut entry.info.status {
                ProposalStatus::Pending => {
                    let mut reviewers = BTreeSet::new();
                    reviewers.insert(reviewer);
                    entry.info.status = ProposalStatus::Approved {
                        reviewers,
                        approved_revision: current_rev,
                    };
                }
                ProposalStatus::Approved {
                    reviewers,
                    approved_revision,
                } => {
                    if reviewers.insert(reviewer) {
                        *approved_revision = current_rev;
                    }
                }
                ProposalStatus::Committed { .. } => {}
                ProposalStatus::Rejected { .. } => unreachable!("checked above"),
            }
            Ok(entry.info.clone())
        }

        /// Permanently rejects a pending or uncommitted proposal as an authorized human reviewer.
        pub fn reject_proposal(
            &self,
            request: RequestId,
            reviewer: ActorId,
            reason: Option<String>,
        ) -> Result<ProposalInfo, CoreError> {
            let mut guard = self.write_guard()?;
            let current = guard.current_snapshot()?;
            let entry = guard.proposals.get(&request).ok_or_else(|| {
                CoreError::new(
                    CoreErrorKind::MissingProposal,
                    format!("unknown proposal {}", request.raw()),
                )
            })?;
            match &entry.info.status {
                ProposalStatus::Committed { .. } => {
                    return Err(CoreError::new(
                        CoreErrorKind::ProposalAlreadyCommitted,
                        format!("proposal {} is already committed", request.raw()),
                    ));
                }
                ProposalStatus::Rejected { .. } => {
                    return Err(CoreError::new(
                        CoreErrorKind::ProposalRejected,
                        format!("proposal {} is already rejected", request.raw()),
                    ));
                }
                ProposalStatus::Pending | ProposalStatus::Approved { .. } => {}
            }
            let targets = entry.info.buffers.clone();
            // Follow Proposal::approve's authorization order: capability check, then human role check.
            guard
                .policy
                .authorize(
                    &guard.actors,
                    current.catalog(),
                    reviewer,
                    Capability::Review,
                    targets,
                )
                .map_err(|e| map_access_error(&e, &guard.actors))?;
            if !guard
                .actors
                .role_of(reviewer)
                .is_some_and(|role| role.is_human())
            {
                return Err(CoreError::new(
                    CoreErrorKind::NotHumanReviewer,
                    format!("actor {reviewer} is not a human reviewer"),
                ));
            }
            let entry = guard.proposals.get_mut(&request).expect("checked entry");
            entry.info.status = ProposalStatus::Rejected { reviewer, reason };
            Ok(entry.info.clone())
        }

        /// Commits a previously approved proposal atomically.
        pub fn commit_proposal(&self, request: RequestId) -> Result<CommitOutcome, CoreError> {
            let mut guard = self.write_guard()?;
            if let Some(entry) = guard.proposals.get(&request) {
                match entry.info.status {
                    ProposalStatus::Rejected { .. } => {
                        return Err(CoreError::new(
                            CoreErrorKind::ProposalRejected,
                            format!("proposal {} was rejected", request.raw()),
                        ));
                    }
                    ProposalStatus::Committed { receipt } => {
                        if let Some(&(_, existing)) = guard.request_outcomes.get(&request) {
                            return Ok(existing);
                        }
                        let group = guard
                            .group_plan
                            .groups()
                            .iter()
                            .copied()
                            .find(|g| g.id == receipt)
                            .expect("committed proposal has change group");
                        return Ok(CommitOutcome {
                            receipt,
                            buffer: None,
                            change_group: group,
                        });
                    }
                    ProposalStatus::Pending | ProposalStatus::Approved { .. } => {}
                }
            }
            let outcome: CheckpointOutcome = guard
                .coordinator
                .commit_proposal(request)
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            let receipt = outcome
                .receipt
                .expect("committed proposal produces receipt");
            let commit = guard.after_edit_commit(receipt, None)?;
            let digest = digest_committed_proposal(request, receipt);
            guard.request_outcomes.insert(request, (digest, commit));
            if let Some(entry) = guard.proposals.get_mut(&request) {
                entry.info.status = ProposalStatus::Committed { receipt };
            }
            Ok(commit)
        }

        /// Reads the observable state of a proposal, if present.
        pub fn proposal_info(&self, request: RequestId) -> Result<Option<ProposalInfo>, CoreError> {
            let guard = self.read_guard()?;
            Ok(guard.proposals.get(&request).map(|e| e.info.clone()))
        }

        // ------------------------------------------------------------------
        // Selective undo and redo across all three formats
        // ------------------------------------------------------------------

        /// Executes a frontend-typed undo or redo command.
        pub fn undo_redo(
            &self,
            request: RequestId,
            actor: ActorId,
            nonce: Nonce,
            command: FrontendCommand<'_>,
        ) -> Result<CommitOutcome, CoreError> {
            let mut guard = self.write_guard()?;
            let op_txn = TxnId::new(actor, nonce);
            let core_req = undo::normalize(actor, op_txn, command).map_err(map_undo_error)?;
            let digest = digest_undo_redo(request, core_req);
            if let Some(existing) = guard.check_request_retry(request, digest)? {
                return Ok(existing);
            }
            let coord_op = match core_req {
                CoreRequest::Undo(u) => CoordinatorOperation::Undo {
                    actor: u.actor,
                    target: u.target,
                    transaction: u.operation,
                },
                CoreRequest::Redo(r) => CoordinatorOperation::Redo {
                    actor: r.actor,
                    target: r.target,
                    transaction: r.operation,
                },
            };
            let outcome = guard
                .coordinator
                .apply(request, coord_op)
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            let receipt = outcome.receipt.expect("undo/redo produces receipt");
            let commit = guard.after_edit_commit(receipt, None)?;
            guard.request_outcomes.insert(request, (digest, commit));
            Ok(commit)
        }

        /// Executes an Emacs `undo-only` command.
        pub fn emacs_undo(
            &self,
            request: RequestId,
            actor: ActorId,
            nonce: Nonce,
            target: UndoTarget<'_>,
        ) -> Result<CommitOutcome, CoreError> {
            self.undo_redo(
                request,
                actor,
                nonce,
                FrontendCommand::EmacsUndoOnly(target),
            )
        }

        /// Executes an Emacs `undo-redo` command.
        pub fn emacs_redo(
            &self,
            request: RequestId,
            actor: ActorId,
            nonce: Nonce,
            target: UndoTarget<'_>,
        ) -> Result<CommitOutcome, CoreError> {
            self.undo_redo(
                request,
                actor,
                nonce,
                FrontendCommand::EmacsUndoRedo(target),
            )
        }

        /// Executes a Neovim `u` undo command.
        pub fn neovim_undo(
            &self,
            request: RequestId,
            actor: ActorId,
            nonce: Nonce,
            target: UndoTarget<'_>,
        ) -> Result<CommitOutcome, CoreError> {
            self.undo_redo(request, actor, nonce, FrontendCommand::NeovimUndo(target))
        }

        /// Executes a Neovim `Ctrl-r` redo command.
        pub fn neovim_redo(
            &self,
            request: RequestId,
            actor: ActorId,
            nonce: Nonce,
            target: UndoTarget<'_>,
        ) -> Result<CommitOutcome, CoreError> {
            self.undo_redo(request, actor, nonce, FrontendCommand::NeovimRedo(target))
        }

        /// Executes an Agent explicit-transaction undo command.
        pub fn agent_undo(
            &self,
            request: RequestId,
            actor: ActorId,
            nonce: Nonce,
            target: TxnId,
        ) -> Result<CommitOutcome, CoreError> {
            self.undo_redo(request, actor, nonce, FrontendCommand::AgentUndo(target))
        }

        /// Executes an Agent explicit-transaction redo command.
        pub fn agent_redo(
            &self,
            request: RequestId,
            actor: ActorId,
            nonce: Nonce,
            target: TxnId,
        ) -> Result<CommitOutcome, CoreError> {
            self.undo_redo(request, actor, nonce, FrontendCommand::AgentRedo(target))
        }

        // ------------------------------------------------------------------
        // Subscriptions, event delivery, compaction, and reconnect/resync
        // ------------------------------------------------------------------

        /// Subscribes an authorized `actor` to `buffer` in `format`, returning a client handle and initial snapshot.
        pub fn subscribe(
            &self,
            actor: ActorId,
            buffer: BufferId,
            format: HostFormat,
        ) -> Result<(ClientId, HostSnapshot), CoreError> {
            let mut guard = self.write_guard()?;
            let current = guard.current_snapshot()?;
            guard
                .policy
                .authorize(
                    &guard.actors,
                    current.catalog(),
                    actor,
                    Capability::Read,
                    [buffer],
                )
                .map_err(|e| map_access_error(&e, &guard.actors))?;
            let resync = Resync::new(&current, buffer).map_err(map_resync_error)?;
            let (_, projections) =
                Subscription::subscribe(&current, buffer).map_err(map_boundary_error)?;
            let initial = HostSnapshot::encode(format, &current, buffer)?;
            let id = ClientId::new(guard.next_client_id);
            guard.next_client_id = guard
                .next_client_id
                .checked_add(1)
                .ok_or_else(|| CoreError::new(CoreErrorKind::IdExhausted, "client id exhausted"))?;
            guard.clients.insert(
                id,
                ClientRecord {
                    actor,
                    buffer,
                    format,
                    track: ClientTrack::Live {
                        resync: Box::new(resync),
                        projections: Box::new(projections),
                    },
                },
            );
            Ok((id, initial))
        }

        /// Removes a subscribed client stream.
        pub fn unsubscribe(&self, client: ClientId) -> Result<(), CoreError> {
            let mut guard = self.write_guard()?;
            guard.clients.remove(&client).map(|_| ()).ok_or_else(|| {
                CoreError::new(
                    CoreErrorKind::UnknownClient,
                    format!("unknown client {client}"),
                )
            })
        }

        /// Delivers incremental deltas to a subscribed client.
        ///
        /// If the client's observed revision has fallen behind the compaction or
        /// delivery floor, returns [`CoreErrorKind::ResyncRequired`] without
        /// modifying client state so the caller can invoke [`EditorCore::reconnect`].
        pub fn deliver_incremental(
            &self,
            client: ClientId,
        ) -> Result<IncrementalDelivery, CoreError> {
            let mut guard = self.write_guard()?;
            let current = guard.current_snapshot()?;
            let (actor, buffer, format, client_rev, is_restored_only) = {
                let rec = guard.clients.get(&client).ok_or_else(|| {
                    CoreError::new(
                        CoreErrorKind::UnknownClient,
                        format!("unknown client {client}"),
                    )
                })?;
                (
                    rec.actor,
                    rec.buffer,
                    rec.format,
                    rec.revision(),
                    matches!(rec.track, ClientTrack::Restored { .. }),
                )
            };
            guard.authorize_client(&current, actor, buffer)?;
            if is_restored_only {
                return Err(CoreError::new(
                    CoreErrorKind::ResyncRequired,
                    "client restored from checkpoint requires snapshot reconnect",
                ));
            }
            let catch_up = guard
                .coordinator
                .catch_up(client_rev)
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            match catch_up {
                CatchUp::Snapshot(_) => Err(CoreError::new(
                    CoreErrorKind::ResyncRequired,
                    format!(
                        "client revision {client_rev} is below delivery floor; reconnect required"
                    ),
                )),
                CatchUp::Events(events) => {
                    let rec = guard.clients.get_mut(&client).expect("checked client");
                    let ClientTrack::Live {
                        resync,
                        projections,
                    } = &mut rec.track
                    else {
                        unreachable!("checked live track");
                    };
                    let notifications =
                        apply_live_events(resync, projections, buffer, format, &events)?;
                    let final_rev = resync.revision();
                    let closed = resync.is_closed();
                    Ok(IncrementalDelivery {
                        client,
                        buffer,
                        previous_revision: client_rev,
                        revision: final_rev,
                        notifications,
                        closed,
                    })
                }
            }
        }

        /// Reconnects and resynchronizes a subscribed client via incremental deltas when
        /// retained, or via full snapshot (or closed status) after compaction or lifecycle transitions.
        pub fn reconnect(&self, client: ClientId) -> Result<ReconnectOutcome, CoreError> {
            let mut guard = self.write_guard()?;
            let current = guard.current_snapshot()?;
            let (actor, buffer, format, client_rev, is_restored_only) = {
                let rec = guard.clients.get(&client).ok_or_else(|| {
                    CoreError::new(
                        CoreErrorKind::UnknownClient,
                        format!("unknown client {client}"),
                    )
                })?;
                (
                    rec.actor,
                    rec.buffer,
                    rec.format,
                    rec.revision(),
                    matches!(rec.track, ClientTrack::Restored { .. }),
                )
            };
            guard.authorize_client(&current, actor, buffer)?;

            if is_restored_only {
                let rec = guard.clients.get_mut(&client).expect("checked client");
                if current.catalog().is_tombstoned(buffer) {
                    rec.track = ClientTrack::Restored {
                        revision: current.revision(),
                        closed: true,
                    };
                    return Ok(ReconnectOutcome::Closed {
                        client,
                        buffer,
                        previous_revision: client_rev,
                        revision: current.revision(),
                    });
                }
                let resync = Resync::new(&current, buffer).map_err(map_resync_error)?;
                let (_, projections) =
                    Subscription::subscribe(&current, buffer).map_err(map_boundary_error)?;
                let snapshot = HostSnapshot::encode(format, &current, buffer)?;
                rec.track = ClientTrack::Live {
                    resync: Box::new(resync),
                    projections: Box::new(projections),
                };
                return Ok(ReconnectOutcome::Snapshot {
                    client,
                    buffer,
                    previous_revision: client_rev,
                    revision: current.revision(),
                    snapshot,
                });
            }

            let catch_up = guard
                .coordinator
                .catch_up(client_rev)
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            match catch_up {
                CatchUp::Events(events) => {
                    let rec = guard.clients.get_mut(&client).expect("checked client");
                    let ClientTrack::Live {
                        resync,
                        projections,
                    } = &mut rec.track
                    else {
                        unreachable!("checked live track");
                    };
                    let notifications =
                        apply_live_events(resync, projections, buffer, format, &events)?;
                    let final_rev = resync.revision();
                    let closed = resync.is_closed();
                    if closed && notifications.is_empty() {
                        Ok(ReconnectOutcome::Closed {
                            client,
                            buffer,
                            previous_revision: client_rev,
                            revision: final_rev,
                        })
                    } else {
                        Ok(ReconnectOutcome::Deltas(IncrementalDelivery {
                            client,
                            buffer,
                            previous_revision: client_rev,
                            revision: final_rev,
                            notifications,
                            closed,
                        }))
                    }
                }
                CatchUp::Snapshot(tip) => {
                    let rec = guard.clients.get_mut(&client).expect("checked client");
                    let ClientTrack::Live {
                        resync,
                        projections,
                    } = &mut rec.track
                    else {
                        unreachable!("checked live track");
                    };
                    let mut cand_resync = resync.clone();
                    let mut cand_proj = projections.clone();
                    cand_resync
                        .catch_up_projected(&CatchUp::Snapshot(tip.clone()), &mut cand_proj)
                        .map_err(map_resync_error)?;
                    let closed = cand_resync.is_closed();
                    let final_rev = cand_resync.revision();
                    if closed {
                        *resync = cand_resync;
                        *projections = cand_proj;
                        Ok(ReconnectOutcome::Closed {
                            client,
                            buffer,
                            previous_revision: client_rev,
                            revision: final_rev,
                        })
                    } else {
                        let snapshot = HostSnapshot::encode(format, &tip, buffer)?;
                        *resync = cand_resync;
                        *projections = cand_proj;
                        Ok(ReconnectOutcome::Snapshot {
                            client,
                            buffer,
                            previous_revision: client_rev,
                            revision: final_rev,
                            snapshot,
                        })
                    }
                }
            }
        }

        /// Returns the client's currently projected host snapshot (checking current authorization).
        pub fn client_snapshot(&self, client: ClientId) -> Result<HostSnapshot, CoreError> {
            let guard = self.read_guard()?;
            let current = guard.current_snapshot()?;
            let rec = guard.clients.get(&client).ok_or_else(|| {
                CoreError::new(
                    CoreErrorKind::UnknownClient,
                    format!("unknown client {client}"),
                )
            })?;
            guard.authorize_client(&current, rec.actor, rec.buffer)?;
            if rec.is_closed() {
                return Err(CoreError::new(
                    CoreErrorKind::BufferClosed,
                    format!("subscribed buffer {} is closed", rec.buffer),
                ));
            }
            match &rec.track {
                ClientTrack::Live { projections, .. } => match rec.format {
                    HostFormat::Emacs => projections
                        .emacs()
                        .cloned()
                        .map(HostSnapshot::Emacs)
                        .map_err(map_boundary_error),
                    HostFormat::Neovim => projections
                        .neovim()
                        .cloned()
                        .map(HostSnapshot::Neovim)
                        .map_err(map_boundary_error),
                    HostFormat::Agent(_) => projections
                        .agent()
                        .cloned()
                        .map(HostSnapshot::Agent)
                        .map_err(map_boundary_error),
                },
                ClientTrack::Restored { .. } => Err(CoreError::new(
                    CoreErrorKind::ResyncRequired,
                    "restored client must reconnect before reading projected snapshot",
                )),
            }
        }

        /// Returns the client's currently observed revision (checking current authorization).
        pub fn client_revision(&self, client: ClientId) -> Result<Revision, CoreError> {
            let guard = self.read_guard()?;
            let current = guard.current_snapshot()?;
            let rec = guard.clients.get(&client).ok_or_else(|| {
                CoreError::new(
                    CoreErrorKind::UnknownClient,
                    format!("unknown client {client}"),
                )
            })?;
            guard.authorize_client(&current, rec.actor, rec.buffer)?;
            Ok(rec.revision())
        }

        /// Validates raw UTF-8 bytes at the public editing boundary.
        pub fn validate_utf8(bytes: &[u8]) -> Result<Text, CoreError> {
            Text::from_bytes(bytes).map_err(map_utf8_error)
        }

        /// Recovers a buffer snapshot from a retained observed revision via the request boundary.
        pub fn recover_buffer(
            &self,
            actor: ActorId,
            buffer: BufferId,
            observed_revision: Revision,
            format: HostFormat,
        ) -> Result<Option<HostSnapshot>, CoreError> {
            let guard = self.read_guard()?;
            guard.ensure_actor_capability(actor, Capability::Read, [buffer])?;
            let current = guard.current_snapshot()?;
            let observed = guard.snapshot_for_base(observed_revision)?;
            let envelope = Envelope::BufferOperation(Input {
                actor: Some(actor),
                revision: Some(observed_revision),
                target: Some(Target::Buffer(buffer)),
                operation: InputOperation::Resync,
            });
            let request = envelope.normalize(&observed).map_err(map_boundary_error)?;
            match request.recover(&guard.policy, &guard.actors, &observed, &current) {
                Ok(crate::editor::interop::requests::Recovery::Live(_)) => {
                    HostSnapshot::encode(format, &current, buffer).map(Some)
                }
                Ok(crate::editor::interop::requests::Recovery::Closed { .. }) => Ok(None),
                // Retained pre-restore snapshots below `delivery_floor` originate from the same
                // checkpointed workspace before restore created a new origin token.
                Err(RequestError::DifferentWorkspace) if observed_revision < current.revision() => {
                    if current.catalog().is_tombstoned(buffer) {
                        Ok(None)
                    } else {
                        HostSnapshot::encode(format, &current, buffer).map(Some)
                    }
                }
                Err(e) => Err(map_request_error(&e, &guard.actors)),
            }
        }

        /// Advances the retention/compaction floor and drops superseded snapshots below it.
        pub fn compact(&self, floor: Revision) -> Result<Revision, CoreError> {
            let mut guard = self.write_guard()?;
            guard
                .coordinator
                .compact(floor)
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            let new_floor = guard
                .coordinator
                .floor()
                .map_err(|e| map_concurrent_error(&e, &guard.actors))?;
            guard.retained_snapshots.retain(|&rev, _| rev >= new_floor);
            Ok(new_floor)
        }

        // ------------------------------------------------------------------
        // Lock guards
        // ------------------------------------------------------------------

        fn read_guard(&self) -> Result<std::sync::RwLockReadGuard<'_, CoreState>, CoreError> {
            self.state.read().map_err(|_| {
                CoreError::new(CoreErrorKind::LockPoisoned, "editor core lock poisoned")
            })
        }

        fn write_guard(&self) -> Result<std::sync::RwLockWriteGuard<'_, CoreState>, CoreError> {
            self.state.write().map_err(|_| {
                CoreError::new(CoreErrorKind::LockPoisoned, "editor core lock poisoned")
            })
        }
    }

    impl CoreState {
        fn current_snapshot(&self) -> Result<WorkspaceSnapshot, CoreError> {
            self.coordinator
                .snapshot()
                .map_err(|e| map_concurrent_error(&e, &self.actors))
        }

        fn ensure_actor_active(&self, actor: ActorId) -> Result<(), CoreError> {
            if self.actors.is_active(actor) {
                Ok(())
            } else if self.actors.is_revoked(actor) {
                Err(CoreError::new(
                    CoreErrorKind::RevokedActor,
                    format!("actor {actor} is revoked"),
                ))
            } else {
                Err(CoreError::new(
                    CoreErrorKind::UnknownActor,
                    format!("unknown actor {actor}"),
                ))
            }
        }

        fn ensure_actor_capability(
            &self,
            actor: ActorId,
            capability: Capability,
            buffers: impl IntoIterator<Item = BufferId>,
        ) -> Result<(), CoreError> {
            self.ensure_actor_active(actor)?;
            for buffer in buffers.into_iter().collect::<BTreeSet<_>>() {
                if !self.policy.is_granted(actor, buffer, capability) {
                    return Err(CoreError::new(
                        CoreErrorKind::PermissionDenied,
                        format!("actor {actor} lacks {capability:?} for buffer {buffer}"),
                    ));
                }
            }
            Ok(())
        }

        fn authorize_client(
            &self,
            current: &WorkspaceSnapshot,
            actor: ActorId,
            buffer: BufferId,
        ) -> Result<(), CoreError> {
            self.ensure_actor_active(actor)?;
            if current.catalog().contains_live(buffer) {
                self.policy
                    .authorize(
                        &self.actors,
                        current.catalog(),
                        actor,
                        Capability::Read,
                        [buffer],
                    )
                    .map_err(|e| map_access_error(&e, &self.actors))
            } else if current.catalog().is_tombstoned(buffer) {
                if !self.policy.is_granted(actor, buffer, Capability::Read) {
                    return Err(CoreError::new(
                        CoreErrorKind::PermissionDenied,
                        format!("actor {actor} lacks Read for closed buffer {buffer}"),
                    ));
                }
                Ok(())
            } else {
                Err(CoreError::new(
                    CoreErrorKind::UnknownBuffer,
                    format!("unknown buffer {buffer}"),
                ))
            }
        }

        fn authorized_text_and_view<'a>(
            &self,
            current: &'a WorkspaceSnapshot,
            actor: ActorId,
            buffer: BufferId,
        ) -> Result<(&'a Text, BufferView), CoreError> {
            self.policy
                .authorize(
                    &self.actors,
                    current.catalog(),
                    actor,
                    Capability::Read,
                    [buffer],
                )
                .map_err(|e| map_access_error(&e, &self.actors))?;
            let view = match self
                .session
                .view(actor, buffer)
                .map_err(|e| map_session_error(&e, &self.actors))?
            {
                Some(view) => view,
                None => BufferView::full(current.catalog(), buffer).map_err(map_view_error)?,
            };
            let text = current
                .catalog()
                .get(buffer)
                .expect("authorized live buffer");
            Ok((text, view))
        }

        fn snapshot_for_base(&self, base: Revision) -> Result<WorkspaceSnapshot, CoreError> {
            let current = self.current_snapshot()?;
            let floor = self
                .coordinator
                .floor()
                .map_err(|e| map_concurrent_error(&e, &self.actors))?;
            if base > current.revision() {
                return Err(CoreError::new(
                    CoreErrorKind::FutureRevision,
                    format!("base revision {base} exceeds tip {}", current.revision()),
                ));
            }
            if base < floor {
                return Err(CoreError::new(
                    CoreErrorKind::BaseExpired,
                    format!("base revision {base} is below compaction floor {floor}"),
                ));
            }
            self.retained_snapshots.get(&base).cloned().ok_or_else(|| {
                CoreError::new(
                    CoreErrorKind::BaseExpired,
                    format!("snapshot for base revision {base} is no longer retained"),
                )
            })
        }

        fn check_request_retry(
            &self,
            request: RequestId,
            digest: [u8; 32],
        ) -> Result<Option<CommitOutcome>, CoreError> {
            if self.proposals.contains_key(&request) {
                return Err(CoreError::new(
                    CoreErrorKind::RequestReuse,
                    format!("request {} is reserved by a proposal", request.raw()),
                ));
            }
            if let Some(&(old_digest, existing)) = self.request_outcomes.get(&request) {
                if old_digest != digest {
                    return Err(CoreError::new(
                        CoreErrorKind::RequestReuse,
                        format!("request {} reused with different payload", request.raw()),
                    ));
                }
                return Ok(Some(existing));
            }
            Ok(None)
        }

        fn check_sequence_step_available(
            &self,
            sequence_step: Option<SequenceStep>,
        ) -> Result<(), CoreError> {
            if let Some(step) = sequence_step {
                if self
                    .group_plan
                    .groups()
                    .iter()
                    .any(|g| g.sequence == Some(step))
                {
                    return Err(map_group_error(GroupError::StepConflict(step)));
                }
            }
            Ok(())
        }

        fn commit_edit_transaction(
            &mut self,
            request: RequestId,
            digest: [u8; 32],
            txn: Transaction,
            sequence_step: Option<SequenceStep>,
        ) -> Result<CommitOutcome, CoreError> {
            self.check_sequence_step_available(sequence_step)?;
            let outcome = self
                .coordinator
                .apply(request, CoordinatorOperation::Edit(txn))
                .map_err(|e| map_concurrent_error(&e, &self.actors))?;
            let receipt = outcome.receipt.expect("edit produces receipt");
            let commit = self.after_edit_commit(receipt, sequence_step)?;
            self.request_outcomes.insert(request, (digest, commit));
            Ok(commit)
        }

        fn after_edit_commit(
            &mut self,
            receipt: CommitReceipt,
            sequence_step: Option<SequenceStep>,
        ) -> Result<CommitOutcome, CoreError> {
            let after = self.current_snapshot()?;
            if receipt.committed() > self.session.revision() {
                let catch_up = self
                    .coordinator
                    .catch_up(receipt.previous())
                    .map_err(|e| map_concurrent_error(&e, &self.actors))?;
                let CatchUp::Events(events) = catch_up else {
                    return Err(CoreError::new(
                        CoreErrorKind::InvalidEvent,
                        "expected incremental delivery event after edit commit",
                    ));
                };
                let event = events.last().ok_or_else(|| {
                    CoreError::new(
                        CoreErrorKind::InvalidEvent,
                        "missing delivery event for committed edit",
                    )
                })?;
                self.session
                    .advance(event)
                    .map_err(|e| map_session_error(&e, &self.actors))?;
                self.retained_snapshots
                    .insert(after.revision(), after.clone());
                self.prune_evicted_annotation_keys();
            }
            let group = self
                .group_plan
                .project(SubmitOutcome::Committed(receipt), sequence_step)
                .map_err(map_group_error)?
                .expect("committed outcome yields change group");
            Ok(CommitOutcome {
                receipt,
                buffer: None,
                change_group: group,
            })
        }

        fn record_open_commit(
            &mut self,
            request: RequestId,
            digest: [u8; 32],
            receipt: CommitReceipt,
            buffer: BufferId,
        ) -> Result<CommitOutcome, CoreError> {
            let after = self.current_snapshot()?;
            if receipt.committed() > self.session.revision() {
                self.session
                    .open(after.clone(), buffer)
                    .map_err(|e| map_session_error(&e, &self.actors))?;
                self.retained_snapshots.insert(after.revision(), after);
            }
            let group = self
                .group_plan
                .project(SubmitOutcome::Committed(receipt), None)
                .map_err(map_group_error)?
                .expect("open produces change group");
            let commit = CommitOutcome {
                receipt,
                buffer: Some(buffer),
                change_group: group,
            };
            self.request_outcomes.insert(request, (digest, commit));
            Ok(commit)
        }

        fn record_close_commit(
            &mut self,
            request: RequestId,
            digest: [u8; 32],
            receipt: CommitReceipt,
            buffer: BufferId,
        ) -> Result<CommitOutcome, CoreError> {
            let after = self.current_snapshot()?;
            if receipt.committed() > self.session.revision() {
                self.session
                    .close(after.clone(), buffer)
                    .map_err(|e| map_session_error(&e, &self.actors))?;
                self.retained_snapshots.insert(after.revision(), after);
                self.annotation_keys.retain(|(_, key)| key.buffer != buffer);
            }
            let group = self
                .group_plan
                .project(SubmitOutcome::Committed(receipt), None)
                .map_err(map_group_error)?
                .expect("close produces change group");
            let commit = CommitOutcome {
                receipt,
                buffer: Some(buffer),
                change_group: group,
            };
            self.request_outcomes.insert(request, (digest, commit));
            Ok(commit)
        }

        fn rebuild_session_for_registry(&mut self) -> Result<(), CoreError> {
            let snapshot = self.current_snapshot()?;
            let mut next =
                WorkspaceSession::new(snapshot.clone(), self.actors.clone(), self.policy.clone());
            let live = snapshot.catalog().live_ids();
            for (actor, _) in self.actors.active_actors() {
                for &buffer in &live {
                    if let Ok(Some(view)) = self.session.view(actor, buffer) {
                        let _ = next.set_view(actor, view);
                    }
                    if let Ok(Some(sel)) = self.session.selection(actor, buffer) {
                        let _ = next.set_selection(actor, sel);
                    }
                }
            }
            let mut kept_keys = BTreeSet::new();
            for &(actor, key) in &self.annotation_keys {
                if self.actors.is_active(actor) {
                    if let Ok(Some(ann)) = self.session.overlay(actor, key) {
                        if next.set_overlay(actor, ann.clone()).is_ok() {
                            kept_keys.insert((actor, key));
                        }
                    }
                }
            }
            self.annotation_keys = kept_keys;
            self.session = next;
            Ok(())
        }

        fn prune_inaccessible_annotation_keys(&mut self) {
            let session = &self.session;
            self.annotation_keys
                .retain(|&(actor, key)| matches!(session.overlay(actor, key), Ok(Some(_))));
        }

        fn prune_evicted_annotation_keys(&mut self) {
            let session = &self.session;
            let actors = &self.actors;
            self.annotation_keys.retain(|&(actor, key)| {
                actors.is_active(actor) && matches!(session.overlay(actor, key), Ok(Some(_)))
            });
        }
    }

    // ----------------------------------------------------------------------
    // Pure compilation, projection, digest, and error-mapping helpers
    // ----------------------------------------------------------------------

    fn apply_live_events(
        resync: &mut Resync,
        projections: &mut InitialSnapshots,
        buffer: BufferId,
        format: HostFormat,
        events: &[DeliveryEvent],
    ) -> Result<Vec<HostNotification>, CoreError> {
        let mut cand_resync = resync.clone();
        let mut cand_proj = projections.clone();
        let mut notifications = Vec::new();
        for event in events {
            let was_closed = cand_resync.is_closed();
            let prev = cand_resync.revision();
            let step = cand_resync
                .catch_up_projected(&CatchUp::Events(vec![event.clone()]), &mut cand_proj)
                .map_err(map_resync_error)?;
            let next = cand_resync.revision();
            if next > prev && !was_closed {
                if step.is_empty() {
                    notifications.push(HostNotification::RevisionAdvanced {
                        buffer,
                        previous_revision: prev,
                        revision: next,
                    });
                } else {
                    for note in step {
                        notifications.push(match note {
                            Notification::Change(formats) => {
                                HostNotification::Change(HostDelta::from_formats(format, formats))
                            }
                            Notification::Closed { buffer, revision } => {
                                HostNotification::Closed { buffer, revision }
                            }
                        });
                    }
                }
            }
        }
        *resync = cand_resync;
        *projections = cand_proj;
        Ok(notifications)
    }

    fn command_capabilities(commands: &HostCommands) -> (bool, bool) {
        let (mut has_edit, mut has_select) = (false, false);
        match commands {
            HostCommands::Emacs(cmds) => {
                for cmd in cmds {
                    match cmd {
                        EmacsCommand::Insert(_)
                        | EmacsCommand::ReplaceRegion(_)
                        | EmacsCommand::BeginChangeGroup
                        | EmacsCommand::EndChangeGroup => has_edit = true,
                        EmacsCommand::SelectRegion => has_select = true,
                        EmacsCommand::Point(_)
                        | EmacsCommand::Mark(_)
                        | EmacsCommand::Unsupported(_) => {}
                    }
                }
            }
            HostCommands::Neovim(cmds) => {
                for cmd in cmds {
                    match cmd {
                        NeovimCommand::Insert(_)
                        | NeovimCommand::SetText { .. }
                        | NeovimCommand::BeginUndoBlock
                        | NeovimCommand::EndUndoBlock => has_edit = true,
                        NeovimCommand::Select { .. } => has_select = true,
                        NeovimCommand::Cursor(_) | NeovimCommand::Unsupported(_) => {}
                    }
                }
            }
            HostCommands::Agent(cmds) => {
                for cmd in cmds {
                    match cmd {
                        AgentCommand::Replace { .. }
                        | AgentCommand::BeginBatch
                        | AgentCommand::EndBatch => has_edit = true,
                        AgentCommand::Select { .. } => has_select = true,
                        AgentCommand::Unsupported(_) => {}
                    }
                }
            }
        }
        (has_edit, has_select || !has_edit)
    }

    fn proposal_target_buffers(payload: &ProposalPayload) -> BTreeSet<BufferId> {
        let mut targets = BTreeSet::new();
        match payload {
            ProposalPayload::Edit(edit) => {
                targets.insert(edit.buffer);
            }
            ProposalPayload::MultiBatch { primary, edits } => {
                targets.insert(*primary);
                for edit in edits {
                    targets.insert(edit.buffer);
                }
            }
            ProposalPayload::Commands { buffer, .. }
            | ProposalPayload::Rectangle { buffer, .. }
            | ProposalPayload::Indent { buffer, .. } => {
                targets.insert(*buffer);
            }
        }
        targets
    }

    fn apply_access_change(change: AccessChange, policy: &mut AccessPolicy) {
        match change {
            AccessChange::GrantDefault(cap) => policy.grant_default(cap),
            AccessChange::RevokeDefault(cap) => policy.revoke_default(cap),
            AccessChange::GrantActor(actor, cap) => policy.grant_actor(actor, cap),
            AccessChange::RevokeActor(actor, cap) => policy.revoke_actor(actor, cap),
            AccessChange::GrantBuffer(actor, buf, cap) => policy.grant_buffer(actor, buf, cap),
            AccessChange::RevokeBuffer(actor, buf, cap) => policy.revoke_buffer(actor, buf, cap),
        }
    }

    fn host_position_to_byte(text: &Text, position: HostPosition) -> Result<ByteOffset, CoreError> {
        match position {
            HostPosition::Emacs(pos) => {
                emacs_position::emacs_to_byte(text, pos).map_err(map_boundary_error)
            }
            HostPosition::Neovim(pos) => {
                neovim_position::line_col_to_byte(text, pos).map_err(map_boundary_error)
            }
            HostPosition::Agent(pos) => {
                agent_position::position_to_byte(text, pos).map_err(map_boundary_error)
            }
        }
    }

    fn byte_to_host_position(
        text: &Text,
        offset: ByteOffset,
        format: HostFormat,
    ) -> Result<HostPosition, CoreError> {
        match format {
            HostFormat::Emacs => emacs_position::byte_to_emacs(text, offset)
                .map(HostPosition::Emacs)
                .map_err(map_boundary_error),
            HostFormat::Neovim => neovim_position::byte_to_line_col(text, offset)
                .map(HostPosition::Neovim)
                .map_err(map_boundary_error),
            HostFormat::Agent(kind) => {
                agent_position::byte_to_agent_position(text, offset, kind.to_coord_kind())
                    .map(HostPosition::Agent)
                    .map_err(map_boundary_error)
            }
        }
    }

    fn encode_host_view(
        text: &Text,
        revision: Revision,
        view: BufferView,
        format: HostFormat,
    ) -> Result<HostView, CoreError> {
        let span = view.visible();
        let start = byte_to_host_position(text, span.start(), format)?;
        let end = byte_to_host_position(text, span.end(), format)?;
        Ok(HostView {
            buffer: view.buffer(),
            revision,
            start,
            end,
            start_byte: span.start().as_usize(),
            end_byte: span.end().as_usize(),
        })
    }

    fn encode_host_selection(
        catalog: &crate::editor::workspace::catalog::BufferCatalog,
        view: &BufferView,
        revision: Revision,
        selection: Selection,
        format: HostFormat,
    ) -> Result<HostSelection, CoreError> {
        let region = OrientedRegion::from_selection(
            catalog,
            view,
            selection,
            format.coord_kind(),
            EndpointMode::Exclusive,
        )
        .map_err(map_boundary_error)?;
        Ok(HostSelection {
            buffer: selection.buffer(),
            revision,
            region,
            anchor_byte: selection.anchor().as_usize(),
            head_byte: selection.head().as_usize(),
            direction: selection.direction(),
        })
    }

    fn project_host_annotation(
        text: &Text,
        ann: &Annotation,
        format: HostFormat,
    ) -> Result<HostAnnotation, CoreError> {
        match format {
            HostFormat::Emacs => annotations::project_emacs(text, ann)
                .map(HostAnnotation::Emacs)
                .map_err(map_boundary_error),
            HostFormat::Neovim => annotations::project_neovim(text, ann)
                .map(HostAnnotation::Neovim)
                .map_err(map_boundary_error),
            HostFormat::Agent(kind) => annotations::project_agent(text, ann, kind.to_coord_kind())
                .map(HostAnnotation::Agent)
                .map_err(map_boundary_error),
        }
    }

    fn project_search_hits(
        snapshot: &WorkspaceSnapshot,
        matches: &[search::WorkspaceMatch],
        format: HostFormat,
    ) -> Result<Vec<HostSearchHit>, CoreError> {
        let mut hits = Vec::with_capacity(matches.len());
        for found in matches {
            let text = snapshot
                .catalog()
                .get(found.buffer)
                .expect("matched buffer is live");
            let start = byte_to_host_position(text, found.span.start(), format)?;
            let end = byte_to_host_position(text, found.span.end(), format)?;
            hits.push(HostSearchHit {
                revision: found.revision,
                buffer: found.buffer,
                start,
                end,
                start_byte: found.span.start().as_usize(),
                end_byte: found.span.end().as_usize(),
            });
        }
        Ok(hits)
    }

    fn compile_single_edit(
        base_snapshot: &WorkspaceSnapshot,
        actor: ActorId,
        nonce: Nonce,
        base: Revision,
        edit: &EditSpec,
    ) -> Result<Transaction, CoreError> {
        let envelope = Envelope::Command(Input {
            actor: Some(actor),
            revision: Some(base),
            target: Some(Target::Buffer(edit.buffer)),
            operation: InputOperation::Edit(Replacement {
                region: edit.region,
                text: edit.replacement.clone(),
            }),
        });
        let request = envelope
            .normalize(base_snapshot)
            .map_err(map_boundary_error)?;
        let (_, crate::editor::collab::lifecycle::Operation::Edit(txn)) =
            request.lifecycle_operation(nonce).ok_or_else(|| {
                CoreError::new(
                    CoreErrorKind::CorruptSequence,
                    "failed to compile edit transaction",
                )
            })?
        else {
            return Err(CoreError::new(
                CoreErrorKind::CorruptSequence,
                "expected edit operation",
            ));
        };
        Ok(txn)
    }

    fn compile_multi_batch(
        base_snapshot: &WorkspaceSnapshot,
        actor: ActorId,
        nonce: Nonce,
        base: Revision,
        primary: BufferId,
        edits: &[EditSpec],
    ) -> Result<Transaction, CoreError> {
        let batch_edits: Vec<BatchEdit> = edits
            .iter()
            .map(|e| BatchEdit {
                buffer: e.buffer,
                replacement: Replacement {
                    region: e.region,
                    text: e.replacement.clone(),
                },
            })
            .collect();
        let envelope = Envelope::BufferOperation(Input {
            actor: Some(actor),
            revision: Some(base),
            target: Some(Target::Buffer(primary)),
            operation: InputOperation::MultiBatch(batch_edits),
        });
        let request = envelope
            .normalize(base_snapshot)
            .map_err(map_boundary_error)?;
        let (_, crate::editor::collab::lifecycle::Operation::Edit(txn)) =
            request.lifecycle_operation(nonce).ok_or_else(|| {
                CoreError::new(
                    CoreErrorKind::CorruptSequence,
                    "failed to compile multi-buffer batch transaction",
                )
            })?
        else {
            return Err(CoreError::new(
                CoreErrorKind::CorruptSequence,
                "expected edit operation",
            ));
        };
        Ok(txn)
    }

    fn lower_host_commands(
        base_snapshot: &WorkspaceSnapshot,
        actor: ActorId,
        buffer: BufferId,
        commands: &HostCommands,
    ) -> Result<Vec<Request>, CoreError> {
        let ctx = CommandContext { actor, buffer };
        match commands {
            HostCommands::Emacs(cmds) => {
                commands::lower_emacs(base_snapshot, ctx, cmds).map_err(map_boundary_error)
            }
            HostCommands::Neovim(cmds) => {
                commands::lower_neovim(base_snapshot, ctx, cmds).map_err(map_boundary_error)
            }
            HostCommands::Agent(cmds) => {
                commands::lower_agent(base_snapshot, ctx, cmds).map_err(map_boundary_error)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn compile_rectangle_edit(
        base_snapshot: &WorkspaceSnapshot,
        actor: ActorId,
        nonce: Nonce,
        base: Revision,
        buffer: BufferId,
        spec: RectangleSpec,
        policy: ShortLinePolicy,
        operation: &RectangleEditOp,
    ) -> Result<Transaction, CoreError> {
        let text = base_snapshot.catalog().get(buffer).ok_or_else(|| {
            if base_snapshot.catalog().is_tombstoned(buffer) {
                CoreError::new(
                    CoreErrorKind::BufferClosed,
                    format!("buffer {buffer} is closed"),
                )
            } else {
                CoreError::new(
                    CoreErrorKind::UnknownBuffer,
                    format!("unknown buffer {buffer}"),
                )
            }
        })?;
        let rectangle = match spec {
            RectangleSpec::Visual(rect) => rect,
            RectangleSpec::Positions {
                first,
                second,
                endpoint,
                tab_width,
            } => {
                let first_off = host_position_to_byte(text, first)?;
                let second_off = host_position_to_byte(text, second)?;
                let first_vis = rectangle::position_at_offset(text, first_off, tab_width)
                    .map_err(map_column_error)?;
                let second_vis = rectangle::position_at_offset(text, second_off, tab_width)
                    .map_err(map_column_error)?;
                Rectangle {
                    first: first_vis,
                    second: second_vis,
                    endpoint,
                    tab_width,
                }
            }
        };
        let intent = EditIntent::for_actor(actor, nonce, base);
        let row_refs: Vec<&str> = match operation {
            RectangleEditOp::Replace(rows) => rows.iter().map(String::as_str).collect(),
            _ => Vec::new(),
        };
        let op_ref = match operation {
            RectangleEditOp::Delete => RectangleOperation::Delete,
            RectangleEditOp::Replace(_) => RectangleOperation::Replace(&row_refs),
            RectangleEditOp::Uniform(value) => RectangleOperation::Uniform(value.as_str()),
            RectangleEditOp::Insert(value) => RectangleOperation::Insert(value.as_str()),
        };
        rectangle_edit::plan_transaction(base_snapshot, buffer, rectangle, policy, op_ref, intent)
            .map_err(map_rectangle_error)
    }

    fn compile_indent_region(
        base_snapshot: &WorkspaceSnapshot,
        actor: ActorId,
        nonce: Nonce,
        base: Revision,
        buffer: BufferId,
        spec: IndentSpec,
    ) -> Result<Transaction, CoreError> {
        let text = base_snapshot.catalog().get(buffer).ok_or_else(|| {
            if base_snapshot.catalog().is_tombstoned(buffer) {
                CoreError::new(
                    CoreErrorKind::BufferClosed,
                    format!("buffer {buffer} is closed"),
                )
            } else {
                CoreError::new(
                    CoreErrorKind::UnknownBuffer,
                    format!("unknown buffer {buffer}"),
                )
            }
        })?;
        let region = spec.start_line..spec.end_line;
        let patch = match spec.action {
            IndentAction::SetColumn(column) => indent_region::plan(
                text,
                region,
                IndentPolicy {
                    column,
                    tab_width: spec.tab_width,
                    style: spec.style,
                },
            )
            .map_err(map_indent_error)?,
            IndentAction::IndentBy(delta) | IndentAction::DedentBy(delta) => {
                // Validate line range and nonzero tab width via indent_region first.
                let _ = indent_region::plan(
                    text,
                    region.clone(),
                    IndentPolicy {
                        column: 0,
                        tab_width: spec.tab_width,
                        style: spec.style,
                    },
                )
                .map_err(map_indent_error)?;
                let index = LineIndex::from_text(text);
                let mut splices = Vec::new();
                for line in region.clone() {
                    let cur = indent::leading_indentation(text, &index, line, spec.tab_width)
                        .map_err(|e| map_indent_error(IndentRegionError::Indent(e)))?;
                    let target_col = match spec.action {
                        IndentAction::IndentBy(_) => {
                            cur.column.checked_add(delta).ok_or_else(|| {
                                map_indent_error(IndentRegionError::Indent(
                                    IndentError::ColumnOverflow,
                                ))
                            })?
                        }
                        IndentAction::DedentBy(_) => cur.column.saturating_sub(delta),
                        IndentAction::SetColumn(_) => unreachable!(),
                    };
                    let line_patch = indent_region::plan(
                        text,
                        line..line + 1,
                        IndentPolicy {
                            column: target_col,
                            tab_width: spec.tab_width,
                            style: spec.style,
                        },
                    )
                    .map_err(map_indent_error)?;
                    splices.extend(line_patch.splices().iter().cloned());
                }
                let patch = Patch::new(splices)
                    .map_err(|e| map_indent_error(IndentRegionError::Patch(e)))?;
                indent_region::validate_patch(text, region, &patch).map_err(map_indent_error)?;
                patch
            }
        };
        let intent = EditIntent::for_actor(actor, nonce, base);
        Transaction::new(intent, vec![(buffer, patch)]).map_err(map_transaction_error)
    }

    // ----------------------------------------------------------------------
    // Domain-separated request digests (zero superseded text retained)
    // ----------------------------------------------------------------------

    fn put_u64(buf: &mut Vec<u8>, v: u64) {
        buf.extend_from_slice(&v.to_be_bytes());
    }
    fn put_usize(buf: &mut Vec<u8>, v: usize) {
        put_u64(buf, u64::try_from(v).expect("usize fits u64"));
    }
    fn put_bytes(buf: &mut Vec<u8>, data: &[u8]) {
        put_usize(buf, data.len());
        buf.extend_from_slice(data);
    }
    fn put_coord(buf: &mut Vec<u8>, coord: RegionCoord) {
        match coord {
            RegionCoord::Emacs(p) => {
                buf.push(0);
                put_usize(buf, p.raw());
            }
            RegionCoord::Neovim(p) => {
                buf.push(1);
                put_usize(buf, p.line());
                put_usize(buf, p.col_byte());
            }
            RegionCoord::Agent(AgentPosition::Byte(p)) => {
                buf.push(2);
                put_usize(buf, p.raw());
            }
            RegionCoord::Agent(AgentPosition::LineColUtf16(p)) => {
                buf.push(3);
                put_usize(buf, p.line());
                put_usize(buf, p.col_utf16());
            }
        }
    }
    fn put_region(buf: &mut Vec<u8>, region: OrientedRegion) {
        put_coord(buf, region.anchor);
        put_coord(buf, region.head);
        buf.push(match region.direction {
            Direction::Empty => 0,
            Direction::Forward => 1,
            Direction::Backward => 2,
        });
        buf.push(match region.mode {
            EndpointMode::Exclusive => 0,
            EndpointMode::Inclusive => 1,
        });
    }
    fn put_edit_spec(buf: &mut Vec<u8>, edit: &EditSpec) {
        put_u64(buf, edit.buffer.raw());
        put_region(buf, edit.region);
        put_bytes(buf, edit.replacement.as_bytes());
    }
    fn put_sequence_step(buf: &mut Vec<u8>, step: Option<SequenceStep>) {
        match step {
            None => buf.push(0),
            Some(step) => {
                buf.push(1);
                put_u64(buf, step.sequence.actor().raw());
                put_u64(buf, step.sequence.nonce().raw());
                put_usize(buf, step.index);
            }
        }
    }

    fn digest_committed_proposal(request: RequestId, receipt: CommitReceipt) -> [u8; 32] {
        let mut buf = b"zen:core:proposal_commit:v1\0".to_vec();
        put_u64(&mut buf, request.raw());
        put_u64(&mut buf, receipt.txn().actor().raw());
        put_u64(&mut buf, receipt.txn().nonce().raw());
        put_u64(&mut buf, receipt.intent().base().raw());
        put_u64(&mut buf, receipt.previous().raw());
        put_u64(&mut buf, receipt.committed().raw());
        retention::sha256(buf)
    }

    fn digest_open(req: &OpenBufferRequest) -> [u8; 32] {
        let mut buf = b"zen:core:open:v1\0".to_vec();
        put_u64(&mut buf, req.request.raw());
        put_u64(&mut buf, req.actor.raw());
        put_u64(&mut buf, req.nonce.raw());
        put_u64(&mut buf, req.base.raw());
        put_bytes(&mut buf, req.name.as_bytes());
        put_bytes(&mut buf, req.text.as_bytes());
        retention::sha256(buf)
    }

    fn digest_close(req: &CloseBufferRequest) -> [u8; 32] {
        let mut buf = b"zen:core:close:v1\0".to_vec();
        put_u64(&mut buf, req.request.raw());
        put_u64(&mut buf, req.actor.raw());
        put_u64(&mut buf, req.nonce.raw());
        put_u64(&mut buf, req.base.raw());
        put_u64(&mut buf, req.buffer.raw());
        buf.push(match req.policy {
            ClosePolicy::RequireClean => 0,
            ClosePolicy::Discard => 1,
        });
        retention::sha256(buf)
    }

    fn digest_edit_request(req: &EditRequest) -> [u8; 32] {
        let mut buf = b"zen:core:edit:v1\0".to_vec();
        put_u64(&mut buf, req.request.raw());
        put_u64(&mut buf, req.actor.raw());
        put_u64(&mut buf, req.nonce.raw());
        put_u64(&mut buf, req.base.raw());
        put_edit_spec(&mut buf, &req.edit);
        put_sequence_step(&mut buf, req.sequence_step);
        retention::sha256(buf)
    }

    fn digest_multi_batch_request(req: &MultiBufferBatchRequest) -> [u8; 32] {
        let mut buf = b"zen:core:batch:v1\0".to_vec();
        put_u64(&mut buf, req.request.raw());
        put_u64(&mut buf, req.actor.raw());
        put_u64(&mut buf, req.nonce.raw());
        put_u64(&mut buf, req.base.raw());
        put_u64(&mut buf, req.primary.raw());
        put_usize(&mut buf, req.edits.len());
        for edit in &req.edits {
            put_edit_spec(&mut buf, edit);
        }
        put_sequence_step(&mut buf, req.sequence_step);
        retention::sha256(buf)
    }

    fn digest_command_request(req: &CommandRequest) -> [u8; 32] {
        let mut buf = b"zen:core:commands:v1\0".to_vec();
        put_u64(&mut buf, req.request.raw());
        put_u64(&mut buf, req.actor.raw());
        put_u64(&mut buf, req.nonce.raw());
        put_u64(&mut buf, req.base.raw());
        put_u64(&mut buf, req.buffer.raw());
        match &req.commands {
            HostCommands::Emacs(cmds) => {
                buf.push(0);
                put_usize(&mut buf, cmds.len());
                for c in cmds {
                    match c {
                        EmacsCommand::Point(p) => {
                            buf.push(0);
                            put_usize(&mut buf, p.raw());
                        }
                        EmacsCommand::Mark(p) => {
                            buf.push(1);
                            put_usize(&mut buf, p.raw());
                        }
                        EmacsCommand::SelectRegion => buf.push(2),
                        EmacsCommand::Insert(t) => {
                            buf.push(3);
                            put_bytes(&mut buf, t.as_bytes());
                        }
                        EmacsCommand::ReplaceRegion(t) => {
                            buf.push(4);
                            put_bytes(&mut buf, t.as_bytes());
                        }
                        EmacsCommand::BeginChangeGroup => buf.push(5),
                        EmacsCommand::EndChangeGroup => buf.push(6),
                        EmacsCommand::Unsupported(s) => {
                            buf.push(7);
                            put_bytes(&mut buf, s.as_bytes());
                        }
                    }
                }
            }
            HostCommands::Neovim(cmds) => {
                buf.push(1);
                put_usize(&mut buf, cmds.len());
                for c in cmds {
                    match c {
                        NeovimCommand::Cursor(p) => {
                            buf.push(0);
                            put_usize(&mut buf, p.line());
                            put_usize(&mut buf, p.col_byte());
                        }
                        NeovimCommand::Select { anchor, head } => {
                            buf.push(1);
                            put_usize(&mut buf, anchor.line());
                            put_usize(&mut buf, anchor.col_byte());
                            put_usize(&mut buf, head.line());
                            put_usize(&mut buf, head.col_byte());
                        }
                        NeovimCommand::Insert(t) => {
                            buf.push(2);
                            put_bytes(&mut buf, t.as_bytes());
                        }
                        NeovimCommand::SetText { start, end, text } => {
                            buf.push(3);
                            put_usize(&mut buf, start.line());
                            put_usize(&mut buf, start.col_byte());
                            put_usize(&mut buf, end.line());
                            put_usize(&mut buf, end.col_byte());
                            put_bytes(&mut buf, text.as_bytes());
                        }
                        NeovimCommand::BeginUndoBlock => buf.push(4),
                        NeovimCommand::EndUndoBlock => buf.push(5),
                        NeovimCommand::Unsupported(s) => {
                            buf.push(6);
                            put_bytes(&mut buf, s.as_bytes());
                        }
                    }
                }
            }
            HostCommands::Agent(cmds) => {
                buf.push(2);
                put_usize(&mut buf, cmds.len());
                for c in cmds {
                    match c {
                        AgentCommand::Select { anchor, head } => {
                            buf.push(0);
                            put_coord(&mut buf, RegionCoord::Agent(*anchor));
                            put_coord(&mut buf, RegionCoord::Agent(*head));
                        }
                        AgentCommand::Replace { start, end, text } => {
                            buf.push(1);
                            put_coord(&mut buf, RegionCoord::Agent(*start));
                            put_coord(&mut buf, RegionCoord::Agent(*end));
                            put_bytes(&mut buf, text.as_bytes());
                        }
                        AgentCommand::BeginBatch => buf.push(2),
                        AgentCommand::EndBatch => buf.push(3),
                        AgentCommand::Unsupported(s) => {
                            buf.push(4);
                            put_bytes(&mut buf, s.as_bytes());
                        }
                    }
                }
            }
        }
        put_sequence_step(&mut buf, req.sequence_step);
        retention::sha256(buf)
    }

    fn digest_rectangle_request(req: &RectangleEditRequest) -> [u8; 32] {
        let mut buf = b"zen:core:rect:v1\0".to_vec();
        put_u64(&mut buf, req.request.raw());
        put_u64(&mut buf, req.actor.raw());
        put_u64(&mut buf, req.nonce.raw());
        put_u64(&mut buf, req.base.raw());
        put_u64(&mut buf, req.buffer.raw());
        match req.rectangle {
            RectangleSpec::Visual(r) => {
                buf.push(0);
                put_usize(&mut buf, r.first.line);
                put_usize(&mut buf, r.first.column);
                put_usize(&mut buf, r.second.line);
                put_usize(&mut buf, r.second.column);
                buf.push(match r.endpoint {
                    Endpoint::Exclusive => 0,
                    Endpoint::Inclusive => 1,
                });
                put_usize(&mut buf, r.tab_width);
            }
            RectangleSpec::Positions {
                first,
                second,
                endpoint,
                tab_width,
            } => {
                buf.push(1);
                put_coord(&mut buf, first.to_region_coord());
                put_coord(&mut buf, second.to_region_coord());
                buf.push(match endpoint {
                    Endpoint::Exclusive => 0,
                    Endpoint::Inclusive => 1,
                });
                put_usize(&mut buf, tab_width);
            }
        }
        buf.push(match req.policy {
            ShortLinePolicy::Clamp => 0,
            ShortLinePolicy::Error => 1,
        });
        match &req.operation {
            RectangleEditOp::Delete => buf.push(0),
            RectangleEditOp::Replace(rows) => {
                buf.push(1);
                put_usize(&mut buf, rows.len());
                for row in rows {
                    put_bytes(&mut buf, row.as_bytes());
                }
            }
            RectangleEditOp::Uniform(v) => {
                buf.push(2);
                put_bytes(&mut buf, v.as_bytes());
            }
            RectangleEditOp::Insert(v) => {
                buf.push(3);
                put_bytes(&mut buf, v.as_bytes());
            }
        }
        put_sequence_step(&mut buf, req.sequence_step);
        retention::sha256(buf)
    }

    fn digest_indent_request(req: &IndentRegionRequest) -> [u8; 32] {
        let mut buf = b"zen:core:indent:v1\0".to_vec();
        put_u64(&mut buf, req.request.raw());
        put_u64(&mut buf, req.actor.raw());
        put_u64(&mut buf, req.nonce.raw());
        put_u64(&mut buf, req.base.raw());
        put_u64(&mut buf, req.buffer.raw());
        put_usize(&mut buf, req.spec.start_line);
        put_usize(&mut buf, req.spec.end_line);
        put_usize(&mut buf, req.spec.tab_width);
        buf.push(match req.spec.style {
            IndentStyle::Spaces => 0,
            IndentStyle::TabsAndSpaces => 1,
        });
        match req.spec.action {
            IndentAction::SetColumn(c) => {
                buf.push(0);
                put_usize(&mut buf, c);
            }
            IndentAction::IndentBy(c) => {
                buf.push(1);
                put_usize(&mut buf, c);
            }
            IndentAction::DedentBy(c) => {
                buf.push(2);
                put_usize(&mut buf, c);
            }
        }
        put_sequence_step(&mut buf, req.sequence_step);
        retention::sha256(buf)
    }

    fn digest_undo_redo(request: RequestId, core_req: CoreRequest) -> [u8; 32] {
        let mut buf = b"zen:core:undo_redo:v1\0".to_vec();
        put_u64(&mut buf, request.raw());
        match core_req {
            CoreRequest::Undo(u) => {
                buf.push(0);
                put_u64(&mut buf, u.actor.raw());
                put_u64(&mut buf, u.target.actor().raw());
                put_u64(&mut buf, u.target.nonce().raw());
                put_u64(&mut buf, u.operation.actor().raw());
                put_u64(&mut buf, u.operation.nonce().raw());
            }
            CoreRequest::Redo(r) => {
                buf.push(1);
                put_u64(&mut buf, r.actor.raw());
                put_u64(&mut buf, r.target.actor().raw());
                put_u64(&mut buf, r.target.nonce().raw());
                put_u64(&mut buf, r.operation.actor().raw());
                put_u64(&mut buf, r.operation.nonce().raw());
            }
        }
        retention::sha256(buf)
    }

    // ----------------------------------------------------------------------
    // Exhaustive error classifiers
    // ----------------------------------------------------------------------

    fn map_boundary_error(error: BoundaryError) -> CoreError {
        let kind = match error.code() {
            ErrorCode::InvalidUtf8 => CoreErrorKind::InvalidUtf8,
            ErrorCode::InvalidBoundary => CoreErrorKind::InvalidBoundary,
            ErrorCode::InvalidUtf16Boundary => CoreErrorKind::InvalidUtf16Boundary,
            ErrorCode::OutOfRange => CoreErrorKind::OutOfRange,
            ErrorCode::InvalidSpan => CoreErrorKind::InvalidSpan,
            ErrorCode::RevisionOverflow => CoreErrorKind::RevisionOverflow,
            ErrorCode::RevisionMismatch => CoreErrorKind::RevisionMismatch,
            ErrorCode::BaseExpired => CoreErrorKind::BaseExpired,
            ErrorCode::UnknownBuffer => CoreErrorKind::UnknownBuffer,
            ErrorCode::BufferClosed => CoreErrorKind::BufferClosed,
            ErrorCode::DuplicateBufferId => CoreErrorKind::DuplicateBufferId,
            ErrorCode::UnknownActor => CoreErrorKind::UnknownActor,
            ErrorCode::RevokedActor => CoreErrorKind::RevokedActor,
            ErrorCode::Conflict => CoreErrorKind::Conflict,
            ErrorCode::TxnIdReuse => CoreErrorKind::TxnIdReuse,
            ErrorCode::EmptyPattern => CoreErrorKind::EmptyPattern,
            ErrorCode::PermissionDenied => CoreErrorKind::PermissionDenied,
            ErrorCode::StaleApproval => CoreErrorKind::StaleApproval,
            ErrorCode::UnsupportedOperation => CoreErrorKind::UnsupportedOperation,
            ErrorCode::InvalidCoordinate => CoreErrorKind::InvalidCoordinate,
            ErrorCode::InvalidLineEndings => CoreErrorKind::InvalidLineEndings,
            ErrorCode::CorruptSequence => CoreErrorKind::CorruptSequence,
        };
        CoreError::new(kind, error.to_string())
    }

    fn map_registry_error(error: RegistryError) -> CoreError {
        let kind = match error {
            RegistryError::RoleConflict { .. } => CoreErrorKind::RoleConflict,
            RegistryError::ActorRevoked(_) => CoreErrorKind::RevokedActor,
            RegistryError::UnknownActor(_) => CoreErrorKind::UnknownActor,
        };
        CoreError::new(kind, error.to_string())
    }

    fn map_access_error(error: &AccessError, registry: &ActorRegistry) -> CoreError {
        let kind = match error {
            AccessError::InactiveActor(actor) => {
                if registry.is_revoked(*actor) {
                    CoreErrorKind::RevokedActor
                } else {
                    CoreErrorKind::UnknownActor
                }
            }
            AccessError::UnknownBuffer(_) => CoreErrorKind::UnknownBuffer,
            AccessError::BufferClosed(_) => CoreErrorKind::BufferClosed,
            AccessError::Denied { .. } => CoreErrorKind::PermissionDenied,
            AccessError::Transaction(err) => return map_transaction_error(err.clone()),
        };
        CoreError::new(kind, error.to_string())
    }

    fn map_transaction_error(error: TransactionError) -> CoreError {
        let kind = match error {
            TransactionError::RevisionOverflow => CoreErrorKind::RevisionOverflow,
            TransactionError::RevisionMismatch { .. } => CoreErrorKind::RevisionMismatch,
            TransactionError::UnknownBuffer(_) => CoreErrorKind::UnknownBuffer,
            TransactionError::BufferClosed(_) => CoreErrorKind::BufferClosed,
            TransactionError::DuplicateBufferId(_) => CoreErrorKind::DuplicateBufferId,
            TransactionError::Patch { .. } => CoreErrorKind::InvalidSpan,
        };
        CoreError::new(kind, error.to_string())
    }

    fn map_rebase_error(error: &RebaseError) -> CoreError {
        let kind = match error {
            RebaseError::RevisionMismatch { .. } => CoreErrorKind::RevisionMismatch,
            RebaseError::RevisionOverflow => CoreErrorKind::RevisionOverflow,
            RebaseError::Conflict(_) => CoreErrorKind::Conflict,
            RebaseError::Transaction(err) => return map_transaction_error(err.clone()),
            RebaseError::TargetMismatch | RebaseError::DisconnectedChain => {
                CoreErrorKind::InvalidEvent
            }
            RebaseError::Anchor(_) | RebaseError::Span(_) | RebaseError::Patch { .. } => {
                CoreErrorKind::InvalidSpan
            }
        };
        CoreError::new(kind, format!("{error:?}"))
    }

    fn map_retention_error(error: &RetentionError) -> CoreError {
        let kind = match error {
            RetentionError::BaseExpired { .. } => CoreErrorKind::BaseExpired,
            RetentionError::FutureRevision { .. } => CoreErrorKind::FutureRevision,
            RetentionError::TxnIdReuse(_) => CoreErrorKind::TxnIdReuse,
            RetentionError::RequestIdReuse(_) => CoreErrorKind::RequestReuse,
            RetentionError::InvalidCommit => CoreErrorKind::InvalidEvent,
            RetentionError::Rebase(err) => return map_rebase_error(err),
        };
        CoreError::new(kind, error.to_string())
    }

    fn map_delivery_error(error: &DeliveryError) -> CoreError {
        let kind = match error {
            DeliveryError::FutureRevision { .. } => CoreErrorKind::FutureRevision,
            DeliveryError::BaseExpired { .. } => CoreErrorKind::BaseExpired,
            DeliveryError::NonContiguous { .. } => CoreErrorKind::ResyncGap,
            DeliveryError::InvalidEvent => CoreErrorKind::InvalidEvent,
        };
        CoreError::new(kind, error.to_string())
    }

    fn map_checkpoint_error(error: &CheckpointError) -> CoreError {
        let kind = match error {
            CheckpointError::InconsistentState => CoreErrorKind::InvalidEvent,
            CheckpointError::InactiveActor => CoreErrorKind::UnknownActor,
            CheckpointError::Gone(_) => CoreErrorKind::BufferClosed,
            CheckpointError::UnknownBuffer(_) => CoreErrorKind::UnknownBuffer,
            CheckpointError::NameInUse => CoreErrorKind::NameInUse,
            CheckpointError::Dirty(_) => CoreErrorKind::DirtyBuffer,
            CheckpointError::IdExhausted => CoreErrorKind::IdExhausted,
            CheckpointError::BaseExpired { .. } => CoreErrorKind::BaseExpired,
            CheckpointError::FutureRevision { .. } => CoreErrorKind::FutureRevision,
            CheckpointError::RequestIdReuse(_) => CoreErrorKind::RequestReuse,
            CheckpointError::TxnIdReuse(_) => CoreErrorKind::TxnIdReuse,
            CheckpointError::Delivery(err) => return map_delivery_error(err),
            CheckpointError::Retention(err) => return map_retention_error(err),
            CheckpointError::Workspace(_) => CoreErrorKind::RevisionOverflow,
            CheckpointError::Transaction(err) => return map_transaction_error(err.clone()),
            CheckpointError::Rebase(err) => return map_rebase_error(err),
            CheckpointError::Journal(_) => CoreErrorKind::InvalidEvent,
        };
        CoreError::new(kind, error.to_string())
    }

    fn map_proposal_error(error: &ProposalError, registry: &ActorRegistry) -> CoreError {
        match error {
            ProposalError::Access(err) => map_access_error(err, registry),
            ProposalError::Transaction(err) => map_transaction_error(err.clone()),
            ProposalError::NotHuman(_) => {
                CoreError::new(CoreErrorKind::NotHumanReviewer, error.to_string())
            }
            ProposalError::EmptyProposal => {
                CoreError::new(CoreErrorKind::EmptyProposal, error.to_string())
            }
            ProposalError::ApprovalMismatch => {
                CoreError::new(CoreErrorKind::ApprovalMismatch, error.to_string())
            }
            ProposalError::RevisionMismatch { .. } => {
                CoreError::new(CoreErrorKind::RevisionMismatch, error.to_string())
            }
        }
    }

    fn map_coordinator_error(error: &CoordinatorError, registry: &ActorRegistry) -> CoreError {
        match error {
            CoordinatorError::Access(err) => map_access_error(err, registry),
            CoordinatorError::Checkpoint(err) => map_checkpoint_error(err),
            CoordinatorError::Proposal(err) => map_proposal_error(err, registry),
            CoordinatorError::Register(err) => {
                CoreError::new(CoreErrorKind::InvalidSpan, format!("{err:?}"))
            }
            CoordinatorError::RequestReuse(_) => {
                CoreError::new(CoreErrorKind::RequestReuse, format!("{error:?}"))
            }
            CoordinatorError::TransactionReuse(_) => {
                CoreError::new(CoreErrorKind::TxnIdReuse, format!("{error:?}"))
            }
            CoordinatorError::MissingProposal(_) => {
                CoreError::new(CoreErrorKind::MissingProposal, format!("{error:?}"))
            }
            CoordinatorError::Unapproved(_) => {
                CoreError::new(CoreErrorKind::UnapprovedProposal, format!("{error:?}"))
            }
            CoordinatorError::StaleApproval(_) => {
                CoreError::new(CoreErrorKind::StaleApproval, format!("{error:?}"))
            }
            CoordinatorError::RevisionMismatch { .. } => {
                CoreError::new(CoreErrorKind::RevisionMismatch, format!("{error:?}"))
            }
            CoordinatorError::UnknownTarget(_) => {
                CoreError::new(CoreErrorKind::UnknownUndoTarget, format!("{error:?}"))
            }
            CoordinatorError::NotOwner(_) => {
                CoreError::new(CoreErrorKind::NotUndoOwner, format!("{error:?}"))
            }
            CoordinatorError::InvalidTransaction(_) => {
                CoreError::new(CoreErrorKind::InvalidUndoTransaction, format!("{error:?}"))
            }
            CoordinatorError::AlreadyUndone(_) => {
                CoreError::new(CoreErrorKind::AlreadyUndone, format!("{error:?}"))
            }
            CoordinatorError::NotUndone(_) => {
                CoreError::new(CoreErrorKind::NotUndone, format!("{error:?}"))
            }
            CoordinatorError::NoChange => {
                CoreError::new(CoreErrorKind::NoChange, "operation produced no change")
            }
        }
    }

    fn map_concurrent_error(error: &ConcurrentError, registry: &ActorRegistry) -> CoreError {
        match error {
            ConcurrentError::LockPoisoned => {
                CoreError::new(CoreErrorKind::LockPoisoned, error.to_string())
            }
            ConcurrentError::Coordinator(err) => map_coordinator_error(err, registry),
            ConcurrentError::Access(err) => map_access_error(err, registry),
            ConcurrentError::Registry(err) => map_registry_error(*err),
            ConcurrentError::Journal(err) => {
                CoreError::new(CoreErrorKind::InvalidEvent, err.to_string())
            }
            ConcurrentError::Delivery(err) => map_delivery_error(err),
            ConcurrentError::Checkpoint(err) => map_checkpoint_error(err),
        }
    }

    fn map_resync_error(error: ResyncError) -> CoreError {
        match error {
            ResyncError::DifferentWorkspace => {
                CoreError::new(CoreErrorKind::DifferentWorkspace, error.to_string())
            }
            ResyncError::StaleSnapshot => {
                CoreError::new(CoreErrorKind::StaleSnapshot, error.to_string())
            }
            ResyncError::Conflict => CoreError::new(CoreErrorKind::Conflict, error.to_string()),
            ResyncError::Gap { .. } => CoreError::new(CoreErrorKind::ResyncGap, error.to_string()),
            ResyncError::InvalidEvent => {
                CoreError::new(CoreErrorKind::InvalidEvent, error.to_string())
            }
            ResyncError::Projection(err) => map_boundary_error(err),
        }
    }

    fn map_session_error(error: &SessionError, registry: &ActorRegistry) -> CoreError {
        match error {
            SessionError::Access(err) => map_access_error(err, registry),
            SessionError::InvalidState | SessionError::InvalidTransition => {
                CoreError::new(CoreErrorKind::InvalidSessionState, format!("{error:?}"))
            }
        }
    }

    fn map_group_error(error: GroupError) -> CoreError {
        CoreError::new(CoreErrorKind::GroupConflict, error.to_string())
    }

    fn map_reject_reason(reason: RejectReason) -> CoreErrorKind {
        match reason {
            RejectReason::Conflict => CoreErrorKind::Conflict,
            RejectReason::RevisionMismatch => CoreErrorKind::RevisionMismatch,
            RejectReason::BaseExpired => CoreErrorKind::BaseExpired,
            RejectReason::TxnIdReuse => CoreErrorKind::TxnIdReuse,
            RejectReason::PermissionDenied => CoreErrorKind::PermissionDenied,
            RejectReason::UnknownBuffer => CoreErrorKind::UnknownBuffer,
            RejectReason::BufferClosed => CoreErrorKind::BufferClosed,
            RejectReason::UnknownActor => CoreErrorKind::UnknownActor,
            RejectReason::RevokedActor => CoreErrorKind::RevokedActor,
            RejectReason::InvalidInput => CoreErrorKind::InvalidCoordinate,
        }
    }

    fn map_undo_error(error: UndoInteropError) -> CoreError {
        let kind = match &error {
            UndoInteropError::AmbiguousTarget => CoreErrorKind::AmbiguousUndoTarget,
            UndoInteropError::InvalidOperationActor => CoreErrorKind::InvalidUndoTransaction,
            UndoInteropError::TargetActorMismatch { .. } => CoreErrorKind::NotUndoOwner,
            UndoInteropError::Undo(err) => match err {
                UndoError::UnknownTarget(_) => CoreErrorKind::UnknownUndoTarget,
                UndoError::NotOwner { .. } => CoreErrorKind::NotUndoOwner,
                UndoError::AlreadyUndone { .. } => CoreErrorKind::AlreadyUndone,
                UndoError::CompensationTarget { .. } | UndoError::InvalidCompensationActor => {
                    CoreErrorKind::InvalidUndoTransaction
                }
                UndoError::CompensationIdReuse(_) => CoreErrorKind::TxnIdReuse,
                UndoError::Rejected(reason) => map_reject_reason(*reason),
                UndoError::NoOp => CoreErrorKind::NoChange,
            },
            UndoInteropError::Redo(err) => match err {
                RedoError::UnknownTarget(_) => CoreErrorKind::UnknownUndoTarget,
                RedoError::NotOwner { .. } => CoreErrorKind::NotUndoOwner,
                RedoError::NotUndone(_) => CoreErrorKind::NotUndone,
                RedoError::AlreadyRedone { .. } => CoreErrorKind::AlreadyUndone,
                RedoError::InvalidRedoActor => CoreErrorKind::InvalidUndoTransaction,
                RedoError::RedoIdReuse(_) => CoreErrorKind::TxnIdReuse,
                RedoError::Rejected(reason) => map_reject_reason(*reason),
                RedoError::NoOp => CoreErrorKind::NoChange,
            },
        };
        CoreError::new(kind, error.to_string())
    }

    fn map_span_error(error: SpanError) -> CoreError {
        let kind = match error {
            SpanError::InvertedRange { .. } => CoreErrorKind::InvalidSpan,
            SpanError::OutOfBounds { .. } => CoreErrorKind::OutOfRange,
            SpanError::NotCharBoundary { .. } => CoreErrorKind::InvalidBoundary,
        };
        CoreError::new(kind, error.to_string())
    }

    fn map_view_error(error: ViewError) -> CoreError {
        let kind = match &error {
            ViewError::UnknownBuffer(_) => CoreErrorKind::UnknownBuffer,
            ViewError::BufferClosed(_) => CoreErrorKind::BufferClosed,
            ViewError::BufferMismatch { .. } => CoreErrorKind::InvalidCoordinate,
            ViewError::SpanOutsideVisible { .. } => CoreErrorKind::OutOfRange,
            ViewError::InvalidSpan(err) => return map_span_error(err.clone()),
        };
        CoreError::new(kind, error.to_string())
    }

    fn map_selection_error(error: SelectionError) -> CoreError {
        let kind = match &error {
            SelectionError::BufferMismatch { .. } => CoreErrorKind::InvalidCoordinate,
            SelectionError::SpanOutsideVisible { .. } => CoreErrorKind::OutOfRange,
            SelectionError::UnknownBuffer(_) => CoreErrorKind::UnknownBuffer,
            SelectionError::BufferClosed(_) => CoreErrorKind::BufferClosed,
            SelectionError::InvalidSpan(err) => return map_span_error(err.clone()),
        };
        CoreError::new(kind, error.to_string())
    }

    fn map_column_error(error: ColumnError) -> CoreError {
        let kind = match &error {
            ColumnError::ZeroTabWidth | ColumnError::IndexMismatch => {
                CoreErrorKind::InvalidNavigation
            }
            ColumnError::ColumnOverflow => CoreErrorKind::OutOfRange,
            ColumnError::Line(line_err) => match line_err {
                crate::editor::text::lines::LineError::NotCharBoundary { .. } => {
                    CoreErrorKind::InvalidBoundary
                }
                _ => CoreErrorKind::OutOfRange,
            },
        };
        CoreError::new(kind, error.to_string())
    }

    fn map_navigation_error(error: NavigationError) -> CoreError {
        match error {
            NavigationError::View(e) => map_view_error(e),
            NavigationError::Selection(e) => map_selection_error(e),
            NavigationError::Column(e) => map_column_error(e),
            NavigationError::Word(e) => {
                CoreError::new(CoreErrorKind::InvalidNavigation, e.to_string())
            }
        }
    }

    fn map_search_error(error: &WorkspaceSearchError, registry: &ActorRegistry) -> CoreError {
        match error {
            WorkspaceSearchError::Search(_) => {
                CoreError::new(CoreErrorKind::EmptyPattern, error.to_string())
            }
            WorkspaceSearchError::Access(err) => map_access_error(err, registry),
        }
    }

    fn map_context_error(error: &ContextError, registry: &ActorRegistry) -> CoreError {
        match error {
            ContextError::InvalidBudget(_) => {
                CoreError::new(CoreErrorKind::InvalidBudget, error.to_string())
            }
            ContextError::EmptyParticipants => {
                CoreError::new(CoreErrorKind::EmptyParticipants, error.to_string())
            }
            ContextError::RevisionMismatch { .. } => {
                CoreError::new(CoreErrorKind::RevisionMismatch, error.to_string())
            }
            ContextError::Access(err) => map_access_error(err, registry),
            ContextError::BufferOutsideScope { .. } | ContextError::SpanOutsideScope { .. } => {
                CoreError::new(CoreErrorKind::OutsideScope, error.to_string())
            }
            ContextError::StaleView {
                error: span_err, ..
            }
            | ContextError::InvalidSpan {
                error: span_err, ..
            } => map_span_error(span_err.clone()),
            ContextError::InvalidLineRange { .. } => {
                CoreError::new(CoreErrorKind::InvalidLineRange, error.to_string())
            }
            ContextError::DisjointScopeForLineRange { .. } => {
                CoreError::new(CoreErrorKind::DisjointScope, error.to_string())
            }
            ContextError::WorkspaceTargetUnsupportedForSession => {
                CoreError::new(CoreErrorKind::UnsupportedOperation, error.to_string())
            }
        }
    }

    fn map_rectangle_error(error: RectangleEditError) -> CoreError {
        match error {
            RectangleEditError::Transaction(err) => map_transaction_error(err),
            RectangleEditError::Rectangle(_)
            | RectangleEditError::ShortLine { .. }
            | RectangleEditError::RowCount { .. }
            | RectangleEditError::Patch(_) => {
                CoreError::new(CoreErrorKind::InvalidRectangle, error.to_string())
            }
        }
    }

    fn map_indent_error(error: IndentRegionError) -> CoreError {
        match error {
            IndentRegionError::InvalidSpan(err) => map_span_error(err),
            IndentRegionError::InvalidRegion { .. }
            | IndentRegionError::Indent(_)
            | IndentRegionError::Patch(_)
            | IndentRegionError::OutsideRegion { .. }
            | IndentRegionError::NonIndentEdit { .. } => {
                CoreError::new(CoreErrorKind::InvalidIndent, error.to_string())
            }
        }
    }

    fn map_request_error(error: &RequestError, registry: &ActorRegistry) -> CoreError {
        match error {
            RequestError::Access(e) => map_access_error(e, registry),
            RequestError::Recovery(e) => map_resync_error(e.clone()),
            RequestError::Lifecycle(_) => {
                CoreError::new(CoreErrorKind::InvalidEvent, format!("{error:?}"))
            }
            RequestError::DifferentWorkspace => {
                CoreError::new(CoreErrorKind::DifferentWorkspace, format!("{error:?}"))
            }
            RequestError::InvalidOperation | RequestError::OpenRequiresLifecycle => {
                CoreError::new(CoreErrorKind::UnsupportedOperation, format!("{error:?}"))
            }
            RequestError::InvalidRecovery => {
                CoreError::new(CoreErrorKind::InvalidEvent, format!("{error:?}"))
            }
        }
    }

    fn map_utf8_error(error: Utf8Error) -> CoreError {
        let kind = match error {
            Utf8Error::InvalidUtf8 => CoreErrorKind::InvalidUtf8,
            Utf8Error::OutOfBounds { .. } => CoreErrorKind::OutOfRange,
            Utf8Error::NotCharBoundary { .. } => CoreErrorKind::InvalidBoundary,
            Utf8Error::InvertedRange { .. } => CoreErrorKind::InvalidSpan,
        };
        CoreError::new(kind, error.to_string())
    }
}

pub use facade::*;

#[cfg(test)]
mod tests {
    // Strictly a sibling of `mod facade`, so the compiler enforces that every
    // test interaction goes exclusively through `EditorCore`'s `pub` methods.
    use super::*;

    use std::collections::{BTreeMap, BTreeSet};
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::{Arc, Barrier};

    use crate::editor::{
        collab::{
            access::{AccessPolicy, Capability},
            concurrent::AccessChange,
            proposal::RequestId,
            receipt::CommitReceipt,
            registry::ActorRegistry,
            role::ActorRole,
        },
        edit::{
            ids::{ActorId, BufferId, Nonce, TxnId},
            intent::EditIntent,
            navigation::{SelectionMotion, WordMotion},
            rectangle_edit::{Rectangle, ShortLinePolicy},
        },
        interop::{
            agent_position::AgentPosition,
            annotations::{AgentAnnotation, AnnotationMetadata, EmacsOverlay, NeovimExtmark},
            commands::{AgentCommand, EmacsCommand, NeovimCommand},
            coordinates::{BytePos, EmacsCharPos, LineColByte, LineColUtf16},
            groups::{ChangeGroup, SequenceStep},
            region::{EndpointMode, OrientedRegion, RegionCoord},
            undo::Target as UndoTarget,
        },
        test_support::Rng,
        text::{
            anchors::Affinity,
            columns::{Rounding, VisualPosition},
            delimiters::DelimiterTable,
            indent::IndentStyle,
            rectangle::Endpoint,
            utf8::Text,
        },
        workspace::{
            annotations::{AnnotationId, AnnotationKey, CollapsePolicy, NamespaceId},
            buffer_lifecycle::ClosePolicy,
            context::{
                ContextBudget, ContextPack, ContextRequest, ContextScope, ContextTarget,
                ParticipantScope,
            },
            selections::Direction,
            version::Revision,
        },
    };

    // ======================================================================
    // Independent Plain-Data Reference Model & Oracles (zero prod feedback)
    // ======================================================================

    const ALL_FORMATS: [HostFormat; 4] = [
        HostFormat::Emacs,
        HostFormat::Neovim,
        HostFormat::AGENT_BYTE,
        HostFormat::AGENT_UTF16,
    ];

    const THREE_FORMATS: [HostFormat; 3] = [
        HostFormat::Emacs,
        HostFormat::Neovim,
        HostFormat::AGENT_BYTE,
    ];

    fn ref_byte_to_emacs(text: &str, byte: usize) -> EmacsCharPos {
        assert!(text.is_char_boundary(byte));
        EmacsCharPos::new(1 + text[..byte].chars().count()).unwrap()
    }

    fn ref_byte_to_neovim(text: &str, byte: usize) -> LineColByte {
        assert!(text.is_char_boundary(byte));
        let prefix = &text[..byte];
        let line = prefix.bytes().filter(|&b| b == b'\n').count();
        let col = prefix.rsplit('\n').next().unwrap().len();
        LineColByte::new(line, col)
    }

    fn ref_byte_to_utf16(text: &str, byte: usize) -> LineColUtf16 {
        assert!(text.is_char_boundary(byte));
        let prefix = &text[..byte];
        let line = prefix.bytes().filter(|&b| b == b'\n').count();
        let col = prefix.rsplit('\n').next().unwrap().encode_utf16().count();
        LineColUtf16::new(line, col)
    }

    fn ref_pos(text: &str, byte: usize, format: HostFormat) -> HostPosition {
        match format {
            HostFormat::Emacs => HostPosition::Emacs(ref_byte_to_emacs(text, byte)),
            HostFormat::Neovim => HostPosition::Neovim(ref_byte_to_neovim(text, byte)),
            HostFormat::Agent(AgentCoordKind::Byte) => {
                HostPosition::Agent(AgentPosition::Byte(BytePos::new(byte)))
            }
            HostFormat::Agent(AgentCoordKind::LineColUtf16) => {
                HostPosition::Agent(AgentPosition::LineColUtf16(ref_byte_to_utf16(text, byte)))
            }
        }
    }

    fn ref_region(
        text: &str,
        anchor: usize,
        head: usize,
        mode: EndpointMode,
        format: HostFormat,
    ) -> OrientedRegion {
        let direction = match anchor.cmp(&head) {
            std::cmp::Ordering::Equal => Direction::Empty,
            std::cmp::Ordering::Less => Direction::Forward,
            std::cmp::Ordering::Greater => Direction::Backward,
        };
        let (a_enc, h_enc) = match mode {
            EndpointMode::Exclusive => (anchor, head),
            EndpointMode::Inclusive => {
                if anchor == head {
                    assert_eq!(anchor, text.len(), "inclusive empty only valid at EOF");
                    (anchor, head)
                } else if anchor < head {
                    let prev = text[..head].char_indices().next_back().unwrap().0;
                    (anchor, prev)
                } else {
                    let prev = text[..anchor].char_indices().next_back().unwrap().0;
                    (prev, head)
                }
            }
        };
        OrientedRegion {
            anchor: ref_pos(text, a_enc, format).to_region_coord(),
            head: ref_pos(text, h_enc, format).to_region_coord(),
            direction,
            mode,
        }
    }

    fn ref_split_neovim(text: &str) -> (Vec<String>, bool) {
        if text.is_empty() {
            return (Vec::new(), false);
        }
        if let Some(stripped) = text.strip_suffix('\n') {
            (stripped.split('\n').map(str::to_owned).collect(), true)
        } else {
            (text.split('\n').map(str::to_owned).collect(), false)
        }
    }

    fn ref_char_boundaries(text: &str) -> Vec<usize> {
        text.char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(text.len()))
            .collect()
    }

    fn ref_transport_offset(
        offset: usize,
        right_affinity: bool,
        start: usize,
        end: usize,
        inserted_len: usize,
    ) -> usize {
        if (start == end && inserted_len == 0) || offset < start {
            offset
        } else if offset > end {
            offset - (end - start) + inserted_len
        } else if right_affinity {
            start + inserted_len
        } else {
            start
        }
    }

    fn ref_search_all(text: &str, pattern: &str) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for (idx, _) in text.char_indices() {
            if text[idx..].starts_with(pattern) {
                out.push((idx, idx + pattern.len()));
            }
        }
        out
    }

    fn ref_truncate_utf8(slice: &str, max_bytes: usize) -> usize {
        if slice.len() <= max_bytes {
            return slice.len();
        }
        let mut end = max_bytes;
        while end > 0 && !slice.is_char_boundary(end) {
            end -= 1;
        }
        end
    }

    fn ref_move_word(
        text: &str,
        vis_start: usize,
        vis_end: usize,
        point: usize,
        motion: WordMotion,
    ) -> usize {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Kind {
            Space,
            Word,
            Punct,
        }
        let kind_of = |ch: char| {
            if ch.is_whitespace() {
                Kind::Space
            } else if ch.is_alphanumeric() || ch == '_' {
                Kind::Word
            } else {
                Kind::Punct
            }
        };
        let chars: Vec<(usize, char, Kind)> = text[vis_start..vis_end]
            .char_indices()
            .map(|(rel, ch)| (vis_start + rel, ch, kind_of(ch)))
            .collect();
        match motion {
            WordMotion::ForwardEnd => chars
                .iter()
                .enumerate()
                .filter_map(|(i, &(off, ch, kind))| {
                    let is_end = kind == Kind::Word
                        && (i + 1 == chars.len() || chars[i + 1].2 != Kind::Word);
                    is_end.then_some(off + ch.len_utf8())
                })
                .find(|&end| end > point)
                .unwrap_or(vis_end),
            WordMotion::NextStart => chars
                .iter()
                .enumerate()
                .filter_map(|(i, &(off, _, kind))| {
                    let is_start = kind != Kind::Space && (i == 0 || chars[i - 1].2 != kind);
                    is_start.then_some(off)
                })
                .find(|&s| s > point)
                .unwrap_or(vis_end),
            WordMotion::PreviousStart => chars
                .iter()
                .enumerate()
                .rev()
                .filter_map(|(i, &(off, _, kind))| {
                    let is_start = kind != Kind::Space && (i == 0 || chars[i - 1].2 != kind);
                    is_start.then_some(off)
                })
                .find(|&s| s < point)
                .unwrap_or(vis_start),
            WordMotion::NextEnd => chars
                .iter()
                .enumerate()
                .filter_map(|(i, &(off, _, kind))| {
                    let is_last =
                        kind != Kind::Space && (i + 1 == chars.len() || chars[i + 1].2 != kind);
                    is_last.then_some(off)
                })
                .find(|&last| last > point)
                .unwrap_or(vis_end),
        }
    }

    fn ref_delimiter_pairs(
        text: &str,
        vis_start: usize,
        vis_end: usize,
        defs: &[(char, char)],
    ) -> Vec<(usize, usize, usize)> {
        let mut stack: Vec<(usize, char)> = Vec::new();
        let mut pairs = Vec::new();
        for (rel, ch) in text[vis_start..vis_end].char_indices() {
            let abs = vis_start + rel;
            if let Some(&(_, close)) = defs.iter().find(|&&(open, _)| open == ch) {
                stack.push((abs, close));
            } else if defs.iter().any(|&(_, close)| close == ch)
                && stack.last().is_some_and(|&(_, expected)| expected == ch)
            {
                let (open, _) = stack.pop().unwrap();
                pairs.push((open, abs, abs + ch.len_utf8()));
            }
        }
        pairs.sort_by_key(|&(open, _, _)| open);
        pairs
    }

    fn ref_move_visual(
        text: &str,
        view: (usize, usize),
        point: usize,
        delta: isize,
        goal: Option<usize>,
        tab_width: usize,
        rounding: Rounding,
    ) -> (usize, usize) {
        let (vis_start, vis_end) = view;
        let mut lines = Vec::new();
        let mut start = 0usize;
        for segment in text.split('\n') {
            let end = start + segment.len();
            lines.push((start, end));
            start = end + 1;
        }
        let line_of = |off: usize| {
            lines
                .iter()
                .position(|&(s, e)| s <= off && off <= e)
                .unwrap()
        };
        let first_line = line_of(vis_start);
        let last_line = line_of(vis_end);
        let point_line = line_of(point);
        let sticky = goal.unwrap_or_else(|| {
            let (ls, _) = lines[point_line];
            let mut col = 0usize;
            for ch in text[ls..point].chars() {
                col += 1;
                if ch == '\t' {
                    while !col.is_multiple_of(tab_width) {
                        col += 1;
                    }
                }
            }
            col
        });
        let target_line = point_line
            .saturating_add_signed(delta)
            .clamp(first_line, last_line);
        let (ts, te) = lines[target_line];
        let mut col = 0usize;
        let mut target_off = te;
        for (rel, ch) in text[ts..te].char_indices() {
            if col == sticky {
                target_off = ts + rel;
                break;
            }
            let mut next_col = col + 1;
            if ch == '\t' {
                while !next_col.is_multiple_of(tab_width) {
                    next_col += 1;
                }
            }
            if sticky < next_col {
                target_off = match rounding {
                    Rounding::Floor => ts + rel,
                    Rounding::Ceil => ts + rel + ch.len_utf8(),
                };
                break;
            }
            col = next_col;
        }
        (target_off.clamp(vis_start, vis_end), sticky)
    }

    fn ref_indent_text(text: &str, spec: IndentSpec) -> String {
        let mut out = String::new();
        let mut line_idx = 0usize;
        let mut pos = 0usize;
        let bytes = text.as_bytes();
        loop {
            let line_start = pos;
            while pos < bytes.len() && bytes[pos] != b'\n' {
                pos += 1;
            }
            let line_end = pos;
            let has_lf = pos < bytes.len() && bytes[pos] == b'\n';
            let line_str = &text[line_start..line_end];
            if line_idx >= spec.start_line && line_idx < spec.end_line {
                let mut lead_bytes = 0usize;
                let mut cur_col = 0usize;
                for &b in line_str.as_bytes() {
                    if b == b' ' {
                        cur_col += 1;
                        lead_bytes += 1;
                    } else if b == b'\t' {
                        cur_col += spec.tab_width - cur_col % spec.tab_width;
                        lead_bytes += 1;
                    } else {
                        break;
                    }
                }
                let target_col = match spec.action {
                    IndentAction::SetColumn(c) => c,
                    IndentAction::IndentBy(d) => cur_col + d,
                    IndentAction::DedentBy(d) => cur_col.saturating_sub(d),
                };
                match spec.style {
                    IndentStyle::Spaces => {
                        for _ in 0..target_col {
                            out.push(' ');
                        }
                    }
                    IndentStyle::TabsAndSpaces => {
                        for _ in 0..(target_col / spec.tab_width) {
                            out.push('\t');
                        }
                        for _ in 0..(target_col % spec.tab_width) {
                            out.push(' ');
                        }
                    }
                }
                out.push_str(&line_str[lead_bytes..]);
            } else {
                out.push_str(line_str);
            }
            if has_lf {
                out.push('\n');
                pos += 1;
                line_idx += 1;
            } else {
                break;
            }
        }
        out
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RefAnnotation {
        start: usize,
        end: usize,
        start_affinity: Affinity,
        end_affinity: Affinity,
        priority: u32,
        collapse: CollapsePolicy,
        label: String,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RefProposal {
        proposer: ActorId,
        txn: TxnId,
        base_revision: Revision,
        buffers: Vec<BufferId>,
        status: ProposalStatus,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RefClient {
        actor: ActorId,
        buffer: BufferId,
        format: HostFormat,
        revision: Revision,
        text: String,
        closed: bool,
    }

    /// Plain-data independent reference model with zero production helper feedback.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    struct RefModel {
        revision: u64,
        floor: u64,
        delivery_floor: u64,
        next_buffer_id: u64,
        active_actors: BTreeMap<ActorId, ActorRole>,
        revoked_actors: BTreeSet<ActorId>,
        default_caps: BTreeSet<Capability>,
        actor_caps: BTreeSet<(ActorId, Capability)>,
        buffer_caps: BTreeSet<(ActorId, BufferId, Capability)>,
        live_buffers: BTreeMap<BufferId, String>,
        buffer_names: BTreeMap<String, BufferId>,
        tombstones: BTreeSet<BufferId>,
        saved_text: BTreeMap<BufferId, String>,
        views: BTreeMap<(ActorId, BufferId), (usize, usize)>,
        selections: BTreeMap<(ActorId, BufferId), (usize, usize)>,
        annotations: BTreeMap<(ActorId, AnnotationKey), RefAnnotation>,
        proposals: BTreeMap<RequestId, RefProposal>,
        clients: BTreeMap<ClientId, RefClient>,
        change_groups: Vec<Revision>,
        receipts_by_request: BTreeMap<RequestId, (ActorId, Nonce, Revision, Revision)>,
    }

    impl RefModel {
        fn register_actor(&mut self, actor: ActorId, role: ActorRole) {
            self.active_actors.insert(actor, role);
        }

        fn revoke_actor(&mut self, actor: ActorId) {
            self.active_actors.remove(&actor);
            self.revoked_actors.insert(actor);
            self.views.retain(|&(a, _), _| a != actor);
            self.selections.retain(|&(a, _), _| a != actor);
            self.annotations.retain(|&(a, _), _| a != actor);
        }

        fn has_cap(&self, actor: ActorId, buffer: BufferId, cap: Capability) -> bool {
            self.active_actors.contains_key(&actor)
                && (self.default_caps.contains(&cap)
                    || self.actor_caps.contains(&(actor, cap))
                    || self.buffer_caps.contains(&(actor, buffer, cap)))
        }

        fn apply_access(&mut self, change: AccessChange) {
            match change {
                AccessChange::GrantDefault(c) => {
                    self.default_caps.insert(c);
                }
                AccessChange::RevokeDefault(c) => {
                    self.default_caps.remove(&c);
                }
                AccessChange::GrantActor(a, c) => {
                    self.actor_caps.insert((a, c));
                }
                AccessChange::RevokeActor(a, c) => {
                    self.actor_caps.remove(&(a, c));
                }
                AccessChange::GrantBuffer(a, b, c) => {
                    self.buffer_caps.insert((a, b, c));
                }
                AccessChange::RevokeBuffer(a, b, c) => {
                    self.buffer_caps.remove(&(a, b, c));
                }
            }
            let live: BTreeSet<BufferId> = self.live_buffers.keys().copied().collect();
            let active = self.active_actors.clone();
            let def = self.default_caps.clone();
            let ac = self.actor_caps.clone();
            let bc = self.buffer_caps.clone();
            let readable = |a: ActorId, b: BufferId| -> bool {
                active.contains_key(&a)
                    && live.contains(&b)
                    && (def.contains(&Capability::Read)
                        || ac.contains(&(a, Capability::Read))
                        || bc.contains(&(a, b, Capability::Read)))
            };
            self.views.retain(|&(a, b), _| readable(a, b));
            self.selections.retain(|&(a, b), _| readable(a, b));
            self.annotations.retain(|&(a, k), _| readable(a, k.buffer));
        }

        fn open_buffer(
            &mut self,
            request: RequestId,
            actor: ActorId,
            nonce: Nonce,
            name: &str,
            text: &str,
        ) -> BufferId {
            let id = BufferId::new(self.next_buffer_id);
            self.next_buffer_id += 1;
            let prev = Revision::new(self.revision);
            self.revision += 1;
            let committed = Revision::new(self.revision);
            self.delivery_floor = self.revision;
            self.live_buffers.insert(id, text.to_owned());
            self.buffer_names.insert(name.to_owned(), id);
            self.saved_text.insert(id, text.to_owned());
            self.change_groups.push(committed);
            self.receipts_by_request
                .insert(request, (actor, nonce, prev, committed));
            id
        }

        fn close_buffer(
            &mut self,
            request: RequestId,
            actor: ActorId,
            nonce: Nonce,
            buffer: BufferId,
        ) {
            let prev = Revision::new(self.revision);
            self.revision += 1;
            let committed = Revision::new(self.revision);
            self.delivery_floor = self.revision;
            self.live_buffers.remove(&buffer);
            self.buffer_names.retain(|_, id| *id != buffer);
            self.saved_text.remove(&buffer);
            self.tombstones.insert(buffer);
            self.views.retain(|&(_, b), _| b != buffer);
            self.selections.retain(|&(_, b), _| b != buffer);
            self.annotations.retain(|&(_, k), _| k.buffer != buffer);
            self.change_groups.push(committed);
            self.receipts_by_request
                .insert(request, (actor, nonce, prev, committed));
        }

        /// Applies strictly separated ascending splices to one or more buffers in the reference model,
        /// independently transporting views, selections, and annotations.
        fn apply_splices(
            &mut self,
            request: RequestId,
            actor: ActorId,
            nonce: Nonce,
            per_buffer: &[BufferSplices<'_>],
        ) {
            let prev = Revision::new(self.revision);
            self.revision += 1;
            let committed = Revision::new(self.revision);
            for (buffer, splices) in per_buffer {
                let mut text = self.live_buffers.get(buffer).unwrap().clone();
                // Apply splices in descending order so earlier offsets stay valid.
                let mut ordered = splices.clone();
                ordered.sort_by_key(|&(s, e, _)| (s, e));
                for &(start, end, repl) in ordered.iter().rev() {
                    if start == end && repl.is_empty() {
                        continue;
                    }
                    text.replace_range(start..end, repl);
                    let ins = repl.len();
                    for (&(_, b), (vs, ve)) in &mut self.views {
                        if b == *buffer {
                            *vs = ref_transport_offset(*vs, false, start, end, ins);
                            *ve = ref_transport_offset(*ve, true, start, end, ins);
                        }
                    }
                    for (&(a, b), (sa, sh)) in &mut self.selections {
                        if b == *buffer {
                            let na = ref_transport_offset(*sa, true, start, end, ins);
                            let nh = ref_transport_offset(*sh, true, start, end, ins);
                            let (vs, ve) = self.views[&(a, b)];
                            *sa = na.clamp(vs, ve);
                            *sh = nh.clamp(vs, ve);
                        }
                    }
                    let mut evicted = Vec::new();
                    for (&(a, k), ann) in &mut self.annotations {
                        if k.buffer == *buffer {
                            let old_empty = ann.start == ann.end;
                            let ns = ref_transport_offset(
                                ann.start,
                                ann.start_affinity == Affinity::Right,
                                start,
                                end,
                                ins,
                            );
                            let ne = ref_transport_offset(
                                ann.end,
                                ann.end_affinity == Affinity::Right,
                                start,
                                end,
                                ins,
                            );
                            let collapsed = (!old_empty && ns >= ne) || (old_empty && ns > ne);
                            if collapsed {
                                match ann.collapse {
                                    CollapsePolicy::Evict => evicted.push((a, k)),
                                    CollapsePolicy::RetainEmpty => {
                                        ann.start = ns;
                                        ann.end = ns;
                                    }
                                }
                            } else {
                                ann.start = ns;
                                ann.end = ne;
                            }
                        }
                    }
                    for key in evicted {
                        self.annotations.remove(&key);
                    }
                }
                self.live_buffers.insert(*buffer, text);
            }
            self.change_groups.push(committed);
            self.receipts_by_request
                .insert(request, (actor, nonce, prev, committed));
        }

        fn set_view(&mut self, actor: ActorId, buffer: BufferId, start: usize, end: usize) {
            self.views.insert((actor, buffer), (start, end));
        }

        fn set_selection(&mut self, actor: ActorId, buffer: BufferId, anchor: usize, head: usize) {
            if !self.views.contains_key(&(actor, buffer)) {
                let len = self.live_buffers[&buffer].len();
                self.views.insert((actor, buffer), (0, len));
            }
            self.selections.insert((actor, buffer), (anchor, head));
        }

        fn set_annotation(&mut self, actor: ActorId, key: AnnotationKey, ann: RefAnnotation) {
            self.annotations.insert((actor, key), ann);
        }

        fn compact(&mut self, target_floor: u64) {
            self.floor = self.floor.max(target_floor);
            self.delivery_floor = self.delivery_floor.max(self.floor);
        }
    }

    // ======================================================================
    // Strict Public-Entrypoint-Only Observers & Comparators
    // ======================================================================

    type BufferSplices<'a> = (BufferId, Vec<(usize, usize, &'a str)>);
    type SpanLookupMap =
        BTreeMap<(ActorId, BufferId), Result<Option<(usize, usize)>, CoreErrorKind>>;

    /// Full public observable state snapshot captured strictly via `pub` methods.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct PublicStateStamp {
        checkpoint: CoreCheckpoint,
        revision: Revision,
        floor: Revision,
        live_buffers: Vec<BufferId>,
        tombstoned_buffers: Vec<BufferId>,
        buffer_names: BTreeMap<String, BufferId>,
        active_actors: Vec<(ActorId, ActorRole)>,
        change_groups: Vec<ChangeGroup>,
        reads: BTreeMap<(ActorId, BufferId), Result<String, CoreErrorKind>>,
        views: SpanLookupMap,
        selections: SpanLookupMap,
        proposals: BTreeMap<RequestId, Option<ProposalInfo>>,
        client_revs: BTreeMap<ClientId, Result<Revision, CoreErrorKind>>,
    }

    fn capture_public_state(
        core: &EditorCore,
        actors: &[ActorId],
        buffers: &[BufferId],
        proposals: &[RequestId],
        clients: &[ClientId],
    ) -> PublicStateStamp {
        let mut reads = BTreeMap::new();
        let mut views = BTreeMap::new();
        let mut selections = BTreeMap::new();
        for &a in actors {
            for &b in buffers {
                reads.insert(
                    (a, b),
                    core.read_agent(a, b).map(|s| s.text).map_err(|e| e.kind()),
                );
                views.insert(
                    (a, b),
                    core.view(a, b, HostFormat::AGENT_BYTE)
                        .map(|opt| opt.map(|v| (v.start_byte, v.end_byte)))
                        .map_err(|e| e.kind()),
                );
                selections.insert(
                    (a, b),
                    core.selection(a, b, HostFormat::AGENT_BYTE)
                        .map(|opt| opt.map(|s| (s.anchor_byte, s.head_byte)))
                        .map_err(|e| e.kind()),
                );
            }
        }
        let prop_map = proposals
            .iter()
            .map(|&r| (r, core.proposal_info(r).unwrap()))
            .collect();
        let client_revs = clients
            .iter()
            .map(|&c| (c, core.client_revision(c).map_err(|e| e.kind())))
            .collect();
        PublicStateStamp {
            checkpoint: core.checkpoint().unwrap(),
            revision: core.revision().unwrap(),
            floor: core.floor().unwrap(),
            live_buffers: core.live_buffers().unwrap(),
            tombstoned_buffers: core.tombstoned_buffers().unwrap(),
            buffer_names: core.buffer_names().unwrap(),
            active_actors: core.active_actors().unwrap(),
            change_groups: core.change_groups().unwrap(),
            reads,
            views,
            selections,
            proposals: prop_map,
            client_revs,
        }
    }

    /// Asserts an operation fails with `expected` and leaves ALL public state unchanged.
    fn assert_rejection_atomic<T: std::fmt::Debug>(
        core: &EditorCore,
        actors: &[ActorId],
        buffers: &[BufferId],
        proposals: &[RequestId],
        clients: &[ClientId],
        expected: CoreErrorKind,
        op: impl FnOnce() -> Result<T, CoreError>,
    ) {
        let before = capture_public_state(core, actors, buffers, proposals, clients);
        let err = op().expect_err("expected operation to be rejected");
        assert_eq!(err.kind(), expected, "unexpected error: {err}");
        let after = capture_public_state(core, actors, buffers, proposals, clients);
        assert_eq!(
            after, before,
            "rejected operation mutated public core state"
        );
    }

    /// Verifies EVERY public observation of `core` against the independent `RefModel`.
    fn assert_core_matches_model(
        core: &EditorCore,
        model: &RefModel,
        known_actors: &[ActorId],
        local_client_snaps: &BTreeMap<ClientId, HostSnapshot>,
    ) {
        assert_eq!(core.revision().unwrap(), Revision::new(model.revision));
        assert_eq!(core.floor().unwrap(), Revision::new(model.floor));
        assert_eq!(
            core.live_buffers().unwrap(),
            model.live_buffers.keys().copied().collect::<Vec<_>>()
        );
        assert_eq!(
            core.tombstoned_buffers().unwrap(),
            model.tombstones.iter().copied().collect::<Vec<_>>()
        );
        assert_eq!(core.buffer_names().unwrap(), model.buffer_names);
        assert_eq!(
            core.active_actors().unwrap(),
            model
                .active_actors
                .iter()
                .map(|(&a, &r)| (a, r))
                .collect::<Vec<_>>()
        );
        let group_revs: Vec<Revision> = core
            .change_groups()
            .unwrap()
            .into_iter()
            .map(|g| g.receipt.committed())
            .collect();
        assert_eq!(group_revs, model.change_groups);

        for (&req, &(actor, nonce, prev, committed)) in &model.receipts_by_request {
            let r = core.receipt_by_request(req).unwrap().unwrap();
            assert_eq!(r.txn(), TxnId::new(actor, nonce));
            assert_eq!(r.previous(), prev);
            assert_eq!(r.committed(), committed);
            assert_eq!(core.receipt_by_txn(r.txn()).unwrap(), Some(r));
            assert_eq!(core.receipt_by_revision(committed).unwrap(), Some(r));
        }

        for &actor in known_actors {
            let active = model.active_actors.contains_key(&actor);
            let revoked = model.revoked_actors.contains(&actor);
            assert_eq!(core.is_actor_active(actor).unwrap(), active);
            assert_eq!(core.is_actor_revoked(actor).unwrap(), revoked);
            assert_eq!(
                core.actor_role(actor).unwrap(),
                model.active_actors.get(&actor).copied()
            );

            for (&buf, expected_text) in &model.live_buffers {
                for cap in Capability::ALL {
                    assert_eq!(
                        core.has_capability(actor, buf, cap).unwrap(),
                        model.has_cap(actor, buf, cap)
                    );
                }
                if model.has_cap(actor, buf, Capability::Read) {
                    let em = core.read_emacs(actor, buf).unwrap();
                    let nv = core.read_neovim(actor, buf).unwrap();
                    let ag = core.read_agent(actor, buf).unwrap();
                    assert_eq!(em.revision, Revision::new(model.revision));
                    assert_eq!(nv.revision, Revision::new(model.revision));
                    assert_eq!(ag.revision, Revision::new(model.revision));
                    assert_eq!(em.text, *expected_text);
                    assert_eq!(ag.text, *expected_text);
                    let (exp_lines, exp_eol) = ref_split_neovim(expected_text);
                    assert_eq!(nv.lines, exp_lines);
                    assert_eq!(nv.endofline, exp_eol);

                    let exp_view = model.views.get(&(actor, buf)).copied();
                    for fmt in ALL_FORMATS {
                        let actual_view = core.view(actor, buf, fmt).unwrap();
                        assert_eq!(actual_view.map(|v| (v.start_byte, v.end_byte)), exp_view);
                        if let (Some(v), Some((s, e))) = (actual_view, exp_view) {
                            assert_eq!(v.start, ref_pos(expected_text, s, fmt));
                            assert_eq!(v.end, ref_pos(expected_text, e, fmt));
                        }
                        let exp_sel = model.selections.get(&(actor, buf)).copied();
                        let actual_sel = core.selection(actor, buf, fmt).unwrap();
                        assert_eq!(actual_sel.map(|s| (s.anchor_byte, s.head_byte)), exp_sel);
                        if let (Some(sel), Some((a, h))) = (actual_sel, exp_sel) {
                            assert_eq!(
                                sel.region.anchor,
                                ref_pos(expected_text, a, fmt).to_region_coord()
                            );
                            assert_eq!(
                                sel.region.head,
                                ref_pos(expected_text, h, fmt).to_region_coord()
                            );
                        }
                    }

                    let exp_anns: Vec<(AnnotationKey, &RefAnnotation)> = model
                        .annotations
                        .iter()
                        .filter(|&(&(a, k), _)| a == actor && k.buffer == buf)
                        .map(|(&(_, k), v)| (k, v))
                        .collect();
                    for fmt in ALL_FORMATS {
                        let got = core.annotations_in_buffer(actor, buf, fmt).unwrap();
                        assert_eq!(got.len(), exp_anns.len());
                        for (actual_ann, (key, exp)) in got.iter().zip(&exp_anns) {
                            assert_eq!(actual_ann.key(), *key);
                            assert_eq!(actual_ann.metadata().priority, exp.priority);
                            assert_eq!(actual_ann.metadata().collapse, exp.collapse);
                            assert_eq!(actual_ann.metadata().label, exp.label);
                            match (fmt, actual_ann) {
                                (HostFormat::Emacs, HostAnnotation::Emacs(ov)) => {
                                    assert_eq!(
                                        ov.start,
                                        ref_byte_to_emacs(expected_text, exp.start)
                                    );
                                    assert_eq!(ov.end, ref_byte_to_emacs(expected_text, exp.end));
                                    assert_eq!(
                                        ov.front_advance,
                                        exp.start_affinity == Affinity::Right
                                    );
                                    assert_eq!(
                                        ov.rear_advance,
                                        exp.end_affinity == Affinity::Right
                                    );
                                }
                                (HostFormat::Neovim, HostAnnotation::Neovim(em)) => {
                                    assert_eq!(
                                        em.start,
                                        ref_byte_to_neovim(expected_text, exp.start)
                                    );
                                    assert_eq!(em.end, ref_byte_to_neovim(expected_text, exp.end));
                                    assert_eq!(
                                        em.right_gravity,
                                        exp.start_affinity == Affinity::Right
                                    );
                                    assert_eq!(
                                        em.end_right_gravity,
                                        exp.end_affinity == Affinity::Right
                                    );
                                }
                                (HostFormat::Agent(kind), HostAnnotation::Agent(ag)) => {
                                    let exp_start = match kind {
                                        AgentCoordKind::Byte => {
                                            AgentPosition::Byte(BytePos::new(exp.start))
                                        }
                                        AgentCoordKind::LineColUtf16 => {
                                            AgentPosition::LineColUtf16(ref_byte_to_utf16(
                                                expected_text,
                                                exp.start,
                                            ))
                                        }
                                    };
                                    let exp_end = match kind {
                                        AgentCoordKind::Byte => {
                                            AgentPosition::Byte(BytePos::new(exp.end))
                                        }
                                        AgentCoordKind::LineColUtf16 => {
                                            AgentPosition::LineColUtf16(ref_byte_to_utf16(
                                                expected_text,
                                                exp.end,
                                            ))
                                        }
                                    };
                                    assert_eq!(ag.start, exp_start);
                                    assert_eq!(ag.end, exp_end);
                                }
                                _ => panic!("host annotation format mismatch"),
                            }
                        }
                    }
                } else {
                    let exp_err = if revoked {
                        CoreErrorKind::RevokedActor
                    } else if !active {
                        CoreErrorKind::UnknownActor
                    } else {
                        CoreErrorKind::PermissionDenied
                    };
                    assert_eq!(core.read_emacs(actor, buf).unwrap_err().kind(), exp_err);
                    assert_eq!(core.read_neovim(actor, buf).unwrap_err().kind(), exp_err);
                    assert_eq!(core.read_agent(actor, buf).unwrap_err().kind(), exp_err);
                }
            }
        }

        for (&req, exp_prop) in &model.proposals {
            let actual = core.proposal_info(req).unwrap().expect("tracked proposal");
            assert_eq!(actual.proposer, exp_prop.proposer);
            assert_eq!(actual.txn, exp_prop.txn);
            assert_eq!(actual.base_revision, exp_prop.base_revision);
            assert_eq!(actual.buffers, exp_prop.buffers);
            assert_eq!(actual.status, exp_prop.status);
        }

        for (&cid, exp_client) in &model.clients {
            if model.has_cap(exp_client.actor, exp_client.buffer, Capability::Read) {
                assert_eq!(core.client_revision(cid).unwrap(), exp_client.revision);
                if exp_client.closed {
                    assert_eq!(
                        core.client_snapshot(cid).unwrap_err().kind(),
                        CoreErrorKind::BufferClosed
                    );
                } else {
                    let snap = core.client_snapshot(cid).unwrap();
                    let decoded = snap.decode().unwrap();
                    assert_eq!(decoded.revision, exp_client.revision);
                    assert_eq!(decoded.text.as_str(), exp_client.text);
                    if let Some(local) = local_client_snaps.get(&cid) {
                        assert_eq!(local, &snap, "local host-replayed snapshot drifted");
                    }
                }
            }
        }
    }

    /// Normalized snapshot of one host instance for cross-host equivalence assertions.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct HostEquivView {
        revision: Revision,
        floor: Revision,
        live_buffers: BTreeMap<BufferId, String>,
        tombstones: BTreeSet<BufferId>,
        views: BTreeMap<(ActorId, BufferId), (usize, usize)>,
        selections: BTreeMap<(ActorId, BufferId), (usize, usize, Direction)>,
        annotations: BTreeMap<(ActorId, AnnotationKey), RefAnnotation>,
        search_hits: Vec<(Revision, BufferId, usize, usize)>,
        context_pack: Option<ContextPack>,
        change_groups: Vec<CommitReceipt>,
        rejections: Vec<CoreErrorKind>,
    }

    struct EquivQuery<'a> {
        actors: &'a [ActorId],
        ann_keys: &'a [(ActorId, AnnotationKey)],
        search_actor: ActorId,
        search_pattern: &'a str,
        context_actor: ActorId,
    }

    fn extract_host_equiv(
        core: &EditorCore,
        format: HostFormat,
        query: &EquivQuery<'_>,
        rejections: Vec<CoreErrorKind>,
    ) -> HostEquivView {
        let EquivQuery {
            actors,
            ann_keys,
            search_actor,
            search_pattern,
            context_actor,
        } = *query;
        let mut live_buffers = BTreeMap::new();
        for buf in core.live_buffers().unwrap() {
            // Find any active actor with Read permission to read the buffer in `format`.
            if let Some(&reader) = actors
                .iter()
                .find(|&&a| core.has_capability(a, buf, Capability::Read).unwrap())
            {
                let snap = core.read_snapshot(reader, buf, format).unwrap();
                let decoded = snap.decode().unwrap();
                assert_eq!(decoded.revision, core.revision().unwrap());
                live_buffers.insert(buf, decoded.text.as_str().to_owned());
            }
        }
        let mut views = BTreeMap::new();
        let mut selections = BTreeMap::new();
        for &a in actors {
            for &b in live_buffers.keys() {
                if core.has_capability(a, b, Capability::Read).unwrap() {
                    if let Some(v) = core.view(a, b, format).unwrap() {
                        views.insert((a, b), (v.start_byte, v.end_byte));
                    }
                    if let Some(s) = core.selection(a, b, format).unwrap() {
                        selections.insert((a, b), (s.anchor_byte, s.head_byte, s.direction));
                    }
                }
            }
        }
        let mut annotations = BTreeMap::new();
        for &(a, key) in ann_keys {
            if live_buffers.contains_key(&key.buffer)
                && core
                    .has_capability(a, key.buffer, Capability::Read)
                    .unwrap()
            {
                if let Some(ann) = core.annotation(a, key, format).unwrap() {
                    let text = &live_buffers[&key.buffer];
                    let (start, end, sa, ea) = match &ann {
                        HostAnnotation::Emacs(ov) => {
                            let s = text
                                .char_indices()
                                .map(|(i, _)| i)
                                .chain(std::iter::once(text.len()))
                                .nth(ov.start.zero_based_scalar())
                                .unwrap();
                            let e = text
                                .char_indices()
                                .map(|(i, _)| i)
                                .chain(std::iter::once(text.len()))
                                .nth(ov.end.zero_based_scalar())
                                .unwrap();
                            (
                                s,
                                e,
                                if ov.front_advance {
                                    Affinity::Right
                                } else {
                                    Affinity::Left
                                },
                                if ov.rear_advance {
                                    Affinity::Right
                                } else {
                                    Affinity::Left
                                },
                            )
                        }
                        HostAnnotation::Neovim(em) => {
                            let to_byte = |pos: LineColByte| {
                                let mut off = 0usize;
                                for (idx, line) in text.split('\n').enumerate() {
                                    if idx == pos.line() {
                                        return off + pos.col_byte();
                                    }
                                    off += line.len() + 1;
                                }
                                panic!("line out of range");
                            };
                            (
                                to_byte(em.start),
                                to_byte(em.end),
                                if em.right_gravity {
                                    Affinity::Right
                                } else {
                                    Affinity::Left
                                },
                                if em.end_right_gravity {
                                    Affinity::Right
                                } else {
                                    Affinity::Left
                                },
                            )
                        }
                        HostAnnotation::Agent(ag) => {
                            let to_byte = |pos: AgentPosition| match pos {
                                AgentPosition::Byte(p) => p.raw(),
                                AgentPosition::LineColUtf16(p) => {
                                    let mut off = 0usize;
                                    for (idx, line) in text.split('\n').enumerate() {
                                        if idx == p.line() {
                                            let mut units = 0;
                                            for (rel, ch) in line.char_indices() {
                                                if units == p.col_utf16() {
                                                    return off + rel;
                                                }
                                                units += ch.len_utf16();
                                            }
                                            assert_eq!(units, p.col_utf16());
                                            return off + line.len();
                                        }
                                        off += line.len() + 1;
                                    }
                                    panic!("line out of range");
                                }
                            };
                            (
                                to_byte(ag.start),
                                to_byte(ag.end),
                                ag.start_affinity,
                                ag.end_affinity,
                            )
                        }
                    };
                    annotations.insert(
                        (a, key),
                        RefAnnotation {
                            start,
                            end,
                            start_affinity: sa,
                            end_affinity: ea,
                            priority: ann.metadata().priority,
                            collapse: ann.metadata().collapse,
                            label: ann.metadata().label.clone(),
                        },
                    );
                }
            }
        }
        let search_hits = core
            .search_workspace(search_actor, search_pattern, format)
            .unwrap()
            .into_iter()
            .map(|h| (h.revision, h.buffer, h.start_byte, h.end_byte))
            .collect();
        let context_pack = core
            .extract_context(
                context_actor,
                &ContextScope::all_readable(),
                &ContextRequest::workspace(ContextBudget::new(256).unwrap()),
                format,
            )
            .ok()
            .map(|hp| hp.pack);
        HostEquivView {
            revision: core.revision().unwrap(),
            floor: core.floor().unwrap(),
            live_buffers,
            tombstones: core.tombstoned_buffers().unwrap().into_iter().collect(),
            views,
            selections,
            annotations,
            search_hits,
            context_pack,
            change_groups: core
                .change_groups()
                .unwrap()
                .into_iter()
                .map(|g| g.receipt)
                .collect(),
            rejections,
        }
    }

    fn assert_hosts_equivalent(views: &[(HostFormat, HostEquivView)]) {
        assert!(views.len() >= 2);
        let (base_fmt, base) = &views[0];
        for (fmt, other) in &views[1..] {
            assert_eq!(
                other.revision, base.revision,
                "revision diverged between {base_fmt} and {fmt}"
            );
            assert_eq!(
                other.floor, base.floor,
                "floor diverged between {base_fmt} and {fmt}"
            );
            assert_eq!(
                other.live_buffers, base.live_buffers,
                "buffer text diverged between {base_fmt} and {fmt}"
            );
            assert_eq!(
                other.tombstones, base.tombstones,
                "tombstones diverged between {base_fmt} and {fmt}"
            );
            assert_eq!(
                other.views, base.views,
                "views diverged between {base_fmt} and {fmt}"
            );
            assert_eq!(
                other.selections, base.selections,
                "selections diverged between {base_fmt} and {fmt}"
            );
            assert_eq!(
                other.annotations, base.annotations,
                "annotations diverged between {base_fmt} and {fmt}"
            );
            assert_eq!(
                other.search_hits, base.search_hits,
                "search hits diverged between {base_fmt} and {fmt}"
            );
            assert_eq!(
                other.context_pack, base.context_pack,
                "context pack diverged between {base_fmt} and {fmt}"
            );
            assert_eq!(
                other.change_groups, base.change_groups,
                "change groups diverged between {base_fmt} and {fmt}"
            );
            assert_eq!(
                other.rejections, base.rejections,
                "rejection reasons diverged between {base_fmt} and {fmt}"
            );
        }
    }

    fn build_host_annotation(
        format: HostFormat,
        text: &str,
        key: AnnotationKey,
        owner: ActorId,
        spec: &RefAnnotation,
    ) -> HostAnnotation {
        let metadata = AnnotationMetadata {
            key,
            owner,
            priority: spec.priority,
            collapse: spec.collapse,
            label: spec.label.clone(),
        };
        match format {
            HostFormat::Emacs => HostAnnotation::Emacs(EmacsOverlay {
                start: ref_byte_to_emacs(text, spec.start),
                end: ref_byte_to_emacs(text, spec.end),
                front_advance: spec.start_affinity == Affinity::Right,
                rear_advance: spec.end_affinity == Affinity::Right,
                metadata,
            }),
            HostFormat::Neovim => HostAnnotation::Neovim(NeovimExtmark {
                start: ref_byte_to_neovim(text, spec.start),
                end: ref_byte_to_neovim(text, spec.end),
                right_gravity: spec.start_affinity == Affinity::Right,
                end_right_gravity: spec.end_affinity == Affinity::Right,
                metadata,
            }),
            HostFormat::Agent(kind) => HostAnnotation::Agent(AgentAnnotation {
                start: match kind {
                    AgentCoordKind::Byte => AgentPosition::Byte(BytePos::new(spec.start)),
                    AgentCoordKind::LineColUtf16 => {
                        AgentPosition::LineColUtf16(ref_byte_to_utf16(text, spec.start))
                    }
                },
                end: match kind {
                    AgentCoordKind::Byte => AgentPosition::Byte(BytePos::new(spec.end)),
                    AgentCoordKind::LineColUtf16 => {
                        AgentPosition::LineColUtf16(ref_byte_to_utf16(text, spec.end))
                    }
                },
                start_affinity: spec.start_affinity,
                end_affinity: spec.end_affinity,
                metadata,
            }),
        }
    }

    fn build_replace_commands(
        format: HostFormat,
        text: &str,
        start: usize,
        end: usize,
        replacement: &str,
    ) -> HostCommands {
        let repl = Text::from_str(replacement);
        match format {
            HostFormat::Emacs => HostCommands::Emacs(vec![
                EmacsCommand::Mark(ref_byte_to_emacs(text, start)),
                EmacsCommand::Point(ref_byte_to_emacs(text, end)),
                EmacsCommand::ReplaceRegion(repl),
            ]),
            HostFormat::Neovim => HostCommands::Neovim(vec![NeovimCommand::SetText {
                start: ref_byte_to_neovim(text, start),
                end: ref_byte_to_neovim(text, end),
                text: repl,
            }]),
            HostFormat::Agent(kind) => {
                let s = match kind {
                    AgentCoordKind::Byte => AgentPosition::Byte(BytePos::new(start)),
                    AgentCoordKind::LineColUtf16 => {
                        AgentPosition::LineColUtf16(ref_byte_to_utf16(text, start))
                    }
                };
                let e = match kind {
                    AgentCoordKind::Byte => AgentPosition::Byte(BytePos::new(end)),
                    AgentCoordKind::LineColUtf16 => {
                        AgentPosition::LineColUtf16(ref_byte_to_utf16(text, end))
                    }
                };
                HostCommands::Agent(vec![AgentCommand::Replace {
                    start: s,
                    end: e,
                    text: repl,
                }])
            }
        }
    }

    // ======================================================================
    // Test 1: Complete End-to-End Round-Trip Across All Host Formats
    // read -> propose -> approve/reject -> commit -> deliver -> undo -> redo
    // -> compact -> reconnect (deltas & snapshot) -> checkpoint/restore
    // ======================================================================

    #[test]
    fn end_to_end_round_trip_across_all_three_hosts() {
        let human = ActorId::new(1);
        let agent = ActorId::new(2);
        let actors = [human, agent];
        let mut host_views = Vec::new();

        for format in ALL_FORMATS {
            let core = EditorCore::new();
            let mut model = RefModel::default();
            let mut local_snaps: BTreeMap<ClientId, HostSnapshot> = BTreeMap::new();
            let mut rejections = Vec::new();

            // Register Human and Agent actors and grant capabilities.
            core.register_actor(human, ActorRole::Human).unwrap();
            model.register_actor(human, ActorRole::Human);
            core.register_actor(agent, ActorRole::Agent).unwrap();
            model.register_actor(agent, ActorRole::Agent);
            for cap in Capability::ALL {
                core.grant_default(cap).unwrap();
                model.apply_access(AccessChange::GrantDefault(cap));
            }

            // Open two buffers with multi-byte UTF-8 and CRLF content.
            let initial_a = "alpha 界😀\r\nbeta e\u{0301}\n";
            let open_a = core
                .open_buffer(&OpenBufferRequest {
                    request: RequestId::new(1),
                    actor: human,
                    nonce: Nonce::new(1),
                    base: Revision::ZERO,
                    name: "main.txt".into(),
                    text: Text::from_str(initial_a),
                })
                .unwrap();
            let buf_a = model.open_buffer(
                RequestId::new(1),
                human,
                Nonce::new(1),
                "main.txt",
                initial_a,
            );
            assert_eq!(open_a.buffer, buf_a);

            let initial_b = "notes alpha\n";
            let open_b = core
                .open_buffer(&OpenBufferRequest {
                    request: RequestId::new(2),
                    actor: human,
                    nonce: Nonce::new(2),
                    base: Revision::new(1),
                    name: "notes.txt".into(),
                    text: Text::from_str(initial_b),
                })
                .unwrap();
            let buf_b = model.open_buffer(
                RequestId::new(2),
                human,
                Nonce::new(2),
                "notes.txt",
                initial_b,
            );
            assert_eq!(open_b.buffer, buf_b);

            // Subscribe an active client and a lagging client on buf_a.
            let (active_client, active_init) = core.subscribe(human, buf_a, format).unwrap();
            let (lagging_client, lagging_init) = core.subscribe(agent, buf_a, format).unwrap();
            assert_eq!(active_init, lagging_init);
            local_snaps.insert(active_client, active_init);
            local_snaps.insert(lagging_client, lagging_init);
            model.clients.insert(
                active_client,
                RefClient {
                    actor: human,
                    buffer: buf_a,
                    format,
                    revision: Revision::new(2),
                    text: initial_a.to_owned(),
                    closed: false,
                },
            );
            model.clients.insert(
                lagging_client,
                RefClient {
                    actor: agent,
                    buffer: buf_a,
                    format,
                    revision: Revision::new(2),
                    text: initial_a.to_owned(),
                    closed: false,
                },
            );

            // 1. READ in native host format + set view, selection, and annotation.
            let read_snap = core.read_snapshot(agent, buf_a, format).unwrap();
            assert_eq!(read_snap.decode().unwrap().text.as_str(), initial_a);

            let view_region = ref_region(
                initial_a,
                0,
                initial_a.len(),
                EndpointMode::Exclusive,
                format,
            );
            let narrowed = core.narrow_view(human, buf_a, view_region).unwrap();
            assert_eq!(
                (narrowed.start_byte, narrowed.end_byte),
                (0, initial_a.len())
            );
            model.set_view(human, buf_a, 0, initial_a.len());

            // Select "界😀" (bytes 6..13).
            let sel_region = ref_region(initial_a, 6, 13, EndpointMode::Exclusive, format);
            let sel = core.set_selection(human, buf_a, sel_region).unwrap();
            assert_eq!(
                (sel.anchor_byte, sel.head_byte, sel.direction),
                (6, 13, Direction::Forward)
            );
            model.set_selection(human, buf_a, 6, 13);

            let ann_key = AnnotationKey {
                buffer: buf_a,
                namespace: NamespaceId::new(1),
                id: AnnotationId::new(7),
            };
            let ann_spec = RefAnnotation {
                start: 6,
                end: 13,
                start_affinity: Affinity::Left,
                end_affinity: Affinity::Right,
                priority: 42,
                collapse: CollapsePolicy::RetainEmpty,
                label: "highlight".into(),
            };
            let ann = build_host_annotation(format, initial_a, ann_key, human, &ann_spec);
            core.set_annotation(human, &ann).unwrap();
            model.set_annotation(human, ann_key, ann_spec);

            // 2. PROPOSE + REJECT_PROPOSAL path (and verify rejected proposal stays dead).
            let rej_req = RequestId::new(10);
            let rej_edit = EditSpec {
                buffer: buf_a,
                region: ref_region(initial_a, 0, 5, EndpointMode::Exclusive, format),
                replacement: Text::from_str("REJECTED"),
            };
            core.propose(&ProposalRequest {
                request: rej_req,
                actor: agent,
                nonce: Nonce::new(10),
                base: Revision::new(2),
                payload: ProposalPayload::Edit(rej_edit),
            })
            .unwrap();
            model.proposals.insert(
                rej_req,
                RefProposal {
                    proposer: agent,
                    txn: TxnId::new(agent, Nonce::new(10)),
                    base_revision: Revision::new(2),
                    buffers: vec![buf_a],
                    status: ProposalStatus::Pending,
                },
            );
            // Agent cannot reject a proposal (must be human).
            let err_agent_rej = core
                .reject_proposal(rej_req, agent, Some("nope".into()))
                .unwrap_err()
                .kind();
            assert_eq!(err_agent_rej, CoreErrorKind::NotHumanReviewer);
            rejections.push(err_agent_rej);

            core.reject_proposal(rej_req, human, Some("out of scope".into()))
                .unwrap();
            model.proposals.get_mut(&rej_req).unwrap().status = ProposalStatus::Rejected {
                reviewer: human,
                reason: Some("out of scope".into()),
            };
            // Approving or committing the rejected proposal fails atomically.
            let err_approve_rej = core.approve_proposal(rej_req, human).unwrap_err().kind();
            assert_eq!(err_approve_rej, CoreErrorKind::ProposalRejected);
            rejections.push(err_approve_rej);
            let err_commit_rej = core.commit_proposal(rej_req).unwrap_err().kind();
            assert_eq!(err_commit_rej, CoreErrorKind::ProposalRejected);
            rejections.push(err_commit_rej);

            // 3. PROPOSE -> APPROVE -> COMMIT (using native host command payload!).
            let prop_req = RequestId::new(11);
            let prop_txn = TxnId::new(agent, Nonce::new(11));
            core.propose(&ProposalRequest {
                request: prop_req,
                actor: agent,
                nonce: Nonce::new(11),
                base: Revision::new(2),
                payload: ProposalPayload::Commands {
                    buffer: buf_a,
                    commands: build_replace_commands(format, initial_a, 0, 5, "ALPHA"),
                },
            })
            .unwrap();
            model.proposals.insert(
                prop_req,
                RefProposal {
                    proposer: agent,
                    txn: prop_txn,
                    base_revision: Revision::new(2),
                    buffers: vec![buf_a],
                    status: ProposalStatus::Pending,
                },
            );

            // Commit before approval fails atomically.
            let err_unapproved = core.commit_proposal(prop_req).unwrap_err().kind();
            assert_eq!(err_unapproved, CoreErrorKind::UnapprovedProposal);
            rejections.push(err_unapproved);

            core.approve_proposal(prop_req, human).unwrap();
            let mut reviewers = BTreeSet::new();
            reviewers.insert(human);
            model.proposals.get_mut(&prop_req).unwrap().status = ProposalStatus::Approved {
                reviewers,
                approved_revision: Revision::new(2),
            };

            let committed = core.commit_proposal(prop_req).unwrap();
            let expected_receipt = CommitReceipt::new(
                EditIntent::for_actor(agent, Nonce::new(11), Revision::new(2)),
                Revision::new(2),
            )
            .unwrap();
            assert_eq!(committed.receipt, expected_receipt);
            assert_eq!(committed.receipt.intent().base(), Revision::new(2));
            assert_eq!(committed.receipt.committed(), Revision::new(3));
            model.apply_splices(
                prop_req,
                agent,
                Nonce::new(11),
                &[(buf_a, vec![(0, 5, "ALPHA")])],
            );
            model.proposals.get_mut(&prop_req).unwrap().status = ProposalStatus::Committed {
                receipt: expected_receipt,
            };

            // 4. DELIVER incremental delta to active client and apply to local HostSnapshot.
            let delivery = core.deliver_incremental(active_client).unwrap();
            assert_eq!(delivery.previous_revision, Revision::new(2));
            assert_eq!(delivery.revision, Revision::new(3));
            assert_eq!(delivery.notifications.len(), 1);
            assert!(matches!(
                delivery.notifications[0],
                HostNotification::Change(_)
            ));
            assert!(local_snaps
                .get_mut(&active_client)
                .unwrap()
                .apply_delivery(&delivery)
                .unwrap());
            model.clients.get_mut(&active_client).unwrap().revision = Revision::new(3);
            model.clients.get_mut(&active_client).unwrap().text =
                model.live_buffers[&buf_a].clone();

            assert_core_matches_model(&core, &model, &actors, &local_snaps);

            // 5. UNDO across native format + DELIVER.
            let undo_res = match format {
                HostFormat::Emacs => core
                    .emacs_undo(
                        RequestId::new(12),
                        agent,
                        Nonce::new(12),
                        UndoTarget::Resolved(&[prop_txn]),
                    )
                    .unwrap(),
                HostFormat::Neovim => core
                    .neovim_undo(
                        RequestId::new(12),
                        agent,
                        Nonce::new(12),
                        UndoTarget::Resolved(&[prop_txn]),
                    )
                    .unwrap(),
                HostFormat::Agent(_) => core
                    .agent_undo(RequestId::new(12), agent, Nonce::new(12), prop_txn)
                    .unwrap(),
            };
            assert_eq!(undo_res.receipt.committed(), Revision::new(4));
            model.apply_splices(
                RequestId::new(12),
                agent,
                Nonce::new(12),
                &[(buf_a, vec![(0, 5, "alpha")])],
            );

            let del_undo = core.deliver_incremental(active_client).unwrap();
            assert!(local_snaps
                .get_mut(&active_client)
                .unwrap()
                .apply_delivery(&del_undo)
                .unwrap());
            model.clients.get_mut(&active_client).unwrap().revision = Revision::new(4);
            model.clients.get_mut(&active_client).unwrap().text =
                model.live_buffers[&buf_a].clone();
            assert_core_matches_model(&core, &model, &actors, &local_snaps);

            // Subscribe a mid-stream client at rev 4 so we can test reconnect_deltas after compact(4)!
            let (delta_client, delta_init) = core.subscribe(human, buf_a, format).unwrap();
            local_snaps.insert(delta_client, delta_init);
            model.clients.insert(
                delta_client,
                RefClient {
                    actor: human,
                    buffer: buf_a,
                    format,
                    revision: Revision::new(4),
                    text: model.live_buffers[&buf_a].clone(),
                    closed: false,
                },
            );

            // 6. REDO across native format + edit to unrelated buf_b (silent revision for buf_a).
            let redo_res = match format {
                HostFormat::Emacs => core
                    .emacs_redo(
                        RequestId::new(13),
                        agent,
                        Nonce::new(13),
                        UndoTarget::Explicit(prop_txn),
                    )
                    .unwrap(),
                HostFormat::Neovim => core
                    .neovim_redo(
                        RequestId::new(13),
                        agent,
                        Nonce::new(13),
                        UndoTarget::Explicit(prop_txn),
                    )
                    .unwrap(),
                HostFormat::Agent(_) => core
                    .agent_redo(RequestId::new(13), agent, Nonce::new(13), prop_txn)
                    .unwrap(),
            };
            assert_eq!(redo_res.receipt.committed(), Revision::new(5));
            model.apply_splices(
                RequestId::new(13),
                agent,
                Nonce::new(13),
                &[(buf_a, vec![(0, 5, "ALPHA")])],
            );

            // Edit buf_b so buf_a clients also traverse a silent revision advancement.
            let text_b = model.live_buffers[&buf_b].clone();
            core.apply_edit(&EditRequest {
                request: RequestId::new(14),
                actor: human,
                nonce: Nonce::new(14),
                base: Revision::new(5),
                edit: EditSpec {
                    buffer: buf_b,
                    region: ref_region(&text_b, 6, 11, EndpointMode::Exclusive, format),
                    replacement: Text::from_str("ALPHA"),
                },
                sequence_step: None,
            })
            .unwrap();
            model.apply_splices(
                RequestId::new(14),
                human,
                Nonce::new(14),
                &[(buf_b, vec![(6, 11, "ALPHA")])],
            );

            // 7. COMPACT to revision 4 (lagging_client at rev 2 expires; delta_client at rev 4 stays retained!).
            let new_floor = core.compact(Revision::new(4)).unwrap();
            assert_eq!(new_floor, Revision::new(4));
            model.compact(4);

            // Lagging client cannot use deliver_incremental (ResyncRequired, strictly atomic).
            let err_resync = core.deliver_incremental(lagging_client).unwrap_err().kind();
            assert_eq!(err_resync, CoreErrorKind::ResyncRequired);
            rejections.push(err_resync);
            assert_eq!(
                core.client_revision(lagging_client).unwrap(),
                Revision::new(2)
            );

            // 8a. RECONNECT via incremental deltas for delta_client (at rev 4 == floor).
            let rec_deltas = core.reconnect(delta_client).unwrap();
            assert!(matches!(rec_deltas, ReconnectOutcome::Deltas(_)));
            assert!(local_snaps
                .get_mut(&delta_client)
                .unwrap()
                .apply_reconnect(&rec_deltas)
                .unwrap());
            model.clients.get_mut(&delta_client).unwrap().revision = Revision::new(6);
            model.clients.get_mut(&delta_client).unwrap().text = model.live_buffers[&buf_a].clone();

            // 8b. RECONNECT via post-compaction snapshot recovery for lagging_client (at rev 2 < floor).
            let rec_snap = core.reconnect(lagging_client).unwrap();
            assert!(matches!(rec_snap, ReconnectOutcome::Snapshot { .. }));
            assert!(local_snaps
                .get_mut(&lagging_client)
                .unwrap()
                .apply_reconnect(&rec_snap)
                .unwrap());
            model.clients.get_mut(&lagging_client).unwrap().revision = Revision::new(6);
            model.clients.get_mut(&lagging_client).unwrap().text =
                model.live_buffers[&buf_a].clone();

            // Active client catches up incrementally from 4 -> 6 too.
            let del_final = core.deliver_incremental(active_client).unwrap();
            assert!(local_snaps
                .get_mut(&active_client)
                .unwrap()
                .apply_delivery(&del_final)
                .unwrap());
            model.clients.get_mut(&active_client).unwrap().revision = Revision::new(6);
            model.clients.get_mut(&active_client).unwrap().text =
                model.live_buffers[&buf_a].clone();

            assert_core_matches_model(&core, &model, &actors, &local_snaps);

            // 9. Post-reconnect incremental edit works seamlessly on previously compacted client!
            let cur_a = model.live_buffers[&buf_a].clone();
            let end_a = cur_a.len();
            core.apply_edit(&EditRequest {
                request: RequestId::new(15),
                actor: human,
                nonce: Nonce::new(15),
                base: Revision::new(6),
                edit: EditSpec {
                    buffer: buf_a,
                    region: ref_region(&cur_a, end_a, end_a, EndpointMode::Exclusive, format),
                    replacement: Text::from_str("tail界\n"),
                },
                sequence_step: None,
            })
            .unwrap();
            model.apply_splices(
                RequestId::new(15),
                human,
                Nonce::new(15),
                &[(buf_a, vec![(end_a, end_a, "tail界\n")])],
            );
            for cid in [active_client, delta_client, lagging_client] {
                let d = core.deliver_incremental(cid).unwrap();
                assert!(local_snaps
                    .get_mut(&cid)
                    .unwrap()
                    .apply_delivery(&d)
                    .unwrap());
                model.clients.get_mut(&cid).unwrap().revision = Revision::new(7);
                model.clients.get_mut(&cid).unwrap().text = model.live_buffers[&buf_a].clone();
            }
            assert_core_matches_model(&core, &model, &actors, &local_snaps);

            // 10. Checkpoint & Restore preserves every observable state, including rejected proposals!
            let cp = core.checkpoint().unwrap();
            let restored = EditorCore::restore(&cp).unwrap();
            assert_core_matches_model(&restored, &model, &actors, &local_snaps);
            assert_eq!(
                restored
                    .approve_proposal(rej_req, human)
                    .unwrap_err()
                    .kind(),
                CoreErrorKind::ProposalRejected
            );

            // 11. Verify close_buffer and revoke_actor against the reference model on `core`.
            core.close_buffer(&CloseBufferRequest {
                request: RequestId::new(16),
                actor: human,
                nonce: Nonce::new(16),
                base: Revision::new(7),
                buffer: buf_b,
                policy: ClosePolicy::Discard,
            })
            .unwrap();
            model.close_buffer(RequestId::new(16), human, Nonce::new(16), buf_b);
            core.revoke_actor(agent).unwrap();
            model.revoke_actor(agent);
            assert_core_matches_model(&core, &model, &actors, &local_snaps);

            host_views.push((
                format,
                extract_host_equiv(
                    &restored,
                    format,
                    &EquivQuery {
                        actors: &actors,
                        ann_keys: &[(human, ann_key)],
                        search_actor: agent,
                        search_pattern: "ALPHA",
                        context_actor: agent,
                    },
                    rejections,
                ),
            ));
        }

        assert_hosts_equivalent(&host_views);
    }

    // ======================================================================
    // Test 2: Deterministic Corner Cases (UTF-8, LF/CRLF, Batches, Change
    // Groups, Rectangle, Indent/Dedent, Navigation, Search, Context)
    // ======================================================================

    #[test]
    fn corner_cases_utf8_line_endings_rectangle_indent_navigation_search_context() {
        let human = ActorId::new(1);
        let agent = ActorId::new(2);

        // A. Empty buffer, LF, CRLF, standalone CR, CJK, emoji, combining marks across all formats.
        let corner_texts = [
            "",
            "\n",
            "\n\n",
            "\r",
            "a\r\nb\r\n",
            "日本語界",
            "e\u{0301}😀🚀",
            "a\r\n界e\u{0301}\n😀\r",
        ];
        for &source in &corner_texts {
            let core = EditorCore::new();
            core.register_actor(human, ActorRole::Human).unwrap();
            core.grant_default(Capability::Read).unwrap();
            core.grant_default(Capability::Commit).unwrap();
            let opened = core
                .open_buffer(&OpenBufferRequest {
                    request: RequestId::new(1),
                    actor: human,
                    nonce: Nonce::new(1),
                    base: Revision::ZERO,
                    name: "corner".into(),
                    text: Text::from_str(source),
                })
                .unwrap();
            let em = core.read_emacs(human, opened.buffer).unwrap();
            let nv = core.read_neovim(human, opened.buffer).unwrap();
            let ag = core.read_agent(human, opened.buffer).unwrap();
            assert_eq!(em.text, source);
            assert_eq!(ag.text, source);
            let (exp_lines, exp_eol) = ref_split_neovim(source);
            assert_eq!(nv.lines, exp_lines);
            assert_eq!(nv.endofline, exp_eol);
            assert_eq!(
                HostSnapshot::Neovim(nv).decode().unwrap().text.as_str(),
                source
            );
        }

        // B. Multi-buffer atomic batch + Change Groups across Emacs, Neovim, Agent.
        let mut group_views = Vec::new();
        for format in ALL_FORMATS {
            let core = EditorCore::new();
            let mut model = RefModel::default();
            core.register_actor(human, ActorRole::Human).unwrap();
            model.register_actor(human, ActorRole::Human);
            for cap in Capability::ALL {
                core.grant_default(cap).unwrap();
                model.apply_access(AccessChange::GrantDefault(cap));
            }
            let b0 = core
                .open_buffer(&OpenBufferRequest {
                    request: RequestId::new(1),
                    actor: human,
                    nonce: Nonce::new(1),
                    base: Revision::ZERO,
                    name: "b0".into(),
                    text: Text::from_str("a😀z\r\n界"),
                })
                .unwrap()
                .buffer;
            model.open_buffer(RequestId::new(1), human, Nonce::new(1), "b0", "a😀z\r\n界");
            let b1 = core
                .open_buffer(&OpenBufferRequest {
                    request: RequestId::new(2),
                    actor: human,
                    nonce: Nonce::new(2),
                    base: Revision::new(1),
                    name: "b1".into(),
                    text: Text::from_str("e\u{0301}bc"),
                })
                .unwrap()
                .buffer;
            model.open_buffer(RequestId::new(2), human, Nonce::new(2), "b1", "e\u{0301}bc");

            // Multi-buffer batch with Inclusive endpoint mode on b0 ("😀", bytes 1..5) and Exclusive on b1 ("b", 3..4).
            let t0 = model.live_buffers[&b0].clone();
            let t1 = model.live_buffers[&b1].clone();
            core.apply_multi_buffer_batch(&MultiBufferBatchRequest {
                request: RequestId::new(3),
                actor: human,
                nonce: Nonce::new(3),
                base: Revision::new(2),
                primary: b0,
                edits: vec![
                    EditSpec {
                        buffer: b0,
                        region: ref_region(&t0, 1, 5, EndpointMode::Inclusive, format),
                        replacement: Text::from_str("🚀"),
                    },
                    EditSpec {
                        buffer: b1,
                        region: ref_region(&t1, 3, 4, EndpointMode::Exclusive, format),
                        replacement: Text::from_str("B"),
                    },
                ],
                sequence_step: Some(SequenceStep {
                    sequence: TxnId::new(human, Nonce::new(50)),
                    index: 0,
                }),
            })
            .unwrap();
            model.apply_splices(
                RequestId::new(3),
                human,
                Nonce::new(3),
                &[(b0, vec![(1, 5, "🚀")]), (b1, vec![(3, 4, "B")])],
            );

            // Change group: two sequential edits collapsed into one commit, plus post-edit selection!
            let cur_b0 = model.live_buffers[&b0].clone();
            let cmds = match format {
                HostFormat::Emacs => HostCommands::Emacs(vec![
                    EmacsCommand::BeginChangeGroup,
                    EmacsCommand::Point(ref_byte_to_emacs(&cur_b0, 0)),
                    EmacsCommand::Insert(Text::from_str("PRE:")),
                    // After inserting "PRE:" (4 ASCII bytes), offset 4 + 1 = 5 is start of "🚀".
                    EmacsCommand::Mark(EmacsCharPos::new(6).unwrap()),
                    EmacsCommand::Point(EmacsCharPos::new(7).unwrap()),
                    EmacsCommand::ReplaceRegion(Text::from_str("界")),
                    EmacsCommand::EndChangeGroup,
                    EmacsCommand::Mark(EmacsCharPos::new(1).unwrap()),
                    EmacsCommand::Point(EmacsCharPos::new(5).unwrap()),
                    EmacsCommand::SelectRegion,
                ]),
                HostFormat::Neovim => HostCommands::Neovim(vec![
                    NeovimCommand::BeginUndoBlock,
                    NeovimCommand::Cursor(LineColByte::new(0, 0)),
                    NeovimCommand::Insert(Text::from_str("PRE:")),
                    NeovimCommand::SetText {
                        start: LineColByte::new(0, 5),
                        end: LineColByte::new(0, 9),
                        text: Text::from_str("界"),
                    },
                    NeovimCommand::EndUndoBlock,
                    NeovimCommand::Select {
                        anchor: LineColByte::new(0, 0),
                        head: LineColByte::new(0, 4),
                    },
                ]),
                HostFormat::Agent(kind) => {
                    let pos = |after_pre: bool, byte: usize| match kind {
                        AgentCoordKind::Byte => AgentPosition::Byte(BytePos::new(byte)),
                        AgentCoordKind::LineColUtf16 => {
                            let s = if after_pre {
                                format!("PRE:{cur_b0}")
                            } else {
                                cur_b0.clone()
                            };
                            AgentPosition::LineColUtf16(ref_byte_to_utf16(&s, byte))
                        }
                    };
                    HostCommands::Agent(vec![
                        AgentCommand::BeginBatch,
                        AgentCommand::Replace {
                            start: pos(false, 0),
                            end: pos(false, 0),
                            text: Text::from_str("PRE:"),
                        },
                        AgentCommand::Replace {
                            start: pos(true, 5),
                            end: pos(true, 9),
                            text: Text::from_str("界"),
                        },
                        AgentCommand::EndBatch,
                        AgentCommand::Select {
                            anchor: pos(true, 0),
                            head: pos(true, 4),
                        },
                    ])
                }
            };
            let cmd_out = core
                .execute_commands(&CommandRequest {
                    request: RequestId::new(4),
                    actor: human,
                    nonce: Nonce::new(4),
                    base: Revision::new(3),
                    buffer: b0,
                    commands: cmds,
                    sequence_step: Some(SequenceStep {
                        sequence: TxnId::new(human, Nonce::new(50)),
                        index: 1,
                    }),
                })
                .unwrap();
            assert!(cmd_out.commit.is_some());
            assert_eq!(cmd_out.selection.unwrap().anchor_byte, 0);
            assert_eq!(cmd_out.selection.unwrap().head_byte, 4);
            // Composed splices in base coordinates: (0,0,"PRE:") and (1,5,"界").
            model.apply_splices(
                RequestId::new(4),
                human,
                Nonce::new(4),
                &[(b0, vec![(0, 0, "PRE:"), (1, 5, "界")])],
            );
            model.set_selection(human, b0, 0, 4);
            assert_core_matches_model(&core, &model, &[human], &BTreeMap::new());
            group_views.push((
                format,
                extract_host_equiv(
                    &core,
                    format,
                    &EquivQuery {
                        actors: &[human],
                        ann_keys: &[],
                        search_actor: human,
                        search_pattern: "界",
                        context_actor: human,
                    },
                    vec![],
                ),
            ));
        }
        assert_hosts_equivalent(&group_views);

        // C. Rectangle edits (Visual & HostPosition, Delete/Replace/Uniform/Insert, Clamp/Error) + Indent/Dedent.
        let core = EditorCore::new();
        core.register_actor(human, ActorRole::Human).unwrap();
        for cap in Capability::ALL {
            core.grant_default(cap).unwrap();
        }
        let rect_buf = core
            .open_buffer(&OpenBufferRequest {
                request: RequestId::new(100),
                actor: human,
                nonce: Nonce::new(100),
                base: Revision::ZERO,
                name: "rect.txt".into(),
                text: Text::from_str("abcd\r\nefgh\r\nij\r\n"),
            })
            .unwrap()
            .buffer;

        // ShortLinePolicy::Error rejects short line 2 atomically.
        assert_rejection_atomic(
            &core,
            &[human],
            &[rect_buf],
            &[],
            &[],
            CoreErrorKind::InvalidRectangle,
            || {
                core.apply_rectangle_edit(&RectangleEditRequest {
                    request: RequestId::new(101),
                    actor: human,
                    nonce: Nonce::new(101),
                    base: Revision::new(1),
                    buffer: rect_buf,
                    rectangle: RectangleSpec::Visual(Rectangle {
                        first: VisualPosition { line: 0, column: 1 },
                        second: VisualPosition { line: 2, column: 3 },
                        endpoint: Endpoint::Exclusive,
                        tab_width: 4,
                    }),
                    policy: ShortLinePolicy::Error,
                    operation: RectangleEditOp::Delete,
                    sequence_step: None,
                })
            },
        );

        // Rectangle Replace with Clamp across CRLF lines.
        let r1 = core
            .apply_rectangle_edit(&RectangleEditRequest {
                request: RequestId::new(102),
                actor: human,
                nonce: Nonce::new(102),
                base: Revision::new(1),
                buffer: rect_buf,
                rectangle: RectangleSpec::Visual(Rectangle {
                    first: VisualPosition { line: 0, column: 1 },
                    second: VisualPosition { line: 2, column: 3 },
                    endpoint: Endpoint::Exclusive,
                    tab_width: 4,
                }),
                policy: ShortLinePolicy::Clamp,
                operation: RectangleEditOp::Replace(vec!["X".into(), "Y".into(), "Z".into()]),
                sequence_step: None,
            })
            .unwrap();
        assert_eq!(
            core.read_agent(human, rect_buf).unwrap().text,
            "aXd\r\neYh\r\niZ\r\n"
        );

        // Rectangle Insert using host-native Emacs positions on lines 0..=1 at col 0
        // (strictly disjoint from r1's col 1..2 so selective undo succeeds cleanly).
        let cur_rect = core.read_agent(human, rect_buf).unwrap().text;
        core.apply_rectangle_edit(&RectangleEditRequest {
            request: RequestId::new(103),
            actor: human,
            nonce: Nonce::new(103),
            base: Revision::new(2),
            buffer: rect_buf,
            rectangle: RectangleSpec::Positions {
                first: ref_pos(&cur_rect, 0, HostFormat::Emacs),
                second: ref_pos(&cur_rect, 5, HostFormat::Emacs),
                endpoint: Endpoint::Exclusive,
                tab_width: 4,
            },
            policy: ShortLinePolicy::Clamp,
            operation: RectangleEditOp::Insert("!".into()),
            sequence_step: None,
        })
        .unwrap();
        assert_eq!(
            core.read_agent(human, rect_buf).unwrap().text,
            "!aXd\r\n!eYh\r\niZ\r\n"
        );

        // Undo both rectangle edits selectively in reverse.
        core.agent_undo(
            RequestId::new(104),
            human,
            Nonce::new(104),
            TxnId::new(human, Nonce::new(103)),
        )
        .unwrap();
        assert_eq!(
            core.read_agent(human, rect_buf).unwrap().text,
            "aXd\r\neYh\r\niZ\r\n"
        );
        core.agent_undo(
            RequestId::new(105),
            human,
            Nonce::new(105),
            r1.receipt.txn(),
        )
        .unwrap();
        assert_eq!(
            core.read_agent(human, rect_buf).unwrap().text,
            "abcd\r\nefgh\r\nij\r\n"
        );

        // Indent / Dedent: SetColumn -> IndentBy -> DedentBy -> TabsAndSpaces + Undo, checked against ref_indent_text!
        let ind_initial = "  foo\r\n\tbar\n界\n";
        let ind_buf = core
            .open_buffer(&OpenBufferRequest {
                request: RequestId::new(110),
                actor: human,
                nonce: Nonce::new(110),
                base: core.revision().unwrap(),
                name: "indent.txt".into(),
                text: Text::from_str(ind_initial),
            })
            .unwrap()
            .buffer;
        let indent_steps = [
            IndentSpec {
                start_line: 0,
                end_line: 3,
                tab_width: 4,
                style: IndentStyle::Spaces,
                action: IndentAction::IndentBy(2),
            },
            IndentSpec {
                start_line: 0,
                end_line: 3,
                tab_width: 4,
                style: IndentStyle::TabsAndSpaces,
                action: IndentAction::DedentBy(1),
            },
            IndentSpec {
                start_line: 1,
                end_line: 3,
                tab_width: 4,
                style: IndentStyle::Spaces,
                action: IndentAction::SetColumn(6),
            },
        ];
        let mut expected_ind = ind_initial.to_owned();
        for (i, spec) in indent_steps.into_iter().enumerate() {
            let rev = core.revision().unwrap();
            let id = 111 + i as u64;
            core.apply_indent_region(&IndentRegionRequest {
                request: RequestId::new(id),
                actor: human,
                nonce: Nonce::new(id),
                base: rev,
                buffer: ind_buf,
                spec,
                sequence_step: None,
            })
            .unwrap();
            expected_ind = ref_indent_text(&expected_ind, spec);
            assert_eq!(core.read_agent(human, ind_buf).unwrap().text, expected_ind);
        }
        // Invalid indent region (inverted lines, zero tab width) rejected atomically.
        let cur_rev = core.revision().unwrap();
        assert_rejection_atomic(
            &core,
            &[human],
            &[ind_buf],
            &[],
            &[],
            CoreErrorKind::InvalidIndent,
            || {
                core.apply_indent_region(&IndentRegionRequest {
                    request: RequestId::new(120),
                    actor: human,
                    nonce: Nonce::new(120),
                    base: cur_rev,
                    buffer: ind_buf,
                    spec: IndentSpec {
                        start_line: 2,
                        end_line: 1,
                        tab_width: 4,
                        style: IndentStyle::Spaces,
                        action: IndentAction::IndentBy(2),
                    },
                    sequence_step: None,
                })
            },
        );

        // D. Navigation (word, delimiter, visual column, move_selection_word) across ALL FOUR formats.
        let nav_text = "a_b!! 界 (x[y]) z\n\t😀mid\nend";
        let nav_buf = core
            .open_buffer(&OpenBufferRequest {
                request: RequestId::new(130),
                actor: human,
                nonce: Nonce::new(130),
                base: core.revision().unwrap(),
                name: "nav.txt".into(),
                text: Text::from_str(nav_text),
            })
            .unwrap()
            .buffer;
        let delims = DelimiterTable::new(&[('(', ')'), ('[', ']')]).unwrap();
        let delim_pairs =
            ref_delimiter_pairs(nav_text, 0, nav_text.len(), &[('(', ')'), ('[', ']')]);

        for format in ALL_FORMATS {
            // Test all word motions at multiple boundaries.
            for &byte in &[0usize, 3, 5, 10, 18] {
                for motion in [
                    WordMotion::ForwardEnd,
                    WordMotion::NextStart,
                    WordMotion::PreviousStart,
                    WordMotion::NextEnd,
                ] {
                    let pos = ref_pos(nav_text, byte, format);
                    let got = core.move_word(human, nav_buf, pos, motion).unwrap();
                    let exp = ref_move_word(nav_text, 0, nav_text.len(), byte, motion);
                    assert_eq!(got, ref_pos(nav_text, exp, format));
                }
            }
            // Test matching delimiter & pair traversal.
            for &byte in &[0usize, 10, 12, 14, 15] {
                let pos = ref_pos(nav_text, byte, format);
                let got_match = core
                    .matching_delimiter(human, nav_buf, pos, &delims)
                    .unwrap();
                let exp_match = delim_pairs.iter().find_map(|&(o, c, _)| {
                    if o == byte {
                        Some(c)
                    } else if c == byte {
                        Some(o)
                    } else {
                        None
                    }
                });
                assert_eq!(got_match, exp_match.map(|b| ref_pos(nav_text, b, format)));

                for forward in [false, true] {
                    let got_trav = core
                        .traverse_delimiter_pair(human, nav_buf, pos, &delims, forward)
                        .unwrap();
                    let exp_trav = delim_pairs
                        .iter()
                        .filter(|&&(o, _, oe)| o <= byte && byte < oe)
                        .max_by_key(|&&(o, _, _)| o)
                        .map(|&(o, _, oe)| if forward { oe } else { o });
                    assert_eq!(got_trav, exp_trav.map(|b| ref_pos(nav_text, b, format)));
                }
            }
            // Test vertical visual column movement with tabs, Floor/Ceil, sticky goal.
            for rounding in [Rounding::Floor, Rounding::Ceil] {
                let start_pos = ref_pos(nav_text, 2, format);
                let (got_pos, goal) = core
                    .move_visual(&VisualMoveRequest {
                        actor: human,
                        buffer: nav_buf,
                        position: start_pos,
                        delta_lines: 1,
                        goal_column: None,
                        tab_width: 4,
                        rounding,
                    })
                    .unwrap();
                let (exp_off, exp_goal) =
                    ref_move_visual(nav_text, (0, nav_text.len()), 2, 1, None, 4, rounding);
                assert_eq!(goal, exp_goal);
                assert_eq!(got_pos, ref_pos(nav_text, exp_off, format));
            }
            // Test move_selection_word (Move and Extend).
            let init_sel = ref_region(nav_text, 0, 0, EndpointMode::Exclusive, format);
            core.set_selection(human, nav_buf, init_sel).unwrap();
            let extended = core
                .move_selection_word(
                    human,
                    nav_buf,
                    WordMotion::ForwardEnd,
                    SelectionMotion::Extend,
                    format,
                )
                .unwrap();
            assert_eq!((extended.anchor_byte, extended.head_byte), (0, 3));
            let moved = core
                .move_selection_word(
                    human,
                    nav_buf,
                    WordMotion::NextStart,
                    SelectionMotion::Move,
                    format,
                )
                .unwrap();
            let exp_next = ref_move_word(nav_text, 0, nav_text.len(), 3, WordMotion::NextStart);
            assert_eq!(exp_next, 6);
            assert_eq!((moved.anchor_byte, moved.head_byte), (exp_next, exp_next));
        }

        // E. Search & Bounded Permission-Scoped Context Extraction (with narrowed views & non-interference).
        let ctx_core = EditorCore::new();
        ctx_core.register_actor(human, ActorRole::Human).unwrap();
        ctx_core.register_actor(agent, ActorRole::Agent).unwrap();
        ctx_core.grant_actor(human, Capability::Read).unwrap();
        ctx_core.grant_actor(human, Capability::Commit).unwrap();
        let pub_buf = ctx_core
            .open_buffer(&OpenBufferRequest {
                request: RequestId::new(1),
                actor: human,
                nonce: Nonce::new(1),
                base: Revision::ZERO,
                name: "public.txt".into(),
                text: Text::from_str("aaa界😀aaa"),
            })
            .unwrap()
            .buffer;
        let sec_buf = ctx_core
            .open_buffer(&OpenBufferRequest {
                request: RequestId::new(2),
                actor: human,
                nonce: Nonce::new(2),
                base: Revision::new(1),
                name: "secret.txt".into(),
                text: Text::from_str("secret aaa token"),
            })
            .unwrap()
            .buffer;
        // Grant agent Read ONLY on pub_buf (not sec_buf).
        ctx_core
            .grant_buffer(agent, pub_buf, Capability::Read)
            .unwrap();

        // Overlapping search matches for "aa" must omit sec_buf for agent, and match ref_search_all on pub_buf.
        let pub_text = "aaa界😀aaa";
        let exp_hits = ref_search_all(pub_text, "aa");
        assert_eq!(exp_hits, vec![(0, 2), (1, 3), (10, 12), (11, 13)]);
        for format in ALL_FORMATS {
            let hits = ctx_core.search_workspace(agent, "aa", format).unwrap();
            assert_eq!(hits.len(), exp_hits.len());
            for (h, &(s, e)) in hits.iter().zip(&exp_hits) {
                assert_eq!(h.buffer, pub_buf);
                assert_eq!((h.start_byte, h.end_byte), (s, e));
                assert_eq!(h.start, ref_pos(pub_text, s, format));
                assert_eq!(h.end, ref_pos(pub_text, e, format));
            }
            // Empty pattern fails cleanly.
            assert_eq!(
                ctx_core
                    .search_workspace(agent, "", format)
                    .unwrap_err()
                    .kind(),
                CoreErrorKind::EmptyPattern
            );
        }

        // Narrow agent's view on pub_buf to "aaa界😀" (bytes 0..10) and verify bounded UTF-8-safe context extraction.
        ctx_core
            .narrow_view(
                agent,
                pub_buf,
                ref_region(pub_text, 0, 10, EndpointMode::Exclusive, HostFormat::Emacs),
            )
            .unwrap();
        // Budget 7 bytes: "aaa界" is 6 bytes, next char "😀" is 4 bytes -> truncates cleanly at 6 bytes!
        let budget = ContextBudget::new(7).unwrap();
        let req = ContextRequest::workspace(budget);
        let shared_before = ctx_core
            .extract_shared_context(
                &[
                    ParticipantScope::all_readable(human),
                    ParticipantScope::all_readable(agent),
                ],
                &req,
                HostFormat::AGENT_UTF16,
            )
            .unwrap();
        assert_eq!(shared_before.pack.fragments.len(), 1);
        let frag = &shared_before.pack.fragments[0];
        assert_eq!(frag.buffer, pub_buf);
        let exp_trunc_len = ref_truncate_utf8(&pub_text[..10], 7);
        assert_eq!(exp_trunc_len, 6);
        assert_eq!(frag.as_str(), &pub_text[..exp_trunc_len]);
        assert!(frag.truncated);
        assert_eq!(frag.omitted_bytes, 4);
        assert_eq!(
            shared_before.projected_spans[0],
            (
                ref_pos(pub_text, 0, HostFormat::AGENT_UTF16),
                ref_pos(pub_text, exp_trunc_len, HostFormat::AGENT_UTF16),
            )
        );

        // Non-interference: mutating secret buffer produces identical context content!
        ctx_core
            .apply_edit(&EditRequest {
                request: RequestId::new(3),
                actor: human,
                nonce: Nonce::new(3),
                base: Revision::new(2),
                edit: EditSpec {
                    buffer: sec_buf,
                    region: ref_region(
                        "secret aaa token",
                        0,
                        6,
                        EndpointMode::Exclusive,
                        HostFormat::AGENT_BYTE,
                    ),
                    replacement: Text::from_str("TOPSECRET"),
                },
                sequence_step: None,
            })
            .unwrap();
        let shared_after = ctx_core
            .extract_shared_context(
                &[
                    ParticipantScope::all_readable(human),
                    ParticipantScope::all_readable(agent),
                ],
                &req,
                HostFormat::AGENT_UTF16,
            )
            .unwrap();
        assert!(shared_after.pack.same_content(&shared_before.pack));
        assert_eq!(shared_after.pack.revision, Revision::new(3));

        // Strict context request for unreadable sec_buf rejects with PermissionDenied.
        assert_eq!(
            ctx_core
                .extract_context(
                    agent,
                    &ContextScope::all_readable(),
                    &ContextRequest::new(vec![ContextTarget::Buffer(sec_buf)], budget),
                    HostFormat::Emacs,
                )
                .unwrap_err()
                .kind(),
            CoreErrorKind::PermissionDenied
        );
    }

    // ======================================================================
    // Test 3: Disjoint Concurrent Edits, Overlapping Conflicts, Permissions,
    // Revocation Propagation, Stale Base After Compaction, and Close Resync
    // ======================================================================

    #[test]
    fn disjoint_concurrent_edits_conflicts_permissions_revocation_and_compaction() {
        let human = ActorId::new(1);
        let agent = ActorId::new(2);
        let reviewer2 = ActorId::new(3);
        let actors = [human, agent, reviewer2];

        for format in THREE_FORMATS {
            let core = EditorCore::new();
            core.register_actor(human, ActorRole::Human).unwrap();
            core.register_actor(agent, ActorRole::Agent).unwrap();
            core.register_actor(reviewer2, ActorRole::Human).unwrap();
            for cap in Capability::ALL {
                core.grant_default(cap).unwrap();
            }

            let base_text = "aaaa bbbb cccc";
            let buf = core
                .open_buffer(&OpenBufferRequest {
                    request: RequestId::new(1),
                    actor: human,
                    nonce: Nonce::new(1),
                    base: Revision::ZERO,
                    name: "shared.txt".into(),
                    text: Text::from_str(base_text),
                })
                .unwrap()
                .buffer;
            let (client_early, mut local_early) = core.subscribe(human, buf, format).unwrap();

            // Human edits "aaaa" -> "AAAA" at base rev 1 -> rev 2.
            core.apply_edit(&EditRequest {
                request: RequestId::new(2),
                actor: human,
                nonce: Nonce::new(2),
                base: Revision::new(1),
                edit: EditSpec {
                    buffer: buf,
                    region: ref_region(base_text, 0, 4, EndpointMode::Exclusive, format),
                    replacement: Text::from_str("AAAA"),
                },
                sequence_step: None,
            })
            .unwrap();

            // Agent submits DISJOINT concurrent edit "cccc" -> "CCCC" based on stale rev 1 -> rebases cleanly to rev 3!
            core.apply_edit(&EditRequest {
                request: RequestId::new(3),
                actor: agent,
                nonce: Nonce::new(3),
                base: Revision::new(1),
                edit: EditSpec {
                    buffer: buf,
                    region: ref_region(base_text, 10, 14, EndpointMode::Exclusive, format),
                    replacement: Text::from_str("CCCC"),
                },
                sequence_step: None,
            })
            .unwrap();
            assert_eq!(core.read_agent(human, buf).unwrap().text, "AAAA bbbb CCCC");

            // Reviewer2 submits OVERLAPPING concurrent edit "aa bb" (bytes 2..7) based on rev 1 -> Conflict (atomic)!
            assert_rejection_atomic(
                &core,
                &actors,
                &[buf],
                &[],
                &[client_early],
                CoreErrorKind::Conflict,
                || {
                    core.apply_edit(&EditRequest {
                        request: RequestId::new(4),
                        actor: reviewer2,
                        nonce: Nonce::new(4),
                        base: Revision::new(1),
                        edit: EditSpec {
                            buffer: buf,
                            region: ref_region(base_text, 2, 7, EndpointMode::Exclusive, format),
                            replacement: Text::from_str("CONFLICT"),
                        },
                        sequence_step: None,
                    })
                },
            );

            // Compaction to rev 3 expires base rev 1.
            assert_eq!(core.compact(Revision::new(3)).unwrap(), Revision::new(3));
            assert_rejection_atomic(
                &core,
                &actors,
                &[buf],
                &[],
                &[client_early],
                CoreErrorKind::BaseExpired,
                || {
                    core.apply_edit(&EditRequest {
                        request: RequestId::new(5),
                        actor: agent,
                        nonce: Nonce::new(5),
                        base: Revision::new(1),
                        edit: EditSpec {
                            buffer: buf,
                            region: ref_region(base_text, 5, 9, EndpointMode::Exclusive, format),
                            replacement: Text::from_str("STALE"),
                        },
                        sequence_step: None,
                    })
                },
            );
            // Compacted client fails incremental delivery and recovers via snapshot reconnect.
            assert_eq!(
                core.deliver_incremental(client_early).unwrap_err().kind(),
                CoreErrorKind::ResyncRequired
            );
            let rec = core.reconnect(client_early).unwrap();
            assert!(matches!(rec, ReconnectOutcome::Snapshot { .. }));
            assert!(local_early.apply_reconnect(&rec).unwrap());
            assert_eq!(
                local_early.decode().unwrap().text.as_str(),
                "AAAA bbbb CCCC"
            );

            // recover_buffer works for retained rev 3 and fails BaseExpired for compacted rev 1.
            assert_eq!(
                core.recover_buffer(human, buf, Revision::new(1), format)
                    .unwrap_err()
                    .kind(),
                CoreErrorKind::BaseExpired
            );
            let rec_buf = core
                .recover_buffer(human, buf, Revision::new(3), format)
                .unwrap()
                .unwrap();
            assert_eq!(rec_buf.decode().unwrap().text.as_str(), "AAAA bbbb CCCC");

            // Permission propagation: agent proposes -> reviewer2 approves -> revoking reviewer2's Review blocks commit!
            let cur_text = core.read_agent(human, buf).unwrap().text;
            let prop_id = RequestId::new(6);
            core.propose(&ProposalRequest {
                request: prop_id,
                actor: agent,
                nonce: Nonce::new(6),
                base: Revision::new(3),
                payload: ProposalPayload::Edit(EditSpec {
                    buffer: buf,
                    region: ref_region(&cur_text, 5, 9, EndpointMode::Exclusive, format),
                    replacement: Text::from_str("BBBB"),
                }),
            })
            .unwrap();
            core.approve_proposal(prop_id, reviewer2).unwrap();
            core.revoke_default(Capability::Review).unwrap();
            assert_rejection_atomic(
                &core,
                &actors,
                &[buf],
                &[prop_id],
                &[client_early],
                CoreErrorKind::PermissionDenied,
                || core.commit_proposal(prop_id),
            );
            // Restore Review for reviewer2, revoke Commit from agent -> commit blocked!
            core.grant_actor(reviewer2, Capability::Review).unwrap();
            core.revoke_default(Capability::Commit).unwrap();
            core.grant_actor(human, Capability::Commit).unwrap();
            assert_rejection_atomic(
                &core,
                &actors,
                &[buf],
                &[prop_id],
                &[client_early],
                CoreErrorKind::PermissionDenied,
                || core.commit_proposal(prop_id),
            );
            // Restore Commit for agent -> commit succeeds!
            core.grant_buffer(agent, buf, Capability::Commit).unwrap();
            core.commit_proposal(prop_id).unwrap();
            assert_eq!(core.read_agent(human, buf).unwrap().text, "AAAA BBBB CCCC");

            // Revoking agent permanently blocks reads, subscriptions, search, context, edits, and re-registration.
            let (agent_client, _) = core.subscribe(agent, buf, format).unwrap();
            core.revoke_actor(agent).unwrap();
            assert!(core.is_actor_revoked(agent).unwrap());
            assert!(!core.is_actor_active(agent).unwrap());
            assert_eq!(
                core.register_actor(agent, ActorRole::Agent)
                    .unwrap_err()
                    .kind(),
                CoreErrorKind::RevokedActor
            );
            assert_eq!(
                core.read_snapshot(agent, buf, format).unwrap_err().kind(),
                CoreErrorKind::RevokedActor
            );
            assert_eq!(
                core.deliver_incremental(agent_client).unwrap_err().kind(),
                CoreErrorKind::RevokedActor
            );
            assert_eq!(
                core.reconnect(agent_client).unwrap_err().kind(),
                CoreErrorKind::RevokedActor
            );
            assert_eq!(
                core.search_workspace(agent, "AAAA", format)
                    .unwrap_err()
                    .kind(),
                CoreErrorKind::RevokedActor
            );
            assert_eq!(
                core.extract_context(
                    agent,
                    &ContextScope::all_readable(),
                    &ContextRequest::workspace(ContextBudget::new(64).unwrap()),
                    format,
                )
                .unwrap_err()
                .kind(),
                CoreErrorKind::RevokedActor
            );

            // Closing dirty buffer with RequireClean fails atomically, Discard succeeds and notifies client.
            assert!(core.is_buffer_dirty(buf).unwrap());
            assert_rejection_atomic(
                &core,
                &actors,
                &[buf],
                &[prop_id],
                &[client_early],
                CoreErrorKind::DirtyBuffer,
                || {
                    core.close_buffer(&CloseBufferRequest {
                        request: RequestId::new(7),
                        actor: human,
                        nonce: Nonce::new(7),
                        base: Revision::new(4),
                        buffer: buf,
                        policy: ClosePolicy::RequireClean,
                    })
                },
            );
            core.close_buffer(&CloseBufferRequest {
                request: RequestId::new(8),
                actor: human,
                nonce: Nonce::new(8),
                base: Revision::new(4),
                buffer: buf,
                policy: ClosePolicy::Discard,
            })
            .unwrap();
            let closed_rec = core.reconnect(client_early).unwrap();
            assert!(matches!(closed_rec, ReconnectOutcome::Closed { .. }));
            assert!(!local_early.apply_reconnect(&closed_rec).unwrap());
            assert_eq!(
                core.client_snapshot(client_early).unwrap_err().kind(),
                CoreErrorKind::BufferClosed
            );
            assert_eq!(
                core.read_snapshot(human, buf, format).unwrap_err().kind(),
                CoreErrorKind::BufferClosed
            );
        }
    }

    // ======================================================================
    // Test 4: Invalid Frontend Commands, Coordinates, and Atomic Rejections
    // ======================================================================

    #[test]
    fn invalid_frontend_commands_coordinates_and_atomic_rejections() {
        let human = ActorId::new(1);
        let other = ActorId::new(2);
        let core = EditorCore::new();
        core.register_actor(human, ActorRole::Human).unwrap();
        core.register_actor(other, ActorRole::Human).unwrap();
        for cap in Capability::ALL {
            core.grant_default(cap).unwrap();
        }
        // Invalid UTF-8 rejected at boundary.
        assert_eq!(
            EditorCore::validate_utf8(&[0xff, 0xfe]).unwrap_err().kind(),
            CoreErrorKind::InvalidUtf8
        );
        // Role conflict rejected atomically.
        assert_eq!(
            core.register_actor(human, ActorRole::Agent)
                .unwrap_err()
                .kind(),
            CoreErrorKind::RoleConflict
        );

        let buf = core
            .open_buffer(&OpenBufferRequest {
                request: RequestId::new(1),
                actor: human,
                nonce: Nonce::new(1),
                base: Revision::ZERO,
                name: "doc".into(),
                text: Text::from_str("a😀z"),
            })
            .unwrap()
            .buffer;

        // Duplicate buffer name rejected atomically.
        assert_rejection_atomic(
            &core,
            &[human, other],
            &[buf],
            &[],
            &[],
            CoreErrorKind::NameInUse,
            || {
                core.open_buffer(&OpenBufferRequest {
                    request: RequestId::new(2),
                    actor: human,
                    nonce: Nonce::new(2),
                    base: Revision::new(1),
                    name: "doc".into(),
                    text: Text::from_str("dup"),
                })
            },
        );

        // Invalid coordinates across all three hosts (mid-UTF-8 byte, surrogate UTF-16, out-of-range).
        let bad_coords: Vec<(HostCommands, CoreErrorKind)> = vec![
            (
                HostCommands::Emacs(vec![EmacsCommand::Point(EmacsCharPos::new(99).unwrap())]),
                CoreErrorKind::OutOfRange,
            ),
            (
                HostCommands::Neovim(vec![NeovimCommand::Cursor(LineColByte::new(0, 2))]),
                CoreErrorKind::InvalidBoundary,
            ),
            (
                HostCommands::Agent(vec![AgentCommand::Select {
                    anchor: AgentPosition::Byte(BytePos::new(2)),
                    head: AgentPosition::Byte(BytePos::new(2)),
                }]),
                CoreErrorKind::InvalidBoundary,
            ),
            (
                HostCommands::Agent(vec![AgentCommand::Select {
                    anchor: AgentPosition::LineColUtf16(LineColUtf16::new(0, 2)),
                    head: AgentPosition::LineColUtf16(LineColUtf16::new(0, 2)),
                }]),
                CoreErrorKind::InvalidUtf16Boundary,
            ),
            (
                HostCommands::Emacs(vec![EmacsCommand::Unsupported("evil-macro".into())]),
                CoreErrorKind::UnsupportedOperation,
            ),
            (
                HostCommands::Neovim(vec![NeovimCommand::Unsupported("lua-hook".into())]),
                CoreErrorKind::UnsupportedOperation,
            ),
            (
                HostCommands::Agent(vec![AgentCommand::Unsupported("shell".into())]),
                CoreErrorKind::UnsupportedOperation,
            ),
            (
                HostCommands::Emacs(vec![
                    EmacsCommand::BeginChangeGroup,
                    EmacsCommand::BeginChangeGroup,
                ]),
                CoreErrorKind::CorruptSequence,
            ),
            (
                HostCommands::Neovim(vec![
                    NeovimCommand::BeginUndoBlock,
                    NeovimCommand::EndUndoBlock,
                ]),
                CoreErrorKind::CorruptSequence,
            ),
            (
                HostCommands::Agent(vec![
                    AgentCommand::BeginBatch,
                    AgentCommand::Replace {
                        start: AgentPosition::Byte(BytePos::new(0)),
                        end: AgentPosition::Byte(BytePos::new(0)),
                        text: Text::from_str("x"),
                    },
                ]),
                CoreErrorKind::CorruptSequence,
            ),
            (
                HostCommands::Emacs(vec![EmacsCommand::EndChangeGroup]),
                CoreErrorKind::CorruptSequence,
            ),
            (
                HostCommands::Neovim(vec![
                    NeovimCommand::BeginUndoBlock,
                    NeovimCommand::Select {
                        anchor: LineColByte::new(0, 0),
                        head: LineColByte::new(0, 1),
                    },
                    NeovimCommand::EndUndoBlock,
                ]),
                CoreErrorKind::UnsupportedOperation,
            ),
            // Two ungrouped edits without a change group.
            (
                HostCommands::Agent(vec![
                    AgentCommand::Replace {
                        start: AgentPosition::Byte(BytePos::new(0)),
                        end: AgentPosition::Byte(BytePos::new(0)),
                        text: Text::from_str("x"),
                    },
                    AgentCommand::Replace {
                        start: AgentPosition::Byte(BytePos::new(0)),
                        end: AgentPosition::Byte(BytePos::new(0)),
                        text: Text::from_str("y"),
                    },
                ]),
                CoreErrorKind::UnsupportedOperation,
            ),
        ];
        for (i, (cmds, exp_kind)) in bad_coords.into_iter().enumerate() {
            let rid = 200 + i as u64;
            assert_rejection_atomic(&core, &[human, other], &[buf], &[], &[], exp_kind, || {
                core.execute_commands(&CommandRequest {
                    request: RequestId::new(rid),
                    actor: human,
                    nonce: Nonce::new(rid),
                    base: Revision::new(1),
                    buffer: buf,
                    commands: cmds,
                    sequence_step: None,
                })
            });
        }

        // Overlapping edits in multi-buffer batch rejected atomically.
        assert_rejection_atomic(
            &core,
            &[human, other],
            &[buf],
            &[],
            &[],
            CoreErrorKind::Conflict,
            || {
                core.apply_multi_buffer_batch(&MultiBufferBatchRequest {
                    request: RequestId::new(250),
                    actor: human,
                    nonce: Nonce::new(250),
                    base: Revision::new(1),
                    primary: buf,
                    edits: vec![
                        EditSpec {
                            buffer: buf,
                            region: ref_region(
                                "a😀z",
                                0,
                                5,
                                EndpointMode::Exclusive,
                                HostFormat::AGENT_BYTE,
                            ),
                            replacement: Text::from_str("X"),
                        },
                        EditSpec {
                            buffer: buf,
                            region: ref_region(
                                "a😀z",
                                1,
                                6,
                                EndpointMode::Exclusive,
                                HostFormat::AGENT_BYTE,
                            ),
                            replacement: Text::from_str("Y"),
                        },
                    ],
                    sequence_step: None,
                })
            },
        );

        // Post-edit selection outside narrowed view MUST roll back / never commit the edit!
        core.narrow_view(
            human,
            buf,
            ref_region(
                "a😀z",
                0,
                5,
                EndpointMode::Exclusive,
                HostFormat::AGENT_BYTE,
            ),
        )
        .unwrap();
        assert_rejection_atomic(
            &core,
            &[human, other],
            &[buf],
            &[],
            &[],
            CoreErrorKind::OutOfRange,
            || {
                core.execute_commands(&CommandRequest {
                    request: RequestId::new(251),
                    actor: human,
                    nonce: Nonce::new(251),
                    base: Revision::new(1),
                    buffer: buf,
                    commands: HostCommands::Agent(vec![
                        AgentCommand::Replace {
                            start: AgentPosition::Byte(BytePos::new(0)),
                            end: AgentPosition::Byte(BytePos::new(1)),
                            text: Text::from_str("A"),
                        },
                        AgentCommand::Select {
                            anchor: AgentPosition::Byte(BytePos::new(0)),
                            head: AgentPosition::Byte(BytePos::new(6)),
                        },
                    ]),
                    sequence_step: None,
                })
            },
        );
        core.widen_view(human, buf, HostFormat::AGENT_BYTE).unwrap();

        // Invalid annotations: Neovim priority > 65535 and mixed Agent coordinate kinds.
        let key = AnnotationKey {
            buffer: buf,
            namespace: NamespaceId::new(1),
            id: AnnotationId::new(1),
        };
        assert_rejection_atomic(
            &core,
            &[human, other],
            &[buf],
            &[],
            &[],
            CoreErrorKind::UnsupportedOperation,
            || {
                core.set_neovim_extmark(
                    human,
                    &NeovimExtmark {
                        start: LineColByte::new(0, 0),
                        end: LineColByte::new(0, 1),
                        right_gravity: false,
                        end_right_gravity: true,
                        metadata: AnnotationMetadata {
                            key,
                            owner: human,
                            priority: 65536,
                            collapse: CollapsePolicy::Evict,
                            label: "too-high".into(),
                        },
                    },
                )
            },
        );
        assert_rejection_atomic(
            &core,
            &[human, other],
            &[buf],
            &[],
            &[],
            CoreErrorKind::InvalidCoordinate,
            || {
                core.set_agent_annotation(
                    human,
                    &AgentAnnotation {
                        start: AgentPosition::Byte(BytePos::new(0)),
                        end: AgentPosition::LineColUtf16(LineColUtf16::new(0, 1)),
                        start_affinity: Affinity::Left,
                        end_affinity: Affinity::Right,
                        metadata: AnnotationMetadata {
                            key,
                            owner: human,
                            priority: 1,
                            collapse: CollapsePolicy::Evict,
                            label: "mixed".into(),
                        },
                    },
                )
            },
        );

        // Commit one edit, then verify idempotent retry, conflicting reuse, TxnId reuse, and undo errors.
        let good_req = EditRequest {
            request: RequestId::new(300),
            actor: human,
            nonce: Nonce::new(300),
            base: Revision::new(1),
            edit: EditSpec {
                buffer: buf,
                region: ref_region("a😀z", 0, 1, EndpointMode::Exclusive, HostFormat::Emacs),
                replacement: Text::from_str("A"),
            },
            sequence_step: Some(SequenceStep {
                sequence: TxnId::new(human, Nonce::new(900)),
                index: 0,
            }),
        };
        let committed = core.apply_edit(&good_req).unwrap();
        // Exact retry returns identical outcome without advancing revision.
        assert_eq!(core.apply_edit(&good_req).unwrap(), committed);
        assert_eq!(core.revision().unwrap(), Revision::new(2));

        // Conflicting RequestId reuse fails atomically (even after compaction!).
        core.compact(Revision::new(2)).unwrap();
        assert_eq!(core.apply_edit(&good_req).unwrap(), committed);
        assert_rejection_atomic(
            &core,
            &[human, other],
            &[buf],
            &[],
            &[],
            CoreErrorKind::RequestReuse,
            || {
                let mut diff = good_req.clone();
                diff.edit.replacement = Text::from_str("Z");
                core.apply_edit(&diff)
            },
        );

        // Contradictory SequenceStep on new request fails atomically (GroupConflict).
        assert_rejection_atomic(
            &core,
            &[human, other],
            &[buf],
            &[],
            &[],
            CoreErrorKind::GroupConflict,
            || {
                core.apply_edit(&EditRequest {
                    request: RequestId::new(301),
                    actor: human,
                    nonce: Nonce::new(301),
                    base: Revision::new(2),
                    edit: EditSpec {
                        buffer: buf,
                        region: ref_region(
                            "A😀z",
                            0,
                            1,
                            EndpointMode::Exclusive,
                            HostFormat::Emacs,
                        ),
                        replacement: Text::from_str("B"),
                    },
                    sequence_step: good_req.sequence_step,
                })
            },
        );

        // Undo errors: ambiguous target, non-owner, unknown target, unknown client.
        let txn = committed.receipt.txn();
        assert_rejection_atomic(
            &core,
            &[human, other],
            &[buf],
            &[],
            &[],
            CoreErrorKind::AmbiguousUndoTarget,
            || {
                core.emacs_undo(
                    RequestId::new(302),
                    human,
                    Nonce::new(302),
                    UndoTarget::Resolved(&[]),
                )
            },
        );
        assert_rejection_atomic(
            &core,
            &[human, other],
            &[buf],
            &[],
            &[],
            CoreErrorKind::NotUndoOwner,
            || {
                core.neovim_undo(
                    RequestId::new(303),
                    other,
                    Nonce::new(303),
                    UndoTarget::Explicit(txn),
                )
            },
        );
        assert_eq!(
            core.deliver_incremental(ClientId::new(999))
                .unwrap_err()
                .kind(),
            CoreErrorKind::UnknownClient
        );
        assert_eq!(
            core.unsubscribe(ClientId::new(999)).unwrap_err().kind(),
            CoreErrorKind::UnknownClient
        );
    }

    // ======================================================================
    // Test 5: Deterministic Randomized Multi-Actor, Multi-Frontend Law Suite
    // ======================================================================

    #[derive(Clone, Debug, Default)]
    struct LifecycleCoverage {
        read: usize,
        propose: usize,
        approve: usize,
        reject_proposal: usize,
        commit: usize,
        deliver: usize,
        undo: usize,
        redo: usize,
        compact: usize,
        reconnect_deltas: usize,
        reconnect_snapshot: usize,
        conflict: usize,
        permission_denied: usize,
        navigation_search_context: usize,
    }

    #[derive(Clone, Debug)]
    struct RefUndoRecord {
        txn: TxnId,
        actor: ActorId,
        buffer: BufferId,
        cur_start: usize,
        cur_end: usize,
        original: String,
        replacement: String,
        undone: bool,
        tainted: bool,
        last_rev: u64,
    }

    #[derive(Clone, Debug)]
    struct RefPendingProp {
        req: RequestId,
        proposer: ActorId,
        nonce: Nonce,
        txn: TxnId,
        base: u64,
        buffer: BufferId,
        cur_start: usize,
        cur_end: usize,
        replacement: String,
        tainted: bool,
        approved: bool,
        finalized: bool,
    }

    fn propagate_splice_tracking(
        buffer: BufferId,
        start: usize,
        end: usize,
        inserted_len: usize,
        skip_undo_idx: Option<usize>,
        undo_records: &mut [RefUndoRecord],
        pending_props: &mut [RefPendingProp],
    ) {
        for (idx, rec) in undo_records.iter_mut().enumerate() {
            if Some(idx) == skip_undo_idx || rec.buffer != buffer || rec.tainted {
                continue;
            }
            if start <= rec.cur_end && end >= rec.cur_start {
                rec.tainted = true;
            } else if end < rec.cur_start {
                rec.cur_start = rec.cur_start - (end - start) + inserted_len;
                rec.cur_end = rec.cur_end - (end - start) + inserted_len;
            }
        }
        for prop in pending_props.iter_mut() {
            if prop.finalized || prop.buffer != buffer || prop.tainted {
                continue;
            }
            if start <= prop.cur_end && end >= prop.cur_start {
                prop.tainted = true;
            } else if end < prop.cur_start {
                prop.cur_start = prop.cur_start - (end - start) + inserted_len;
                prop.cur_end = prop.cur_end - (end - start) + inserted_len;
            }
        }
    }

    fn pick_nonempty_span_and_repl(rng: &mut Rng, text: &str) -> (usize, usize, String) {
        let bounds = ref_char_boundaries(text);
        assert!(bounds.len() >= 2);
        let max_chars = bounds.len() - 1;
        let i = rng.gen_range(0..max_chars);
        let max_span = (max_chars - i).min(3);
        let j = i + rng.gen_range(1..max_span + 1);
        let start = bounds[i];
        let end = bounds[j];
        let existing = &text[start..end];
        let tokens = ["αβ", "界", "😀", "e\u{0301}", "xy", "Z9", "ab\n"];
        let mut repl = (*rng.choose(&tokens).unwrap()).to_owned();
        // Keep buffer length bounded and never emit a no-op splice.
        if text.len() > 140 {
            repl = if existing == "q" {
                "w".into()
            } else {
                "q".into()
            };
        } else if repl == existing {
            repl.push('!');
        }
        (start, end, repl)
    }

    #[test]
    fn randomized_multi_actor_multi_frontend_laws() {
        let mut cov = LifecycleCoverage::default();
        let seeds = [1u64, 7, 42, 2026, 99_991];
        let h0 = ActorId::new(1);
        let h1 = ActorId::new(2);
        let a0 = ActorId::new(3);
        let a1 = ActorId::new(4);
        let restricted = ActorId::new(5);
        let all_actors = [h0, h1, a0, a1, restricted];
        let writers = [h0, h1, a0, a1];
        let humans = [h0, h1];

        for &seed in &seeds {
            let mut rng = Rng::new(seed);
            let core = EditorCore::new();
            let mut model = RefModel::default();
            let mut local_snaps: BTreeMap<ClientId, HostSnapshot> = BTreeMap::new();
            let mut next_id = 1u64;
            let mut alloc_id = || {
                let id = next_id;
                next_id += 1;
                (RequestId::new(id), Nonce::new(id))
            };

            for &(actor, role) in &[
                (h0, ActorRole::Human),
                (h1, ActorRole::Human),
                (a0, ActorRole::Agent),
                (a1, ActorRole::Agent),
                (restricted, ActorRole::Agent),
            ] {
                core.register_actor(actor, role).unwrap();
                model.register_actor(actor, role);
            }
            for &actor in &writers {
                for cap in Capability::ALL {
                    core.grant_actor(actor, cap).unwrap();
                    model.apply_access(AccessChange::GrantActor(actor, cap));
                }
            }

            let init_b0 = "s0:αa\ns1:界b\ns2:😀c\ns3:e\u{0301}d\ns4:xy\ns5:z0\n";
            let init_b1 = "p0:α\np1:界\np2:😀\np3:ok\n";
            let (req0, n0) = alloc_id();
            let b0 = core
                .open_buffer(&OpenBufferRequest {
                    request: req0,
                    actor: h0,
                    nonce: n0,
                    base: Revision::ZERO,
                    name: "doc0.txt".into(),
                    text: Text::from_str(init_b0),
                })
                .unwrap()
                .buffer;
            assert_eq!(model.open_buffer(req0, h0, n0, "doc0.txt", init_b0), b0);
            let (req1, n1) = alloc_id();
            let b1 = core
                .open_buffer(&OpenBufferRequest {
                    request: req1,
                    actor: h1,
                    nonce: n1,
                    base: Revision::new(1),
                    name: "doc1.txt".into(),
                    text: Text::from_str(init_b1),
                })
                .unwrap()
                .buffer;
            assert_eq!(model.open_buffer(req1, h1, n1, "doc1.txt", init_b1), b1);
            let buffers = [b0, b1];

            // Restricted actor gets Read on b0 only (no b1 Read, no Commit/Propose/Review).
            core.grant_buffer(restricted, b0, Capability::Read).unwrap();
            model.apply_access(AccessChange::GrantBuffer(restricted, b0, Capability::Read));

            // Subscribe 4 clients (one per HostFormat):
            // clients[0], clients[1] stay caught up via deliver / reconnect_deltas;
            // clients[2], clients[3] often lag so compact triggers reconnect_snapshot.
            let mut client_ids = Vec::new();
            for (idx, &fmt) in ALL_FORMATS.iter().enumerate() {
                let actor = writers[idx % writers.len()];
                let buf = buffers[idx % buffers.len()];
                let (cid, snap) = core.subscribe(actor, buf, fmt).unwrap();
                local_snaps.insert(cid, snap);
                model.clients.insert(
                    cid,
                    RefClient {
                        actor,
                        buffer: buf,
                        format: fmt,
                        revision: Revision::new(model.revision),
                        text: model.live_buffers[&buf].clone(),
                        closed: false,
                    },
                );
                client_ids.push(cid);
            }

            let mut history_texts: BTreeMap<u64, BTreeMap<BufferId, String>> = BTreeMap::new();
            history_texts.insert(model.revision, model.live_buffers.clone());
            // (committed_rev, buffer, pre_start, pre_end) for deterministic conflict generation.
            let mut commit_spans: Vec<(u64, BufferId, usize, usize)> = Vec::new();
            let mut undo_records: Vec<RefUndoRecord> = Vec::new();
            let mut pending_props: Vec<RefPendingProp> = Vec::new();
            let mut all_prop_reqs: Vec<RequestId> = Vec::new();

            for step in 0..90usize {
                let fmt = *rng.choose(&ALL_FORMATS).unwrap();
                let op = rng.gen_range(0..15);

                match op {
                    // 0..=1: Direct edit across random writer & host format.
                    0..=1 => {
                        let actor = *rng.choose(&writers).unwrap();
                        let buf = *rng.choose(&buffers).unwrap();
                        let cur = model.live_buffers[&buf].clone();
                        let (start, end, repl) = pick_nonempty_span_and_repl(&mut rng, &cur);
                        let orig = cur[start..end].to_owned();
                        let (req, nonce) = alloc_id();
                        let base = Revision::new(model.revision);
                        let out = core
                            .apply_edit(&EditRequest {
                                request: req,
                                actor,
                                nonce,
                                base,
                                edit: EditSpec {
                                    buffer: buf,
                                    region: ref_region(
                                        &cur,
                                        start,
                                        end,
                                        EndpointMode::Exclusive,
                                        fmt,
                                    ),
                                    replacement: Text::from_str(&repl),
                                },
                                sequence_step: None,
                            })
                            .unwrap();
                        model.apply_splices(req, actor, nonce, &[(buf, vec![(start, end, &repl)])]);
                        assert_eq!(out.receipt.committed(), Revision::new(model.revision));
                        propagate_splice_tracking(
                            buf,
                            start,
                            end,
                            repl.len(),
                            None,
                            &mut undo_records,
                            &mut pending_props,
                        );
                        undo_records.push(RefUndoRecord {
                            txn: TxnId::new(actor, nonce),
                            actor,
                            buffer: buf,
                            cur_start: start,
                            cur_end: start + repl.len(),
                            original: orig,
                            replacement: repl,
                            undone: false,
                            tainted: false,
                            last_rev: model.revision,
                        });
                        commit_spans.push((model.revision, buf, start, end));
                        history_texts.insert(model.revision, model.live_buffers.clone());
                    }
                    // 2: Multi-format read check.
                    2 => {
                        let actor = *rng.choose(&writers).unwrap();
                        let buf = *rng.choose(&buffers).unwrap();
                        let snap = core.read_snapshot(actor, buf, fmt).unwrap();
                        let decoded = snap.decode().unwrap();
                        assert_eq!(decoded.revision, Revision::new(model.revision));
                        assert_eq!(decoded.text.as_str(), model.live_buffers[&buf]);
                        cov.read += 1;
                    }
                    // 3: Propose (Edit or Commands payload).
                    3 => {
                        let actor = *rng.choose(&writers).unwrap();
                        let buf = *rng.choose(&buffers).unwrap();
                        let cur = model.live_buffers[&buf].clone();
                        let (start, end, repl) = pick_nonempty_span_and_repl(&mut rng, &cur);
                        let (req, nonce) = alloc_id();
                        let base = Revision::new(model.revision);
                        let payload = if rng.gen_bool() {
                            ProposalPayload::Edit(EditSpec {
                                buffer: buf,
                                region: ref_region(&cur, start, end, EndpointMode::Exclusive, fmt),
                                replacement: Text::from_str(&repl),
                            })
                        } else {
                            ProposalPayload::Commands {
                                buffer: buf,
                                commands: build_replace_commands(fmt, &cur, start, end, &repl),
                            }
                        };
                        let info = core
                            .propose(&ProposalRequest {
                                request: req,
                                actor,
                                nonce,
                                base,
                                payload,
                            })
                            .unwrap();
                        assert_eq!(info.status, ProposalStatus::Pending);
                        let txn = TxnId::new(actor, nonce);
                        model.proposals.insert(
                            req,
                            RefProposal {
                                proposer: actor,
                                txn,
                                base_revision: base,
                                buffers: vec![buf],
                                status: ProposalStatus::Pending,
                            },
                        );
                        pending_props.push(RefPendingProp {
                            req,
                            proposer: actor,
                            nonce,
                            txn,
                            base: model.revision,
                            buffer: buf,
                            cur_start: start,
                            cur_end: end,
                            replacement: repl,
                            tainted: false,
                            approved: false,
                            finalized: false,
                        });
                        all_prop_reqs.push(req);
                        cov.propose += 1;
                    }
                    // 4: Approve a pending proposal (fresh succeeds; stale fails atomically).
                    4 => {
                        let fresh: Vec<usize> = pending_props
                            .iter()
                            .enumerate()
                            .filter(|(_, p)| {
                                !p.finalized && !p.approved && p.base == model.revision
                            })
                            .map(|(i, _)| i)
                            .collect();
                        let stale: Vec<usize> = pending_props
                            .iter()
                            .enumerate()
                            .filter(|(_, p)| {
                                !p.finalized && !p.approved && p.base != model.revision
                            })
                            .map(|(i, _)| i)
                            .collect();
                        let reviewer = *rng.choose(&humans).unwrap();
                        if let Some(&idx) = rng.choose(&fresh) {
                            let req = pending_props[idx].req;
                            let info = core.approve_proposal(req, reviewer).unwrap();
                            let mut reviewers = BTreeSet::new();
                            reviewers.insert(reviewer);
                            let expected_status = ProposalStatus::Approved {
                                reviewers,
                                approved_revision: Revision::new(model.revision),
                            };
                            assert_eq!(info.status, expected_status);
                            model.proposals.get_mut(&req).unwrap().status = expected_status;
                            pending_props[idx].approved = true;
                            cov.approve += 1;
                        } else if let Some(&idx) = rng.choose(&stale) {
                            let req = pending_props[idx].req;
                            assert_rejection_atomic(
                                &core,
                                &all_actors,
                                &buffers,
                                &all_prop_reqs,
                                &client_ids,
                                CoreErrorKind::RevisionMismatch,
                                || core.approve_proposal(req, reviewer),
                            );
                            pending_props[idx].finalized = true;
                        } else {
                            // Propose fresh and approve immediately so both fresh-commit and stale-approval paths stay populated.
                            let actor = *rng.choose(&writers).unwrap();
                            let buf = *rng.choose(&buffers).unwrap();
                            let cur = model.live_buffers[&buf].clone();
                            let (start, end, repl) = pick_nonempty_span_and_repl(&mut rng, &cur);
                            let (req, nonce) = alloc_id();
                            let base = Revision::new(model.revision);
                            core.propose(&ProposalRequest {
                                request: req,
                                actor,
                                nonce,
                                base,
                                payload: ProposalPayload::Edit(EditSpec {
                                    buffer: buf,
                                    region: ref_region(
                                        &cur,
                                        start,
                                        end,
                                        EndpointMode::Exclusive,
                                        fmt,
                                    ),
                                    replacement: Text::from_str(&repl),
                                }),
                            })
                            .unwrap();
                            let txn = TxnId::new(actor, nonce);
                            let info = core.approve_proposal(req, reviewer).unwrap();
                            let mut reviewers = BTreeSet::new();
                            reviewers.insert(reviewer);
                            let expected_status = ProposalStatus::Approved {
                                reviewers,
                                approved_revision: base,
                            };
                            assert_eq!(info.status, expected_status);
                            model.proposals.insert(
                                req,
                                RefProposal {
                                    proposer: actor,
                                    txn,
                                    base_revision: base,
                                    buffers: vec![buf],
                                    status: expected_status,
                                },
                            );
                            pending_props.push(RefPendingProp {
                                req,
                                proposer: actor,
                                nonce,
                                txn,
                                base: model.revision,
                                buffer: buf,
                                cur_start: start,
                                cur_end: end,
                                replacement: repl,
                                tainted: false,
                                approved: true,
                                finalized: false,
                            });
                            all_prop_reqs.push(req);
                            cov.propose += 1;
                            cov.approve += 1;
                        }
                    }
                    // 5: Reject a pending or approved proposal.
                    5 => {
                        let cands: Vec<usize> = pending_props
                            .iter()
                            .enumerate()
                            .filter(|(_, p)| !p.finalized)
                            .map(|(i, _)| i)
                            .collect();
                        if let Some(&idx) = rng.choose(&cands) {
                            let reviewer = *rng.choose(&humans).unwrap();
                            let req = pending_props[idx].req;
                            let reason = Some(format!("reject-{step}"));
                            core.reject_proposal(req, reviewer, reason.clone()).unwrap();
                            model.proposals.get_mut(&req).unwrap().status =
                                ProposalStatus::Rejected { reviewer, reason };
                            pending_props[idx].finalized = true;
                            assert_rejection_atomic(
                                &core,
                                &all_actors,
                                &buffers,
                                &all_prop_reqs,
                                &client_ids,
                                CoreErrorKind::ProposalRejected,
                                || core.commit_proposal(req),
                            );
                            cov.reject_proposal += 1;
                        }
                    }
                    // 6: Commit an approved proposal (fresh commits; stale rejects atomically with StaleApproval).
                    6 => {
                        let fresh: Vec<usize> = pending_props
                            .iter()
                            .enumerate()
                            .filter(|(_, p)| !p.finalized && p.approved && p.base == model.revision)
                            .map(|(i, _)| i)
                            .collect();
                        let stale: Vec<usize> = pending_props
                            .iter()
                            .enumerate()
                            .filter(|(_, p)| !p.finalized && p.approved && p.base != model.revision)
                            .map(|(i, _)| i)
                            .collect();
                        if let Some(&idx) = rng.choose(&fresh) {
                            let prop = pending_props[idx].clone();
                            let out = core.commit_proposal(prop.req).unwrap();
                            let cur = model.live_buffers[&prop.buffer].clone();
                            let orig = cur[prop.cur_start..prop.cur_end].to_owned();
                            let prev_rev = Revision::new(model.revision);
                            let expected_receipt = CommitReceipt::new(
                                EditIntent::for_actor(
                                    prop.proposer,
                                    prop.nonce,
                                    Revision::new(prop.base),
                                ),
                                prev_rev,
                            )
                            .unwrap();
                            assert_eq!(out.receipt, expected_receipt);
                            assert_eq!(out.receipt.intent().base(), Revision::new(prop.base));
                            model.apply_splices(
                                prop.req,
                                prop.proposer,
                                prop.nonce,
                                &[(
                                    prop.buffer,
                                    vec![(prop.cur_start, prop.cur_end, &prop.replacement)],
                                )],
                            );
                            assert_eq!(out.receipt.committed(), Revision::new(model.revision));
                            model.proposals.get_mut(&prop.req).unwrap().status =
                                ProposalStatus::Committed {
                                    receipt: expected_receipt,
                                };
                            pending_props[idx].finalized = true;
                            propagate_splice_tracking(
                                prop.buffer,
                                prop.cur_start,
                                prop.cur_end,
                                prop.replacement.len(),
                                None,
                                &mut undo_records,
                                &mut pending_props,
                            );
                            undo_records.push(RefUndoRecord {
                                txn: prop.txn,
                                actor: prop.proposer,
                                buffer: prop.buffer,
                                cur_start: prop.cur_start,
                                cur_end: prop.cur_start + prop.replacement.len(),
                                original: orig,
                                replacement: prop.replacement.clone(),
                                undone: false,
                                tainted: false,
                                last_rev: model.revision,
                            });
                            commit_spans.push((
                                model.revision,
                                prop.buffer,
                                prop.cur_start,
                                prop.cur_end,
                            ));
                            history_texts.insert(model.revision, model.live_buffers.clone());
                            cov.commit += 1;
                        } else if let Some(&idx) = rng.choose(&stale) {
                            let req = pending_props[idx].req;
                            assert_rejection_atomic(
                                &core,
                                &all_actors,
                                &buffers,
                                &all_prop_reqs,
                                &client_ids,
                                CoreErrorKind::StaleApproval,
                                || core.commit_proposal(req),
                            );
                            pending_props[idx].finalized = true;
                        } else {
                            // Run a full fresh propose -> approve -> commit cycle in the chosen format.
                            let actor = *rng.choose(&writers).unwrap();
                            let reviewer = *rng.choose(&humans).unwrap();
                            let buf = *rng.choose(&buffers).unwrap();
                            let cur = model.live_buffers[&buf].clone();
                            let (start, end, repl) = pick_nonempty_span_and_repl(&mut rng, &cur);
                            let orig = cur[start..end].to_owned();
                            let (req, nonce) = alloc_id();
                            let base = Revision::new(model.revision);
                            core.propose(&ProposalRequest {
                                request: req,
                                actor,
                                nonce,
                                base,
                                payload: ProposalPayload::Commands {
                                    buffer: buf,
                                    commands: build_replace_commands(fmt, &cur, start, end, &repl),
                                },
                            })
                            .unwrap();
                            core.approve_proposal(req, reviewer).unwrap();
                            let out = core.commit_proposal(req).unwrap();
                            let txn = TxnId::new(actor, nonce);
                            let expected_receipt =
                                CommitReceipt::new(EditIntent::for_actor(actor, nonce, base), base)
                                    .unwrap();
                            assert_eq!(out.receipt, expected_receipt);
                            assert_eq!(out.receipt.intent().base(), base);
                            model.apply_splices(
                                req,
                                actor,
                                nonce,
                                &[(buf, vec![(start, end, &repl)])],
                            );
                            assert_eq!(out.receipt.committed(), Revision::new(model.revision));
                            model.proposals.insert(
                                req,
                                RefProposal {
                                    proposer: actor,
                                    txn,
                                    base_revision: base,
                                    buffers: vec![buf],
                                    status: ProposalStatus::Committed {
                                        receipt: expected_receipt,
                                    },
                                },
                            );
                            all_prop_reqs.push(req);
                            propagate_splice_tracking(
                                buf,
                                start,
                                end,
                                repl.len(),
                                None,
                                &mut undo_records,
                                &mut pending_props,
                            );
                            undo_records.push(RefUndoRecord {
                                txn,
                                actor,
                                buffer: buf,
                                cur_start: start,
                                cur_end: start + repl.len(),
                                original: orig,
                                replacement: repl,
                                undone: false,
                                tainted: false,
                                last_rev: model.revision,
                            });
                            commit_spans.push((model.revision, buf, start, end));
                            history_texts.insert(model.revision, model.live_buffers.clone());
                            cov.propose += 1;
                            cov.approve += 1;
                            cov.commit += 1;
                        }
                    }
                    // 7: Deliver incremental deltas to an eligible client (preferring clients[0..2]).
                    7 => {
                        let eligible: Vec<ClientId> = client_ids
                            .iter()
                            .copied()
                            .filter(|cid| {
                                let c = &model.clients[cid];
                                c.revision.raw() >= model.delivery_floor
                                    && c.revision.raw() < model.revision
                            })
                            .collect();
                        if let Some(&cid) = rng.choose(&eligible) {
                            let del = core.deliver_incremental(cid).unwrap();
                            assert!(local_snaps
                                .get_mut(&cid)
                                .unwrap()
                                .apply_delivery(&del)
                                .unwrap());
                            let c = model.clients.get_mut(&cid).unwrap();
                            c.revision = Revision::new(model.revision);
                            c.text = model.live_buffers[&c.buffer].clone();
                            cov.deliver += 1;
                        }
                    }
                    // 8: Selective Undo across rotating Emacs / Neovim / Agent formats.
                    8 => {
                        let cands: Vec<usize> = undo_records
                            .iter()
                            .enumerate()
                            .filter(|(_, r)| !r.tainted && !r.undone && r.last_rev > model.floor)
                            .map(|(i, _)| i)
                            .collect();
                        if let Some(&idx) = rng.choose(&cands) {
                            let rec = undo_records[idx].clone();
                            let (req, nonce) = alloc_id();
                            let out = match fmt {
                                HostFormat::Emacs => core
                                    .emacs_undo(
                                        req,
                                        rec.actor,
                                        nonce,
                                        UndoTarget::Explicit(rec.txn),
                                    )
                                    .unwrap(),
                                HostFormat::Neovim => core
                                    .neovim_undo(
                                        req,
                                        rec.actor,
                                        nonce,
                                        UndoTarget::Resolved(&[rec.txn]),
                                    )
                                    .unwrap(),
                                HostFormat::Agent(_) => {
                                    core.agent_undo(req, rec.actor, nonce, rec.txn).unwrap()
                                }
                            };
                            model.apply_splices(
                                req,
                                rec.actor,
                                nonce,
                                &[(
                                    rec.buffer,
                                    vec![(rec.cur_start, rec.cur_end, &rec.original)],
                                )],
                            );
                            assert_eq!(out.receipt.committed(), Revision::new(model.revision));
                            propagate_splice_tracking(
                                rec.buffer,
                                rec.cur_start,
                                rec.cur_end,
                                rec.original.len(),
                                Some(idx),
                                &mut undo_records,
                                &mut pending_props,
                            );
                            let slot = &mut undo_records[idx];
                            slot.undone = true;
                            slot.cur_end = slot.cur_start + slot.original.len();
                            slot.last_rev = model.revision;
                            commit_spans.push((
                                model.revision,
                                rec.buffer,
                                rec.cur_start,
                                rec.cur_end,
                            ));
                            history_texts.insert(model.revision, model.live_buffers.clone());
                            cov.undo += 1;
                        }
                    }
                    // 9: Selective Redo across rotating Emacs / Neovim / Agent formats.
                    9 => {
                        let cands: Vec<usize> = undo_records
                            .iter()
                            .enumerate()
                            .filter(|(_, r)| !r.tainted && r.undone && r.last_rev > model.floor)
                            .map(|(i, _)| i)
                            .collect();
                        if let Some(&idx) = rng.choose(&cands) {
                            let rec = undo_records[idx].clone();
                            let (req, nonce) = alloc_id();
                            let out = match fmt {
                                HostFormat::Emacs => core
                                    .emacs_redo(
                                        req,
                                        rec.actor,
                                        nonce,
                                        UndoTarget::Explicit(rec.txn),
                                    )
                                    .unwrap(),
                                HostFormat::Neovim => core
                                    .neovim_redo(
                                        req,
                                        rec.actor,
                                        nonce,
                                        UndoTarget::Resolved(&[rec.txn]),
                                    )
                                    .unwrap(),
                                HostFormat::Agent(_) => {
                                    core.agent_redo(req, rec.actor, nonce, rec.txn).unwrap()
                                }
                            };
                            model.apply_splices(
                                req,
                                rec.actor,
                                nonce,
                                &[(
                                    rec.buffer,
                                    vec![(rec.cur_start, rec.cur_end, &rec.replacement)],
                                )],
                            );
                            assert_eq!(out.receipt.committed(), Revision::new(model.revision));
                            propagate_splice_tracking(
                                rec.buffer,
                                rec.cur_start,
                                rec.cur_end,
                                rec.replacement.len(),
                                Some(idx),
                                &mut undo_records,
                                &mut pending_props,
                            );
                            let slot = &mut undo_records[idx];
                            slot.undone = false;
                            slot.cur_end = slot.cur_start + slot.replacement.len();
                            slot.last_rev = model.revision;
                            commit_spans.push((
                                model.revision,
                                rec.buffer,
                                rec.cur_start,
                                rec.cur_end,
                            ));
                            history_texts.insert(model.revision, model.live_buffers.clone());
                            cov.redo += 1;
                        }
                    }
                    // 10: Compact history (deliberately straddling clients when possible).
                    10 => {
                        if model.revision > model.floor + 2 {
                            let target = (model.floor + 1)
                                .max(model.revision.saturating_sub(2))
                                .min(model.revision);
                            let new_floor = core.compact(Revision::new(target)).unwrap();
                            assert_eq!(new_floor, Revision::new(target));
                            model.compact(target);
                            cov.compact += 1;
                        }
                    }
                    // 11: Reconnect (both delta catch-up and post-compaction snapshot recovery).
                    11 => {
                        // Prefer snapshot recovery if any client is behind delivery_floor, otherwise delta reconnect.
                        let snap_cands: Vec<ClientId> = client_ids
                            .iter()
                            .copied()
                            .filter(|cid| model.clients[cid].revision.raw() < model.delivery_floor)
                            .collect();
                        let delta_cands: Vec<ClientId> = client_ids
                            .iter()
                            .copied()
                            .filter(|cid| {
                                let r = model.clients[cid].revision.raw();
                                r >= model.delivery_floor && r < model.revision
                            })
                            .collect();
                        if !snap_cands.is_empty() && (delta_cands.is_empty() || rng.gen_bool()) {
                            let cid = *rng.choose(&snap_cands).unwrap();
                            assert_rejection_atomic(
                                &core,
                                &all_actors,
                                &buffers,
                                &all_prop_reqs,
                                &client_ids,
                                CoreErrorKind::ResyncRequired,
                                || core.deliver_incremental(cid),
                            );
                            let rec = core.reconnect(cid).unwrap();
                            assert!(matches!(rec, ReconnectOutcome::Snapshot { .. }));
                            assert!(local_snaps
                                .get_mut(&cid)
                                .unwrap()
                                .apply_reconnect(&rec)
                                .unwrap());
                            let c = model.clients.get_mut(&cid).unwrap();
                            c.revision = Revision::new(model.revision);
                            c.text = model.live_buffers[&c.buffer].clone();
                            cov.reconnect_snapshot += 1;
                        } else if let Some(&cid) = rng.choose(&delta_cands) {
                            let rec = core.reconnect(cid).unwrap();
                            assert!(matches!(rec, ReconnectOutcome::Deltas(_)));
                            assert!(local_snaps
                                .get_mut(&cid)
                                .unwrap()
                                .apply_reconnect(&rec)
                                .unwrap());
                            let c = model.clients.get_mut(&cid).unwrap();
                            c.revision = Revision::new(model.revision);
                            c.text = model.live_buffers[&c.buffer].clone();
                            cov.reconnect_deltas += 1;
                        }
                    }
                    // 12: Intentional overlapping stale-base edit -> atomic Conflict rejection.
                    12 => {
                        let cands: Vec<(u64, BufferId, usize, usize)> = commit_spans
                            .iter()
                            .copied()
                            .filter(|&(rev, _, s, e)| rev > model.floor && s < e)
                            .collect();
                        if let Some(&(rev, buf, start, end)) = rng.choose(&cands) {
                            let base_rev = rev - 1;
                            let base_text = &history_texts[&base_rev][&buf];
                            let (req, nonce) = alloc_id();
                            let actor = *rng.choose(&writers).unwrap();
                            let repl = if &base_text[start..end] == "CONF" {
                                "CONF2"
                            } else {
                                "CONF"
                            };
                            assert_rejection_atomic(
                                &core,
                                &all_actors,
                                &buffers,
                                &all_prop_reqs,
                                &client_ids,
                                CoreErrorKind::Conflict,
                                || {
                                    core.apply_edit(&EditRequest {
                                        request: req,
                                        actor,
                                        nonce,
                                        base: Revision::new(base_rev),
                                        edit: EditSpec {
                                            buffer: buf,
                                            region: ref_region(
                                                base_text,
                                                start,
                                                end,
                                                EndpointMode::Exclusive,
                                                fmt,
                                            ),
                                            replacement: Text::from_str(repl),
                                        },
                                        sequence_step: None,
                                    })
                                },
                            );
                            cov.conflict += 1;
                        }
                    }
                    // 13: Permission-denied checks across read, edit, propose, and context.
                    13 => {
                        let kind = rng.gen_range(0..4);
                        let (req, nonce) = alloc_id();
                        let cur_b0 = model.live_buffers[&b0].clone();
                        match kind {
                            0 => assert_rejection_atomic(
                                &core,
                                &all_actors,
                                &buffers,
                                &all_prop_reqs,
                                &client_ids,
                                CoreErrorKind::PermissionDenied,
                                || core.read_snapshot(restricted, b1, fmt),
                            ),
                            1 => assert_rejection_atomic(
                                &core,
                                &all_actors,
                                &buffers,
                                &all_prop_reqs,
                                &client_ids,
                                CoreErrorKind::PermissionDenied,
                                || {
                                    core.apply_edit(&EditRequest {
                                        request: req,
                                        actor: restricted,
                                        nonce,
                                        base: Revision::new(model.revision),
                                        edit: EditSpec {
                                            buffer: b0,
                                            region: ref_region(
                                                &cur_b0,
                                                0,
                                                1,
                                                EndpointMode::Exclusive,
                                                fmt,
                                            ),
                                            replacement: Text::from_str("!"),
                                        },
                                        sequence_step: None,
                                    })
                                },
                            ),
                            2 => assert_rejection_atomic(
                                &core,
                                &all_actors,
                                &buffers,
                                &all_prop_reqs,
                                &client_ids,
                                CoreErrorKind::PermissionDenied,
                                || {
                                    core.propose(&ProposalRequest {
                                        request: req,
                                        actor: restricted,
                                        nonce,
                                        base: Revision::new(model.revision),
                                        payload: ProposalPayload::Edit(EditSpec {
                                            buffer: b0,
                                            region: ref_region(
                                                &cur_b0,
                                                0,
                                                1,
                                                EndpointMode::Exclusive,
                                                fmt,
                                            ),
                                            replacement: Text::from_str("!"),
                                        }),
                                    })
                                },
                            ),
                            _ => assert_rejection_atomic(
                                &core,
                                &all_actors,
                                &buffers,
                                &all_prop_reqs,
                                &client_ids,
                                CoreErrorKind::PermissionDenied,
                                || {
                                    core.extract_context(
                                        restricted,
                                        &ContextScope::all_readable(),
                                        &ContextRequest::new(
                                            vec![ContextTarget::Buffer(b1)],
                                            ContextBudget::new(32).unwrap(),
                                        ),
                                        fmt,
                                    )
                                },
                            ),
                        }
                        cov.permission_denied += 1;
                    }
                    // 14: Navigation, search, bounded context + selection/annotation oracle checks.
                    _ => {
                        let actor = *rng.choose(&writers).unwrap();
                        let buf = *rng.choose(&buffers).unwrap();
                        let cur = model.live_buffers[&buf].clone();
                        let bounds = ref_char_boundaries(&cur);
                        let byte = *rng.choose(&bounds).unwrap();
                        let motion = *rng
                            .choose(&[
                                WordMotion::ForwardEnd,
                                WordMotion::NextStart,
                                WordMotion::PreviousStart,
                                WordMotion::NextEnd,
                            ])
                            .unwrap();
                        let got_word = core
                            .move_word(actor, buf, ref_pos(&cur, byte, fmt), motion)
                            .unwrap();
                        let exp_word = ref_move_word(&cur, 0, cur.len(), byte, motion);
                        assert_eq!(got_word, ref_pos(&cur, exp_word, fmt));

                        let delta = if rng.gen_bool() { 1 } else { -1 };
                        let rounding = if rng.gen_bool() {
                            Rounding::Floor
                        } else {
                            Rounding::Ceil
                        };
                        let (got_vis, got_goal) = core
                            .move_visual(&VisualMoveRequest {
                                actor,
                                buffer: buf,
                                position: ref_pos(&cur, byte, fmt),
                                delta_lines: delta,
                                goal_column: None,
                                tab_width: 4,
                                rounding,
                            })
                            .unwrap();
                        let (exp_off, exp_goal) =
                            ref_move_visual(&cur, (0, cur.len()), byte, delta, None, 4, rounding);
                        assert_eq!(got_goal, exp_goal);
                        assert_eq!(got_vis, ref_pos(&cur, exp_off, fmt));

                        // Search oracle across all readable buffers.
                        let pat = *rng.choose(&["α", "界", "😀", "s", ":"]).unwrap();
                        let hits = core.search_workspace(actor, pat, fmt).unwrap();
                        let mut exp_hits = Vec::new();
                        for (&bid, btext) in &model.live_buffers {
                            for (hs, he) in ref_search_all(btext, pat) {
                                exp_hits.push((bid, hs, he));
                            }
                        }
                        let got_hits: Vec<(BufferId, usize, usize)> = hits
                            .iter()
                            .map(|h| (h.buffer, h.start_byte, h.end_byte))
                            .collect();
                        assert_eq!(got_hits, exp_hits);

                        // Bounded context oracle for restricted actor (must see ONLY b0!).
                        let budget_bytes = rng.gen_range(4..24);
                        let ctx = core
                            .extract_context(
                                restricted,
                                &ContextScope::all_readable(),
                                &ContextRequest::workspace(
                                    ContextBudget::new(budget_bytes).unwrap(),
                                ),
                                fmt,
                            )
                            .unwrap();
                        let b0_text = &model.live_buffers[&b0];
                        let exp_len = ref_truncate_utf8(b0_text, budget_bytes);
                        assert_eq!(ctx.pack.fragments.len(), 1);
                        assert_eq!(ctx.pack.fragments[0].buffer, b0);
                        assert_eq!(ctx.pack.fragments[0].as_str(), &b0_text[..exp_len]);
                        assert_eq!(
                            ctx.projected_spans[0],
                            (ref_pos(b0_text, 0, fmt), ref_pos(b0_text, exp_len, fmt))
                        );

                        // Exercise selection & annotation transport across randomized edits.
                        let a_idx = rng.gen_range(0..bounds.len());
                        let b_idx = rng.gen_range(0..bounds.len());
                        let (sa, sh) = (bounds[a_idx], bounds[b_idx]);
                        core.set_selection(
                            actor,
                            buf,
                            ref_region(&cur, sa, sh, EndpointMode::Exclusive, fmt),
                        )
                        .unwrap();
                        model.set_selection(actor, buf, sa, sh);

                        let (as_, ae_) = (sa.min(sh), sa.max(sh));
                        let key = AnnotationKey {
                            buffer: buf,
                            namespace: NamespaceId::new(1),
                            id: AnnotationId::new((step % 3) as u64 + 1),
                        };
                        let ann_spec = RefAnnotation {
                            start: as_,
                            end: ae_,
                            start_affinity: Affinity::Left,
                            end_affinity: Affinity::Right,
                            priority: 10,
                            collapse: CollapsePolicy::RetainEmpty,
                            label: "rand-ann".into(),
                        };
                        let ann = build_host_annotation(fmt, &cur, key, actor, &ann_spec);
                        core.set_annotation(actor, &ann).unwrap();
                        model.set_annotation(actor, key, ann_spec);

                        cov.navigation_search_context += 1;
                    }
                }

                // Periodically validate full public state against the independent reference model.
                if step.is_multiple_of(15) {
                    assert_core_matches_model(&core, &model, &all_actors, &local_snaps);
                }
            }

            // Full verification + checkpoint/restore equivalence at the end of every seed.
            assert_core_matches_model(&core, &model, &all_actors, &local_snaps);
            let cp = core.checkpoint().unwrap();
            let restored = EditorCore::restore(&cp).unwrap();
            // After restore, live clients ahead of floor still match; behind floor require reconnect.
            for &cid in &client_ids {
                let rec = restored.reconnect(cid).unwrap();
                assert!(local_snaps
                    .get_mut(&cid)
                    .unwrap()
                    .apply_reconnect(&rec)
                    .unwrap());
                let c = model.clients.get_mut(&cid).unwrap();
                c.revision = Revision::new(model.revision);
                c.text = model.live_buffers[&c.buffer].clone();
            }
            assert_core_matches_model(&restored, &model, &all_actors, &local_snaps);
        }

        // Assert NON-ZERO branch coverage across every single lifecycle stage.
        assert!(cov.read > 0, "read branch not covered");
        assert!(cov.propose > 0, "propose branch not covered");
        assert!(cov.approve > 0, "approve branch not covered");
        assert!(
            cov.reject_proposal > 0,
            "reject_proposal branch not covered"
        );
        assert!(cov.commit > 0, "commit branch not covered");
        assert!(cov.deliver > 0, "deliver branch not covered");
        assert!(cov.undo > 0, "undo branch not covered");
        assert!(cov.redo > 0, "redo branch not covered");
        assert!(cov.compact > 0, "compact branch not covered");
        assert!(
            cov.reconnect_deltas > 0,
            "reconnect_deltas branch not covered"
        );
        assert!(
            cov.reconnect_snapshot > 0,
            "reconnect_snapshot branch not covered"
        );
        assert!(cov.conflict > 0, "conflict branch not covered");
        assert!(
            cov.permission_denied > 0,
            "permission_denied branch not covered"
        );
        assert!(
            cov.navigation_search_context > 0,
            "navigation_search_context branch not covered"
        );

        // Part B: Randomized lockstep equivalence across Emacs, Neovim, and Agent cores!
        let mut lockstep_views = Vec::new();
        for format in THREE_FORMATS {
            let mut rng = Rng::new(20261126);
            let core = EditorCore::new();
            core.register_actor(h0, ActorRole::Human).unwrap();
            core.register_actor(a0, ActorRole::Agent).unwrap();
            for cap in Capability::ALL {
                core.grant_default(cap).unwrap();
            }
            let mut cur_text = "alpha界😀\nbeta e\u{0301}\ngamma".to_owned();
            let buf = core
                .open_buffer(&OpenBufferRequest {
                    request: RequestId::new(1),
                    actor: h0,
                    nonce: Nonce::new(1),
                    base: Revision::ZERO,
                    name: "lockstep.txt".into(),
                    text: Text::from_str(&cur_text),
                })
                .unwrap()
                .buffer;
            let (cid, mut local_snap) = core.subscribe(a0, buf, format).unwrap();
            let ann_key = AnnotationKey {
                buffer: buf,
                namespace: NamespaceId::new(1),
                id: AnnotationId::new(1),
            };
            let mut rejections = Vec::new();
            let mut rev = 1u64;
            for step in 0..24usize {
                let (s, e, repl) = pick_nonempty_span_and_repl(&mut rng, &cur_text);
                let id = 2 + step as u64;
                if step.is_multiple_of(3) {
                    // Propose -> Approve -> Commit
                    core.propose(&ProposalRequest {
                        request: RequestId::new(id),
                        actor: a0,
                        nonce: Nonce::new(id),
                        base: Revision::new(rev),
                        payload: ProposalPayload::Commands {
                            buffer: buf,
                            commands: build_replace_commands(format, &cur_text, s, e, &repl),
                        },
                    })
                    .unwrap();
                    // Agent cannot approve -> identical rejection across hosts.
                    rejections.push(
                        core.approve_proposal(RequestId::new(id), a0)
                            .unwrap_err()
                            .kind(),
                    );
                    core.approve_proposal(RequestId::new(id), h0).unwrap();
                    core.commit_proposal(RequestId::new(id)).unwrap();
                } else {
                    core.apply_edit(&EditRequest {
                        request: RequestId::new(id),
                        actor: h0,
                        nonce: Nonce::new(id),
                        base: Revision::new(rev),
                        edit: EditSpec {
                            buffer: buf,
                            region: ref_region(&cur_text, s, e, EndpointMode::Exclusive, format),
                            replacement: Text::from_str(&repl),
                        },
                        sequence_step: None,
                    })
                    .unwrap();
                }
                cur_text.replace_range(s..e, &repl);
                rev += 1;
                if step.is_multiple_of(4) {
                    let del = core.deliver_incremental(cid).unwrap();
                    assert!(local_snap.apply_delivery(&del).unwrap());
                } else if step == 17 {
                    core.compact(Revision::new(rev)).unwrap();
                    rejections.push(core.deliver_incremental(cid).unwrap_err().kind());
                    let rec = core.reconnect(cid).unwrap();
                    assert!(local_snap.apply_reconnect(&rec).unwrap());
                }
                let ann_spec = RefAnnotation {
                    start: 0,
                    end: cur_text.len(),
                    start_affinity: Affinity::Left,
                    end_affinity: Affinity::Right,
                    priority: 5,
                    collapse: CollapsePolicy::RetainEmpty,
                    label: "ls".into(),
                };
                let ann = build_host_annotation(format, &cur_text, ann_key, h0, &ann_spec);
                core.set_annotation(h0, &ann).unwrap();
            }
            let final_rec = core.reconnect(cid).unwrap();
            assert!(local_snap.apply_reconnect(&final_rec).unwrap());
            assert_eq!(local_snap.decode().unwrap().text.as_str(), cur_text);
            lockstep_views.push((
                format,
                extract_host_equiv(
                    &core,
                    format,
                    &EquivQuery {
                        actors: &[h0, a0],
                        ann_keys: &[(h0, ann_key)],
                        search_actor: a0,
                        search_pattern: "a",
                        context_actor: a0,
                    },
                    rejections,
                ),
            ));
        }
        assert_hosts_equivalent(&lockstep_views);
    }

    // ======================================================================
    // Test 6: Multi-Threaded Concurrent Public Entrypoints (Linearizability)
    // ======================================================================

    #[test]
    fn multithreaded_concurrent_public_entrypoints() {
        let h1 = ActorId::new(1);
        let h2 = ActorId::new(2);
        let a1 = ActorId::new(3);
        let a2 = ActorId::new(4);

        // Round A: Atomic multi-buffer batch writers vs concurrent multi-format readers
        // and subscriber delivery (zero torn reads across paired buffers).
        {
            let core = EditorCore::new();
            core.register_actor(h1, ActorRole::Human).unwrap();
            core.register_actor(h2, ActorRole::Human).unwrap();
            core.register_actor(a1, ActorRole::Agent).unwrap();
            for cap in Capability::ALL {
                core.grant_default(cap).unwrap();
            }
            let b0 = core
                .open_buffer(&OpenBufferRequest {
                    request: RequestId::new(1),
                    actor: h1,
                    nonce: Nonce::new(1),
                    base: Revision::ZERO,
                    name: "pair0".into(),
                    text: Text::from_str("0000"),
                })
                .unwrap()
                .buffer;
            let b1 = core
                .open_buffer(&OpenBufferRequest {
                    request: RequestId::new(2),
                    actor: h1,
                    nonce: Nonce::new(2),
                    base: Revision::new(1),
                    name: "pair1".into(),
                    text: Text::from_str("0000"),
                })
                .unwrap()
                .buffer;
            let (sub_cid, mut sub_snap) = core.subscribe(a1, b0, HostFormat::Neovim).unwrap();
            let done = std::sync::atomic::AtomicBool::new(false);
            let barrier = Arc::new(Barrier::new(6));

            std::thread::scope(|scope| {
                // 2 writers (Emacs and Neovim) updating (b0, b1) pairwise.
                for (w_idx, (actor, fmt)) in [(h1, HostFormat::Emacs), (h2, HostFormat::Neovim)]
                    .into_iter()
                    .enumerate()
                {
                    let core = core.clone();
                    let barrier = Arc::clone(&barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        let mut committed = 0u64;
                        let mut attempt = 0u64;
                        while committed < 12 {
                            attempt += 1;
                            let snap = core.read_agent(actor, b0).unwrap();
                            let token = format!("w{w_idx}{committed:02}");
                            let id = 1_000 * (w_idx as u64 + 1) + attempt;
                            let res = core.apply_multi_buffer_batch(&MultiBufferBatchRequest {
                                request: RequestId::new(id),
                                actor,
                                nonce: Nonce::new(id),
                                base: snap.revision,
                                primary: b0,
                                edits: vec![
                                    EditSpec {
                                        buffer: b0,
                                        region: ref_region(
                                            &snap.text,
                                            0,
                                            snap.text.len(),
                                            EndpointMode::Exclusive,
                                            fmt,
                                        ),
                                        replacement: Text::from_str(&token),
                                    },
                                    EditSpec {
                                        buffer: b1,
                                        region: ref_region(
                                            &snap.text,
                                            0,
                                            snap.text.len(),
                                            EndpointMode::Exclusive,
                                            fmt,
                                        ),
                                        replacement: Text::from_str(&token),
                                    },
                                ],
                                sequence_step: None,
                            });
                            match res {
                                Ok(_) => committed += 1,
                                Err(err) => {
                                    assert!(
                                        matches!(
                                            err.kind(),
                                            CoreErrorKind::Conflict | CoreErrorKind::NoChange
                                        ),
                                        "unexpected concurrent writer error: {err}"
                                    );
                                }
                            }
                        }
                    });
                }

                // 3 readers (Emacs, Neovim, Agent) verifying zero torn reads across b0 & b1.
                for fmt in THREE_FORMATS {
                    let core = core.clone();
                    let barrier = Arc::clone(&barrier);
                    let done = &done;
                    scope.spawn(move || {
                        barrier.wait();
                        for _ in 0..40 {
                            let ctx = core
                                .extract_context(
                                    a1,
                                    &ContextScope::all_readable(),
                                    &ContextRequest::workspace(ContextBudget::new(128).unwrap()),
                                    fmt,
                                )
                                .unwrap();
                            assert_eq!(ctx.pack.fragments.len(), 2);
                            assert_eq!(
                                ctx.pack.fragments[0].as_str(),
                                ctx.pack.fragments[1].as_str(),
                                "torn multi-buffer read detected in {fmt}"
                            );
                            let cp = core.checkpoint().unwrap();
                            let restored = EditorCore::restore(&cp).unwrap();
                            assert_eq!(
                                restored.read_agent(a1, b0).unwrap().text,
                                restored.read_agent(a1, b1).unwrap().text,
                                "torn checkpoint detected"
                            );
                            if done.load(std::sync::atomic::Ordering::Acquire) {
                                break;
                            }
                        }
                    });
                }

                // 1 subscriber thread catching up concurrently with writers.
                let sub_core = core.clone();
                let sub_barrier = Arc::clone(&barrier);
                let sub_done = &done;
                let sub_handle = scope.spawn(move || {
                    sub_barrier.wait();
                    while !sub_done.load(std::sync::atomic::Ordering::Acquire) {
                        let del = sub_core.deliver_incremental(sub_cid).unwrap();
                        assert!(sub_snap.apply_delivery(&del).unwrap());
                        std::thread::yield_now();
                    }
                    let final_del = sub_core.deliver_incremental(sub_cid).unwrap();
                    assert!(sub_snap.apply_delivery(&final_del).unwrap());
                    sub_snap
                });

                // Signal readers/subscriber once writers finish (scoped threads join at end;
                // use a helper thread or wait via separate scope for writers — instead, run
                // a small monitor loop: wait until revision reaches 2 + 24 = 26).
                while core.revision().unwrap().raw() < 26 {
                    std::thread::yield_now();
                }
                done.store(true, std::sync::atomic::Ordering::Release);
                let final_sub_snap = sub_handle.join().unwrap();
                assert_eq!(
                    final_sub_snap.decode().unwrap().text.as_str(),
                    core.read_agent(a1, b0).unwrap().text
                );
                assert_eq!(
                    core.read_agent(a1, b0).unwrap().text,
                    core.read_agent(a1, b1).unwrap().text
                );
            });
            assert_eq!(core.revision().unwrap(), Revision::new(26));
        }

        // Round B: Idempotent retry storm + disjoint rebasing + overlapping conflict race + reuse rejection.
        {
            let core = EditorCore::new();
            for &(actor, role) in &[
                (h1, ActorRole::Human),
                (h2, ActorRole::Human),
                (a1, ActorRole::Agent),
                (a2, ActorRole::Agent),
            ] {
                core.register_actor(actor, role).unwrap();
            }
            for cap in Capability::ALL {
                core.grant_default(cap).unwrap();
            }
            let init = "aaaa bbbb cccc dddd";
            let buf = core
                .open_buffer(&OpenBufferRequest {
                    request: RequestId::new(1),
                    actor: h1,
                    nonce: Nonce::new(1),
                    base: Revision::ZERO,
                    name: "race.txt".into(),
                    text: Text::from_str(init),
                })
                .unwrap()
                .buffer;

            let retry_req = EditRequest {
                request: RequestId::new(100),
                actor: h1,
                nonce: Nonce::new(100),
                base: Revision::new(1),
                edit: EditSpec {
                    buffer: buf,
                    region: ref_region(init, 0, 4, EndpointMode::Exclusive, HostFormat::Emacs),
                    replacement: Text::from_str("AAAA"),
                },
                sequence_step: None,
            };

            let barrier = Arc::new(Barrier::new(8));
            let (retry_results, disjoint_ok, conflict_outcomes) = std::thread::scope(|scope| {
                let mut retry_handles = Vec::new();
                for _ in 0..4 {
                    let c = core.clone();
                    let b = Arc::clone(&barrier);
                    let r = retry_req.clone();
                    retry_handles.push(scope.spawn(move || {
                        b.wait();
                        c.apply_edit(&r).unwrap()
                    }));
                }

                // 2 disjoint concurrent edits at base rev 1 -> BOTH must succeed via rebase!
                let c_d1 = core.clone();
                let b_d1 = Arc::clone(&barrier);
                let d1 = scope.spawn(move || {
                    b_d1.wait();
                    c_d1.apply_edit(&EditRequest {
                        request: RequestId::new(101),
                        actor: h2,
                        nonce: Nonce::new(101),
                        base: Revision::new(1),
                        edit: EditSpec {
                            buffer: buf,
                            region: ref_region(
                                init,
                                5,
                                9,
                                EndpointMode::Exclusive,
                                HostFormat::Neovim,
                            ),
                            replacement: Text::from_str("BBBB"),
                        },
                        sequence_step: None,
                    })
                    .unwrap()
                });
                let c_d2 = core.clone();
                let b_d2 = Arc::clone(&barrier);
                let d2 = scope.spawn(move || {
                    b_d2.wait();
                    c_d2.apply_edit(&EditRequest {
                        request: RequestId::new(102),
                        actor: a1,
                        nonce: Nonce::new(102),
                        base: Revision::new(1),
                        edit: EditSpec {
                            buffer: buf,
                            region: ref_region(
                                init,
                                15,
                                19,
                                EndpointMode::Exclusive,
                                HostFormat::AGENT_BYTE,
                            ),
                            replacement: Text::from_str("DDDD"),
                        },
                        sequence_step: None,
                    })
                    .unwrap()
                });

                // 2 conflicting concurrent edits on "cccc" (10..14) at base rev 1 -> EXACTLY ONE wins!
                let c_k1 = core.clone();
                let b_k1 = Arc::clone(&barrier);
                let k1 = scope.spawn(move || {
                    b_k1.wait();
                    c_k1.apply_edit(&EditRequest {
                        request: RequestId::new(103),
                        actor: a1,
                        nonce: Nonce::new(103),
                        base: Revision::new(1),
                        edit: EditSpec {
                            buffer: buf,
                            region: ref_region(
                                init,
                                10,
                                14,
                                EndpointMode::Exclusive,
                                HostFormat::AGENT_UTF16,
                            ),
                            replacement: Text::from_str("CCC1"),
                        },
                        sequence_step: None,
                    })
                });
                let c_k2 = core.clone();
                let b_k2 = Arc::clone(&barrier);
                let k2 = scope.spawn(move || {
                    b_k2.wait();
                    c_k2.apply_edit(&EditRequest {
                        request: RequestId::new(104),
                        actor: a2,
                        nonce: Nonce::new(104),
                        base: Revision::new(1),
                        edit: EditSpec {
                            buffer: buf,
                            region: ref_region(
                                init,
                                10,
                                14,
                                EndpointMode::Exclusive,
                                HostFormat::Emacs,
                            ),
                            replacement: Text::from_str("CCC2"),
                        },
                        sequence_step: None,
                    })
                });

                let retries: Vec<CommitOutcome> = retry_handles
                    .into_iter()
                    .map(|h| h.join().unwrap())
                    .collect();
                let _ = (d1.join().unwrap(), d2.join().unwrap());
                (retries, true, (k1.join().unwrap(), k2.join().unwrap()))
            });

            assert!(disjoint_ok);
            for r in &retry_results[1..] {
                assert_eq!(
                    r, &retry_results[0],
                    "idempotent retries must return identical outcome"
                );
            }
            match conflict_outcomes {
                (Ok(_), Err(e)) | (Err(e), Ok(_)) => assert_eq!(e.kind(), CoreErrorKind::Conflict),
                other => panic!("expected exactly one conflict winner, got {other:?}"),
            }
            assert_eq!(core.revision().unwrap(), Revision::new(5));
            let final_text = core.read_agent(h1, buf).unwrap().text;
            assert!(
                final_text == "AAAA BBBB CCC1 DDDD" || final_text == "AAAA BBBB CCC2 DDDD",
                "unexpected linearizable result: {final_text:?}"
            );

            // Conflicting RequestId reuse storm must fail 100% with RequestReuse.
            let reuse_barrier = Arc::new(Barrier::new(4));
            std::thread::scope(|scope| {
                for i in 0..4 {
                    let c = core.clone();
                    let b = Arc::clone(&reuse_barrier);
                    let mut diff = retry_req.clone();
                    diff.edit.replacement = Text::from_str(&format!("BAD{i}"));
                    scope.spawn(move || {
                        b.wait();
                        assert_eq!(
                            c.apply_edit(&diff).unwrap_err().kind(),
                            CoreErrorKind::RequestReuse
                        );
                    });
                }
            });
            assert_eq!(core.read_agent(h1, buf).unwrap().text, final_text);
        }

        // Round C & D: Concurrent proposal approve/commit vs reject race, then concurrent revoke_actor.
        {
            let core = EditorCore::new();
            core.register_actor(h1, ActorRole::Human).unwrap();
            core.register_actor(h2, ActorRole::Human).unwrap();
            core.register_actor(a1, ActorRole::Agent).unwrap();
            for cap in Capability::ALL {
                core.grant_default(cap).unwrap();
            }
            let buf = core
                .open_buffer(&OpenBufferRequest {
                    request: RequestId::new(1),
                    actor: h1,
                    nonce: Nonce::new(1),
                    base: Revision::ZERO,
                    name: "prop.txt".into(),
                    text: Text::from_str("hello"),
                })
                .unwrap()
                .buffer;
            let prop_req = RequestId::new(10);
            core.propose(&ProposalRequest {
                request: prop_req,
                actor: a1,
                nonce: Nonce::new(10),
                base: Revision::new(1),
                payload: ProposalPayload::Edit(EditSpec {
                    buffer: buf,
                    region: ref_region("hello", 0, 5, EndpointMode::Exclusive, HostFormat::Emacs),
                    replacement: Text::from_str("HELLO"),
                }),
            })
            .unwrap();

            let barrier = Arc::new(Barrier::new(3));
            let (commit_res, reject_res, agent_res) = std::thread::scope(|scope| {
                let c1 = core.clone();
                let b1 = Arc::clone(&barrier);
                let t1 = scope.spawn(move || {
                    b1.wait();
                    match c1.approve_proposal(prop_req, h1) {
                        Ok(_) => c1.commit_proposal(prop_req),
                        Err(e) => Err(e),
                    }
                });
                let c2 = core.clone();
                let b2 = Arc::clone(&barrier);
                let t2 = scope.spawn(move || {
                    b2.wait();
                    c2.reject_proposal(prop_req, h2, Some("veto".into()))
                });
                let c3 = core.clone();
                let b3 = Arc::clone(&barrier);
                let t3 = scope.spawn(move || {
                    b3.wait();
                    (
                        c3.approve_proposal(prop_req, a1),
                        c3.reject_proposal(prop_req, a1, None),
                    )
                });
                (t1.join().unwrap(), t2.join().unwrap(), t3.join().unwrap())
            });

            // Agent MUST never succeed at approving or rejecting.
            assert!(agent_res.0.is_err());
            assert!(agent_res.1.is_err());
            let info = core.proposal_info(prop_req).unwrap().unwrap();
            match (&commit_res, &reject_res, &info.status) {
                (Ok(_), Err(re), ProposalStatus::Committed { .. }) => {
                    assert_eq!(re.kind(), CoreErrorKind::ProposalAlreadyCommitted);
                    assert_eq!(core.read_agent(h1, buf).unwrap().text, "HELLO");
                }
                (Err(ce), Ok(_), ProposalStatus::Rejected { reviewer, .. }) => {
                    assert_eq!(ce.kind(), CoreErrorKind::ProposalRejected);
                    assert_eq!(*reviewer, h2);
                    assert_eq!(core.read_agent(h1, buf).unwrap().text, "hello");
                }
                other => panic!("non-linearizable proposal race outcome: {other:?}"),
            }

            // Concurrent revoke_actor(a1) vs Agent reads/deliveries/context.
            let (a_cid, _) = core.subscribe(a1, buf, HostFormat::AGENT_BYTE).unwrap();
            let rev_barrier = Arc::new(Barrier::new(3));
            std::thread::scope(|scope| {
                let c_rev = core.clone();
                let b_rev = Arc::clone(&rev_barrier);
                scope.spawn(move || {
                    b_rev.wait();
                    c_rev.revoke_actor(a1).unwrap();
                });
                for _ in 0..2 {
                    let c_ag = core.clone();
                    let b_ag = Arc::clone(&rev_barrier);
                    scope.spawn(move || {
                        b_ag.wait();
                        for _ in 0..32 {
                            match c_ag.read_emacs(a1, buf) {
                                Ok(snap) => {
                                    assert!(snap.text == "hello" || snap.text == "HELLO");
                                }
                                Err(e) => assert_eq!(e.kind(), CoreErrorKind::RevokedActor),
                            }
                            match c_ag.deliver_incremental(a_cid) {
                                Ok(_) => {}
                                Err(e) => assert_eq!(e.kind(), CoreErrorKind::RevokedActor),
                            }
                        }
                    });
                }
            });
            assert!(core.is_actor_revoked(a1).unwrap());
            assert_eq!(
                core.read_agent(a1, buf).unwrap_err().kind(),
                CoreErrorKind::RevokedActor
            );
            assert_eq!(
                core.reconnect(a_cid).unwrap_err().kind(),
                CoreErrorKind::RevokedActor
            );
        }
    }

    // ======================================================================
    // Test 7: Negative Self-Checks (Falsifiability of Reference Comparator,
    // Rejection-Atomicity Detector, and Cross-Host Equivalence Oracle)
    // ======================================================================

    #[test]
    fn negative_self_checks_detect_injected_divergence_and_drift() {
        let human = ActorId::new(1);
        let agent = ActorId::new(2);
        let actors = [human, agent];

        // Construct via EditorCore::with_initial to cover pre-populated registry & policy too.
        let mut registry = ActorRegistry::new();
        registry.register(human, ActorRole::Human).unwrap();
        registry.register(agent, ActorRole::Agent).unwrap();
        let mut policy = AccessPolicy::new();
        for cap in Capability::ALL {
            policy.grant_default(cap);
        }
        let core = EditorCore::with_initial(registry, policy);
        let mut model = RefModel::default();
        model.register_actor(human, ActorRole::Human);
        model.register_actor(agent, ActorRole::Agent);
        for cap in Capability::ALL {
            model.apply_access(AccessChange::GrantDefault(cap));
        }

        let buf = core
            .open_buffer(&OpenBufferRequest {
                request: RequestId::new(1),
                actor: human,
                nonce: Nonce::new(1),
                base: Revision::ZERO,
                name: "neg.txt".into(),
                text: Text::from_str("hello界"),
            })
            .unwrap()
            .buffer;
        model.open_buffer(
            RequestId::new(1),
            human,
            Nonce::new(1),
            "neg.txt",
            "hello界",
        );

        core.set_selection(
            human,
            buf,
            ref_region("hello界", 0, 5, EndpointMode::Exclusive, HostFormat::Emacs),
        )
        .unwrap();
        model.set_selection(human, buf, 0, 5);

        let ann_key = AnnotationKey {
            buffer: buf,
            namespace: NamespaceId::new(1),
            id: AnnotationId::new(1),
        };
        let ann_spec = RefAnnotation {
            start: 0,
            end: 5,
            start_affinity: Affinity::Left,
            end_affinity: Affinity::Right,
            priority: 7,
            collapse: CollapsePolicy::Evict,
            label: "tag".into(),
        };
        let ann = build_host_annotation(HostFormat::Emacs, "hello界", ann_key, human, &ann_spec);
        core.set_annotation(human, &ann).unwrap();
        model.set_annotation(human, ann_key, ann_spec);

        let prop_req = RequestId::new(2);
        core.propose(&ProposalRequest {
            request: prop_req,
            actor: agent,
            nonce: Nonce::new(2),
            base: Revision::new(1),
            payload: ProposalPayload::Edit(EditSpec {
                buffer: buf,
                region: ref_region(
                    "hello界",
                    0,
                    5,
                    EndpointMode::Exclusive,
                    HostFormat::AGENT_BYTE,
                ),
                replacement: Text::from_str("HELLO"),
            }),
        })
        .unwrap();
        model.proposals.insert(
            prop_req,
            RefProposal {
                proposer: agent,
                txn: TxnId::new(agent, Nonce::new(2)),
                base_revision: Revision::new(1),
                buffers: vec![buf],
                status: ProposalStatus::Pending,
            },
        );

        let (cid, init_snap) = core.subscribe(agent, buf, HostFormat::Neovim).unwrap();
        let mut local_snaps = BTreeMap::new();
        local_snaps.insert(cid, init_snap);
        model.clients.insert(
            cid,
            RefClient {
                actor: agent,
                buffer: buf,
                format: HostFormat::Neovim,
                revision: Revision::new(1),
                text: "hello界".into(),
                closed: false,
            },
        );

        // Positive baseline: synchronized core & reference model MUST pass.
        assert_core_matches_model(&core, &model, &actors, &local_snaps);

        // 1. Reference comparator MUST panic on every injected model divergence!
        let mut bad_rev = model.clone();
        bad_rev.revision += 1;
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                assert_core_matches_model(&core, &bad_rev, &actors, &local_snaps)
            }))
            .is_err(),
            "reference comparator failed to catch revision divergence"
        );

        let mut bad_text = model.clone();
        bad_text.live_buffers.insert(buf, "HELLO界".into());
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                assert_core_matches_model(&core, &bad_text, &actors, &local_snaps)
            }))
            .is_err(),
            "reference comparator failed to catch text divergence"
        );

        let mut bad_sel = model.clone();
        bad_sel.selections.insert((human, buf), (0, 4));
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                assert_core_matches_model(&core, &bad_sel, &actors, &local_snaps)
            }))
            .is_err(),
            "reference comparator failed to catch selection divergence"
        );

        let mut bad_ann = model.clone();
        bad_ann
            .annotations
            .get_mut(&(human, ann_key))
            .unwrap()
            .label = "drifted".into();
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                assert_core_matches_model(&core, &bad_ann, &actors, &local_snaps)
            }))
            .is_err(),
            "reference comparator failed to catch annotation divergence"
        );

        let mut bad_prop = model.clone();
        bad_prop.proposals.get_mut(&prop_req).unwrap().status = ProposalStatus::Rejected {
            reviewer: human,
            reason: None,
        };
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                assert_core_matches_model(&core, &bad_prop, &actors, &local_snaps)
            }))
            .is_err(),
            "reference comparator failed to catch proposal status divergence"
        );

        let mut bad_local = local_snaps.clone();
        if let HostSnapshot::Neovim(nv) = bad_local.get_mut(&cid).unwrap() {
            nv.lines = vec!["drifted".into()];
        }
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                assert_core_matches_model(&core, &model, &actors, &bad_local)
            }))
            .is_err(),
            "reference comparator failed to catch local subscriber snapshot drift"
        );

        // 2. Rejection-atomicity detector MUST panic when an operation mutates public state before returning Err!
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                assert_rejection_atomic(
                    &core,
                    &actors,
                    &[buf],
                    &[prop_req],
                    &[cid],
                    CoreErrorKind::Conflict,
                    || -> Result<(), CoreError> {
                        // Mutate real public state (selection) then return an Err.
                        core.set_selection(
                            human,
                            buf,
                            ref_region("hello界", 1, 5, EndpointMode::Exclusive, HostFormat::Emacs),
                        )
                        .unwrap();
                        Err(CoreError::new(
                            CoreErrorKind::Conflict,
                            "injected state drift",
                        ))
                    },
                )
            }))
            .is_err(),
            "rejection-atomicity detector failed to catch real state mutation"
        );

        // 3. Cross-host equivalence oracle MUST panic on every injected field divergence!
        let query = EquivQuery {
            actors: &actors,
            ann_keys: &[(human, ann_key)],
            search_actor: agent,
            search_pattern: "hello",
            context_actor: agent,
        };
        let view_em = extract_host_equiv(
            &core,
            HostFormat::Emacs,
            &query,
            vec![CoreErrorKind::Conflict],
        );
        let view_nv = extract_host_equiv(
            &core,
            HostFormat::Neovim,
            &query,
            vec![CoreErrorKind::Conflict],
        );
        // Positive baseline passes.
        assert_hosts_equivalent(&[
            (HostFormat::Emacs, view_em.clone()),
            (HostFormat::Neovim, view_nv.clone()),
        ]);

        let mut div_rev = view_nv.clone();
        div_rev.revision = Revision::new(99);
        assert!(catch_unwind(AssertUnwindSafe(|| {
            assert_hosts_equivalent(&[
                (HostFormat::Emacs, view_em.clone()),
                (HostFormat::Neovim, div_rev),
            ])
        }))
        .is_err());

        let mut div_buf = view_nv.clone();
        div_buf.live_buffers.insert(buf, "drifted".into());
        assert!(catch_unwind(AssertUnwindSafe(|| {
            assert_hosts_equivalent(&[
                (HostFormat::Emacs, view_em.clone()),
                (HostFormat::Neovim, div_buf),
            ])
        }))
        .is_err());

        let mut div_hits = view_nv.clone();
        div_hits.search_hits.clear();
        assert!(catch_unwind(AssertUnwindSafe(|| {
            assert_hosts_equivalent(&[
                (HostFormat::Emacs, view_em.clone()),
                (HostFormat::Neovim, div_hits),
            ])
        }))
        .is_err());

        let mut div_ctx = view_nv.clone();
        div_ctx.context_pack = None;
        assert!(catch_unwind(AssertUnwindSafe(|| {
            assert_hosts_equivalent(&[
                (HostFormat::Emacs, view_em.clone()),
                (HostFormat::Neovim, div_ctx),
            ])
        }))
        .is_err());

        let mut div_rej = view_nv;
        div_rej.rejections.push(CoreErrorKind::PermissionDenied);
        assert!(catch_unwind(AssertUnwindSafe(|| {
            assert_hosts_equivalent(&[(HostFormat::Emacs, view_em), (HostFormat::Neovim, div_rej)])
        }))
        .is_err());
    }

    // ======================================================================
    // Test 8: Restore Preserves Origin-Bound Snapshots & Live Subscriptions
    // ======================================================================

    #[test]
    fn regression_restore_preserves_origin_snapshots_and_live_clients() {
        let human = ActorId::new(1);
        let agent = ActorId::new(2);

        for format in ALL_FORMATS {
            // A. Restore after Open (floor = 0 < delivery_floor = 1 < tip = 3):
            // retained snapshots at rev 1 & 2 MUST share the restored origin so recover_buffer
            // succeeds and live clients at rev 1 & 2 stay ClientTrack::Live!
            let core = EditorCore::new();
            core.register_actor(human, ActorRole::Human).unwrap();
            core.register_actor(agent, ActorRole::Agent).unwrap();
            for cap in Capability::ALL {
                core.grant_default(cap).unwrap();
            }

            let text_r1 = "alpha 界\n";
            let buf_a = core
                .open_buffer(&OpenBufferRequest {
                    request: RequestId::new(1),
                    actor: human,
                    nonce: Nonce::new(1),
                    base: Revision::ZERO,
                    name: "a.txt".into(),
                    text: Text::from_str(text_r1),
                })
                .unwrap()
                .buffer;
            let (client_r1, mut local_r1) = core.subscribe(human, buf_a, format).unwrap();

            core.apply_edit(&EditRequest {
                request: RequestId::new(2),
                actor: human,
                nonce: Nonce::new(2),
                base: Revision::new(1),
                edit: EditSpec {
                    buffer: buf_a,
                    region: ref_region(text_r1, 0, 5, EndpointMode::Exclusive, format),
                    replacement: Text::from_str("ALPHA"),
                },
                sequence_step: None,
            })
            .unwrap();
            let text_r2 = "ALPHA 界\n";
            let (client_r2, mut local_r2) = core.subscribe(agent, buf_a, format).unwrap();

            core.apply_edit(&EditRequest {
                request: RequestId::new(3),
                actor: agent,
                nonce: Nonce::new(3),
                base: Revision::new(2),
                edit: EditSpec {
                    buffer: buf_a,
                    region: ref_region(text_r2, 9, 9, EndpointMode::Exclusive, format),
                    replacement: Text::from_str("!"),
                },
                sequence_step: None,
            })
            .unwrap();
            let text_r3 = "ALPHA 界!\n";

            let cp = core.checkpoint().unwrap();
            let restored = EditorCore::restore(&cp).unwrap();
            assert_eq!(restored.floor().unwrap(), Revision::ZERO);
            assert_eq!(restored.revision().unwrap(), Revision::new(3));

            for rev in [Revision::new(1), Revision::new(2), Revision::new(3)] {
                let recovered = restored
                    .recover_buffer(human, buf_a, rev, format)
                    .unwrap()
                    .unwrap();
                let decoded = recovered.decode().unwrap();
                assert_eq!(decoded.revision, Revision::new(3));
                assert_eq!(decoded.text.as_str(), text_r3);
            }

            // client_r1 (rev 1 == delivery_floor < tip) remains live and catches up incrementally.
            let proj_r1 = restored.client_snapshot(client_r1).unwrap();
            assert_eq!(proj_r1.revision(), Revision::new(1));
            assert_eq!(proj_r1.decode().unwrap().text.as_str(), text_r1);
            let del_r1 = restored.deliver_incremental(client_r1).unwrap();
            assert_eq!(del_r1.previous_revision, Revision::new(1));
            assert_eq!(del_r1.revision, Revision::new(3));
            assert_eq!(del_r1.notifications.len(), 2);
            assert!(local_r1.apply_delivery(&del_r1).unwrap());
            assert_eq!(local_r1.decode().unwrap().text.as_str(), text_r3);
            assert_eq!(restored.client_snapshot(client_r1).unwrap(), local_r1);

            // client_r2 (delivery_floor < rev 2 < tip) remains live and reconnects with Deltas.
            let proj_r2 = restored.client_snapshot(client_r2).unwrap();
            assert_eq!(proj_r2.revision(), Revision::new(2));
            assert_eq!(proj_r2.decode().unwrap().text.as_str(), text_r2);
            let rec_r2 = restored.reconnect(client_r2).unwrap();
            assert!(matches!(rec_r2, ReconnectOutcome::Deltas(_)));
            assert!(local_r2.apply_reconnect(&rec_r2).unwrap());
            assert_eq!(local_r2.decode().unwrap().text.as_str(), text_r3);
            assert_eq!(restored.client_snapshot(client_r2).unwrap(), local_r2);

            // B. Restore after a second Open AND after Close (floor < old_rev < delivery_floor < tip):
            // clients below delivery_floor become Restored (ResyncRequired -> Snapshot),
            // clients at delivery_floor <= rev < tip stay Live, and recover_buffer works across all retained revs!
            let core2 = EditorCore::new();
            core2.register_actor(human, ActorRole::Human).unwrap();
            core2.register_actor(agent, ActorRole::Agent).unwrap();
            for cap in Capability::ALL {
                core2.grant_default(cap).unwrap();
            }

            let b_one = core2
                .open_buffer(&OpenBufferRequest {
                    request: RequestId::new(10),
                    actor: human,
                    nonce: Nonce::new(10),
                    base: Revision::ZERO,
                    name: "one.txt".into(),
                    text: Text::from_str("one\n"),
                })
                .unwrap()
                .buffer;
            let (pre_open_client, mut local_pre) = core2.subscribe(human, b_one, format).unwrap();
            core2
                .apply_edit(&EditRequest {
                    request: RequestId::new(11),
                    actor: human,
                    nonce: Nonce::new(11),
                    base: Revision::new(1),
                    edit: EditSpec {
                        buffer: b_one,
                        region: ref_region("one\n", 0, 3, EndpointMode::Exclusive, format),
                        replacement: Text::from_str("two"),
                    },
                    sequence_step: None,
                })
                .unwrap();
            // Opening a second buffer resets delivery_floor to 3 while compaction floor stays 0.
            let b_two = core2
                .open_buffer(&OpenBufferRequest {
                    request: RequestId::new(12),
                    actor: human,
                    nonce: Nonce::new(12),
                    base: Revision::new(2),
                    name: "two.txt".into(),
                    text: Text::from_str("second\n"),
                })
                .unwrap()
                .buffer;
            let (deliv_client, mut local_deliv) = core2.subscribe(human, b_one, format).unwrap();
            let (deliv_rec_client, mut local_deliv_rec) =
                core2.subscribe(agent, b_one, format).unwrap();
            core2
                .apply_edit(&EditRequest {
                    request: RequestId::new(13),
                    actor: human,
                    nonce: Nonce::new(13),
                    base: Revision::new(3),
                    edit: EditSpec {
                        buffer: b_one,
                        region: ref_region("two\n", 0, 3, EndpointMode::Exclusive, format),
                        replacement: Text::from_str("three"),
                    },
                    sequence_step: None,
                })
                .unwrap();

            let cp2 = core2.checkpoint().unwrap();
            let rest2 = EditorCore::restore(&cp2).unwrap();

            for rev in [Revision::new(1), Revision::new(2), Revision::new(3)] {
                let snap = rest2
                    .recover_buffer(human, b_one, rev, format)
                    .unwrap()
                    .unwrap();
                assert_eq!(snap.decode().unwrap().text.as_str(), "three\n");
            }

            // Clients at rev 3 (== delivery_floor < tip 4) stay Live.
            assert_eq!(
                rest2
                    .client_snapshot(deliv_client)
                    .unwrap()
                    .decode()
                    .unwrap()
                    .text
                    .as_str(),
                "two\n"
            );
            let del_live = rest2.deliver_incremental(deliv_client).unwrap();
            assert!(local_deliv.apply_delivery(&del_live).unwrap());
            assert_eq!(local_deliv.decode().unwrap().text.as_str(), "three\n");

            let rec_deltas = rest2.reconnect(deliv_rec_client).unwrap();
            assert!(matches!(rec_deltas, ReconnectOutcome::Deltas(_)));
            assert!(local_deliv_rec.apply_reconnect(&rec_deltas).unwrap());
            assert_eq!(local_deliv_rec.decode().unwrap().text.as_str(), "three\n");

            // Client at rev 1 (< delivery_floor 3) properly becomes Restored and recovers via Snapshot.
            assert_eq!(
                rest2
                    .deliver_incremental(pre_open_client)
                    .unwrap_err()
                    .kind(),
                CoreErrorKind::ResyncRequired
            );
            assert_eq!(
                rest2.client_snapshot(pre_open_client).unwrap_err().kind(),
                CoreErrorKind::ResyncRequired
            );
            let rec_snap = rest2.reconnect(pre_open_client).unwrap();
            assert!(matches!(rec_snap, ReconnectOutcome::Snapshot { .. }));
            assert!(local_pre.apply_reconnect(&rec_snap).unwrap());
            assert_eq!(local_pre.decode().unwrap().text.as_str(), "three\n");

            // Close b_two (rev 5, advancing delivery_floor to 5), subscribe at rev 5, edit b_one (rev 6), restore.
            rest2
                .close_buffer(&CloseBufferRequest {
                    request: RequestId::new(14),
                    actor: human,
                    nonce: Nonce::new(14),
                    base: Revision::new(4),
                    buffer: b_two,
                    policy: ClosePolicy::Discard,
                })
                .unwrap();
            let (after_close_client, mut local_after_close) =
                rest2.subscribe(human, b_one, format).unwrap();
            rest2
                .apply_edit(&EditRequest {
                    request: RequestId::new(15),
                    actor: human,
                    nonce: Nonce::new(15),
                    base: Revision::new(5),
                    edit: EditSpec {
                        buffer: b_one,
                        region: ref_region("three\n", 0, 5, EndpointMode::Exclusive, format),
                        replacement: Text::from_str("four"),
                    },
                    sequence_step: None,
                })
                .unwrap();

            let cp3 = rest2.checkpoint().unwrap();
            let rest3 = EditorCore::restore(&cp3).unwrap();
            // Closed buffer b_two observed at rev 3 recovers cleanly as None (no DifferentWorkspace).
            assert_eq!(
                rest3
                    .recover_buffer(human, b_two, Revision::new(3), format)
                    .unwrap(),
                None
            );
            assert_eq!(
                rest3
                    .recover_buffer(human, b_one, Revision::new(5), format)
                    .unwrap()
                    .unwrap()
                    .decode()
                    .unwrap()
                    .text
                    .as_str(),
                "four\n"
            );
            let del_after_close = rest3.deliver_incremental(after_close_client).unwrap();
            assert!(local_after_close.apply_delivery(&del_after_close).unwrap());
            assert_eq!(local_after_close.decode().unwrap().text.as_str(), "four\n");

            // C. Compact + Restore: client below compaction floor becomes Restored,
            // while client at compaction floor stays Live.
            let (lag_comp_client, mut local_lag_comp) =
                rest3.subscribe(human, b_one, format).unwrap();
            rest3
                .apply_edit(&EditRequest {
                    request: RequestId::new(16),
                    actor: human,
                    nonce: Nonce::new(16),
                    base: Revision::new(6),
                    edit: EditSpec {
                        buffer: b_one,
                        region: ref_region("four\n", 0, 4, EndpointMode::Exclusive, format),
                        replacement: Text::from_str("five"),
                    },
                    sequence_step: None,
                })
                .unwrap();
            let (floor_client, mut local_floor) = rest3.subscribe(agent, b_one, format).unwrap();
            rest3
                .apply_edit(&EditRequest {
                    request: RequestId::new(17),
                    actor: human,
                    nonce: Nonce::new(17),
                    base: Revision::new(7),
                    edit: EditSpec {
                        buffer: b_one,
                        region: ref_region("five\n", 0, 4, EndpointMode::Exclusive, format),
                        replacement: Text::from_str("six"),
                    },
                    sequence_step: None,
                })
                .unwrap();
            assert_eq!(rest3.compact(Revision::new(7)).unwrap(), Revision::new(7));
            let cp4 = rest3.checkpoint().unwrap();
            let rest4 = EditorCore::restore(&cp4).unwrap();

            assert_eq!(
                rest4
                    .deliver_incremental(lag_comp_client)
                    .unwrap_err()
                    .kind(),
                CoreErrorKind::ResyncRequired
            );
            let rec_comp = rest4.reconnect(lag_comp_client).unwrap();
            assert!(matches!(rec_comp, ReconnectOutcome::Snapshot { .. }));
            assert!(local_lag_comp.apply_reconnect(&rec_comp).unwrap());
            assert_eq!(local_lag_comp.decode().unwrap().text.as_str(), "six\n");

            assert_eq!(
                rest4
                    .client_snapshot(floor_client)
                    .unwrap()
                    .decode()
                    .unwrap()
                    .text
                    .as_str(),
                "five\n"
            );
            let rec_floor = rest4.reconnect(floor_client).unwrap();
            assert!(matches!(rec_floor, ReconnectOutcome::Deltas(_)));
            assert!(local_floor.apply_reconnect(&rec_floor).unwrap());
            assert_eq!(local_floor.decode().unwrap().text.as_str(), "six\n");

            assert_eq!(
                rest4
                    .recover_buffer(human, b_one, Revision::new(6), format)
                    .unwrap_err()
                    .kind(),
                CoreErrorKind::BaseExpired
            );
            assert_eq!(
                rest4
                    .recover_buffer(human, b_one, Revision::new(7), format)
                    .unwrap()
                    .unwrap()
                    .decode()
                    .unwrap()
                    .text
                    .as_str(),
                "six\n"
            );
        }
    }

    // ======================================================================
    // Test 9: Permission & Revocation Enforcement Before Decoding / Probing
    // ======================================================================

    #[test]
    fn regression_auth_enforced_before_decoding_and_existence_checks() {
        let admin = ActorId::new(1);
        let zero_cap = ActorId::new(2);
        let read_only = ActorId::new(3);
        let partial_commit = ActorId::new(4);
        let revoked = ActorId::new(5);
        let unknown = ActorId::new(99);

        let core = EditorCore::new();
        core.register_actor(admin, ActorRole::Human).unwrap();
        core.register_actor(zero_cap, ActorRole::Agent).unwrap();
        core.register_actor(read_only, ActorRole::Agent).unwrap();
        core.register_actor(partial_commit, ActorRole::Agent)
            .unwrap();
        core.register_actor(revoked, ActorRole::Human).unwrap();
        for cap in Capability::ALL {
            core.grant_actor(admin, cap).unwrap();
            core.grant_actor(revoked, cap).unwrap();
        }

        let live_buf = core
            .open_buffer(&OpenBufferRequest {
                request: RequestId::new(1),
                actor: admin,
                nonce: Nonce::new(1),
                base: Revision::ZERO,
                name: "live.txt".into(),
                text: Text::from_str("a😀z\nline2\n"),
            })
            .unwrap()
            .buffer;
        let secret_buf = core
            .open_buffer(&OpenBufferRequest {
                request: RequestId::new(2),
                actor: admin,
                nonce: Nonce::new(2),
                base: Revision::new(1),
                name: "secret.txt".into(),
                text: Text::from_str("topsecret\n"),
            })
            .unwrap()
            .buffer;
        let closed_buf = core
            .open_buffer(&OpenBufferRequest {
                request: RequestId::new(3),
                actor: admin,
                nonce: Nonce::new(3),
                base: Revision::new(2),
                name: "closed.txt".into(),
                text: Text::from_str("gone\n"),
            })
            .unwrap()
            .buffer;
        core.close_buffer(&CloseBufferRequest {
            request: RequestId::new(4),
            actor: admin,
            nonce: Nonce::new(4),
            base: Revision::new(3),
            buffer: closed_buf,
            policy: ClosePolicy::Discard,
        })
        .unwrap();

        let used_step = SequenceStep {
            sequence: TxnId::new(admin, Nonce::new(700)),
            index: 0,
        };
        core.apply_edit(&EditRequest {
            request: RequestId::new(5),
            actor: admin,
            nonce: Nonce::new(5),
            base: Revision::new(4),
            edit: EditSpec {
                buffer: live_buf,
                region: ref_region(
                    "a😀z\nline2\n",
                    0,
                    1,
                    EndpointMode::Exclusive,
                    HostFormat::AGENT_BYTE,
                ),
                replacement: Text::from_str("A"),
            },
            sequence_step: Some(used_step),
        })
        .unwrap();
        // Compact to rev 4 so rev 1 is expired (< floor).
        core.compact(Revision::new(4)).unwrap();

        core.grant_buffer(read_only, live_buf, Capability::Read)
            .unwrap();
        core.grant_buffer(partial_commit, live_buf, Capability::Commit)
            .unwrap();
        core.revoke_actor(revoked).unwrap();

        let missing_buf = BufferId::new(999);
        let tracked_actors = [admin, zero_cap, read_only, partial_commit, revoked];
        let tracked_buffers = [live_buf, secret_buf];
        let mut next_id = 100u64;
        let mut alloc = || {
            let id = next_id;
            next_id += 1;
            (RequestId::new(id), Nonce::new(id))
        };

        let bad_utf8_region = OrientedRegion {
            anchor: RegionCoord::Agent(AgentPosition::Byte(BytePos::new(2))),
            head: RegionCoord::Agent(AgentPosition::Byte(BytePos::new(2))),
            direction: Direction::Empty,
            mode: EndpointMode::Exclusive,
        };
        let bad_utf16_region = OrientedRegion {
            anchor: RegionCoord::Agent(AgentPosition::LineColUtf16(LineColUtf16::new(0, 2))),
            head: RegionCoord::Agent(AgentPosition::LineColUtf16(LineColUtf16::new(0, 2))),
            direction: Direction::Empty,
            mode: EndpointMode::Exclusive,
        };
        let bad_emacs_region = OrientedRegion {
            anchor: RegionCoord::Emacs(EmacsCharPos::new(999).unwrap()),
            head: RegionCoord::Emacs(EmacsCharPos::new(999).unwrap()),
            direction: Direction::Empty,
            mode: EndpointMode::Exclusive,
        };

        let probe_bases = [Revision::new(1), Revision::new(5), Revision::new(99)];
        let probe_buffers = [live_buf, closed_buf, missing_buf];

        for &(actor, expected) in &[
            (zero_cap, CoreErrorKind::PermissionDenied),
            (revoked, CoreErrorKind::RevokedActor),
            (unknown, CoreErrorKind::UnknownActor),
        ] {
            for &buf in &probe_buffers {
                for &base in &probe_bases {
                    // 1. apply_edit with bad UTF-8 / UTF-16 / out-of-range + colliding step.
                    for &region in &[bad_utf8_region, bad_utf16_region, bad_emacs_region] {
                        let (req, nonce) = alloc();
                        assert_rejection_atomic(
                            &core,
                            &tracked_actors,
                            &tracked_buffers,
                            &[],
                            &[],
                            expected,
                            || {
                                core.apply_edit(&EditRequest {
                                    request: req,
                                    actor,
                                    nonce,
                                    base,
                                    edit: EditSpec {
                                        buffer: buf,
                                        region,
                                        replacement: Text::from_str("x"),
                                    },
                                    sequence_step: Some(used_step),
                                })
                            },
                        );
                    }

                    // 2. apply_multi_buffer_batch with invalid coordinates.
                    let (req, nonce) = alloc();
                    assert_rejection_atomic(
                        &core,
                        &tracked_actors,
                        &tracked_buffers,
                        &[],
                        &[],
                        expected,
                        || {
                            core.apply_multi_buffer_batch(&MultiBufferBatchRequest {
                                request: req,
                                actor,
                                nonce,
                                base,
                                primary: buf,
                                edits: vec![EditSpec {
                                    buffer: buf,
                                    region: bad_utf8_region,
                                    replacement: Text::from_str("x"),
                                }],
                                sequence_step: Some(used_step),
                            })
                        },
                    );

                    // 3. execute_commands across Emacs, Neovim, Agent (read-only, edit, select, corrupt, unsupported).
                    let bad_cmds = [
                        HostCommands::Emacs(vec![EmacsCommand::Point(
                            EmacsCharPos::new(999).unwrap(),
                        )]),
                        HostCommands::Emacs(vec![
                            EmacsCommand::Point(EmacsCharPos::new(999).unwrap()),
                            EmacsCommand::Insert(Text::from_str("x")),
                        ]),
                        HostCommands::Neovim(vec![
                            NeovimCommand::Cursor(LineColByte::new(0, 2)),
                            NeovimCommand::Insert(Text::from_str("x")),
                        ]),
                        HostCommands::Agent(vec![AgentCommand::Select {
                            anchor: AgentPosition::LineColUtf16(LineColUtf16::new(0, 2)),
                            head: AgentPosition::LineColUtf16(LineColUtf16::new(0, 2)),
                        }]),
                        HostCommands::Agent(vec![
                            AgentCommand::BeginBatch,
                            AgentCommand::Replace {
                                start: AgentPosition::Byte(BytePos::new(2)),
                                end: AgentPosition::Byte(BytePos::new(2)),
                                text: Text::from_str("x"),
                            },
                        ]),
                        HostCommands::Neovim(vec![NeovimCommand::Unsupported("probe".into())]),
                    ];
                    for commands in bad_cmds {
                        let (req, nonce) = alloc();
                        assert_rejection_atomic(
                            &core,
                            &tracked_actors,
                            &tracked_buffers,
                            &[],
                            &[],
                            expected,
                            || {
                                core.execute_commands(&CommandRequest {
                                    request: req,
                                    actor,
                                    nonce,
                                    base,
                                    buffer: buf,
                                    commands,
                                    sequence_step: Some(used_step),
                                })
                            },
                        );
                    }

                    // 4. apply_rectangle_edit with invalid tab_width=0 & out-of-range HostPosition.
                    let (req, nonce) = alloc();
                    assert_rejection_atomic(
                        &core,
                        &tracked_actors,
                        &tracked_buffers,
                        &[],
                        &[],
                        expected,
                        || {
                            core.apply_rectangle_edit(&RectangleEditRequest {
                                request: req,
                                actor,
                                nonce,
                                base,
                                buffer: buf,
                                rectangle: RectangleSpec::Positions {
                                    first: HostPosition::Emacs(EmacsCharPos::new(999).unwrap()),
                                    second: HostPosition::Emacs(EmacsCharPos::new(999).unwrap()),
                                    endpoint: Endpoint::Exclusive,
                                    tab_width: 0,
                                },
                                policy: ShortLinePolicy::Error,
                                operation: RectangleEditOp::Delete,
                                sequence_step: Some(used_step),
                            })
                        },
                    );

                    // 5. apply_indent_region with inverted line range and zero tab width.
                    let (req, nonce) = alloc();
                    assert_rejection_atomic(
                        &core,
                        &tracked_actors,
                        &tracked_buffers,
                        &[],
                        &[],
                        expected,
                        || {
                            core.apply_indent_region(&IndentRegionRequest {
                                request: req,
                                actor,
                                nonce,
                                base,
                                buffer: buf,
                                spec: IndentSpec {
                                    start_line: 9,
                                    end_line: 1,
                                    tab_width: 0,
                                    style: IndentStyle::Spaces,
                                    action: IndentAction::IndentBy(2),
                                },
                                sequence_step: Some(used_step),
                            })
                        },
                    );

                    // 6. propose across all five ProposalPayload variants with invalid payloads.
                    let payloads = [
                        ProposalPayload::Edit(EditSpec {
                            buffer: buf,
                            region: bad_utf8_region,
                            replacement: Text::from_str("x"),
                        }),
                        ProposalPayload::MultiBatch {
                            primary: buf,
                            edits: vec![EditSpec {
                                buffer: buf,
                                region: bad_utf16_region,
                                replacement: Text::from_str("x"),
                            }],
                        },
                        ProposalPayload::Commands {
                            buffer: buf,
                            commands: HostCommands::Emacs(vec![EmacsCommand::Point(
                                EmacsCharPos::new(999).unwrap(),
                            )]),
                        },
                        ProposalPayload::Rectangle {
                            buffer: buf,
                            rectangle: RectangleSpec::Visual(Rectangle {
                                first: VisualPosition { line: 9, column: 9 },
                                second: VisualPosition { line: 0, column: 0 },
                                endpoint: Endpoint::Exclusive,
                                tab_width: 0,
                            }),
                            policy: ShortLinePolicy::Error,
                            operation: RectangleEditOp::Delete,
                        },
                        ProposalPayload::Indent {
                            buffer: buf,
                            spec: IndentSpec {
                                start_line: 5,
                                end_line: 1,
                                tab_width: 0,
                                style: IndentStyle::Spaces,
                                action: IndentAction::SetColumn(4),
                            },
                        },
                    ];
                    for payload in payloads {
                        let (req, nonce) = alloc();
                        assert_rejection_atomic(
                            &core,
                            &tracked_actors,
                            &tracked_buffers,
                            &[],
                            &[],
                            expected,
                            || {
                                core.propose(&ProposalRequest {
                                    request: req,
                                    actor,
                                    nonce,
                                    base,
                                    payload,
                                })
                            },
                        );
                    }

                    // 7. close_buffer must fail auth BEFORE base/liveness checks.
                    let (req, nonce) = alloc();
                    assert_rejection_atomic(
                        &core,
                        &tracked_actors,
                        &tracked_buffers,
                        &[],
                        &[],
                        expected,
                        || {
                            core.close_buffer(&CloseBufferRequest {
                                request: req,
                                actor,
                                nonce,
                                base,
                                buffer: buf,
                                policy: ClosePolicy::Discard,
                            })
                        },
                    );

                    // 8. recover_buffer must fail auth BEFORE base/liveness checks.
                    for fmt in ALL_FORMATS {
                        assert_eq!(
                            core.recover_buffer(actor, buf, base, fmt)
                                .unwrap_err()
                                .kind(),
                            expected
                        );
                    }
                }
            }
        }

        // Read-only actor on live_buf still cannot probe write/propose/close decoding errors.
        let (req, nonce) = alloc();
        assert_eq!(
            core.apply_edit(&EditRequest {
                request: req,
                actor: read_only,
                nonce,
                base: Revision::new(5),
                edit: EditSpec {
                    buffer: live_buf,
                    region: bad_utf8_region,
                    replacement: Text::from_str("x"),
                },
                sequence_step: None,
            })
            .unwrap_err()
            .kind(),
            CoreErrorKind::PermissionDenied
        );
        let (req, nonce) = alloc();
        assert_eq!(
            core.execute_commands(&CommandRequest {
                request: req,
                actor: read_only,
                nonce,
                base: Revision::new(5),
                buffer: live_buf,
                commands: HostCommands::Neovim(vec![
                    NeovimCommand::Cursor(LineColByte::new(0, 2)),
                    NeovimCommand::Insert(Text::from_str("x")),
                ]),
                sequence_step: None,
            })
            .unwrap_err()
            .kind(),
            CoreErrorKind::PermissionDenied
        );
        // Partial-commit actor (Commit on live_buf, NOT secret_buf) gets PermissionDenied on batch touching both,
        // and gets PermissionDenied on edit+select command when lacking Read!
        let (req, nonce) = alloc();
        assert_eq!(
            core.apply_multi_buffer_batch(&MultiBufferBatchRequest {
                request: req,
                actor: partial_commit,
                nonce,
                base: Revision::new(5),
                primary: live_buf,
                edits: vec![
                    EditSpec {
                        buffer: live_buf,
                        region: bad_utf8_region,
                        replacement: Text::from_str("x"),
                    },
                    EditSpec {
                        buffer: secret_buf,
                        region: bad_utf8_region,
                        replacement: Text::from_str("y"),
                    },
                ],
                sequence_step: None,
            })
            .unwrap_err()
            .kind(),
            CoreErrorKind::PermissionDenied
        );
        let (req, nonce) = alloc();
        assert_eq!(
            core.execute_commands(&CommandRequest {
                request: req,
                actor: partial_commit,
                nonce,
                base: Revision::new(5),
                buffer: live_buf,
                commands: HostCommands::Agent(vec![
                    AgentCommand::Replace {
                        start: AgentPosition::Byte(BytePos::new(0)),
                        end: AgentPosition::Byte(BytePos::new(1)),
                        text: Text::from_str("B"),
                    },
                    AgentCommand::Select {
                        anchor: AgentPosition::LineColUtf16(LineColUtf16::new(0, 2)),
                        head: AgentPosition::LineColUtf16(LineColUtf16::new(0, 2)),
                    },
                ]),
                sequence_step: None,
            })
            .unwrap_err()
            .kind(),
            CoreErrorKind::PermissionDenied
        );
    }

    // ======================================================================
    // Test 10: Request-Digest Replay, SequenceStep Binding & CommandOutcome
    // ======================================================================

    #[test]
    fn regression_request_digest_replay_sequence_step_and_command_outcome() {
        let human = ActorId::new(1);
        let worker = ActorId::new(2);
        let core = EditorCore::new();
        core.register_actor(human, ActorRole::Human).unwrap();
        core.register_actor(worker, ActorRole::Agent).unwrap();
        for cap in Capability::ALL {
            core.grant_default(cap).unwrap();
        }

        let buf_a = core
            .open_buffer(&OpenBufferRequest {
                request: RequestId::new(1),
                actor: human,
                nonce: Nonce::new(1),
                base: Revision::ZERO,
                name: "a.txt".into(),
                text: Text::from_str("  alpha\n  beta\n"),
            })
            .unwrap()
            .buffer;
        let buf_b = core
            .open_buffer(&OpenBufferRequest {
                request: RequestId::new(2),
                actor: human,
                nonce: Nonce::new(2),
                base: Revision::new(1),
                name: "b.txt".into(),
                text: Text::from_str("second\n"),
            })
            .unwrap()
            .buffer;

        // A. Committed proposal RequestId reuse MUST return RequestReuse before & after compaction
        // and after checkpoint/restore (never BaseExpired or falling through to coordinator replay).
        let prop_req = RequestId::new(10);
        let prop_edit = EditSpec {
            buffer: buf_a,
            region: ref_region(
                "  alpha\n  beta\n",
                2,
                7,
                EndpointMode::Exclusive,
                HostFormat::AGENT_BYTE,
            ),
            replacement: Text::from_str("ALPHA"),
        };
        core.propose(&ProposalRequest {
            request: prop_req,
            actor: worker,
            nonce: Nonce::new(10),
            base: Revision::new(2),
            payload: ProposalPayload::Edit(prop_edit.clone()),
        })
        .unwrap();
        core.approve_proposal(prop_req, human).unwrap();
        let prop_commit = core.commit_proposal(prop_req).unwrap();
        assert_eq!(prop_commit.receipt.committed(), Revision::new(3));
        assert_eq!(core.commit_proposal(prop_req).unwrap(), prop_commit);

        // Reuse prop_req BEFORE compaction with both identical and different payloads.
        assert_rejection_atomic(
            &core,
            &[human, worker],
            &[buf_a, buf_b],
            &[prop_req],
            &[],
            CoreErrorKind::RequestReuse,
            || {
                core.apply_edit(&EditRequest {
                    request: prop_req,
                    actor: worker,
                    nonce: Nonce::new(10),
                    base: Revision::new(2),
                    edit: prop_edit.clone(),
                    sequence_step: None,
                })
            },
        );
        assert_rejection_atomic(
            &core,
            &[human, worker],
            &[buf_a, buf_b],
            &[prop_req],
            &[],
            CoreErrorKind::RequestReuse,
            || {
                core.close_buffer(&CloseBufferRequest {
                    request: prop_req,
                    actor: human,
                    nonce: Nonce::new(10),
                    base: Revision::new(3),
                    buffer: buf_b,
                    policy: ClosePolicy::Discard,
                })
            },
        );

        // Compact past base rev 2 -> reusing prop_req MUST STILL return RequestReuse, NEVER BaseExpired!
        core.compact(Revision::new(3)).unwrap();
        assert_rejection_atomic(
            &core,
            &[human, worker],
            &[buf_a, buf_b],
            &[prop_req],
            &[],
            CoreErrorKind::RequestReuse,
            || {
                core.apply_edit(&EditRequest {
                    request: prop_req,
                    actor: worker,
                    nonce: Nonce::new(10),
                    base: Revision::new(2),
                    edit: prop_edit.clone(),
                    sequence_step: None,
                })
            },
        );
        let cp_prop = core.checkpoint().unwrap();
        let restored_prop = EditorCore::restore(&cp_prop).unwrap();
        assert_eq!(
            restored_prop
                .apply_edit(&EditRequest {
                    request: prop_req,
                    actor: worker,
                    nonce: Nonce::new(10),
                    base: Revision::new(2),
                    edit: prop_edit,
                    sequence_step: None,
                })
                .unwrap_err()
                .kind(),
            CoreErrorKind::RequestReuse
        );
        assert_eq!(
            restored_prop.commit_proposal(prop_req).unwrap(),
            prop_commit
        );

        // B. sequence_step MUST be covered by digest across all 5 request types,
        // exact retry MUST be strictly read-only, and a revoked actor MUST NEVER mutate group_plan!
        let edit_req = EditRequest {
            request: RequestId::new(20),
            actor: worker,
            nonce: Nonce::new(20),
            base: Revision::new(3),
            edit: EditSpec {
                buffer: buf_a,
                region: ref_region(
                    "  ALPHA\n  beta\n",
                    10,
                    14,
                    EndpointMode::Exclusive,
                    HostFormat::AGENT_BYTE,
                ),
                replacement: Text::from_str("BETA"),
            },
            sequence_step: None,
        };
        let edit_out = core.apply_edit(&edit_req).unwrap();

        let batch_req = MultiBufferBatchRequest {
            request: RequestId::new(21),
            actor: worker,
            nonce: Nonce::new(21),
            base: Revision::new(4),
            primary: buf_a,
            edits: vec![
                EditSpec {
                    buffer: buf_a,
                    region: ref_region(
                        "  ALPHA\n  BETA\n",
                        2,
                        7,
                        EndpointMode::Exclusive,
                        HostFormat::AGENT_BYTE,
                    ),
                    replacement: Text::from_str("Alpha"),
                },
                EditSpec {
                    buffer: buf_b,
                    region: ref_region(
                        "second\n",
                        0,
                        6,
                        EndpointMode::Exclusive,
                        HostFormat::AGENT_BYTE,
                    ),
                    replacement: Text::from_str("SECOND"),
                },
            ],
            sequence_step: None,
        };
        let batch_out = core.apply_multi_buffer_batch(&batch_req).unwrap();

        // C. execute_commands with edit + selection -> exact retry MUST return original selection & revision!
        let cmd_req = CommandRequest {
            request: RequestId::new(22),
            actor: worker,
            nonce: Nonce::new(22),
            base: Revision::new(5),
            buffer: buf_a,
            commands: HostCommands::Agent(vec![
                AgentCommand::Replace {
                    start: AgentPosition::Byte(BytePos::new(2)),
                    end: AgentPosition::Byte(BytePos::new(7)),
                    text: Text::from_str("GAMMA"),
                },
                AgentCommand::Select {
                    anchor: AgentPosition::Byte(BytePos::new(2)),
                    head: AgentPosition::Byte(BytePos::new(7)),
                },
            ]),
            sequence_step: None,
        };
        let cmd_out = core.execute_commands(&cmd_req).unwrap();
        assert_eq!(cmd_out.revision, Revision::new(6));
        let orig_sel = cmd_out.selection.expect("command produced selection");
        assert_eq!(orig_sel.revision, Revision::new(6));
        assert_eq!((orig_sel.anchor_byte, orig_sel.head_byte), (2, 7));

        let rect_req = RectangleEditRequest {
            request: RequestId::new(23),
            actor: worker,
            nonce: Nonce::new(23),
            base: Revision::new(6),
            buffer: buf_a,
            rectangle: RectangleSpec::Visual(Rectangle {
                first: VisualPosition { line: 0, column: 0 },
                second: VisualPosition { line: 1, column: 2 },
                endpoint: Endpoint::Exclusive,
                tab_width: 4,
            }),
            policy: ShortLinePolicy::Clamp,
            operation: RectangleEditOp::Uniform(">>".into()),
            sequence_step: None,
        };
        let rect_out = core.apply_rectangle_edit(&rect_req).unwrap();

        let indent_req = IndentRegionRequest {
            request: RequestId::new(24),
            actor: worker,
            nonce: Nonce::new(24),
            base: Revision::new(7),
            buffer: buf_a,
            spec: IndentSpec {
                start_line: 0,
                end_line: 2,
                tab_width: 4,
                style: IndentStyle::Spaces,
                action: IndentAction::IndentBy(2),
            },
            sequence_step: None,
        };
        let indent_out = core.apply_indent_region(&indent_req).unwrap();

        // Advance workspace AND mutate worker's selection on buf_a to prove exact command retry
        // returns original selection & revision (rev 6) without clobbering current selection (rev 8)!
        core.set_selection(
            worker,
            buf_a,
            ref_region(
                "  >>GAMMA\n  >>BETA\n",
                0,
                2,
                EndpointMode::Exclusive,
                HostFormat::AGENT_BYTE,
            ),
        )
        .unwrap();
        let live_sel_before_retry = core
            .selection(worker, buf_a, HostFormat::AGENT_BYTE)
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                live_sel_before_retry.anchor_byte,
                live_sel_before_retry.head_byte
            ),
            (0, 2)
        );
        let cmd_retry = core.execute_commands(&cmd_req).unwrap();
        assert_eq!(cmd_retry, cmd_out);
        assert_eq!(cmd_retry.revision, Revision::new(6));
        assert_eq!(cmd_retry.selection, Some(orig_sel));
        assert_eq!(
            core.selection(worker, buf_a, HostFormat::AGENT_BYTE)
                .unwrap()
                .unwrap(),
            live_sel_before_retry
        );

        // Also test committed request with Some(step_a) rejects None and Some(step_b).
        let step_a = SequenceStep {
            sequence: TxnId::new(human, Nonce::new(500)),
            index: 0,
        };
        let step_b = SequenceStep {
            sequence: TxnId::new(human, Nonce::new(500)),
            index: 1,
        };
        let stepped_req = EditRequest {
            request: RequestId::new(25),
            actor: human,
            nonce: Nonce::new(25),
            base: Revision::new(8),
            edit: EditSpec {
                buffer: buf_b,
                region: ref_region(
                    "SECOND\n",
                    0,
                    6,
                    EndpointMode::Exclusive,
                    HostFormat::AGENT_BYTE,
                ),
                replacement: Text::from_str("THIRD!"),
            },
            sequence_step: Some(step_a),
        };
        let stepped_out = core.apply_edit(&stepped_req).unwrap();
        assert_eq!(core.apply_edit(&stepped_req).unwrap(), stepped_out);
        for bad_step in [None, Some(step_b)] {
            let mut altered = stepped_req.clone();
            altered.sequence_step = bad_step;
            assert_rejection_atomic(
                &core,
                &[human, worker],
                &[buf_a, buf_b],
                &[prop_req],
                &[],
                CoreErrorKind::RequestReuse,
                || core.apply_edit(&altered),
            );
        }

        // Revoke worker, then verify mismatched sequence_step is rejected atomically on ALL 5 request types
        // and exact retries remain strictly read-only without mutating group_plan or request_outcomes!
        core.revoke_actor(worker).unwrap();
        let injected_step = SequenceStep {
            sequence: TxnId::new(worker, Nonce::new(888)),
            index: 0,
        };
        let stamp_before = capture_public_state(&core, &[human, worker], &[buf_a, buf_b], &[], &[]);

        assert_eq!(core.apply_edit(&edit_req).unwrap(), edit_out);
        assert_eq!(
            core.apply_multi_buffer_batch(&batch_req).unwrap(),
            batch_out
        );
        assert_eq!(core.execute_commands(&cmd_req).unwrap(), cmd_out);
        assert_eq!(core.apply_rectangle_edit(&rect_req).unwrap(), rect_out);
        assert_eq!(core.apply_indent_region(&indent_req).unwrap(), indent_out);

        let stamp_after_exact =
            capture_public_state(&core, &[human, worker], &[buf_a, buf_b], &[], &[]);
        assert_eq!(stamp_after_exact, stamp_before);

        {
            let mut m = edit_req.clone();
            m.sequence_step = Some(injected_step);
            assert_rejection_atomic(
                &core,
                &[human, worker],
                &[buf_a, buf_b],
                &[],
                &[],
                CoreErrorKind::RequestReuse,
                || core.apply_edit(&m),
            );
        }
        {
            let mut m = batch_req.clone();
            m.sequence_step = Some(injected_step);
            assert_rejection_atomic(
                &core,
                &[human, worker],
                &[buf_a, buf_b],
                &[],
                &[],
                CoreErrorKind::RequestReuse,
                || core.apply_multi_buffer_batch(&m),
            );
        }
        {
            let mut m = cmd_req.clone();
            m.sequence_step = Some(injected_step);
            assert_rejection_atomic(
                &core,
                &[human, worker],
                &[buf_a, buf_b],
                &[],
                &[],
                CoreErrorKind::RequestReuse,
                || core.execute_commands(&m),
            );
        }
        {
            let mut m = rect_req.clone();
            m.sequence_step = Some(injected_step);
            assert_rejection_atomic(
                &core,
                &[human, worker],
                &[buf_a, buf_b],
                &[],
                &[],
                CoreErrorKind::RequestReuse,
                || core.apply_rectangle_edit(&m),
            );
        }
        {
            let mut m = indent_req;
            m.sequence_step = Some(injected_step);
            assert_rejection_atomic(
                &core,
                &[human, worker],
                &[buf_a, buf_b],
                &[],
                &[],
                CoreErrorKind::RequestReuse,
                || core.apply_indent_region(&m),
            );
        }

        // CommandOutcome survives compaction + checkpoint/restore!
        core.compact(Revision::new(9)).unwrap();
        let cp = core.checkpoint().unwrap();
        let restored = EditorCore::restore(&cp).unwrap();
        assert_eq!(restored.execute_commands(&cmd_req).unwrap(), cmd_out);
    }

    // ======================================================================
    // Test 11: Closed Live Subscriptions Stay Terminal on Other-Buffer Edits
    // ======================================================================

    #[test]
    fn regression_closed_live_subscription_stays_terminal_across_other_buffer_edits() {
        let human = ActorId::new(1);

        for format in ALL_FORMATS {
            let core = EditorCore::new();
            core.register_actor(human, ActorRole::Human).unwrap();
            for cap in Capability::ALL {
                core.grant_default(cap).unwrap();
            }

            let buf_a = core
                .open_buffer(&OpenBufferRequest {
                    request: RequestId::new(1),
                    actor: human,
                    nonce: Nonce::new(1),
                    base: Revision::ZERO,
                    name: "a.txt".into(),
                    text: Text::from_str("alpha"),
                })
                .unwrap()
                .buffer;
            let buf_b = core
                .open_buffer(&OpenBufferRequest {
                    request: RequestId::new(2),
                    actor: human,
                    nonce: Nonce::new(2),
                    base: Revision::new(1),
                    name: "b.txt".into(),
                    text: Text::from_str("beta"),
                })
                .unwrap()
                .buffer;

            let (client_deliv, mut local_deliv) = core.subscribe(human, buf_a, format).unwrap();
            let (client_rec, mut local_rec) = core.subscribe(human, buf_a, format).unwrap();

            core.close_buffer(&CloseBufferRequest {
                request: RequestId::new(3),
                actor: human,
                nonce: Nonce::new(3),
                base: Revision::new(2),
                buffer: buf_a,
                policy: ClosePolicy::Discard,
            })
            .unwrap();

            // Observe closure on both live clients (leaves both on ClientTrack::Live with is_closed == true).
            for (cid, local) in [
                (client_deliv, &mut local_deliv),
                (client_rec, &mut local_rec),
            ] {
                let closed = core.reconnect(cid).unwrap();
                assert_eq!(
                    closed,
                    ReconnectOutcome::Closed {
                        client: cid,
                        buffer: buf_a,
                        previous_revision: Revision::new(2),
                        revision: Revision::new(3),
                    }
                );
                assert!(!local.apply_reconnect(&closed).unwrap());
                assert_eq!(
                    core.client_snapshot(cid).unwrap_err().kind(),
                    CoreErrorKind::BufferClosed
                );
            }

            // Snapshot a restored copy so we can verify ClientTrack::Live matches ClientTrack::Restored { closed: true }.
            let cp = core.checkpoint().unwrap();
            let restored = EditorCore::restore(&cp).unwrap();

            // Edit unrelated buf_b twice (rev 4 and rev 5) on both cores.
            for instance in [&core, &restored] {
                instance
                    .apply_edit(&EditRequest {
                        request: RequestId::new(4),
                        actor: human,
                        nonce: Nonce::new(4),
                        base: Revision::new(3),
                        edit: EditSpec {
                            buffer: buf_b,
                            region: ref_region("beta", 0, 4, EndpointMode::Exclusive, format),
                            replacement: Text::from_str("BETA"),
                        },
                        sequence_step: None,
                    })
                    .unwrap();
                instance
                    .apply_edit(&EditRequest {
                        request: RequestId::new(5),
                        actor: human,
                        nonce: Nonce::new(5),
                        base: Revision::new(4),
                        edit: EditSpec {
                            buffer: buf_b,
                            region: ref_region("BETA", 4, 4, EndpointMode::Exclusive, format),
                            replacement: Text::from_str("!"),
                        },
                        sequence_step: None,
                    })
                    .unwrap();
            }

            // deliver_incremental on already-closed live client MUST NOT emit RevisionAdvanced!
            let deliv = core.deliver_incremental(client_deliv).unwrap();
            assert!(deliv.closed);
            assert!(
                deliv.notifications.is_empty(),
                "closed subscription emitted notifications: {:?}",
                deliv.notifications
            );
            assert_eq!(deliv.previous_revision, Revision::new(3));
            assert_eq!(deliv.revision, Revision::new(5));
            assert!(!local_deliv.apply_delivery(&deliv).unwrap());
            assert_eq!(local_deliv.revision(), Revision::new(5));

            // reconnect on already-closed live client MUST return ReconnectOutcome::Closed
            // matching ClientTrack::Restored { closed: true }!
            let rec_live = core.reconnect(client_rec).unwrap();
            let rec_restored = restored.reconnect(client_rec).unwrap();
            assert_eq!(
                rec_live,
                ReconnectOutcome::Closed {
                    client: client_rec,
                    buffer: buf_a,
                    previous_revision: Revision::new(3),
                    revision: Revision::new(5),
                }
            );
            assert_eq!(rec_live, rec_restored);
            assert!(!local_rec.apply_reconnect(&rec_live).unwrap());
            assert_eq!(local_rec.revision(), Revision::new(5));
            assert_eq!(
                core.client_snapshot(client_rec).unwrap_err().kind(),
                CoreErrorKind::BufferClosed
            );
        }
    }

    // ======================================================================
    // Test 12: Remaining Public Entrypoints (is_poisoned, capability
    // revocations, set_emacs_overlay, search_buffers)
    // ======================================================================

    #[test]
    fn public_entrypoints_poison_revocations_emacs_overlay_and_search_buffers() {
        let human = ActorId::new(1);
        let agent = ActorId::new(2);

        let core = EditorCore::new();
        assert!(!core.is_poisoned());
        core.register_actor(human, ActorRole::Human).unwrap();
        core.register_actor(agent, ActorRole::Agent).unwrap();
        for cap in Capability::ALL {
            core.grant_actor(human, cap).unwrap();
        }

        let b0 = core
            .open_buffer(&OpenBufferRequest {
                request: RequestId::new(1),
                actor: human,
                nonce: Nonce::new(1),
                base: Revision::ZERO,
                name: "b0.txt".into(),
                text: Text::from_str("hit 界 hit"),
            })
            .unwrap()
            .buffer;
        let b1 = core
            .open_buffer(&OpenBufferRequest {
                request: RequestId::new(2),
                actor: human,
                nonce: Nonce::new(2),
                base: Revision::new(1),
                name: "b1.txt".into(),
                text: Text::from_str("😀hit\n"),
            })
            .unwrap()
            .buffer;
        let b2 = core
            .open_buffer(&OpenBufferRequest {
                request: RequestId::new(3),
                actor: human,
                nonce: Nonce::new(3),
                base: Revision::new(2),
                name: "secret.txt".into(),
                text: Text::from_str("secret hit"),
            })
            .unwrap()
            .buffer;

        // 1. revoke_actor_capability (with additive buffer grant preservation & session pruning).
        core.grant_actor(agent, Capability::Read).unwrap();
        core.grant_actor(agent, Capability::Commit).unwrap();
        core.grant_buffer(agent, b1, Capability::Read).unwrap();
        core.grant_buffer(agent, b1, Capability::Commit).unwrap();
        assert!(core.has_capability(agent, b0, Capability::Read).unwrap());
        assert!(core.has_capability(agent, b1, Capability::Read).unwrap());

        core.set_selection(
            agent,
            b0,
            ref_region(
                "hit 界 hit",
                0,
                3,
                EndpointMode::Exclusive,
                HostFormat::Emacs,
            ),
        )
        .unwrap();
        core.revoke_actor_capability(agent, Capability::Read)
            .unwrap();
        assert!(!core.has_capability(agent, b0, Capability::Read).unwrap());
        assert_eq!(
            core.read_agent(agent, b0).unwrap_err().kind(),
            CoreErrorKind::PermissionDenied
        );
        // Additive buffer-scoped Read on b1 survives actor-wide Read revocation.
        assert!(core.has_capability(agent, b1, Capability::Read).unwrap());
        assert_eq!(core.read_agent(agent, b1).unwrap().text, "😀hit\n");

        core.revoke_actor_capability(agent, Capability::Commit)
            .unwrap();
        assert!(!core.has_capability(agent, b0, Capability::Commit).unwrap());
        assert!(core.has_capability(agent, b1, Capability::Commit).unwrap());

        // 2. revoke_buffer drops buffer-scoped capability independently.
        core.apply_edit(&EditRequest {
            request: RequestId::new(4),
            actor: agent,
            nonce: Nonce::new(4),
            base: Revision::new(3),
            edit: EditSpec {
                buffer: b1,
                region: ref_region(
                    "😀hit\n",
                    7,
                    7,
                    EndpointMode::Exclusive,
                    HostFormat::AGENT_BYTE,
                ),
                replacement: Text::from_str("!"),
            },
            sequence_step: None,
        })
        .unwrap();
        core.revoke_buffer(agent, b1, Capability::Commit).unwrap();
        assert!(!core.has_capability(agent, b1, Capability::Commit).unwrap());
        assert_eq!(
            core.apply_edit(&EditRequest {
                request: RequestId::new(5),
                actor: agent,
                nonce: Nonce::new(5),
                base: Revision::new(4),
                edit: EditSpec {
                    buffer: b1,
                    region: ref_region(
                        "😀hit!\n",
                        0,
                        4,
                        EndpointMode::Exclusive,
                        HostFormat::AGENT_BYTE,
                    ),
                    replacement: Text::from_str("X"),
                },
                sequence_step: None,
            })
            .unwrap_err()
            .kind(),
            CoreErrorKind::PermissionDenied
        );
        assert!(core.has_capability(agent, b1, Capability::Read).unwrap());
        core.revoke_buffer(agent, b1, Capability::Read).unwrap();
        assert!(!core.has_capability(agent, b1, Capability::Read).unwrap());
        assert_eq!(
            core.read_agent(agent, b1).unwrap_err().kind(),
            CoreErrorKind::PermissionDenied
        );

        // 3. set_emacs_overlay round-trips and projects accurately across all host formats.
        let ov_key = AnnotationKey {
            buffer: b1,
            namespace: NamespaceId::new(3),
            id: AnnotationId::new(9),
        };
        let overlay = EmacsOverlay {
            // "😀hit!\n": char 1='😀'(0..4), char 2='h'(4..5), char 3='i'(5..6), char 4='t'(6..7), char 5='!'(7..8)
            start: EmacsCharPos::new(1).unwrap(),
            end: EmacsCharPos::new(5).unwrap(),
            front_advance: false,
            rear_advance: true,
            metadata: AnnotationMetadata {
                key: ov_key,
                owner: human,
                priority: 12,
                collapse: CollapsePolicy::RetainEmpty,
                label: "emacs-ov".into(),
            },
        };
        let saved_ov = core.set_emacs_overlay(human, &overlay).unwrap();
        assert_eq!(saved_ov, overlay);
        assert_eq!(
            core.annotation(human, ov_key, HostFormat::Emacs).unwrap(),
            Some(HostAnnotation::Emacs(saved_ov))
        );
        let nv_proj = core
            .annotation(human, ov_key, HostFormat::Neovim)
            .unwrap()
            .unwrap();
        let HostAnnotation::Neovim(em) = nv_proj else {
            panic!("expected Neovim extmark projection");
        };
        assert_eq!(em.start, LineColByte::new(0, 0));
        assert_eq!(em.end, LineColByte::new(0, 7));
        assert!(!em.right_gravity);
        assert!(em.end_right_gravity);
        let utf16_proj = core
            .annotation(human, ov_key, HostFormat::AGENT_UTF16)
            .unwrap()
            .unwrap();
        let HostAnnotation::Agent(ag) = utf16_proj else {
            panic!("expected Agent annotation projection");
        };
        assert_eq!(
            ag.start,
            AgentPosition::LineColUtf16(LineColUtf16::new(0, 0))
        );
        assert_eq!(ag.end, AgentPosition::LineColUtf16(LineColUtf16::new(0, 5)));

        // Out-of-range Emacs overlay is rejected atomically.
        assert_rejection_atomic(
            &core,
            &[human, agent],
            &[b0, b1, b2],
            &[],
            &[],
            CoreErrorKind::OutOfRange,
            || {
                let mut bad = overlay.clone();
                bad.end = EmacsCharPos::new(99).unwrap();
                core.set_emacs_overlay(human, &bad)
            },
        );

        // 4. search_buffers across readable, duplicate, unreadable, and unknown buffer IDs.
        core.grant_buffer(agent, b0, Capability::Read).unwrap();
        core.grant_buffer(agent, b1, Capability::Read).unwrap();
        for format in ALL_FORMATS {
            // Duplicate & unsorted input IDs are deduplicated and sorted by BufferId.
            let hits = core
                .search_buffers(agent, &[b1, b0, b1], "hit", format)
                .unwrap();
            let spans: Vec<(BufferId, usize, usize)> = hits
                .iter()
                .map(|h| (h.buffer, h.start_byte, h.end_byte))
                .collect();
            assert_eq!(spans, vec![(b0, 0, 3), (b0, 8, 11), (b1, 4, 7)]);
            for hit in &hits {
                let text = if hit.buffer == b0 {
                    "hit 界 hit"
                } else {
                    "😀hit!\n"
                };
                assert_eq!(hit.start, ref_pos(text, hit.start_byte, format));
                assert_eq!(hit.end, ref_pos(text, hit.end_byte, format));
            }

            // Subset search returns only requested buffer hits; unreadable b2 and unknown IDs are omitted.
            let scoped = core
                .search_buffers(agent, &[b0, b2, BufferId::new(777)], "hit", format)
                .unwrap();
            assert_eq!(scoped.len(), 2);
            assert!(scoped.iter().all(|h| h.buffer == b0));

            // Empty pattern rejected cleanly.
            assert_eq!(
                core.search_buffers(agent, &[b0], "", format)
                    .unwrap_err()
                    .kind(),
                CoreErrorKind::EmptyPattern
            );
        }

        // Revoked actor fails search_buffers with RevokedActor.
        core.revoke_actor(agent).unwrap();
        assert_eq!(
            core.search_buffers(agent, &[b0], "hit", HostFormat::Emacs)
                .unwrap_err()
                .kind(),
            CoreErrorKind::RevokedActor
        );

        // 5. is_poisoned stays false across checkpoint & restore.
        let cp = core.checkpoint().unwrap();
        let restored = EditorCore::restore(&cp).unwrap();
        assert!(!core.is_poisoned());
        assert!(!restored.is_poisoned());
    }
}
