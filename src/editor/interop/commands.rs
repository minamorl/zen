//! Lower native commands to validated requests against successive snapshots.
//!
//! Group coordinates refer to the text resulting from preceding group edits.
//! A group becomes one request against the snapshot at its start. Sequential
//! edits, consecutive groups, no-op edits, and post-edit selections outside an
//! open group are supported.
//! Point, mark, and cursor commands set local lowering state and emit no request;
//! they must be supplied again after an edit.
//! Unsupported commands fail the whole sequence without changing the workspace.

use super::{
    agent_position::{self, AgentPosition},
    coordinates::{BytePos, EmacsCharPos, LineColByte},
    emacs_position,
    error::{BoundaryError, ErrorCode},
    neovim_position,
    region::{EndpointMode, OrientedRegion, RegionCoord},
    requests::{
        Envelope, Input, InputOperation, Operation, Replacement, Request, RequestError, Target,
    },
};
use crate::editor::{
    collab::{
        access::{AccessError, AccessPolicy},
        registry::ActorRegistry,
    },
    edit::{
        ids::{ActorId, BufferId, Nonce},
        transaction::TransactionError,
    },
    text::utf8::Text,
    workspace::{
        catalog::CatalogError,
        selections::{Direction, Selection},
        snapshots::{VersionedWorkspace, WorkspaceError, WorkspaceSnapshot},
    },
};

/// The common target and actor for a sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context {
    pub actor: ActorId,
    pub buffer: BufferId,
}

/// Supported point, region and change-group operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmacsCommand {
    Point(EmacsCharPos),
    Mark(EmacsCharPos),
    SelectRegion,
    Insert(Text),
    ReplaceRegion(Text),
    BeginChangeGroup,
    EndChangeGroup,
    Unsupported(String),
}

/// Supported cursor, text API and undo-block operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NeovimCommand {
    Cursor(LineColByte),
    Select {
        anchor: LineColByte,
        head: LineColByte,
    },
    Insert(Text),
    SetText {
        start: LineColByte,
        end: LineColByte,
        text: Text,
    },
    BeginUndoBlock,
    EndUndoBlock,
    Unsupported(String),
}

/// Direct typed operations, with exclusive endpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentCommand {
    Select {
        anchor: AgentPosition,
        head: AgentPosition,
    },
    Replace {
        start: AgentPosition,
        end: AgentPosition,
        text: Text,
    },
    BeginBatch,
    EndBatch,
    Unsupported(String),
}

fn error(code: ErrorCode, message: impl Into<String>) -> BoundaryError {
    BoundaryError::new(code, message)
}

fn workspace_error(err: WorkspaceError) -> BoundaryError {
    let code = match err {
        WorkspaceError::RevisionOverflow => ErrorCode::RevisionOverflow,
        WorkspaceError::Catalog(CatalogError::UnknownBuffer(_)) => ErrorCode::UnknownBuffer,
        WorkspaceError::Catalog(CatalogError::TombstonedBufferId(_)) => ErrorCode::BufferClosed,
        WorkspaceError::Catalog(CatalogError::DuplicateBufferId(_)) => ErrorCode::DuplicateBufferId,
    };
    error(code, err.to_string())
}

/// Compare requests by their public content, disregarding snapshot identity.
pub fn requests_equivalent(left: &[Request], right: &[Request]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(a, b)| {
            a.actor() == b.actor()
                && a.revision() == b.revision()
                && a.target() == b.target()
                && a.operation() == b.operation()
        })
}

/// Apply lowered edits and selections in order to a matching workspace.
///
/// All requests are authorized and validated before any edits are committed.
/// The returned selection is the last one
/// explicitly requested; later edits do not transform it.
pub fn apply_requests(
    workspace: &mut VersionedWorkspace,
    policy: &AccessPolicy,
    registry: &ActorRegistry,
    requests: &[Request],
) -> Result<Option<Selection>, BoundaryError> {
    let live = workspace.capture();
    let mut sim = VersionedWorkspace::resume_from_snapshot(&live);
    let mut pending = Vec::new();
    let mut selection = None;
    for request in requests {
        request.authorize(policy, registry, &live).map_err(|err| {
            let code = match &err {
                RequestError::DifferentWorkspace => ErrorCode::CorruptSequence,
                RequestError::Access(AccessError::InactiveActor(actor))
                    if registry.is_revoked(*actor) =>
                {
                    ErrorCode::RevokedActor
                }
                RequestError::Access(AccessError::InactiveActor(_)) => ErrorCode::UnknownActor,
                RequestError::Access(AccessError::Denied { .. }) => ErrorCode::PermissionDenied,
                RequestError::Access(AccessError::UnknownBuffer(_)) => ErrorCode::UnknownBuffer,
                RequestError::Access(AccessError::BufferClosed(_)) => ErrorCode::BufferClosed,
                _ => ErrorCode::UnsupportedOperation,
            };
            error(code, format!("request authorization failed: {err:?}"))
        })?;
        if request.revision() != sim.revision() {
            return Err(error(
                ErrorCode::RevisionMismatch,
                "request revision does not match workspace",
            ));
        }
        match request.operation() {
            Operation::Region(value) => {
                let Target::Buffer(id) = request.target() else {
                    return Err(error(
                        ErrorCode::UnsupportedOperation,
                        "selection requires a buffer",
                    ));
                };
                let text = sim.catalog().get(*id).ok_or_else(|| {
                    let code = if sim.catalog().is_tombstoned(*id) {
                        ErrorCode::BufferClosed
                    } else {
                        ErrorCode::UnknownBuffer
                    };
                    error(code, "selection buffer is not live")
                })?;
                for offset in [value.anchor(), value.head()] {
                    let offset = offset.as_usize();
                    if offset > text.len_bytes() {
                        return Err(error(
                            ErrorCode::OutOfRange,
                            "selection endpoint is out of range",
                        ));
                    }
                    if !text.as_str().is_char_boundary(offset) {
                        return Err(error(
                            ErrorCode::InvalidBoundary,
                            "selection endpoint is not a UTF-8 boundary",
                        ));
                    }
                }
                selection = Some(*value);
            }
            Operation::Edit(patches) => {
                let (_, crate::editor::collab::lifecycle::Operation::Edit(transaction)) = request
                    .lifecycle_operation(Nonce::new(0))
                    .ok_or_else(|| error(ErrorCode::CorruptSequence, "invalid edit"))?
                else {
                    return Err(error(ErrorCode::CorruptSequence, "invalid edit"));
                };
                let result = transaction
                    .apply_with_inverse(&sim.capture())
                    .map_err(|err| {
                        let code = match err {
                            TransactionError::RevisionOverflow => ErrorCode::RevisionOverflow,
                            TransactionError::RevisionMismatch { .. } => {
                                ErrorCode::RevisionMismatch
                            }
                            TransactionError::UnknownBuffer(_) => ErrorCode::UnknownBuffer,
                            TransactionError::BufferClosed(_) => ErrorCode::BufferClosed,
                            TransactionError::DuplicateBufferId(_) => ErrorCode::DuplicateBufferId,
                            TransactionError::Patch { .. } => ErrorCode::InvalidSpan,
                        };
                        error(code, err.to_string())
                    })?;
                if !result.is_noop() {
                    let updates: Vec<_> = patches
                        .iter()
                        .map(|(id, _)| {
                            (
                                *id,
                                result.catalog().get(*id).expect("validated buffer").clone(),
                            )
                        })
                        .collect();
                    sim.update_batch(updates.clone()).map_err(workspace_error)?;
                    pending.push(updates);
                }
            }
            _ => {
                return Err(error(
                    ErrorCode::UnsupportedOperation,
                    "request is not an edit or selection",
                ))
            }
        }
    }
    for updates in pending {
        workspace.update_batch(updates).map_err(workspace_error)?;
    }
    Ok(selection)
}

fn region(anchor: RegionCoord, head: RegionCoord) -> OrientedRegion {
    OrientedRegion {
        anchor,
        head,
        direction: Direction::Forward,
        mode: EndpointMode::Exclusive,
    }
}

struct Lowerer {
    workspace: VersionedWorkspace,
    current: WorkspaceSnapshot,
    origin: WorkspaceSnapshot,
    context: Context,
    requests: Vec<Request>,
    group: Option<Group>,
}

struct Group {
    text: Text,
    cells: Vec<Cell>,
    count: usize,
}

struct Cell {
    ch: char,
    original: Option<(usize, usize)>,
}

impl Lowerer {
    fn new(snapshot: &WorkspaceSnapshot, context: Context) -> Self {
        Self {
            workspace: VersionedWorkspace::resume_from_snapshot(snapshot),
            current: snapshot.clone(),
            origin: snapshot.clone(),
            context,
            requests: Vec::new(),
            group: None,
        }
    }

    fn normalize(&self, operation: InputOperation) -> Result<Request, BoundaryError> {
        Envelope::Command(Input {
            actor: Some(self.context.actor),
            revision: Some(self.current.revision()),
            target: Some(Target::Buffer(self.context.buffer)),
            operation,
        })
        .normalize(&self.current)
        .map(|request| request.rebind_origin(self.origin.clone()))
    }

    fn validate_position(&self, position: RegionCoord) -> Result<(), BoundaryError> {
        self.offset(position)?;
        Ok(())
    }

    fn offset(&self, position: RegionCoord) -> Result<usize, BoundaryError> {
        let text = if let Some(group) = &self.group {
            &group.text
        } else {
            self.normalize(InputOperation::Region(region(position, position)))?;
            self.current
                .catalog()
                .get(self.context.buffer)
                .expect("validated buffer")
        };
        let offset = match position {
            RegionCoord::Emacs(pos) => emacs_position::emacs_to_byte(text, pos),
            RegionCoord::Neovim(pos) => neovim_position::line_col_to_byte(text, pos),
            RegionCoord::Agent(pos) => agent_position::position_to_byte(text, pos),
        }?;
        Ok(offset.as_usize())
    }

    fn select(&mut self, anchor: RegionCoord, head: RegionCoord) -> Result<(), BoundaryError> {
        if self.group.is_some() {
            return Err(error(
                ErrorCode::UnsupportedOperation,
                "selection inside edit group",
            ));
        }
        let request = self.normalize(InputOperation::Region(region(anchor, head)))?;
        self.requests.push(request);
        Ok(())
    }

    fn replace(
        &mut self,
        start: RegionCoord,
        end: RegionCoord,
        text: Text,
    ) -> Result<(), BoundaryError> {
        // Text APIs use ordered endpoints; an oriented selection is handled by
        // the Emacs adapter before reaching this function.
        let a = self.offset(start)?;
        let b = self.offset(end)?;
        if a > b {
            return Err(error(
                ErrorCode::InvalidSpan,
                "replacement endpoints are reversed",
            ));
        }
        let replacement = Replacement {
            region: region(start, end),
            text,
        };
        if let Some(group) = &mut self.group {
            let boundaries = group.text.char_boundaries();
            let mut first = boundaries
                .binary_search_by_key(&a, |offset| offset.as_usize())
                .expect("validated boundary");
            let mut last = boundaries
                .binary_search_by_key(&b, |offset| offset.as_usize())
                .expect("validated boundary");
            let mut start = a;
            let mut end = b;
            let mut inserted = replacement.text.as_str();
            while let (Some(old), Some(new)) = (
                group.text.as_str()[start..end].chars().next(),
                inserted.chars().next(),
            ) {
                if old != new {
                    break;
                }
                start += old.len_utf8();
                first += 1;
                inserted = &inserted[new.len_utf8()..];
            }
            while let (Some(old), Some(new)) = (
                group.text.as_str()[start..end].chars().next_back(),
                inserted.chars().next_back(),
            ) {
                if old != new {
                    break;
                }
                end -= old.len_utf8();
                last -= 1;
                inserted = &inserted[..inserted.len() - new.len_utf8()];
            }
            group.count += 1;
            if first == last && inserted.is_empty() {
                return Ok(());
            }
            group.cells.splice(
                first..last,
                inserted.chars().map(|ch| Cell { ch, original: None }),
            );
            let mut result = group.text.as_str().to_owned();
            result.replace_range(start..end, inserted);
            group.text = Text::from_str(&result);
        } else {
            let old = self
                .current
                .catalog()
                .get(self.context.buffer)
                .expect("validated buffer");
            let mut result = old.as_str().to_owned();
            result.replace_range(a..b, replacement.text.as_str());
            let changed = result != old.as_str();
            let request = self.normalize(InputOperation::Edit(replacement))?;
            self.requests.push(request);
            if changed {
                self.workspace
                    .update(self.context.buffer, Text::from_str(&result))
                    .map_err(workspace_error)?;
                self.current = self.workspace.capture();
            }
        }
        Ok(())
    }

    fn begin(&mut self) -> Result<(), BoundaryError> {
        if self.group.is_some() {
            return Err(error(ErrorCode::CorruptSequence, "nested edit group"));
        }
        let pos = RegionCoord::Agent(AgentPosition::Byte(BytePos::new(0)));
        self.validate_position(pos)?;
        let text = self
            .current
            .catalog()
            .get(self.context.buffer)
            .expect("validated buffer")
            .clone();
        let cells = text
            .as_str()
            .char_indices()
            .map(|(start, ch)| Cell {
                ch,
                original: Some((start, start + ch.len_utf8())),
            })
            .collect();
        self.group = Some(Group {
            text,
            cells,
            count: 0,
        });
        Ok(())
    }

    fn end(&mut self) -> Result<(), BoundaryError> {
        let group = self
            .group
            .take()
            .ok_or_else(|| error(ErrorCode::CorruptSequence, "end without edit group"))?;
        if group.count == 0 {
            return Err(error(ErrorCode::CorruptSequence, "empty edit group"));
        }
        let result_text = group.text.clone();
        let original = self
            .current
            .catalog()
            .get(self.context.buffer)
            .expect("validated buffer");
        let mut edits = Vec::new();
        let mut cursor = 0;
        let mut inserted = String::new();
        let make_edit = |start, end, text: &str| Replacement {
            region: region(
                RegionCoord::Agent(AgentPosition::Byte(BytePos::new(start))),
                RegionCoord::Agent(AgentPosition::Byte(BytePos::new(end))),
            ),
            text: Text::from_str(text),
        };
        let mut push_hunk = |mut start: usize, mut end: usize, text: &str| {
            let mut ins_start = 0;
            let mut ins_end = text.len();
            while let (Some(a), Some(b)) = (
                original.as_str()[start..end].chars().next(),
                text[ins_start..ins_end].chars().next(),
            ) {
                if a != b {
                    break;
                }
                start += a.len_utf8();
                ins_start += b.len_utf8();
            }
            while let (Some(a), Some(b)) = (
                original.as_str()[start..end].chars().next_back(),
                text[ins_start..ins_end].chars().next_back(),
            ) {
                if a != b {
                    break;
                }
                end -= a.len_utf8();
                ins_end -= b.len_utf8();
            }
            if start != end || ins_start != ins_end {
                edits.push(make_edit(start, end, &text[ins_start..ins_end]));
            }
        };
        for cell in group.cells {
            if let Some((start, end)) = cell.original {
                if start != cursor || !inserted.is_empty() {
                    push_hunk(cursor, start, &inserted);
                    inserted.clear();
                }
                cursor = end;
            } else {
                inserted.push(cell.ch);
            }
        }
        if cursor != original.len_bytes() || !inserted.is_empty() {
            push_hunk(cursor, original.len_bytes(), &inserted);
        }
        let changed = result_text != *original;
        if !changed {
            edits.clear();
        }
        let request = self.normalize(InputOperation::Batch(edits))?;
        self.requests.push(request);
        if changed {
            self.workspace
                .update(self.context.buffer, result_text)
                .map_err(workspace_error)?;
            self.current = self.workspace.capture();
        }
        Ok(())
    }

    fn finish(self) -> Result<Vec<Request>, BoundaryError> {
        if self.group.is_some() {
            return Err(error(ErrorCode::CorruptSequence, "unclosed edit group"));
        }
        Ok(self.requests)
    }
}

fn unsupported(name: &str) -> BoundaryError {
    error(
        ErrorCode::UnsupportedOperation,
        format!("unsupported command: {name}"),
    )
}

/// Lower an Emacs sequence. Point and mark must be supplied before use.
pub fn lower_emacs(
    snapshot: &WorkspaceSnapshot,
    context: Context,
    commands: &[EmacsCommand],
) -> Result<Vec<Request>, BoundaryError> {
    let mut lowerer = Lowerer::new(snapshot, context);
    let (mut point, mut mark) = (None, None);
    for command in commands {
        match command {
            EmacsCommand::Point(pos) => {
                let pos = RegionCoord::Emacs(*pos);
                lowerer.validate_position(pos)?;
                point = Some(pos);
            }
            EmacsCommand::Mark(pos) => {
                let pos = RegionCoord::Emacs(*pos);
                lowerer.validate_position(pos)?;
                mark = Some(pos);
            }
            EmacsCommand::SelectRegion => lowerer.select(
                mark.ok_or_else(|| error(ErrorCode::InvalidCoordinate, "missing mark"))?,
                point.ok_or_else(|| error(ErrorCode::InvalidCoordinate, "missing point"))?,
            )?,
            EmacsCommand::Insert(text) => {
                let pos =
                    point.ok_or_else(|| error(ErrorCode::InvalidCoordinate, "missing point"))?;
                lowerer.replace(pos, pos, text.clone())?;
                point = None;
                mark = None;
            }
            EmacsCommand::ReplaceRegion(text) => {
                let p =
                    point.ok_or_else(|| error(ErrorCode::InvalidCoordinate, "missing point"))?;
                let m = mark.ok_or_else(|| error(ErrorCode::InvalidCoordinate, "missing mark"))?;
                let (start, end) = if lowerer.offset(m)? <= lowerer.offset(p)? {
                    (m, p)
                } else {
                    (p, m)
                };
                lowerer.replace(start, end, text.clone())?;
                point = None;
                mark = None;
            }
            EmacsCommand::BeginChangeGroup => lowerer.begin()?,
            EmacsCommand::EndChangeGroup => lowerer.end()?,
            EmacsCommand::Unsupported(name) => return Err(unsupported(name)),
        }
    }
    lowerer.finish()
}

/// Lower a Neovim sequence. Cursor must be supplied before insertion.
pub fn lower_neovim(
    snapshot: &WorkspaceSnapshot,
    context: Context,
    commands: &[NeovimCommand],
) -> Result<Vec<Request>, BoundaryError> {
    let mut lowerer = Lowerer::new(snapshot, context);
    let mut cursor = None;
    for command in commands {
        match command {
            NeovimCommand::Cursor(pos) => {
                let pos = RegionCoord::Neovim(*pos);
                lowerer.validate_position(pos)?;
                cursor = Some(pos);
            }
            NeovimCommand::Select { anchor, head } => {
                lowerer.select(RegionCoord::Neovim(*anchor), RegionCoord::Neovim(*head))?
            }
            NeovimCommand::Insert(text) => {
                let pos =
                    cursor.ok_or_else(|| error(ErrorCode::InvalidCoordinate, "missing cursor"))?;
                lowerer.replace(pos, pos, text.clone())?;
                cursor = None;
            }
            NeovimCommand::SetText { start, end, text } => {
                lowerer.replace(
                    RegionCoord::Neovim(*start),
                    RegionCoord::Neovim(*end),
                    text.clone(),
                )?;
                cursor = None;
            }
            NeovimCommand::BeginUndoBlock => lowerer.begin()?,
            NeovimCommand::EndUndoBlock => lowerer.end()?,
            NeovimCommand::Unsupported(name) => return Err(unsupported(name)),
        }
    }
    lowerer.finish()
}

/// Lower direct agent operations.
pub fn lower_agent(
    snapshot: &WorkspaceSnapshot,
    context: Context,
    commands: &[AgentCommand],
) -> Result<Vec<Request>, BoundaryError> {
    let mut lowerer = Lowerer::new(snapshot, context);
    for command in commands {
        match command {
            AgentCommand::Select { anchor, head } => {
                lowerer.select(RegionCoord::Agent(*anchor), RegionCoord::Agent(*head))?
            }
            AgentCommand::Replace { start, end, text } => lowerer.replace(
                RegionCoord::Agent(*start),
                RegionCoord::Agent(*end),
                text.clone(),
            )?,
            AgentCommand::BeginBatch => lowerer.begin()?,
            AgentCommand::EndBatch => lowerer.end()?,
            AgentCommand::Unsupported(name) => return Err(unsupported(name)),
        }
    }
    lowerer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::{access::Capability, lifecycle, role::ActorRole},
        edit::ids::Nonce,
        interop::{coordinates::BytePos, emacs_position, neovim_position},
        test_support::{assert_law, texts, Rng},
        workspace::{catalog::BufferCatalog, snapshots::VersionedWorkspace},
    };

    fn access(context: Context) -> (AccessPolicy, ActorRegistry) {
        let mut registry = ActorRegistry::new();
        registry.register(context.actor, ActorRole::Agent).unwrap();
        let mut policy = AccessPolicy::new();
        policy.grant_default(Capability::Read);
        policy.grant_default(Capability::Commit);
        (policy, registry)
    }

    fn fixture(source: &str) -> (WorkspaceSnapshot, Context) {
        let (workspace, context) = live_fixture(source);
        (workspace.capture(), context)
    }

    fn live_fixture(source: &str) -> (VersionedWorkspace, Context) {
        let context = Context {
            actor: ActorId::new(7),
            buffer: BufferId::new(1),
        };
        let mut catalog = BufferCatalog::new();
        catalog
            .register(context.buffer, Text::from_str(source))
            .unwrap();
        (VersionedWorkspace::new(catalog), context)
    }

    // Each insertion is at the start, so all adapters exercise coordinates
    // against the current text, including after groups and no-ops.
    fn sequence(source: &str, steps: &[(bool, &str)], select: bool) {
        let (snapshot, context) = fixture(source);
        let mut em = Vec::new();
        let mut nv = Vec::new();
        let mut ag = Vec::new();
        let mut expected = source.to_owned();
        let mut revisions = 0;
        for &(group, value) in steps {
            if group {
                em.push(EmacsCommand::BeginChangeGroup);
                nv.push(NeovimCommand::BeginUndoBlock);
                ag.push(AgentCommand::BeginBatch);
            }
            em.extend([
                EmacsCommand::Point(EmacsCharPos::new(1).unwrap()),
                EmacsCommand::Insert(Text::from_str(value)),
            ]);
            nv.extend([
                NeovimCommand::Cursor(LineColByte::new(0, 0)),
                NeovimCommand::Insert(Text::from_str(value)),
            ]);
            let zero = AgentPosition::Byte(BytePos::new(0));
            ag.push(AgentCommand::Replace {
                start: zero,
                end: zero,
                text: Text::from_str(value),
            });
            if group {
                em.push(EmacsCommand::EndChangeGroup);
                nv.push(NeovimCommand::EndUndoBlock);
                ag.push(AgentCommand::EndBatch);
            }
            expected.insert_str(0, value);
            revisions += u64::from(!value.is_empty());
        }
        let mut expected_selection = None;
        if select {
            let current = Text::from_str(&expected);
            let start = current.validate_offset(0).unwrap();
            let end = current.char_boundaries().get(1).copied().unwrap_or(start);
            em.extend([
                EmacsCommand::Mark(emacs_position::byte_to_emacs(&current, start).unwrap()),
                EmacsCommand::Point(emacs_position::byte_to_emacs(&current, end).unwrap()),
                EmacsCommand::SelectRegion,
            ]);
            nv.push(NeovimCommand::Select {
                anchor: neovim_position::byte_to_line_col(&current, start).unwrap(),
                head: neovim_position::byte_to_line_col(&current, end).unwrap(),
            });
            ag.push(AgentCommand::Select {
                anchor: AgentPosition::Byte(BytePos::new(start.as_usize())),
                head: AgentPosition::Byte(BytePos::new(end.as_usize())),
            });
            expected_selection = Some((start, end));
        }
        let requests = [
            lower_emacs(&snapshot, context, &em).unwrap(),
            lower_neovim(&snapshot, context, &nv).unwrap(),
            lower_agent(&snapshot, context, &ag).unwrap(),
        ];
        assert_eq!(requests[0], requests[1]);
        assert_eq!(requests[0], requests[2]);
        let (policy, registry) = access(context);
        let mut results = Vec::new();
        for commands in &requests {
            let mut workspace = VersionedWorkspace::resume_from_snapshot(&snapshot);
            let commands: Vec<_> = commands
                .iter()
                .cloned()
                .map(|r| r.rebind_origin(workspace.capture()))
                .collect();
            let selection = apply_requests(&mut workspace, &policy, &registry, &commands).unwrap();
            assert_eq!(
                workspace.catalog().get(context.buffer).unwrap().as_str(),
                expected
            );
            assert_eq!(workspace.revision().raw(), revisions);
            assert_eq!(
                selection.map(|s| (s.anchor(), s.head())),
                expected_selection
            );
            results.push((workspace.catalog().clone(), workspace.revision(), selection));
        }
        assert_eq!(results[0], results[1]);
        assert_eq!(results[0], results[2]);
    }

    #[test]
    fn successive_edits_and_selection() {
        // Insert in reverse order to obtain the requested suffix.
        let (snapshot, context) = fixture("abc");
        let em = [
            EmacsCommand::Point(EmacsCharPos::new(4).unwrap()),
            EmacsCommand::Insert(Text::from_str("X")),
            EmacsCommand::Point(EmacsCharPos::new(5).unwrap()),
            EmacsCommand::Insert(Text::from_str("Y")),
        ];
        let nv = [
            NeovimCommand::Cursor(LineColByte::new(0, 3)),
            NeovimCommand::Insert(Text::from_str("X")),
            NeovimCommand::Cursor(LineColByte::new(0, 4)),
            NeovimCommand::Insert(Text::from_str("Y")),
        ];
        let p = |n| AgentPosition::Byte(BytePos::new(n));
        let ag = [
            AgentCommand::Replace {
                start: p(3),
                end: p(3),
                text: Text::from_str("X"),
            },
            AgentCommand::Replace {
                start: p(4),
                end: p(4),
                text: Text::from_str("Y"),
            },
        ];
        let requests = [
            lower_emacs(&snapshot, context, &em).unwrap(),
            lower_neovim(&snapshot, context, &nv).unwrap(),
            lower_agent(&snapshot, context, &ag).unwrap(),
        ];
        let (policy, registry) = access(context);
        assert!(requests_equivalent(&requests[0], &requests[1]));
        assert!(requests_equivalent(&requests[0], &requests[2]));
        for r in &requests {
            let mut workspace = VersionedWorkspace::resume_from_snapshot(&snapshot);
            let r: Vec<_> = r
                .iter()
                .cloned()
                .map(|r| r.rebind_origin(workspace.capture()))
                .collect();
            apply_requests(&mut workspace, &policy, &registry, &r).unwrap();
            assert_eq!(
                workspace.catalog().get(context.buffer).unwrap().as_str(),
                "abcXY"
            );
        }
        let em = [
            EmacsCommand::Point(EmacsCharPos::new(4).unwrap()),
            EmacsCommand::Insert(Text::from_str("X")),
            EmacsCommand::Mark(EmacsCharPos::new(4).unwrap()),
            EmacsCommand::Point(EmacsCharPos::new(5).unwrap()),
            EmacsCommand::SelectRegion,
        ];
        let nv = [
            NeovimCommand::Cursor(LineColByte::new(0, 3)),
            NeovimCommand::Insert(Text::from_str("X")),
            NeovimCommand::Select {
                anchor: LineColByte::new(0, 3),
                head: LineColByte::new(0, 4),
            },
        ];
        let ag = [
            AgentCommand::Replace {
                start: p(3),
                end: p(3),
                text: Text::from_str("X"),
            },
            AgentCommand::Select {
                anchor: p(3),
                head: p(4),
            },
        ];
        let requests = [
            lower_emacs(&snapshot, context, &em).unwrap(),
            lower_neovim(&snapshot, context, &nv).unwrap(),
            lower_agent(&snapshot, context, &ag).unwrap(),
        ];
        assert!(requests_equivalent(&requests[0], &requests[1]));
        assert!(requests_equivalent(&requests[0], &requests[2]));
        for r in &requests {
            let mut workspace = VersionedWorkspace::resume_from_snapshot(&snapshot);
            let r: Vec<_> = r
                .iter()
                .cloned()
                .map(|r| r.rebind_origin(workspace.capture()))
                .collect();
            let selection = apply_requests(&mut workspace, &policy, &registry, &r)
                .unwrap()
                .unwrap();
            assert_eq!(
                workspace.catalog().get(context.buffer).unwrap().as_str(),
                "abcX"
            );
            assert_eq!(
                (selection.anchor().as_usize(), selection.head().as_usize()),
                (3, 4)
            );
        }
    }

    #[test]
    fn consecutive_groups_and_noop_then_edit() {
        sequence("abc", &[(true, "X"), (true, "Y")], false);
        sequence("abc", &[(false, ""), (false, "X")], false);
        sequence("abc", &[(true, ""), (true, "X")], false);
    }

    #[test]
    fn selection_in_open_group_is_rejected() {
        let (snapshot, context) = fixture("abc");
        let e = EmacsCharPos::new(1).unwrap();
        let n = LineColByte::new(0, 0);
        let a = AgentPosition::Byte(BytePos::new(0));
        assert_eq!(
            lower_emacs(
                &snapshot,
                context,
                &[
                    EmacsCommand::BeginChangeGroup,
                    EmacsCommand::Mark(e),
                    EmacsCommand::Point(e),
                    EmacsCommand::SelectRegion
                ]
            )
            .unwrap_err()
            .code(),
            ErrorCode::UnsupportedOperation
        );
        assert_eq!(
            lower_neovim(
                &snapshot,
                context,
                &[
                    NeovimCommand::BeginUndoBlock,
                    NeovimCommand::Select { anchor: n, head: n }
                ]
            )
            .unwrap_err()
            .code(),
            ErrorCode::UnsupportedOperation
        );
        assert_eq!(
            lower_agent(
                &snapshot,
                context,
                &[
                    AgentCommand::BeginBatch,
                    AgentCommand::Select { anchor: a, head: a }
                ]
            )
            .unwrap_err()
            .code(),
            ErrorCode::UnsupportedOperation
        );
    }

    #[test]
    fn generated_equivalent_commands_and_results() {
        assert_law(
            &[0, 1, 42, 99, u64::MAX],
            |rng: &mut Rng| (texts(rng), rng.next_u64()),
            |(sources, seed)| {
                for source in sources {
                    let value = ["X", "界", "😀", "\n", "\r\n"][*seed as usize % 5];
                    sequence(
                        source,
                        &[
                            (false, value),
                            (true, "界😀\r\n"),
                            (true, ""),
                            (false, ""),
                            (false, "Z"),
                        ],
                        true,
                    );
                    let (snapshot, context) = fixture(source);
                    let original = snapshot.clone();
                    let text = snapshot.catalog().get(context.buffer).unwrap();
                    let boundaries = text.char_boundaries();
                    let start = (*seed as usize) % boundaries.len();
                    let end = start + ((*seed >> 8) as usize % (boundaries.len() - start));
                    let a = boundaries[start];
                    let b = boundaries[end];
                    let em_a = emacs_position::byte_to_emacs(text, a).unwrap();
                    let em_b = emacs_position::byte_to_emacs(text, b).unwrap();
                    let nv_a = neovim_position::byte_to_line_col(text, a).unwrap();
                    let nv_b = neovim_position::byte_to_line_col(text, b).unwrap();
                    let ag_a = AgentPosition::Byte(BytePos::new(a.as_usize()));
                    let ag_b = AgentPosition::Byte(BytePos::new(b.as_usize()));
                    let value = Text::from_str("界😀\r\n");
                    let emacs = lower_emacs(
                        &snapshot,
                        context,
                        &[
                            EmacsCommand::Mark(em_a),
                            EmacsCommand::Point(em_b),
                            EmacsCommand::SelectRegion,
                            EmacsCommand::BeginChangeGroup,
                            EmacsCommand::ReplaceRegion(value.clone()),
                            EmacsCommand::EndChangeGroup,
                        ],
                    )
                    .unwrap();
                    let neovim = lower_neovim(
                        &snapshot,
                        context,
                        &[
                            NeovimCommand::Select {
                                anchor: nv_a,
                                head: nv_b,
                            },
                            NeovimCommand::BeginUndoBlock,
                            NeovimCommand::SetText {
                                start: nv_a,
                                end: nv_b,
                                text: value.clone(),
                            },
                            NeovimCommand::EndUndoBlock,
                        ],
                    )
                    .unwrap();
                    let agent = lower_agent(
                        &snapshot,
                        context,
                        &[
                            AgentCommand::Select {
                                anchor: ag_a,
                                head: ag_b,
                            },
                            AgentCommand::BeginBatch,
                            AgentCommand::Replace {
                                start: ag_a,
                                end: ag_b,
                                text: value,
                            },
                            AgentCommand::EndBatch,
                        ],
                    )
                    .unwrap();
                    assert_eq!(emacs, neovim);
                    assert_eq!(emacs, agent);
                    let (policy, registry) = access(context);
                    for adapter in 0..3 {
                        let mut workspace = VersionedWorkspace::resume_from_snapshot(&snapshot);
                        let live = workspace.capture();
                        let requests = match adapter {
                            0 => lower_emacs(
                                &live,
                                context,
                                &[
                                    EmacsCommand::Mark(em_a),
                                    EmacsCommand::Point(em_b),
                                    EmacsCommand::SelectRegion,
                                    EmacsCommand::BeginChangeGroup,
                                    EmacsCommand::ReplaceRegion(Text::from_str("界😀\r\n")),
                                    EmacsCommand::EndChangeGroup,
                                ],
                            ),
                            1 => lower_neovim(
                                &live,
                                context,
                                &[
                                    NeovimCommand::Select {
                                        anchor: nv_a,
                                        head: nv_b,
                                    },
                                    NeovimCommand::BeginUndoBlock,
                                    NeovimCommand::SetText {
                                        start: nv_a,
                                        end: nv_b,
                                        text: Text::from_str("界😀\r\n"),
                                    },
                                    NeovimCommand::EndUndoBlock,
                                ],
                            ),
                            _ => lower_agent(
                                &live,
                                context,
                                &[
                                    AgentCommand::Select {
                                        anchor: ag_a,
                                        head: ag_b,
                                    },
                                    AgentCommand::BeginBatch,
                                    AgentCommand::Replace {
                                        start: ag_a,
                                        end: ag_b,
                                        text: Text::from_str("界😀\r\n"),
                                    },
                                    AgentCommand::EndBatch,
                                ],
                            ),
                        }
                        .unwrap();
                        let selection =
                            apply_requests(&mut workspace, &policy, &registry, &requests)
                                .unwrap()
                                .unwrap();
                        assert_eq!((selection.anchor(), selection.head()), (a, b));
                        let expected = format!(
                            "{}界😀\r\n{}",
                            &source[..a.as_usize()],
                            &source[b.as_usize()..]
                        );
                        assert_eq!(
                            workspace.catalog().get(context.buffer).unwrap().as_str(),
                            expected
                        );
                        assert_eq!(workspace.revision().raw(), u64::from(expected != *source));
                    }
                    assert_eq!(snapshot, original);
                }
            },
        );
    }

    #[test]
    fn generated_rejections_are_atomic() {
        assert_law(
            &[0, 1, 42, 99, u64::MAX],
            |rng: &mut Rng| texts(rng),
            |sources| {
                for source in sources {
                    let (snapshot, context) = fixture(source);
                    let original = snapshot.clone();
                    let bad = AgentPosition::Byte(BytePos::new(source.len() + 2));
                    let good = AgentPosition::Byte(BytePos::new(0));
                    let edit = AgentCommand::Replace {
                        start: good,
                        end: good,
                        text: Text::from_str("x"),
                    };
                    let cases = [
                        vec![edit.clone(), AgentCommand::Unsupported("unknown".into())],
                        vec![
                            AgentCommand::BeginBatch,
                            edit.clone(),
                            AgentCommand::Unsupported("unknown".into()),
                        ],
                        vec![
                            edit.clone(),
                            AgentCommand::Replace {
                                start: bad,
                                end: bad,
                                text: Text::from_str("x"),
                            },
                        ],
                        vec![AgentCommand::BeginBatch, edit.clone()],
                        vec![AgentCommand::EndBatch],
                        vec![AgentCommand::BeginBatch, AgentCommand::BeginBatch],
                        vec![AgentCommand::BeginBatch, AgentCommand::EndBatch],
                    ];
                    for commands in cases {
                        assert!(lower_agent(&snapshot, context, &commands).is_err());
                    }
                    assert_eq!(
                        lower_emacs(
                            &snapshot,
                            context,
                            &[EmacsCommand::Insert(Text::from_str("x"))]
                        )
                        .unwrap_err()
                        .code(),
                        ErrorCode::InvalidCoordinate
                    );
                    assert_eq!(
                        lower_emacs(&snapshot, context, &[EmacsCommand::SelectRegion])
                            .unwrap_err()
                            .code(),
                        ErrorCode::InvalidCoordinate
                    );
                    assert_eq!(
                        lower_neovim(
                            &snapshot,
                            context,
                            &[NeovimCommand::Insert(Text::from_str("x"))]
                        )
                        .unwrap_err()
                        .code(),
                        ErrorCode::InvalidCoordinate
                    );
                    assert_eq!(
                        lower_neovim(
                            &snapshot,
                            context,
                            &[NeovimCommand::Cursor(LineColByte::new(usize::MAX, 0))]
                        )
                        .unwrap_err()
                        .code(),
                        ErrorCode::OutOfRange
                    );
                    assert_eq!(
                        lower_emacs(
                            &snapshot,
                            context,
                            &[EmacsCommand::Point(
                                EmacsCharPos::new(source.chars().count() + 2).unwrap()
                            )]
                        )
                        .unwrap_err()
                        .code(),
                        ErrorCode::OutOfRange
                    );
                    assert_eq!(snapshot, original);
                }
            },
        );
    }

    #[test]
    fn generated_sequential_groups_and_rejections() {
        assert_law(
            &[0, 1, 42, 99, u64::MAX],
            |rng: &mut Rng| texts(rng),
            |sources| {
                for source in sources {
                    let (snapshot, context) = fixture(source);
                    let before = snapshot.clone();
                    let changed = Text::from_str(&format!("X{source}"));
                    let end = changed
                        .char_boundaries()
                        .get(2)
                        .copied()
                        .unwrap_or_else(|| changed.validate_offset(1).unwrap());
                    let start = changed.validate_offset(1).unwrap();
                    let ea = emacs_position::byte_to_emacs(&changed, start).unwrap();
                    let eb = emacs_position::byte_to_emacs(&changed, end).unwrap();
                    let na = neovim_position::byte_to_line_col(&changed, start).unwrap();
                    let nb = neovim_position::byte_to_line_col(&changed, end).unwrap();
                    let aa = AgentPosition::Byte(BytePos::new(start.as_usize()));
                    let ab = AgentPosition::Byte(BytePos::new(end.as_usize()));
                    let emacs = vec![
                        EmacsCommand::BeginChangeGroup,
                        EmacsCommand::Point(EmacsCharPos::new(1).unwrap()),
                        EmacsCommand::Insert(Text::from_str("X")),
                        EmacsCommand::Mark(ea),
                        EmacsCommand::Point(eb),
                        EmacsCommand::ReplaceRegion(Text::from_str("Y")),
                        EmacsCommand::EndChangeGroup,
                    ];
                    let neovim = vec![
                        NeovimCommand::BeginUndoBlock,
                        NeovimCommand::Cursor(LineColByte::new(0, 0)),
                        NeovimCommand::Insert(Text::from_str("X")),
                        NeovimCommand::SetText {
                            start: na,
                            end: nb,
                            text: Text::from_str("Y"),
                        },
                        NeovimCommand::EndUndoBlock,
                    ];
                    let zero = AgentPosition::Byte(BytePos::new(0));
                    let agent = vec![
                        AgentCommand::BeginBatch,
                        AgentCommand::Replace {
                            start: zero,
                            end: zero,
                            text: Text::from_str("X"),
                        },
                        AgentCommand::Replace {
                            start: aa,
                            end: ab,
                            text: Text::from_str("Y"),
                        },
                        AgentCommand::EndBatch,
                    ];
                    let a = lower_emacs(&snapshot, context, &emacs).unwrap();
                    let b = lower_neovim(&snapshot, context, &neovim).unwrap();
                    let c = lower_agent(&snapshot, context, &agent).unwrap();
                    assert_eq!(a, b);
                    assert_eq!(a, c);
                    let (_, lifecycle::Operation::Edit(tx)) =
                        a[0].lifecycle_operation(Nonce::new(1)).unwrap()
                    else {
                        panic!("expected edit")
                    };
                    let result = tx.apply_with_inverse(&snapshot).unwrap();
                    assert_eq!(
                        result.catalog().get(context.buffer).unwrap().as_str(),
                        format!("XY{}", &changed.as_str()[end.as_usize()..])
                    );
                    let mut e = emacs[..3].to_vec();
                    e.push(EmacsCommand::Unsupported("unknown".into()));
                    let mut n = neovim[..3].to_vec();
                    n.push(NeovimCommand::Unsupported("unknown".into()));
                    let mut g = agent[..2].to_vec();
                    g.push(AgentCommand::Unsupported("unknown".into()));
                    assert_eq!(
                        lower_emacs(&snapshot, context, &e).unwrap_err().code(),
                        ErrorCode::UnsupportedOperation
                    );
                    assert_eq!(
                        lower_neovim(&snapshot, context, &n).unwrap_err().code(),
                        ErrorCode::UnsupportedOperation
                    );
                    assert_eq!(
                        lower_agent(&snapshot, context, &g).unwrap_err().code(),
                        ErrorCode::UnsupportedOperation
                    );
                    e.pop();
                    e.push(EmacsCommand::Point(
                        EmacsCharPos::new(changed.len_scalars() + 2).unwrap(),
                    ));
                    n.pop();
                    n.push(NeovimCommand::Cursor(LineColByte::new(usize::MAX, 0)));
                    g.pop();
                    g.push(AgentCommand::Replace {
                        start: AgentPosition::Byte(BytePos::new(changed.len_bytes() + 1)),
                        end: zero,
                        text: Text::from_str(""),
                    });
                    assert_eq!(
                        lower_emacs(&snapshot, context, &e).unwrap_err().code(),
                        ErrorCode::OutOfRange
                    );
                    assert_eq!(
                        lower_neovim(&snapshot, context, &n).unwrap_err().code(),
                        ErrorCode::OutOfRange
                    );
                    assert_eq!(
                        lower_agent(&snapshot, context, &g).unwrap_err().code(),
                        ErrorCode::OutOfRange
                    );
                    assert_eq!(snapshot, before);
                }
            },
        );
    }

    #[test]
    fn generated_selection_endpoints_and_direction() {
        assert_law(
            &[0, 1, 42, 99, u64::MAX],
            |rng: &mut Rng| texts(rng),
            |sources| {
                for source in sources {
                    let (snapshot, context) = fixture(source);
                    let text = snapshot.catalog().get(context.buffer).unwrap();
                    let boundaries = text.char_boundaries();
                    for (anchor, head) in [
                        (boundaries[0], *boundaries.last().unwrap()),
                        (*boundaries.last().unwrap(), boundaries[0]),
                        (boundaries[0], boundaries[0]),
                    ] {
                        let em = lower_emacs(
                            &snapshot,
                            context,
                            &[
                                EmacsCommand::Mark(
                                    emacs_position::byte_to_emacs(text, anchor).unwrap(),
                                ),
                                EmacsCommand::Point(
                                    emacs_position::byte_to_emacs(text, head).unwrap(),
                                ),
                                EmacsCommand::SelectRegion,
                            ],
                        )
                        .unwrap();
                        let nv = lower_neovim(
                            &snapshot,
                            context,
                            &[NeovimCommand::Select {
                                anchor: neovim_position::byte_to_line_col(text, anchor).unwrap(),
                                head: neovim_position::byte_to_line_col(text, head).unwrap(),
                            }],
                        )
                        .unwrap();
                        let ag = lower_agent(
                            &snapshot,
                            context,
                            &[AgentCommand::Select {
                                anchor: AgentPosition::Byte(BytePos::new(anchor.as_usize())),
                                head: AgentPosition::Byte(BytePos::new(head.as_usize())),
                            }],
                        )
                        .unwrap();
                        assert_eq!(em, nv);
                        assert_eq!(em, ag);
                        let super::super::requests::Operation::Region(selection) =
                            em[0].operation()
                        else {
                            panic!("expected selection")
                        };
                        assert_eq!(selection.anchor(), anchor);
                        assert_eq!(selection.head(), head);
                        let direction = match anchor.cmp(&head) {
                            std::cmp::Ordering::Less => Direction::Forward,
                            std::cmp::Ordering::Equal => Direction::Empty,
                            std::cmp::Ordering::Greater => Direction::Backward,
                        };
                        assert_eq!(selection.direction(), direction);
                    }
                }
            },
        );
    }

    #[test]
    fn group_preserves_untouched_gaps() {
        use crate::editor::collab::footprints::Footprint;
        let (snapshot, context) = fixture("abcdef");
        let p = |n| AgentPosition::Byte(BytePos::new(n));
        let commands = [
            AgentCommand::BeginBatch,
            AgentCommand::Replace {
                start: p(0),
                end: p(1),
                text: Text::from_str("X"),
            },
            AgentCommand::Replace {
                start: p(4),
                end: p(5),
                text: Text::from_str("Y"),
            },
            AgentCommand::EndBatch,
        ];
        let group = lower_agent(&snapshot, context, &commands).unwrap();
        let independent = lower_agent(
            &snapshot,
            context,
            &[AgentCommand::Replace {
                start: p(2),
                end: p(3),
                text: Text::from_str("Z"),
            }],
        )
        .unwrap();
        let super::super::requests::Operation::Edit(group_patches) = group[0].operation() else {
            panic!("expected edit")
        };
        let super::super::requests::Operation::Edit(other_patches) = independent[0].operation()
        else {
            panic!("expected edit")
        };
        let spans: Vec<_> = group_patches[0]
            .1
            .splices()
            .iter()
            .map(|s| (s.span().start().as_usize(), s.span().end().as_usize()))
            .collect();
        assert_eq!(spans, [(0, 1), (4, 5)]);
        let left = Footprint::from_patches(group_patches.iter().map(|(id, patch)| (*id, patch)));
        let right = Footprint::from_patches(other_patches.iter().map(|(id, patch)| (*id, patch)));
        assert!(!left.conflicts(&right));
    }

    #[test]
    fn origin_authorization_and_atomic_validation() {
        let (mut workspace, context) = live_fixture("ab");
        let snapshot = workspace.capture();
        let (mut other, _) = live_fixture("zz");
        let foreign = other.capture();
        let (policy, registry) = access(context);
        let p = |n| AgentPosition::Byte(BytePos::new(n));
        let edit = |start, end, value| AgentCommand::Replace {
            start: p(start),
            end: p(end),
            text: Text::from_str(value),
        };
        let requests = lower_agent(
            &snapshot,
            context,
            &[
                edit(0, 0, "x"),
                edit(0, 0, "y"),
                AgentCommand::Select {
                    anchor: p(0),
                    head: p(1),
                },
                AgentCommand::BeginBatch,
                edit(0, 1, "z"),
                AgentCommand::EndBatch,
            ],
        )
        .unwrap();
        for request in &requests {
            assert_eq!(request.authorize(&policy, &registry, &snapshot), Ok(()));
            assert_eq!(
                request.authorize(&policy, &registry, &foreign),
                Err(RequestError::DifferentWorkspace)
            );
        }
        let before = other.capture();
        assert_eq!(
            apply_requests(&mut other, &policy, &registry, &requests)
                .unwrap_err()
                .code(),
            ErrorCode::CorruptSequence
        );
        assert_eq!(other.capture(), before);
        let before = workspace.capture();
        let empty_registry = ActorRegistry::new();
        assert_eq!(
            apply_requests(&mut workspace, &policy, &empty_registry, &requests)
                .unwrap_err()
                .code(),
            ErrorCode::UnknownActor
        );
        let mut revoked = registry.clone();
        revoked.revoke(context.actor).unwrap();
        assert_eq!(
            apply_requests(&mut workspace, &policy, &revoked, &requests)
                .unwrap_err()
                .code(),
            ErrorCode::RevokedActor
        );
        for capability in [Capability::Read, Capability::Commit] {
            let mut restricted = policy.clone();
            restricted.revoke_default(capability);
            assert_eq!(
                apply_requests(&mut workspace, &restricted, &registry, &requests)
                    .unwrap_err()
                    .code(),
                ErrorCode::PermissionDenied
            );
            assert_eq!(workspace.capture(), before);
        }
        let duplicate = vec![requests[0].clone(), requests[0].clone()];
        assert_eq!(
            apply_requests(&mut workspace, &policy, &registry, &duplicate)
                .unwrap_err()
                .code(),
            ErrorCode::RevisionMismatch
        );
        assert_eq!(workspace.capture(), before);
        for (replacement, endpoint, code) in [
            ("", 2, ErrorCode::OutOfRange),
            ("é", 1, ErrorCode::InvalidBoundary),
        ] {
            let first = lower_agent(&snapshot, context, &[edit(0, 2, replacement)]).unwrap();
            let source = lower_agent(
                &snapshot,
                context,
                &[
                    edit(0, 2, "cd"),
                    AgentCommand::Select {
                        anchor: p(endpoint),
                        head: p(endpoint),
                    },
                ],
            )
            .unwrap();
            let sequence = vec![first[0].clone(), source[1].clone()];
            assert_eq!(
                apply_requests(&mut workspace, &policy, &registry, &sequence)
                    .unwrap_err()
                    .code(),
                code
            );
            assert_eq!(workspace.capture(), before);
        }
    }

    #[test]
    fn group_trims_identity_edits_and_preserves_footprints() {
        use crate::editor::collab::footprints::Footprint;
        let (mut workspace, context) = live_fixture("abcdef");
        let snapshot = workspace.capture();
        let p = |n| AgentPosition::Byte(BytePos::new(n));
        let edit = |start, end, value| AgentCommand::Replace {
            start: p(start),
            end: p(end),
            text: Text::from_str(value),
        };
        let grouped = lower_agent(
            &snapshot,
            context,
            &[
                AgentCommand::BeginBatch,
                edit(0, 1, "a"),
                edit(4, 5, "Y"),
                AgentCommand::EndBatch,
            ],
        )
        .unwrap();
        let independent = lower_agent(&snapshot, context, &[edit(0, 1, "X")]).unwrap();
        let Operation::Edit(patches) = grouped[0].operation() else {
            panic!("expected edit")
        };
        let Operation::Edit(other) = independent[0].operation() else {
            panic!("expected edit")
        };
        let splices = patches[0].1.splices();
        assert_eq!(splices.len(), 1);
        assert_eq!(splices[0].replacement().as_str(), "Y");
        assert_eq!(
            (
                splices[0].span().start().as_usize(),
                splices[0].span().end().as_usize()
            ),
            (4, 5)
        );
        assert!(
            !Footprint::from_patches(patches.iter().map(|(id, patch)| (*id, patch))).conflicts(
                &Footprint::from_patches(other.iter().map(|(id, patch)| (*id, patch)))
            )
        );
        let noop = lower_agent(
            &snapshot,
            context,
            &[
                AgentCommand::BeginBatch,
                edit(0, 1, "a"),
                AgentCommand::EndBatch,
            ],
        )
        .unwrap();
        assert!(matches!(noop[0].operation(), Operation::Edit(patches) if patches.is_empty()));
        let (policy, registry) = access(context);
        apply_requests(&mut workspace, &policy, &registry, &noop).unwrap();
        assert_eq!(workspace.capture(), snapshot);
    }

    #[test]
    fn canceled_groups_emit_empty_patches_for_all_adapters() {
        let (snapshot, context) = fixture("aa");
        let p = |n| AgentPosition::Byte(BytePos::new(n));
        let e = |n| EmacsCharPos::new(n).unwrap();
        let n = |col| LineColByte::new(0, col);
        for cancel in [false, true] {
            let mut em = vec![
                EmacsCommand::BeginChangeGroup,
                EmacsCommand::Mark(e(1)),
                EmacsCommand::Point(e(2)),
                EmacsCommand::ReplaceRegion(Text::from_str(if cancel { "" } else { "a" })),
            ];
            let mut nv = vec![
                NeovimCommand::BeginUndoBlock,
                NeovimCommand::SetText {
                    start: n(0),
                    end: n(1),
                    text: Text::from_str(if cancel { "" } else { "a" }),
                },
            ];
            let mut ag = vec![
                AgentCommand::BeginBatch,
                AgentCommand::Replace {
                    start: p(0),
                    end: p(1),
                    text: Text::from_str(if cancel { "" } else { "a" }),
                },
            ];
            if cancel {
                em.extend([
                    EmacsCommand::Point(e(2)),
                    EmacsCommand::Insert(Text::from_str("a")),
                ]);
                nv.push(NeovimCommand::SetText {
                    start: n(1),
                    end: n(1),
                    text: Text::from_str("a"),
                });
                ag.push(AgentCommand::Replace {
                    start: p(1),
                    end: p(1),
                    text: Text::from_str("a"),
                });
            }
            em.push(EmacsCommand::EndChangeGroup);
            nv.push(NeovimCommand::EndUndoBlock);
            ag.push(AgentCommand::EndBatch);
            for requests in [
                lower_emacs(&snapshot, context, &em).unwrap(),
                lower_neovim(&snapshot, context, &nv).unwrap(),
                lower_agent(&snapshot, context, &ag).unwrap(),
            ] {
                assert_eq!(requests.len(), 1);
                assert!(
                    matches!(requests[0].operation(), Operation::Edit(patches) if patches.is_empty())
                );
                let mut workspace = VersionedWorkspace::resume_from_snapshot(&snapshot);
                let before = workspace.capture();
                let requests: Vec<_> = requests
                    .into_iter()
                    .map(|request| request.rebind_origin(before.clone()))
                    .collect();
                let (policy, registry) = access(context);
                apply_requests(&mut workspace, &policy, &registry, &requests).unwrap();
                assert_eq!(workspace.capture(), before);
            }
        }
    }

    #[test]
    fn group_identity_preserves_tracked_gap() {
        use crate::editor::collab::footprints::Footprint;
        let (snapshot, context) = fixture("abcde");
        let p = |n| AgentPosition::Byte(BytePos::new(n));
        let edit = |start, end, value| AgentCommand::Replace {
            start: p(start),
            end: p(end),
            text: Text::from_str(value),
        };
        let grouped = lower_agent(
            &snapshot,
            context,
            &[
                AgentCommand::BeginBatch,
                edit(0, 1, "X"),
                edit(1, 4, "bcd"),
                edit(4, 5, "Y"),
                AgentCommand::EndBatch,
            ],
        )
        .unwrap();
        let independent = lower_agent(&snapshot, context, &[edit(2, 3, "Z")]).unwrap();
        let Operation::Edit(patches) = grouped[0].operation() else {
            panic!("expected edit")
        };
        let Operation::Edit(other) = independent[0].operation() else {
            panic!("expected edit")
        };
        assert_eq!(patches.len(), 1);
        let splices: Vec<_> = patches[0]
            .1
            .splices()
            .iter()
            .map(|splice| {
                (
                    splice.span().start().as_usize(),
                    splice.span().end().as_usize(),
                    splice.replacement().as_str(),
                )
            })
            .collect();
        assert_eq!(splices, [(0, 1, "X"), (4, 5, "Y")]);
        assert!(
            !Footprint::from_patches(patches.iter().map(|(id, patch)| (*id, patch))).conflicts(
                &Footprint::from_patches(other.iter().map(|(id, patch)| (*id, patch)))
            )
        );
    }

    #[test]
    fn later_invalid_coordinates_report_the_coordinate_error() {
        let (snapshot, context) = fixture("abc");
        let zero = AgentPosition::Byte(BytePos::new(0));
        let bad = AgentPosition::Byte(BytePos::new(99));
        let e = [
            EmacsCommand::Point(EmacsCharPos::new(1).unwrap()),
            EmacsCommand::Insert(Text::from_str("X")),
            EmacsCommand::Point(EmacsCharPos::new(99).unwrap()),
        ];
        let n = [
            NeovimCommand::Cursor(LineColByte::new(0, 0)),
            NeovimCommand::Insert(Text::from_str("X")),
            NeovimCommand::SetText {
                start: LineColByte::new(0, 99),
                end: LineColByte::new(0, 99),
                text: Text::from_str("Y"),
            },
        ];
        let a = [
            AgentCommand::Replace {
                start: zero,
                end: zero,
                text: Text::from_str("X"),
            },
            AgentCommand::Replace {
                start: bad,
                end: bad,
                text: Text::from_str("Y"),
            },
        ];
        assert_eq!(
            lower_emacs(&snapshot, context, &e).unwrap_err().code(),
            ErrorCode::OutOfRange
        );
        assert_eq!(
            lower_neovim(&snapshot, context, &n).unwrap_err().code(),
            ErrorCode::OutOfRange
        );
        assert_eq!(
            lower_agent(&snapshot, context, &a).unwrap_err().code(),
            ErrorCode::OutOfRange
        );
        assert_eq!(
            snapshot.catalog().get(context.buffer).unwrap().as_str(),
            "abc"
        );
    }

    #[test]
    fn coordinates_are_checked_against_text_after_deletion() {
        let (snapshot, context) = fixture("abc");
        let a = |n| AgentPosition::Byte(BytePos::new(n));
        let agent_edit = AgentCommand::Replace {
            start: a(0),
            end: a(3),
            text: Text::empty(),
        };
        let agent = [
            agent_edit.clone(),
            AgentCommand::Replace {
                start: a(3),
                end: a(3),
                text: Text::empty(),
            },
        ];
        let agent_selection = [
            agent_edit,
            AgentCommand::Select {
                anchor: a(3),
                head: a(3),
            },
        ];
        let neovim_edit = NeovimCommand::SetText {
            start: LineColByte::new(0, 0),
            end: LineColByte::new(0, 3),
            text: Text::empty(),
        };
        let neovim = [
            neovim_edit.clone(),
            NeovimCommand::SetText {
                start: LineColByte::new(0, 3),
                end: LineColByte::new(0, 3),
                text: Text::empty(),
            },
        ];
        let neovim_selection = [
            neovim_edit,
            NeovimCommand::Select {
                anchor: LineColByte::new(0, 3),
                head: LineColByte::new(0, 3),
            },
        ];
        let emacs = [
            EmacsCommand::Mark(EmacsCharPos::new(1).unwrap()),
            EmacsCommand::Point(EmacsCharPos::new(4).unwrap()),
            EmacsCommand::ReplaceRegion(Text::empty()),
            EmacsCommand::Point(EmacsCharPos::new(4).unwrap()),
        ];
        assert_eq!(
            lower_agent(&snapshot, context, &agent).unwrap_err().code(),
            ErrorCode::OutOfRange
        );
        assert_eq!(
            lower_agent(&snapshot, context, &agent_selection)
                .unwrap_err()
                .code(),
            ErrorCode::OutOfRange
        );
        assert_eq!(
            lower_neovim(&snapshot, context, &neovim)
                .unwrap_err()
                .code(),
            ErrorCode::OutOfRange
        );
        assert_eq!(
            lower_neovim(&snapshot, context, &neovim_selection)
                .unwrap_err()
                .code(),
            ErrorCode::OutOfRange
        );
        assert_eq!(
            lower_emacs(&snapshot, context, &emacs).unwrap_err().code(),
            ErrorCode::OutOfRange
        );
        assert_eq!(
            snapshot.catalog().get(context.buffer).unwrap().as_str(),
            "abc"
        );
    }
}
