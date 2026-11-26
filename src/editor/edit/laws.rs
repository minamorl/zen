//! Cross-cutting edit operation invariants and reference-model laws.
//!
//! Verifies identity, disjoint commutativity, inverse restoration, failure
//! atomicity, and cross-entry-point equivalence against an independent plain
//! UTF-8 reference model.

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fmt::Debug;
    use std::ops::Range;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use crate::editor::collab::{
        access::{AccessPolicy, Capability},
        delivery::DeliveryLog,
        journal::CommitJournal,
        rebase::{rebase_over_step, RebaseStep},
        receipt::{CommitReceipt, RejectReason, SubmitOutcome},
        redo::{Redo, RedoError},
        registry::ActorRegistry,
        role::ActorRole,
        selective_undo::{SelectiveUndo, UndoError},
        sequencer::Sequencer,
    };
    use crate::editor::edit::{
        command::{self, CommandError, EditCommand},
        history::{EditHistory, HistoryError},
        ids::{ActorId, BufferId, Nonce, TxnId},
        indent_region::{self, IndentPolicy, IndentRegionError},
        intent::EditIntent,
        motion::MotionError,
        multi_cursor::{self, MultiCursorError},
        patch::{Patch, PatchError},
        recorded_commands::{
            self, FoldResult, RecordedAction, RecordedCommand, RecordedCommandError,
        },
        rectangle_edit::{
            self, Rectangle, RectangleEditError, RectangleOperation, ShortLinePolicy,
        },
        register_edit::{self, CopyTarget, CutReceipts, RegisterEditError},
        replace_all::{self, ReplaceAllError},
        restore::{self, RestoreError, RestoreOutcome},
        splice::{Splice, SpliceError},
        transaction::{Transaction, TransactionError},
        workspace_edit::{WorkspaceEdit, WorkspaceEditError},
    };
    use crate::editor::interop::{
        agent_position::AgentPosition,
        batch::{decode_batch, ExternalEdit},
        commands::{
            apply_requests, lower_agent, lower_emacs, lower_neovim, requests_equivalent,
            AgentCommand, Context as InteropContext, EmacsCommand, NeovimCommand,
        },
        coordinates::{BytePos, CoordKind, EmacsCharPos, LineColByte},
        error::ErrorCode,
        groups::{GroupError, GroupPlan, SequenceStep},
        region::{EndpointMode, OrientedRegion, RegionCoord},
        undo::{self as interop_undo, FrontendCommand, InteropError, Target as UndoTarget},
    };
    use crate::editor::test_support::{assert_law, Rng};
    use crate::editor::text::{
        anchors::{Affinity, Anchor},
        columns::VisualPosition,
        indent::{IndentError, IndentStyle},
        lines::LineIndex,
        rectangle::{Endpoint, RectangleError},
        search::SearchError,
        span::{ByteSpan, SpanError},
        utf8::Text,
    };
    use crate::editor::workspace::{
        annotations::{Annotation, AnnotationId, AnnotationKey, CollapsePolicy, NamespaceId},
        bookmarks::{Bookmark, BookmarkName},
        catalog::BufferCatalog,
        registers::{KillRings, RegisterEntry, RegisterKind, RegisterName, RegisterStore},
        selections::{Direction, Selection},
        session::{SessionError, WorkspaceSession},
        snapshots::{SnapshotStore, VersionedWorkspace, WorkspaceSnapshot},
        version::Revision,
        view::{BufferView, ViewError},
    };

    // =========================================================================
    // Independent Plain-UTF-8 Reference Model
    // =========================================================================

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RefEdit {
        start: usize,
        end: usize,
        text: String,
    }

    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    struct RefPlan {
        buffers: BTreeMap<u64, Vec<RefEdit>>,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum RefError {
        OutOfBounds,
        NotCharBoundary,
        InvertedRange,
        Contact,
        UnknownBuffer(u64),
        BufferClosed(u64),
        InvalidRegion,
        ZeroTabWidth,
        ShortLine,
        RowCount,
        EmptyPattern,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RefHistoryStep {
        previous_rev: u64,
        committed_rev: u64,
        txn: (u64, u64),
        before: BTreeMap<u64, String>,
        after: BTreeMap<u64, String>,
        forward: RefPlan,
        inverse: RefPlan,
    }

    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    struct RefWorkspace {
        live: BTreeMap<u64, String>,
        tombstones: BTreeSet<u64>,
        revision: u64,
        history: Vec<RefHistoryStep>,
    }

    impl RefWorkspace {
        fn new(entries: &[(u64, &str)], closed: &[u64]) -> Self {
            let mut live = BTreeMap::new();
            for &(id, text) in entries {
                live.insert(id, text.to_owned());
            }
            let mut tombstones = BTreeSet::new();
            for &id in closed {
                live.remove(&id);
                tombstones.insert(id);
            }
            Self {
                live,
                tombstones,
                revision: 0,
                history: Vec::new(),
            }
        }

        fn apply_plan_preview(
            &self,
            plan: &RefPlan,
        ) -> Result<(BTreeMap<u64, String>, RefPlan, bool), RefError> {
            let mut next_live = self.live.clone();
            let mut inverse = BTreeMap::new();
            let mut is_noop = true;
            for (&buf_id, edits) in &plan.buffers {
                let base = match self.live.get(&buf_id) {
                    Some(text) => text,
                    None if self.tombstones.contains(&buf_id) => {
                        return Err(RefError::BufferClosed(buf_id));
                    }
                    None => return Err(RefError::UnknownBuffer(buf_id)),
                };
                let (next_text, inv_edits) = ref_apply_single(base, edits)?;
                if next_text != *base {
                    is_noop = false;
                }
                next_live.insert(buf_id, next_text);
                inverse.insert(buf_id, inv_edits);
            }
            Ok((next_live, RefPlan { buffers: inverse }, is_noop))
        }

        fn commit_plan(
            &mut self,
            txn: (u64, u64),
            plan: &RefPlan,
            advance_on_noop: bool,
        ) -> Result<(RefPlan, bool), RefError> {
            let (next_live, inverse, is_noop) = self.apply_plan_preview(plan)?;
            if !is_noop || advance_on_noop {
                let prev = self.revision;
                self.revision += 1;
                let before = self.live.clone();
                self.live = next_live;
                self.history.push(RefHistoryStep {
                    previous_rev: prev,
                    committed_rev: self.revision,
                    txn,
                    before,
                    after: self.live.clone(),
                    forward: plan.clone(),
                    inverse: inverse.clone(),
                });
            }
            Ok((inverse, is_noop))
        }

        fn to_catalog(&self) -> BufferCatalog {
            let mut catalog = BufferCatalog::new();
            for (&id, text) in &self.live {
                catalog
                    .register(BufferId::new(id), Text::from_str(text))
                    .expect("fresh live buffer");
            }
            for &id in &self.tombstones {
                catalog
                    .register(BufferId::new(id), Text::empty())
                    .expect("fresh closed buffer");
                catalog.remove(BufferId::new(id)).expect("tombstone buffer");
            }
            catalog
        }
    }

    fn ref_validate_span(text: &str, start: usize, end: usize) -> Result<(), RefError> {
        if start > text.len() || end > text.len() {
            return Err(RefError::OutOfBounds);
        }
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            return Err(RefError::NotCharBoundary);
        }
        if start > end {
            return Err(RefError::InvertedRange);
        }
        Ok(())
    }

    fn ref_normalize_edits(edits: &[RefEdit]) -> Result<Vec<RefEdit>, RefError> {
        let mut sorted = edits.to_vec();
        sorted.sort_by_key(|e| (e.start, e.end));
        for pair in sorted.windows(2) {
            if pair[0].end >= pair[1].start {
                return Err(RefError::Contact);
            }
        }
        Ok(sorted)
    }

    fn ref_apply_single(base: &str, edits: &[RefEdit]) -> Result<(String, Vec<RefEdit>), RefError> {
        let sorted = ref_normalize_edits(edits)?;
        for edit in &sorted {
            ref_validate_span(base, edit.start, edit.end)?;
        }
        let mut out = String::new();
        let mut inv = Vec::with_capacity(sorted.len());
        let mut cursor = 0usize;
        for edit in &sorted {
            out.push_str(&base[cursor..edit.start]);
            let inv_start = out.len();
            out.push_str(&edit.text);
            let inv_end = out.len();
            inv.push(RefEdit {
                start: inv_start,
                end: inv_end,
                text: base[edit.start..edit.end].to_owned(),
            });
            cursor = edit.end;
        }
        out.push_str(&base[cursor..]);
        Ok((out, inv))
    }

    fn ref_edits_contact(a: &RefEdit, b: &RefEdit) -> bool {
        a.start <= b.end && b.start <= a.end
    }

    fn ref_transform_disjoint_single(
        target: &[RefEdit],
        applied: &[RefEdit],
    ) -> Result<Vec<RefEdit>, RefError> {
        let norm_target = ref_normalize_edits(target)?;
        let norm_applied = ref_normalize_edits(applied)?;
        for t in &norm_target {
            for a in &norm_applied {
                if ref_edits_contact(t, a) {
                    return Err(RefError::Contact);
                }
            }
        }
        let shift = |pos: usize| -> usize {
            let mut delta: isize = 0;
            for a in &norm_applied {
                if a.end < pos {
                    delta += a.text.len() as isize - (a.end - a.start) as isize;
                }
            }
            (pos as isize + delta) as usize
        };
        Ok(norm_target
            .into_iter()
            .map(|t| RefEdit {
                start: shift(t.start),
                end: shift(t.end),
                text: t.text,
            })
            .collect())
    }

    fn ref_transform_disjoint_plan(
        target: &RefPlan,
        applied: &RefPlan,
    ) -> Result<RefPlan, RefError> {
        let mut buffers = BTreeMap::new();
        for (&buf_id, edits) in &target.buffers {
            let transformed = if let Some(other) = applied.buffers.get(&buf_id) {
                ref_transform_disjoint_single(edits, other)?
            } else {
                ref_normalize_edits(edits)?
            };
            buffers.insert(buf_id, transformed);
        }
        Ok(RefPlan { buffers })
    }

    fn ref_compose_plans(left: &RefPlan, right: &RefPlan) -> Result<RefPlan, RefError> {
        let mut buffers = left.buffers.clone();
        for (&buf_id, edits) in &right.buffers {
            if let Some(existing) = buffers.get(&buf_id) {
                let mut merged = existing.clone();
                merged.extend(edits.iter().cloned());
                buffers.insert(buf_id, ref_normalize_edits(&merged)?);
            } else {
                buffers.insert(buf_id, ref_normalize_edits(edits)?);
            }
        }
        Ok(RefPlan { buffers })
    }

    /// Independent line splitting on LF only; CR before LF is tracked separately.
    fn ref_line_ranges(text: &str) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        let mut start = 0usize;
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                ranges.push((start, i));
                start = i + 1;
            }
        }
        ranges.push((start, text.len()));
        ranges
    }

    #[derive(Clone, Copy)]
    struct RefRectSpec<'a> {
        first_line: usize,
        first_col: usize,
        second_line: usize,
        second_col: usize,
        inclusive: bool,
        tab_width: usize,
        clamp_short: bool,
        op: RectangleOperation<'a>,
    }

    /// Independent visual-column rectangle edit planner operating directly on `&str`.
    fn ref_rectangle_edits(text: &str, spec: RefRectSpec<'_>) -> Result<Vec<RefEdit>, RefError> {
        if spec.tab_width == 0 {
            return Err(RefError::ZeroTabWidth);
        }
        let lines = ref_line_ranges(text);
        let top = spec.first_line.min(spec.second_line);
        let bottom = spec.first_line.max(spec.second_line);
        if bottom >= lines.len() {
            return Err(RefError::InvalidRegion);
        }
        let row_count = bottom - top + 1;
        if let RectangleOperation::Replace(rows) = spec.op {
            if rows.len() != row_count {
                return Err(RefError::RowCount);
            }
        }
        let left = spec.first_col.min(spec.second_col) as u128;
        let right = spec.first_col.max(spec.second_col) as u128;
        let is_insert = matches!(spec.op, RectangleOperation::Insert(_));
        let tab = spec.tab_width as u128;

        let mut edits = Vec::with_capacity(row_count);
        for (row_idx, &(raw_start, raw_end)) in lines[top..=bottom].iter().enumerate() {
            let logical_end = if text.as_bytes().get(raw_end) == Some(&b'\n')
                && raw_end > raw_start
                && text.as_bytes()[raw_end - 1] == b'\r'
            {
                raw_end - 1
            } else {
                raw_end
            };
            // Build (byte_offset, visual_col) boundaries for this line's logical content.
            let mut bounds: Vec<(usize, u128)> = vec![(raw_start, 0)];
            let mut col = 0u128;
            for (rel, ch) in text[raw_start..logical_end].char_indices() {
                let cells = if ch == '\t' { tab - (col % tab) } else { 1 };
                col += cells;
                bounds.push((raw_start + rel + ch.len_utf8(), col));
            }
            let line_width = col;
            if !spec.clamp_short {
                let required = if is_insert {
                    left
                } else if spec.inclusive {
                    right + 1
                } else {
                    right
                };
                if line_width < required {
                    return Err(RefError::ShortLine);
                }
            }
            let (span_start, span_end) = if is_insert {
                let begin = bounds
                    .iter()
                    .rev()
                    .find(|&&(_, c)| c <= left)
                    .map_or(logical_end, |&(b, _)| b);
                (begin, begin)
            } else {
                let begin = bounds
                    .iter()
                    .rev()
                    .find(|&&(_, c)| c <= left)
                    .map_or(logical_end, |&(b, _)| b);
                let target_end = if spec.inclusive { right + 1 } else { right };
                let finish = if !spec.inclusive && left == right {
                    begin
                } else {
                    bounds
                        .iter()
                        .find(|&&(_, c)| c >= target_end)
                        .map_or(logical_end, |&(b, _)| b)
                };
                (begin, finish)
            };
            let repl = match spec.op {
                RectangleOperation::Delete => "",
                RectangleOperation::Replace(rows) => rows[row_idx],
                RectangleOperation::Uniform(s) | RectangleOperation::Insert(s) => s,
            };
            edits.push(RefEdit {
                start: span_start,
                end: span_end,
                text: repl.to_owned(),
            });
        }
        Ok(edits)
    }

    /// Independent leading-indentation planner operating directly on `&str`.
    fn ref_indent_region_edits(
        text: &str,
        region: Range<usize>,
        policy: IndentPolicy,
    ) -> Result<Vec<RefEdit>, RefError> {
        let lines = ref_line_ranges(text);
        if region.start > region.end || region.end > lines.len() {
            return Err(RefError::InvalidRegion);
        }
        if policy.tab_width == 0 {
            return Err(RefError::ZeroTabWidth);
        }
        if region.is_empty() {
            return Ok(Vec::new());
        }
        let target = match policy.style {
            IndentStyle::Spaces => " ".repeat(policy.column),
            IndentStyle::TabsAndSpaces => format!(
                "{}{}",
                "\t".repeat(policy.column / policy.tab_width),
                " ".repeat(policy.column % policy.tab_width)
            ),
        };
        let mut edits = Vec::new();
        for &(start, end) in &lines[region] {
            let leading_len = text.as_bytes()[start..end]
                .iter()
                .take_while(|&&b| b == b' ' || b == b'\t')
                .count();
            let current = &text[start..start + leading_len];
            if current != target {
                edits.push(RefEdit {
                    start,
                    end: start + leading_len,
                    text: target.clone(),
                });
            }
        }
        Ok(edits)
    }

    /// Independent leftmost-first literal replace-all operating directly on `&str`.
    fn ref_replace_all_edits(
        text: &str,
        span_start: usize,
        span_end: usize,
        pattern: &str,
        replacement: &str,
    ) -> Result<Vec<RefEdit>, RefError> {
        ref_validate_span(text, span_start, span_end)?;
        if pattern.is_empty() {
            return Err(RefError::EmptyPattern);
        }
        let slice = &text[span_start..span_end];
        let mut edits: Vec<RefEdit> = Vec::new();
        let mut pos = 0usize;
        while let Some(rel) = slice[pos..].find(pattern) {
            let m_start = span_start + pos + rel;
            let m_end = m_start + pattern.len();
            if let Some(last) = edits.last_mut() {
                if last.end == m_start {
                    last.end = m_end;
                    last.text.push_str(replacement);
                    pos += rel + pattern.len();
                    continue;
                }
            }
            edits.push(RefEdit {
                start: m_start,
                end: m_end,
                text: replacement.to_owned(),
            });
            pos += rel + pattern.len();
        }
        Ok(edits)
    }

    /// Independent register cut calculator (validates all, drops empty, coalesces adjacent).
    fn ref_register_cut(
        text: &str,
        ranges: &[(usize, usize)],
    ) -> Result<(String, Vec<RefEdit>), RefError> {
        let mut nonempty = Vec::new();
        for &(s, e) in ranges {
            ref_validate_span(text, s, e)?;
            if s < e {
                nonempty.push((s, e));
            }
        }
        nonempty.sort();
        nonempty.dedup();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (s, e) in nonempty {
            if let Some(last) = merged.last_mut() {
                if s < last.1 {
                    return Err(RefError::Contact);
                }
                if s == last.1 {
                    last.1 = e;
                    continue;
                }
            }
            merged.push((s, e));
        }
        let mut payload = String::new();
        let mut edits = Vec::with_capacity(merged.len());
        for (s, e) in merged {
            payload.push_str(&text[s..e]);
            edits.push(RefEdit {
                start: s,
                end: e,
                text: String::new(),
            });
        }
        Ok((payload, edits))
    }

    /// Independent register paste calculator (dedups, rejects overlap, coalesces adjacent).
    fn ref_register_paste(
        text: &str,
        ranges: &[(usize, usize)],
        payload: &str,
    ) -> Result<Vec<RefEdit>, RefError> {
        let mut sorted = Vec::with_capacity(ranges.len());
        for &(s, e) in ranges {
            ref_validate_span(text, s, e)?;
            sorted.push((s, e));
        }
        sorted.sort();
        sorted.dedup();
        let mut edits: Vec<RefEdit> = Vec::new();
        for (s, e) in sorted {
            if let Some(last) = edits.last_mut() {
                if s < last.end {
                    return Err(RefError::Contact);
                }
                if s == last.end {
                    last.end = e;
                    last.text.push_str(payload);
                    continue;
                }
            }
            edits.push(RefEdit {
                start: s,
                end: e,
                text: payload.to_owned(),
            });
        }
        Ok(edits)
    }

    /// Independent sequential change-group text transition directly on `String`.
    fn ref_apply_sequential_steps(base: &str, steps: &[(usize, usize, &str)]) -> String {
        let mut current = base.to_owned();
        for &(s, e, repl) in steps {
            current.replace_range(s..e, repl);
        }
        current
    }

    /// Independent single-offset transport across disjoint edits (right-to-left).
    fn ref_transport_offset(offset: usize, applied: &[RefEdit], right_affinity: bool) -> usize {
        let mut pos = offset;
        for edit in applied.iter().rev() {
            if edit.start == edit.end && edit.text.is_empty() {
                continue;
            }
            if pos < edit.start {
                // strictly before edit: unchanged
            } else if pos > edit.end {
                pos = pos - (edit.end - edit.start) + edit.text.len();
            } else {
                pos = if right_affinity {
                    edit.start + edit.text.len()
                } else {
                    edit.start
                };
            }
        }
        pos
    }

    /// Independent minimal single-span diff (longest common scalar prefix, then suffix).
    fn ref_diff_edit(before: &str, after: &str) -> Option<RefEdit> {
        if before == after {
            return None;
        }
        let prefix: usize = before
            .chars()
            .zip(after.chars())
            .take_while(|(a, b)| a == b)
            .map(|(ch, _)| ch.len_utf8())
            .sum();
        let suffix: usize = before[prefix..]
            .chars()
            .rev()
            .zip(after[prefix..].chars().rev())
            .take_while(|(a, b)| a == b)
            .map(|(ch, _)| ch.len_utf8())
            .sum();
        Some(RefEdit {
            start: prefix,
            end: before.len() - suffix,
            text: after[prefix..after.len() - suffix].to_owned(),
        })
    }

    /// Independent multi-buffer restoration plan against plain UTF-8 target texts.
    fn ref_restore_plan(current: &BTreeMap<u64, String>, targets: &[(u64, &str)]) -> RefPlan {
        let mut buffers = BTreeMap::new();
        for &(id, target) in targets {
            if let Some(cur) = current.get(&id) {
                if let Some(edit) = ref_diff_edit(cur, target) {
                    buffers.insert(id, vec![edit]);
                }
            }
        }
        RefPlan { buffers }
    }

    /// Rebase a plan based at `base_rev` through all subsequent reference history steps.
    fn ref_rebase_plan_from(
        history: &[RefHistoryStep],
        base_rev: u64,
        plan: &RefPlan,
    ) -> Result<RefPlan, RefError> {
        let mut cur = plan.clone();
        for step in history.iter().filter(|h| h.previous_rev >= base_rev) {
            cur = ref_transform_disjoint_plan(&cur, &step.forward)?;
        }
        Ok(cur)
    }

    // =========================================================================
    // Comparators & Full State-Snapshot Atomicity Detector
    // =========================================================================

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ActorSessionSnapshot {
        texts: BTreeMap<BufferId, Option<String>>,
        views: BTreeMap<BufferId, Option<BufferView>>,
        selections: BTreeMap<BufferId, Option<Selection>>,
        bookmarks: BTreeMap<(BufferId, String), Option<Bookmark>>,
        overlays: BTreeMap<AnnotationKey, Option<Annotation>>,
        registers: BTreeMap<String, Option<RegisterEntry>>,
        current_kill: Option<RegisterEntry>,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct SessionObservation {
        revision: Revision,
        actors: BTreeMap<ActorId, ActorSessionSnapshot>,
    }

    fn capture_session_observation(
        session: &WorkspaceSession,
        actors: &[ActorId],
        buffers: &[BufferId],
        bookmark_names: &[&str],
        overlay_keys: &[AnnotationKey],
        register_names: &[&str],
    ) -> SessionObservation {
        let mut per_actor = BTreeMap::new();
        for &actor in actors {
            let mut texts = BTreeMap::new();
            let mut views = BTreeMap::new();
            let mut selections = BTreeMap::new();
            let mut bookmarks = BTreeMap::new();
            for &buf in buffers {
                texts.insert(
                    buf,
                    session.read(actor, buf).ok().map(|t| t.as_str().to_owned()),
                );
                views.insert(buf, session.view(actor, buf).ok().flatten());
                selections.insert(buf, session.selection(actor, buf).ok().flatten());
                for &bname in bookmark_names {
                    let name = BookmarkName::new(bname).expect("valid bookmark name");
                    bookmarks.insert(
                        (buf, bname.to_owned()),
                        session.bookmark(actor, buf, &name).ok().flatten(),
                    );
                }
            }
            let mut overlays = BTreeMap::new();
            for &key in overlay_keys {
                overlays.insert(key, session.overlay(actor, key).ok().flatten().cloned());
            }
            let mut registers = BTreeMap::new();
            for &rname in register_names {
                let name = RegisterName::new(rname).expect("valid register name");
                registers.insert(rname.to_owned(), session.register(actor, &name).cloned());
            }
            per_actor.insert(
                actor,
                ActorSessionSnapshot {
                    texts,
                    views,
                    selections,
                    bookmarks,
                    overlays,
                    registers,
                    current_kill: session.current_kill(actor).cloned(),
                },
            );
        }
        SessionObservation {
            revision: session.revision(),
            actors: per_actor,
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct FullStateSnapshot {
        snapshot: WorkspaceSnapshot,
        snapshots: SnapshotStore,
        journal: CommitJournal,
        history: EditHistory,
        registers: RegisterStore,
        rings: KillRings,
        groups: GroupPlan,
        session: SessionObservation,
    }

    fn verify_catalog_matches_ref(
        catalog: &BufferCatalog,
        ref_ws: &RefWorkspace,
    ) -> Result<(), String> {
        let expected_live: Vec<BufferId> = ref_ws.live.keys().copied().map(BufferId::new).collect();
        if catalog.live_ids() != expected_live {
            return Err(format!(
                "live buffer IDs diverged: actual {:?} != ref {:?}",
                catalog.live_ids(),
                expected_live
            ));
        }
        let expected_dead: Vec<BufferId> = ref_ws
            .tombstones
            .iter()
            .copied()
            .map(BufferId::new)
            .collect();
        if catalog.tombstoned_ids() != expected_dead {
            return Err(format!(
                "tombstone IDs diverged: actual {:?} != ref {:?}",
                catalog.tombstoned_ids(),
                expected_dead
            ));
        }
        for (&id, expected_text) in &ref_ws.live {
            let actual = catalog
                .get(BufferId::new(id))
                .ok_or_else(|| format!("missing live buffer {id}"))?;
            if actual.as_str() != expected_text {
                return Err(format!(
                    "buffer {id} text diverged: actual {:?} != ref {:?}",
                    actual.as_str(),
                    expected_text
                ));
            }
        }
        Ok(())
    }

    fn assert_catalog_matches_ref(catalog: &BufferCatalog, ref_ws: &RefWorkspace, context: &str) {
        if let Err(err) = verify_catalog_matches_ref(catalog, ref_ws) {
            panic!("reference model mismatch ({context}): {err}");
        }
    }

    fn verify_state_unchanged<T: PartialEq + Debug>(
        before: &T,
        after: &T,
        context: &str,
    ) -> Result<(), String> {
        if before != after {
            return Err(format!(
                "failure atomicity violated ({context}):\n  before: {before:?}\n  after:  {after:?}"
            ));
        }
        Ok(())
    }

    fn assert_state_unchanged<T: PartialEq + Debug>(before: &T, after: &T, context: &str) {
        if let Err(err) = verify_state_unchanged(before, after, context) {
            panic!("{err}");
        }
    }

    fn verify_inverse_outcome(
        base: &WorkspaceSnapshot,
        txn: &Transaction,
        after_catalog: &BufferCatalog,
        inverse: &Transaction,
    ) -> Result<(), String> {
        let restored = inverse
            .apply_to_catalog(after_catalog)
            .map_err(|e| format!("inverse apply failed: {e}"))?;
        if restored != *base.catalog() {
            return Err(format!(
                "inverse did not restore base catalog: got {restored:?}, expected {:?}",
                base.catalog()
            ));
        }
        if txn.patches().len() != inverse.patches().len() {
            return Err(format!(
                "inverse patch count mismatch: forward {} != inverse {}",
                txn.patches().len(),
                inverse.patches().len()
            ));
        }
        // Double-inversion involution on each buffer's patch.
        for ((buf_a, fwd_patch), (buf_b, inv_patch)) in
            txn.patches().iter().zip(inverse.patches().iter())
        {
            if buf_a != buf_b {
                return Err(format!("buffer id mismatch in inverse: {buf_a} vs {buf_b}"));
            }
            let after_text = after_catalog
                .get(*buf_a)
                .ok_or_else(|| format!("missing buffer {buf_a} after apply"))?;
            let (back_text, inv_inv_patch) = inv_patch
                .apply_with_inverse(after_text)
                .map_err(|e| format!("double inverse failed: {e}"))?;
            let orig_text = base
                .catalog()
                .get(*buf_a)
                .ok_or_else(|| format!("missing buffer {buf_a} in base"))?;
            if back_text != *orig_text {
                return Err(format!(
                    "double inverse text mismatch on buffer {buf_a}: {:?} != {:?}",
                    back_text.as_str(),
                    orig_text.as_str()
                ));
            }
            if inv_inv_patch != *fwd_patch {
                return Err(format!(
                    "double inverse patch not involutive on buffer {buf_a}: {inv_inv_patch:?} != {fwd_patch:?}"
                ));
            }
        }
        Ok(())
    }

    fn verify_inverse_restores(base: &WorkspaceSnapshot, txn: &Transaction) -> Result<(), String> {
        let res = txn
            .apply_with_inverse(base)
            .map_err(|e| format!("forward apply failed: {e}"))?;
        verify_inverse_outcome(base, txn, res.catalog(), res.inverse())
    }

    fn assert_inverse_restores(base: &WorkspaceSnapshot, txn: &Transaction, context: &str) {
        if let Err(err) = verify_inverse_restores(base, txn) {
            panic!("inverse law failed ({context}): {err}");
        }
    }

    fn verify_commutativity(
        ab_catalog: &BufferCatalog,
        ba_catalog: &BufferCatalog,
        simultaneous_catalog: &BufferCatalog,
        expected_ref: &RefWorkspace,
    ) -> Result<(), String> {
        if ab_catalog != ba_catalog {
            return Err(format!(
                "A-then-B catalog differs from B-then-A catalog:\n  AB: {ab_catalog:?}\n  BA: {ba_catalog:?}"
            ));
        }
        if ab_catalog != simultaneous_catalog {
            return Err(format!(
                "sequential catalog differs from simultaneous catalog:\n  seq: {ab_catalog:?}\n  sim: {simultaneous_catalog:?}"
            ));
        }
        verify_catalog_matches_ref(ab_catalog, expected_ref)
    }

    fn assert_commutativity(
        ab_catalog: &BufferCatalog,
        ba_catalog: &BufferCatalog,
        simultaneous_catalog: &BufferCatalog,
        expected_ref: &RefWorkspace,
        context: &str,
    ) {
        if let Err(err) =
            verify_commutativity(ab_catalog, ba_catalog, simultaneous_catalog, expected_ref)
        {
            panic!("commutativity law failed ({context}): {err}");
        }
    }

    // =========================================================================
    // Production Helpers (Bridges between RefPlan and Production Types)
    // =========================================================================

    fn patch_from_ref(text: &Text, edits: &[RefEdit]) -> Patch {
        let splices = edits
            .iter()
            .map(|e| {
                let span = ByteSpan::validate_in(text, e.start, e.end)
                    .expect("reference edit span valid in text");
                Splice::new(span, Text::from_str(&e.text))
            })
            .collect();
        Patch::new(splices).expect("reference edits are strictly separated")
    }

    fn txn_from_ref(
        snapshot: &WorkspaceSnapshot,
        actor: ActorId,
        nonce: u64,
        plan: &RefPlan,
    ) -> Transaction {
        let patches = plan
            .buffers
            .iter()
            .map(|(&buf_id, edits)| {
                let id = BufferId::new(buf_id);
                let text = snapshot.catalog().get(id).expect("live buffer");
                (id, patch_from_ref(text, edits))
            })
            .collect();
        Transaction::new(
            EditIntent::for_actor(actor, Nonce::new(nonce), snapshot.revision()),
            patches,
        )
        .expect("distinct buffer IDs")
    }

    fn ref_plan_from_txn(txn: &Transaction) -> RefPlan {
        let mut buffers = BTreeMap::new();
        for (id, patch) in txn.patches() {
            let edits = patch
                .splices()
                .iter()
                .map(|s| RefEdit {
                    start: s.span().start().as_usize(),
                    end: s.span().end().as_usize(),
                    text: s.replacement().as_str().to_owned(),
                })
                .collect();
            buffers.insert(id.raw(), edits);
        }
        RefPlan { buffers }
    }

    fn default_actors() -> ActorRegistry {
        let mut reg = ActorRegistry::new();
        reg.register(ActorId::new(1), ActorRole::Human)
            .expect("actor 1");
        reg.register(ActorId::new(2), ActorRole::Agent)
            .expect("actor 2");
        reg
    }

    fn full_access_policy() -> AccessPolicy {
        let mut policy = AccessPolicy::new();
        policy.grant_default(Capability::Read);
        policy.grant_default(Capability::Commit);
        policy
    }

    fn byte_to_emacs_pos(text: &str, byte_offset: usize) -> EmacsCharPos {
        EmacsCharPos::new(1 + text[..byte_offset].chars().count()).expect("nonzero emacs pos")
    }

    fn byte_to_neovim_pos(text: &str, byte_offset: usize) -> LineColByte {
        let prefix = &text[..byte_offset];
        let line = prefix.bytes().filter(|&b| b == b'\n').count();
        let col = match prefix.rfind('\n') {
            Some(lf) => byte_offset - (lf + 1),
            None => byte_offset,
        };
        LineColByte::new(line, col)
    }

    fn populate_session_for_actor(
        session: &mut WorkspaceSession,
        actor: ActorId,
        buffer: BufferId,
        anchor_offset: usize,
        head_offset: usize,
    ) {
        let text = session.read(actor, buffer).expect("read").clone();
        let view = BufferView::narrowed(
            &catalog_for_single(buffer, &text),
            buffer,
            ByteSpan::full_of(&text),
        )
        .expect("full view");
        let a = text.validate_offset(anchor_offset).expect("anchor");
        let h = text.validate_offset(head_offset).expect("head");
        let sel = Selection::new(&catalog_for_single(buffer, &text), &view, buffer, a, h)
            .expect("selection");
        session.set_view(actor, view).expect("set view");
        session.set_selection(actor, sel).expect("set selection");
        session
            .set_bookmark(
                actor,
                BookmarkName::new("bm").expect("bm"),
                Bookmark {
                    buffer,
                    anchor: Anchor {
                        offset: h,
                        affinity: Affinity::Right,
                    },
                    revision: session.revision(),
                },
            )
            .expect("set bookmark");
        let span = ByteSpan::new(a.min(h), a.max(h)).expect("ordered");
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
                    span,
                    start_affinity: Affinity::Left,
                    end_affinity: Affinity::Right,
                    priority: 0,
                    collapse: CollapsePolicy::RetainEmpty,
                    label: "mark".into(),
                },
            )
            .expect("set overlay");
    }

    fn catalog_for_single(buffer: BufferId, text: &Text) -> BufferCatalog {
        let mut cat = BufferCatalog::new();
        cat.register(buffer, text.clone()).expect("register");
        cat
    }

    fn corner_texts() -> Vec<&'static str> {
        vec![
            "",
            "\n",
            "\n\n",
            "\r\n",
            "\r",
            "ascii",
            "a\nb\nc",
            "a\r\nb\r\nc",
            "a\rb\r\nc\n",
            "日本語漢字",
            "a界b😀c",
            "e\u{0301}界😀\r\n\t次e\u{0301}",
            " \t界\n\t  😀\r\n  \n\t",
            "aaaa\n  aa  \r\n\taaa",
        ]
    }

    fn char_boundaries_of(s: &str) -> Vec<usize> {
        s.char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(s.len()))
            .collect()
    }

    fn random_text(rng: &mut Rng, max_chunks: usize) -> String {
        let chunks = [
            "a",
            "b",
            " ",
            "\t",
            "\n",
            "\r",
            "\r\n",
            "é",
            "界",
            "e\u{0301}",
            "😀",
            "aa",
        ];
        let count = rng.gen_range(0..max_chunks + 1);
        let mut s = String::new();
        for _ in 0..count {
            s.push_str(rng.choose(&chunks).expect("nonempty chunks"));
        }
        s
    }

    fn random_valid_span(rng: &mut Rng, s: &str) -> (usize, usize) {
        let bounds = char_boundaries_of(s);
        let i = rng.gen_range(0..bounds.len());
        let j = rng.gen_range(0..bounds.len());
        let (a, b) = (bounds[i.min(j)], bounds[i.max(j)]);
        (a, b)
    }

    fn random_disjoint_pair(rng: &mut Rng, s: &str) -> Option<((usize, usize), (usize, usize))> {
        let bounds = char_boundaries_of(s);
        // Need at least 2 distinct scalar boundaries with a >= 1-scalar gap between spans,
        // i.e., a_end < b_start -> requires at least 2 boundaries (nonempty string).
        if bounds.len() < 2 {
            return None;
        }
        // Pick 0 <= i <= j < k <= l < bounds.len()
        let split = rng.gen_range(0..bounds.len() - 1);
        let i = rng.gen_range(0..split + 1);
        let j = rng.gen_range(i..split + 1);
        let k = rng.gen_range(split + 1..bounds.len());
        let l = rng.gen_range(k..bounds.len());
        let left = (bounds[i], bounds[j]);
        let right = (bounds[k], bounds[l]);
        assert!(left.1 < right.0);
        if rng.gen_bool() {
            Some((left, right))
        } else {
            Some((right, left))
        }
    }

    // =========================================================================
    // Test 1: Negative Self-Check (Comparator & Atomicity Detector)
    // =========================================================================

    #[test]
    fn negative_self_check_detects_drift_and_atomicity_violations() {
        let mut ref_ws = RefWorkspace::new(&[(0, "a界b😀"), (1, "line1\r\nline2")], &[2]);
        let mut catalog = ref_ws.to_catalog();
        assert_eq!(verify_catalog_matches_ref(&catalog, &ref_ws), Ok(()));

        // 1a. Inject 1-scalar UTF-8 text drift into catalog.
        let mut drifted_text = catalog.clone();
        drifted_text
            .update(BufferId::new(0), Text::from_str("a界b😁"))
            .expect("update");
        assert!(verify_catalog_matches_ref(&drifted_text, &ref_ws).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| {
            assert_catalog_matches_ref(&drifted_text, &ref_ws, "injected text drift");
        }))
        .is_err());

        // 1b. Inject extra live buffer drift.
        let mut extra_buf = catalog.clone();
        extra_buf
            .register(BufferId::new(9), Text::from_str("extra"))
            .expect("register");
        assert!(verify_catalog_matches_ref(&extra_buf, &ref_ws).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| {
            assert_catalog_matches_ref(&extra_buf, &ref_ws, "injected extra buffer");
        }))
        .is_err());

        // 1c. Inject tombstone drift.
        let mut extra_tomb = catalog.clone();
        extra_tomb.remove(BufferId::new(1)).expect("remove");
        assert!(verify_catalog_matches_ref(&extra_tomb, &ref_ws).is_err());
        ref_ws.live.remove(&1);
        assert!(verify_catalog_matches_ref(&extra_tomb, &ref_ws).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| {
            assert_catalog_matches_ref(&extra_tomb, &ref_ws, "injected tombstone drift");
        }))
        .is_err());

        // 2. Full state-snapshot atomicity detector catches EVERY component's drift.
        let base_ref = RefWorkspace::new(&[(0, "abcd"), (1, "wxyz")], &[2]);
        catalog = base_ref.to_catalog();
        let mut seq = Sequencer::new(catalog, default_actors());
        let mut log = DeliveryLog::new(seq.snapshot());
        let mut session =
            WorkspaceSession::new(seq.snapshot(), default_actors(), full_access_policy());
        let actor = ActorId::new(1);
        let buf0 = BufferId::new(0);
        let buf1 = BufferId::new(1);
        populate_session_for_actor(&mut session, actor, buf0, 1, 2);
        populate_session_for_actor(&mut session, actor, buf1, 0, 1);
        let receipts = CutReceipts::default();
        let groups = GroupPlan::new();

        let overlay_keys = [
            AnnotationKey {
                buffer: buf0,
                namespace: NamespaceId::new(1),
                id: AnnotationId::new(1),
            },
            AnnotationKey {
                buffer: buf1,
                namespace: NamespaceId::new(1),
                id: AnnotationId::new(1),
            },
        ];
        let snap_full = |s: &Sequencer,
                         r: &CutReceipts,
                         g: &GroupPlan,
                         sess: &WorkspaceSession|
         -> FullStateSnapshot {
            FullStateSnapshot {
                snapshot: s.snapshot(),
                snapshots: s.snapshots().clone(),
                journal: s.journal().clone(),
                history: s.history().clone(),
                registers: r.registers().clone(),
                rings: r.rings().clone(),
                groups: g.clone(),
                session: capture_session_observation(
                    sess,
                    &[ActorId::new(1), ActorId::new(2)],
                    &[buf0, buf1, BufferId::new(2)],
                    &["bm"],
                    &overlay_keys,
                    &["a"],
                ),
            }
        };

        let before = snap_full(&seq, &receipts, &groups, &session);
        assert_eq!(
            verify_state_unchanged(&before, &before, "identical"),
            Ok(())
        );

        // 2a. Mutate register only.
        let mut reg_drift = before.clone();
        reg_drift.registers.put(
            actor,
            RegisterName::new("a").expect("name"),
            RegisterEntry {
                text: Text::from_str("leak"),
                kind: RegisterKind::Charwise,
            },
        );
        assert!(verify_state_unchanged(&before, &reg_drift, "register drift").is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| {
            assert_state_unchanged(&before, &reg_drift, "register drift");
        }))
        .is_err());

        // 2b. Mutate kill ring only.
        let mut ring_drift = before.clone();
        ring_drift.rings.get_mut(actor).push(RegisterEntry {
            text: Text::from_str("kill"),
            kind: RegisterKind::Charwise,
        });
        assert!(verify_state_unchanged(&before, &ring_drift, "kill ring drift").is_err());

        // 2c. Mutate session selection only.
        let mut sess_mut = session.clone();
        let text0 = sess_mut.read(actor, buf0).expect("read").clone();
        let view0 = sess_mut.view(actor, buf0).expect("view").expect("some");
        let moved_sel = Selection::new(
            seq.snapshot().catalog(),
            &view0,
            buf0,
            text0.validate_offset(2).expect("2"),
            text0.validate_offset(3).expect("3"),
        )
        .expect("sel");
        sess_mut.set_selection(actor, moved_sel).expect("set sel");
        let sel_drift = snap_full(&seq, &receipts, &groups, &sess_mut);
        assert!(verify_state_unchanged(&before, &sel_drift, "selection drift").is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| {
            assert_state_unchanged(&before, &sel_drift, "selection drift");
        }))
        .is_err());

        // 2d. Mutate sequencer / history / journal / groups via real commit.
        let plan = RefPlan {
            buffers: BTreeMap::from([(
                0,
                vec![RefEdit {
                    start: 0,
                    end: 1,
                    text: "X".into(),
                }],
            )]),
        };
        let txn = txn_from_ref(&seq.snapshot(), actor, 1, &plan);
        let outcome = seq.submit(txn);
        let receipt = outcome.committed_receipt().expect("committed");
        log.record_workspace(&seq, receipt).expect("record");
        let mut groups_mut = groups.clone();
        groups_mut.project(outcome, None).expect("project");
        let commit_drift = snap_full(&seq, &receipts, &groups_mut, &session);
        assert!(verify_state_unchanged(&before, &commit_drift, "commit drift").is_err());

        // 3. Inverse and commutativity detectors fail on injected mismatches.
        let base_ws = VersionedWorkspace::new(base_ref.to_catalog());
        let base_snap = base_ws.capture();
        let bad_ab = base_ref.to_catalog();
        let mut bad_ba = base_ref.to_catalog();
        bad_ba.update(buf0, Text::from_str("Xbcd")).expect("update");
        assert!(verify_commutativity(&bad_ab, &bad_ba, &bad_ab, &base_ref).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| {
            assert_commutativity(&bad_ab, &bad_ba, &bad_ab, &base_ref, "injected mismatch");
        }))
        .is_err());
        // 3a. Forward apply failure in verify_inverse_restores + assert_inverse_restores panic.
        let stale_txn = Transaction::new(
            EditIntent::for_actor(actor, Nonce::new(9), Revision::new(99)),
            vec![],
        )
        .expect("txn");
        let fwd_err = verify_inverse_restores(&base_snap, &stale_txn).expect_err("stale fwd");
        assert!(
            fwd_err.contains("forward apply failed"),
            "unexpected error: {fwd_err}"
        );
        assert!(catch_unwind(AssertUnwindSafe(|| {
            assert_inverse_restores(&base_snap, &stale_txn, "injected forward apply failure");
        }))
        .is_err());

        // Set up a genuine multi-buffer transaction on base_snap (buf0 = "abcd", buf1 = "wxyz"):
        // buf0 replaces "a" -> "X", buf1 carries an empty no-op patch.
        let base_text0 = base_snap.catalog().get(buf0).expect("buf0");
        let fwd_patch0 = Patch::new(vec![Splice::new(
            ByteSpan::validate_in(base_text0, 0, 1).expect("0..1"),
            Text::from_str("X"),
        )])
        .expect("fwd patch");
        let fwd_txn = Transaction::new(
            EditIntent::for_actor(actor, Nonce::new(10), base_snap.revision()),
            vec![(buf0, fwd_patch0), (buf1, Patch::default())],
        )
        .expect("fwd txn");
        let fwd_res = fwd_txn.apply_with_inverse(&base_snap).expect("apply fwd");
        assert_eq!(
            verify_inverse_outcome(&base_snap, &fwd_txn, fwd_res.catalog(), fwd_res.inverse()),
            Ok(())
        );
        let after_text0 = fwd_res.catalog().get(buf0).expect("after buf0");

        // 3b. Corrupted inverse that fails text restoration ("X" -> "Q" instead of "a").
        let bad_restore_inv = Transaction::new(
            fwd_res.inverse().intent(),
            vec![
                (
                    buf0,
                    Patch::new(vec![Splice::new(
                        ByteSpan::validate_in(after_text0, 0, 1).expect("0..1"),
                        Text::from_str("Q"),
                    )])
                    .expect("bad restore patch"),
                ),
                (buf1, Patch::default()),
            ],
        )
        .expect("bad restore inv");
        let restore_err =
            verify_inverse_outcome(&base_snap, &fwd_txn, fwd_res.catalog(), &bad_restore_inv)
                .expect_err("corrupted inverse must fail restoration");
        assert!(
            restore_err.contains("inverse did not restore base catalog"),
            "unexpected error: {restore_err}"
        );

        // 3c. Truncated inverse patch list (restores text cleanly, but drops buf1's patch entry;
        // caught by the explicit patch-length guard before zipping).
        let truncated_inv = Transaction::new(
            fwd_res.inverse().intent(),
            vec![fwd_res.inverse().patches()[0].clone()],
        )
        .expect("truncated inv");
        let trunc_err =
            verify_inverse_outcome(&base_snap, &fwd_txn, fwd_res.catalog(), &truncated_inv)
                .expect_err("truncated inverse must fail patch-length check");
        assert!(
            trunc_err.contains("inverse patch count mismatch"),
            "unexpected error: {trunc_err}"
        );

        // 3d. Corrupted inverse that restores exact base text and preserves patch count,
        // yet breaks double-inversion involution ("Xb" -> "ab" instead of "X" -> "a").
        let non_involutive_inv = Transaction::new(
            fwd_res.inverse().intent(),
            vec![
                (
                    buf0,
                    Patch::new(vec![Splice::new(
                        ByteSpan::validate_in(after_text0, 0, 2).expect("0..2"),
                        Text::from_str("ab"),
                    )])
                    .expect("non-involutive patch"),
                ),
                (buf1, Patch::default()),
            ],
        )
        .expect("non-involutive inv");
        let invol_err =
            verify_inverse_outcome(&base_snap, &fwd_txn, fwd_res.catalog(), &non_involutive_inv)
                .expect_err("non-involutive inverse must fail involution check");
        assert!(
            invol_err.contains("double inverse patch not involutive"),
            "unexpected error: {invol_err}"
        );
    }

    // =========================================================================
    // Test 2: Empty / No-Op Identity Law Across EVERY Entry Point
    // =========================================================================

    #[test]
    fn empty_and_noop_identity_across_all_entry_points() {
        let actor = ActorId::new(1);
        let buf0 = BufferId::new(0);
        let buf1 = BufferId::new(1);

        for (case_idx, source) in corner_texts().into_iter().enumerate() {
            let ref_ws = RefWorkspace::new(&[(0, source), (1, "peer\r\n界")], &[2]);
            let mut ws = VersionedWorkspace::new(ref_ws.to_catalog());
            let snap = ws.capture();
            let text = snap.catalog().get(buf0).expect("buf0");
            let full_view = BufferView::full(snap.catalog(), buf0).expect("full view");
            let bounds = text.char_boundaries();
            let mid = bounds[bounds.len() / 2];

            // 1. Splice & Patch identity.
            let noop_insert = Splice::insert(mid, Text::empty());
            assert!(noop_insert.is_noop());
            assert_eq!(noop_insert.apply(text), Ok(text.clone()));
            let (next_t, inv_s) = noop_insert.apply_with_inverse(text).expect("apply");
            assert_eq!(next_t, *text);
            assert_eq!(inv_s.apply(&next_t), Ok(text.clone()));

            let full_same = Splice::new(ByteSpan::full_of(text), text.clone());
            assert_eq!(full_same.apply(text), Ok(text.clone()));

            let empty_patch = Patch::default();
            let (p_next, p_inv) = empty_patch.apply_with_inverse(text).expect("empty patch");
            assert_eq!(p_next, *text);
            assert_eq!(p_inv, Patch::default());

            // 2. command::plan no-ops (all 5 EditCommand variants).
            let intent = EditIntent::for_actor(actor, Nonce::new(1), snap.revision());
            let caret = Selection::new(snap.catalog(), &full_view, buf0, mid, mid).expect("caret");
            let full_sel = Selection::new(
                snap.catalog(),
                &full_view,
                buf0,
                bounds[0],
                *bounds.last().expect("end"),
            )
            .expect("full sel");
            let noop_commands = [
                EditCommand::Insert {
                    at: mid,
                    text: Text::empty(),
                },
                EditCommand::Delete {
                    span: ByteSpan::empty_at(mid),
                },
                EditCommand::Replace {
                    span: ByteSpan::full_of(text),
                    text: text.clone(),
                },
                EditCommand::ReplaceSelection {
                    selection: full_sel,
                    text: text.clone(),
                },
                EditCommand::ReplaceSelection {
                    selection: full_sel.flip(),
                    text: text.clone(),
                },
                EditCommand::DeleteSelection { selection: caret },
            ];
            for cmd in noop_commands {
                let txn = command::plan(intent, &full_view, &snap, cmd).expect("plan noop");
                let res = txn.apply_with_inverse(&snap).expect("apply noop");
                assert!(res.is_noop());
                assert_catalog_matches_ref(res.catalog(), &ref_ws, "command noop");
                assert_eq!(res.inverse().intent().base(), snap.revision());
                assert_inverse_restores(&snap, &txn, "command noop");
            }

            // 3. Transaction empty & multi-buffer identity replacement.
            let empty_txn = Transaction::new(intent, vec![]).expect("empty txn");
            let empty_res = empty_txn.apply_with_inverse(&snap).expect("apply empty");
            assert!(empty_res.is_noop());
            assert_eq!(empty_res.inverse().intent().base(), snap.revision());
            assert_catalog_matches_ref(empty_res.catalog(), &ref_ws, "empty txn");

            // 4. Sequencer no-op submission leaves revision, snapshot, journal, history untouched.
            let mut seq = Sequencer::new(ref_ws.to_catalog(), default_actors());
            let seq_before = (
                seq.snapshot(),
                seq.snapshots().clone(),
                seq.journal().clone(),
                seq.history().clone(),
            );
            let outcome = seq.submit(empty_txn.clone());
            assert!(outcome.is_noop());
            assert_eq!(outcome.resulting_revision(), Revision::ZERO);
            assert_state_unchanged(
                &seq_before,
                &(
                    seq.snapshot(),
                    seq.snapshots().clone(),
                    seq.journal().clone(),
                    seq.history().clone(),
                ),
                "sequencer noop",
            );

            // 5. GroupPlan ignores NoOp and Rejected outcomes cleanly.
            let mut groups = GroupPlan::new();
            let g_before = groups.clone();
            assert_eq!(groups.project(outcome, None), Ok(None));
            assert_state_unchanged(&g_before, &groups, "group plan noop");

            // 6. WorkspaceEdit::empty identity law.
            let we_empty = WorkspaceEdit::empty(actor, snap.revision());
            let we_id = WorkspaceEdit::new(
                actor,
                snap.revision(),
                vec![(buf0, Patch::new(vec![full_same]).expect("patch"))],
                &snap,
            )
            .expect("we identity");
            assert_eq!(we_empty.compose(&we_id), Ok(we_id.clone()));
            assert_eq!(we_id.compose(&we_empty), Ok(we_id.clone()));
            let we_res = we_empty
                .apply(Nonce::new(2), &snap)
                .expect("apply empty we");
            assert!(we_res.is_noop());
            assert_catalog_matches_ref(we_res.catalog(), &ref_ws, "workspace_edit empty");

            // 7. Restore no-op (empty targets and identical current text).
            assert_eq!(
                restore::restore(&snap, buf0, text.clone(), actor, Nonce::new(3)),
                Ok(RestoreOutcome::NoChange)
            );
            assert_eq!(
                restore::restore_many(&snap, [], actor, Nonce::new(3)),
                Ok(RestoreOutcome::NoChange)
            );
            assert_eq!(
                restore::restore_slice(
                    &snap,
                    &[(buf0, text.clone()), (buf1, Text::from_str("peer\r\n界"))],
                    actor,
                    Nonce::new(3),
                ),
                Ok(RestoreOutcome::NoChange)
            );

            // 8. Recorded commands: empty slice is full identity; no-op command commits revision with identical text.
            let ws_before = ws.capture();
            let empty_fold = recorded_commands::fold(&mut ws, &[]);
            assert_eq!(
                empty_fold,
                FoldResult {
                    edits: vec![],
                    failure: None,
                }
            );
            assert_state_unchanged(&ws_before, &ws.capture(), "recorded_commands empty fold");

            let noop_recorded = RecordedCommand {
                intent: EditIntent::for_actor(actor, Nonce::new(10), ws.revision()),
                view: full_view,
                command: RecordedAction::Edit(EditCommand::Insert {
                    at: mid,
                    text: Text::empty(),
                }),
            };
            let noop_fold = recorded_commands::fold(&mut ws, &[noop_recorded]);
            assert_eq!(noop_fold.failure, None);
            assert_eq!(noop_fold.edits.len(), 1);
            assert_eq!(ws.revision(), Revision::new(1));
            assert_catalog_matches_ref(ws.catalog(), &ref_ws, "recorded noop preserves text");

            // 9. Interop change groups: empty command list and self-cancelling change group are full identity (no revision advance).
            let mut ws_grp = VersionedWorkspace::new(ref_ws.to_catalog());
            let grp_snap = ws_grp.capture();
            let ctx = InteropContext {
                actor,
                buffer: buf0,
            };
            let policy = full_access_policy();
            let reg = default_actors();
            let empty_em = lower_emacs(&grp_snap, ctx, &[]).expect("empty emacs");
            assert!(empty_em.is_empty());
            assert_eq!(
                apply_requests(&mut ws_grp, &policy, &reg, &empty_em),
                Ok(None)
            );
            assert_state_unchanged(&grp_snap, &ws_grp.capture(), "empty emacs commands");

            let empty_nv = lower_neovim(&grp_snap, ctx, &[]).expect("empty neovim");
            assert!(empty_nv.is_empty());
            assert_eq!(
                apply_requests(&mut ws_grp, &policy, &reg, &empty_nv),
                Ok(None)
            );
            assert_state_unchanged(&grp_snap, &ws_grp.capture(), "empty neovim commands");

            let empty_ag = lower_agent(&grp_snap, ctx, &[]).expect("empty agent");
            assert!(empty_ag.is_empty());
            assert_eq!(
                apply_requests(&mut ws_grp, &policy, &reg, &empty_ag),
                Ok(None)
            );
            assert_state_unchanged(&grp_snap, &ws_grp.capture(), "empty agent commands");

            let empty_batch = decode_batch(&snap, intent, &[]).expect("empty decode_batch");
            assert!(empty_batch.patches().is_empty());
            let empty_batch_res = empty_batch
                .apply_with_inverse(&snap)
                .expect("apply empty batch");
            assert!(empty_batch_res.is_noop());
            assert_eq!(empty_batch_res.inverse().intent().base(), snap.revision());
            assert_catalog_matches_ref(empty_batch_res.catalog(), &ref_ws, "decode_batch empty");
            assert_inverse_restores(&snap, &empty_batch, "decode_batch empty");

            // Self-cancelling change group: insert "界" at start, then delete it inside one group.
            let cancel_agent = [
                AgentCommand::BeginBatch,
                AgentCommand::Replace {
                    start: AgentPosition::Byte(BytePos::new(0)),
                    end: AgentPosition::Byte(BytePos::new(0)),
                    text: Text::from_str("界"),
                },
                AgentCommand::Replace {
                    start: AgentPosition::Byte(BytePos::new(0)),
                    end: AgentPosition::Byte(BytePos::new("界".len())),
                    text: Text::empty(),
                },
                AgentCommand::EndBatch,
            ];
            let reqs = lower_agent(&grp_snap, ctx, &cancel_agent).expect("cancel batch");
            assert_eq!(apply_requests(&mut ws_grp, &policy, &reg, &reqs), Ok(None));
            assert_state_unchanged(
                &grp_snap,
                &ws_grp.capture(),
                "self-cancelling change group identity",
            );

            // 10. Multi-cursor empty command list and non-empty no-op commands.
            let mc_empty = multi_cursor::plan(intent, &full_view, &snap, vec![]).expect("empty mc");
            let mc_res = mc_empty.apply_with_inverse(&snap).expect("apply mc empty");
            assert!(mc_res.is_noop());
            assert_catalog_matches_ref(mc_res.catalog(), &ref_ws, "multi_cursor empty");

            let mut mc_noop_sets: Vec<Vec<EditCommand>> = vec![
                vec![EditCommand::Insert {
                    at: mid,
                    text: Text::empty(),
                }],
                vec![
                    EditCommand::Delete {
                        span: ByteSpan::empty_at(mid),
                    },
                    EditCommand::DeleteSelection { selection: caret },
                ],
                vec![EditCommand::ReplaceSelection {
                    selection: full_sel.flip(),
                    text: text.clone(),
                }],
            ];
            if bounds.len() >= 2 {
                let first_b = bounds[0];
                let last_b = *bounds.last().expect("end");
                mc_noop_sets.push(vec![
                    EditCommand::Insert {
                        at: first_b,
                        text: Text::empty(),
                    },
                    EditCommand::Delete {
                        span: ByteSpan::empty_at(last_b),
                    },
                ]);
            }
            if bounds.len() >= 4 {
                let span_a = ByteSpan::new(bounds[0], bounds[1]).expect("span a");
                let span_b = ByteSpan::new(bounds[2], bounds[3]).expect("span b");
                let sel_b = Selection::new(snap.catalog(), &full_view, buf0, bounds[3], bounds[2])
                    .expect("sel b");
                mc_noop_sets.push(vec![
                    EditCommand::Replace {
                        span: span_a,
                        text: Text::from_str(span_a.slice_in(text).expect("slice a")),
                    },
                    EditCommand::ReplaceSelection {
                        selection: sel_b,
                        text: Text::from_str(span_b.slice_in(text).expect("slice b")),
                    },
                ]);
            }
            for cmds in mc_noop_sets {
                let mc_txn =
                    multi_cursor::plan(intent, &full_view, &snap, cmds).expect("noop mc plan");
                let res = mc_txn.apply_with_inverse(&snap).expect("apply noop mc");
                assert!(res.is_noop());
                assert_eq!(res.inverse().intent().base(), snap.revision());
                assert_catalog_matches_ref(res.catalog(), &ref_ws, "multi_cursor noop");
                assert_inverse_restores(&snap, &mc_txn, "multi_cursor noop");
            }

            // 11. Rectangle no-op (empty exclusive column Delete / Insert("")).
            let last_line = LineIndex::from_text(text).line_count() - 1;
            let empty_rect = Rectangle {
                first: VisualPosition { line: 0, column: 0 },
                second: VisualPosition {
                    line: last_line,
                    column: 0,
                },
                endpoint: Endpoint::Exclusive,
                tab_width: 4,
            };
            for op in [
                RectangleOperation::Delete,
                RectangleOperation::Uniform(""),
                RectangleOperation::Insert(""),
            ] {
                let r_txn = rectangle_edit::plan_transaction(
                    &snap,
                    buf0,
                    empty_rect,
                    ShortLinePolicy::Clamp,
                    op,
                    intent,
                )
                .expect("noop rect");
                let r_res = r_txn.apply_with_inverse(&snap).expect("apply noop rect");
                assert!(r_res.is_noop());
                assert_catalog_matches_ref(r_res.catalog(), &ref_ws, "rectangle noop");
            }

            // 12. Indent region empty range & idempotent re-application.
            let ind_policy = IndentPolicy {
                column: 4,
                tab_width: 4,
                style: if case_idx % 2 == 0 {
                    IndentStyle::Spaces
                } else {
                    IndentStyle::TabsAndSpaces
                },
            };
            let empty_ind = indent_region::plan(text, 0..0, ind_policy).expect("empty indent");
            assert!(empty_ind.splices().is_empty());
            assert_eq!(empty_ind.apply(text), Ok(text.clone()));
            let formatted = indent_region::plan(text, 0..last_line + 1, ind_policy)
                .expect("format indent")
                .apply(text)
                .expect("apply indent");
            let second_pass =
                indent_region::plan(&formatted, 0..last_line + 1, ind_policy).expect("second");
            assert!(second_pass.splices().is_empty());
            assert_eq!(second_pass.apply(&formatted), Ok(formatted));

            // 13. Replace-all no-match and identity replacement.
            let ra_miss = replace_all::plan_transaction(
                &snap,
                full_view,
                full_view.visible(),
                "NEVER_PRESENT_TOKEN_XYZ",
                "replacement",
                intent,
            )
            .expect("replace miss");
            assert!(ra_miss.apply_with_inverse(&snap).expect("apply").is_noop());
            let ra_same = replace_all::plan_transaction(
                &snap,
                full_view,
                full_view.visible(),
                "a",
                "a",
                intent,
            )
            .expect("replace same");
            assert!(ra_same.apply_with_inverse(&snap).expect("apply").is_noop());

            // 14. Register-backed no-ops (empty cut, caret-only cut, empty paste).
            let mut reg_seq = Sequencer::new(ref_ws.to_catalog(), default_actors());
            let mut cut_receipts = CutReceipts::default();
            let reg_snap = reg_seq.snapshot();
            let reg_view = BufferView::full(reg_snap.catalog(), buf0).expect("view");
            let reg_intent = EditIntent::for_actor(actor, Nonce::new(20), reg_snap.revision());
            let reg_name = RegisterName::new("r").expect("reg name");
            let empty_cut = register_edit::cut(
                reg_intent,
                &reg_view,
                &reg_snap,
                &[caret],
                reg_name.clone(),
                RegisterKind::Charwise,
            )
            .expect("empty cut");
            assert!(empty_cut.entry().text.is_empty());
            assert!(empty_cut
                .transaction()
                .apply_with_inverse(&reg_snap)
                .expect("apply")
                .is_noop());
            let cut_outcome =
                register_edit::submit_cut(&mut cut_receipts, &mut reg_seq, &empty_cut)
                    .expect("submit empty cut");
            assert!(cut_outcome.is_noop());
            assert_eq!(reg_seq.revision(), Revision::ZERO);

            // Copy empty selection then paste at caret -> no-op.
            cut_receipts
                .copy(
                    reg_intent,
                    &reg_view,
                    &reg_snap,
                    &[caret],
                    CopyTarget {
                        name: reg_name.clone(),
                        kind: RegisterKind::Charwise,
                    },
                )
                .expect("copy empty");
            let empty_paste = register_edit::paste(
                cut_receipts.registers(),
                reg_intent,
                &reg_view,
                &reg_snap,
                &[caret],
                &reg_name,
            )
            .expect("paste empty");
            assert!(empty_paste
                .apply_with_inverse(&reg_snap)
                .expect("apply")
                .is_noop());
        }
    }

    // =========================================================================
    // Test 3: Disjoint Commutativity (Same-Buffer, Multi-Buffer, Session, Specialized)
    // =========================================================================

    #[test]
    fn disjoint_commutativity_single_and_multi_buffer_and_session() {
        let a1 = ActorId::new(1);
        let a2 = ActorId::new(2);
        let buf0 = BufferId::new(0);
        let buf1 = BufferId::new(1);
        let overlay_keys = [
            AnnotationKey {
                buffer: buf0,
                namespace: NamespaceId::new(1),
                id: AnnotationId::new(1),
            },
            AnnotationKey {
                buffer: buf1,
                namespace: NamespaceId::new(1),
                id: AnnotationId::new(1),
            },
        ];

        // 3a. Same-buffer strictly disjoint edits commute across rebase, Sequencer, WorkspaceEdit,
        // multi_cursor, and WorkspaceSession mapped state!
        let single_cases = [
            ("abcdef", (0, 1, "XY"), (4, 5, "Z"), 2),
            ("a界b😀c", (0, 1, "新"), (5, 9, ""), 4),
            ("e\u{0301}界😀z", (0, 3, ""), (6, 10, "ok"), 3),
            ("line1\r\nline2\r\nline3", (0, 5, "L1"), (14, 19, "L3"), 7),
            ("a\rb\rc", (0, 1, "AA"), (4, 5, "CC"), 2),
            ("x界y", (0, 0, "PRE"), (4, 4, "SUF"), 1),
        ];
        for (source, (as0, ae0, arepl), (bs0, be0, brepl), cursor_pos) in single_cases {
            let ref_base = RefWorkspace::new(&[(0, source), (1, "untouched")], &[2]);
            let plan_a = RefPlan {
                buffers: BTreeMap::from([(
                    0,
                    vec![RefEdit {
                        start: as0,
                        end: ae0,
                        text: arepl.into(),
                    }],
                )]),
            };
            let plan_b = RefPlan {
                buffers: BTreeMap::from([(
                    0,
                    vec![RefEdit {
                        start: bs0,
                        end: be0,
                        text: brepl.into(),
                    }],
                )]),
            };
            // Reference model: A then B', B then A', and simultaneous A U B.
            let mut ref_ab = ref_base.clone();
            ref_ab.commit_plan((1, 1), &plan_a, false).expect("ref A");
            let ref_b_prime = ref_transform_disjoint_plan(&plan_b, &plan_a).expect("transform B");
            ref_ab
                .commit_plan((2, 1), &ref_b_prime, false)
                .expect("ref B'");

            let mut ref_ba = ref_base.clone();
            ref_ba.commit_plan((2, 1), &plan_b, false).expect("ref B");
            let ref_a_prime = ref_transform_disjoint_plan(&plan_a, &plan_b).expect("transform A");
            ref_ba
                .commit_plan((1, 1), &ref_a_prime, false)
                .expect("ref A'");

            let mut ref_sim = ref_base.clone();
            let merged_plan = ref_compose_plans(&plan_a, &plan_b).expect("compose");
            ref_sim
                .commit_plan((1, 9), &merged_plan, false)
                .expect("ref sim");
            assert_eq!(ref_ab.live, ref_ba.live);
            assert_eq!(ref_ab.live, ref_sim.live);

            // Production order 1: Sequencer + WorkspaceSession applying A then stale B (rebased).
            let run_order = |first_actor: ActorId,
                             first_plan: &RefPlan,
                             second_actor: ActorId,
                             second_plan: &RefPlan|
             -> (BufferCatalog, SessionObservation) {
                let mut seq = Sequencer::new(ref_base.to_catalog(), default_actors());
                let mut log = DeliveryLog::new(seq.snapshot());
                let mut session =
                    WorkspaceSession::new(seq.snapshot(), default_actors(), full_access_policy());
                populate_session_for_actor(&mut session, a1, buf0, cursor_pos, cursor_pos);
                populate_session_for_actor(&mut session, a2, buf0, 0, source.len());

                let snap0 = seq.snapshot();
                let t1 = txn_from_ref(&snap0, first_actor, 1, first_plan);
                let t2 = txn_from_ref(&snap0, second_actor, 1, second_plan);

                let r1 = seq
                    .submit(t1.clone())
                    .committed_receipt()
                    .expect("commit 1");
                log.record_workspace(&seq, r1).expect("log 1");
                session
                    .advance(log.events().last().expect("ev 1"))
                    .expect("adv 1");

                // Also verify rebase_over_step matches our independent reference coordinate shift!
                let rebased = rebase_over_step(
                    &t2,
                    RebaseStep {
                        before: &snap0,
                        committed: &t1,
                        after: &seq.snapshot(),
                    },
                )
                .expect("rebase");
                let expected_rebased =
                    ref_transform_disjoint_plan(second_plan, first_plan).expect("ref shift");
                assert_eq!(ref_plan_from_txn(&rebased), expected_rebased);

                let r2 = seq.submit(t2).committed_receipt().expect("commit 2");
                log.record_workspace(&seq, r2).expect("log 2");
                session
                    .advance(log.events().last().expect("ev 2"))
                    .expect("adv 2");

                let obs = capture_session_observation(
                    &session,
                    &[a1, a2],
                    &[buf0, buf1],
                    &["bm"],
                    &overlay_keys,
                    &["a"],
                );
                (seq.snapshot().catalog().clone(), obs)
            };

            let (cat_ab, obs_ab) = run_order(a1, &plan_a, a2, &plan_b);
            let (cat_ba, obs_ba) = run_order(a2, &plan_b, a1, &plan_a);

            // Production simultaneous via WorkspaceEdit::compose and multi_cursor::plan.
            let snap0 = VersionedWorkspace::new(ref_base.to_catalog()).capture();
            let text0 = snap0.catalog().get(buf0).expect("buf0");
            let view0 = BufferView::full(snap0.catalog(), buf0).expect("view0");
            let we_a = WorkspaceEdit::new(
                a1,
                snap0.revision(),
                vec![(buf0, patch_from_ref(text0, &plan_a.buffers[&0]))],
                &snap0,
            )
            .expect("we A");
            let we_b = WorkspaceEdit::new(
                a1,
                snap0.revision(),
                vec![(buf0, patch_from_ref(text0, &plan_b.buffers[&0]))],
                &snap0,
            )
            .expect("we B");
            assert_eq!(we_a.compose(&we_b), we_b.compose(&we_a));
            let sim_cat = we_a
                .compose(&we_b)
                .expect("compose")
                .apply(Nonce::new(1), &snap0)
                .expect("apply")
                .catalog()
                .clone();

            let mc_cmds = vec![
                EditCommand::Replace {
                    span: ByteSpan::validate_in(text0, as0, ae0).expect("span a"),
                    text: Text::from_str(arepl),
                },
                EditCommand::Replace {
                    span: ByteSpan::validate_in(text0, bs0, be0).expect("span b"),
                    text: Text::from_str(brepl),
                },
            ];
            let mc_txn = multi_cursor::plan(
                EditIntent::for_actor(a1, Nonce::new(1), snap0.revision()),
                &view0,
                &snap0,
                mc_cmds,
            )
            .expect("mc plan");
            let mc_cat = mc_txn
                .apply_with_inverse(&snap0)
                .expect("mc apply")
                .catalog()
                .clone();
            assert_eq!(sim_cat, mc_cat);

            assert_commutativity(&cat_ab, &cat_ba, &sim_cat, &ref_ab, "same-buffer disjoint");
            assert_eq!(obs_ab, obs_ba, "WorkspaceSession mapped state must commute");

            // Check actor 1's projected caret offset matches independent reference transport!
            let expected_cursor = ref_transport_offset(
                ref_transport_offset(cursor_pos, &plan_a.buffers[&0], true),
                &ref_b_prime.buffers[&0],
                true,
            );
            assert_eq!(
                obs_ab.actors[&a1].selections[&buf0]
                    .expect("sel")
                    .head()
                    .as_usize(),
                expected_cursor
            );
        }

        // 3b. Multi-buffer disjoint plans commute across WorkspaceEdit, Transaction, Sequencer, Session.
        {
            let ref_base = RefWorkspace::new(&[(0, "αβγ"), (1, "😀界\r\nend")], &[2]);
            let plan_a = RefPlan {
                buffers: BTreeMap::from([(
                    0,
                    vec![RefEdit {
                        start: 0,
                        end: "α".len(),
                        text: "A".into(),
                    }],
                )]),
            };
            let plan_b = RefPlan {
                buffers: BTreeMap::from([(
                    1,
                    vec![RefEdit {
                        start: "😀".len(),
                        end: "😀界".len(),
                        text: "WORLD".into(),
                    }],
                )]),
            };
            let mut ref_expected = ref_base.clone();
            let merged = ref_compose_plans(&plan_a, &plan_b).expect("compose");
            ref_expected
                .commit_plan((1, 1), &merged, false)
                .expect("commit");

            let run_multi = |first: &RefPlan,
                             second: &RefPlan|
             -> (BufferCatalog, SessionObservation) {
                let mut seq = Sequencer::new(ref_base.to_catalog(), default_actors());
                let mut log = DeliveryLog::new(seq.snapshot());
                let mut session =
                    WorkspaceSession::new(seq.snapshot(), default_actors(), full_access_policy());
                populate_session_for_actor(&mut session, a1, buf0, 0, "αβ".len());
                populate_session_for_actor(&mut session, a1, buf1, "😀".len(), "😀界".len());

                let s0 = seq.snapshot();
                let t1 = txn_from_ref(&s0, a1, 1, first);
                let t2 = txn_from_ref(&s0, a2, 1, second);
                let r1 = seq.submit(t1).committed_receipt().expect("r1");
                log.record_workspace(&seq, r1).expect("log1");
                session
                    .advance(log.events().last().expect("e1"))
                    .expect("a1");
                let r2 = seq.submit(t2).committed_receipt().expect("r2");
                log.record_workspace(&seq, r2).expect("log2");
                session
                    .advance(log.events().last().expect("e2"))
                    .expect("a2");
                let obs = capture_session_observation(
                    &session,
                    &[a1],
                    &[buf0, buf1],
                    &["bm"],
                    &overlay_keys,
                    &[],
                );
                (seq.snapshot().catalog().clone(), obs)
            };

            let (cat_ab, obs_ab) = run_multi(&plan_a, &plan_b);
            let (cat_ba, obs_ba) = run_multi(&plan_b, &plan_a);
            let snap0 = VersionedWorkspace::new(ref_base.to_catalog()).capture();
            let we_a = WorkspaceEdit::new(
                a1,
                snap0.revision(),
                vec![(
                    buf0,
                    patch_from_ref(snap0.catalog().get(buf0).expect("0"), &plan_a.buffers[&0]),
                )],
                &snap0,
            )
            .expect("we a");
            let we_b = WorkspaceEdit::new(
                a1,
                snap0.revision(),
                vec![(
                    buf1,
                    patch_from_ref(snap0.catalog().get(buf1).expect("1"), &plan_b.buffers[&1]),
                )],
                &snap0,
            )
            .expect("we b");
            assert_eq!(we_a.compose(&we_b), we_b.compose(&we_a));
            let sim_cat = we_a
                .compose(&we_b)
                .expect("compose")
                .apply(Nonce::new(1), &snap0)
                .expect("apply")
                .catalog()
                .clone();
            assert_commutativity(
                &cat_ab,
                &cat_ba,
                &sim_cat,
                &ref_expected,
                "multi-buffer disjoint",
            );
            assert_eq!(obs_ab, obs_ba);
        }

        // 3c. Specialized entry points commute on disjoint regions: indent, rectangle, replace_all.
        {
            let source = "  aa  \r\n\tbb界\n   cc😀\r\n  dd  ";
            let text = Text::from_str(source);

            // Disjoint indent regions: lines 0..2 and lines 2..4.
            let p1 = IndentPolicy {
                column: 4,
                tab_width: 4,
                style: IndentStyle::Spaces,
            };
            let p2 = IndentPolicy {
                column: 6,
                tab_width: 4,
                style: IndentStyle::TabsAndSpaces,
            };
            let ind_a = indent_region::plan(&text, 0..2, p1).expect("ind A");
            let ind_b = indent_region::plan(&text, 2..4, p2).expect("ind B");
            let after_a = ind_a.apply(&text).expect("after A");
            let ab_text = indent_region::plan(&after_a, 2..4, p2)
                .expect("plan B on after A")
                .apply(&after_a)
                .expect("apply B");
            let after_b = ind_b.apply(&text).expect("after B");
            let ba_text = indent_region::plan(&after_b, 0..2, p1)
                .expect("plan A on after B")
                .apply(&after_b)
                .expect("apply A");
            assert_eq!(ab_text, ba_text);

            let ref_ind_a = ref_indent_region_edits(source, 0..2, p1).expect("ref ind A");
            let ref_ind_b = ref_indent_region_edits(source, 2..4, p2).expect("ref ind B");
            let mut merged_ind = ref_ind_a.clone();
            merged_ind.extend(ref_ind_b.clone());
            let (ref_ind_out, _) = ref_apply_single(source, &merged_ind).expect("ref ind");
            assert_eq!(ab_text.as_str(), ref_ind_out);

            // Simultaneous via WorkspaceEdit::compose:
            let snap = VersionedWorkspace::new(catalog_for_single(buf0, &text)).capture();
            let we_a = WorkspaceEdit::new(a1, snap.revision(), vec![(buf0, ind_a)], &snap)
                .expect("we ind A");
            let we_b = WorkspaceEdit::new(a1, snap.revision(), vec![(buf0, ind_b)], &snap)
                .expect("we ind B");
            assert_eq!(we_a.compose(&we_b), we_b.compose(&we_a));
            assert_eq!(
                we_a.compose(&we_b)
                    .expect("compose")
                    .apply(Nonce::new(1), &snap)
                    .expect("apply")
                    .catalog()
                    .get(buf0)
                    .expect("0"),
                &ab_text
            );

            // Disjoint rectangle edits on disjoint row bands: rows 0..=1 and rows 2..=3.
            let rect_a = Rectangle {
                first: VisualPosition { line: 0, column: 1 },
                second: VisualPosition { line: 1, column: 3 },
                endpoint: Endpoint::Exclusive,
                tab_width: 4,
            };
            let rect_b = Rectangle {
                first: VisualPosition { line: 2, column: 2 },
                second: VisualPosition { line: 3, column: 4 },
                endpoint: Endpoint::Inclusive,
                tab_width: 4,
            };
            let rp_a = rectangle_edit::plan_patch(
                &text,
                rect_a,
                ShortLinePolicy::Clamp,
                RectangleOperation::Uniform("X"),
            )
            .expect("rp A");
            let rp_b = rectangle_edit::plan_patch(
                &text,
                rect_b,
                ShortLinePolicy::Clamp,
                RectangleOperation::Insert("!"),
            )
            .expect("rp B");
            let r_after_a = rp_a.apply(&text).expect("apply rect A");
            let r_ab = rectangle_edit::plan_patch(
                &r_after_a,
                rect_b,
                ShortLinePolicy::Clamp,
                RectangleOperation::Insert("!"),
            )
            .expect("rect B on after A")
            .apply(&r_after_a)
            .expect("apply rect B");
            let r_after_b = rp_b.apply(&text).expect("apply rect B");
            let r_ba = rectangle_edit::plan_patch(
                &r_after_b,
                rect_a,
                ShortLinePolicy::Clamp,
                RectangleOperation::Uniform("X"),
            )
            .expect("rect A on after B")
            .apply(&r_after_b)
            .expect("apply rect A");
            assert_eq!(r_ab, r_ba);
            let mut ref_rect_merged = ref_rectangle_edits(
                source,
                RefRectSpec {
                    first_line: 0,
                    first_col: 1,
                    second_line: 1,
                    second_col: 3,
                    inclusive: false,
                    tab_width: 4,
                    clamp_short: true,
                    op: RectangleOperation::Uniform("X"),
                },
            )
            .expect("ref rect A");
            ref_rect_merged.extend(
                ref_rectangle_edits(
                    source,
                    RefRectSpec {
                        first_line: 2,
                        first_col: 2,
                        second_line: 3,
                        second_col: 4,
                        inclusive: true,
                        tab_width: 4,
                        clamp_short: true,
                        op: RectangleOperation::Insert("!"),
                    },
                )
                .expect("ref rect B"),
            );
            let (ref_rect_out, _) =
                ref_apply_single(source, &ref_rect_merged).expect("ref rect apply");
            assert_eq!(r_ab.as_str(), ref_rect_out);
            let we_ra =
                WorkspaceEdit::new(a1, snap.revision(), vec![(buf0, rp_a)], &snap).expect("we ra");
            let we_rb =
                WorkspaceEdit::new(a1, snap.revision(), vec![(buf0, rp_b)], &snap).expect("we rb");
            assert_eq!(we_ra.compose(&we_rb), we_rb.compose(&we_ra));
            assert_eq!(
                we_ra
                    .compose(&we_rb)
                    .expect("compose")
                    .apply(Nonce::new(2), &snap)
                    .expect("apply")
                    .catalog()
                    .get(buf0)
                    .expect("0"),
                &r_ab
            );

            // Disjoint replace_all spans on the same buffer.
            let ra_src = "aa_sep_aa";
            let ra_text = Text::from_str(ra_src);
            let ra_snap = VersionedWorkspace::new(catalog_for_single(buf0, &ra_text)).capture();
            let ra_view = BufferView::full(ra_snap.catalog(), buf0).expect("view");
            let left_span = ByteSpan::validate_in(&ra_text, 0, 2).expect("left");
            let right_span = ByteSpan::validate_in(&ra_text, 7, 9).expect("right");
            let pat_a =
                replace_all::plan_patch(&ra_snap, ra_view, left_span, "a", "界").expect("ra left");
            let pat_b = replace_all::plan_patch(&ra_snap, ra_view, right_span, "a", "😀")
                .expect("ra right");
            let we_l = WorkspaceEdit::new(a1, ra_snap.revision(), vec![(buf0, pat_a)], &ra_snap)
                .expect("we l");
            let we_r = WorkspaceEdit::new(a1, ra_snap.revision(), vec![(buf0, pat_b)], &ra_snap)
                .expect("we r");
            assert_eq!(we_l.compose(&we_r), we_r.compose(&we_l));
            let composed_ra = we_l
                .compose(&we_r)
                .expect("compose")
                .apply(Nonce::new(1), &ra_snap)
                .expect("apply");
            assert_eq!(
                composed_ra.catalog().get(buf0).expect("0").as_str(),
                "界界_sep_😀😀"
            );
        }
    }

    // =========================================================================
    // Test 4: Inverse Plan Restoration, Involution, History, Undo/Redo, Restore
    // =========================================================================

    #[test]
    fn inverse_restoration_involutive_and_history_undo_redo_restore() {
        let a1 = ActorId::new(1);
        let a2 = ActorId::new(2);
        let buf0 = BufferId::new(0);
        let buf1 = BufferId::new(1);

        // 4a. Multi-step EditHistory forward replay & reverse inverse restoration across all slices,
        // including returning to an earlier text without collapsing history records.
        {
            let mut ref_ws = RefWorkspace::new(&[(0, "a界b😀\r\nline"), (1, "012345")], &[2]);
            let mut ws = VersionedWorkspace::new(ref_ws.to_catalog());
            let mut history = EditHistory::new();
            let mut catalogs = vec![ws.catalog().clone()];

            let steps: Vec<RefPlan> = vec![
                // Step 1: multi-buffer replacement with UTF-8.
                RefPlan {
                    buffers: BTreeMap::from([
                        (
                            0,
                            vec![RefEdit {
                                start: 1,
                                end: 4,
                                text: "NEW".into(),
                            }],
                        ),
                        (
                            1,
                            vec![RefEdit {
                                start: 2,
                                end: 4,
                                text: "界😀".into(),
                            }],
                        ),
                    ]),
                },
                // Step 2: content-preserving no-op recorded explicitly.
                RefPlan {
                    buffers: BTreeMap::from([(
                        0,
                        vec![RefEdit {
                            start: 0,
                            end: 1,
                            text: "a".into(),
                        }],
                    )]),
                },
                // Step 3: return buffer 0 to original text.
                RefPlan {
                    buffers: BTreeMap::from([(
                        0,
                        vec![RefEdit {
                            start: 1,
                            end: 4,
                            text: "界".into(),
                        }],
                    )]),
                },
                // Step 4: deletion on buffer 1.
                RefPlan {
                    buffers: BTreeMap::from([(
                        1,
                        vec![RefEdit {
                            start: 0,
                            end: 2,
                            text: String::new(),
                        }],
                    )]),
                },
            ];

            for (i, plan) in steps.iter().enumerate() {
                let before = ws.capture();
                let txn = txn_from_ref(&before, a1, i as u64 + 1, plan);
                assert_inverse_restores(&before, &txn, "history step");

                let eval = txn.apply_with_inverse(&before).expect("eval");
                let updates: Vec<_> = txn
                    .patches()
                    .iter()
                    .map(|(id, _)| (*id, eval.catalog().get(*id).expect("buf").clone()))
                    .collect();
                ws.update_batch(updates).expect("commit");
                history
                    .record(&before, ws.revision(), ws.catalog(), txn.clone())
                    .expect("record history");
                ref_ws
                    .commit_plan((1, i as u64 + 1), plan, true)
                    .expect("ref commit");
                assert_catalog_matches_ref(ws.catalog(), &ref_ws, "history step");
                catalogs.push(ws.catalog().clone());
            }
            assert_eq!(history.entries().len(), steps.len());

            // Every contiguous slice [start..end] replays forward and restores backward!
            for start in 0..=steps.len() {
                for end in start..=steps.len() {
                    let mut fwd = catalogs[start].clone();
                    for entry in &history.entries()[start..end] {
                        fwd = entry.forward().apply_to_catalog(&fwd).expect("fwd replay");
                    }
                    assert_eq!(fwd, catalogs[end]);
                    for entry in history.entries()[start..end].iter().rev() {
                        fwd = entry.inverse().apply_to_catalog(&fwd).expect("inv replay");
                    }
                    assert_eq!(fwd, catalogs[start]);
                }
            }

            // 4b. Restore API restores any historical catalog and its inverse returns to tip.
            let tip = ws.capture();
            for (rev_idx, target_cat) in catalogs.iter().enumerate() {
                let targets = [
                    (buf0, target_cat.get(buf0).expect("0").clone()),
                    (buf1, target_cat.get(buf1).expect("1").clone()),
                ];
                let outcome =
                    restore::restore_slice(&tip, &targets, a1, Nonce::new(100 + rev_idx as u64))
                        .expect("restore_slice");
                match outcome {
                    RestoreOutcome::NoChange => {
                        assert_eq!(tip.catalog(), target_cat);
                    }
                    RestoreOutcome::Transaction(r_txn) => {
                        assert_inverse_restores(&tip, &r_txn, "restore transaction");
                        let eval = r_txn.apply_with_inverse(&tip).expect("apply restore");
                        assert_eq!(eval.catalog(), target_cat);
                    }
                }
            }
        }

        // 4c. SelectiveUndo + Redo + interop::undo across multi-buffer edits & disjoint intervening commits.
        {
            let base_ref = RefWorkspace::new(&[(0, "0123456789界"), (1, "abcdef😀")], &[2]);
            let seq = Sequencer::new(base_ref.to_catalog(), default_actors());
            let mut engine = Redo::new(SelectiveUndo::new(seq));

            let plan_a = RefPlan {
                buffers: BTreeMap::from([
                    (
                        0,
                        vec![RefEdit {
                            start: 6,
                            end: 8,
                            text: "界界".into(),
                        }],
                    ),
                    (
                        1,
                        vec![RefEdit {
                            start: 4,
                            end: 6,
                            text: "XY".into(),
                        }],
                    ),
                ]),
            };
            let plan_b = RefPlan {
                buffers: BTreeMap::from([
                    (
                        0,
                        vec![RefEdit {
                            start: 0,
                            end: 2,
                            text: "PRE".into(),
                        }],
                    ),
                    (
                        1,
                        vec![RefEdit {
                            start: 0,
                            end: 1,
                            text: "Q".into(),
                        }],
                    ),
                ]),
            };

            let t_a = txn_from_ref(&engine.sequencer().snapshot(), a1, 1, &plan_a);
            assert!(engine.submit(t_a).is_committed());
            let after_a_snap = engine.sequencer().snapshot();

            let t_b = txn_from_ref(&after_a_snap, a2, 1, &plan_b);
            assert!(engine.submit(t_b).is_committed());

            // Reference: applying B alone to base should match undo(A) after A+B!
            let mut ref_b_only = base_ref.clone();
            ref_b_only
                .commit_plan((2, 1), &plan_b, false)
                .expect("B only");

            // Reference: applying A+B should match redo(A)!
            let mut ref_ab = base_ref.clone();
            let merged_ab = ref_compose_plans(&plan_a, &plan_b).expect("merge");
            ref_ab.commit_plan((1, 1), &merged_ab, false).expect("A+B");

            let target_id = TxnId::new(a1, Nonce::new(1));
            let undo_id = TxnId::new(a1, Nonce::new(2));
            let redo_id = TxnId::new(a1, Nonce::new(3));
            let undo2_id = TxnId::new(a1, Nonce::new(4));

            // Undo via interop::undo (Emacs frontend).
            let cands = [target_id];
            interop_undo::submit(
                &mut engine,
                a1,
                undo_id,
                FrontendCommand::EmacsUndoOnly(UndoTarget::Resolved(&cands)),
            )
            .expect("undo A");
            assert_catalog_matches_ref(
                engine.sequencer().snapshot().catalog(),
                &ref_b_only,
                "selective undo over disjoint intervening edit",
            );

            // Redo via interop::undo (Neovim frontend).
            let r_receipt = interop_undo::submit(
                &mut engine,
                a1,
                redo_id,
                FrontendCommand::NeovimRedo(UndoTarget::Explicit(target_id)),
            )
            .expect("redo A");
            assert_catalog_matches_ref(
                engine.sequencer().snapshot().catalog(),
                &ref_ab,
                "redo over disjoint intervening edit",
            );

            // Redo retry idempotence via Agent frontend.
            let retry_receipt = interop_undo::submit(
                &mut engine,
                a1,
                redo_id,
                FrontendCommand::AgentRedo(target_id),
            )
            .expect("retry redo");
            assert_eq!(retry_receipt, r_receipt);

            // Second undo cycle returns again to B-only!
            interop_undo::submit(
                &mut engine,
                a1,
                undo2_id,
                FrontendCommand::AgentUndo(target_id),
            )
            .expect("second undo");
            assert_catalog_matches_ref(
                engine.sequencer().snapshot().catalog(),
                &ref_b_only,
                "second undo cycle",
            );
        }

        // 4d. Register cut followed by paste/yank at cut start restores text byte-for-byte.
        for source in ["a界b😀c", "e\u{0301}\r\nline2", "abcdef"] {
            let ref_ws = RefWorkspace::new(&[(0, source)], &[]);
            let mut seq = Sequencer::new(ref_ws.to_catalog(), default_actors());
            let mut receipts = CutReceipts::default();
            let snap0 = seq.snapshot();
            let text0 = snap0.catalog().get(buf0).expect("0");
            let view0 = BufferView::full(snap0.catalog(), buf0).expect("view0");
            let bounds = text0.char_boundaries();
            let sel = Selection::new(
                snap0.catalog(),
                &view0,
                buf0,
                bounds[1],
                bounds[bounds.len() - 1],
            )
            .expect("sel");
            let reg_name = RegisterName::new("cut").expect("name");
            let plan = register_edit::cut(
                EditIntent::for_actor(a1, Nonce::new(1), snap0.revision()),
                &view0,
                &snap0,
                &[sel],
                reg_name.clone(),
                RegisterKind::Charwise,
            )
            .expect("cut");
            assert_inverse_restores(&snap0, plan.transaction(), "register cut");
            let outcome =
                register_edit::submit_cut(&mut receipts, &mut seq, &plan).expect("submit");
            assert_eq!(receipts.apply(a1, &plan, outcome, &seq), Ok(true));
            assert_eq!(receipts.apply(a1, &plan, outcome, &seq), Ok(false));

            let snap1 = seq.snapshot();
            let view1 = BufferView::full(snap1.catalog(), buf0).expect("view1");
            let at = snap1
                .catalog()
                .get(buf0)
                .expect("0")
                .validate_offset(bounds[1].as_usize())
                .expect("at");
            let caret = Selection::new(snap1.catalog(), &view1, buf0, at, at).expect("caret");
            let p_intent = EditIntent::for_actor(a1, Nonce::new(2), snap1.revision());
            let pasted = register_edit::paste(
                receipts.registers(),
                p_intent,
                &view1,
                &snap1,
                &[caret],
                &reg_name,
            )
            .expect("paste");
            let yanked = register_edit::yank(receipts.rings(), p_intent, &view1, &snap1, &[caret])
                .expect("yank");
            assert_eq!(pasted, yanked);
            let after_paste = pasted.apply_with_inverse(&snap1).expect("apply paste");
            assert_eq!(after_paste.catalog().get(buf0).expect("0").as_str(), source);
        }
    }

    // =========================================================================
    // Test 5: Failure Atomicity Across ALL Rejection Classes & Entry Points
    // =========================================================================

    #[test]
    fn failure_atomicity_all_rejection_classes_across_all_entry_points() {
        let a1 = ActorId::new(1);
        let a2 = ActorId::new(2);
        let unknown_actor = ActorId::new(99);
        let buf0 = BufferId::new(0);
        let buf1 = BufferId::new(1);
        let closed_buf = BufferId::new(2);
        let missing_buf = BufferId::new(3);

        let ref_base = RefWorkspace::new(&[(0, "a界b😀\r\nx"), (1, " \tfoo\nbar")], &[2]);
        let mut seq = Sequencer::new(ref_base.to_catalog(), default_actors());
        let mut log = DeliveryLog::new(seq.snapshot());
        let mut session =
            WorkspaceSession::new(seq.snapshot(), default_actors(), full_access_policy());
        populate_session_for_actor(&mut session, a1, buf0, 0, 4);
        populate_session_for_actor(&mut session, a1, buf1, 0, 2);
        let mut receipts = CutReceipts::default();
        let mut groups = GroupPlan::new();

        let overlay_keys = [
            AnnotationKey {
                buffer: buf0,
                namespace: NamespaceId::new(1),
                id: AnnotationId::new(1),
            },
            AnnotationKey {
                buffer: buf1,
                namespace: NamespaceId::new(1),
                id: AnnotationId::new(1),
            },
        ];
        let snap_all = |s: &Sequencer,
                        r: &CutReceipts,
                        g: &GroupPlan,
                        sess: &WorkspaceSession|
         -> FullStateSnapshot {
            FullStateSnapshot {
                snapshot: s.snapshot(),
                snapshots: s.snapshots().clone(),
                journal: s.journal().clone(),
                history: s.history().clone(),
                registers: r.registers().clone(),
                rings: r.rings().clone(),
                groups: g.clone(),
                session: capture_session_observation(
                    sess,
                    &[a1, a2],
                    &[buf0, buf1, closed_buf, missing_buf],
                    &["bm"],
                    &overlay_keys,
                    &["a"],
                ),
            }
        };

        let base_snap = seq.snapshot();
        let text0 = base_snap.catalog().get(buf0).expect("buf0");
        let text1 = base_snap.catalog().get(buf1).expect("buf1");
        let full0 = BufferView::full(base_snap.catalog(), buf0).expect("full0");
        let full1 = BufferView::full(base_snap.catalog(), buf1).expect("full1");
        let narrow0 = BufferView::narrowed(
            base_snap.catalog(),
            buf0,
            ByteSpan::validate_in(text0, 1, 5).expect("1..5"),
        )
        .expect("narrow0");
        let ascii = Text::from_str(&"z".repeat(32));
        let bad_utf8_span = ByteSpan::validate_in(&ascii, 2, 3).expect("inside 界");
        let oob_span = ByteSpan::validate_in(&ascii, 0, 25).expect("oob");
        let intent0 = EditIntent::for_actor(a1, Nonce::new(1), base_snap.revision());
        let stale_intent = EditIntent::for_actor(a1, Nonce::new(2), Revision::new(9));

        let saved = snap_all(&seq, &receipts, &groups, &session);

        // 5.1 Splice & Patch rejections: OOB, non-UTF-8 boundary, inverted range, closed-interval contact.
        assert!(matches!(
            ByteSpan::validate_in(text0, 5, 1),
            Err(SpanError::InvertedRange { start: 5, end: 1 })
        ));
        assert!(matches!(
            Splice::delete(bad_utf8_span).apply(text0),
            Err(SpliceError::NotCharBoundary { offset: 2 })
        ));
        assert!(matches!(
            Splice::delete(oob_span).apply(text0),
            Err(SpliceError::OutOfBounds { .. })
        ));
        let s01 = ByteSpan::validate_in(text0, 0, 1).expect("0..1");
        let s14 = ByteSpan::validate_in(text0, 1, 4).expect("1..4");
        let s04 = ByteSpan::validate_in(text0, 0, 4).expect("0..4");
        for (x, y) in [
            (s01, s14),
            (s04, s14),
            (
                ByteSpan::empty_at(s01.start()),
                ByteSpan::empty_at(s01.start()),
            ),
        ] {
            assert!(matches!(
                Patch::new(vec![Splice::delete(x), Splice::delete(y)]),
                Err(PatchError::Contact { .. })
            ));
        }

        // 5.2 command::plan rejections: stale rev, wrong buffer, outside visible, bad UTF-8, OOB.
        let sel1 = Selection::new(
            base_snap.catalog(),
            &full1,
            buf1,
            text1.validate_offset(0).expect("0"),
            text1.validate_offset(1).expect("1"),
        )
        .expect("sel1");
        assert!(matches!(
            command::plan(
                stale_intent,
                &full0,
                &base_snap,
                EditCommand::Delete { span: s01 }
            ),
            Err(CommandError::Transaction(
                TransactionError::RevisionMismatch { .. }
            ))
        ));
        assert!(matches!(
            command::plan(
                intent0,
                &full0,
                &base_snap,
                EditCommand::DeleteSelection { selection: sel1 }
            ),
            Err(CommandError::View(ViewError::BufferMismatch { .. }))
        ));
        assert!(matches!(
            command::plan(
                intent0,
                &narrow0,
                &base_snap,
                EditCommand::Delete { span: s01 }
            ),
            Err(CommandError::View(ViewError::SpanOutsideVisible { .. }))
        ));
        assert!(matches!(
            command::plan(
                intent0,
                &full0,
                &base_snap,
                EditCommand::Delete {
                    span: bad_utf8_span
                }
            ),
            Err(CommandError::View(ViewError::InvalidSpan(
                SpanError::NotCharBoundary { .. }
            )))
        ));
        assert!(matches!(
            command::plan(
                intent0,
                &full0,
                &base_snap,
                EditCommand::Delete { span: oob_span }
            ),
            Err(CommandError::View(ViewError::SpanOutsideVisible { .. }))
        ));

        // 5.3 Transaction & WorkspaceEdit rejections: duplicate buffer, unknown/closed buffer, stale rev, actor/base mismatch, contact.
        let good_patch = Patch::new(vec![Splice::delete(s01)]).expect("good");
        assert!(matches!(
            Transaction::new(
                intent0,
                vec![(buf0, good_patch.clone()), (buf0, good_patch.clone())]
            ),
            Err(TransactionError::DuplicateBufferId(_))
        ));
        assert_eq!(
            restore::restore(&base_snap, closed_buf, Text::empty(), a1, Nonce::new(1)),
            Err(RestoreError::BufferClosed(closed_buf))
        );
        assert_eq!(
            restore::restore(&base_snap, missing_buf, Text::empty(), a1, Nonce::new(1)),
            Err(RestoreError::UnknownBuffer(missing_buf))
        );
        assert_eq!(
            restore::restore_many(
                &base_snap,
                [(buf0, Text::from_str("x")), (buf0, Text::from_str("y"))],
                a1,
                Nonce::new(1),
            ),
            Err(RestoreError::DuplicateBufferId(buf0))
        );
        for (bad_id, expected_closed) in [(closed_buf, true), (missing_buf, false)] {
            let err = WorkspaceEdit::new(
                a1,
                base_snap.revision(),
                vec![(buf0, good_patch.clone()), (bad_id, Patch::default())],
                &base_snap,
            )
            .expect_err("must reject bad buffer");
            if expected_closed {
                assert_eq!(
                    err,
                    WorkspaceEditError::Transaction(TransactionError::BufferClosed(bad_id))
                );
            } else {
                assert_eq!(
                    err,
                    WorkspaceEditError::Transaction(TransactionError::UnknownBuffer(bad_id))
                );
            }
        }
        let we0 = WorkspaceEdit::new(
            a1,
            base_snap.revision(),
            vec![(buf0, good_patch.clone())],
            &base_snap,
        )
        .expect("we0");
        assert!(matches!(
            we0.compose(&WorkspaceEdit::empty(a2, base_snap.revision())),
            Err(WorkspaceEditError::ActorMismatch { .. })
        ));
        assert!(matches!(
            we0.compose(&WorkspaceEdit::empty(a1, Revision::new(5))),
            Err(WorkspaceEditError::BaseMismatch { .. })
        ));
        let we_touch = WorkspaceEdit::new(
            a1,
            base_snap.revision(),
            vec![(buf0, Patch::new(vec![Splice::delete(s14)]).expect("s14"))],
            &base_snap,
        )
        .expect("we touch");
        assert!(matches!(
            we0.compose(&we_touch),
            Err(WorkspaceEditError::Patch {
                error: PatchError::Contact { .. },
                ..
            })
        ));

        // 5.4 Multi-cursor rejections: contacting distinct edits, wrong buffer, invalid boundary, stale rev.
        assert!(matches!(
            multi_cursor::plan(
                intent0,
                &full0,
                &base_snap,
                vec![
                    EditCommand::Replace {
                        span: s01,
                        text: Text::from_str("X")
                    },
                    EditCommand::Replace {
                        span: s14,
                        text: Text::from_str("Y")
                    },
                ],
            ),
            Err(MultiCursorError::Patch(PatchError::Contact { .. }))
        ));
        assert!(matches!(
            multi_cursor::plan(
                intent0,
                &full0,
                &base_snap,
                vec![
                    EditCommand::Delete { span: s01 },
                    EditCommand::Delete {
                        span: bad_utf8_span
                    },
                ],
            ),
            Err(MultiCursorError::View(ViewError::InvalidSpan(_)))
        ));
        assert!(matches!(
            multi_cursor::plan(stale_intent, &full0, &base_snap, vec![]),
            Err(MultiCursorError::Transaction(
                TransactionError::RevisionMismatch { .. }
            ))
        ));

        // 5.5 Rectangle edit rejections: ZeroTabWidth, out-of-range line, ShortLine, RowCount, closed/unknown/stale.
        let valid_rect = Rectangle {
            first: VisualPosition { line: 0, column: 0 },
            second: VisualPosition { line: 1, column: 3 },
            endpoint: Endpoint::Exclusive,
            tab_width: 4,
        };
        assert!(matches!(
            rectangle_edit::plan_patch(
                text0,
                Rectangle {
                    tab_width: 0,
                    ..valid_rect
                },
                ShortLinePolicy::Clamp,
                RectangleOperation::Delete,
            ),
            Err(RectangleEditError::Rectangle(RectangleError::ZeroTabWidth))
        ));
        assert!(matches!(
            rectangle_edit::plan_patch(
                text0,
                Rectangle {
                    second: VisualPosition { line: 9, column: 1 },
                    ..valid_rect
                },
                ShortLinePolicy::Clamp,
                RectangleOperation::Delete,
            ),
            Err(RectangleEditError::Rectangle(RectangleError::Line(_)))
        ));
        assert!(matches!(
            rectangle_edit::plan_patch(
                text0,
                valid_rect,
                ShortLinePolicy::Error,
                RectangleOperation::Delete,
            ),
            Err(RectangleEditError::ShortLine { line: 1, .. })
        ));
        assert!(matches!(
            rectangle_edit::plan_patch(
                text0,
                Rectangle {
                    first: VisualPosition { line: 0, column: 2 },
                    second: VisualPosition { line: 1, column: 3 },
                    ..valid_rect
                },
                ShortLinePolicy::Error,
                RectangleOperation::Insert("!"),
            ),
            Err(RectangleEditError::ShortLine { line: 1, .. })
        ));
        assert!(matches!(
            rectangle_edit::plan_patch(
                text0,
                valid_rect,
                ShortLinePolicy::Clamp,
                RectangleOperation::Replace(&["only_one"]),
            ),
            Err(RectangleEditError::RowCount {
                expected: 2,
                actual: 1
            })
        ));
        assert!(matches!(
            rectangle_edit::plan_transaction(
                &base_snap,
                closed_buf,
                valid_rect,
                ShortLinePolicy::Clamp,
                RectangleOperation::Delete,
                intent0,
            ),
            Err(RectangleEditError::Transaction(
                TransactionError::BufferClosed(_)
            ))
        ));
        assert!(matches!(
            rectangle_edit::plan_transaction(
                &base_snap,
                buf0,
                valid_rect,
                ShortLinePolicy::Clamp,
                RectangleOperation::Delete,
                stale_intent,
            ),
            Err(RectangleEditError::Transaction(
                TransactionError::RevisionMismatch { .. }
            ))
        ));

        // 5.6 Indent region rejections: inverted, out-of-range, zero tab width, outside region, non-indent edit, invalid span.
        let ind_policy = IndentPolicy {
            column: 2,
            tab_width: 4,
            style: IndentStyle::Spaces,
        };
        assert!(matches!(
            indent_region::plan(text1, Range { start: 2, end: 1 }, ind_policy),
            Err(IndentRegionError::InvalidRegion { .. })
        ));
        assert!(matches!(
            indent_region::plan(text1, 0..9, ind_policy),
            Err(IndentRegionError::InvalidRegion { .. })
        ));
        assert!(matches!(
            indent_region::plan(
                text1,
                0..1,
                IndentPolicy {
                    tab_width: 0,
                    ..ind_policy
                }
            ),
            Err(IndentRegionError::Indent(IndentError::ZeroTabWidth))
        ));
        let non_indent_patch = Patch::new(vec![Splice::new(
            ByteSpan::validate_in(text1, 2, 3).expect("f"),
            Text::from_str(" "),
        )])
        .expect("patch");
        assert!(matches!(
            indent_region::validate_patch(text1, 0..2, &non_indent_patch),
            Err(IndentRegionError::NonIndentEdit { .. })
        ));
        let outside_patch = Patch::new(vec![Splice::insert(
            text1.validate_offset(0).expect("0"),
            Text::from_str(" "),
        )])
        .expect("patch");
        assert!(matches!(
            indent_region::validate_patch(text1, 1..2, &outside_patch),
            Err(IndentRegionError::OutsideRegion { .. })
        ));
        let bad_ind_span = Patch::new(vec![Splice::delete(bad_utf8_span)]).expect("bad");
        assert!(matches!(
            indent_region::validate_patch(text0, 0..2, &bad_ind_span),
            Err(IndentRegionError::InvalidSpan(_))
        ));

        // 5.7 Replace-all rejections: empty pattern, span outside view, invalid boundary, stale rev.
        assert_eq!(
            replace_all::plan_patch(&base_snap, full0, full0.visible(), "", "x"),
            Err(ReplaceAllError::Search(SearchError::EmptyPattern))
        );
        assert!(matches!(
            replace_all::plan_patch(&base_snap, narrow0, ByteSpan::full_of(text0), "a", "x"),
            Err(ReplaceAllError::View(ViewError::SpanOutsideVisible { .. }))
        ));
        assert!(matches!(
            replace_all::plan_patch(&base_snap, full0, bad_utf8_span, "a", "x"),
            Err(ReplaceAllError::View(ViewError::InvalidSpan(_)))
        ));
        assert!(matches!(
            replace_all::plan_transaction(
                &base_snap,
                full0,
                full0.visible(),
                "a",
                "x",
                stale_intent
            ),
            Err(ReplaceAllError::Transaction(
                TransactionError::RevisionMismatch { .. }
            ))
        ));

        // 5.8 Register-backed rejections: missing register, overlapping selections, forged receipt, invalid boundary.
        let reg_a = RegisterName::new("a").expect("a");
        assert_eq!(
            register_edit::paste(
                receipts.registers(),
                intent0,
                &full0,
                &base_snap,
                &[],
                &reg_a
            ),
            Err(RegisterEditError::MissingRegister)
        );
        assert_eq!(
            register_edit::yank(receipts.rings(), intent0, &full0, &base_snap, &[]),
            Err(RegisterEditError::MissingRegister)
        );
        let ov1 = Selection::new(
            base_snap.catalog(),
            &full0,
            buf0,
            text0.validate_offset(0).expect("0"),
            text0.validate_offset(4).expect("4"),
        )
        .expect("ov1");
        let ov2 = Selection::new(
            base_snap.catalog(),
            &full0,
            buf0,
            text0.validate_offset(1).expect("1"),
            text0.validate_offset(5).expect("5"),
        )
        .expect("ov2");
        assert!(matches!(
            register_edit::cut(
                intent0,
                &full0,
                &base_snap,
                &[ov1, ov2],
                reg_a.clone(),
                RegisterKind::Charwise
            ),
            Err(RegisterEditError::Plan(MultiCursorError::Patch(
                PatchError::Contact { .. }
            )))
        ));
        let cut_ok = register_edit::cut(
            intent0,
            &full0,
            &base_snap,
            &[ov1],
            reg_a.clone(),
            RegisterKind::Charwise,
        )
        .expect("cut ok");
        let forged =
            SubmitOutcome::Committed(CommitReceipt::new(intent0, base_snap.revision()).expect("r"));
        assert_eq!(
            receipts.apply(a1, &cut_ok, forged, &seq),
            Err(RegisterEditError::MismatchedCommit)
        );

        // Additional register_edit atomicity: submit_cut rejections and ConflictingReplay.
        let snap_reg = |s: &Sequencer, r: &CutReceipts| {
            (
                s.snapshot(),
                s.snapshots().clone(),
                s.journal().clone(),
                s.history().clone(),
                r.registers().clone(),
                r.rings().clone(),
            )
        };
        let mut reg_seq = Sequencer::new(ref_base.to_catalog(), default_actors());
        let mut reg_receipts = CutReceipts::default();
        let reg_snap0 = reg_seq.snapshot();
        let reg_full0 = BufferView::full(reg_snap0.catalog(), buf0).expect("reg full0");
        let reg_text0 = reg_snap0.catalog().get(buf0).expect("reg text0");
        let reg_sel04 = Selection::new(
            reg_snap0.catalog(),
            &reg_full0,
            buf0,
            reg_text0.validate_offset(0).expect("0"),
            reg_text0.validate_offset(4).expect("4"),
        )
        .expect("reg sel04");
        let reg_sel01 = Selection::new(
            reg_snap0.catalog(),
            &reg_full0,
            buf0,
            reg_text0.validate_offset(0).expect("0"),
            reg_text0.validate_offset(1).expect("1"),
        )
        .expect("reg sel01");

        // submit_cut rejected by sequencer (unknown actor) leaves sequencer and CutReceipts unchanged.
        let reg_before_unknown = snap_reg(&reg_seq, &reg_receipts);
        let cut_unknown = register_edit::cut(
            EditIntent::for_actor(unknown_actor, Nonce::new(1), reg_snap0.revision()),
            &reg_full0,
            &reg_snap0,
            &[reg_sel04],
            reg_a.clone(),
            RegisterKind::Charwise,
        )
        .expect("cut unknown actor");
        let unknown_outcome =
            register_edit::submit_cut(&mut reg_receipts, &mut reg_seq, &cut_unknown)
                .expect("sequencer rejection returns outcome");
        assert_eq!(
            unknown_outcome.reject_reason(),
            Some(RejectReason::UnknownActor)
        );
        assert_eq!(
            reg_receipts.apply(unknown_actor, &cut_unknown, unknown_outcome, &reg_seq),
            Err(RegisterEditError::MismatchedCommit)
        );
        assert_state_unchanged(
            &reg_before_unknown,
            &snap_reg(&reg_seq, &reg_receipts),
            "submit_cut rejected by sequencer",
        );

        // Commit one real cut, then verify stale submit_cut and ConflictingReplay atomicity.
        let reg_intent1 = EditIntent::for_actor(a1, Nonce::new(1), reg_snap0.revision());
        let cut_real = register_edit::cut(
            reg_intent1,
            &reg_full0,
            &reg_snap0,
            &[reg_sel04],
            reg_a.clone(),
            RegisterKind::Charwise,
        )
        .expect("cut real");
        let ok_outcome = register_edit::submit_cut(&mut reg_receipts, &mut reg_seq, &cut_real)
            .expect("submit real cut");
        assert!(ok_outcome.is_committed());
        assert_eq!(
            reg_receipts.apply(a1, &cut_real, ok_outcome, &reg_seq),
            Ok(true)
        );
        let reg_after_real = snap_reg(&reg_seq, &reg_receipts);

        let stale_cut = register_edit::cut(
            EditIntent::for_actor(a1, Nonce::new(2), reg_snap0.revision()),
            &reg_full0,
            &reg_snap0,
            &[reg_sel01],
            reg_a.clone(),
            RegisterKind::Charwise,
        )
        .expect("stale cut");
        assert_eq!(
            register_edit::submit_cut(&mut reg_receipts, &mut reg_seq, &stale_cut),
            Err(RegisterEditError::MismatchedCommit)
        );
        assert_state_unchanged(
            &reg_after_real,
            &snap_reg(&reg_seq, &reg_receipts),
            "submit_cut stale base",
        );

        let reg_b = RegisterName::new("b").expect("b");
        let conflicting_cut = register_edit::cut(
            reg_intent1,
            &reg_full0,
            &reg_snap0,
            &[reg_sel04],
            reg_b,
            RegisterKind::Charwise,
        )
        .expect("conflicting cut");
        assert_eq!(
            register_edit::submit_cut(&mut reg_receipts, &mut reg_seq, &conflicting_cut),
            Err(RegisterEditError::ConflictingReplay)
        );
        assert_state_unchanged(
            &reg_after_real,
            &snap_reg(&reg_seq, &reg_receipts),
            "submit_cut ConflictingReplay",
        );
        assert_eq!(
            reg_receipts.apply(a1, &conflicting_cut, ok_outcome, &reg_seq),
            Err(RegisterEditError::ConflictingReplay)
        );
        assert_state_unchanged(
            &reg_after_real,
            &snap_reg(&reg_seq, &reg_receipts),
            "CutReceipts::apply ConflictingReplay",
        );

        // submit_cut with a TxnId already committed directly in sequencer -> MismatchedCommit.
        let reg_snap1 = reg_seq.snapshot();
        let reg_full1 = BufferView::full(reg_snap1.catalog(), buf1).expect("reg full1");
        let reg_text1 = reg_snap1.catalog().get(buf1).expect("reg text1");
        let reg_sel1 = Selection::new(
            reg_snap1.catalog(),
            &reg_full1,
            buf1,
            reg_text1.validate_offset(0).expect("0"),
            reg_text1.validate_offset(1).expect("1"),
        )
        .expect("reg sel1");
        let direct_intent = EditIntent::for_actor(a2, Nonce::new(1), reg_snap1.revision());
        let cut_pre_direct = register_edit::cut(
            direct_intent,
            &reg_full1,
            &reg_snap1,
            &[reg_sel1],
            reg_a.clone(),
            RegisterKind::Charwise,
        )
        .expect("cut pre direct");
        assert!(reg_seq
            .submit(cut_pre_direct.transaction().clone())
            .is_committed());
        let reg_after_direct = snap_reg(&reg_seq, &reg_receipts);
        assert_eq!(
            register_edit::submit_cut(&mut reg_receipts, &mut reg_seq, &cut_pre_direct),
            Err(RegisterEditError::MismatchedCommit)
        );
        assert_state_unchanged(
            &reg_after_direct,
            &snap_reg(&reg_seq, &reg_receipts),
            "submit_cut journal txn collision",
        );
        assert!(matches!(
            reg_receipts.copy(
                stale_intent,
                &reg_full1,
                &reg_snap1,
                &[reg_sel1],
                CopyTarget {
                    name: reg_a,
                    kind: RegisterKind::Charwise,
                },
            ),
            Err(RegisterEditError::Plan(MultiCursorError::Transaction(
                TransactionError::RevisionMismatch { .. }
            )))
        ));
        assert_state_unchanged(
            &reg_after_direct,
            &snap_reg(&reg_seq, &reg_receipts),
            "CutReceipts::copy stale revision",
        );

        // 5.9 Sequencer rejections: unknown actor, revision mismatch, unknown buffer, closed buffer,
        // bad UTF-8, plus TxnIdReuse, Conflict, RevokedActor, and BaseExpired.
        for (bad_txn, expected_reason) in [
            (
                Transaction::new(
                    EditIntent::for_actor(unknown_actor, Nonce::new(1), base_snap.revision()),
                    vec![(buf0, good_patch.clone())],
                )
                .expect("t"),
                RejectReason::UnknownActor,
            ),
            (
                Transaction::new(stale_intent, vec![(buf0, good_patch.clone())]).expect("t"),
                RejectReason::RevisionMismatch,
            ),
            (
                Transaction::new(intent0, vec![(missing_buf, Patch::default())]).expect("t"),
                RejectReason::UnknownBuffer,
            ),
            (
                Transaction::new(intent0, vec![(closed_buf, Patch::default())]).expect("t"),
                RejectReason::BufferClosed,
            ),
            (
                Transaction::new(
                    intent0,
                    vec![(
                        buf0,
                        Patch::new(vec![Splice::delete(bad_utf8_span)]).expect("p"),
                    )],
                )
                .expect("t"),
                RejectReason::InvalidInput,
            ),
        ] {
            assert_eq!(seq.submit(bad_txn).reject_reason(), Some(expected_reason));
        }

        let snap_seq = |s: &Sequencer| {
            (
                s.snapshot(),
                s.snapshots().clone(),
                s.journal().clone(),
                s.history().clone(),
            )
        };
        let mut seq_adv = Sequencer::new(ref_base.to_catalog(), default_actors());
        let adv_snap0 = seq_adv.snapshot();
        let adv_txn0 = Transaction::new(
            EditIntent::for_actor(a1, Nonce::new(1), adv_snap0.revision()),
            vec![(buf0, good_patch.clone())],
        )
        .expect("adv txn0");
        assert!(seq_adv.submit(adv_txn0).is_committed());
        let adv_saved = snap_seq(&seq_adv);

        // TxnIdReuse: same (a1, Nonce(1)) with a different payload.
        let reuse_txn = Transaction::new(
            EditIntent::for_actor(a1, Nonce::new(1), adv_snap0.revision()),
            vec![(
                buf0,
                Patch::new(vec![Splice::new(s01, Text::from_str("Y"))]).expect("reuse patch"),
            )],
        )
        .expect("reuse txn");
        assert_eq!(
            seq_adv.submit(reuse_txn).reject_reason(),
            Some(RejectReason::TxnIdReuse)
        );
        assert_state_unchanged(&adv_saved, &snap_seq(&seq_adv), "sequencer TxnIdReuse");

        // Conflict: overlapping (0..1) and endpoint-contacting (1..4) stale transactions at rev 0.
        for (nonce_val, span) in [(1, s01), (2, s14)] {
            let conflict_txn = Transaction::new(
                EditIntent::for_actor(a2, Nonce::new(nonce_val), adv_snap0.revision()),
                vec![(
                    buf0,
                    Patch::new(vec![Splice::new(span, Text::from_str("C"))])
                        .expect("conflict patch"),
                )],
            )
            .expect("conflict txn");
            assert_eq!(
                seq_adv.submit(conflict_txn).reject_reason(),
                Some(RejectReason::Conflict)
            );
            assert_state_unchanged(&adv_saved, &snap_seq(&seq_adv), "sequencer Conflict");
        }

        // RevokedActor: revoke a2, then submit a valid edit at tip rev 1.
        seq_adv.actors_mut().revoke(a2).expect("revoke a2");
        let adv_revoked = snap_seq(&seq_adv);
        let buf1_s01 = ByteSpan::validate_in(text1, 0, 1).expect("buf1 0..1");
        let revoked_txn = Transaction::new(
            EditIntent::for_actor(a2, Nonce::new(3), seq_adv.revision()),
            vec![(
                buf1,
                Patch::new(vec![Splice::new(buf1_s01, Text::from_str("Q"))]).expect("p"),
            )],
        )
        .expect("revoked txn");
        assert_eq!(
            seq_adv.submit(revoked_txn).reject_reason(),
            Some(RejectReason::RevokedActor)
        );
        assert_state_unchanged(&adv_revoked, &snap_seq(&seq_adv), "sequencer RevokedActor");

        // BaseExpired: prune rev 0, then submit a disjoint edit based at rev 0.
        seq_adv.prune_before(Revision::new(1));
        let adv_pruned = snap_seq(&seq_adv);
        let expired_txn = Transaction::new(
            EditIntent::for_actor(a1, Nonce::new(2), adv_snap0.revision()),
            vec![(
                buf1,
                Patch::new(vec![Splice::new(buf1_s01, Text::from_str("Q"))]).expect("p"),
            )],
        )
        .expect("expired txn");
        assert_eq!(
            seq_adv.submit(expired_txn).reject_reason(),
            Some(RejectReason::BaseExpired)
        );
        assert_state_unchanged(&adv_pruned, &snap_seq(&seq_adv), "sequencer BaseExpired");

        // 5.10 WorkspaceSession invalid transitions leave session strictly unchanged.
        let mut foreign_seq = Sequencer::new(ref_base.to_catalog(), default_actors());
        let mut foreign_log = DeliveryLog::new(foreign_seq.snapshot());
        let foreign_txn = Transaction::new(
            EditIntent::for_actor(a1, Nonce::new(1), foreign_seq.revision()),
            vec![(buf0, good_patch.clone())],
        )
        .expect("foreign txn");
        let foreign_r = foreign_seq
            .submit(foreign_txn)
            .committed_receipt()
            .expect("fr");
        foreign_log
            .record_workspace(&foreign_seq, foreign_r)
            .expect("flog");
        assert_eq!(
            session.advance(foreign_log.events().last().expect("fev")),
            Err(SessionError::InvalidTransition)
        );
        assert_eq!(
            session.update_batch(foreign_seq.snapshot(), [buf0, buf0]),
            Err(SessionError::InvalidTransition)
        );

        // 5.11 Verify ALL state across Sequencer, CutReceipts, GroupPlan, WorkspaceSession is strictly unchanged!
        assert_state_unchanged(
            &saved,
            &snap_all(&seq, &receipts, &groups, &session),
            "all rejections up to 5.10",
        );

        // 5.12 Interop commands & change-group rejections leave VersionedWorkspace unchanged.
        let mut ws_cmd = VersionedWorkspace::new(ref_base.to_catalog());
        let ws_cmd_before = ws_cmd.capture();
        let ctx = InteropContext {
            actor: a1,
            buffer: buf0,
        };
        let invalid_emacs_seqs: &[&[EmacsCommand]] = &[
            &[EmacsCommand::BeginChangeGroup, EmacsCommand::EndChangeGroup], // empty group
            &[
                EmacsCommand::BeginChangeGroup,
                EmacsCommand::BeginChangeGroup,
            ], // nested group
            &[EmacsCommand::EndChangeGroup],                                 // end without begin
            &[
                EmacsCommand::BeginChangeGroup,
                EmacsCommand::Point(EmacsCharPos::new(1).expect("1")),
                EmacsCommand::Insert(Text::from_str("x")),
            ], // unclosed group
            &[EmacsCommand::Insert(Text::from_str("x"))],                    // missing point
            &[EmacsCommand::Unsupported("custom".into())],
            &[
                EmacsCommand::BeginChangeGroup,
                EmacsCommand::Point(EmacsCharPos::new(1).expect("1")),
                EmacsCommand::Mark(EmacsCharPos::new(2).expect("2")),
                EmacsCommand::SelectRegion, // selection inside open group
                EmacsCommand::EndChangeGroup,
            ],
        ];
        for seq_cmds in invalid_emacs_seqs {
            assert!(lower_emacs(&ws_cmd_before, ctx, seq_cmds).is_err());
            assert_state_unchanged(&ws_cmd_before, &ws_cmd.capture(), "invalid emacs group");
        }
        // Reversed agent endpoints & invalid UTF-8 byte boundary.
        let bad_agent = [AgentCommand::Replace {
            start: AgentPosition::Byte(BytePos::new(2)),
            end: AgentPosition::Byte(BytePos::new(3)),
            text: Text::from_str("x"),
        }];
        let err = lower_agent(&ws_cmd_before, ctx, &bad_agent).expect_err("bad boundary");
        assert_eq!(err.code(), ErrorCode::InvalidBoundary);
        // Permission denied in apply_requests leaves workspace unchanged.
        let valid_agent = [AgentCommand::Replace {
            start: AgentPosition::Byte(BytePos::new(0)),
            end: AgentPosition::Byte(BytePos::new(1)),
            text: Text::from_str("Z"),
        }];
        let valid_reqs = lower_agent(&ws_cmd_before, ctx, &valid_agent).expect("lower");
        let readonly = {
            let mut p = AccessPolicy::new();
            p.grant_default(Capability::Read);
            p
        };
        assert_eq!(
            apply_requests(&mut ws_cmd, &readonly, &default_actors(), &valid_reqs)
                .expect_err("denied")
                .code(),
            ErrorCode::PermissionDenied
        );
        assert_state_unchanged(&ws_cmd_before, &ws_cmd.capture(), "apply_requests denied");

        // lower_neovim rejections leave workspace unchanged.
        let invalid_neovim_seqs: &[(&[NeovimCommand], ErrorCode)] = &[
            (
                &[NeovimCommand::BeginUndoBlock, NeovimCommand::EndUndoBlock],
                ErrorCode::CorruptSequence,
            ),
            (
                &[NeovimCommand::BeginUndoBlock, NeovimCommand::BeginUndoBlock],
                ErrorCode::CorruptSequence,
            ),
            (&[NeovimCommand::EndUndoBlock], ErrorCode::CorruptSequence),
            (
                &[
                    NeovimCommand::BeginUndoBlock,
                    NeovimCommand::Cursor(LineColByte::new(0, 0)),
                    NeovimCommand::Insert(Text::from_str("x")),
                ],
                ErrorCode::CorruptSequence,
            ),
            (
                &[NeovimCommand::Insert(Text::from_str("x"))],
                ErrorCode::InvalidCoordinate,
            ),
            (
                &[NeovimCommand::Unsupported("custom".into())],
                ErrorCode::UnsupportedOperation,
            ),
            (
                &[
                    NeovimCommand::BeginUndoBlock,
                    NeovimCommand::Select {
                        anchor: LineColByte::new(0, 0),
                        head: LineColByte::new(0, 1),
                    },
                    NeovimCommand::EndUndoBlock,
                ],
                ErrorCode::UnsupportedOperation,
            ),
            (
                &[NeovimCommand::SetText {
                    start: LineColByte::new(0, 4),
                    end: LineColByte::new(0, 1),
                    text: Text::from_str("x"),
                }],
                ErrorCode::InvalidSpan,
            ),
            (
                &[NeovimCommand::Cursor(LineColByte::new(0, 2))],
                ErrorCode::InvalidBoundary,
            ),
            (
                &[NeovimCommand::Cursor(LineColByte::new(9, 0))],
                ErrorCode::OutOfRange,
            ),
        ];
        for (seq_cmds, expected_code) in invalid_neovim_seqs {
            assert_eq!(
                lower_neovim(&ws_cmd_before, ctx, seq_cmds)
                    .expect_err("invalid neovim sequence")
                    .code(),
                *expected_code
            );
            assert_state_unchanged(&ws_cmd_before, &ws_cmd.capture(), "invalid neovim sequence");
        }

        // decode_batch rejections: stale revision, closed/unknown buffer, contacting spans, invalid boundary.
        let make_byte_ext =
            |buffer: BufferId, edit_intent: EditIntent, start: usize, end: usize, repl: &str| {
                ExternalEdit {
                    buffer,
                    intent: edit_intent,
                    region: OrientedRegion {
                        anchor: RegionCoord::Agent(AgentPosition::Byte(BytePos::new(start))),
                        head: RegionCoord::Agent(AgentPosition::Byte(BytePos::new(end))),
                        direction: Direction::Forward,
                        mode: EndpointMode::Exclusive,
                    },
                    replacement: Text::from_str(repl),
                }
            };
        let ext_valid_buf0 = make_byte_ext(buf0, intent0, 0, 1, "A");
        let ext_valid_buf1 = make_byte_ext(buf1, intent0, 0, 1, "B");
        for (batch_intent, edits, expected_code, label) in [
            (
                stale_intent,
                vec![ext_valid_buf0.clone()],
                ErrorCode::RevisionMismatch,
                "decode_batch stale batch intent",
            ),
            (
                intent0,
                vec![
                    ext_valid_buf0.clone(),
                    make_byte_ext(buf1, stale_intent, 0, 1, "S"),
                ],
                ErrorCode::RevisionMismatch,
                "decode_batch stale item intent",
            ),
            (
                intent0,
                vec![
                    ext_valid_buf0.clone(),
                    make_byte_ext(closed_buf, intent0, 0, 0, "C"),
                ],
                ErrorCode::BufferClosed,
                "decode_batch closed buffer",
            ),
            (
                intent0,
                vec![
                    ext_valid_buf0.clone(),
                    make_byte_ext(missing_buf, intent0, 0, 0, "U"),
                ],
                ErrorCode::UnknownBuffer,
                "decode_batch unknown buffer",
            ),
            (
                intent0,
                vec![
                    ext_valid_buf0.clone(),
                    make_byte_ext(buf0, intent0, 1, 4, "T"),
                ],
                ErrorCode::Conflict,
                "decode_batch touching spans",
            ),
            (
                intent0,
                vec![
                    make_byte_ext(buf0, intent0, 0, 0, "P"),
                    make_byte_ext(buf0, intent0, 0, 0, "Q"),
                ],
                ErrorCode::Conflict,
                "decode_batch coincident insertions",
            ),
            (
                intent0,
                vec![ext_valid_buf1, make_byte_ext(buf0, intent0, 2, 3, "I")],
                ErrorCode::InvalidBoundary,
                "decode_batch invalid UTF-8 boundary",
            ),
        ] {
            assert_eq!(
                decode_batch(&ws_cmd_before, batch_intent, &edits)
                    .expect_err(label)
                    .code(),
                expected_code,
                "{label}"
            );
            assert_state_unchanged(&ws_cmd_before, &ws_cmd.capture(), label);
        }

        // apply_requests rejections: DifferentWorkspace, RevokedActor, and RevisionMismatch.
        let foreign_ws = VersionedWorkspace::new(ref_base.to_catalog());
        let foreign_reqs =
            lower_agent(&foreign_ws.capture(), ctx, &valid_agent).expect("foreign lower");
        assert_eq!(
            apply_requests(
                &mut ws_cmd,
                &full_access_policy(),
                &default_actors(),
                &foreign_reqs,
            )
            .expect_err("different workspace")
            .code(),
            ErrorCode::CorruptSequence
        );
        assert_state_unchanged(
            &ws_cmd_before,
            &ws_cmd.capture(),
            "apply_requests DifferentWorkspace",
        );

        let mut revoked_actors = default_actors();
        revoked_actors.revoke(a1).expect("revoke a1");
        assert_eq!(
            apply_requests(
                &mut ws_cmd,
                &full_access_policy(),
                &revoked_actors,
                &valid_reqs,
            )
            .expect_err("revoked actor")
            .code(),
            ErrorCode::RevokedActor
        );
        assert_state_unchanged(
            &ws_cmd_before,
            &ws_cmd.capture(),
            "apply_requests RevokedActor",
        );

        let mut ws_rev = VersionedWorkspace::new(ref_base.to_catalog());
        let stale_reqs = lower_agent(&ws_rev.capture(), ctx, &valid_agent).expect("stale lower");
        ws_rev
            .update(buf1, Text::from_str("advanced"))
            .expect("advance ws_rev");
        let rev1_snap = ws_rev.capture();
        assert_eq!(
            apply_requests(
                &mut ws_rev,
                &full_access_policy(),
                &default_actors(),
                &stale_reqs,
            )
            .expect_err("stale revision")
            .code(),
            ErrorCode::RevisionMismatch
        );
        assert_state_unchanged(
            &rev1_snap,
            &ws_rev.capture(),
            "apply_requests RevisionMismatch",
        );

        let mut fresh_then_stale =
            lower_agent(&rev1_snap, ctx, &valid_agent).expect("fresh rev1 lower");
        fresh_then_stale.extend(stale_reqs);
        assert_eq!(
            apply_requests(
                &mut ws_rev,
                &full_access_policy(),
                &default_actors(),
                &fresh_then_stale,
            )
            .expect_err("multi-request trailing stale revision")
            .code(),
            ErrorCode::RevisionMismatch
        );
        assert_state_unchanged(
            &rev1_snap,
            &ws_rev.capture(),
            "apply_requests multi-req RevisionMismatch atomicity",
        );

        // 5.13 Recorded commands failure preserves committed prefix and ignores executable suffix.
        let mut ws_rec = VersionedWorkspace::new(ref_base.to_catalog());
        let rec_v0 = BufferView::full(ws_rec.catalog(), buf0).expect("v0");
        let rec_p0 = text0.validate_offset(0).expect("0");
        let rec_cmds = [
            RecordedCommand {
                intent: EditIntent::for_actor(a1, Nonce::new(1), Revision::ZERO),
                view: rec_v0,
                command: RecordedAction::Edit(EditCommand::Insert {
                    at: rec_p0,
                    text: Text::from_str("OK"),
                }),
            },
            RecordedCommand {
                intent: EditIntent::for_actor(a1, Nonce::new(2), Revision::new(1)),
                view: rec_v0,
                command: RecordedAction::InsertAfterMotion {
                    point: ascii.validate_offset(20).expect("20"),
                    delta: 1,
                    text: Text::from_str("FAIL"),
                },
            },
            RecordedCommand {
                intent: EditIntent::for_actor(a1, Nonce::new(3), Revision::new(1)),
                view: rec_v0,
                command: RecordedAction::Edit(EditCommand::Insert {
                    at: rec_p0,
                    text: Text::from_str("MUST_NOT_RUN"),
                }),
            },
        ];
        let fold_res = recorded_commands::fold(&mut ws_rec, &rec_cmds);
        assert_eq!(fold_res.edits.len(), 1);
        assert!(matches!(
            fold_res.failure,
            Some((1, RecordedCommandError::Motion(MotionError::View(_))))
        ));
        assert_eq!(ws_rec.revision(), Revision::new(1));
        assert_eq!(
            ws_rec.catalog().get(buf0).expect("0").as_str(),
            "OKa界b😀\r\nx"
        );

        // 5.14 EditHistory, GroupPlan, SelectiveUndo, Redo, interop::undo rejections are atomic.
        let r1_txn = Transaction::new(intent0, vec![(buf0, good_patch.clone())]).expect("r1");
        let outcome1 = seq.submit(r1_txn.clone());
        let r1 = outcome1.committed_receipt().expect("r1 committed");
        log.record_workspace(&seq, r1).expect("log r1");
        session
            .advance(log.events().last().expect("ev1"))
            .expect("adv1");

        let step0 = SequenceStep {
            sequence: TxnId::new(a1, Nonce::new(50)),
            index: 0,
        };
        groups.project(outcome1, Some(step0)).expect("project 1");
        let groups_saved = groups.clone();
        assert_eq!(
            groups.project(
                outcome1,
                Some(SequenceStep {
                    sequence: TxnId::new(a1, Nonce::new(51)),
                    index: 0,
                }),
            ),
            Err(GroupError::SequenceConflict(r1.txn()))
        );
        let alt_receipt = CommitReceipt::new(
            EditIntent::for_actor(a2, Nonce::new(1), Revision::ZERO),
            Revision::ZERO,
        )
        .expect("alt");
        assert_eq!(
            groups.project(SubmitOutcome::Committed(alt_receipt), None),
            Err(GroupError::RevisionConflict)
        );
        let next_receipt = CommitReceipt::new(
            EditIntent::for_actor(a2, Nonce::new(2), Revision::new(1)),
            Revision::new(1),
        )
        .expect("next");
        assert_eq!(
            groups.project(SubmitOutcome::Committed(next_receipt), Some(step0)),
            Err(GroupError::StepConflict(step0))
        );
        assert_state_unchanged(&groups_saved, &groups, "GroupPlan conflicts");

        // History rejections.
        let mut hist = seq.history().clone();
        let hist_saved = hist.clone();
        assert!(matches!(
            hist.record(
                &base_snap,
                Revision::new(1),
                seq.snapshot().catalog(),
                r1_txn.clone()
            ),
            Err(HistoryError::NonContiguous { .. })
        ));
        let tip_snap = seq.snapshot();
        assert!(matches!(
            hist.record(&tip_snap, Revision::new(2), tip_snap.catalog(), r1_txn),
            Err(HistoryError::BaseMismatch { .. })
        ));
        let dup_txn =
            Transaction::new(EditIntent::new(r1.txn(), tip_snap.revision()), vec![]).expect("dup");
        assert!(matches!(
            hist.record(&tip_snap, Revision::new(2), tip_snap.catalog(), dup_txn),
            Err(HistoryError::DuplicateTxnId(_))
        ));
        let fresh_txn = Transaction::new(
            EditIntent::for_actor(a1, Nonce::new(77), tip_snap.revision()),
            vec![],
        )
        .expect("fresh");
        assert!(matches!(
            hist.record(
                &tip_snap,
                Revision::new(5),
                tip_snap.catalog(),
                fresh_txn.clone()
            ),
            Err(HistoryError::CommittedMismatch { .. })
        ));
        assert_eq!(
            hist.record(&tip_snap, Revision::new(2), base_snap.catalog(), fresh_txn),
            Err(HistoryError::CommittedCatalogMismatch)
        );
        assert_state_unchanged(&hist_saved, &hist, "EditHistory rejections");

        // Undo / Redo rejections.
        let mut redo_engine = Redo::new(SelectiveUndo::new(seq));
        let engine_before = (
            redo_engine.sequencer().snapshot(),
            redo_engine.sequencer().snapshots().clone(),
            redo_engine.sequencer().journal().clone(),
            redo_engine.sequencer().history().clone(),
        );
        assert_eq!(
            redo_engine.undo(a2, r1.txn(), TxnId::new(a2, Nonce::new(2))),
            Err(UndoError::NotOwner {
                target: r1.txn(),
                actor: a2,
            })
        );
        assert_eq!(
            redo_engine.undo(
                a1,
                TxnId::new(a1, Nonce::new(999)),
                TxnId::new(a1, Nonce::new(2))
            ),
            Err(UndoError::UnknownTarget(TxnId::new(a1, Nonce::new(999))))
        );
        assert_eq!(
            redo_engine.undo(a1, r1.txn(), TxnId::new(a2, Nonce::new(2))),
            Err(UndoError::InvalidCompensationActor)
        );
        assert_eq!(
            redo_engine.undo(a1, r1.txn(), r1.txn()),
            Err(UndoError::CompensationIdReuse(r1.txn()))
        );
        assert_eq!(
            redo_engine.redo(a1, r1.txn(), TxnId::new(a1, Nonce::new(3))),
            Err(RedoError::NotUndone(r1.txn()))
        );
        assert_eq!(
            interop_undo::submit(
                &mut redo_engine,
                a1,
                TxnId::new(a1, Nonce::new(4)),
                FrontendCommand::EmacsUndoOnly(UndoTarget::Resolved(&[])),
            ),
            Err(InteropError::AmbiguousTarget)
        );
        assert_state_unchanged(
            &engine_before,
            &(
                redo_engine.sequencer().snapshot(),
                redo_engine.sequencer().snapshots().clone(),
                redo_engine.sequencer().journal().clone(),
                redo_engine.sequencer().history().clone(),
            ),
            "undo/redo rejections",
        );

        let snap_redo = |e: &Redo, targets: &[TxnId], comps: &[TxnId]| {
            (
                e.sequencer().snapshot(),
                e.sequencer().snapshots().clone(),
                e.sequencer().journal().clone(),
                e.sequencer().history().clone(),
                targets
                    .iter()
                    .map(|&t| e.compensation_for(t))
                    .collect::<Vec<_>>(),
                comps.iter().map(|&c| e.redo_for(c)).collect::<Vec<_>>(),
            )
        };

        // Commit a disjoint edit on buf1 by a1, then undo r1 to test AlreadyUndone & CompensationTarget.
        let t_disjoint = Transaction::new(
            EditIntent::for_actor(a1, Nonce::new(10), redo_engine.sequencer().revision()),
            vec![(
                buf1,
                Patch::new(vec![Splice::new(buf1_s01, Text::from_str("X"))]).expect("p"),
            )],
        )
        .expect("t disjoint");
        let t_disjoint_id = t_disjoint.intent().txn();
        assert!(redo_engine.submit(t_disjoint).is_committed());

        let comp1 = TxnId::new(a1, Nonce::new(11));
        assert!(redo_engine.undo(a1, r1.txn(), comp1).is_ok());
        let before_already = snap_redo(&redo_engine, &[r1.txn(), comp1], &[comp1]);
        assert_eq!(
            redo_engine.undo(a1, r1.txn(), TxnId::new(a1, Nonce::new(12))),
            Err(UndoError::AlreadyUndone {
                target: r1.txn(),
                compensation: comp1,
            })
        );
        assert_state_unchanged(
            &before_already,
            &snap_redo(&redo_engine, &[r1.txn(), comp1], &[comp1]),
            "UndoError::AlreadyUndone",
        );
        assert_eq!(
            redo_engine.undo(a1, comp1, TxnId::new(a1, Nonce::new(13))),
            Err(UndoError::CompensationTarget {
                target: comp1,
                original: r1.txn(),
            })
        );
        assert_state_unchanged(
            &before_already,
            &snap_redo(&redo_engine, &[r1.txn(), comp1], &[comp1]),
            "UndoError::CompensationTarget",
        );

        // Redo r1, then test RedoError::RedoIdReuse and RedoError::AlreadyRedone.
        let redo1 = TxnId::new(a1, Nonce::new(14));
        assert!(redo_engine.redo(a1, r1.txn(), redo1).is_ok());
        let before_redone = snap_redo(&redo_engine, &[r1.txn(), comp1, redo1], &[comp1]);
        assert_eq!(
            redo_engine.redo(a1, comp1, redo1),
            Err(RedoError::RedoIdReuse(redo1))
        );
        assert_state_unchanged(
            &before_redone,
            &snap_redo(&redo_engine, &[r1.txn(), comp1, redo1], &[comp1]),
            "RedoError::RedoIdReuse (request mismatch)",
        );
        assert_eq!(
            redo_engine.redo(a1, r1.txn(), r1.txn()),
            Err(RedoError::RedoIdReuse(r1.txn()))
        );
        assert_state_unchanged(
            &before_redone,
            &snap_redo(&redo_engine, &[r1.txn(), comp1, redo1], &[comp1]),
            "RedoError::RedoIdReuse (journal collision)",
        );
        assert_eq!(
            redo_engine.redo(a1, comp1, TxnId::new(a1, Nonce::new(15))),
            Err(RedoError::AlreadyRedone {
                compensation: comp1,
                redo: redo1,
            })
        );
        assert_state_unchanged(
            &before_redone,
            &snap_redo(&redo_engine, &[r1.txn(), comp1, redo1], &[comp1]),
            "RedoError::AlreadyRedone",
        );

        // UndoError::Rejected(RejectReason::Conflict): a2 edits buf1[0..1], conflicting with t_disjoint.
        let buf1_cur = redo_engine
            .sequencer()
            .snapshot()
            .catalog()
            .get(buf1)
            .expect("buf1")
            .clone();
        let conflict_u_txn = Transaction::new(
            EditIntent::for_actor(a2, Nonce::new(10), redo_engine.sequencer().revision()),
            vec![(
                buf1,
                Patch::new(vec![Splice::new(
                    ByteSpan::validate_in(&buf1_cur, 0, 1).expect("0..1"),
                    Text::from_str("Y"),
                )])
                .expect("p"),
            )],
        )
        .expect("conflict u");
        assert!(redo_engine.submit(conflict_u_txn).is_committed());
        let before_u_conflict = snap_redo(&redo_engine, &[t_disjoint_id, r1.txn()], &[comp1]);
        assert_eq!(
            redo_engine.undo(a1, t_disjoint_id, TxnId::new(a1, Nonce::new(16))),
            Err(UndoError::Rejected(RejectReason::Conflict))
        );
        assert_state_unchanged(
            &before_u_conflict,
            &snap_redo(&redo_engine, &[t_disjoint_id, r1.txn()], &[comp1]),
            "UndoError::Rejected(Conflict)",
        );

        // RedoError::Rejected(RejectReason::Conflict): undo r1 again, then a2 overwrites buf0[0..1].
        let comp2 = TxnId::new(a1, Nonce::new(17));
        assert!(redo_engine.undo(a1, r1.txn(), comp2).is_ok());
        let buf0_cur = redo_engine
            .sequencer()
            .snapshot()
            .catalog()
            .get(buf0)
            .expect("buf0")
            .clone();
        let conflict_r_txn = Transaction::new(
            EditIntent::for_actor(a2, Nonce::new(11), redo_engine.sequencer().revision()),
            vec![(
                buf0,
                Patch::new(vec![Splice::new(
                    ByteSpan::validate_in(&buf0_cur, 0, 1).expect("0..1"),
                    Text::from_str("Z"),
                )])
                .expect("p"),
            )],
        )
        .expect("conflict r");
        assert!(redo_engine.submit(conflict_r_txn).is_committed());
        let before_r_conflict = snap_redo(&redo_engine, &[r1.txn(), redo1], &[comp1, comp2]);
        assert_eq!(
            redo_engine.redo(a1, r1.txn(), TxnId::new(a1, Nonce::new(18))),
            Err(RedoError::Rejected(RejectReason::Conflict))
        );
        assert_state_unchanged(
            &before_r_conflict,
            &snap_redo(&redo_engine, &[r1.txn(), redo1], &[comp1, comp2]),
            "RedoError::Rejected(Conflict)",
        );
    }

    // =========================================================================
    // Test 6: Cross-Entry-Point Equivalence Against Shared Reference Model
    // =========================================================================

    #[test]
    fn cross_entry_point_equivalence_against_reference_model() {
        let actor = ActorId::new(1);
        let buf0 = BufferId::new(0);
        let buf1 = BufferId::new(1);
        let policy = full_access_policy();
        let registry = default_actors();

        // 6a. Single-span edit expressed via 10 distinct entry points matches RefModel!
        for (source, start, end, replacement) in [
            ("a界b😀c", 1, 4, "新😀"),
            ("hello\r\nworld", 5, 7, "\n"),
            ("e\u{0301}x", 0, 3, ""),
            ("", 0, 0, "界😀"),
            ("abc", 1, 1, "XYZ"),
        ] {
            let ref_edit = RefEdit {
                start,
                end,
                text: replacement.to_owned(),
            };
            let (expected_text, expected_inv) =
                ref_apply_single(source, std::slice::from_ref(&ref_edit)).expect("ref");
            let mut ref_ws = RefWorkspace::new(&[(0, source)], &[]);
            ref_ws
                .commit_plan(
                    (1, 1),
                    &RefPlan {
                        buffers: BTreeMap::from([(0, vec![ref_edit.clone()])]),
                    },
                    false,
                )
                .expect("ref commit");

            let base_ws =
                VersionedWorkspace::new(catalog_for_single(buf0, &Text::from_str(source)));
            let snap = base_ws.capture();
            let text = snap.catalog().get(buf0).expect("0");
            let view = BufferView::full(snap.catalog(), buf0).expect("view");
            let span = ByteSpan::validate_in(text, start, end).expect("span");
            let sel =
                Selection::new(snap.catalog(), &view, buf0, span.start(), span.end()).expect("sel");
            let intent = EditIntent::for_actor(actor, Nonce::new(1), snap.revision());

            // 1) Splice
            let splice = Splice::new(span, Text::from_str(replacement));
            let (sp_out, sp_inv) = splice.apply_with_inverse(text).expect("splice");
            assert_eq!(sp_out.as_str(), expected_text);
            assert_eq!(
                (
                    sp_inv.span().start().as_usize(),
                    sp_inv.span().end().as_usize(),
                    sp_inv.replacement().as_str()
                ),
                (
                    expected_inv[0].start,
                    expected_inv[0].end,
                    expected_inv[0].text.as_str()
                )
            );

            // 2) Patch
            let patch = Patch::new(vec![splice]).expect("patch");
            assert_eq!(patch.apply(text).expect("patch").as_str(), expected_text);

            // 3) command::plan (Replace, ReplaceSelection, flipped, plus Insert/Delete when applicable)
            let cmd_txn = command::plan(
                intent,
                &view,
                &snap,
                EditCommand::Replace {
                    span,
                    text: Text::from_str(replacement),
                },
            )
            .expect("cmd replace");
            assert_eq!(
                command::plan(
                    intent,
                    &view,
                    &snap,
                    EditCommand::ReplaceSelection {
                        selection: sel.flip(),
                        text: Text::from_str(replacement),
                    },
                ),
                Ok(cmd_txn.clone())
            );
            assert_catalog_matches_ref(
                cmd_txn.apply_with_inverse(&snap).expect("apply").catalog(),
                &ref_ws,
                "command::plan",
            );

            // 4) Transaction
            let raw_txn = Transaction::new(intent, vec![(buf0, patch.clone())]).expect("txn");
            assert_eq!(raw_txn, cmd_txn);

            // 5) WorkspaceEdit
            let we =
                WorkspaceEdit::new(actor, snap.revision(), vec![(buf0, patch)], &snap).expect("we");
            assert_eq!(we.to_transaction(Nonce::new(1), &snap), Ok(cmd_txn.clone()));

            // 6) multi_cursor::plan (including deduplicated duplicate)
            let mc_txn = multi_cursor::plan(
                intent,
                &view,
                &snap,
                vec![
                    EditCommand::Replace {
                        span,
                        text: Text::from_str(replacement),
                    },
                    EditCommand::ReplaceSelection {
                        selection: sel.flip(),
                        text: Text::from_str(replacement),
                    },
                ],
            )
            .expect("mc");
            assert_eq!(mc_txn, cmd_txn);

            // 7) recorded_commands::fold
            let mut ws_rec =
                VersionedWorkspace::new(catalog_for_single(buf0, &Text::from_str(source)));
            let rec_cmd = RecordedCommand {
                intent,
                view,
                command: RecordedAction::Edit(EditCommand::Replace {
                    span,
                    text: Text::from_str(replacement),
                }),
            };
            let fold = recorded_commands::fold(&mut ws_rec, &[rec_cmd]);
            assert_eq!(fold.failure, None);
            assert_catalog_matches_ref(ws_rec.catalog(), &ref_ws, "recorded_commands");

            // 8) interop::commands: Emacs, Neovim, Agent (both direct and wrapped in change group)
            let ctx = InteropContext {
                actor,
                buffer: buf0,
            };
            let em_start = byte_to_emacs_pos(source, start);
            let em_end = byte_to_emacs_pos(source, end);
            let nv_start = byte_to_neovim_pos(source, start);
            let nv_end = byte_to_neovim_pos(source, end);
            let em_direct = [
                EmacsCommand::Mark(em_start),
                EmacsCommand::Point(em_end),
                EmacsCommand::ReplaceRegion(Text::from_str(replacement)),
            ];
            let nv_direct = [NeovimCommand::SetText {
                start: nv_start,
                end: nv_end,
                text: Text::from_str(replacement),
            }];
            let ag_direct = [AgentCommand::Replace {
                start: AgentPosition::Byte(BytePos::new(start)),
                end: AgentPosition::Byte(BytePos::new(end)),
                text: Text::from_str(replacement),
            }];
            let em_group = [
                EmacsCommand::BeginChangeGroup,
                EmacsCommand::Mark(em_start),
                EmacsCommand::Point(em_end),
                EmacsCommand::ReplaceRegion(Text::from_str(replacement)),
                EmacsCommand::EndChangeGroup,
            ];
            let nv_group = [
                NeovimCommand::BeginUndoBlock,
                NeovimCommand::SetText {
                    start: nv_start,
                    end: nv_end,
                    text: Text::from_str(replacement),
                },
                NeovimCommand::EndUndoBlock,
            ];
            let ag_group = [
                AgentCommand::BeginBatch,
                AgentCommand::Replace {
                    start: AgentPosition::Byte(BytePos::new(start)),
                    end: AgentPosition::Byte(BytePos::new(end)),
                    text: Text::from_str(replacement),
                },
                AgentCommand::EndBatch,
            ];
            macro_rules! check_interop {
                ($label:expr, $lower:expr, $cmds:expr) => {{
                    let mut ws_io =
                        VersionedWorkspace::new(catalog_for_single(buf0, &Text::from_str(source)));
                    let snap_io = ws_io.capture();
                    let reqs = $lower(&snap_io, ctx, &$cmds).expect($label);
                    apply_requests(&mut ws_io, &policy, &registry, &reqs).expect("apply io");
                    assert_catalog_matches_ref(ws_io.catalog(), &ref_ws, $label);
                    reqs
                }};
            }
            let em_dir_reqs = check_interop!("interop emacs direct", lower_emacs, em_direct);
            let nv_dir_reqs = check_interop!("interop neovim direct", lower_neovim, nv_direct);
            let ag_dir_reqs = check_interop!("interop agent direct", lower_agent, ag_direct);
            assert!(requests_equivalent(&em_dir_reqs, &nv_dir_reqs));
            assert!(requests_equivalent(&nv_dir_reqs, &ag_dir_reqs));

            let em_grp_reqs = check_interop!("interop emacs group", lower_emacs, em_group);
            let nv_grp_reqs = check_interop!("interop neovim group", lower_neovim, nv_group);
            let ag_grp_reqs = check_interop!("interop agent group", lower_agent, ag_group);
            assert!(requests_equivalent(&em_grp_reqs, &nv_grp_reqs));
            assert!(requests_equivalent(&nv_grp_reqs, &ag_grp_reqs));

            // 9) restore::restore targeting expected_text
            if expected_text != source {
                let RestoreOutcome::Transaction(rst_txn) = restore::restore(
                    &snap,
                    buf0,
                    Text::from_str(&expected_text),
                    actor,
                    Nonce::new(1),
                )
                .expect("restore") else {
                    panic!("expected changed restoration");
                };
                assert_catalog_matches_ref(
                    rst_txn.apply_with_inverse(&snap).expect("apply").catalog(),
                    &ref_ws,
                    "restore",
                );
            }

            // 10) register_edit::paste (and cut when deleting)
            let mut reg_store = RegisterStore::new();
            let reg_name = RegisterName::new("eq").expect("name");
            reg_store.put(
                actor,
                reg_name.clone(),
                RegisterEntry {
                    text: Text::from_str(replacement),
                    kind: RegisterKind::Charwise,
                },
            );
            let paste_txn =
                register_edit::paste(&reg_store, intent, &view, &snap, &[sel], &reg_name)
                    .expect("paste");
            assert_eq!(paste_txn, cmd_txn);
            if replacement.is_empty() && start < end {
                let cut_plan = register_edit::cut(
                    intent,
                    &view,
                    &snap,
                    &[sel],
                    reg_name,
                    RegisterKind::Charwise,
                )
                .expect("cut");
                assert_eq!(cut_plan.transaction(), &cmd_txn);
            }
        }

        // 6b. Specialized multi-span entry points (rectangle, indent, replace_all, multi-cursor,
        // change group, register paste) agree with each other AND with RefModel!
        {
            // Rectangle vs multi_cursor vs WorkspaceEdit vs RefModel.
            let src = "ab\t界😀\r\ncd\tx\nefghij";
            let text = Text::from_str(src);
            let snap = VersionedWorkspace::new(catalog_for_single(buf0, &text)).capture();
            let view = BufferView::full(snap.catalog(), buf0).expect("view");
            let intent = EditIntent::for_actor(actor, Nonce::new(1), snap.revision());
            let rows = ["R0", "R1", "R2"];
            for endpoint in [Endpoint::Exclusive, Endpoint::Inclusive] {
                for op in [
                    RectangleOperation::Delete,
                    RectangleOperation::Uniform("界"),
                    RectangleOperation::Insert("!"),
                    RectangleOperation::Replace(&rows),
                ] {
                    let rect = Rectangle {
                        first: VisualPosition { line: 0, column: 1 },
                        second: VisualPosition { line: 2, column: 5 },
                        endpoint,
                        tab_width: 4,
                    };
                    let ref_edits = ref_rectangle_edits(
                        src,
                        RefRectSpec {
                            first_line: 0,
                            first_col: 1,
                            second_line: 2,
                            second_col: 5,
                            inclusive: endpoint == Endpoint::Inclusive,
                            tab_width: 4,
                            clamp_short: true,
                            op,
                        },
                    )
                    .expect("ref rect");
                    let (ref_out, _) = ref_apply_single(src, &ref_edits).expect("ref apply");
                    let r_txn = rectangle_edit::plan_transaction(
                        &snap,
                        buf0,
                        rect,
                        ShortLinePolicy::Clamp,
                        op,
                        intent,
                    )
                    .expect("rect txn");
                    let mc_cmds: Vec<_> = ref_edits
                        .iter()
                        .map(|e| EditCommand::Replace {
                            span: ByteSpan::validate_in(&text, e.start, e.end).expect("span"),
                            text: Text::from_str(&e.text),
                        })
                        .collect();
                    let mc_txn = multi_cursor::plan(intent, &view, &snap, mc_cmds).expect("mc");
                    assert_eq!(r_txn, mc_txn);
                    assert_eq!(
                        r_txn
                            .apply_with_inverse(&snap)
                            .expect("apply")
                            .catalog()
                            .get(buf0)
                            .expect("0")
                            .as_str(),
                        ref_out
                    );
                    assert_inverse_restores(&snap, &r_txn, "rectangle equiv");
                }
            }

            // Indent & Dedent region vs multi_cursor vs RefModel.
            let ind_src = "   \tfoo\r\n\t  界\n😀\n  ";
            let ind_text = Text::from_str(ind_src);
            let ind_snap = VersionedWorkspace::new(catalog_for_single(buf0, &ind_text)).capture();
            let ind_view = BufferView::full(ind_snap.catalog(), buf0).expect("view");
            let ind_intent = EditIntent::for_actor(actor, Nonce::new(1), ind_snap.revision());
            for (col, style) in [
                (0, IndentStyle::Spaces),        // dedent all the way to 0
                (2, IndentStyle::Spaces),        // dedent partially
                (6, IndentStyle::TabsAndSpaces), // indent up with tabs + spaces
                (8, IndentStyle::Spaces),        // indent up with spaces
            ] {
                let ipol = IndentPolicy {
                    column: col,
                    tab_width: 4,
                    style,
                };
                let ref_edits = ref_indent_region_edits(ind_src, 0..4, ipol).expect("ref ind");
                let (ref_out, _) = ref_apply_single(ind_src, &ref_edits).expect("apply ref");
                let ind_patch = indent_region::plan(&ind_text, 0..4, ipol).expect("ind plan");
                let mc_cmds: Vec<_> = ref_edits
                    .iter()
                    .map(|e| EditCommand::Replace {
                        span: ByteSpan::validate_in(&ind_text, e.start, e.end).expect("span"),
                        text: Text::from_str(&e.text),
                    })
                    .collect();
                let mc_txn =
                    multi_cursor::plan(ind_intent, &ind_view, &ind_snap, mc_cmds).expect("mc");
                assert_eq!(mc_txn.patches(), &[(buf0, ind_patch.clone())]);
                assert_eq!(ind_patch.apply(&ind_text).expect("apply").as_str(), ref_out);
            }

            // Replace-all vs multi_cursor vs RefModel.
            let ra_src = "界aaaa界aa界";
            let ra_text = Text::from_str(ra_src);
            let ra_snap = VersionedWorkspace::new(catalog_for_single(buf0, &ra_text)).capture();
            let ra_view = BufferView::full(ra_snap.catalog(), buf0).expect("view");
            let ra_intent = EditIntent::for_actor(actor, Nonce::new(1), ra_snap.revision());
            let ref_edits =
                ref_replace_all_edits(ra_src, 0, ra_src.len(), "aa", "😀").expect("ref ra");
            let (ref_out, _) = ref_apply_single(ra_src, &ref_edits).expect("apply ref");
            let ra_txn = replace_all::plan_transaction(
                &ra_snap,
                ra_view,
                ra_view.visible(),
                "aa",
                "😀",
                ra_intent,
            )
            .expect("ra txn");
            let mc_cmds: Vec<_> = ref_edits
                .iter()
                .map(|e| EditCommand::Replace {
                    span: ByteSpan::validate_in(&ra_text, e.start, e.end).expect("span"),
                    text: Text::from_str(&e.text),
                })
                .collect();
            let mc_txn = multi_cursor::plan(ra_intent, &ra_view, &ra_snap, mc_cmds).expect("mc ra");
            assert_eq!(ra_txn, mc_txn);
            assert_eq!(
                ra_txn
                    .apply_with_inverse(&ra_snap)
                    .expect("apply")
                    .catalog()
                    .get(buf0)
                    .expect("0")
                    .as_str(),
                ref_out
            );
        }

        // 6c. Multi-buffer equivalence: Transaction vs WorkspaceEdit vs decode_batch vs restore_many vs RefWorkspace.
        {
            let ref_base = RefWorkspace::new(&[(0, "a界b"), (1, "x😀y\r\nz")], &[2]);
            let plan = RefPlan {
                buffers: BTreeMap::from([
                    (
                        0,
                        vec![RefEdit {
                            start: 1,
                            end: 4,
                            text: "OK".into(),
                        }],
                    ),
                    (
                        1,
                        vec![RefEdit {
                            start: 1,
                            end: 5,
                            text: "NO".into(),
                        }],
                    ),
                ]),
            };
            let mut ref_after = ref_base.clone();
            ref_after.commit_plan((1, 1), &plan, false).expect("ref");

            let snap = VersionedWorkspace::new(ref_base.to_catalog()).capture();
            let intent = EditIntent::for_actor(actor, Nonce::new(1), snap.revision());
            let direct_txn = txn_from_ref(&snap, actor, 1, &plan);
            let we_txn =
                WorkspaceEdit::new(actor, snap.revision(), direct_txn.patches().to_vec(), &snap)
                    .expect("we")
                    .to_transaction(Nonce::new(1), &snap)
                    .expect("to txn");
            assert_eq!(direct_txn, we_txn);

            // decode_batch with oriented external edits.
            let ext_edits: Vec<_> = [(buf0, 1, 4, "OK"), (buf1, 1, 5, "NO")]
                .into_iter()
                .map(|(buf, s, e, repl)| {
                    let t = snap.catalog().get(buf).expect("buf");
                    let v = BufferView::full(snap.catalog(), buf).expect("view");
                    let sel = Selection::new(
                        snap.catalog(),
                        &v,
                        buf,
                        t.validate_offset(s).expect("s"),
                        t.validate_offset(e).expect("e"),
                    )
                    .expect("sel");
                    ExternalEdit {
                        buffer: buf,
                        intent,
                        region: OrientedRegion::from_selection(
                            snap.catalog(),
                            &v,
                            sel,
                            CoordKind::Byte,
                            EndpointMode::Exclusive,
                        )
                        .expect("region"),
                        replacement: Text::from_str(repl),
                    }
                })
                .collect();
            let batch_txn = decode_batch(&snap, intent, &ext_edits).expect("decode_batch");
            assert_eq!(batch_txn, direct_txn);

            let RestoreOutcome::Transaction(rst_txn) = restore::restore_many(
                &snap,
                [
                    (buf0, Text::from_str(&ref_after.live[&0])),
                    (buf1, Text::from_str(&ref_after.live[&1])),
                ],
                actor,
                Nonce::new(1),
            )
            .expect("restore_many") else {
                panic!("expected changed restore");
            };
            assert_catalog_matches_ref(
                direct_txn
                    .apply_with_inverse(&snap)
                    .expect("apply")
                    .catalog(),
                &ref_after,
                "multi-buffer direct",
            );
            assert_catalog_matches_ref(
                rst_txn.apply_with_inverse(&snap).expect("apply").catalog(),
                &ref_after,
                "multi-buffer restore_many",
            );
        }
    }

    // =========================================================================
    // Test 7: Randomized Multi-Step Property Suite With Branch Coverage Counters
    // =========================================================================

    #[derive(Default, Debug)]
    struct MultiStepCoverage {
        cmd_single: usize,
        workspace_multi_buf: usize,
        multi_cursor: usize,
        rectangle: usize,
        indent_region: usize,
        replace_all: usize,
        register_cut: usize,
        register_paste: usize,
        restore_path: usize,
        change_group_sidecheck: usize,
        recorded_fold_sidecheck: usize,
        undo_committed: usize,
        redo_committed: usize,
        noop_submitted: usize,
        rejected_atomic: usize,
        multibyte_utf8_edited: usize,
        crlf_or_cr_edited: usize,
        history_slice_verified: usize,
    }

    #[test]
    fn randomized_multistep_edit_laws_with_coverage() {
        let mut cov = MultiStepCoverage::default();

        assert_law(
            &[0, 1, 7, 42, 1337, u64::MAX],
            |rng: &mut Rng| {
                (0..4)
                    .map(|_| {
                        let s0 = format!("{}\r\n{}", random_text(rng, 8), random_text(rng, 6));
                        let s1 = format!(" \t{}\n{}", random_text(rng, 8), random_text(rng, 6));
                        let step_tags: Vec<usize> = (0..48).map(|_| rng.gen_range(0..13)).collect();
                        let sub_seed = rng.next_u64();
                        (s0, s1, step_tags, sub_seed)
                    })
                    .collect::<Vec<_>>()
            },
            |cases| {
                for (s0, s1, step_tags, sub_seed) in cases {
                    let mut rng = Rng::new(*sub_seed);
                    let a1 = ActorId::new(1);
                    let a2 = ActorId::new(2);
                    let buf0 = BufferId::new(0);
                    let buf1 = BufferId::new(1);
                    let closed_buf = BufferId::new(2);
                    let overlay_keys = [
                        AnnotationKey {
                            buffer: buf0,
                            namespace: NamespaceId::new(1),
                            id: AnnotationId::new(1),
                        },
                        AnnotationKey {
                            buffer: buf1,
                            namespace: NamespaceId::new(1),
                            id: AnnotationId::new(1),
                        },
                    ];

                    let mut ref_ws = RefWorkspace::new(&[(0, s0), (1, s1)], &[2]);
                    let initial_ref_live = ref_ws.live.clone();
                    let mut engine = Redo::new(SelectiveUndo::new(Sequencer::new(
                        ref_ws.to_catalog(),
                        default_actors(),
                    )));
                    let mut log = DeliveryLog::new(engine.sequencer().snapshot());
                    let mut session = WorkspaceSession::new(
                        engine.sequencer().snapshot(),
                        default_actors(),
                        full_access_policy(),
                    );
                    populate_session_for_actor(&mut session, a1, buf0, 0, 0);
                    populate_session_for_actor(&mut session, a1, buf1, 0, 0);
                    populate_session_for_actor(&mut session, a2, buf0, 0, 0);
                    let mut groups = GroupPlan::new();
                    let mut reg_store = RegisterStore::new();
                    let mut ref_registers: BTreeMap<(ActorId, String), String> = BTreeMap::new();
                    let mut snapshots_by_rev =
                        vec![engine.sequencer().snapshot().catalog().clone()];
                    let mut actor_txns: Vec<(ActorId, TxnId)> = Vec::new();
                    let mut undone_targets: Vec<(ActorId, TxnId, TxnId)> = Vec::new();
                    let mut nonce_counter = 1u64;

                    let mut next_nonce = || {
                        let n = nonce_counter;
                        nonce_counter += 1;
                        Nonce::new(n)
                    };

                    let submit_and_verify =
                        |engine: &mut Redo,
                         log: &mut DeliveryLog,
                         session: &mut WorkspaceSession,
                         groups: &mut GroupPlan,
                         ref_ws: &mut RefWorkspace,
                         snapshots_by_rev: &mut Vec<BufferCatalog>,
                         actor_txns: &mut Vec<(ActorId, TxnId)>,
                         cov: &mut MultiStepCoverage,
                         txn: Transaction,
                         expected_plan: &RefPlan,
                         tag: &str| {
                            let base = engine.sequencer().snapshot();
                            assert_inverse_restores(&base, &txn, tag);
                            let (expected_next, _, expected_noop) = ref_ws
                                .apply_plan_preview(expected_plan)
                                .expect("ref preview");
                            let before_state = (
                                engine.sequencer().snapshot(),
                                engine.sequencer().snapshots().clone(),
                                engine.sequencer().journal().clone(),
                                engine.sequencer().history().clone(),
                                groups.clone(),
                            );
                            let outcome = engine.submit(txn.clone());
                            if expected_noop {
                                assert!(outcome.is_noop(), "{tag}: expected noop");
                                assert_eq!(groups.project(outcome, None), Ok(None));
                                assert_state_unchanged(
                                    &before_state,
                                    &(
                                        engine.sequencer().snapshot(),
                                        engine.sequencer().snapshots().clone(),
                                        engine.sequencer().journal().clone(),
                                        engine.sequencer().history().clone(),
                                        groups.clone(),
                                    ),
                                    "randomized noop",
                                );
                                cov.noop_submitted += 1;
                            } else {
                                let mut edited_utf8 = false;
                                let mut edited_cr = false;
                                for (&buf_id, edits) in &expected_plan.buffers {
                                    let before_buf = &ref_ws.live[&buf_id];
                                    for edit in edits {
                                        let removed = &before_buf[edit.start..edit.end];
                                        let inserted = edit.text.as_str();
                                        if removed == inserted {
                                            continue;
                                        }
                                        if removed.bytes().any(|b| b >= 0x80)
                                            || inserted.bytes().any(|b| b >= 0x80)
                                        {
                                            edited_utf8 = true;
                                        }
                                        if removed.contains('\r')
                                            || inserted.contains('\r')
                                            || (edit.start > 0
                                                && before_buf.as_bytes()[edit.start - 1] == b'\r'
                                                && (removed.starts_with('\n')
                                                    || inserted.starts_with('\n')))
                                        {
                                            edited_cr = true;
                                        }
                                    }
                                }
                                let receipt = outcome.committed_receipt().expect("committed");
                                log.record_workspace(engine.sequencer(), receipt)
                                    .expect("log");
                                session
                                    .advance(log.events().last().expect("ev"))
                                    .expect("session advance");
                                let grp = groups
                                    .project(outcome, None)
                                    .expect("project")
                                    .expect("group");
                                assert_eq!(grp.id, receipt);
                                ref_ws
                                    .commit_plan(
                                        (txn.intent().actor().raw(), txn.intent().nonce().raw()),
                                        expected_plan,
                                        false,
                                    )
                                    .expect("ref commit");
                                assert_eq!(ref_ws.live, expected_next);
                                assert_catalog_matches_ref(
                                    engine.sequencer().snapshot().catalog(),
                                    ref_ws,
                                    tag,
                                );
                                assert_eq!(engine.sequencer().revision().raw(), ref_ws.revision);
                                assert_eq!(session.revision().raw(), ref_ws.revision);
                                if edited_utf8 {
                                    cov.multibyte_utf8_edited += 1;
                                }
                                if edited_cr {
                                    cov.crlf_or_cr_edited += 1;
                                }
                                snapshots_by_rev
                                    .push(engine.sequencer().snapshot().catalog().clone());
                                actor_txns.push((txn.intent().actor(), txn.intent().txn()));
                            }
                        };

                    for &step in step_tags {
                        let snap = engine.sequencer().snapshot();
                        let actor = if rng.gen_bool() { a1 } else { a2 };
                        let buf_idx = rng.gen_range(0..2) as u64;
                        let buf = BufferId::new(buf_idx);
                        let cur_str = ref_ws.live[&buf_idx].clone();
                        let cur_text = snap.catalog().get(buf).expect("live").clone();
                        let view = BufferView::full(snap.catalog(), buf).expect("view");
                        let nonce = next_nonce();
                        let intent = EditIntent::for_actor(actor, nonce, snap.revision());

                        match step {
                            // 0: Single-buffer command::plan
                            0 => {
                                let (s, e) = random_valid_span(&mut rng, &cur_str);
                                let repl = random_text(&mut rng, 3);
                                let span = ByteSpan::validate_in(&cur_text, s, e).expect("span");
                                let sel = Selection::new(
                                    snap.catalog(),
                                    &view,
                                    buf,
                                    span.start(),
                                    span.end(),
                                )
                                .expect("sel");
                                let flip = rng.gen_bool();
                                let (cmd, ref_edit) = match rng.gen_range(0..5) {
                                    0 => (
                                        EditCommand::Insert {
                                            at: span.start(),
                                            text: Text::from_str(&repl),
                                        },
                                        RefEdit {
                                            start: s,
                                            end: s,
                                            text: repl,
                                        },
                                    ),
                                    1 => (
                                        EditCommand::Delete { span },
                                        RefEdit {
                                            start: s,
                                            end: e,
                                            text: String::new(),
                                        },
                                    ),
                                    2 => (
                                        EditCommand::Replace {
                                            span,
                                            text: Text::from_str(&repl),
                                        },
                                        RefEdit {
                                            start: s,
                                            end: e,
                                            text: repl,
                                        },
                                    ),
                                    3 => (
                                        EditCommand::ReplaceSelection {
                                            selection: if flip { sel } else { sel.flip() },
                                            text: Text::from_str(&repl),
                                        },
                                        RefEdit {
                                            start: s,
                                            end: e,
                                            text: repl,
                                        },
                                    ),
                                    _ => (
                                        EditCommand::DeleteSelection {
                                            selection: if flip { sel } else { sel.flip() },
                                        },
                                        RefEdit {
                                            start: s,
                                            end: e,
                                            text: String::new(),
                                        },
                                    ),
                                };
                                let txn =
                                    command::plan(intent, &view, &snap, cmd).expect("cmd plan");
                                let plan = RefPlan {
                                    buffers: BTreeMap::from([(buf_idx, vec![ref_edit])]),
                                };
                                submit_and_verify(
                                    &mut engine,
                                    &mut log,
                                    &mut session,
                                    &mut groups,
                                    &mut ref_ws,
                                    &mut snapshots_by_rev,
                                    &mut actor_txns,
                                    &mut cov,
                                    txn,
                                    &plan,
                                    "rand command::plan",
                                );
                                cov.cmd_single += 1;
                            }
                            // 1: Multi-buffer WorkspaceEdit
                            1 => {
                                let (s0_a, e0_a) = random_valid_span(&mut rng, &ref_ws.live[&0]);
                                let (s1_a, e1_a) = random_valid_span(&mut rng, &ref_ws.live[&1]);
                                let r0 = random_text(&mut rng, 2);
                                let r1 = random_text(&mut rng, 2);
                                let plan = RefPlan {
                                    buffers: BTreeMap::from([
                                        (
                                            0,
                                            vec![RefEdit {
                                                start: s0_a,
                                                end: e0_a,
                                                text: r0,
                                            }],
                                        ),
                                        (
                                            1,
                                            vec![RefEdit {
                                                start: s1_a,
                                                end: e1_a,
                                                text: r1,
                                            }],
                                        ),
                                    ]),
                                };
                                let p0 = patch_from_ref(
                                    snap.catalog().get(buf0).expect("0"),
                                    &plan.buffers[&0],
                                );
                                let p1 = patch_from_ref(
                                    snap.catalog().get(buf1).expect("1"),
                                    &plan.buffers[&1],
                                );
                                let we0 = WorkspaceEdit::new(
                                    actor,
                                    snap.revision(),
                                    vec![(buf0, p0)],
                                    &snap,
                                )
                                .expect("we0");
                                let we1 = WorkspaceEdit::new(
                                    actor,
                                    snap.revision(),
                                    vec![(buf1, p1)],
                                    &snap,
                                )
                                .expect("we1");
                                assert_eq!(we0.compose(&we1), we1.compose(&we0));
                                let txn = we0
                                    .compose(&we1)
                                    .expect("compose")
                                    .to_transaction(nonce, &snap)
                                    .expect("to txn");
                                submit_and_verify(
                                    &mut engine,
                                    &mut log,
                                    &mut session,
                                    &mut groups,
                                    &mut ref_ws,
                                    &mut snapshots_by_rev,
                                    &mut actor_txns,
                                    &mut cov,
                                    txn,
                                    &plan,
                                    "rand workspace_edit",
                                );
                                cov.workspace_multi_buf += 1;
                            }
                            // 2: Multi-cursor disjoint edits
                            2 => {
                                let mut edits = Vec::new();
                                let mut cmds = Vec::new();
                                if let Some(((a_s, a_e), (b_s, b_e))) =
                                    random_disjoint_pair(&mut rng, &cur_str)
                                {
                                    let ra = random_text(&mut rng, 2);
                                    let rb = random_text(&mut rng, 2);
                                    cmds.push(EditCommand::Replace {
                                        span: ByteSpan::validate_in(&cur_text, a_s, a_e)
                                            .expect("a"),
                                        text: Text::from_str(&ra),
                                    });
                                    cmds.push(EditCommand::Replace {
                                        span: ByteSpan::validate_in(&cur_text, b_s, b_e)
                                            .expect("b"),
                                        text: Text::from_str(&rb),
                                    });
                                    edits.push(RefEdit {
                                        start: a_s,
                                        end: a_e,
                                        text: ra,
                                    });
                                    edits.push(RefEdit {
                                        start: b_s,
                                        end: b_e,
                                        text: rb,
                                    });
                                }
                                let txn =
                                    multi_cursor::plan(intent, &view, &snap, cmds).expect("mc");
                                let plan = RefPlan {
                                    buffers: BTreeMap::from([(buf_idx, edits)]),
                                };
                                submit_and_verify(
                                    &mut engine,
                                    &mut log,
                                    &mut session,
                                    &mut groups,
                                    &mut ref_ws,
                                    &mut snapshots_by_rev,
                                    &mut actor_txns,
                                    &mut cov,
                                    txn,
                                    &plan,
                                    "rand multi_cursor",
                                );
                                cov.multi_cursor += 1;
                            }
                            // 3: Rectangle edit
                            3 => {
                                let line_count = ref_line_ranges(&cur_str).len();
                                let l0 = rng.gen_range(0..line_count);
                                let l1 = rng.gen_range(0..line_count);
                                let c0 = rng.gen_range(0..6);
                                let c1 = rng.gen_range(0..6);
                                let endpoint = if rng.gen_bool() {
                                    Endpoint::Exclusive
                                } else {
                                    Endpoint::Inclusive
                                };
                                let tab_width = [2, 4, 8][rng.gen_range(0..3)];
                                let uniform_val = random_text(&mut rng, 2);
                                let op = match rng.gen_range(0..3) {
                                    0 => RectangleOperation::Delete,
                                    1 => RectangleOperation::Uniform(&uniform_val),
                                    _ => RectangleOperation::Insert(&uniform_val),
                                };
                                let rect = Rectangle {
                                    first: VisualPosition {
                                        line: l0,
                                        column: c0,
                                    },
                                    second: VisualPosition {
                                        line: l1,
                                        column: c1,
                                    },
                                    endpoint,
                                    tab_width,
                                };
                                let ref_edits = ref_rectangle_edits(
                                    &cur_str,
                                    RefRectSpec {
                                        first_line: l0,
                                        first_col: c0,
                                        second_line: l1,
                                        second_col: c1,
                                        inclusive: endpoint == Endpoint::Inclusive,
                                        tab_width,
                                        clamp_short: true,
                                        op,
                                    },
                                )
                                .expect("ref rect");
                                let txn = rectangle_edit::plan_transaction(
                                    &snap,
                                    buf,
                                    rect,
                                    ShortLinePolicy::Clamp,
                                    op,
                                    intent,
                                )
                                .expect("rect txn");
                                let plan = RefPlan {
                                    buffers: BTreeMap::from([(buf_idx, ref_edits)]),
                                };
                                submit_and_verify(
                                    &mut engine,
                                    &mut log,
                                    &mut session,
                                    &mut groups,
                                    &mut ref_ws,
                                    &mut snapshots_by_rev,
                                    &mut actor_txns,
                                    &mut cov,
                                    txn,
                                    &plan,
                                    "rand rectangle",
                                );
                                cov.rectangle += 1;
                            }
                            // 4: Indent / dedent region
                            4 => {
                                let line_count = ref_line_ranges(&cur_str).len();
                                let a = rng.gen_range(0..line_count + 1);
                                let b = rng.gen_range(0..line_count + 1);
                                let region = a.min(b)..a.max(b);
                                let ipol = IndentPolicy {
                                    column: rng.gen_range(0..7),
                                    tab_width: [2, 4][rng.gen_range(0..2)],
                                    style: if rng.gen_bool() {
                                        IndentStyle::Spaces
                                    } else {
                                        IndentStyle::TabsAndSpaces
                                    },
                                };
                                let ref_edits =
                                    ref_indent_region_edits(&cur_str, region.clone(), ipol)
                                        .expect("ref ind");
                                let patch = indent_region::plan(&cur_text, region.clone(), ipol)
                                    .expect("ind plan");
                                assert_eq!(
                                    indent_region::validate_patch(&cur_text, region, &patch),
                                    Ok(())
                                );
                                let txn =
                                    Transaction::new(intent, vec![(buf, patch)]).expect("txn");
                                let plan = RefPlan {
                                    buffers: BTreeMap::from([(buf_idx, ref_edits)]),
                                };
                                submit_and_verify(
                                    &mut engine,
                                    &mut log,
                                    &mut session,
                                    &mut groups,
                                    &mut ref_ws,
                                    &mut snapshots_by_rev,
                                    &mut actor_txns,
                                    &mut cov,
                                    txn,
                                    &plan,
                                    "rand indent_region",
                                );
                                cov.indent_region += 1;
                            }
                            // 5: Replace-all
                            5 => {
                                let (s, e) = random_valid_span(&mut rng, &cur_str);
                                let span = ByteSpan::validate_in(&cur_text, s, e).expect("span");
                                let pat = ["a", " ", "界", "😀", "\n", "aa"][rng.gen_range(0..6)];
                                let repl = ["", "b", "界", "x"][rng.gen_range(0..4)];
                                let ref_edits = ref_replace_all_edits(&cur_str, s, e, pat, repl)
                                    .expect("ref ra");
                                let txn = replace_all::plan_transaction(
                                    &snap, view, span, pat, repl, intent,
                                )
                                .expect("ra txn");
                                let plan = RefPlan {
                                    buffers: BTreeMap::from([(buf_idx, ref_edits)]),
                                };
                                submit_and_verify(
                                    &mut engine,
                                    &mut log,
                                    &mut session,
                                    &mut groups,
                                    &mut ref_ws,
                                    &mut snapshots_by_rev,
                                    &mut actor_txns,
                                    &mut cov,
                                    txn,
                                    &plan,
                                    "rand replace_all",
                                );
                                cov.replace_all += 1;
                            }
                            // 6: Register cut + paste
                            6 => {
                                let (s, e) = random_valid_span(&mut rng, &cur_str);
                                let span = ByteSpan::validate_in(&cur_text, s, e).expect("span");
                                let sel = Selection::new(
                                    snap.catalog(),
                                    &view,
                                    buf,
                                    span.start(),
                                    span.end(),
                                )
                                .expect("sel");
                                let reg_key = "r";
                                let reg_name = RegisterName::new(reg_key).expect("r");
                                let ref_reg_entry =
                                    ref_registers.get(&(actor, reg_key.to_owned())).cloned();
                                assert_eq!(
                                    reg_store
                                        .get(actor, &reg_name)
                                        .map(|entry| entry.text.as_str()),
                                    ref_reg_entry.as_deref(),
                                    "production RegisterStore diverged from reference register map"
                                );
                                let prefer_cut = rng.gen_bool();
                                match ref_reg_entry {
                                    Some(payload) if !prefer_cut => {
                                        let ref_edits =
                                            ref_register_paste(&cur_str, &[(s, e)], &payload)
                                                .expect("ref paste");
                                        let paste_txn = register_edit::paste(
                                            &reg_store,
                                            intent,
                                            &view,
                                            &snap,
                                            &[sel],
                                            &reg_name,
                                        )
                                        .expect("paste");
                                        let plan = RefPlan {
                                            buffers: BTreeMap::from([(buf_idx, ref_edits)]),
                                        };
                                        submit_and_verify(
                                            &mut engine,
                                            &mut log,
                                            &mut session,
                                            &mut groups,
                                            &mut ref_ws,
                                            &mut snapshots_by_rev,
                                            &mut actor_txns,
                                            &mut cov,
                                            paste_txn,
                                            &plan,
                                            "rand register paste",
                                        );
                                        cov.register_paste += 1;
                                    }
                                    _ => {
                                        let (payload, ref_edits) =
                                            ref_register_cut(&cur_str, &[(s, e)]).expect("ref cut");
                                        let cut_plan = register_edit::cut(
                                            intent,
                                            &view,
                                            &snap,
                                            &[sel],
                                            reg_name.clone(),
                                            RegisterKind::Charwise,
                                        )
                                        .expect("cut");
                                        assert_eq!(cut_plan.entry().text.as_str(), payload);
                                        ref_registers.insert((actor, reg_key.to_owned()), payload);
                                        reg_store.put(
                                            actor,
                                            reg_name.clone(),
                                            cut_plan.entry().clone(),
                                        );
                                        assert_eq!(
                                            reg_store
                                                .get(actor, &reg_name)
                                                .map(|entry| entry.text.as_str()),
                                            ref_registers
                                                .get(&(actor, reg_key.to_owned()))
                                                .map(String::as_str),
                                            "RegisterStore mismatch immediately after cut"
                                        );
                                        let plan = RefPlan {
                                            buffers: BTreeMap::from([(buf_idx, ref_edits)]),
                                        };
                                        submit_and_verify(
                                            &mut engine,
                                            &mut log,
                                            &mut session,
                                            &mut groups,
                                            &mut ref_ws,
                                            &mut snapshots_by_rev,
                                            &mut actor_txns,
                                            &mut cov,
                                            cut_plan.transaction().clone(),
                                            &plan,
                                            "rand register cut",
                                        );
                                        cov.register_cut += 1;
                                    }
                                }
                            }
                            // 7: Restore to an earlier snapshot
                            7 => {
                                let target_idx = rng.gen_range(0..snapshots_by_rev.len());
                                let target_cat = snapshots_by_rev[target_idx].clone();
                                let ref_target = if target_idx == 0 {
                                    initial_ref_live.clone()
                                } else {
                                    ref_ws.history[target_idx - 1].after.clone()
                                };
                                let expected_plan = ref_restore_plan(
                                    &ref_ws.live,
                                    &[(0, &ref_target[&0]), (1, &ref_target[&1])],
                                );
                                let t0 = target_cat.get(buf0).expect("0").clone();
                                let t1 = target_cat.get(buf1).expect("1").clone();
                                let outcome = restore::restore_slice(
                                    &snap,
                                    &[(buf0, t0), (buf1, t1)],
                                    actor,
                                    nonce,
                                )
                                .expect("restore");
                                match outcome {
                                    RestoreOutcome::NoChange => {
                                        assert!(expected_plan.buffers.is_empty());
                                        assert_eq!(snap.catalog(), &target_cat);
                                        cov.noop_submitted += 1;
                                    }
                                    RestoreOutcome::Transaction(txn) => {
                                        assert_eq!(ref_plan_from_txn(&txn), expected_plan);
                                        submit_and_verify(
                                            &mut engine,
                                            &mut log,
                                            &mut session,
                                            &mut groups,
                                            &mut ref_ws,
                                            &mut snapshots_by_rev,
                                            &mut actor_txns,
                                            &mut cov,
                                            txn,
                                            &expected_plan,
                                            "rand restore",
                                        );
                                        assert_eq!(
                                            engine.sequencer().snapshot().catalog(),
                                            &target_cat
                                        );
                                    }
                                }
                                cov.restore_path += 1;
                            }
                            // 8: Undo a recent non-undone commit (supports both immediate and rebased selective undo)
                            8 => {
                                if let Some(&(owner, target_txn)) = actor_txns
                                    .iter()
                                    .rev()
                                    .find(|&&(_, id)| engine.compensation_for(id).is_none())
                                {
                                    let comp_id = TxnId::new(owner, next_nonce());
                                    let target_step = ref_ws
                                        .history
                                        .iter()
                                        .rev()
                                        .find(|h| h.txn == (owner.raw(), target_txn.nonce().raw()))
                                        .expect("ref history")
                                        .clone();
                                    let ref_rebase_result = ref_rebase_plan_from(
                                        &ref_ws.history,
                                        target_step.committed_rev,
                                        &target_step.inverse,
                                    );
                                    let before_undo = (
                                        engine.sequencer().snapshot(),
                                        engine.sequencer().history().clone(),
                                        engine.sequencer().snapshots().clone(),
                                        engine.sequencer().journal().clone(),
                                        engine.compensation_for(target_txn),
                                        engine.redo_for(target_txn),
                                    );
                                    match (
                                        engine.undo(owner, target_txn, comp_id),
                                        ref_rebase_result,
                                    ) {
                                        (Ok(receipt), Ok(rebased_inv)) => {
                                            log.record_workspace(engine.sequencer(), receipt)
                                                .expect("log undo");
                                            session
                                                .advance(log.events().last().expect("ev"))
                                                .expect("adv undo");
                                            groups
                                                .project(SubmitOutcome::Committed(receipt), None)
                                                .expect("grp undo");
                                            ref_ws
                                                .commit_plan(
                                                    (owner.raw(), comp_id.nonce().raw()),
                                                    &rebased_inv,
                                                    false,
                                                )
                                                .expect("ref undo");
                                            assert_catalog_matches_ref(
                                                engine.sequencer().snapshot().catalog(),
                                                &ref_ws,
                                                "rand undo",
                                            );
                                            snapshots_by_rev.push(
                                                engine.sequencer().snapshot().catalog().clone(),
                                            );
                                            undone_targets.push((owner, target_txn, comp_id));
                                            cov.undo_committed += 1;
                                        }
                                        (Err(err), Err(ref_err)) => {
                                            assert_eq!(ref_err, RefError::Contact);
                                            assert_eq!(
                                                err,
                                                UndoError::Rejected(RejectReason::Conflict)
                                            );
                                            assert_state_unchanged(
                                                &before_undo,
                                                &(
                                                    engine.sequencer().snapshot(),
                                                    engine.sequencer().history().clone(),
                                                    engine.sequencer().snapshots().clone(),
                                                    engine.sequencer().journal().clone(),
                                                    engine.compensation_for(target_txn),
                                                    engine.redo_for(target_txn),
                                                ),
                                                "rand undo rejection atomicity",
                                            );
                                            cov.rejected_atomic += 1;
                                        }
                                        (Ok(receipt), Err(ref_err)) => {
                                            panic!(
                                                "undo oracle predicted {ref_err:?} but production committed {receipt:?}"
                                            );
                                        }
                                        (Err(err), Ok(rebased_inv)) => {
                                            panic!(
                                                "undo oracle predicted Ok({rebased_inv:?}) but production rejected with {err:?}"
                                            );
                                        }
                                    }
                                }
                            }
                            // 9: Redo an undone commit (supports both immediate and rebased redo)
                            9 => {
                                if let Some((owner, target_txn, comp_id)) = undone_targets.pop() {
                                    let redo_id = TxnId::new(owner, next_nonce());
                                    let comp_step = ref_ws
                                        .history
                                        .iter()
                                        .rev()
                                        .find(|h| h.txn == (owner.raw(), comp_id.nonce().raw()))
                                        .expect("ref undo step")
                                        .clone();
                                    let ref_rebase_result = ref_rebase_plan_from(
                                        &ref_ws.history,
                                        comp_step.committed_rev,
                                        &comp_step.inverse,
                                    );
                                    let before_redo = (
                                        engine.sequencer().snapshot(),
                                        engine.sequencer().history().clone(),
                                        engine.sequencer().snapshots().clone(),
                                        engine.sequencer().journal().clone(),
                                        engine.compensation_for(target_txn),
                                        engine.redo_for(target_txn),
                                        engine.redo_for(comp_id),
                                    );
                                    match (
                                        engine.redo(owner, target_txn, redo_id),
                                        ref_rebase_result,
                                    ) {
                                        (Ok(receipt), Ok(rebased_redo)) => {
                                            log.record_workspace(engine.sequencer(), receipt)
                                                .expect("log redo");
                                            session
                                                .advance(log.events().last().expect("ev"))
                                                .expect("adv redo");
                                            groups
                                                .project(SubmitOutcome::Committed(receipt), None)
                                                .expect("grp redo");
                                            ref_ws
                                                .commit_plan(
                                                    (owner.raw(), redo_id.nonce().raw()),
                                                    &rebased_redo,
                                                    false,
                                                )
                                                .expect("ref redo");
                                            assert_catalog_matches_ref(
                                                engine.sequencer().snapshot().catalog(),
                                                &ref_ws,
                                                "rand redo",
                                            );
                                            snapshots_by_rev.push(
                                                engine.sequencer().snapshot().catalog().clone(),
                                            );
                                            actor_txns.retain(|&(_, id)| id != target_txn);
                                            actor_txns.push((owner, redo_id));
                                            cov.redo_committed += 1;
                                        }
                                        (Err(err), Err(ref_err)) => {
                                            assert_eq!(ref_err, RefError::Contact);
                                            assert_eq!(
                                                err,
                                                RedoError::Rejected(RejectReason::Conflict)
                                            );
                                            assert_state_unchanged(
                                                &before_redo,
                                                &(
                                                    engine.sequencer().snapshot(),
                                                    engine.sequencer().history().clone(),
                                                    engine.sequencer().snapshots().clone(),
                                                    engine.sequencer().journal().clone(),
                                                    engine.compensation_for(target_txn),
                                                    engine.redo_for(target_txn),
                                                    engine.redo_for(comp_id),
                                                ),
                                                "rand redo rejection atomicity",
                                            );
                                            cov.rejected_atomic += 1;
                                        }
                                        (Ok(receipt), Err(ref_err)) => {
                                            panic!(
                                                "redo oracle predicted {ref_err:?} but production committed {receipt:?}"
                                            );
                                        }
                                        (Err(err), Ok(rebased_redo)) => {
                                            panic!(
                                                "redo oracle predicted Ok({rebased_redo:?}) but production rejected with {err:?}"
                                            );
                                        }
                                    }
                                }
                            }
                            // 10: Side-check interop change group (2 sequential edits inside one group)
                            10 => {
                                let mut ws_side = VersionedWorkspace::new(snap.catalog().clone());
                                let snap_side = ws_side.capture();
                                let (s1_a, e1_a) = random_valid_span(&mut rng, &cur_str);
                                let r1 = random_text(&mut rng, 2);
                                let mid_str =
                                    ref_apply_sequential_steps(&cur_str, &[(s1_a, e1_a, &r1)]);
                                let (s2_a, e2_a) = random_valid_span(&mut rng, &mid_str);
                                let r2 = random_text(&mut rng, 2);
                                let expected_str = ref_apply_sequential_steps(
                                    &cur_str,
                                    &[(s1_a, e1_a, &r1), (s2_a, e2_a, &r2)],
                                );
                                let cmds = [
                                    AgentCommand::BeginBatch,
                                    AgentCommand::Replace {
                                        start: AgentPosition::Byte(BytePos::new(s1_a)),
                                        end: AgentPosition::Byte(BytePos::new(e1_a)),
                                        text: Text::from_str(&r1),
                                    },
                                    AgentCommand::Replace {
                                        start: AgentPosition::Byte(BytePos::new(s2_a)),
                                        end: AgentPosition::Byte(BytePos::new(e2_a)),
                                        text: Text::from_str(&r2),
                                    },
                                    AgentCommand::EndBatch,
                                ];
                                let ctx = InteropContext { actor, buffer: buf };
                                let reqs =
                                    lower_agent(&snap_side, ctx, &cmds).expect("lower group");
                                apply_requests(
                                    &mut ws_side,
                                    &full_access_policy(),
                                    &default_actors(),
                                    &reqs,
                                )
                                .expect("apply group");
                                assert_eq!(
                                    ws_side.catalog().get(buf).expect("buf").as_str(),
                                    expected_str
                                );
                                cov.change_group_sidecheck += 1;
                            }
                            // 11: Side-check recorded_commands::fold homomorphism
                            11 => {
                                let mut ws_whole = VersionedWorkspace::new(snap.catalog().clone());
                                let mut ws_split = VersionedWorkspace::new(snap.catalog().clone());
                                let v_first =
                                    BufferView::full(ws_whole.catalog(), buf).expect("v0");
                                let p0 = cur_text.validate_offset(0).expect("0");
                                let r_a = random_text(&mut rng, 2);
                                let r_b = random_text(&mut rng, 2);
                                let after_first = Text::from_str(&format!("{r_a}{cur_str}"));
                                let v_second =
                                    BufferView::full(&catalog_for_single(buf, &after_first), buf)
                                        .expect("v1");
                                let seq_cmds = [
                                    RecordedCommand {
                                        intent: EditIntent::for_actor(
                                            actor,
                                            Nonce::new(1),
                                            Revision::ZERO,
                                        ),
                                        view: v_first,
                                        command: RecordedAction::Edit(EditCommand::Insert {
                                            at: p0,
                                            text: Text::from_str(&r_a),
                                        }),
                                    },
                                    RecordedCommand {
                                        intent: EditIntent::for_actor(
                                            actor,
                                            Nonce::new(2),
                                            Revision::new(1),
                                        ),
                                        view: v_second,
                                        command: RecordedAction::Edit(EditCommand::Insert {
                                            at: p0,
                                            text: Text::from_str(&r_b),
                                        }),
                                    },
                                ];
                                let whole = recorded_commands::fold(&mut ws_whole, &seq_cmds);
                                let p1 = recorded_commands::fold(&mut ws_split, &seq_cmds[..1]);
                                let p2 = recorded_commands::fold(&mut ws_split, &seq_cmds[1..]);
                                assert_eq!(whole.failure, None);
                                assert_eq!(p1.failure, None);
                                assert_eq!(p2.failure, None);
                                assert_eq!([p1.edits, p2.edits].concat(), whole.edits);
                                assert_eq!(ws_whole.revision(), ws_split.revision());
                                assert_eq!(ws_whole.catalog(), ws_split.catalog());
                                assert_eq!(
                                    ws_whole.catalog().get(buf).expect("buf").as_str(),
                                    format!("{r_b}{r_a}{cur_str}")
                                );
                                cov.recorded_fold_sidecheck += 1;
                            }
                            // 12: Deliberate invalid/rejected op + strict atomicity verification
                            _ => {
                                let before = (
                                    engine.sequencer().snapshot(),
                                    engine.sequencer().snapshots().clone(),
                                    engine.sequencer().journal().clone(),
                                    engine.sequencer().history().clone(),
                                    groups.clone(),
                                    capture_session_observation(
                                        &session,
                                        &[a1, a2],
                                        &[buf0, buf1, closed_buf],
                                        &["bm"],
                                        &overlay_keys,
                                        &[],
                                    ),
                                );
                                match rng.gen_range(0..4) {
                                    0 => {
                                        // Stale future revision submission.
                                        let bad = Transaction::new(
                                            EditIntent::for_actor(
                                                actor,
                                                next_nonce(),
                                                Revision::new(snap.revision().raw() + 5),
                                            ),
                                            vec![],
                                        )
                                        .expect("bad");
                                        assert_eq!(
                                            engine.submit(bad).reject_reason(),
                                            Some(RejectReason::RevisionMismatch)
                                        );
                                    }
                                    1 => {
                                        // Closed buffer submission.
                                        let bad = Transaction::new(
                                            EditIntent::for_actor(
                                                actor,
                                                next_nonce(),
                                                snap.revision(),
                                            ),
                                            vec![(closed_buf, Patch::default())],
                                        )
                                        .expect("bad");
                                        assert_eq!(
                                            engine.submit(bad).reject_reason(),
                                            Some(RejectReason::BufferClosed)
                                        );
                                    }
                                    2 => {
                                        // Unknown actor submission.
                                        let bad = Transaction::new(
                                            EditIntent::for_actor(
                                                ActorId::new(777),
                                                next_nonce(),
                                                snap.revision(),
                                            ),
                                            vec![],
                                        )
                                        .expect("bad");
                                        assert_eq!(
                                            engine.submit(bad).reject_reason(),
                                            Some(RejectReason::UnknownActor)
                                        );
                                    }
                                    _ => {
                                        // Out-of-bounds splice.
                                        let big = Text::from_str(&"x".repeat(cur_str.len() + 4));
                                        let oob = ByteSpan::validate_in(
                                            &big,
                                            cur_str.len() + 1,
                                            cur_str.len() + 2,
                                        )
                                        .expect("oob");
                                        let bad = Transaction::new(
                                            EditIntent::for_actor(
                                                actor,
                                                next_nonce(),
                                                snap.revision(),
                                            ),
                                            vec![(
                                                buf,
                                                Patch::new(vec![Splice::delete(oob)]).expect("p"),
                                            )],
                                        )
                                        .expect("bad");
                                        assert_eq!(
                                            engine.submit(bad).reject_reason(),
                                            Some(RejectReason::InvalidInput)
                                        );
                                    }
                                }
                                let after = (
                                    engine.sequencer().snapshot(),
                                    engine.sequencer().snapshots().clone(),
                                    engine.sequencer().journal().clone(),
                                    engine.sequencer().history().clone(),
                                    groups.clone(),
                                    capture_session_observation(
                                        &session,
                                        &[a1, a2],
                                        &[buf0, buf1, closed_buf],
                                        &["bm"],
                                        &overlay_keys,
                                        &[],
                                    ),
                                );
                                assert_state_unchanged(&before, &after, "rand rejection atomicity");
                                cov.rejected_atomic += 1;
                            }
                        }
                    }

                    // Verify full EditHistory replay, provenance & inverse restoration over the entire session!
                    let hist = engine.sequencer().history();
                    assert_eq!(hist.entries().len() + 1, snapshots_by_rev.len());
                    assert_eq!(hist.entries().len(), ref_ws.history.len());
                    let mut replay_cat = snapshots_by_rev[0].clone();
                    for (i, (entry, ref_step)) in
                        hist.entries().iter().zip(ref_ws.history.iter()).enumerate()
                    {
                        assert_eq!(entry.previous().raw(), ref_step.previous_rev);
                        assert_eq!(entry.committed().raw(), ref_step.committed_rev);
                        for (&id, expected_before) in &ref_step.before {
                            assert_eq!(
                                replay_cat.get(BufferId::new(id)).expect("buf").as_str(),
                                expected_before
                            );
                        }
                        replay_cat = entry.forward().apply_to_catalog(&replay_cat).expect("fwd");
                        assert_eq!(replay_cat, snapshots_by_rev[i + 1]);
                        for (&id, expected_after) in &ref_step.after {
                            assert_eq!(
                                replay_cat.get(BufferId::new(id)).expect("buf").as_str(),
                                expected_after
                            );
                        }
                    }
                    for (i, entry) in hist.entries().iter().enumerate().rev() {
                        replay_cat = entry.inverse().apply_to_catalog(&replay_cat).expect("inv");
                        assert_eq!(replay_cat, snapshots_by_rev[i]);
                    }
                    cov.history_slice_verified += 1;
                }
            },
        );

        // Release-enforced non-zero coverage assertions for EVERY branch.
        assert!(cov.cmd_single > 0, "cmd_single branch never hit");
        assert!(
            cov.workspace_multi_buf > 0,
            "workspace_multi_buf branch never hit"
        );
        assert!(cov.multi_cursor > 0, "multi_cursor branch never hit");
        assert!(cov.rectangle > 0, "rectangle branch never hit");
        assert!(cov.indent_region > 0, "indent_region branch never hit");
        assert!(cov.replace_all > 0, "replace_all branch never hit");
        assert!(cov.register_cut > 0, "register_cut branch never hit");
        assert!(cov.register_paste > 0, "register_paste branch never hit");
        assert!(cov.restore_path > 0, "restore_path branch never hit");
        assert!(
            cov.change_group_sidecheck > 0,
            "change_group_sidecheck branch never hit"
        );
        assert!(
            cov.recorded_fold_sidecheck > 0,
            "recorded_fold_sidecheck branch never hit"
        );
        assert!(cov.undo_committed > 0, "undo_committed branch never hit");
        assert!(cov.redo_committed > 0, "redo_committed branch never hit");
        assert!(cov.noop_submitted > 0, "noop_submitted branch never hit");
        assert!(cov.rejected_atomic > 0, "rejected_atomic branch never hit");
        assert!(
            cov.multibyte_utf8_edited > 0,
            "multibyte_utf8_edited branch never hit"
        );
        assert!(
            cov.crlf_or_cr_edited > 0,
            "crlf_or_cr_edited branch never hit"
        );
        assert!(
            cov.history_slice_verified > 0,
            "history_slice_verified branch never hit"
        );
    }

    // =========================================================================
    // Test 8: Randomized Disjoint Commutativity & Cross-Entry Equivalence
    // =========================================================================

    #[derive(Default, Debug)]
    struct CommuteCoverage {
        same_buffer_commuted: usize,
        multi_buffer_commuted: usize,
        session_projection_commuted: usize,
        indent_commuted: usize,
        rectangle_commuted: usize,
        change_group_matched_ref: usize,
        utf8_cases: usize,
        crlf_cases: usize,
    }

    #[test]
    fn randomized_disjoint_commutativity_and_cross_entry_equivalence() {
        let mut cov = CommuteCoverage::default();

        assert_law(
            &[0, 1, 7, 42, 999, u64::MAX],
            |rng: &mut Rng| rng.next_u64(),
            |&seed| {
                let mut rng = Rng::new(seed);
                let a1 = ActorId::new(1);
                let a2 = ActorId::new(2);
                let buf0 = BufferId::new(0);
                let buf1 = BufferId::new(1);
                let overlay_keys = [
                    AnnotationKey {
                        buffer: buf0,
                        namespace: NamespaceId::new(1),
                        id: AnnotationId::new(1),
                    },
                    AnnotationKey {
                        buffer: buf1,
                        namespace: NamespaceId::new(1),
                        id: AnnotationId::new(1),
                    },
                ];

                for _ in 0..20 {
                    let s0 = format!(
                        "a界{}\r\n{}😀z",
                        random_text(&mut rng, 6),
                        random_text(&mut rng, 5)
                    );
                    let s1 = format!(
                        " \t{}\n  {}",
                        random_text(&mut rng, 5),
                        random_text(&mut rng, 5)
                    );
                    if s0.bytes().any(|b| b >= 0x80) {
                        cov.utf8_cases += 1;
                    }
                    if s0.contains("\r\n") {
                        cov.crlf_cases += 1;
                    }

                    // 8a. Same-buffer disjoint pair: AB == BA == simultaneous == RefModel,
                    // plus WorkspaceSession projection commutativity!
                    if let Some(((a_s, a_e), (b_s, b_e))) = random_disjoint_pair(&mut rng, &s0) {
                        let ra = random_text(&mut rng, 2);
                        let rb = random_text(&mut rng, 2);
                        let plan_a = RefPlan {
                            buffers: BTreeMap::from([(
                                0,
                                vec![RefEdit {
                                    start: a_s,
                                    end: a_e,
                                    text: ra.clone(),
                                }],
                            )]),
                        };
                        let plan_b = RefPlan {
                            buffers: BTreeMap::from([(
                                0,
                                vec![RefEdit {
                                    start: b_s,
                                    end: b_e,
                                    text: rb.clone(),
                                }],
                            )]),
                        };
                        let ref_base = RefWorkspace::new(&[(0, &s0), (1, &s1)], &[]);
                        let merged = ref_compose_plans(&plan_a, &plan_b).expect("merge");
                        let mut ref_expected = ref_base.clone();
                        ref_expected
                            .commit_plan((1, 1), &merged, false)
                            .expect("ref merged");

                        let bounds0 = char_boundaries_of(&s0);
                        let anchor_pt = bounds0[rng.gen_range(0..bounds0.len())];
                        let head_pt = bounds0[rng.gen_range(0..bounds0.len())];

                        let run_seq = |first_a: ActorId,
                                       first_p: &RefPlan,
                                       second_a: ActorId,
                                       second_p: &RefPlan| {
                            let mut seq = Sequencer::new(ref_base.to_catalog(), default_actors());
                            let mut log = DeliveryLog::new(seq.snapshot());
                            let mut sess = WorkspaceSession::new(
                                seq.snapshot(),
                                default_actors(),
                                full_access_policy(),
                            );
                            populate_session_for_actor(&mut sess, a1, buf0, anchor_pt, head_pt);
                            let s_init = seq.snapshot();
                            let t1 = txn_from_ref(&s_init, first_a, 1, first_p);
                            let t2 = txn_from_ref(&s_init, second_a, 1, second_p);
                            for t in [t1, t2] {
                                if let Some(r) = seq.submit(t).committed_receipt() {
                                    log.record_workspace(&seq, r).expect("log");
                                    sess.advance(log.events().last().expect("ev")).expect("adv");
                                }
                            }
                            let obs = capture_session_observation(
                                &sess,
                                &[a1],
                                &[buf0, buf1],
                                &["bm"],
                                &overlay_keys,
                                &[],
                            );
                            (seq.snapshot().catalog().clone(), obs)
                        };

                        let (cat_ab, obs_ab) = run_seq(a1, &plan_a, a2, &plan_b);
                        let (cat_ba, obs_ba) = run_seq(a2, &plan_b, a1, &plan_a);
                        let snap0 = VersionedWorkspace::new(ref_base.to_catalog()).capture();
                        let t_sim = txn_from_ref(&snap0, a1, 9, &merged);
                        let cat_sim = t_sim
                            .apply_with_inverse(&snap0)
                            .expect("sim")
                            .catalog()
                            .clone();

                        assert_commutativity(
                            &cat_ab,
                            &cat_ba,
                            &cat_sim,
                            &ref_expected,
                            "rand same-buffer commute",
                        );
                        // Session projections commute whenever both edits actually committed
                        // (note: if one edit is a no-op, the other commits once and session matches too!).
                        assert_eq!(obs_ab.actors[&a1].texts, obs_ba.actors[&a1].texts);
                        assert_eq!(obs_ab.actors[&a1].views, obs_ba.actors[&a1].views);
                        assert_eq!(obs_ab.actors[&a1].selections, obs_ba.actors[&a1].selections);
                        assert_eq!(obs_ab.actors[&a1].bookmarks, obs_ba.actors[&a1].bookmarks);
                        assert_eq!(obs_ab.actors[&a1].overlays, obs_ba.actors[&a1].overlays);
                        cov.same_buffer_commuted += 1;
                        cov.session_projection_commuted += 1;

                        // Also cross-check Emacs/Neovim/Agent change group applying A then shifted B!
                        let b_shifted =
                            ref_transform_disjoint_single(&plan_b.buffers[&0], &plan_a.buffers[&0])
                                .expect("shift");
                        let ctx = InteropContext {
                            actor: a1,
                            buffer: buf0,
                        };
                        let ag_cmds = [
                            AgentCommand::BeginBatch,
                            AgentCommand::Replace {
                                start: AgentPosition::Byte(BytePos::new(a_s)),
                                end: AgentPosition::Byte(BytePos::new(a_e)),
                                text: Text::from_str(&ra),
                            },
                            AgentCommand::Replace {
                                start: AgentPosition::Byte(BytePos::new(b_shifted[0].start)),
                                end: AgentPosition::Byte(BytePos::new(b_shifted[0].end)),
                                text: Text::from_str(&rb),
                            },
                            AgentCommand::EndBatch,
                        ];
                        let mut ws_cg = VersionedWorkspace::new(ref_base.to_catalog());
                        let reqs = lower_agent(&ws_cg.capture(), ctx, &ag_cmds).expect("lower");
                        apply_requests(&mut ws_cg, &full_access_policy(), &default_actors(), &reqs)
                            .expect("apply");
                        assert_catalog_matches_ref(
                            ws_cg.catalog(),
                            &ref_expected,
                            "rand change group equivalence",
                        );
                        cov.change_group_matched_ref += 1;
                    }

                    // 8b. Multi-buffer disjoint plans commute.
                    {
                        let (s0_a, e0_a) = random_valid_span(&mut rng, &s0);
                        let (s1_a, e1_a) = random_valid_span(&mut rng, &s1);
                        let plan_a = RefPlan {
                            buffers: BTreeMap::from([(
                                0,
                                vec![RefEdit {
                                    start: s0_a,
                                    end: e0_a,
                                    text: random_text(&mut rng, 2),
                                }],
                            )]),
                        };
                        let plan_b = RefPlan {
                            buffers: BTreeMap::from([(
                                1,
                                vec![RefEdit {
                                    start: s1_a,
                                    end: e1_a,
                                    text: random_text(&mut rng, 2),
                                }],
                            )]),
                        };
                        let ref_base = RefWorkspace::new(&[(0, &s0), (1, &s1)], &[]);
                        let merged = ref_compose_plans(&plan_a, &plan_b).expect("merge");
                        let mut ref_expected = ref_base.clone();
                        ref_expected
                            .commit_plan((1, 1), &merged, false)
                            .expect("ref");

                        let snap0 = VersionedWorkspace::new(ref_base.to_catalog()).capture();
                        let t_a = txn_from_ref(&snap0, a1, 1, &plan_a);
                        let t_b = txn_from_ref(&snap0, a1, 2, &plan_b);
                        let we_a = WorkspaceEdit::new(
                            a1,
                            snap0.revision(),
                            t_a.patches().to_vec(),
                            &snap0,
                        )
                        .expect("we a");
                        let we_b = WorkspaceEdit::new(
                            a1,
                            snap0.revision(),
                            t_b.patches().to_vec(),
                            &snap0,
                        )
                        .expect("we b");
                        assert_eq!(we_a.compose(&we_b), we_b.compose(&we_a));
                        let sim_cat = we_a
                            .compose(&we_b)
                            .expect("compose")
                            .apply(Nonce::new(3), &snap0)
                            .expect("apply")
                            .catalog()
                            .clone();
                        let ab_cat = t_b
                            .apply_to_catalog(t_a.apply_with_inverse(&snap0).expect("a").catalog())
                            .expect("b after a");
                        let ba_cat = t_a
                            .apply_to_catalog(t_b.apply_with_inverse(&snap0).expect("b").catalog())
                            .expect("a after b");
                        assert_commutativity(
                            &ab_cat,
                            &ba_cat,
                            &sim_cat,
                            &ref_expected,
                            "rand multi-buffer commute",
                        );
                        cov.multi_buffer_commuted += 1;
                    }

                    // 8c. Disjoint indent regions & rectangle row bands commute.
                    {
                        let lines = ref_line_ranges(&s1);
                        if lines.len() >= 2 {
                            let mid = lines.len() / 2;
                            let text1 = Text::from_str(&s1);
                            let pol_top = IndentPolicy {
                                column: rng.gen_range(0..5),
                                tab_width: 4,
                                style: IndentStyle::Spaces,
                            };
                            let pol_bot = IndentPolicy {
                                column: rng.gen_range(0..6),
                                tab_width: 4,
                                style: IndentStyle::TabsAndSpaces,
                            };
                            let top_p = indent_region::plan(&text1, 0..mid, pol_top).expect("top");
                            let bot_p = indent_region::plan(&text1, mid..lines.len(), pol_bot)
                                .expect("bot");
                            let t_top = top_p.apply(&text1).expect("apply top");
                            let ab_ind = indent_region::plan(&t_top, mid..lines.len(), pol_bot)
                                .expect("bot on top")
                                .apply(&t_top)
                                .expect("apply");
                            let t_bot = bot_p.apply(&text1).expect("apply bot");
                            let ba_ind = indent_region::plan(&t_bot, 0..mid, pol_top)
                                .expect("top on bot")
                                .apply(&t_bot)
                                .expect("apply");
                            assert_eq!(ab_ind, ba_ind);
                            let mut ref_ind =
                                ref_indent_region_edits(&s1, 0..mid, pol_top).expect("ref top");
                            ref_ind.extend(
                                ref_indent_region_edits(&s1, mid..lines.len(), pol_bot)
                                    .expect("ref bot"),
                            );
                            let (expected_ind, _) =
                                ref_apply_single(&s1, &ref_ind).expect("ref apply ind");
                            assert_eq!(ab_ind.as_str(), expected_ind);
                            cov.indent_commuted += 1;

                            // Disjoint row-band rectangle edits commute!
                            let rect_top = Rectangle {
                                first: VisualPosition { line: 0, column: 0 },
                                second: VisualPosition {
                                    line: mid - 1,
                                    column: 2,
                                },
                                endpoint: Endpoint::Exclusive,
                                tab_width: 4,
                            };
                            let rect_bot = Rectangle {
                                first: VisualPosition {
                                    line: mid,
                                    column: 1,
                                },
                                second: VisualPosition {
                                    line: lines.len() - 1,
                                    column: 3,
                                },
                                endpoint: Endpoint::Inclusive,
                                tab_width: 4,
                            };
                            let rp_top = rectangle_edit::plan_patch(
                                &text1,
                                rect_top,
                                ShortLinePolicy::Clamp,
                                RectangleOperation::Uniform("T"),
                            )
                            .expect("rp top");
                            let rp_bot = rectangle_edit::plan_patch(
                                &text1,
                                rect_bot,
                                ShortLinePolicy::Clamp,
                                RectangleOperation::Insert("B"),
                            )
                            .expect("rp bot");
                            let rt = rp_top.apply(&text1).expect("rt");
                            let ab_rect = rectangle_edit::plan_patch(
                                &rt,
                                rect_bot,
                                ShortLinePolicy::Clamp,
                                RectangleOperation::Insert("B"),
                            )
                            .expect("rb on rt")
                            .apply(&rt)
                            .expect("apply");
                            let rb = rp_bot.apply(&text1).expect("rb");
                            let ba_rect = rectangle_edit::plan_patch(
                                &rb,
                                rect_top,
                                ShortLinePolicy::Clamp,
                                RectangleOperation::Uniform("T"),
                            )
                            .expect("rt on rb")
                            .apply(&rb)
                            .expect("apply");
                            assert_eq!(ab_rect, ba_rect);
                            let mut ref_rect = ref_rectangle_edits(
                                &s1,
                                RefRectSpec {
                                    first_line: 0,
                                    first_col: 0,
                                    second_line: mid - 1,
                                    second_col: 2,
                                    inclusive: false,
                                    tab_width: 4,
                                    clamp_short: true,
                                    op: RectangleOperation::Uniform("T"),
                                },
                            )
                            .expect("ref rect top");
                            ref_rect.extend(
                                ref_rectangle_edits(
                                    &s1,
                                    RefRectSpec {
                                        first_line: mid,
                                        first_col: 1,
                                        second_line: lines.len() - 1,
                                        second_col: 3,
                                        inclusive: true,
                                        tab_width: 4,
                                        clamp_short: true,
                                        op: RectangleOperation::Insert("B"),
                                    },
                                )
                                .expect("ref rect bot"),
                            );
                            let (expected_rect, _) =
                                ref_apply_single(&s1, &ref_rect).expect("ref apply rect");
                            assert_eq!(ab_rect.as_str(), expected_rect);
                            cov.rectangle_commuted += 1;
                        }
                    }
                }
            },
        );

        assert!(cov.same_buffer_commuted > 0, "same_buffer_commuted == 0");
        assert!(cov.multi_buffer_commuted > 0, "multi_buffer_commuted == 0");
        assert!(
            cov.session_projection_commuted > 0,
            "session_projection_commuted == 0"
        );
        assert!(cov.indent_commuted > 0, "indent_commuted == 0");
        assert!(cov.rectangle_commuted > 0, "rectangle_commuted == 0");
        assert!(
            cov.change_group_matched_ref > 0,
            "change_group_matched_ref == 0"
        );
        assert!(cov.utf8_cases > 0, "utf8_cases == 0");
        assert!(cov.crlf_cases > 0, "crlf_cases == 0");
    }
}
