//! Cross-cutting workspace law suite.
//!
//! Verifies private-state isolation, shared-text invariance, event-prefix
//! replay equivalence (from initial state and checkpoints/snapshots), immediate
//! close and revocation propagation with non-interference, and failure
//! atomicity against an independent standard-library shadow workspace model.

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use crate::editor::{
        collab::{
            access::{AccessError, AccessPolicy, Capability},
            checkpoints::{Checkpoint, CheckpointError, CheckpointOp, CheckpointState},
            delivery::{replay as replay_delivery, CatchUp, DeliveryEvent, DeliveryLog},
            proposal::RequestId,
            receipt::{RejectReason, SubmitOutcome},
            registry::{ActorRegistry, RegistryError},
            role::ActorRole,
            sequencer::Sequencer,
        },
        edit::{
            ids::{ActorId, BufferId, Nonce},
            intent::EditIntent,
            patch::{Patch, PatchError},
            splice::Splice,
            transaction::{Transaction, TransactionError},
        },
        test_support::{assert_law, Rng},
        text::{
            anchors::{Affinity, Anchor, ReplacementEffect},
            clip::ClipError,
            position_map::{ByteReplacement, PositionMap},
            search::SearchError,
            span::{ByteSpan, SpanError},
            utf8::{ByteOffset, Text, Utf8Error},
        },
        workspace::{
            annotation_mapping::{
                map_annotation, map_store, AnnotationMappingError, MappingRevisions, RevisionedMap,
            },
            annotations::{
                Annotation, AnnotationError, AnnotationId, AnnotationKey, AnnotationStore,
                CollapsePolicy, NamespaceId,
            },
            bookmarks::{
                Bookmark, BookmarkError, BookmarkKey, BookmarkName, BookmarkStep, BookmarkStore,
            },
            buffer_lifecycle::{
                BufferLifecycle, ClosePolicy, LifecycleError, LifecycleEvent, LifecycleKind,
            },
            catalog::{BufferCatalog, CatalogError},
            context::{
                extract_context, extract_session_context, extract_shared_context,
                extract_shared_session_context, extract_workspace_context, BudgetError,
                BufferScope, ContextBudget, ContextError, ContextPack, ContextRequest,
                ContextScope, ContextTarget, ParticipantScope, ScopeMode,
            },
            dirty::{DirtyTracker, SaveError, SaveReceipt},
            events::{submit_with_event, WorkspaceEvent},
            properties::{project_properties, properties_at, AnnotationProperties, PropertyMap},
            registers::{
                KillRing, KillRings, RegisterEntry, RegisterKind, RegisterName, RegisterStore,
            },
            search::{search_buffers, search_workspace, WorkspaceSearchError},
            selections::{ActorSelections, Direction, Selection, SelectionError},
            session::{SessionError, WorkspaceSession},
            snapshots::{
                SnapshotError, SnapshotStore, VersionedWorkspace, WorkspaceError, WorkspaceSnapshot,
            },
            version::{Revision, RevisionOverflow},
            view::{BufferView, ViewError},
            view_mapping::{
                clip_selection, map_and_clip_selection, map_selection, map_view, ViewMappingError,
            },
        },
    };

    const SEEDS: &[u64] = &[0, 1, 42, u64::MAX];
    const FRAGMENTS: &[&str] = &["", "a", "ab", "x", "\n", "\r\n", "é", "界", "😀", "a界"];
    const PROBE_PATTERNS: &[&str] = &["a", "aa", "界", "😀", "\n", "SECRET_TOKEN"];

    // -------------------------------------------------------------------------
    // Independent shadow/reference workspace model (plain std types only)
    // -------------------------------------------------------------------------

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum ShadowRole {
        Human,
        Agent,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    enum ShadowCap {
        Read,
        Propose,
        Review,
        Commit,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ShadowAffinity {
        Left,
        Right,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ShadowCollapse {
        Evict,
        RetainEmpty,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ShadowRegKind {
        Charwise,
        Linewise,
        Blockwise,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ShadowBuffer {
        text: String,
        saved_text: Option<String>,
        saved_rev: Option<u64>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct ShadowSelection {
        anchor: usize,
        head: usize,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct ShadowBookmark {
        buffer: u64,
        offset: usize,
        affinity: ShadowAffinity,
        revision: u64,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ShadowOverlay {
        buffer: u64,
        namespace: u64,
        id: u64,
        owner: u64,
        start: usize,
        end: usize,
        start_affinity: ShadowAffinity,
        end_affinity: ShadowAffinity,
        priority: u32,
        collapse: ShadowCollapse,
        label: String,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ShadowRegister {
        text: String,
        kind: ShadowRegKind,
    }

    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    struct ShadowActorPrivate {
        views: BTreeMap<u64, (usize, usize)>,
        selections: BTreeMap<u64, ShadowSelection>,
        bookmarks: BTreeMap<String, ShadowBookmark>,
        overlays: BTreeMap<(u64, u64, u64), ShadowOverlay>,
        overlay_props: BTreeMap<(u64, u64), BTreeMap<u64, BTreeMap<String, String>>>,
        registers: BTreeMap<String, ShadowRegister>,
        kill_ring: Vec<ShadowRegister>,
        kill_cursor: usize,
    }

    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    struct ShadowPolicy {
        defaults: BTreeSet<ShadowCap>,
        actor_caps: BTreeSet<(u64, ShadowCap)>,
        buffer_caps: BTreeSet<(u64, u64, ShadowCap)>,
    }

    impl ShadowPolicy {
        fn grant_default(&mut self, cap: ShadowCap) {
            self.defaults.insert(cap);
        }

        fn revoke_default(&mut self, cap: ShadowCap) {
            self.defaults.remove(&cap);
        }

        fn grant_actor(&mut self, actor: u64, cap: ShadowCap) {
            self.actor_caps.insert((actor, cap));
        }

        fn grant_buffer(&mut self, actor: u64, buffer: u64, cap: ShadowCap) {
            self.buffer_caps.insert((actor, buffer, cap));
        }

        fn is_granted(&self, actor: u64, buffer: u64, cap: ShadowCap) -> bool {
            self.defaults.contains(&cap)
                || self.actor_caps.contains(&(actor, cap))
                || self.buffer_caps.contains(&(actor, buffer, cap))
        }

        fn to_access_policy(&self) -> AccessPolicy {
            let mut policy = AccessPolicy::new();
            for &cap in &self.defaults {
                policy.grant_default(to_prod_cap(cap));
            }
            for &(actor, cap) in &self.actor_caps {
                policy.grant_actor(ActorId::new(actor), to_prod_cap(cap));
            }
            for &(actor, buffer, cap) in &self.buffer_caps {
                policy.grant_buffer(ActorId::new(actor), BufferId::new(buffer), to_prod_cap(cap));
            }
            policy
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ShadowSplice {
        start: usize,
        end: usize,
        replacement: String,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum ShadowEvent {
        Open {
            buffer: u64,
            text: String,
            clean_on_open: bool,
        },
        Close {
            buffer: u64,
        },
        Edit {
            patches: Vec<(u64, Vec<ShadowSplice>)>,
        },
        Save {
            buffer: u64,
        },
        SetPolicy {
            policy: ShadowPolicy,
        },
        SetView {
            actor: u64,
            buffer: u64,
            start: usize,
            end: usize,
        },
        SetSelection {
            actor: u64,
            buffer: u64,
            anchor: usize,
            head: usize,
        },
        SetBookmark {
            actor: u64,
            name: String,
            buffer: u64,
            offset: usize,
            affinity: ShadowAffinity,
        },
        SetOverlay {
            actor: u64,
            overlay: ShadowOverlay,
        },
        SetOverlayProps {
            actor: u64,
            buffer: u64,
            namespace: u64,
            id: u64,
            props: BTreeMap<String, String>,
        },
        PutRegister {
            actor: u64,
            name: String,
            entry: ShadowRegister,
        },
        PushKill {
            actor: u64,
            entry: ShadowRegister,
        },
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ShadowWorkspace {
        revision: u64,
        live: BTreeMap<u64, ShadowBuffer>,
        tombstones: BTreeSet<u64>,
        active_actors: BTreeMap<u64, ShadowRole>,
        revoked_actors: BTreeSet<u64>,
        policy: ShadowPolicy,
        private: BTreeMap<u64, ShadowActorPrivate>,
        events: Vec<ShadowEvent>,
    }

    fn transport_offset(
        pos: usize,
        start: usize,
        end: usize,
        inserted_len: usize,
        affinity: ShadowAffinity,
    ) -> usize {
        if pos < start {
            pos
        } else if pos > end {
            pos - (end - start) + inserted_len
        } else if matches!(affinity, ShadowAffinity::Left) {
            start
        } else {
            start + inserted_len
        }
    }

    fn apply_splices_str(source: &str, splices: &[ShadowSplice]) -> String {
        let mut out = source.to_owned();
        for splice in splices.iter().rev() {
            assert!(
                splice.start <= splice.end
                    && splice.end <= out.len()
                    && out.is_char_boundary(splice.start)
                    && out.is_char_boundary(splice.end),
                "invalid splice bounds in shadow model"
            );
            out.replace_range(splice.start..splice.end, &splice.replacement);
        }
        out
    }

    fn shadow_min_diff(before: &str, after: &str) -> Vec<ShadowSplice> {
        if before == after {
            return Vec::new();
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
        vec![ShadowSplice {
            start: prefix,
            end: before.len() - suffix,
            replacement: after[prefix..after.len() - suffix].to_owned(),
        }]
    }

    fn merge_raw_spans(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
        spans.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (start, end) in spans {
            if let Some(last) = merged.last_mut() {
                if start <= last.1 {
                    last.1 = last.1.max(end);
                    continue;
                }
            }
            merged.push((start, end));
        }
        merged
    }

    fn intersect_raw_spans(
        left: &[(usize, usize)],
        right: &[(usize, usize)],
    ) -> Vec<(usize, usize)> {
        let mut raw = Vec::new();
        for &(ls, le) in left {
            for &(rs, re) in right {
                let s = ls.max(rs);
                let e = le.min(re);
                if s <= e {
                    raw.push((s, e));
                }
            }
        }
        merge_raw_spans(raw)
    }

    fn truncate_utf8_prefix(slice: &str, max_bytes: usize) -> (&str, bool) {
        let mut end = max_bytes.min(slice.len());
        while !slice.is_char_boundary(end) {
            end -= 1;
        }
        (&slice[..end], end < slice.len())
    }

    impl ShadowWorkspace {
        fn new(
            initial_buffers: &[(u64, &str, bool)],
            actors: &[(u64, ShadowRole)],
            policy: ShadowPolicy,
        ) -> Self {
            let mut live = BTreeMap::new();
            for &(id, text, clean) in initial_buffers {
                live.insert(
                    id,
                    ShadowBuffer {
                        text: text.to_owned(),
                        saved_text: clean.then(|| text.to_owned()),
                        saved_rev: clean.then_some(0),
                    },
                );
            }
            let mut active_actors = BTreeMap::new();
            let mut private = BTreeMap::new();
            for &(actor, role) in actors {
                active_actors.insert(actor, role);
                private.insert(actor, ShadowActorPrivate::default());
            }
            Self {
                revision: 0,
                live,
                tombstones: BTreeSet::new(),
                active_actors,
                revoked_actors: BTreeSet::new(),
                policy,
                private,
                events: Vec::new(),
            }
        }

        fn is_active(&self, actor: u64) -> bool {
            self.active_actors.contains_key(&actor)
        }

        fn auth_read(&self, actor: u64, buffer: u64) -> Result<&str, ObsAccessErr> {
            if !self.is_active(actor) {
                return Err(ObsAccessErr::InactiveActor(actor));
            }
            let Some(buf) = self.live.get(&buffer) else {
                return Err(if self.tombstones.contains(&buffer) {
                    ObsAccessErr::BufferClosed(buffer)
                } else {
                    ObsAccessErr::UnknownBuffer(buffer)
                });
            };
            if !self.policy.is_granted(actor, buffer, ShadowCap::Read) {
                return Err(ObsAccessErr::Denied { actor, buffer });
            }
            Ok(&buf.text)
        }

        fn is_dirty(&self, buffer: u64) -> Option<bool> {
            self.live
                .get(&buffer)
                .map(|buf| buf.saved_text.as_deref() != Some(buf.text.as_str()))
        }

        fn revoke_actor(&mut self, actor: u64) {
            if self.active_actors.remove(&actor).is_some() {
                self.revoked_actors.insert(actor);
            }
        }

        fn apply_event(&mut self, event: ShadowEvent) {
            match &event {
                ShadowEvent::Open {
                    buffer,
                    text,
                    clean_on_open,
                } => {
                    assert!(
                        !self.live.contains_key(buffer) && !self.tombstones.contains(buffer),
                        "shadow open requires fresh buffer id"
                    );
                    self.revision += 1;
                    self.live.insert(
                        *buffer,
                        ShadowBuffer {
                            text: text.clone(),
                            saved_text: clean_on_open.then(|| text.clone()),
                            saved_rev: clean_on_open.then_some(self.revision),
                        },
                    );
                    for state in self.private.values_mut() {
                        for bm in state.bookmarks.values_mut() {
                            bm.revision = self.revision;
                        }
                    }
                }
                ShadowEvent::Close { buffer } => {
                    assert!(
                        self.live.remove(buffer).is_some(),
                        "shadow close requires live buffer"
                    );
                    self.tombstones.insert(*buffer);
                    self.revision += 1;
                    for state in self.private.values_mut() {
                        state.views.remove(buffer);
                        state.selections.remove(buffer);
                        state.bookmarks.retain(|_, bm| bm.buffer != *buffer);
                        state.overlays.retain(|k, _| k.0 != *buffer);
                        for bm in state.bookmarks.values_mut() {
                            bm.revision = self.revision;
                        }
                    }
                }
                ShadowEvent::Edit { patches } => {
                    self.revision += 1;
                    let mut patch_map: BTreeMap<u64, &[ShadowSplice]> = BTreeMap::new();
                    for (buffer, splices) in patches {
                        let buf = self.live.get_mut(buffer).expect("edited buffer is live");
                        buf.text = apply_splices_str(&buf.text, splices);
                        patch_map.insert(*buffer, splices.as_slice());
                    }
                    for state in self.private.values_mut() {
                        for bm in state.bookmarks.values_mut() {
                            if let Some(splices) = patch_map.get(&bm.buffer) {
                                for s in splices.iter().rev() {
                                    if s.start == s.end && s.replacement.is_empty() {
                                        continue;
                                    }
                                    bm.offset = transport_offset(
                                        bm.offset,
                                        s.start,
                                        s.end,
                                        s.replacement.len(),
                                        bm.affinity,
                                    );
                                }
                            }
                            bm.revision = self.revision;
                        }
                        for (&buffer, splices) in &patch_map {
                            if let Some(view) = state.views.get_mut(&buffer) {
                                for s in splices.iter().rev() {
                                    if s.start == s.end && s.replacement.is_empty() {
                                        continue;
                                    }
                                    view.0 = transport_offset(
                                        view.0,
                                        s.start,
                                        s.end,
                                        s.replacement.len(),
                                        ShadowAffinity::Left,
                                    );
                                    view.1 = transport_offset(
                                        view.1,
                                        s.start,
                                        s.end,
                                        s.replacement.len(),
                                        ShadowAffinity::Right,
                                    );
                                }
                            }
                            if let Some(sel) = state.selections.get_mut(&buffer) {
                                for s in splices.iter().rev() {
                                    if s.start == s.end && s.replacement.is_empty() {
                                        continue;
                                    }
                                    sel.anchor = transport_offset(
                                        sel.anchor,
                                        s.start,
                                        s.end,
                                        s.replacement.len(),
                                        ShadowAffinity::Right,
                                    );
                                    sel.head = transport_offset(
                                        sel.head,
                                        s.start,
                                        s.end,
                                        s.replacement.len(),
                                        ShadowAffinity::Right,
                                    );
                                }
                                let view = state.views[&buffer];
                                sel.anchor = sel.anchor.clamp(view.0, view.1);
                                sel.head = sel.head.clamp(view.0, view.1);
                            }
                            let keys: Vec<(u64, u64, u64)> = state
                                .overlays
                                .keys()
                                .copied()
                                .filter(|k| k.0 == buffer)
                                .collect();
                            for key in keys {
                                for s in splices.iter().rev() {
                                    if s.start == s.end && s.replacement.is_empty() {
                                        continue;
                                    }
                                    let Some(ov) = state.overlays.get_mut(&key) else {
                                        break;
                                    };
                                    let ns = transport_offset(
                                        ov.start,
                                        s.start,
                                        s.end,
                                        s.replacement.len(),
                                        ov.start_affinity,
                                    );
                                    let ne = transport_offset(
                                        ov.end,
                                        s.start,
                                        s.end,
                                        s.replacement.len(),
                                        ov.end_affinity,
                                    );
                                    if ns < ne {
                                        ov.start = ns;
                                        ov.end = ne;
                                    } else {
                                        match ov.collapse {
                                            ShadowCollapse::Evict => {
                                                state.overlays.remove(&key);
                                            }
                                            ShadowCollapse::RetainEmpty => {
                                                ov.start = ns;
                                                ov.end = ns;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                ShadowEvent::Save { buffer } => {
                    let buf = self.live.get_mut(buffer).expect("saved buffer is live");
                    buf.saved_text = Some(buf.text.clone());
                    buf.saved_rev = Some(self.revision);
                }
                ShadowEvent::SetPolicy { policy } => {
                    self.policy = policy.clone();
                    let active = &self.active_actors;
                    let live = &self.live;
                    let pol = &self.policy;
                    for (&actor, state) in &mut self.private {
                        let allowed = |buf: u64| {
                            active.contains_key(&actor)
                                && live.contains_key(&buf)
                                && pol.is_granted(actor, buf, ShadowCap::Read)
                        };
                        state.views.retain(|&buf, _| allowed(buf));
                        state.selections.retain(|&buf, _| allowed(buf));
                        state.bookmarks.retain(|_, bm| allowed(bm.buffer));
                        state.overlays.retain(|k, _| allowed(k.0));
                    }
                }
                ShadowEvent::SetView {
                    actor,
                    buffer,
                    start,
                    end,
                } => {
                    self.private
                        .get_mut(actor)
                        .expect("active actor")
                        .views
                        .insert(*buffer, (*start, *end));
                }
                ShadowEvent::SetSelection {
                    actor,
                    buffer,
                    anchor,
                    head,
                } => {
                    self.private
                        .get_mut(actor)
                        .expect("active actor")
                        .selections
                        .insert(
                            *buffer,
                            ShadowSelection {
                                anchor: *anchor,
                                head: *head,
                            },
                        );
                }
                ShadowEvent::SetBookmark {
                    actor,
                    name,
                    buffer,
                    offset,
                    affinity,
                } => {
                    self.private
                        .get_mut(actor)
                        .expect("active actor")
                        .bookmarks
                        .insert(
                            name.clone(),
                            ShadowBookmark {
                                buffer: *buffer,
                                offset: *offset,
                                affinity: *affinity,
                                revision: self.revision,
                            },
                        );
                }
                ShadowEvent::SetOverlay { actor, overlay } => {
                    let key = (overlay.buffer, overlay.namespace, overlay.id);
                    self.private
                        .get_mut(actor)
                        .expect("active actor")
                        .overlays
                        .insert(key, overlay.clone());
                }
                ShadowEvent::SetOverlayProps {
                    actor,
                    buffer,
                    namespace,
                    id,
                    props,
                } => {
                    self.private
                        .get_mut(actor)
                        .expect("active actor")
                        .overlay_props
                        .entry((*buffer, *namespace))
                        .or_default()
                        .insert(*id, props.clone());
                }
                ShadowEvent::PutRegister { actor, name, entry } => {
                    self.private
                        .get_mut(actor)
                        .expect("active actor")
                        .registers
                        .insert(name.clone(), entry.clone());
                }
                ShadowEvent::PushKill { actor, entry } => {
                    let state = self.private.get_mut(actor).expect("active actor");
                    state.kill_ring.insert(0, entry.clone());
                    state.kill_cursor = 0;
                }
            }
            self.events.push(event);
        }

        fn replay_prefix(initial: &Self, prefix: &[ShadowEvent]) -> Self {
            let mut copy = initial.clone();
            copy.events.clear();
            for event in prefix {
                copy.apply_event(event.clone());
            }
            copy
        }

        fn search_buffers(
            &self,
            actor: u64,
            buffers: &[u64],
            pattern: &str,
        ) -> Result<Vec<(u64, u64, usize, usize)>, ObsAccessErr> {
            assert!(!pattern.is_empty(), "shadow probe patterns are nonempty");
            if !self.is_active(actor) {
                return Err(ObsAccessErr::InactiveActor(actor));
            }
            let dedup: BTreeSet<u64> = buffers.iter().copied().collect();
            let mut matches = Vec::new();
            for buffer in dedup {
                if let Ok(text) = self.auth_read(actor, buffer) {
                    for (idx, _) in text.char_indices() {
                        if text[idx..].starts_with(pattern) {
                            matches.push((self.revision, buffer, idx, idx + pattern.len()));
                        }
                    }
                }
            }
            Ok(matches)
        }

        fn search_workspace(
            &self,
            actor: u64,
            pattern: &str,
        ) -> Result<Vec<(u64, u64, usize, usize)>, ObsAccessErr> {
            let live_ids: Vec<u64> = self.live.keys().copied().collect();
            self.search_buffers(actor, &live_ids, pattern)
        }

        fn resolve_props_at(
            &self,
            actor: u64,
            buffer: u64,
            namespace: u64,
            offset: usize,
        ) -> BTreeMap<String, String> {
            let Some(state) = self.private.get(&actor) else {
                return BTreeMap::new();
            };
            let Some(prop_table) = state.overlay_props.get(&(buffer, namespace)) else {
                return BTreeMap::new();
            };
            let mut winners: BTreeMap<String, ((u32, u64), String)> = BTreeMap::new();
            for ov in state.overlays.values() {
                if ov.buffer != buffer || ov.namespace != namespace {
                    continue;
                }
                let covers = if ov.start == ov.end {
                    ov.start == offset
                } else {
                    ov.start <= offset && offset < ov.end
                };
                if !covers {
                    continue;
                }
                if let Some(kv) = prop_table.get(&ov.id) {
                    let rank = (ov.priority, ov.id);
                    for (k, v) in kv {
                        winners
                            .entry(k.clone())
                            .and_modify(|existing| {
                                if rank > existing.0 {
                                    *existing = (rank, v.clone());
                                }
                            })
                            .or_insert_with(|| (rank, v.clone()));
                    }
                }
            }
            winners.into_iter().map(|(k, (_, v))| (k, v)).collect()
        }

        fn project_properties(
            &self,
            actor: u64,
            buffer: u64,
            namespace: u64,
        ) -> Vec<(usize, usize, BTreeMap<String, String>)> {
            let Ok(text) = self.auth_read(actor, buffer) else {
                return Vec::new();
            };
            let boundaries: Vec<usize> = text
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(text.len()))
                .collect();
            let mut segments: Vec<(usize, usize, BTreeMap<String, String>)> = Vec::new();
            for pair in boundaries.windows(2) {
                let vals = self.resolve_props_at(actor, buffer, namespace, pair[0]);
                if let Some(last) = segments.last_mut().filter(|last| last.2 == vals) {
                    last.1 = pair[1];
                } else {
                    segments.push((pair[0], pair[1], vals));
                }
            }
            segments
        }

        fn extract_filter_context(
            &self,
            actor: u64,
            scoped_buffers: Option<&[u64]>,
            use_session_views: bool,
            budget: ContextBudget,
        ) -> Result<ObsContextPack, ObsContextErr> {
            if !self.is_active(actor) {
                return Err(ObsContextErr::Access(ObsAccessErr::InactiveActor(actor)));
            }
            let buffers: Vec<u64> = match scoped_buffers {
                Some(list) => list
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
                None => self.live.keys().copied().collect(),
            };
            let mut candidates: Vec<(u64, &str, usize, usize)> = Vec::new();
            for buffer in buffers {
                let Ok(text) = self.auth_read(actor, buffer) else {
                    continue;
                };
                let spans = if use_session_views {
                    if let Some(&(vs, ve)) =
                        self.private.get(&actor).and_then(|s| s.views.get(&buffer))
                    {
                        intersect_raw_spans(&[(0, text.len())], &[(vs, ve)])
                    } else {
                        vec![(0, text.len())]
                    }
                } else {
                    vec![(0, text.len())]
                };
                for (s, e) in merge_raw_spans(spans) {
                    candidates.push((buffer, text, s, e));
                }
            }
            let per_frag = budget.max_fragment_bytes.unwrap_or(usize::MAX);
            let mut remaining = budget.max_total_bytes;
            let mut total_bytes = 0usize;
            let mut truncated = false;
            let mut omitted_fragments = 0usize;
            let mut fragments = Vec::new();
            for (index, &(buffer, text, start, end)) in candidates.iter().enumerate() {
                if budget
                    .max_fragments
                    .is_some_and(|limit| fragments.len() >= limit)
                {
                    omitted_fragments += candidates.len() - index;
                    truncated = true;
                    break;
                }
                if start == end {
                    fragments.push(ObsContextFragment {
                        revision: self.revision,
                        buffer,
                        start,
                        end,
                        text: String::new(),
                        truncated: false,
                        omitted_bytes: 0,
                    });
                    continue;
                }
                let slice = &text[start..end];
                let limit = remaining.min(per_frag);
                let (emitted, frag_trunc) = truncate_utf8_prefix(slice, limit);
                let emitted_len = emitted.len();
                let omitted_bytes = slice.len() - emitted_len;
                let blocked_by_total = if frag_trunc {
                    let next_len = slice[emitted_len..]
                        .chars()
                        .next()
                        .expect("truncated slice has next scalar")
                        .len_utf8();
                    let needed = emitted_len + next_len;
                    needed > remaining && needed <= per_frag
                } else {
                    false
                };
                if frag_trunc {
                    truncated = true;
                }
                if emitted_len == 0 {
                    omitted_fragments += 1;
                } else {
                    fragments.push(ObsContextFragment {
                        revision: self.revision,
                        buffer,
                        start,
                        end: start + emitted_len,
                        text: emitted.to_owned(),
                        truncated: frag_trunc,
                        omitted_bytes,
                    });
                    total_bytes += emitted_len;
                    remaining -= emitted_len;
                }
                if blocked_by_total {
                    omitted_fragments += candidates.len() - (index + 1);
                    break;
                }
            }
            Ok(ObsContextPack {
                revision: self.revision,
                participants: vec![actor],
                fragments,
                total_bytes,
                truncated,
                omitted_fragments,
            })
        }

        fn observe(&self, probe: &ObservationProbe) -> WorkspaceObservation {
            let live_ids: Vec<u64> = self.live.keys().copied().collect();
            let tombstoned_ids: Vec<u64> = self.tombstones.iter().copied().collect();
            let buffers = self
                .live
                .iter()
                .map(|(&id, buf)| {
                    (
                        id,
                        (
                            buf.text.clone(),
                            self.is_dirty(id).expect("live buffer has dirty state"),
                        ),
                    )
                })
                .collect();
            let mut actors = BTreeMap::new();
            for &actor in &probe.actors {
                let mut reads = BTreeMap::new();
                let mut views = BTreeMap::new();
                let mut selections = BTreeMap::new();
                let mut bookmarks = BTreeMap::new();
                let mut overlays = BTreeMap::new();
                let mut property_projections = BTreeMap::new();
                let state = self.private.get(&actor);
                for &buffer in &probe.buffers {
                    let auth = self.auth_read(actor, buffer);
                    reads.insert(buffer, auth.map(str::to_owned));
                    match auth {
                        Ok(text) => {
                            let view_obs = state
                                .and_then(|s| s.views.get(&buffer))
                                .map(|&(s, e)| (s, e, text[s..e].to_owned()));
                            views.insert(buffer, Ok(view_obs));
                            let sel_obs =
                                state.and_then(|s| s.selections.get(&buffer)).map(|sel| {
                                    let dir = match sel.anchor.cmp(&sel.head) {
                                        std::cmp::Ordering::Equal => 0,
                                        std::cmp::Ordering::Less => 1,
                                        std::cmp::Ordering::Greater => 2,
                                    };
                                    (sel.anchor, sel.head, dir)
                                });
                            selections.insert(buffer, Ok(sel_obs));
                            for name in &probe.bookmark_names {
                                let bm_obs = state
                                    .and_then(|s| s.bookmarks.get(name))
                                    .filter(|bm| bm.buffer == buffer)
                                    .map(|bm| (bm.offset, bm.affinity, bm.revision));
                                bookmarks.insert((buffer, name.clone()), Ok(bm_obs));
                            }
                            for &ns in &probe.namespaces {
                                for &oid in &probe.overlay_ids {
                                    let key = (buffer, ns, oid);
                                    let ov_obs =
                                        state.and_then(|s| s.overlays.get(&key)).map(|ov| {
                                            (ov.start, ov.end, ov.priority, ov.label.clone())
                                        });
                                    overlays.insert(key, Ok(ov_obs));
                                }
                                property_projections.insert(
                                    (buffer, ns),
                                    self.project_properties(actor, buffer, ns),
                                );
                            }
                        }
                        Err(err) => {
                            views.insert(buffer, Err(err));
                            selections.insert(buffer, Err(err));
                            for name in &probe.bookmark_names {
                                bookmarks.insert((buffer, name.clone()), Err(err));
                            }
                            for &ns in &probe.namespaces {
                                for &oid in &probe.overlay_ids {
                                    overlays.insert((buffer, ns, oid), Err(err));
                                }
                            }
                        }
                    }
                }
                let mut registers = BTreeMap::new();
                for name in &probe.register_names {
                    let entry = state
                        .and_then(|s| s.registers.get(name))
                        .map(|r| (r.text.clone(), r.kind));
                    registers.insert(name.clone(), entry);
                }
                let current_kill = state
                    .and_then(|s| s.kill_ring.get(s.kill_cursor))
                    .map(|r| (r.text.clone(), r.kind));
                let mut search_ws = BTreeMap::new();
                let mut search_sub = BTreeMap::new();
                for &pat in &probe.search_patterns {
                    search_ws.insert(pat.to_owned(), self.search_workspace(actor, pat));
                    search_sub.insert(
                        pat.to_owned(),
                        self.search_buffers(actor, &probe.buffers, pat),
                    );
                }
                let snapshot_context_filter =
                    self.extract_filter_context(actor, None, false, probe.budget);
                let session_context_filter =
                    self.extract_filter_context(actor, Some(&probe.buffers), true, probe.budget);
                actors.insert(
                    actor,
                    ActorObservation {
                        reads,
                        views,
                        selections,
                        bookmarks,
                        overlays,
                        property_projections,
                        registers,
                        current_kill,
                        search_workspace: search_ws,
                        search_buffers: search_sub,
                        snapshot_context_filter,
                        session_context_filter,
                    },
                );
            }
            WorkspaceObservation {
                revision: self.revision,
                live_ids,
                tombstoned_ids,
                buffers,
                actors,
            }
        }
    }

    // -------------------------------------------------------------------------
    // Unified cross-surface observation & comparators
    // -------------------------------------------------------------------------

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ObsAccessErr {
        InactiveActor(u64),
        UnknownBuffer(u64),
        BufferClosed(u64),
        Denied { actor: u64, buffer: u64 },
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ObsContextErr {
        Access(ObsAccessErr),
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ObsContextFragment {
        revision: u64,
        buffer: u64,
        start: usize,
        end: usize,
        text: String,
        truncated: bool,
        omitted_bytes: usize,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ObsContextPack {
        revision: u64,
        participants: Vec<u64>,
        fragments: Vec<ObsContextFragment>,
        total_bytes: usize,
        truncated: bool,
        omitted_fragments: usize,
    }

    type PropertySegmentObs = (usize, usize, BTreeMap<String, String>);
    type MatchListObs = Vec<(u64, u64, usize, usize)>;
    type ViewObs = Result<Option<(usize, usize, String)>, ObsAccessErr>;
    type SelectionObs = Result<Option<(usize, usize, u8)>, ObsAccessErr>;
    type BookmarkObs = Result<Option<(usize, ShadowAffinity, u64)>, ObsAccessErr>;
    type OverlayObs = Result<Option<(usize, usize, u32, String)>, ObsAccessErr>;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ActorObservation {
        reads: BTreeMap<u64, Result<String, ObsAccessErr>>,
        views: BTreeMap<u64, ViewObs>,
        selections: BTreeMap<u64, SelectionObs>,
        bookmarks: BTreeMap<(u64, String), BookmarkObs>,
        overlays: BTreeMap<(u64, u64, u64), OverlayObs>,
        property_projections: BTreeMap<(u64, u64), Vec<PropertySegmentObs>>,
        registers: BTreeMap<String, Option<(String, ShadowRegKind)>>,
        current_kill: Option<(String, ShadowRegKind)>,
        search_workspace: BTreeMap<String, Result<MatchListObs, ObsAccessErr>>,
        search_buffers: BTreeMap<String, Result<MatchListObs, ObsAccessErr>>,
        snapshot_context_filter: Result<ObsContextPack, ObsContextErr>,
        session_context_filter: Result<ObsContextPack, ObsContextErr>,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct WorkspaceObservation {
        revision: u64,
        live_ids: Vec<u64>,
        tombstoned_ids: Vec<u64>,
        buffers: BTreeMap<u64, (String, bool)>,
        actors: BTreeMap<u64, ActorObservation>,
    }

    #[derive(Clone, Debug)]
    struct ObservationProbe {
        actors: Vec<u64>,
        buffers: Vec<u64>,
        bookmark_names: Vec<String>,
        namespaces: Vec<u64>,
        overlay_ids: Vec<u64>,
        register_names: Vec<String>,
        search_patterns: Vec<&'static str>,
        budget: ContextBudget,
    }

    type ActorOverlayProps = BTreeMap<(u64, u64, u64), AnnotationProperties>;

    struct LiveObservationInput<'a> {
        snapshot: &'a WorkspaceSnapshot,
        registry: &'a ActorRegistry,
        policy: &'a AccessPolicy,
        session: &'a WorkspaceSession,
        dirty: &'a dyn Fn(BufferId) -> bool,
        actor_props: &'a ActorOverlayProps,
        probe: &'a ObservationProbe,
    }

    fn to_prod_cap(cap: ShadowCap) -> Capability {
        match cap {
            ShadowCap::Read => Capability::Read,
            ShadowCap::Propose => Capability::Propose,
            ShadowCap::Review => Capability::Review,
            ShadowCap::Commit => Capability::Commit,
        }
    }

    fn to_prod_role(role: ShadowRole) -> ActorRole {
        match role {
            ShadowRole::Human => ActorRole::Human,
            ShadowRole::Agent => ActorRole::Agent,
        }
    }

    fn to_prod_affinity(affinity: ShadowAffinity) -> Affinity {
        match affinity {
            ShadowAffinity::Left => Affinity::Left,
            ShadowAffinity::Right => Affinity::Right,
        }
    }

    fn from_prod_affinity(affinity: Affinity) -> ShadowAffinity {
        match affinity {
            Affinity::Left => ShadowAffinity::Left,
            Affinity::Right => ShadowAffinity::Right,
        }
    }

    fn to_prod_collapse(collapse: ShadowCollapse) -> CollapsePolicy {
        match collapse {
            ShadowCollapse::Evict => CollapsePolicy::Evict,
            ShadowCollapse::RetainEmpty => CollapsePolicy::RetainEmpty,
        }
    }

    fn to_prod_reg_kind(kind: ShadowRegKind) -> RegisterKind {
        match kind {
            ShadowRegKind::Charwise => RegisterKind::Charwise,
            ShadowRegKind::Linewise => RegisterKind::Linewise,
            ShadowRegKind::Blockwise => RegisterKind::Blockwise,
        }
    }

    fn from_prod_reg_kind(kind: RegisterKind) -> ShadowRegKind {
        match kind {
            RegisterKind::Charwise => ShadowRegKind::Charwise,
            RegisterKind::Linewise => ShadowRegKind::Linewise,
            RegisterKind::Blockwise => ShadowRegKind::Blockwise,
        }
    }

    fn map_access_err(err: &AccessError) -> ObsAccessErr {
        match err {
            AccessError::InactiveActor(id) => ObsAccessErr::InactiveActor(id.raw()),
            AccessError::UnknownBuffer(id) => ObsAccessErr::UnknownBuffer(id.raw()),
            AccessError::BufferClosed(id) => ObsAccessErr::BufferClosed(id.raw()),
            AccessError::Denied { actor, buffer, .. } => ObsAccessErr::Denied {
                actor: actor.raw(),
                buffer: buffer.raw(),
            },
            AccessError::Transaction(other) => {
                panic!("unexpected transaction error in read/search/context: {other:?}")
            }
        }
    }

    fn map_session_err(err: &SessionError) -> ObsAccessErr {
        match err {
            SessionError::Access(access) => map_access_err(access),
            other => panic!("unexpected session error during read observation: {other:?}"),
        }
    }

    fn map_context_pack(pack: ContextPack) -> ObsContextPack {
        ObsContextPack {
            revision: pack.revision.raw(),
            participants: pack.participants.into_iter().map(ActorId::raw).collect(),
            fragments: pack
                .fragments
                .into_iter()
                .map(|f| ObsContextFragment {
                    revision: f.revision.raw(),
                    buffer: f.buffer.raw(),
                    start: f.span.start().as_usize(),
                    end: f.span.end().as_usize(),
                    text: f.text.as_str().to_owned(),
                    truncated: f.truncated,
                    omitted_bytes: f.omitted_bytes,
                })
                .collect(),
            total_bytes: pack.total_bytes,
            truncated: pack.truncated,
            omitted_fragments: pack.omitted_fragments,
        }
    }

    fn map_context_err(err: ContextError) -> ObsContextErr {
        match err {
            ContextError::Access(access) => ObsContextErr::Access(map_access_err(&access)),
            other => {
                panic!("unexpected non-access context error during filter observation: {other:?}")
            }
        }
    }

    fn observe_live(input: &LiveObservationInput<'_>) -> WorkspaceObservation {
        let catalog = input.snapshot.catalog();
        assert_eq!(
            input.session.revision(),
            input.snapshot.revision(),
            "session revision must match authoritative snapshot revision"
        );
        let live_ids: Vec<u64> = catalog.live_ids().into_iter().map(BufferId::raw).collect();
        let tombstoned_ids: Vec<u64> = catalog
            .tombstoned_ids()
            .into_iter()
            .map(BufferId::raw)
            .collect();
        let mut buffers = BTreeMap::new();
        for &id in &live_ids {
            let bid = BufferId::new(id);
            let text = catalog.get(bid).expect("live buffer exists");
            buffers.insert(id, (text.as_str().to_owned(), (input.dirty)(bid)));
        }
        let probe_bids: Vec<BufferId> = input
            .probe
            .buffers
            .iter()
            .copied()
            .map(BufferId::new)
            .collect();
        let mut actors = BTreeMap::new();
        for &actor in &input.probe.actors {
            let aid = ActorId::new(actor);
            let mut reads = BTreeMap::new();
            let mut views = BTreeMap::new();
            let mut selections = BTreeMap::new();
            let mut bookmarks = BTreeMap::new();
            let mut overlays = BTreeMap::new();
            let mut property_projections = BTreeMap::new();
            for &buffer in &input.probe.buffers {
                let bid = BufferId::new(buffer);
                let read_res = input.session.read(aid, bid);
                reads.insert(
                    buffer,
                    read_res
                        .as_ref()
                        .map(|t| t.as_str().to_owned())
                        .map_err(map_session_err),
                );
                let view_res = input.session.view(aid, bid);
                views.insert(
                    buffer,
                    view_res
                        .map(|opt| {
                            opt.map(|v| {
                                let slice = v
                                    .visible_text(catalog)
                                    .expect("live session view must slice live catalog text");
                                (
                                    v.visible().start().as_usize(),
                                    v.visible().end().as_usize(),
                                    slice.to_owned(),
                                )
                            })
                        })
                        .map_err(|e| map_session_err(&e)),
                );
                let sel_res = input.session.selection(aid, bid);
                selections.insert(
                    buffer,
                    sel_res
                        .map(|opt| {
                            opt.map(|s| {
                                let dir = match s.direction() {
                                    Direction::Empty => 0,
                                    Direction::Forward => 1,
                                    Direction::Backward => 2,
                                };
                                (s.anchor().as_usize(), s.head().as_usize(), dir)
                            })
                        })
                        .map_err(|e| map_session_err(&e)),
                );
                for name in &input.probe.bookmark_names {
                    let bname = BookmarkName::new(name.as_str()).expect("nonempty bookmark name");
                    let bm_res = input.session.bookmark(aid, bid, &bname);
                    bookmarks.insert(
                        (buffer, name.clone()),
                        bm_res
                            .map(|opt| {
                                opt.map(|bm| {
                                    (
                                        bm.anchor.offset.as_usize(),
                                        from_prod_affinity(bm.anchor.affinity),
                                        bm.revision.raw(),
                                    )
                                })
                            })
                            .map_err(|e| map_session_err(&e)),
                    );
                }
                for &ns in &input.probe.namespaces {
                    let mut store = AnnotationStore::new();
                    for &oid in &input.probe.overlay_ids {
                        let key = AnnotationKey {
                            buffer: bid,
                            namespace: NamespaceId::new(ns),
                            id: AnnotationId::new(oid),
                        };
                        let ov_res = input.session.overlay(aid, key);
                        if let Ok(Some(ann)) = ov_res {
                            store
                                .set(catalog, ann.clone())
                                .expect("live session overlay validates in live catalog");
                        }
                        overlays.insert(
                            (buffer, ns, oid),
                            ov_res
                                .map(|opt| {
                                    opt.map(|a| {
                                        (
                                            a.span.start().as_usize(),
                                            a.span.end().as_usize(),
                                            a.priority,
                                            a.label.clone(),
                                        )
                                    })
                                })
                                .map_err(|e| map_session_err(&e)),
                        );
                    }
                    if let Ok(text) = read_res {
                        let props = input
                            .actor_props
                            .get(&(actor, buffer, ns))
                            .cloned()
                            .unwrap_or_default();
                        let projected = project_properties(
                            text,
                            &store,
                            bid,
                            NamespaceId::new(ns),
                            &props,
                            ByteSpan::full_of(text),
                        )
                        .expect("projecting valid session overlays succeeds");
                        for &boundary in &text.char_boundaries() {
                            let point_props = properties_at(
                                text,
                                &store,
                                bid,
                                NamespaceId::new(ns),
                                &props,
                                boundary,
                            )
                            .expect("point property lookup succeeds");
                            if boundary.as_usize() < text.len_bytes() {
                                let seg = projected
                                    .iter()
                                    .find(|s| s.span.start() <= boundary && boundary < s.span.end())
                                    .expect("every interior boundary belongs to a segment");
                                assert_eq!(
                                    seg.properties, point_props,
                                    "projected segment must agree with properties_at"
                                );
                            }
                        }
                        property_projections.insert(
                            (buffer, ns),
                            projected
                                .into_iter()
                                .map(|seg| {
                                    (
                                        seg.span.start().as_usize(),
                                        seg.span.end().as_usize(),
                                        seg.properties,
                                    )
                                })
                                .collect(),
                        );
                    }
                }
            }
            let mut registers = BTreeMap::new();
            for name in &input.probe.register_names {
                let rname = RegisterName::new(name.as_str()).expect("nonempty register name");
                registers.insert(
                    name.clone(),
                    input
                        .session
                        .register(aid, &rname)
                        .map(|e| (e.text.as_str().to_owned(), from_prod_reg_kind(e.kind))),
                );
            }
            let current_kill = input
                .session
                .current_kill(aid)
                .map(|e| (e.text.as_str().to_owned(), from_prod_reg_kind(e.kind)));
            let mut search_ws = BTreeMap::new();
            let mut search_sub = BTreeMap::new();
            for &pat in &input.probe.search_patterns {
                let ws_res =
                    search_workspace(input.snapshot, input.registry, input.policy, aid, pat)
                        .map(|ms| {
                            ms.into_iter()
                                .map(|m| {
                                    (
                                        m.revision.raw(),
                                        m.buffer.raw(),
                                        m.span.start().as_usize(),
                                        m.span.end().as_usize(),
                                    )
                                })
                                .collect()
                        })
                        .map_err(|e| match e {
                            WorkspaceSearchError::Access(a) => map_access_err(&a),
                            WorkspaceSearchError::Search(s) => {
                                panic!("unexpected search error for nonempty pattern: {s:?}")
                            }
                        });
                search_ws.insert(pat.to_owned(), ws_res);
                let sub_res = search_buffers(
                    input.snapshot,
                    input.registry,
                    input.policy,
                    aid,
                    probe_bids.iter().copied(),
                    pat,
                )
                .map(|ms| {
                    ms.into_iter()
                        .map(|m| {
                            (
                                m.revision.raw(),
                                m.buffer.raw(),
                                m.span.start().as_usize(),
                                m.span.end().as_usize(),
                            )
                        })
                        .collect()
                })
                .map_err(|e| match e {
                    WorkspaceSearchError::Access(a) => map_access_err(&a),
                    WorkspaceSearchError::Search(s) => {
                        panic!("unexpected search error for nonempty pattern: {s:?}")
                    }
                });
                search_sub.insert(pat.to_owned(), sub_res);
            }
            let snapshot_context_filter = extract_workspace_context(
                input.snapshot,
                input.registry,
                input.policy,
                aid,
                &ContextScope::all_readable(),
                input.probe.budget,
            )
            .map(map_context_pack)
            .map_err(map_context_err);
            let session_scope = ContextScope::from_buffers(probe_bids.iter().copied());
            let session_context_filter = extract_session_context(
                input.session,
                aid,
                &session_scope,
                &ContextRequest::workspace(input.probe.budget),
            )
            .map(map_context_pack)
            .map_err(map_context_err);
            actors.insert(
                actor,
                ActorObservation {
                    reads,
                    views,
                    selections,
                    bookmarks,
                    overlays,
                    property_projections,
                    registers,
                    current_kill,
                    search_workspace: search_ws,
                    search_buffers: search_sub,
                    snapshot_context_filter,
                    session_context_filter,
                },
            );
        }
        WorkspaceObservation {
            revision: input.snapshot.revision().raw(),
            live_ids,
            tombstoned_ids,
            buffers,
            actors,
        }
    }

    // -------------------------------------------------------------------------
    // Law comparators and state-drift detectors (release-enforced assert! only)
    // -------------------------------------------------------------------------

    fn assert_observations_equal(
        label: &str,
        expected: &WorkspaceObservation,
        actual: &WorkspaceObservation,
    ) {
        assert_eq!(
            actual.revision, expected.revision,
            "{label}: revision mismatch"
        );
        assert_eq!(
            actual.live_ids, expected.live_ids,
            "{label}: live buffer ids mismatch"
        );
        assert_eq!(
            actual.tombstoned_ids, expected.tombstoned_ids,
            "{label}: tombstoned buffer ids mismatch"
        );
        assert_eq!(
            actual.buffers, expected.buffers,
            "{label}: shared buffer text/dirty mismatch"
        );
        assert_eq!(
            actual.actors.keys().collect::<Vec<_>>(),
            expected.actors.keys().collect::<Vec<_>>(),
            "{label}: observed actor set mismatch"
        );
        for (&actor, exp_actor) in &expected.actors {
            let act_actor = &actual.actors[&actor];
            assert_eq!(
                act_actor.reads, exp_actor.reads,
                "{label}: actor {actor} reads mismatch"
            );
            assert_eq!(
                act_actor.views, exp_actor.views,
                "{label}: actor {actor} views mismatch"
            );
            assert_eq!(
                act_actor.selections, exp_actor.selections,
                "{label}: actor {actor} selections mismatch"
            );
            assert_eq!(
                act_actor.bookmarks, exp_actor.bookmarks,
                "{label}: actor {actor} bookmarks mismatch"
            );
            assert_eq!(
                act_actor.overlays, exp_actor.overlays,
                "{label}: actor {actor} overlays mismatch"
            );
            assert_eq!(
                act_actor.property_projections, exp_actor.property_projections,
                "{label}: actor {actor} property projections mismatch"
            );
            assert_eq!(
                act_actor.registers, exp_actor.registers,
                "{label}: actor {actor} registers mismatch"
            );
            assert_eq!(
                act_actor.current_kill, exp_actor.current_kill,
                "{label}: actor {actor} current kill mismatch"
            );
            assert_eq!(
                act_actor.search_workspace, exp_actor.search_workspace,
                "{label}: actor {actor} workspace search mismatch"
            );
            assert_eq!(
                act_actor.search_buffers, exp_actor.search_buffers,
                "{label}: actor {actor} subset search mismatch"
            );
            assert_eq!(
                act_actor.snapshot_context_filter, exp_actor.snapshot_context_filter,
                "{label}: actor {actor} snapshot context mismatch"
            );
            assert_eq!(
                act_actor.session_context_filter, exp_actor.session_context_filter,
                "{label}: actor {actor} session context mismatch"
            );
        }
        assert_eq!(actual, expected, "{label}: full observation mismatch");
    }

    fn assert_atomicity_preserved(
        label: &str,
        before_session: &WorkspaceSession,
        after_session: &WorkspaceSession,
        before_obs: &WorkspaceObservation,
        after_obs: &WorkspaceObservation,
    ) {
        assert_eq!(
            after_session, before_session,
            "{label}: rejected operation mutated WorkspaceSession state"
        );
        assert_observations_equal(label, before_obs, after_obs);
    }

    fn assert_private_isolation(
        label: &str,
        acting_actor: u64,
        before: &WorkspaceObservation,
        after: &WorkspaceObservation,
    ) {
        assert_eq!(
            after.revision, before.revision,
            "{label}: private update changed shared revision"
        );
        assert_eq!(
            after.live_ids, before.live_ids,
            "{label}: private update changed live buffer ids"
        );
        assert_eq!(
            after.tombstoned_ids, before.tombstoned_ids,
            "{label}: private update changed tombstoned buffer ids"
        );
        assert_eq!(
            after.buffers, before.buffers,
            "{label}: private update changed shared text or dirty flags"
        );
        for (&actor, before_actor) in &before.actors {
            let after_actor = &after.actors[&actor];
            assert_eq!(
                after_actor.reads, before_actor.reads,
                "{label}: private update changed actor {actor} shared reads"
            );
            assert_eq!(
                after_actor.search_workspace, before_actor.search_workspace,
                "{label}: private update changed actor {actor} workspace search"
            );
            assert_eq!(
                after_actor.search_buffers, before_actor.search_buffers,
                "{label}: private update changed actor {actor} subset search"
            );
            assert_eq!(
                after_actor.snapshot_context_filter, before_actor.snapshot_context_filter,
                "{label}: private update changed actor {actor} snapshot context"
            );
            if actor != acting_actor {
                assert_eq!(
                    after_actor, before_actor,
                    "{label}: actor {acting_actor} private update leaked into actor {actor}"
                );
            }
        }
    }

    type RevisionlessBookmarkObs = Result<Option<(usize, ShadowAffinity)>, ObsAccessErr>;

    fn strip_bookmark_revision(res: &BookmarkObs) -> RevisionlessBookmarkObs {
        res.as_ref()
            .map(|opt| opt.map(|(offset, affinity, _rev)| (offset, affinity)))
            .map_err(|&e| e)
    }

    fn strip_search_revision(
        res: &Result<MatchListObs, ObsAccessErr>,
    ) -> Result<Vec<(u64, usize, usize)>, ObsAccessErr> {
        res.as_ref()
            .map(|items| items.iter().map(|&(_, b, s, e)| (b, s, e)).collect())
            .map_err(|&e| e)
    }

    type RevisionlessFragment = (u64, usize, usize, String, bool, usize);
    type RevisionlessContextPack = (Vec<u64>, Vec<RevisionlessFragment>, usize, bool, usize);

    fn strip_context_revision(
        res: &Result<ObsContextPack, ObsContextErr>,
    ) -> Result<RevisionlessContextPack, ObsContextErr> {
        res.as_ref()
            .map(|pack| {
                (
                    pack.participants.clone(),
                    pack.fragments
                        .iter()
                        .map(|f| {
                            (
                                f.buffer,
                                f.start,
                                f.end,
                                f.text.clone(),
                                f.truncated,
                                f.omitted_bytes,
                            )
                        })
                        .collect(),
                    pack.total_bytes,
                    pack.truncated,
                    pack.omitted_fragments,
                )
            })
            .map_err(|&e| e)
    }

    fn assert_non_interference(
        label: &str,
        actor: u64,
        left: &ActorObservation,
        right: &ActorObservation,
        require_same_revision: bool,
    ) {
        assert_eq!(
            left.reads, right.reads,
            "{label} (actor {actor}): read non-interference violated"
        );
        assert_eq!(
            left.views, right.views,
            "{label} (actor {actor}): view non-interference violated"
        );
        assert_eq!(
            left.selections, right.selections,
            "{label} (actor {actor}): selection non-interference violated"
        );
        assert_eq!(
            left.overlays, right.overlays,
            "{label} (actor {actor}): overlay non-interference violated"
        );
        assert_eq!(
            left.property_projections, right.property_projections,
            "{label} (actor {actor}): property projection non-interference violated"
        );
        assert_eq!(
            left.registers, right.registers,
            "{label} (actor {actor}): register non-interference violated"
        );
        assert_eq!(
            left.current_kill, right.current_kill,
            "{label} (actor {actor}): kill-ring non-interference violated"
        );
        if require_same_revision {
            assert_eq!(
                left.bookmarks, right.bookmarks,
                "{label} (actor {actor}): bookmark non-interference violated"
            );
            assert_eq!(
                left.search_workspace, right.search_workspace,
                "{label} (actor {actor}): workspace search non-interference violated"
            );
            assert_eq!(
                left.search_buffers, right.search_buffers,
                "{label} (actor {actor}): subset search non-interference violated"
            );
            assert_eq!(
                left.snapshot_context_filter, right.snapshot_context_filter,
                "{label} (actor {actor}): snapshot context non-interference violated"
            );
            assert_eq!(
                left.session_context_filter, right.session_context_filter,
                "{label} (actor {actor}): session context non-interference violated"
            );
        } else {
            let left_bookmarks: BTreeMap<_, _> = left
                .bookmarks
                .iter()
                .map(|(key, res)| (key, strip_bookmark_revision(res)))
                .collect();
            let right_bookmarks: BTreeMap<_, _> = right
                .bookmarks
                .iter()
                .map(|(key, res)| (key, strip_bookmark_revision(res)))
                .collect();
            assert_eq!(
                left_bookmarks, right_bookmarks,
                "{label} (actor {actor}): bookmark content drifted"
            );
            for (pat, lres) in &left.search_workspace {
                assert_eq!(
                    strip_search_revision(lres),
                    strip_search_revision(&right.search_workspace[pat]),
                    "{label} (actor {actor}): workspace search matches drifted for pattern {pat:?}"
                );
            }
            for (pat, lres) in &left.search_buffers {
                assert_eq!(
                    strip_search_revision(lres),
                    strip_search_revision(&right.search_buffers[pat]),
                    "{label} (actor {actor}): subset search matches drifted for pattern {pat:?}"
                );
            }
            assert_eq!(
                strip_context_revision(&left.snapshot_context_filter),
                strip_context_revision(&right.snapshot_context_filter),
                "{label} (actor {actor}): snapshot context content drifted"
            );
            assert_eq!(
                strip_context_revision(&left.session_context_filter),
                strip_context_revision(&right.session_context_filter),
                "{label} (actor {actor}): session context content drifted"
            );
        }
    }

    // -------------------------------------------------------------------------
    // Helpers for constructing deterministic registries, splices, and overlays
    // -------------------------------------------------------------------------

    fn build_registry(actors: &[(u64, ShadowRole)]) -> ActorRegistry {
        let mut registry = ActorRegistry::new();
        for &(id, role) in actors {
            registry
                .register(ActorId::new(id), to_prod_role(role))
                .expect("distinct initial actor");
        }
        registry
    }

    fn shadow_splices_from_patch(patch: &Patch) -> Vec<ShadowSplice> {
        patch
            .splices()
            .iter()
            .map(|s| ShadowSplice {
                start: s.span().start().as_usize(),
                end: s.span().end().as_usize(),
                replacement: s.replacement().as_str().to_owned(),
            })
            .collect()
    }

    fn random_boundary(rng: &mut Rng, text: &Text) -> ByteOffset {
        let bounds = text.char_boundaries();
        bounds[rng.gen_range(0..bounds.len())]
    }

    fn random_span(rng: &mut Rng, text: &Text) -> ByteSpan {
        let a = random_boundary(rng, text);
        let b = random_boundary(rng, text);
        ByteSpan::new(a.min(b), a.max(b)).expect("ordered boundaries")
    }

    fn random_fragment(rng: &mut Rng) -> &'static str {
        rng.choose(FRAGMENTS).expect("nonempty fragments")
    }

    fn random_non_noop_patch(rng: &mut Rng, text: &Text, multi_splice: bool) -> Patch {
        let bounds = text.char_boundaries();
        if multi_splice && bounds.len() >= 4 {
            let a = 0;
            let b = 1;
            let c = bounds.len() - 2;
            let d = bounds.len() - 1;
            if bounds[b] < bounds[c] {
                let s1 = Splice::new(
                    ByteSpan::new(bounds[a], bounds[b]).expect("ordered"),
                    Text::from_str(if rng.gen_bool() { "界" } else { "xy" }),
                );
                let s2 = Splice::new(
                    ByteSpan::new(bounds[c], bounds[d]).expect("ordered"),
                    Text::from_str(if rng.gen_bool() { "😀" } else { "z" }),
                );
                return Patch::new(vec![s1, s2]).expect("strictly separated splices");
            }
        }
        let span = random_span(rng, text);
        let original = span.slice_in(text).expect("valid span");
        let mut replacement = random_fragment(rng);
        if replacement == original {
            replacement = if original == "a" { "界" } else { "a" };
        }
        Patch::new(vec![Splice::new(span, Text::from_str(replacement))]).expect("single splice")
    }

    fn apply_private_event_to_session(
        session: &mut WorkspaceSession,
        snapshot: &WorkspaceSnapshot,
        actor_props: &mut ActorOverlayProps,
        event: &ShadowEvent,
    ) {
        match event {
            ShadowEvent::SetPolicy { policy } => {
                session.set_policy(policy.to_access_policy());
            }
            ShadowEvent::SetView {
                actor,
                buffer,
                start,
                end,
            } => {
                let text = snapshot
                    .catalog()
                    .get(BufferId::new(*buffer))
                    .expect("live buffer");
                let span = ByteSpan::validate_in(text, *start, *end).expect("valid span");
                let view = BufferView::narrowed(snapshot.catalog(), BufferId::new(*buffer), span)
                    .expect("valid view");
                session
                    .set_view(ActorId::new(*actor), view)
                    .expect("valid session set_view");
            }
            ShadowEvent::SetSelection {
                actor,
                buffer,
                anchor,
                head,
            } => {
                let bid = BufferId::new(*buffer);
                let aid = ActorId::new(*actor);
                let text = snapshot.catalog().get(bid).expect("live buffer");
                let view = session
                    .view(aid, bid)
                    .expect("authorized view")
                    .expect("view exists");
                let sel = Selection::new(
                    snapshot.catalog(),
                    &view,
                    bid,
                    text.validate_offset(*anchor).expect("valid anchor"),
                    text.validate_offset(*head).expect("valid head"),
                )
                .expect("valid selection");
                session
                    .set_selection(aid, sel)
                    .expect("valid session set_selection");
            }
            ShadowEvent::SetBookmark {
                actor,
                name,
                buffer,
                offset,
                affinity,
            } => {
                let bid = BufferId::new(*buffer);
                let aid = ActorId::new(*actor);
                let text = snapshot.catalog().get(bid).expect("live buffer");
                let bm = Bookmark {
                    buffer: bid,
                    anchor: Anchor {
                        offset: text.validate_offset(*offset).expect("valid offset"),
                        affinity: to_prod_affinity(*affinity),
                    },
                    revision: session.revision(),
                };
                session
                    .set_bookmark(aid, BookmarkName::new(name.as_str()).expect("nonempty"), bm)
                    .expect("valid set_bookmark");
            }
            ShadowEvent::SetOverlay { actor, overlay } => {
                let bid = BufferId::new(overlay.buffer);
                let aid = ActorId::new(*actor);
                let text = snapshot.catalog().get(bid).expect("live buffer");
                let ann = Annotation {
                    key: AnnotationKey {
                        buffer: bid,
                        namespace: NamespaceId::new(overlay.namespace),
                        id: AnnotationId::new(overlay.id),
                    },
                    owner: ActorId::new(overlay.owner),
                    span: ByteSpan::validate_in(text, overlay.start, overlay.end)
                        .expect("valid span"),
                    start_affinity: to_prod_affinity(overlay.start_affinity),
                    end_affinity: to_prod_affinity(overlay.end_affinity),
                    priority: overlay.priority,
                    collapse: to_prod_collapse(overlay.collapse),
                    label: overlay.label.clone(),
                };
                session
                    .set_overlay(aid, ann)
                    .expect("valid session set_overlay");
            }
            ShadowEvent::SetOverlayProps {
                actor,
                buffer,
                namespace,
                id,
                props,
            } => {
                actor_props
                    .entry((*actor, *buffer, *namespace))
                    .or_default()
                    .insert(AnnotationId::new(*id), props.clone());
            }
            ShadowEvent::PutRegister { actor, name, entry } => {
                session
                    .put_register(
                        ActorId::new(*actor),
                        RegisterName::new(name.as_str()).expect("nonempty"),
                        RegisterEntry {
                            text: Text::from_str(&entry.text),
                            kind: to_prod_reg_kind(entry.kind),
                        },
                    )
                    .expect("valid put_register");
            }
            ShadowEvent::PushKill { actor, entry } => {
                session
                    .push_kill(
                        ActorId::new(*actor),
                        RegisterEntry {
                            text: Text::from_str(&entry.text),
                            kind: to_prod_reg_kind(entry.kind),
                        },
                    )
                    .expect("valid push_kill");
            }
            ShadowEvent::Open { .. }
            | ShadowEvent::Close { .. }
            | ShadowEvent::Edit { .. }
            | ShadowEvent::Save { .. } => {
                panic!("apply_private_event_to_session called with non-private event");
            }
        }
    }

    // -------------------------------------------------------------------------
    // Test 1: Deterministic private-state isolation and projection edge cases
    // -------------------------------------------------------------------------

    #[test]
    fn deterministic_private_isolation_and_projection_edges() {
        let actor_specs = [(1, ShadowRole::Human), (2, ShadowRole::Agent)];
        let initial_buffers = [(0, "a界😀b\ncd", true), (1, "é\r\n界a", true)];
        let mut policy = ShadowPolicy::default();
        policy.grant_default(ShadowCap::Read);

        let mut catalog = BufferCatalog::new();
        let mut dirty = DirtyTracker::new();
        for &(id, text, _) in &initial_buffers {
            let bid = BufferId::new(id);
            let txt = Text::from_str(text);
            catalog.register(bid, txt.clone()).unwrap();
            dirty
                .acknowledge(SaveReceipt {
                    buffer: bid,
                    version: Revision::ZERO,
                    content: txt,
                })
                .unwrap();
        }
        let registry = build_registry(&actor_specs);
        let mut seq = Sequencer::new(catalog, registry.clone());
        let mut log = DeliveryLog::new(seq.snapshot());
        let prod_policy = policy.to_access_policy();
        let mut session =
            WorkspaceSession::new(seq.snapshot(), registry.clone(), prod_policy.clone());
        let mut actor_props: ActorOverlayProps = BTreeMap::new();
        let mut shadow = ShadowWorkspace::new(&initial_buffers, &actor_specs, policy.clone());

        let probe = ObservationProbe {
            actors: vec![1, 2],
            buffers: vec![0, 1, 9],
            bookmark_names: vec!["m0".into(), "m1".into()],
            namespaces: vec![1],
            overlay_ids: vec![1, 2],
            register_names: vec!["r".into(), "界".into()],
            search_patterns: PROBE_PATTERNS.to_vec(),
            budget: ContextBudget::with_limits(16, Some(8), Some(4)).unwrap(),
        };

        let check_private_step = |session: &mut WorkspaceSession,
                                  shadow: &mut ShadowWorkspace,
                                  actor_props: &mut ActorOverlayProps,
                                  acting: u64,
                                  ev: ShadowEvent,
                                  label: &str| {
            let snap = seq.snapshot();
            let dirty_fn = |b: BufferId| snap.catalog().get(b).is_none_or(|t| dirty.is_dirty(b, t));
            let before = observe_live(&LiveObservationInput {
                snapshot: &snap,
                registry: &registry,
                policy: &prod_policy,
                session,
                dirty: &dirty_fn,
                actor_props,
                probe: &probe,
            });
            apply_private_event_to_session(session, &snap, actor_props, &ev);
            shadow.apply_event(ev);
            let after = observe_live(&LiveObservationInput {
                snapshot: &snap,
                registry: &registry,
                policy: &prod_policy,
                session,
                dirty: &dirty_fn,
                actor_props,
                probe: &probe,
            });
            assert_private_isolation(label, acting, &before, &after);
            assert_observations_equal(label, &shadow.observe(&probe), &after);
        };

        // Actor 1 sets narrowed view, backward selection, bookmark, Evict & RetainEmpty overlays,
        // properties, register, kill entry. None may affect shared text or Actor 2.
        let b0_text = seq
            .snapshot()
            .catalog()
            .get(BufferId::new(0))
            .unwrap()
            .clone();
        let b0_bounds = b0_text.char_boundaries();
        let v_start = b0_bounds[1].as_usize(); // start of '界'
        let v_end = b0_bounds[4].as_usize(); // after 'b'
        let mid = b0_bounds[2].as_usize(); // start of '😀'

        check_private_step(
            &mut session,
            &mut shadow,
            &mut actor_props,
            1,
            ShadowEvent::SetView {
                actor: 1,
                buffer: 0,
                start: v_start,
                end: v_end,
            },
            "actor1_narrow_view",
        );
        check_private_step(
            &mut session,
            &mut shadow,
            &mut actor_props,
            1,
            ShadowEvent::SetSelection {
                actor: 1,
                buffer: 0,
                anchor: v_end,
                head: mid,
            },
            "actor1_backward_selection",
        );
        check_private_step(
            &mut session,
            &mut shadow,
            &mut actor_props,
            1,
            ShadowEvent::SetBookmark {
                actor: 1,
                name: "m0".into(),
                buffer: 0,
                offset: mid,
                affinity: ShadowAffinity::Right,
            },
            "actor1_bookmark",
        );
        // Evict overlay covering '界😀' (indices 1..3 in char boundaries)
        check_private_step(
            &mut session,
            &mut shadow,
            &mut actor_props,
            1,
            ShadowEvent::SetOverlay {
                actor: 1,
                overlay: ShadowOverlay {
                    buffer: 0,
                    namespace: 1,
                    id: 1,
                    owner: 1,
                    start: b0_bounds[1].as_usize(),
                    end: b0_bounds[3].as_usize(),
                    start_affinity: ShadowAffinity::Right,
                    end_affinity: ShadowAffinity::Left,
                    priority: 5,
                    collapse: ShadowCollapse::Evict,
                    label: "evict_me".into(),
                },
            },
            "actor1_overlay_evict",
        );
        // RetainEmpty overlay covering the exact same span (will invert on replacement!)
        check_private_step(
            &mut session,
            &mut shadow,
            &mut actor_props,
            1,
            ShadowEvent::SetOverlay {
                actor: 1,
                overlay: ShadowOverlay {
                    buffer: 0,
                    namespace: 1,
                    id: 2,
                    owner: 1,
                    start: b0_bounds[1].as_usize(),
                    end: b0_bounds[3].as_usize(),
                    start_affinity: ShadowAffinity::Right,
                    end_affinity: ShadowAffinity::Left,
                    priority: 2,
                    collapse: ShadowCollapse::RetainEmpty,
                    label: "retain_me".into(),
                },
            },
            "actor1_overlay_retain",
        );
        check_private_step(
            &mut session,
            &mut shadow,
            &mut actor_props,
            1,
            ShadowEvent::SetOverlayProps {
                actor: 1,
                buffer: 0,
                namespace: 1,
                id: 1,
                props: [("face".into(), "bold".into())].into_iter().collect(),
            },
            "actor1_props_1",
        );
        check_private_step(
            &mut session,
            &mut shadow,
            &mut actor_props,
            1,
            ShadowEvent::SetOverlayProps {
                actor: 1,
                buffer: 0,
                namespace: 1,
                id: 2,
                props: [
                    ("face".into(), "italic".into()),
                    ("tag".into(), "keep".into()),
                ]
                .into_iter()
                .collect(),
            },
            "actor1_props_2",
        );
        for (kind, text) in [
            (ShadowRegKind::Charwise, "char"),
            (ShadowRegKind::Linewise, "line\n"),
            (ShadowRegKind::Blockwise, "blk"),
        ] {
            check_private_step(
                &mut session,
                &mut shadow,
                &mut actor_props,
                1,
                ShadowEvent::PutRegister {
                    actor: 1,
                    name: "界".into(),
                    entry: ShadowRegister {
                        text: text.into(),
                        kind,
                    },
                },
                "actor1_put_register",
            );
            check_private_step(
                &mut session,
                &mut shadow,
                &mut actor_props,
                1,
                ShadowEvent::PushKill {
                    actor: 1,
                    entry: ShadowRegister {
                        text: text.into(),
                        kind,
                    },
                },
                "actor1_push_kill",
            );
        }

        // Actor 2 independently populates full view, forward selection, bookmark, register, kill.
        check_private_step(
            &mut session,
            &mut shadow,
            &mut actor_props,
            2,
            ShadowEvent::SetView {
                actor: 2,
                buffer: 0,
                start: 0,
                end: b0_text.len_bytes(),
            },
            "actor2_full_view",
        );
        check_private_step(
            &mut session,
            &mut shadow,
            &mut actor_props,
            2,
            ShadowEvent::SetSelection {
                actor: 2,
                buffer: 0,
                anchor: 0,
                head: b0_bounds[1].as_usize(),
            },
            "actor2_forward_selection",
        );
        check_private_step(
            &mut session,
            &mut shadow,
            &mut actor_props,
            2,
            ShadowEvent::SetBookmark {
                actor: 2,
                name: "m0".into(),
                buffer: 0,
                offset: b0_bounds[1].as_usize(),
                affinity: ShadowAffinity::Left,
            },
            "actor2_bookmark",
        );

        // Now apply a shared edit replacing '界😀' with "X" -> Evict overlay must disappear,
        // RetainEmpty overlay must invert to an empty point at the mapped start (right edge of "X"),
        // views/selections/bookmarks for both actors must transport consistently,
        // registers/kill rings must remain untouched.
        let replace_span = ByteSpan::new(b0_bounds[1], b0_bounds[3]).unwrap();
        let patch = Patch::new(vec![Splice::new(replace_span, Text::from_str("X"))]).unwrap();
        let txn = Transaction::new(
            EditIntent::for_actor(ActorId::new(1), Nonce::new(1), seq.revision()),
            vec![(BufferId::new(0), patch.clone())],
        )
        .unwrap();
        let (outcome, ws_ev) = submit_with_event(&mut seq, txn);
        let receipt = outcome.committed_receipt().expect("commit succeeds");
        let ws_ev = ws_ev.expect("event emitted");
        log.record_workspace(&seq, receipt).unwrap();
        session.advance(log.events().last().unwrap()).unwrap();
        shadow.apply_event(ShadowEvent::Edit {
            patches: vec![(0, shadow_splices_from_patch(&patch))],
        });

        let snap = seq.snapshot();
        let dirty_fn = |b: BufferId| snap.catalog().get(b).is_none_or(|t| dirty.is_dirty(b, t));
        let after_edit = observe_live(&LiveObservationInput {
            snapshot: &snap,
            registry: &registry,
            policy: &prod_policy,
            session: &session,
            dirty: &dirty_fn,
            actor_props: &actor_props,
            probe: &probe,
        });
        assert_observations_equal("after_collapse_edit", &shadow.observe(&probe), &after_edit);
        // Confirm Evict overlay was evicted and RetainEmpty overlay collapsed to point at offset 2.
        assert_eq!(
            session
                .overlay(
                    ActorId::new(1),
                    AnnotationKey {
                        buffer: BufferId::new(0),
                        namespace: NamespaceId::new(1),
                        id: AnnotationId::new(1),
                    }
                )
                .unwrap(),
            None
        );
        let retained = session
            .overlay(
                ActorId::new(1),
                AnnotationKey {
                    buffer: BufferId::new(0),
                    namespace: NamespaceId::new(1),
                    id: AnnotationId::new(2),
                },
            )
            .unwrap()
            .expect("RetainEmpty overlay survives");
        assert!(retained.span.is_empty());
        assert_eq!(retained.span.start().as_usize(), 2);

        // Cross-check standalone view_mapping, annotation_mapping, selections, bookmarks,
        // registers, and kill-rings composition laws on the exact same edit.
        let before_cat = log.base_snapshot().catalog();
        let after_cat = snap.catalog();
        let before_text = before_cat.get(BufferId::new(0)).unwrap();
        let after_text = after_cat.get(BufferId::new(0)).unwrap();
        let pmap = PositionMap::from_replacement(
            before_text.len_bytes(),
            ByteReplacement {
                start: replace_span.start().as_usize(),
                end: replace_span.end().as_usize(),
                inserted_len: 1,
            },
        )
        .unwrap();
        let rmap = RevisionedMap {
            map: &pmap,
            source: Revision::ZERO,
            target: ws_ev.revision(),
        };
        let orig_view = BufferView::narrowed(
            before_cat,
            BufferId::new(0),
            ByteSpan::validate_in(before_text, v_start, v_end).unwrap(),
        )
        .unwrap();
        let mapped_view = map_view(
            after_cat,
            &orig_view,
            &pmap,
            before_text.len_bytes(),
            Affinity::Left,
            Affinity::Right,
        )
        .unwrap();
        assert_eq!(
            session.view(ActorId::new(1), BufferId::new(0)).unwrap(),
            Some(mapped_view)
        );
        assert_eq!(
            mapped_view.widen(after_cat).unwrap(),
            BufferView::full(after_cat, BufferId::new(0)).unwrap()
        );

        let orig_sel = Selection::new(
            before_cat,
            &orig_view,
            BufferId::new(0),
            before_text.validate_offset(v_end).unwrap(),
            before_text.validate_offset(mid).unwrap(),
        )
        .unwrap();
        assert_eq!(orig_sel.flip().flip(), orig_sel);
        let mapped_sel = map_and_clip_selection(
            after_cat,
            orig_sel,
            &mapped_view,
            &pmap,
            before_text.len_bytes(),
            Affinity::Right,
            Affinity::Right,
        )
        .unwrap();
        let unclipped_sel = map_selection(
            after_cat,
            orig_sel,
            &pmap,
            before_text.len_bytes(),
            Affinity::Right,
            Affinity::Right,
        )
        .unwrap();
        assert_eq!(
            clip_selection(after_cat, unclipped_sel, &mapped_view).unwrap(),
            mapped_sel
        );
        assert_eq!(
            session
                .selection(ActorId::new(1), BufferId::new(0))
                .unwrap(),
            Some(mapped_sel)
        );
        // Verify shadow_min_diff agrees with WorkspaceSession::update on a direct snapshot replacement.
        let mut vw_direct = VersionedWorkspace::new(after_cat.clone());
        let mut sess_direct =
            WorkspaceSession::new(vw_direct.capture(), registry.clone(), prod_policy.clone());
        let b0_cur = vw_direct.catalog().get(BufferId::new(0)).unwrap().clone();
        sess_direct
            .set_view(
                ActorId::new(1),
                BufferView::full(vw_direct.catalog(), BufferId::new(0)).unwrap(),
            )
            .unwrap();
        let b0_next = Text::from_str("aXYZb\ncd");
        let diff_splices = shadow_min_diff(b0_cur.as_str(), b0_next.as_str());
        assert_eq!(
            apply_splices_str(b0_cur.as_str(), &diff_splices),
            b0_next.as_str()
        );
        vw_direct.update(BufferId::new(0), b0_next.clone()).unwrap();
        sess_direct
            .update(vw_direct.capture(), BufferId::new(0))
            .unwrap();
        assert_eq!(
            sess_direct
                .read(ActorId::new(1), BufferId::new(0))
                .unwrap()
                .as_str(),
            b0_next.as_str()
        );
        let expected_props: PropertyMap = [
            ("face".into(), "italic".into()),
            ("tag".into(), "keep".into()),
        ]
        .into_iter()
        .collect();
        let mut live_post_store = AnnotationStore::new();
        live_post_store.set(after_cat, retained.clone()).unwrap();
        let live_point_props = properties_at(
            after_text,
            &live_post_store,
            BufferId::new(0),
            NamespaceId::new(1),
            &actor_props[&(1, 0, 1)],
            after_text.validate_offset(2).unwrap(),
        )
        .unwrap();
        assert_eq!(
            live_point_props, expected_props,
            "live properties_at resolves RetainEmpty point overlay properties after eviction of higher-priority overlay"
        );
        assert_eq!(
            shadow.resolve_props_at(1, 0, 1, 2),
            expected_props,
            "RetainEmpty point overlay still resolves properties after eviction of higher-priority overlay"
        );

        let mut actor_sels = ActorSelections::new();
        actor_sels.set(ActorId::new(1), mapped_sel);
        assert_eq!(actor_sels.get(ActorId::new(1)), Some(mapped_sel));
        assert_eq!(actor_sels.get(ActorId::new(2)), None);

        // Standalone annotation_mapping retains inverted/collapsed spans as points regardless of policy.
        let mut standalone_store = AnnotationStore::new();
        let ann = Annotation {
            key: AnnotationKey {
                buffer: BufferId::new(0),
                namespace: NamespaceId::new(1),
                id: AnnotationId::new(1),
            },
            owner: ActorId::new(1),
            span: replace_span,
            start_affinity: Affinity::Right,
            end_affinity: Affinity::Left,
            priority: 5,
            collapse: CollapsePolicy::Evict,
            label: "mapped".into(),
        };
        standalone_store.set(before_cat, ann.clone()).unwrap();
        let revs = MappingRevisions {
            base: Revision::ZERO,
            target: ws_ev.revision(),
        };
        let mapped_ann = map_annotation(&ann, before_text, after_text, &rmap, revs).unwrap();
        let mapped_store = map_store(
            &standalone_store,
            BufferId::new(0),
            before_text,
            after_cat,
            &rmap,
            revs,
        )
        .unwrap();
        assert_eq!(mapped_store.get(ann.key), Some(&mapped_ann));
        assert_eq!(mapped_ann.span.start().as_usize(), 2);
        assert_eq!(mapped_ann.span.end().as_usize(), 2);

        // Standalone RegisterStore + KillRings isolation check.
        let mut reg_store = RegisterStore::new();
        let rn = RegisterName::new("reg").unwrap();
        let e1 = RegisterEntry {
            text: Text::from_str("one"),
            kind: RegisterKind::Charwise,
        };
        let e2 = RegisterEntry {
            text: Text::from_str("two"),
            kind: RegisterKind::Linewise,
        };
        reg_store.put(ActorId::new(1), rn.clone(), e1.clone());
        reg_store.put(ActorId::new(2), rn.clone(), e2.clone());
        assert_eq!(reg_store.get(ActorId::new(1), &rn), Some(&e1));
        assert_eq!(reg_store.get(ActorId::new(2), &rn), Some(&e2));
        let mut rings = KillRings::default();
        rings.get_mut(ActorId::new(1)).push(e1.clone());
        assert!(rings.get_mut(ActorId::new(1)).append(&Text::from_str("+")));
        assert!(rings.get_mut(ActorId::new(1)).prepend(&Text::from_str("-")));
        assert_eq!(
            rings
                .get(ActorId::new(1))
                .unwrap()
                .current()
                .unwrap()
                .text
                .as_str(),
            "-one+"
        );
        assert_eq!(rings.get(ActorId::new(2)), None);
        let mut single_ring = KillRing::new();
        single_ring.push(e1);
        single_ring.push(e2.clone());
        assert_eq!(single_ring.rotate().unwrap().text.as_str(), "one");
        assert_eq!(single_ring.nth(0), Some(&e2));
    }

    // -------------------------------------------------------------------------
    // Test 2: Generated multi-actor edit stream + EVERY prefix replay + checkpoints
    // -------------------------------------------------------------------------

    #[derive(Default, Debug)]
    struct EditStreamCoverage {
        private_view_set: usize,
        private_selection_set: usize,
        private_bookmark_set: usize,
        private_overlay_set: usize,
        private_property_update: usize,
        private_register_put: usize,
        private_kill_push: usize,
        edit_committed: usize,
        edit_multi_buffer: usize,
        edit_multi_splice: usize,
        edit_rebased: usize,
        edit_noop: usize,
        edit_duplicate_retry: usize,
        save_acknowledged: usize,
        policy_updated: usize,
        rejected_closed_buffer: usize,
        rejected_unknown_buffer: usize,
        rejected_revoked_actor: usize,
        rejected_unauthorized_actor: usize,
        rejected_invalid_coords: usize,
        rejected_stale_revision: usize,
        overlay_evicted: usize,
        overlay_retained: usize,
        prefix_replays_checked: usize,
        checkpoint_replays_checked: usize,
    }

    #[derive(Clone)]
    enum EditStreamAction {
        Private(ShadowEvent),
        Commit {
            ws_event: WorkspaceEvent,
            delivery: Box<DeliveryEvent>,
        },
        Save(SaveReceipt),
    }

    fn replay_edit_prefix(
        initial_catalog: &BufferCatalog,
        initial_base_snap: &WorkspaceSnapshot,
        initial_dirty: &DirtyTracker,
        registry: &ActorRegistry,
        initial_policy: &ShadowPolicy,
        actions: &[EditStreamAction],
        probe: &ObservationProbe,
    ) -> WorkspaceObservation {
        let mut replay_cat = initial_catalog.clone();
        let mut delivery_events: Vec<DeliveryEvent> = Vec::new();
        let mut dirty = initial_dirty.clone();
        let mut cur_policy = initial_policy.to_access_policy();
        let mut session = WorkspaceSession::new(
            initial_base_snap.clone(),
            registry.clone(),
            cur_policy.clone(),
        );
        let mut actor_props: ActorOverlayProps = BTreeMap::new();
        let mut cur_snap = initial_base_snap.clone();
        let mut cur_rev = initial_base_snap.revision();

        for action in actions {
            match action {
                EditStreamAction::Private(ev) => {
                    if let ShadowEvent::SetPolicy { policy } = ev {
                        cur_policy = policy.to_access_policy();
                    }
                    apply_private_event_to_session(&mut session, &cur_snap, &mut actor_props, ev);
                }
                EditStreamAction::Commit { ws_event, delivery } => {
                    replay_cat = ws_event
                        .apply_to_catalog(&replay_cat)
                        .expect("valid workspace event replays onto previous catalog");
                    assert_eq!(ws_event.revision(), cur_rev.next().unwrap());
                    cur_rev = ws_event.revision();
                    delivery_events.push(delivery.as_ref().clone());
                    session
                        .advance(delivery)
                        .expect("valid delivery advances replayed session");
                    cur_snap = delivery.after().clone();
                    assert_eq!(
                        &replay_cat,
                        cur_snap.catalog(),
                        "WorkspaceEvent catalog replay must match DeliveryEvent snapshot"
                    );
                }
                EditStreamAction::Save(receipt) => {
                    dirty
                        .acknowledge(receipt.clone())
                        .expect("monotone save receipt replays cleanly");
                }
            }
        }
        let (delivered_cat, delivered_rev) =
            replay_delivery(initial_base_snap, &delivery_events).expect("delivery prefix replays");
        assert_eq!(delivered_cat, replay_cat);
        assert_eq!(delivered_rev, cur_rev);
        let dirty_fn = |b: BufferId| {
            cur_snap
                .catalog()
                .get(b)
                .is_none_or(|t| dirty.is_dirty(b, t))
        };
        observe_live(&LiveObservationInput {
            snapshot: &cur_snap,
            registry,
            policy: &cur_policy,
            session: &session,
            dirty: &dirty_fn,
            actor_props: &actor_props,
            probe,
        })
    }

    #[test]
    fn generated_multi_actor_edit_stream_and_prefix_replay() {
        let mut coverage = EditStreamCoverage::default();
        assert_law(
            SEEDS,
            |rng: &mut Rng| (0..26).map(|_| rng.next_u64()).collect::<Vec<_>>(),
            |step_tokens| {
                let mut rng = Rng::new(step_tokens[0] ^ 0x51a7_993b_2407_1135);
                let actor_specs = [
                    (1, ShadowRole::Human),
                    (2, ShadowRole::Agent),
                    (3, ShadowRole::Human),
                ];
                let initial_buffers = [
                    (0, "a界😀b\ncd", true),
                    (1, "aa界b\n😀e", true),
                    (2, "é界a\nbb", true),
                ];
                let closed_id = BufferId::new(7);
                let unknown_id = BufferId::new(8);
                let revoked_actor = ActorId::new(9);

                let mut init_policy = ShadowPolicy::default();
                init_policy.grant_default(ShadowCap::Read);
                let mut prod_policy = init_policy.to_access_policy();

                let mut initial_catalog = BufferCatalog::new();
                let mut initial_dirty = DirtyTracker::new();
                for &(id, text, _) in &initial_buffers {
                    let bid = BufferId::new(id);
                    let txt = Text::from_str(text);
                    initial_catalog.register(bid, txt.clone()).unwrap();
                    initial_dirty
                        .acknowledge(SaveReceipt {
                            buffer: bid,
                            version: Revision::ZERO,
                            content: txt,
                        })
                        .unwrap();
                }
                // Include a pre-tombstoned buffer in the catalog so closed-buffer checks fire throughout.
                initial_catalog
                    .register(closed_id, Text::from_str("SECRET_TOKEN"))
                    .unwrap();
                initial_catalog.remove(closed_id).unwrap();

                let mut registry = build_registry(&actor_specs);
                registry.register(revoked_actor, ActorRole::Agent).unwrap();
                registry.revoke(revoked_actor).unwrap();

                let mut seq = Sequencer::new(initial_catalog.clone(), registry.clone());
                let initial_base_snap = seq.snapshot();
                let mut log = DeliveryLog::new(initial_base_snap.clone());
                let mut snapshots = SnapshotStore::new();
                snapshots.record(initial_base_snap.clone()).unwrap();
                let mut dirty = initial_dirty.clone();
                let mut session = WorkspaceSession::new(
                    initial_base_snap.clone(),
                    registry.clone(),
                    prod_policy.clone(),
                );
                let mut actor_props: ActorOverlayProps = BTreeMap::new();

                let mut shadow =
                    ShadowWorkspace::new(&initial_buffers, &actor_specs, init_policy.clone());
                shadow.tombstones.insert(closed_id.raw());
                shadow.revoked_actors.insert(revoked_actor.raw());
                let initial_shadow = shadow.clone();

                let probe = ObservationProbe {
                    actors: vec![1, 2, 3, revoked_actor.raw()],
                    buffers: vec![0, 1, 2, closed_id.raw(), unknown_id.raw()],
                    bookmark_names: vec!["bm0".into(), "bm1".into()],
                    namespaces: vec![1],
                    overlay_ids: vec![1, 2],
                    register_names: vec!["reg".into()],
                    search_patterns: PROBE_PATTERNS.to_vec(),
                    budget: ContextBudget::with_limits(18, Some(9), Some(4)).unwrap(),
                };

                let observe_now =
                    |seq: &Sequencer,
                     dirty: &DirtyTracker,
                     policy: &AccessPolicy,
                     session: &WorkspaceSession,
                     actor_props: &ActorOverlayProps| {
                        let snap = seq.snapshot();
                        let dirty_fn = |b: BufferId| {
                            snap.catalog().get(b).is_none_or(|t| dirty.is_dirty(b, t))
                        };
                        observe_live(&LiveObservationInput {
                            snapshot: &snap,
                            registry: &registry,
                            policy,
                            session,
                            dirty: &dirty_fn,
                            actor_props,
                            probe: &probe,
                        })
                    };

                let mut actions: Vec<EditStreamAction> = Vec::new();
                let mut live_history: Vec<WorkspaceObservation> = Vec::new();
                let mut checkpoints: Vec<(
                    usize,
                    WorkspaceSnapshot,
                    DirtyTracker,
                    AccessPolicy,
                    WorkspaceSession,
                    ActorOverlayProps,
                )> = Vec::new();

                let init_obs = observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                assert_observations_equal("edit_stream_step_0", &shadow.observe(&probe), &init_obs);
                live_history.push(init_obs);
                checkpoints.push((
                    0,
                    seq.snapshot(),
                    dirty.clone(),
                    prod_policy.clone(),
                    session.clone(),
                    actor_props.clone(),
                ));

                let mut nonce = 1u64;
                let mut last_committed_txn: Option<(Transaction, SubmitOutcome)> = None;
                let mut stale_candidate: Option<(Revision, BufferId, Text)> = None;

                for (step_idx, &token) in step_tokens.iter().enumerate() {
                    let active_actor = [1u64, 2, 3][rng.gen_range(0..3)];
                    let target_buf = rng.gen_range(0..3) as u64;
                    let can_read = shadow.auth_read(active_actor, target_buf).is_ok();

                    // Deterministic schedule guarantees all core branches hit on every seed,
                    // while PRNG drives choices, spans, payloads, and interleavings.
                    let branch = match step_idx {
                        0 => 0,   // view
                        1 => 1,   // selection
                        2 => 2,   // bookmark
                        3 => 3,   // overlay (Evict)
                        4 => 4,   // overlay (RetainEmpty)
                        5 => 5,   // properties
                        6 => 6,   // register
                        7 => 7,   // kill
                        8 => 8,   // multi-splice commit
                        9 => 9,   // multi-buffer commit
                        10 => 10, // stale-base rebase commit
                        11 => 11, // save receipt
                        12 => 12, // noop + duplicate retry
                        13 => 13, // policy restriction
                        14 => 14, // rejected ops burst
                        15 => 15, // collapsing edit (evict + retain)
                        _ => (token as usize) % 16,
                    };

                    match branch {
                        0 if can_read => {
                            let snap = seq.snapshot();
                            let text = snap.catalog().get(BufferId::new(target_buf)).unwrap();
                            // If actor already has a selection on this buffer, widen to full so set_view succeeds.
                            let (s, e) = if session
                                .selection(ActorId::new(active_actor), BufferId::new(target_buf))
                                .unwrap()
                                .is_some()
                            {
                                (0, text.len_bytes())
                            } else {
                                let sp = random_span(&mut rng, text);
                                (sp.start().as_usize(), sp.end().as_usize())
                            };
                            let ev = ShadowEvent::SetView {
                                actor: active_actor,
                                buffer: target_buf,
                                start: s,
                                end: e,
                            };
                            let before =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            apply_private_event_to_session(
                                &mut session,
                                &snap,
                                &mut actor_props,
                                &ev,
                            );
                            shadow.apply_event(ev.clone());
                            actions.push(EditStreamAction::Private(ev));
                            let after =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            assert_private_isolation("gen_set_view", active_actor, &before, &after);
                            coverage.private_view_set += 1;
                        }
                        1 if can_read => {
                            let snap = seq.snapshot();
                            let bid = BufferId::new(target_buf);
                            let aid = ActorId::new(active_actor);
                            let text = snap.catalog().get(bid).unwrap();
                            if session.view(aid, bid).unwrap().is_none() {
                                let view_ev = ShadowEvent::SetView {
                                    actor: active_actor,
                                    buffer: target_buf,
                                    start: 0,
                                    end: text.len_bytes(),
                                };
                                apply_private_event_to_session(
                                    &mut session,
                                    &snap,
                                    &mut actor_props,
                                    &view_ev,
                                );
                                shadow.apply_event(view_ev.clone());
                                actions.push(EditStreamAction::Private(view_ev));
                                live_history.push(observe_now(
                                    &seq,
                                    &dirty,
                                    &prod_policy,
                                    &session,
                                    &actor_props,
                                ));
                                coverage.private_view_set += 1;
                            }
                            let view = session.view(aid, bid).unwrap().unwrap();
                            let bounds: Vec<usize> = text
                                .char_boundaries()
                                .into_iter()
                                .map(|b| b.as_usize())
                                .filter(|&b| {
                                    b >= view.visible().start().as_usize()
                                        && b <= view.visible().end().as_usize()
                                })
                                .collect();
                            let anchor = *rng.choose(&bounds).unwrap();
                            let head = *rng.choose(&bounds).unwrap();
                            let ev = ShadowEvent::SetSelection {
                                actor: active_actor,
                                buffer: target_buf,
                                anchor,
                                head,
                            };
                            let before =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            apply_private_event_to_session(
                                &mut session,
                                &snap,
                                &mut actor_props,
                                &ev,
                            );
                            shadow.apply_event(ev.clone());
                            actions.push(EditStreamAction::Private(ev));
                            let after =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            assert_private_isolation(
                                "gen_set_selection",
                                active_actor,
                                &before,
                                &after,
                            );
                            coverage.private_selection_set += 1;
                        }
                        2 if can_read => {
                            let snap = seq.snapshot();
                            let text = snap.catalog().get(BufferId::new(target_buf)).unwrap();
                            let offset = random_boundary(&mut rng, text).as_usize();
                            let ev = ShadowEvent::SetBookmark {
                                actor: active_actor,
                                name: if rng.gen_bool() { "bm0" } else { "bm1" }.into(),
                                buffer: target_buf,
                                offset,
                                affinity: if rng.gen_bool() {
                                    ShadowAffinity::Left
                                } else {
                                    ShadowAffinity::Right
                                },
                            };
                            let before =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            apply_private_event_to_session(
                                &mut session,
                                &snap,
                                &mut actor_props,
                                &ev,
                            );
                            shadow.apply_event(ev.clone());
                            actions.push(EditStreamAction::Private(ev));
                            let after =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            assert_private_isolation(
                                "gen_set_bookmark",
                                active_actor,
                                &before,
                                &after,
                            );
                            coverage.private_bookmark_set += 1;
                        }
                        3 | 4 if can_read => {
                            let snap = seq.snapshot();
                            let text = snap.catalog().get(BufferId::new(target_buf)).unwrap();
                            let sp = random_span(&mut rng, text);
                            let (id, collapse) = if branch == 3 {
                                (1, ShadowCollapse::Evict)
                            } else {
                                (2, ShadowCollapse::RetainEmpty)
                            };
                            let ev = ShadowEvent::SetOverlay {
                                actor: active_actor,
                                overlay: ShadowOverlay {
                                    buffer: target_buf,
                                    namespace: 1,
                                    id,
                                    owner: active_actor,
                                    start: sp.start().as_usize(),
                                    end: sp.end().as_usize(),
                                    start_affinity: if rng.gen_bool() {
                                        ShadowAffinity::Left
                                    } else {
                                        ShadowAffinity::Right
                                    },
                                    end_affinity: if rng.gen_bool() {
                                        ShadowAffinity::Left
                                    } else {
                                        ShadowAffinity::Right
                                    },
                                    priority: rng.gen_range(0..5) as u32,
                                    collapse,
                                    label: format!("ov-{id}"),
                                },
                            };
                            let before =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            apply_private_event_to_session(
                                &mut session,
                                &snap,
                                &mut actor_props,
                                &ev,
                            );
                            shadow.apply_event(ev.clone());
                            actions.push(EditStreamAction::Private(ev));
                            let after =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            assert_private_isolation(
                                "gen_set_overlay",
                                active_actor,
                                &before,
                                &after,
                            );
                            coverage.private_overlay_set += 1;
                        }
                        5 => {
                            let snap = seq.snapshot();
                            let id = if rng.gen_bool() { 1 } else { 2 };
                            let mut props = BTreeMap::new();
                            props.insert(
                                "k".into(),
                                if rng.gen_bool() { "v1" } else { "v2" }.into(),
                            );
                            let ev = ShadowEvent::SetOverlayProps {
                                actor: active_actor,
                                buffer: target_buf,
                                namespace: 1,
                                id,
                                props,
                            };
                            let before =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            apply_private_event_to_session(
                                &mut session,
                                &snap,
                                &mut actor_props,
                                &ev,
                            );
                            shadow.apply_event(ev.clone());
                            actions.push(EditStreamAction::Private(ev));
                            let after =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            assert_private_isolation(
                                "gen_set_overlay_props",
                                active_actor,
                                &before,
                                &after,
                            );
                            coverage.private_property_update += 1;
                        }
                        6 => {
                            let snap = seq.snapshot();
                            let kind = match rng.gen_range(0..3) {
                                0 => ShadowRegKind::Charwise,
                                1 => ShadowRegKind::Linewise,
                                _ => ShadowRegKind::Blockwise,
                            };
                            let ev = ShadowEvent::PutRegister {
                                actor: active_actor,
                                name: "reg".into(),
                                entry: ShadowRegister {
                                    text: random_fragment(&mut rng).to_owned(),
                                    kind,
                                },
                            };
                            let before =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            apply_private_event_to_session(
                                &mut session,
                                &snap,
                                &mut actor_props,
                                &ev,
                            );
                            shadow.apply_event(ev.clone());
                            actions.push(EditStreamAction::Private(ev));
                            let after =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            assert_private_isolation(
                                "gen_put_register",
                                active_actor,
                                &before,
                                &after,
                            );
                            coverage.private_register_put += 1;
                        }
                        7 => {
                            let snap = seq.snapshot();
                            let kind = match rng.gen_range(0..3) {
                                0 => ShadowRegKind::Charwise,
                                1 => ShadowRegKind::Linewise,
                                _ => ShadowRegKind::Blockwise,
                            };
                            let ev = ShadowEvent::PushKill {
                                actor: active_actor,
                                entry: ShadowRegister {
                                    text: random_fragment(&mut rng).to_owned(),
                                    kind,
                                },
                            };
                            let before =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            apply_private_event_to_session(
                                &mut session,
                                &snap,
                                &mut actor_props,
                                &ev,
                            );
                            shadow.apply_event(ev.clone());
                            actions.push(EditStreamAction::Private(ev));
                            let after =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            assert_private_isolation(
                                "gen_push_kill",
                                active_actor,
                                &before,
                                &after,
                            );
                            coverage.private_kill_push += 1;
                        }
                        8 | 9 => {
                            let snap = seq.snapshot();
                            let is_multi_buf = branch == 9;
                            let buffers_to_edit = if is_multi_buf {
                                vec![BufferId::new(0), BufferId::new(1)]
                            } else {
                                vec![BufferId::new(target_buf)]
                            };
                            let mut patches = Vec::new();
                            for &bid in &buffers_to_edit {
                                let text = snap.catalog().get(bid).unwrap();
                                let patch = random_non_noop_patch(&mut rng, text, branch == 8);
                                if patch.splices().len() >= 2 {
                                    coverage.edit_multi_splice += 1;
                                }
                                patches.push((bid, patch));
                            }
                            // Save a stale-base candidate on buffer 2 before committing on buffer 0/1.
                            if is_multi_buf {
                                let b2 = BufferId::new(2);
                                stale_candidate = Some((
                                    snap.revision(),
                                    b2,
                                    snap.catalog().get(b2).unwrap().clone(),
                                ));
                            }
                            let txn = Transaction::new(
                                EditIntent::for_actor(
                                    ActorId::new(active_actor),
                                    Nonce::new(nonce),
                                    snap.revision(),
                                ),
                                patches,
                            )
                            .unwrap();
                            nonce += 1;
                            let (outcome, maybe_ev) = submit_with_event(&mut seq, txn.clone());
                            if let Some(ws_ev) = maybe_ev {
                                let receipt = outcome.committed_receipt().unwrap();
                                log.record_workspace(&seq, receipt).unwrap();
                                let delivery = log.events().last().unwrap().clone();
                                snapshots.record(seq.snapshot()).unwrap();
                                session.advance(&delivery).unwrap();
                                let shadow_patches = ws_ev
                                    .patches()
                                    .iter()
                                    .map(|(b, p)| (b.raw(), shadow_splices_from_patch(p)))
                                    .collect();
                                shadow.apply_event(ShadowEvent::Edit {
                                    patches: shadow_patches,
                                });
                                actions.push(EditStreamAction::Commit {
                                    ws_event: ws_ev,
                                    delivery: Box::new(delivery),
                                });
                                last_committed_txn = Some((txn, outcome));
                                coverage.edit_committed += 1;
                                if is_multi_buf {
                                    coverage.edit_multi_buffer += 1;
                                }
                            } else {
                                assert!(outcome.is_noop());
                                coverage.edit_noop += 1;
                            }
                        }
                        10 => {
                            // Stale-base disjoint edit on buffer 2 rebased over intervening edits on 0/1.
                            if let Some((base_rev, bid, base_text)) = stale_candidate.take() {
                                if base_rev < seq.revision() {
                                    let patch = Patch::new(vec![Splice::insert(
                                        base_text.validate_offset(0).unwrap(),
                                        Text::from_str("r"),
                                    )])
                                    .unwrap();
                                    let txn = Transaction::new(
                                        EditIntent::for_actor(
                                            ActorId::new(2),
                                            Nonce::new(nonce),
                                            base_rev,
                                        ),
                                        vec![(bid, patch)],
                                    )
                                    .unwrap();
                                    nonce += 1;
                                    let (outcome, maybe_ev) = submit_with_event(&mut seq, txn);
                                    let ws_ev = maybe_ev.expect("disjoint stale edit rebases");
                                    let receipt = outcome.committed_receipt().unwrap();
                                    log.record_workspace(&seq, receipt).unwrap();
                                    let delivery = log.events().last().unwrap().clone();
                                    snapshots.record(seq.snapshot()).unwrap();
                                    session.advance(&delivery).unwrap();
                                    let shadow_patches = ws_ev
                                        .patches()
                                        .iter()
                                        .map(|(b, p)| (b.raw(), shadow_splices_from_patch(p)))
                                        .collect();
                                    shadow.apply_event(ShadowEvent::Edit {
                                        patches: shadow_patches,
                                    });
                                    actions.push(EditStreamAction::Commit {
                                        ws_event: ws_ev,
                                        delivery: Box::new(delivery),
                                    });
                                    coverage.edit_committed += 1;
                                    coverage.edit_rebased += 1;
                                }
                            }
                        }
                        11 => {
                            let snap = seq.snapshot();
                            let bid = BufferId::new(target_buf);
                            let receipt = SaveReceipt {
                                buffer: bid,
                                version: snap.revision(),
                                content: snap.catalog().get(bid).unwrap().clone(),
                            };
                            dirty.acknowledge(receipt.clone()).unwrap();
                            shadow.apply_event(ShadowEvent::Save { buffer: target_buf });
                            actions.push(EditStreamAction::Save(receipt));
                            coverage.save_acknowledged += 1;
                        }
                        12 => {
                            // Exercise both effective no-op AND idempotent duplicate retry.
                            let before_sess = session.clone();
                            let before_obs =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            let empty_txn = Transaction::new(
                                EditIntent::for_actor(
                                    ActorId::new(1),
                                    Nonce::new(nonce),
                                    seq.revision(),
                                ),
                                vec![],
                            )
                            .unwrap();
                            nonce += 1;
                            let (noop_out, noop_ev) = submit_with_event(&mut seq, empty_txn);
                            assert!(noop_out.is_noop());
                            assert_eq!(noop_ev, None);
                            let after_noop =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            assert_atomicity_preserved(
                                "noop_txn",
                                &before_sess,
                                &session,
                                &before_obs,
                                &after_noop,
                            );
                            coverage.edit_noop += 1;

                            if let Some((orig_txn, orig_out)) = last_committed_txn.clone() {
                                let (retry_out, retry_ev) = submit_with_event(&mut seq, orig_txn);
                                assert_eq!(retry_out, orig_out);
                                assert_eq!(retry_ev, None);
                                let after_retry =
                                    observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                                assert_atomicity_preserved(
                                    "duplicate_retry",
                                    &before_sess,
                                    &session,
                                    &before_obs,
                                    &after_retry,
                                );
                                coverage.edit_duplicate_retry += 1;
                            }
                        }
                        13 => {
                            // Restrict actor 2 from buffer 1 (or toggle back on later random hits).
                            let mut next_policy = ShadowPolicy::default();
                            next_policy.grant_actor(1, ShadowCap::Read);
                            next_policy.grant_actor(3, ShadowCap::Read);
                            next_policy.grant_buffer(2, 0, ShadowCap::Read);
                            if step_idx > 16 && rng.gen_bool() {
                                next_policy.grant_buffer(2, 1, ShadowCap::Read);
                            }
                            next_policy.grant_buffer(2, 2, ShadowCap::Read);
                            let snap = seq.snapshot();
                            let ev = ShadowEvent::SetPolicy {
                                policy: next_policy.clone(),
                            };
                            prod_policy = next_policy.to_access_policy();
                            apply_private_event_to_session(
                                &mut session,
                                &snap,
                                &mut actor_props,
                                &ev,
                            );
                            shadow.apply_event(ev.clone());
                            actions.push(EditStreamAction::Private(ev));
                            coverage.policy_updated += 1;
                        }
                        14 => {
                            // Failure atomicity burst: closed buffer, unknown buffer, revoked actor,
                            // unauthorized actor, invalid coords, stale revision.
                            let before_sess = session.clone();
                            let before_obs =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            let snap = seq.snapshot();

                            // 1) Closed buffer commit rejection
                            let closed_txn = Transaction::new(
                                EditIntent::for_actor(
                                    ActorId::new(1),
                                    Nonce::new(nonce),
                                    snap.revision(),
                                ),
                                vec![(
                                    closed_id,
                                    Patch::new(vec![Splice::insert(
                                        Text::empty().validate_offset(0).unwrap(),
                                        Text::from_str("x"),
                                    )])
                                    .unwrap(),
                                )],
                            )
                            .unwrap();
                            nonce += 1;
                            let (out, ev) = submit_with_event(&mut seq, closed_txn);
                            assert!(matches!(
                                out,
                                SubmitOutcome::Rejected {
                                    reason: RejectReason::BufferClosed,
                                    ..
                                }
                            ));
                            assert_eq!(ev, None);
                            assert_eq!(
                                session.read(ActorId::new(1), closed_id),
                                Err(SessionError::Access(AccessError::BufferClosed(closed_id)))
                            );
                            coverage.rejected_closed_buffer += 1;

                            // 2) Unknown buffer commit rejection
                            let unknown_txn = Transaction::new(
                                EditIntent::for_actor(
                                    ActorId::new(1),
                                    Nonce::new(nonce),
                                    snap.revision(),
                                ),
                                vec![(
                                    unknown_id,
                                    Patch::new(vec![Splice::insert(
                                        Text::empty().validate_offset(0).unwrap(),
                                        Text::from_str("x"),
                                    )])
                                    .unwrap(),
                                )],
                            )
                            .unwrap();
                            nonce += 1;
                            let (out, ev) = submit_with_event(&mut seq, unknown_txn);
                            assert!(matches!(
                                out,
                                SubmitOutcome::Rejected {
                                    reason: RejectReason::UnknownBuffer,
                                    ..
                                }
                            ));
                            assert_eq!(ev, None);
                            coverage.rejected_unknown_buffer += 1;

                            // 3) Revoked actor commit & session rejection
                            let rev_txn = Transaction::new(
                                EditIntent::for_actor(
                                    revoked_actor,
                                    Nonce::new(nonce),
                                    snap.revision(),
                                ),
                                vec![(
                                    BufferId::new(0),
                                    Patch::new(vec![Splice::insert(
                                        Text::empty().validate_offset(0).unwrap(),
                                        Text::from_str("x"),
                                    )])
                                    .unwrap(),
                                )],
                            )
                            .unwrap();
                            nonce += 1;
                            let (out, ev) = submit_with_event(&mut seq, rev_txn);
                            assert!(matches!(
                                out,
                                SubmitOutcome::Rejected {
                                    reason: RejectReason::RevokedActor,
                                    ..
                                }
                            ));
                            assert_eq!(ev, None);
                            assert_eq!(
                                session.read(revoked_actor, BufferId::new(0)),
                                Err(SessionError::Access(AccessError::InactiveActor(
                                    revoked_actor
                                )))
                            );
                            coverage.rejected_revoked_actor += 1;

                            // 4) Unauthorized actor session write rejection (actor 2 lost access to buffer 1 at step 13)
                            let b1_full =
                                BufferView::full(snap.catalog(), BufferId::new(1)).unwrap();
                            if shadow.auth_read(2, 1).is_err() {
                                assert_eq!(
                                    session.set_view(ActorId::new(2), b1_full),
                                    Err(SessionError::Access(AccessError::Denied {
                                        actor: ActorId::new(2),
                                        buffer: BufferId::new(1),
                                        capability: Capability::Read,
                                    }))
                                );
                                coverage.rejected_unauthorized_actor += 1;
                            }

                            // 5) Invalid coordinates (non-char boundary or stale span)
                            let foreign = Text::from_str("01234567890123456789");
                            let bad_bm = Bookmark {
                                buffer: BufferId::new(0),
                                anchor: Anchor {
                                    offset: foreign.validate_offset(foreign.len_bytes()).unwrap(),
                                    affinity: Affinity::Left,
                                },
                                revision: snap.revision(),
                            };
                            if snap.catalog().get(BufferId::new(0)).unwrap().len_bytes()
                                < foreign.len_bytes()
                            {
                                assert_eq!(
                                    session.set_bookmark(
                                        ActorId::new(1),
                                        BookmarkName::new("bad").unwrap(),
                                        bad_bm,
                                    ),
                                    Err(SessionError::InvalidState)
                                );
                                coverage.rejected_invalid_coords += 1;
                            }

                            // 6) Stale revision / future revision rejection
                            let future_rev = snap.revision().advance(5).unwrap();
                            let future_txn = Transaction::new(
                                EditIntent::for_actor(
                                    ActorId::new(1),
                                    Nonce::new(nonce),
                                    future_rev,
                                ),
                                vec![],
                            )
                            .unwrap();
                            nonce += 1;
                            let (out, ev) = submit_with_event(&mut seq, future_txn);
                            assert!(matches!(
                                out,
                                SubmitOutcome::Rejected {
                                    reason: RejectReason::RevisionMismatch,
                                    ..
                                }
                            ));
                            assert_eq!(ev, None);
                            if let Some(last_del) = log.events().last() {
                                assert_eq!(
                                    session.advance(last_del),
                                    Err(SessionError::InvalidTransition)
                                );
                            }
                            coverage.rejected_stale_revision += 1;

                            let after_obs =
                                observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                            assert_atomicity_preserved(
                                "rejected_ops_burst",
                                &before_sess,
                                &session,
                                &before_obs,
                                &after_obs,
                            );
                        }
                        15 => {
                            // Explicitly set both Evict and RetainEmpty overlays on buffer 0 for actor 1,
                            // then delete the full buffer content to force both eviction and collapse!
                            let snap = seq.snapshot();
                            let b0 = BufferId::new(0);
                            let text = snap.catalog().get(b0).unwrap();
                            if !text.is_empty() {
                                for (id, collapse) in
                                    [(1, ShadowCollapse::Evict), (2, ShadowCollapse::RetainEmpty)]
                                {
                                    let ov_ev = ShadowEvent::SetOverlay {
                                        actor: 1,
                                        overlay: ShadowOverlay {
                                            buffer: 0,
                                            namespace: 1,
                                            id,
                                            owner: 1,
                                            start: 0,
                                            end: text.len_bytes(),
                                            start_affinity: ShadowAffinity::Left,
                                            end_affinity: ShadowAffinity::Right,
                                            priority: id as u32,
                                            collapse,
                                            label: format!("col-{id}"),
                                        },
                                    };
                                    apply_private_event_to_session(
                                        &mut session,
                                        &snap,
                                        &mut actor_props,
                                        &ov_ev,
                                    );
                                    shadow.apply_event(ov_ev.clone());
                                    actions.push(EditStreamAction::Private(ov_ev));
                                    live_history.push(observe_now(
                                        &seq,
                                        &dirty,
                                        &prod_policy,
                                        &session,
                                        &actor_props,
                                    ));
                                }
                                let del = Patch::new(vec![Splice::delete(ByteSpan::full_of(text))])
                                    .unwrap();
                                let txn = Transaction::new(
                                    EditIntent::for_actor(
                                        ActorId::new(1),
                                        Nonce::new(nonce),
                                        snap.revision(),
                                    ),
                                    vec![(b0, del)],
                                )
                                .unwrap();
                                nonce += 1;
                                let (outcome, maybe_ev) = submit_with_event(&mut seq, txn);
                                let ws_ev = maybe_ev.expect("nonempty deletion commits");
                                let receipt = outcome.committed_receipt().unwrap();
                                log.record_workspace(&seq, receipt).unwrap();
                                let delivery = log.events().last().unwrap().clone();
                                snapshots.record(seq.snapshot()).unwrap();
                                session.advance(&delivery).unwrap();
                                let shadow_patches = ws_ev
                                    .patches()
                                    .iter()
                                    .map(|(b, p)| (b.raw(), shadow_splices_from_patch(p)))
                                    .collect();
                                shadow.apply_event(ShadowEvent::Edit {
                                    patches: shadow_patches,
                                });
                                actions.push(EditStreamAction::Commit {
                                    ws_event: ws_ev,
                                    delivery: Box::new(delivery),
                                });
                                assert_eq!(
                                    session
                                        .overlay(
                                            ActorId::new(1),
                                            AnnotationKey {
                                                buffer: b0,
                                                namespace: NamespaceId::new(1),
                                                id: AnnotationId::new(1),
                                            },
                                        )
                                        .unwrap(),
                                    None
                                );
                                assert!(session
                                    .overlay(
                                        ActorId::new(1),
                                        AnnotationKey {
                                            buffer: b0,
                                            namespace: NamespaceId::new(1),
                                            id: AnnotationId::new(2),
                                        },
                                    )
                                    .unwrap()
                                    .unwrap()
                                    .span
                                    .is_empty());
                                coverage.edit_committed += 1;
                                coverage.overlay_evicted += 1;
                                coverage.overlay_retained += 1;
                            }
                        }
                        _ => {}
                    }

                    // Always check live == shadow after every step!
                    let now_obs = observe_now(&seq, &dirty, &prod_policy, &session, &actor_props);
                    assert_observations_equal(
                        &format!("edit_stream_live_vs_shadow_step_{step_idx}"),
                        &shadow.observe(&probe),
                        &now_obs,
                    );
                    if actions.len() == live_history.len() {
                        live_history.push(now_obs);
                        if actions.len() & 3 == 0 {
                            checkpoints.push((
                                actions.len(),
                                seq.snapshot(),
                                dirty.clone(),
                                prod_policy.clone(),
                                session.clone(),
                                actor_props.clone(),
                            ));
                        }
                    }
                }

                assert_eq!(live_history.len(), actions.len() + 1);

                // Prefix replay equivalence for EVERY prefix 0..=actions.len()!
                for k in 0..=actions.len() {
                    let replayed = replay_edit_prefix(
                        &initial_catalog,
                        &initial_base_snap,
                        &initial_dirty,
                        &registry,
                        &init_policy,
                        &actions[..k],
                        &probe,
                    );
                    assert_observations_equal(
                        &format!("edit_stream_prefix_replay_{k}"),
                        &live_history[k],
                        &replayed,
                    );
                    let shadow_replayed =
                        ShadowWorkspace::replay_prefix(&initial_shadow, &shadow.events[..k]);
                    assert_observations_equal(
                        &format!("edit_stream_shadow_prefix_replay_{k}"),
                        &live_history[k],
                        &shadow_replayed.observe(&probe),
                    );
                    coverage.prefix_replays_checked += 1;
                }

                // Checkpoint / snapshot replay from every saved checkpoint c to tip!
                for (c, cp_snap, cp_dirty, cp_policy, cp_session, cp_props) in &checkpoints {
                    let mut sess = cp_session.clone();
                    let mut d = cp_dirty.clone();
                    let mut pol = cp_policy.clone();
                    let mut props = cp_props.clone();
                    let mut snap = cp_snap.clone();
                    for action in &actions[*c..] {
                        match action {
                            EditStreamAction::Private(ev) => {
                                if let ShadowEvent::SetPolicy { policy } = ev {
                                    pol = policy.to_access_policy();
                                }
                                apply_private_event_to_session(&mut sess, &snap, &mut props, ev);
                            }
                            EditStreamAction::Commit { delivery, .. } => {
                                sess.advance(delivery).unwrap();
                                snap = delivery.after().clone();
                            }
                            EditStreamAction::Save(receipt) => {
                                d.acknowledge(receipt.clone()).unwrap();
                            }
                        }
                    }
                    let dirty_fn =
                        |b: BufferId| snap.catalog().get(b).is_none_or(|t| d.is_dirty(b, t));
                    let cp_obs = observe_live(&LiveObservationInput {
                        snapshot: &snap,
                        registry: &registry,
                        policy: &pol,
                        session: &sess,
                        dirty: &dirty_fn,
                        actor_props: &props,
                        probe: &probe,
                    });
                    assert_observations_equal(
                        &format!("edit_stream_checkpoint_replay_from_{c}"),
                        live_history.last().unwrap(),
                        &cp_obs,
                    );
                    // Also verify DeliveryLog::catch_up from snapshot revision!
                    match log.catch_up(cp_snap.revision()).unwrap() {
                        CatchUp::Events(suffix) => {
                            let (cat, rev) = replay_delivery(cp_snap, &suffix).unwrap();
                            assert_eq!(&cat, seq.snapshot().catalog());
                            assert_eq!(rev, seq.revision());
                        }
                        CatchUp::Snapshot(_) => panic!("uncompacted log must return events"),
                    }
                    coverage.checkpoint_replays_checked += 1;
                }

                // Verify compacted DeliveryLog falls back cleanly to exact tip snapshot!
                let tip = log.tip();
                if tip > Revision::ZERO {
                    let mut compacted = log.clone();
                    compacted.compact(tip).unwrap();
                    assert_eq!(
                        compacted.catch_up(Revision::ZERO).unwrap(),
                        CatchUp::Snapshot(seq.snapshot())
                    );
                }
            },
        );

        assert!(coverage.private_view_set > 0, "private_view_set uncovered");
        assert!(
            coverage.private_selection_set > 0,
            "private_selection_set uncovered"
        );
        assert!(
            coverage.private_bookmark_set > 0,
            "private_bookmark_set uncovered"
        );
        assert!(
            coverage.private_overlay_set > 0,
            "private_overlay_set uncovered"
        );
        assert!(
            coverage.private_property_update > 0,
            "private_property_update uncovered"
        );
        assert!(
            coverage.private_register_put > 0,
            "private_register_put uncovered"
        );
        assert!(
            coverage.private_kill_push > 0,
            "private_kill_push uncovered"
        );
        assert!(coverage.edit_committed > 0, "edit_committed uncovered");
        assert!(
            coverage.edit_multi_buffer > 0,
            "edit_multi_buffer uncovered"
        );
        assert!(
            coverage.edit_multi_splice > 0,
            "edit_multi_splice uncovered"
        );
        assert!(coverage.edit_rebased > 0, "edit_rebased uncovered");
        assert!(coverage.edit_noop > 0, "edit_noop uncovered");
        assert!(
            coverage.edit_duplicate_retry > 0,
            "edit_duplicate_retry uncovered"
        );
        assert!(
            coverage.save_acknowledged > 0,
            "save_acknowledged uncovered"
        );
        assert!(coverage.policy_updated > 0, "policy_updated uncovered");
        assert!(
            coverage.rejected_closed_buffer > 0,
            "rejected_closed_buffer uncovered"
        );
        assert!(
            coverage.rejected_unknown_buffer > 0,
            "rejected_unknown_buffer uncovered"
        );
        assert!(
            coverage.rejected_revoked_actor > 0,
            "rejected_revoked_actor uncovered"
        );
        assert!(
            coverage.rejected_unauthorized_actor > 0,
            "rejected_unauthorized_actor uncovered"
        );
        assert!(
            coverage.rejected_invalid_coords > 0,
            "rejected_invalid_coords uncovered"
        );
        assert!(
            coverage.rejected_stale_revision > 0,
            "rejected_stale_revision uncovered"
        );
        assert!(coverage.overlay_evicted > 0, "overlay_evicted uncovered");
        assert!(coverage.overlay_retained > 0, "overlay_retained uncovered");
        assert!(
            coverage.prefix_replays_checked > 0,
            "prefix_replays_checked uncovered"
        );
        assert!(
            coverage.checkpoint_replays_checked > 0,
            "checkpoint_replays_checked uncovered"
        );
    }

    // -------------------------------------------------------------------------
    // Test 3: Generated multi-actor lifecycle + checkpoint stream & prefix replay
    // -------------------------------------------------------------------------

    #[derive(Default, Debug)]
    struct LifecycleStreamCoverage {
        opens: usize,
        edits: usize,
        saves: usize,
        closes_clean: usize,
        closes_discard: usize,
        private_updates: usize,
        policy_updates: usize,
        rejected_dirty_close: usize,
        rejected_duplicate_name: usize,
        rejected_closed_op: usize,
        prefix_replays_checked: usize,
        checkpoint_restores_checked: usize,
    }

    #[derive(Clone)]
    enum LifecycleAction {
        Authority(CheckpointOp),
        Private(ShadowEvent),
    }

    fn apply_lifecycle_action(
        authority: &mut CheckpointState,
        lifecycle: &mut BufferLifecycle,
        session: &mut WorkspaceSession,
        policy: &mut AccessPolicy,
        actor_props: &mut ActorOverlayProps,
        action: &LifecycleAction,
    ) {
        match action {
            LifecycleAction::Authority(op) => {
                let outcome = authority.apply(op.clone()).expect("recorded op replays");
                match op {
                    CheckpointOp::Open { text, .. } => {
                        let id = outcome.buffer.expect("open returns buffer id");
                        lifecycle.open(id, text.clone()).unwrap();
                        session.open(authority.snapshot(), id).unwrap();
                    }
                    CheckpointOp::Close {
                        buffer, policy: cp, ..
                    } => {
                        lifecycle.close(*buffer, *cp).unwrap();
                        session.close(authority.snapshot(), *buffer).unwrap();
                    }
                    CheckpointOp::Edit { .. } => {
                        let delivery = authority
                            .delivery()
                            .events()
                            .last()
                            .expect("edit appends delivery event")
                            .clone();
                        for (bid, _) in delivery.workspace_event().patches() {
                            let new_text =
                                authority.snapshot().catalog().get(*bid).unwrap().clone();
                            lifecycle.update(*bid, new_text).unwrap();
                        }
                        session.advance(&delivery).unwrap();
                    }
                    CheckpointOp::MarkSaved { buffer } => {
                        lifecycle.mark_saved(*buffer).unwrap();
                    }
                }
            }
            LifecycleAction::Private(ev) => {
                if let ShadowEvent::SetPolicy { policy: sp } = ev {
                    *policy = sp.to_access_policy();
                }
                let snap = authority.snapshot();
                apply_private_event_to_session(session, &snap, actor_props, ev);
            }
        }
    }

    #[test]
    fn generated_multi_actor_lifecycle_checkpoint_prefix_replay() {
        let mut coverage = LifecycleStreamCoverage::default();
        assert_law(
            SEEDS,
            |rng: &mut Rng| (0..20).map(|_| rng.next_u64()).collect::<Vec<_>>(),
            |tokens| {
                let mut rng = Rng::new(tokens[0] ^ 0x9b4d_7123_55af_0081);
                let actor_specs = [(1, ShadowRole::Human), (2, ShadowRole::Agent)];
                let registry = build_registry(&actor_specs);
                let mut init_policy = ShadowPolicy::default();
                init_policy.grant_default(ShadowCap::Read);
                let mut prod_policy = init_policy.to_access_policy();

                let mut authority = CheckpointState::new(registry.clone());
                let mut lifecycle = BufferLifecycle::new();
                let mut session = WorkspaceSession::new(
                    authority.snapshot(),
                    registry.clone(),
                    prod_policy.clone(),
                );
                let mut actor_props: ActorOverlayProps = BTreeMap::new();
                let mut shadow = ShadowWorkspace::new(&[], &actor_specs, init_policy.clone());
                let initial_shadow = shadow.clone();

                let probe = ObservationProbe {
                    actors: vec![1, 2],
                    buffers: vec![0, 1, 2, 3, 4, 9],
                    bookmark_names: vec!["bm".into()],
                    namespaces: vec![1],
                    overlay_ids: vec![1],
                    register_names: vec!["reg".into()],
                    search_patterns: PROBE_PATTERNS.to_vec(),
                    budget: ContextBudget::with_limits(14, Some(7), Some(3)).unwrap(),
                };

                let observe_now =
                    |authority: &CheckpointState,
                     lifecycle: &BufferLifecycle,
                     policy: &AccessPolicy,
                     session: &WorkspaceSession,
                     actor_props: &ActorOverlayProps| {
                        let snap = authority.snapshot();
                        assert_eq!(snap.catalog(), lifecycle.snapshot().catalog());
                        for id in snap.catalog().live_ids() {
                            assert_eq!(
                                authority.is_dirty(id).unwrap(),
                                lifecycle.is_dirty(id).unwrap(),
                                "CheckpointState and BufferLifecycle dirty flags must agree"
                            );
                        }
                        let dirty_fn = |b: BufferId| authority.is_dirty(b).unwrap_or(true);
                        observe_live(&LiveObservationInput {
                            snapshot: &snap,
                            registry: &registry,
                            policy,
                            session,
                            dirty: &dirty_fn,
                            actor_props,
                            probe: &probe,
                        })
                    };

                let mut actions: Vec<LifecycleAction> = Vec::new();
                let mut live_history: Vec<WorkspaceObservation> = Vec::new();
                let mut saved_checkpoints: Vec<(
                    usize,
                    Checkpoint,
                    AccessPolicy,
                    ShadowWorkspace,
                    ActorOverlayProps,
                )> = Vec::new();

                let init_obs =
                    observe_now(&authority, &lifecycle, &prod_policy, &session, &actor_props);
                assert_observations_equal("lifecycle_step_0", &shadow.observe(&probe), &init_obs);
                live_history.push(init_obs);
                saved_checkpoints.push((
                    0,
                    authority.checkpoint(),
                    prod_policy.clone(),
                    shadow.clone(),
                    actor_props.clone(),
                ));

                let mut req_id = 1u64;
                let mut nonce = 1u64;
                let mut open_seq = 0u64;

                for (step_idx, &token) in tokens.iter().enumerate() {
                    let actor = if rng.gen_bool() { 1u64 } else { 2u64 };
                    let live_ids = authority.snapshot().catalog().live_ids();
                    let branch = match step_idx {
                        0 => 0,  // open buf 0
                        1 => 0,  // open buf 1
                        2 => 1,  // private state populate
                        3 => 2,  // edit buf 0 (makes it dirty)
                        4 => 6,  // reject dirty close RequireClean + duplicate name
                        5 => 3,  // save buf 0 -> clean
                        6 => 4,  // close buf 0 RequireClean
                        7 => 7,  // reject closed buffer op
                        8 => 0,  // open buf 2
                        9 => 2,  // edit buf 1 (dirty)
                        10 => 5, // close buf 1 Discard while dirty
                        11 => 8, // policy restrict
                        _ => (token as usize) % 9,
                    };

                    match branch {
                        0 if open_seq < 5 => {
                            let name = format!("doc-{open_seq}");
                            open_seq += 1;
                            let text = Text::from_str(if open_seq == 1 {
                                "a界😀b"
                            } else {
                                random_fragment(&mut rng)
                            });
                            let op = CheckpointOp::Open {
                                request: RequestId::new(req_id),
                                intent: EditIntent::for_actor(
                                    ActorId::new(actor),
                                    Nonce::new(nonce),
                                    authority.revision(),
                                ),
                                name,
                                text: text.clone(),
                            };
                            req_id += 1;
                            nonce += 1;
                            let action = LifecycleAction::Authority(op);
                            apply_lifecycle_action(
                                &mut authority,
                                &mut lifecycle,
                                &mut session,
                                &mut prod_policy,
                                &mut actor_props,
                                &action,
                            );
                            let opened = *authority.snapshot().catalog().live_ids().last().unwrap();
                            shadow.apply_event(ShadowEvent::Open {
                                buffer: opened.raw(),
                                text: text.as_str().to_owned(),
                                clean_on_open: true,
                            });
                            actions.push(action);
                            coverage.opens += 1;
                        }
                        1 if !live_ids.is_empty() => {
                            let bid = *rng.choose(&live_ids).unwrap();
                            if shadow.auth_read(actor, bid.raw()).is_ok() {
                                let snap = authority.snapshot();
                                let text = snap.catalog().get(bid).unwrap();
                                let view_ev = ShadowEvent::SetView {
                                    actor,
                                    buffer: bid.raw(),
                                    start: 0,
                                    end: text.len_bytes(),
                                };
                                let before = observe_now(
                                    &authority,
                                    &lifecycle,
                                    &prod_policy,
                                    &session,
                                    &actor_props,
                                );
                                apply_private_event_to_session(
                                    &mut session,
                                    &snap,
                                    &mut actor_props,
                                    &view_ev,
                                );
                                shadow.apply_event(view_ev.clone());
                                actions.push(LifecycleAction::Private(view_ev));
                                let after = observe_now(
                                    &authority,
                                    &lifecycle,
                                    &prod_policy,
                                    &session,
                                    &actor_props,
                                );
                                assert_private_isolation("lc_view", actor, &before, &after);
                                live_history.push(after);

                                let pos = random_boundary(&mut rng, text).as_usize();
                                let bm_ev = ShadowEvent::SetBookmark {
                                    actor,
                                    name: "bm".into(),
                                    buffer: bid.raw(),
                                    offset: pos,
                                    affinity: ShadowAffinity::Right,
                                };
                                let before = observe_now(
                                    &authority,
                                    &lifecycle,
                                    &prod_policy,
                                    &session,
                                    &actor_props,
                                );
                                apply_private_event_to_session(
                                    &mut session,
                                    &snap,
                                    &mut actor_props,
                                    &bm_ev,
                                );
                                shadow.apply_event(bm_ev.clone());
                                actions.push(LifecycleAction::Private(bm_ev));
                                let after = observe_now(
                                    &authority,
                                    &lifecycle,
                                    &prod_policy,
                                    &session,
                                    &actor_props,
                                );
                                assert_private_isolation("lc_bm", actor, &before, &after);
                                coverage.private_updates += 2;
                            }
                        }
                        2 if !live_ids.is_empty() => {
                            let bid = live_ids[0];
                            let snap = authority.snapshot();
                            let text = snap.catalog().get(bid).unwrap();
                            let patch = random_non_noop_patch(&mut rng, text, false);
                            let txn = Transaction::new(
                                EditIntent::for_actor(
                                    ActorId::new(actor),
                                    Nonce::new(nonce),
                                    authority.revision(),
                                ),
                                vec![(bid, patch)],
                            )
                            .unwrap();
                            let op = CheckpointOp::Edit {
                                request: RequestId::new(req_id),
                                txn,
                            };
                            req_id += 1;
                            nonce += 1;
                            let action = LifecycleAction::Authority(op);
                            apply_lifecycle_action(
                                &mut authority,
                                &mut lifecycle,
                                &mut session,
                                &mut prod_policy,
                                &mut actor_props,
                                &action,
                            );
                            let del = authority.delivery().events().last().unwrap();
                            let shadow_patches = del
                                .workspace_event()
                                .patches()
                                .iter()
                                .map(|(b, p)| (b.raw(), shadow_splices_from_patch(p)))
                                .collect();
                            shadow.apply_event(ShadowEvent::Edit {
                                patches: shadow_patches,
                            });
                            actions.push(action);
                            coverage.edits += 1;
                        }
                        3 if !live_ids.is_empty() => {
                            let bid = live_ids[0];
                            let op = CheckpointOp::MarkSaved { buffer: bid };
                            let action = LifecycleAction::Authority(op);
                            apply_lifecycle_action(
                                &mut authority,
                                &mut lifecycle,
                                &mut session,
                                &mut prod_policy,
                                &mut actor_props,
                                &action,
                            );
                            shadow.apply_event(ShadowEvent::Save { buffer: bid.raw() });
                            actions.push(action);
                            coverage.saves += 1;
                        }
                        4 => {
                            if let Some(&clean_bid) =
                                live_ids.iter().find(|&&b| !authority.is_dirty(b).unwrap())
                            {
                                let op = CheckpointOp::Close {
                                    request: RequestId::new(req_id),
                                    intent: EditIntent::for_actor(
                                        ActorId::new(actor),
                                        Nonce::new(nonce),
                                        authority.revision(),
                                    ),
                                    buffer: clean_bid,
                                    policy: ClosePolicy::RequireClean,
                                };
                                req_id += 1;
                                nonce += 1;
                                let action = LifecycleAction::Authority(op);
                                apply_lifecycle_action(
                                    &mut authority,
                                    &mut lifecycle,
                                    &mut session,
                                    &mut prod_policy,
                                    &mut actor_props,
                                    &action,
                                );
                                shadow.apply_event(ShadowEvent::Close {
                                    buffer: clean_bid.raw(),
                                });
                                actions.push(action);
                                coverage.closes_clean += 1;
                            }
                        }
                        5 => {
                            if let Some(&dirty_bid) =
                                live_ids.iter().find(|&&b| authority.is_dirty(b).unwrap())
                            {
                                let op = CheckpointOp::Close {
                                    request: RequestId::new(req_id),
                                    intent: EditIntent::for_actor(
                                        ActorId::new(actor),
                                        Nonce::new(nonce),
                                        authority.revision(),
                                    ),
                                    buffer: dirty_bid,
                                    policy: ClosePolicy::Discard,
                                };
                                req_id += 1;
                                nonce += 1;
                                let action = LifecycleAction::Authority(op);
                                apply_lifecycle_action(
                                    &mut authority,
                                    &mut lifecycle,
                                    &mut session,
                                    &mut prod_policy,
                                    &mut actor_props,
                                    &action,
                                );
                                shadow.apply_event(ShadowEvent::Close {
                                    buffer: dirty_bid.raw(),
                                });
                                actions.push(action);
                                coverage.closes_discard += 1;
                            }
                        }
                        6 => {
                            let before_sess = session.clone();
                            let before_obs = observe_now(
                                &authority,
                                &lifecycle,
                                &prod_policy,
                                &session,
                                &actor_props,
                            );
                            if let Some(&dirty_bid) =
                                live_ids.iter().find(|&&b| authority.is_dirty(b).unwrap())
                            {
                                assert_eq!(
                                    authority.apply(CheckpointOp::Close {
                                        request: RequestId::new(req_id),
                                        intent: EditIntent::for_actor(
                                            ActorId::new(1),
                                            Nonce::new(nonce),
                                            authority.revision(),
                                        ),
                                        buffer: dirty_bid,
                                        policy: ClosePolicy::RequireClean,
                                    }),
                                    Err(CheckpointError::Dirty(dirty_bid))
                                );
                                req_id += 1;
                                nonce += 1;
                                assert_eq!(
                                    lifecycle.close(dirty_bid, ClosePolicy::RequireClean),
                                    Err(LifecycleError::DirtyBuffer(dirty_bid))
                                );
                                coverage.rejected_dirty_close += 1;
                            }
                            if let Some(existing_name) = authority.names().keys().next().cloned() {
                                assert_eq!(
                                    authority.apply(CheckpointOp::Open {
                                        request: RequestId::new(req_id),
                                        intent: EditIntent::for_actor(
                                            ActorId::new(1),
                                            Nonce::new(nonce),
                                            authority.revision(),
                                        ),
                                        name: existing_name,
                                        text: Text::empty(),
                                    }),
                                    Err(CheckpointError::NameInUse)
                                );
                                req_id += 1;
                                nonce += 1;
                                coverage.rejected_duplicate_name += 1;
                            }
                            let after_obs = observe_now(
                                &authority,
                                &lifecycle,
                                &prod_policy,
                                &session,
                                &actor_props,
                            );
                            assert_atomicity_preserved(
                                "lc_rejected_dirty_and_dup",
                                &before_sess,
                                &session,
                                &before_obs,
                                &after_obs,
                            );
                        }
                        7 => {
                            let tombstones = authority.snapshot().catalog().tombstoned_ids();
                            if let Some(&closed) = tombstones.first() {
                                let before_sess = session.clone();
                                let before_obs = observe_now(
                                    &authority,
                                    &lifecycle,
                                    &prod_policy,
                                    &session,
                                    &actor_props,
                                );
                                assert_eq!(
                                    authority.apply(CheckpointOp::MarkSaved { buffer: closed }),
                                    Err(CheckpointError::Gone(closed))
                                );
                                assert_eq!(
                                    lifecycle.open(closed, Text::empty()),
                                    Err(LifecycleError::Workspace(WorkspaceError::Catalog(
                                        CatalogError::TombstonedBufferId(closed)
                                    )))
                                );
                                assert_eq!(
                                    session.read(ActorId::new(1), closed),
                                    Err(SessionError::Access(AccessError::BufferClosed(closed)))
                                );
                                let after_obs = observe_now(
                                    &authority,
                                    &lifecycle,
                                    &prod_policy,
                                    &session,
                                    &actor_props,
                                );
                                assert_atomicity_preserved(
                                    "lc_rejected_closed",
                                    &before_sess,
                                    &session,
                                    &before_obs,
                                    &after_obs,
                                );
                                coverage.rejected_closed_op += 1;
                            }
                        }
                        _ => {
                            let mut next_policy = ShadowPolicy::default();
                            next_policy.grant_actor(1, ShadowCap::Read);
                            for id in authority.snapshot().catalog().live_ids() {
                                if id.raw() & 1 == 0 {
                                    next_policy.grant_buffer(2, id.raw(), ShadowCap::Read);
                                }
                            }
                            let ev = ShadowEvent::SetPolicy {
                                policy: next_policy,
                            };
                            let action = LifecycleAction::Private(ev.clone());
                            apply_lifecycle_action(
                                &mut authority,
                                &mut lifecycle,
                                &mut session,
                                &mut prod_policy,
                                &mut actor_props,
                                &action,
                            );
                            shadow.apply_event(ev);
                            actions.push(action);
                            coverage.policy_updates += 1;
                        }
                    }

                    let now_obs =
                        observe_now(&authority, &lifecycle, &prod_policy, &session, &actor_props);
                    assert_observations_equal(
                        &format!("lifecycle_live_vs_shadow_step_{step_idx}"),
                        &shadow.observe(&probe),
                        &now_obs,
                    );
                    if actions.len() == live_history.len() {
                        live_history.push(now_obs);
                        if actions.len().is_multiple_of(3) {
                            saved_checkpoints.push((
                                actions.len(),
                                authority.checkpoint(),
                                prod_policy.clone(),
                                shadow.clone(),
                                actor_props.clone(),
                            ));
                        }
                    }
                }

                assert_eq!(live_history.len(), actions.len() + 1);

                // Prefix replay equivalence for EVERY prefix 0..=actions.len()!
                for k in 0..=actions.len() {
                    let mut rep_auth = CheckpointState::new(registry.clone());
                    let mut rep_lc = BufferLifecycle::new();
                    let mut rep_pol = init_policy.to_access_policy();
                    let mut rep_sess = WorkspaceSession::new(
                        rep_auth.snapshot(),
                        registry.clone(),
                        rep_pol.clone(),
                    );
                    let mut rep_props: ActorOverlayProps = BTreeMap::new();
                    for action in &actions[..k] {
                        apply_lifecycle_action(
                            &mut rep_auth,
                            &mut rep_lc,
                            &mut rep_sess,
                            &mut rep_pol,
                            &mut rep_props,
                            action,
                        );
                    }
                    let rep_obs = observe_now(&rep_auth, &rep_lc, &rep_pol, &rep_sess, &rep_props);
                    assert_observations_equal(
                        &format!("lifecycle_prefix_replay_{k}"),
                        &live_history[k],
                        &rep_obs,
                    );
                    // Also check CheckpointState::replay matches direct authority ops!
                    let auth_ops: Vec<CheckpointOp> = actions[..k]
                        .iter()
                        .filter_map(|a| match a {
                            LifecycleAction::Authority(op) => Some(op.clone()),
                            LifecycleAction::Private(_) => None,
                        })
                        .collect();
                    let mut bulk = CheckpointState::new(registry.clone());
                    bulk.replay(&auth_ops).unwrap();
                    assert_eq!(bulk.snapshot().catalog(), rep_auth.snapshot().catalog());
                    assert_eq!(bulk.revision(), rep_auth.revision());
                    let shadow_rep =
                        ShadowWorkspace::replay_prefix(&initial_shadow, &shadow.events[..k]);
                    assert_observations_equal(
                        &format!("lifecycle_shadow_prefix_replay_{k}"),
                        &live_history[k],
                        &shadow_rep.observe(&probe),
                    );
                    coverage.prefix_replays_checked += 1;
                }

                // Checkpoint::restore() replay equivalence from every saved checkpoint!
                for (c, cp, cp_pol, cp_shadow, cp_props) in &saved_checkpoints {
                    let mut restored = cp.restore().expect("valid checkpoint restores");
                    let mut rep_pol = cp_pol.clone();
                    let restored_base = restored.snapshot();
                    let mut rep_sess = WorkspaceSession::new(
                        restored_base.clone(),
                        registry.clone(),
                        rep_pol.clone(),
                    );
                    let mut rep_props = cp_props.clone();
                    // Rehydrate actor private state at checkpoint c onto the restored origin snapshot.
                    for (&actor, state) in &cp_shadow.private {
                        for (&buf, &(vs, ve)) in &state.views {
                            apply_private_event_to_session(
                                &mut rep_sess,
                                &restored_base,
                                &mut rep_props,
                                &ShadowEvent::SetView {
                                    actor,
                                    buffer: buf,
                                    start: vs,
                                    end: ve,
                                },
                            );
                        }
                        for (&buf, sel) in &state.selections {
                            apply_private_event_to_session(
                                &mut rep_sess,
                                &restored_base,
                                &mut rep_props,
                                &ShadowEvent::SetSelection {
                                    actor,
                                    buffer: buf,
                                    anchor: sel.anchor,
                                    head: sel.head,
                                },
                            );
                        }
                        for (name, bm) in &state.bookmarks {
                            apply_private_event_to_session(
                                &mut rep_sess,
                                &restored_base,
                                &mut rep_props,
                                &ShadowEvent::SetBookmark {
                                    actor,
                                    name: name.clone(),
                                    buffer: bm.buffer,
                                    offset: bm.offset,
                                    affinity: bm.affinity,
                                },
                            );
                        }
                        for ov in state.overlays.values() {
                            apply_private_event_to_session(
                                &mut rep_sess,
                                &restored_base,
                                &mut rep_props,
                                &ShadowEvent::SetOverlay {
                                    actor,
                                    overlay: ov.clone(),
                                },
                            );
                        }
                        for (name, reg_entry) in &state.registers {
                            apply_private_event_to_session(
                                &mut rep_sess,
                                &restored_base,
                                &mut rep_props,
                                &ShadowEvent::PutRegister {
                                    actor,
                                    name: name.clone(),
                                    entry: reg_entry.clone(),
                                },
                            );
                        }
                        for kill_entry in state.kill_ring.iter().rev() {
                            apply_private_event_to_session(
                                &mut rep_sess,
                                &restored_base,
                                &mut rep_props,
                                &ShadowEvent::PushKill {
                                    actor,
                                    entry: kill_entry.clone(),
                                },
                            );
                        }
                    }
                    for action in &actions[*c..] {
                        match action {
                            LifecycleAction::Authority(op) => {
                                let out = restored.apply(op.clone()).unwrap();
                                match op {
                                    CheckpointOp::Open { .. } => {
                                        rep_sess
                                            .open(restored.snapshot(), out.buffer.unwrap())
                                            .unwrap();
                                    }
                                    CheckpointOp::Close { buffer, .. } => {
                                        rep_sess.close(restored.snapshot(), *buffer).unwrap();
                                    }
                                    CheckpointOp::Edit { .. } => {
                                        let del = restored.delivery().events().last().unwrap();
                                        rep_sess.advance(del).unwrap();
                                    }
                                    CheckpointOp::MarkSaved { .. } => {}
                                }
                            }
                            LifecycleAction::Private(ev) => {
                                if let ShadowEvent::SetPolicy { policy: sp } = ev {
                                    rep_pol = sp.to_access_policy();
                                }
                                let snap = restored.snapshot();
                                apply_private_event_to_session(
                                    &mut rep_sess,
                                    &snap,
                                    &mut rep_props,
                                    ev,
                                );
                            }
                        }
                    }
                    let tip_snap = authority.snapshot();
                    assert_eq!(restored.revision(), authority.revision());
                    assert_eq!(restored.snapshot().catalog(), tip_snap.catalog());
                    assert_eq!(restored.names(), authority.names());
                    let restored_snap = restored.snapshot();
                    let restored_dirty = |b: BufferId| restored.is_dirty(b).unwrap_or(true);
                    let restored_obs = observe_live(&LiveObservationInput {
                        snapshot: &restored_snap,
                        registry: &registry,
                        policy: &rep_pol,
                        session: &rep_sess,
                        dirty: &restored_dirty,
                        actor_props: &rep_props,
                        probe: &probe,
                    });
                    assert_observations_equal(
                        &format!("checkpoint_restore_replay_from_{c}"),
                        live_history.last().unwrap(),
                        &restored_obs,
                    );
                    coverage.checkpoint_restores_checked += 1;
                }
            },
        );

        assert!(coverage.opens > 0, "opens uncovered");
        assert!(coverage.edits > 0, "edits uncovered");
        assert!(coverage.saves > 0, "saves uncovered");
        assert!(coverage.closes_clean > 0, "closes_clean uncovered");
        assert!(coverage.closes_discard > 0, "closes_discard uncovered");
        assert!(coverage.private_updates > 0, "private_updates uncovered");
        assert!(coverage.policy_updates > 0, "policy_updates uncovered");
        assert!(
            coverage.rejected_dirty_close > 0,
            "rejected_dirty_close uncovered"
        );
        assert!(
            coverage.rejected_duplicate_name > 0,
            "rejected_duplicate_name uncovered"
        );
        assert!(
            coverage.rejected_closed_op > 0,
            "rejected_closed_op uncovered"
        );
        assert!(
            coverage.prefix_replays_checked > 0,
            "prefix_replays_checked uncovered"
        );
        assert!(
            coverage.checkpoint_restores_checked > 0,
            "checkpoint_restores_checked uncovered"
        );
    }

    // -------------------------------------------------------------------------
    // Test 4: Immediate close/revocation propagation & strict non-interference
    // -------------------------------------------------------------------------

    #[test]
    fn close_revocation_propagation_and_non_interference() {
        const SENTINEL: &str = "SECRET_TOKEN";
        const HIDDEN_A: &str = "alpha SECRET_TOKEN 界😀 one";
        const HIDDEN_B: &str = "completely different SECRET_TOKEN payload with extra bytes 999";

        assert_law(
            SEEDS,
            |rng: &mut Rng| (rng.next_u64(), rng.next_u64()),
            |&(choice, variant)| {
                let actor_a = ActorId::new(1);
                let actor_b = ActorId::new(2);
                let visible = BufferId::new(choice & 1);
                let hidden = BufferId::new(1 - (choice & 1));
                let unknown = BufferId::new(99);
                let actor_specs = [(1, ShadowRole::Human), (2, ShadowRole::Agent)];
                let registry = build_registry(&actor_specs);

                // Part 1: Policy restriction & buffer close propagate immediately across ALL surfaces.
                let mut lc = BufferLifecycle::new();
                lc.open(visible, Text::from_str("public line\nsecond line"))
                    .unwrap();
                lc.open(hidden, Text::from_str(HIDDEN_A)).unwrap();
                let mut full_policy = ShadowPolicy::default();
                full_policy.grant_default(ShadowCap::Read);
                let mut session = WorkspaceSession::new(
                    lc.snapshot(),
                    registry.clone(),
                    full_policy.to_access_policy(),
                );
                let mut bm_store = BookmarkStore::new();
                let mut ann_store = AnnotationStore::new();

                let before_close_snap = lc.snapshot();
                let hidden_text = before_close_snap.catalog().get(hidden).unwrap();
                let hidden_view = BufferView::narrowed(
                    before_close_snap.catalog(),
                    hidden,
                    ByteSpan::validate_in(hidden_text, 0, hidden_text.len_bytes()).unwrap(),
                )
                .unwrap();
                let hidden_sel = Selection::new(
                    before_close_snap.catalog(),
                    &hidden_view,
                    hidden,
                    hidden_text.validate_offset(0).unwrap(),
                    hidden_text.validate_offset(5).unwrap(),
                )
                .unwrap();
                let hidden_bm = Bookmark {
                    buffer: hidden,
                    anchor: Anchor {
                        offset: hidden_text.validate_offset(0).unwrap(),
                        affinity: Affinity::Right,
                    },
                    revision: before_close_snap.revision(),
                };
                let hidden_ann = |owner: ActorId| Annotation {
                    key: AnnotationKey {
                        buffer: hidden,
                        namespace: NamespaceId::new(1),
                        id: AnnotationId::new(1),
                    },
                    owner,
                    span: ByteSpan::full_of(hidden_text),
                    start_affinity: Affinity::Left,
                    end_affinity: Affinity::Right,
                    priority: 1,
                    collapse: CollapsePolicy::RetainEmpty,
                    label: "secret_mark".into(),
                };
                for actor in [actor_a, actor_b] {
                    session.set_view(actor, hidden_view).unwrap();
                    session.set_selection(actor, hidden_sel).unwrap();
                    session
                        .set_bookmark(actor, BookmarkName::new("h").unwrap(), hidden_bm)
                        .unwrap();
                    session.set_overlay(actor, hidden_ann(actor)).unwrap();
                    let vis_view = BufferView::full(before_close_snap.catalog(), visible).unwrap();
                    session.set_view(actor, vis_view).unwrap();
                }
                bm_store
                    .set(
                        &before_close_snap,
                        BookmarkKey {
                            actor: actor_a,
                            name: BookmarkName::new("h").unwrap(),
                        },
                        hidden_bm,
                    )
                    .unwrap();
                ann_store
                    .set(before_close_snap.catalog(), hidden_ann(actor_a))
                    .unwrap();

                // Before restriction, actor_a finds SENTINEL via search & context.
                let full_prod = full_policy.to_access_policy();
                assert!(!search_workspace(
                    &before_close_snap,
                    &registry,
                    &full_prod,
                    actor_a,
                    SENTINEL
                )
                .unwrap()
                .is_empty());
                let budget = ContextBudget::new(256).unwrap();
                assert!(extract_workspace_context(
                    &before_close_snap,
                    &registry,
                    &full_prod,
                    actor_a,
                    &ContextScope::all_readable(),
                    budget,
                )
                .unwrap()
                .fragments
                .iter()
                .any(|f| f.as_str().contains(SENTINEL)));

                // Restrict policy: actor_a loses Read on hidden; actor_b keeps Read on both.
                // Granting Propose/Review/Commit on hidden must not imply Read access.
                let mut restricted = full_policy.clone();
                restricted.revoke_default(ShadowCap::Read);
                restricted.grant_buffer(actor_a.raw(), visible.raw(), ShadowCap::Read);
                restricted.grant_buffer(actor_a.raw(), hidden.raw(), ShadowCap::Propose);
                restricted.grant_buffer(actor_a.raw(), hidden.raw(), ShadowCap::Review);
                restricted.grant_buffer(actor_a.raw(), hidden.raw(), ShadowCap::Commit);
                restricted.grant_actor(actor_b.raw(), ShadowCap::Read);
                let restricted_prod = restricted.to_access_policy();
                session.set_policy(restricted_prod.clone());

                let expected_denied = AccessError::Denied {
                    actor: actor_a,
                    buffer: hidden,
                    capability: Capability::Read,
                };
                assert_eq!(
                    session.read(actor_a, hidden),
                    Err(SessionError::Access(expected_denied.clone()))
                );
                assert_eq!(
                    session.view(actor_a, hidden),
                    Err(SessionError::Access(expected_denied.clone()))
                );
                assert_eq!(
                    session.selection(actor_a, hidden),
                    Err(SessionError::Access(expected_denied.clone()))
                );
                assert_eq!(
                    session.bookmark(actor_a, hidden, &BookmarkName::new("h").unwrap()),
                    Err(SessionError::Access(expected_denied.clone()))
                );
                assert_eq!(
                    session.overlay(actor_a, hidden_ann(actor_a).key),
                    Err(SessionError::Access(expected_denied.clone()))
                );
                // Actor B still has full access to hidden!
                assert_eq!(session.read(actor_b, hidden).unwrap().as_str(), HIDDEN_A);
                assert_eq!(session.view(actor_b, hidden).unwrap(), Some(hidden_view));

                // Search leaks ZERO matches for actor_a!
                assert!(search_workspace(
                    &before_close_snap,
                    &registry,
                    &restricted_prod,
                    actor_a,
                    SENTINEL
                )
                .unwrap()
                .is_empty());
                assert!(search_buffers(
                    &before_close_snap,
                    &registry,
                    &restricted_prod,
                    actor_a,
                    [hidden, visible, unknown],
                    SENTINEL
                )
                .unwrap()
                .is_empty());

                // Context extraction (single + shared, snapshot + session, Strict + Filter) leaks ZERO!
                let strict_req = ContextRequest::new(vec![ContextTarget::Buffer(hidden)], budget);
                let filter_req = ContextRequest::new(
                    vec![
                        ContextTarget::Buffer(hidden),
                        ContextTarget::Buffer(visible),
                        ContextTarget::View(hidden_view),
                    ],
                    budget,
                )
                .with_mode(ScopeMode::Filter);

                assert_eq!(
                    extract_context(
                        &before_close_snap,
                        &registry,
                        &restricted_prod,
                        actor_a,
                        &ContextScope::all_readable(),
                        &strict_req,
                    ),
                    Err(ContextError::Access(expected_denied.clone()))
                );
                assert_eq!(
                    extract_session_context(
                        &session,
                        actor_a,
                        &ContextScope::all_readable(),
                        &strict_req,
                    ),
                    Err(ContextError::Access(expected_denied.clone()))
                );
                let participants = [
                    ParticipantScope::all_readable(actor_a),
                    ParticipantScope::all_readable(actor_b),
                ];
                assert_eq!(
                    extract_shared_context(
                        &before_close_snap,
                        &registry,
                        &restricted_prod,
                        &participants,
                        &strict_req,
                    ),
                    Err(ContextError::Access(expected_denied.clone()))
                );
                assert_eq!(
                    extract_shared_session_context(&session, &participants, &strict_req),
                    Err(ContextError::Access(expected_denied.clone()))
                );
                for pack in [
                    extract_context(
                        &before_close_snap,
                        &registry,
                        &restricted_prod,
                        actor_a,
                        &ContextScope::all_readable(),
                        &filter_req,
                    )
                    .unwrap(),
                    extract_session_context(
                        &session,
                        actor_a,
                        &ContextScope::all_readable(),
                        &filter_req,
                    )
                    .unwrap(),
                    extract_shared_context(
                        &before_close_snap,
                        &registry,
                        &restricted_prod,
                        &participants,
                        &filter_req,
                    )
                    .unwrap(),
                    extract_shared_session_context(&session, &participants, &filter_req).unwrap(),
                ] {
                    assert!(pack.fragments.iter().all(|f| f.buffer == visible));
                    assert!(pack
                        .fragments
                        .iter()
                        .all(|f| !f.as_str().contains(SENTINEL)));
                }

                // Re-granting policy does NOT resurrect purged positional private state!
                let mut regranted = session.clone();
                regranted.set_policy(full_prod.clone());
                assert_eq!(regranted.view(actor_a, hidden).unwrap(), None);
                assert_eq!(regranted.selection(actor_a, hidden).unwrap(), None);
                assert_eq!(
                    regranted
                        .bookmark(actor_a, hidden, &BookmarkName::new("h").unwrap())
                        .unwrap(),
                    None
                );
                assert_eq!(
                    regranted.overlay(actor_a, hidden_ann(actor_a).key).unwrap(),
                    None
                );

                // Now CLOSE `hidden` and propagate to session, bookmark store, views, search, context!
                let close_policy = if variant & 1 == 0 {
                    ClosePolicy::RequireClean
                } else {
                    lc.update(hidden, Text::from_str(HIDDEN_B)).unwrap();
                    session.update(lc.snapshot(), hidden).unwrap();
                    ClosePolicy::Discard
                };
                let pre_close_snap = lc.snapshot();
                lc.close(hidden, close_policy).unwrap();
                let closed_snap = lc.snapshot();
                let close_event = *lc.events().last().unwrap();
                session.close(closed_snap.clone(), hidden).unwrap();
                bm_store
                    .close(&pre_close_snap, &closed_snap, close_event)
                    .unwrap();
                assert_eq!(
                    bm_store.get(&BookmarkKey {
                        actor: actor_a,
                        name: BookmarkName::new("h").unwrap(),
                    }),
                    None
                );

                // Every surface immediately reports BufferClosed / omits closed buffer for BOTH actors!
                assert!(closed_snap.catalog().is_tombstoned(hidden));
                assert!(!closed_snap.catalog().contains_live(hidden));
                assert_eq!(closed_snap.catalog().get(hidden), None);
                assert_eq!(
                    BufferView::full(closed_snap.catalog(), hidden),
                    Err(ViewError::BufferClosed(hidden))
                );
                assert_eq!(
                    hidden_view.visible_text(closed_snap.catalog()),
                    Err(ViewError::BufferClosed(hidden))
                );
                assert_eq!(
                    hidden_view.widen(closed_snap.catalog()),
                    Err(ViewError::BufferClosed(hidden))
                );
                assert_eq!(
                    hidden_sel.validate(closed_snap.catalog(), &hidden_view),
                    Err(SelectionError::BufferClosed(hidden))
                );
                assert_eq!(
                    ann_store.set(closed_snap.catalog(), hidden_ann(actor_b)),
                    Err(AnnotationError::BufferClosed(hidden))
                );
                for actor in [actor_a, actor_b] {
                    assert_eq!(
                        session.read(actor, hidden),
                        Err(SessionError::Access(AccessError::BufferClosed(hidden)))
                    );
                    assert_eq!(
                        session.view(actor, hidden),
                        Err(SessionError::Access(AccessError::BufferClosed(hidden)))
                    );
                    assert_eq!(
                        session.selection(actor, hidden),
                        Err(SessionError::Access(AccessError::BufferClosed(hidden)))
                    );
                    assert_eq!(
                        session.bookmark(actor, hidden, &BookmarkName::new("h").unwrap()),
                        Err(SessionError::Access(AccessError::BufferClosed(hidden)))
                    );
                    assert_eq!(
                        session.overlay(actor, hidden_ann(actor).key),
                        Err(SessionError::Access(AccessError::BufferClosed(hidden)))
                    );
                    assert!(
                        search_workspace(&closed_snap, &registry, &full_prod, actor, SENTINEL)
                            .unwrap()
                            .is_empty()
                    );
                    assert!(search_buffers(
                        &closed_snap,
                        &registry,
                        &full_prod,
                        actor,
                        [hidden, visible],
                        SENTINEL
                    )
                    .unwrap()
                    .is_empty());
                    assert_eq!(
                        extract_context(
                            &closed_snap,
                            &registry,
                            &full_prod,
                            actor,
                            &ContextScope::all_readable(),
                            &strict_req,
                        ),
                        Err(ContextError::Access(AccessError::BufferClosed(hidden)))
                    );
                    assert_eq!(
                        extract_session_context(
                            &session,
                            actor,
                            &ContextScope::all_readable(),
                            &strict_req,
                        ),
                        Err(ContextError::Access(AccessError::BufferClosed(hidden)))
                    );
                    let filtered = extract_context(
                        &closed_snap,
                        &registry,
                        &full_prod,
                        actor,
                        &ContextScope::all_readable(),
                        &filter_req,
                    )
                    .unwrap();
                    assert!(filtered.fragments.iter().all(|f| f.buffer == visible));
                    assert!(filtered
                        .fragments
                        .iter()
                        .all(|f| !f.as_str().contains(SENTINEL)));
                }

                // Part 2: Permanent actor revocation propagates across ALL surfaces regardless of grants!
                let mut revoked_reg = registry.clone();
                revoked_reg.revoke(actor_b).unwrap();
                assert_eq!(
                    revoked_reg.register(actor_b, ActorRole::Agent),
                    Err(RegistryError::ActorRevoked(actor_b))
                );
                let mut generous_shadow = ShadowPolicy::default();
                generous_shadow.grant_default(ShadowCap::Read);
                generous_shadow.grant_actor(actor_b.raw(), ShadowCap::Read);
                generous_shadow.grant_buffer(actor_b.raw(), visible.raw(), ShadowCap::Read);
                let generous = generous_shadow.to_access_policy();
                let rev_session = WorkspaceSession::new(
                    closed_snap.clone(),
                    revoked_reg.clone(),
                    generous.clone(),
                );
                let mut rev_shadow = ShadowWorkspace::new(
                    &[(visible.raw(), "public line\nsecond line", true)],
                    &actor_specs,
                    generous_shadow,
                );
                rev_shadow.revision = closed_snap.revision().raw();
                rev_shadow.tombstones.insert(hidden.raw());
                rev_shadow.revoke_actor(actor_b.raw());
                let rev_probe = ObservationProbe {
                    actors: vec![actor_a.raw(), actor_b.raw()],
                    buffers: vec![visible.raw(), hidden.raw(), unknown.raw()],
                    bookmark_names: vec!["h".into()],
                    namespaces: vec![1],
                    overlay_ids: vec![1],
                    register_names: vec!["r".into()],
                    search_patterns: vec!["public", SENTINEL],
                    budget,
                };
                let rev_dirty_fn = |b: BufferId| lc.is_dirty(b).unwrap_or(true);
                let rev_live_obs = observe_live(&LiveObservationInput {
                    snapshot: &closed_snap,
                    registry: &revoked_reg,
                    policy: &generous,
                    session: &rev_session,
                    dirty: &rev_dirty_fn,
                    actor_props: &BTreeMap::new(),
                    probe: &rev_probe,
                });
                assert_observations_equal(
                    "revoked_actor_shadow_vs_live",
                    &rev_shadow.observe(&rev_probe),
                    &rev_live_obs,
                );
                assert_eq!(
                    rev_session.read(actor_b, visible),
                    Err(SessionError::Access(AccessError::InactiveActor(actor_b)))
                );
                assert_eq!(
                    search_workspace(&closed_snap, &revoked_reg, &generous, actor_b, "public"),
                    Err(WorkspaceSearchError::Access(AccessError::InactiveActor(
                        actor_b
                    )))
                );
                assert_eq!(
                    search_buffers(
                        &closed_snap,
                        &revoked_reg,
                        &generous,
                        actor_b,
                        [visible],
                        "public"
                    ),
                    Err(WorkspaceSearchError::Access(AccessError::InactiveActor(
                        actor_b
                    )))
                );
                assert_eq!(
                    extract_workspace_context(
                        &closed_snap,
                        &revoked_reg,
                        &generous,
                        actor_b,
                        &ContextScope::all_readable(),
                        budget,
                    ),
                    Err(ContextError::Access(AccessError::InactiveActor(actor_b)))
                );
                assert_eq!(
                    extract_session_context(
                        &rev_session,
                        actor_b,
                        &ContextScope::from_buffers([visible]),
                        &ContextRequest::workspace(budget),
                    ),
                    Err(ContextError::Access(AccessError::InactiveActor(actor_b)))
                );
                assert_eq!(
                    extract_shared_context(
                        &closed_snap,
                        &revoked_reg,
                        &generous,
                        &[
                            ParticipantScope::all_readable(actor_a),
                            ParticipantScope::all_readable(actor_b),
                        ],
                        &ContextRequest::workspace(budget),
                    ),
                    Err(ContextError::Access(AccessError::InactiveActor(actor_b)))
                );

                // Part 3: Strict non-interference when unauthorized buffers AND out-of-scope regions mutate!
                let probe = ObservationProbe {
                    actors: vec![1, 2],
                    buffers: vec![0, 1],
                    bookmark_names: vec!["v".into()],
                    namespaces: vec![1],
                    overlay_ids: vec![1],
                    register_names: vec!["r".into()],
                    search_patterns: PROBE_PATTERNS.to_vec(),
                    budget: ContextBudget::with_limits(20, Some(10), Some(3)).unwrap(),
                };
                let mut world_obs = Vec::new();
                for (hidden_edit, extra_close) in
                    [(HIDDEN_A, false), (HIDDEN_B, false), (HIDDEN_B, true)]
                {
                    let mut cat = BufferCatalog::new();
                    cat.register(visible, Text::from_str("prefix|VISIBLE界😀|suffix"))
                        .unwrap();
                    cat.register(hidden, Text::from_str("initial")).unwrap();
                    let mut auth = CheckpointState::from_catalog(cat, registry.clone());
                    let mut sess = WorkspaceSession::new(
                        auth.snapshot(),
                        registry.clone(),
                        restricted_prod.clone(),
                    );
                    let snap0 = auth.snapshot();
                    let vtxt = snap0.catalog().get(visible).unwrap();
                    let vspan =
                        ByteSpan::validate_in(vtxt, "prefix|".len(), "prefix|VISIBLE界😀".len())
                            .unwrap();
                    let vview = BufferView::narrowed(snap0.catalog(), visible, vspan).unwrap();
                    sess.set_view(actor_a, vview).unwrap();
                    sess.set_selection(
                        actor_a,
                        Selection::new(
                            snap0.catalog(),
                            &vview,
                            visible,
                            vspan.start(),
                            vspan.end(),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                    sess.set_bookmark(
                        actor_a,
                        BookmarkName::new("v").unwrap(),
                        Bookmark {
                            buffer: visible,
                            anchor: Anchor {
                                offset: vspan.start(),
                                affinity: Affinity::Left,
                            },
                            revision: snap0.revision(),
                        },
                    )
                    .unwrap();
                    // Mutate unauthorized buffer with totally different secret text!
                    let htxt = snap0.catalog().get(hidden).unwrap();
                    let hpatch = Patch::new(vec![Splice::new(
                        ByteSpan::full_of(htxt),
                        Text::from_str(hidden_edit),
                    )])
                    .unwrap();
                    let htxn = Transaction::new(
                        EditIntent::for_actor(actor_b, Nonce::new(1), auth.revision()),
                        vec![(hidden, hpatch)],
                    )
                    .unwrap();
                    auth.apply(CheckpointOp::Edit {
                        request: RequestId::new(1),
                        txn: htxn,
                    })
                    .unwrap();
                    sess.advance(auth.delivery().events().last().unwrap())
                        .unwrap();

                    // In world 2, also open & close another unauthorized buffer (different revision count).
                    if extra_close {
                        let extra_id = auth
                            .apply(CheckpointOp::Open {
                                request: RequestId::new(2),
                                intent: EditIntent::for_actor(
                                    actor_b,
                                    Nonce::new(2),
                                    auth.revision(),
                                ),
                                name: "temp_secret".into(),
                                text: Text::from_str(SENTINEL),
                            })
                            .unwrap()
                            .buffer
                            .unwrap();
                        sess.open(auth.snapshot(), extra_id).unwrap();
                        auth.apply(CheckpointOp::Close {
                            request: RequestId::new(3),
                            intent: EditIntent::for_actor(actor_b, Nonce::new(3), auth.revision()),
                            buffer: extra_id,
                            policy: ClosePolicy::Discard,
                        })
                        .unwrap();
                        sess.close(auth.snapshot(), extra_id).unwrap();
                    }

                    let snap = auth.snapshot();
                    let dirty_fn = |b: BufferId| auth.is_dirty(b).unwrap_or(true);
                    let obs = observe_live(&LiveObservationInput {
                        snapshot: &snap,
                        registry: &registry,
                        policy: &restricted_prod,
                        session: &sess,
                        dirty: &dirty_fn,
                        actor_props: &BTreeMap::new(),
                        probe: &probe,
                    });
                    world_obs.push(obs);
                }
                assert_eq!(
                    world_obs[0].revision, world_obs[1].revision,
                    "equal-step hidden-payload worlds must share exact revision"
                );
                assert_ne!(
                    world_obs[0].revision, world_obs[2].revision,
                    "extra hidden lifecycle world must diverge in workspace revision"
                );
                assert_non_interference(
                    "hidden_buffer_mutations_equal_step",
                    actor_a.raw(),
                    &world_obs[0].actors[&actor_a.raw()],
                    &world_obs[1].actors[&actor_a.raw()],
                    true,
                );
                assert_non_interference(
                    "hidden_buffer_mutations_and_lifecycle",
                    actor_a.raw(),
                    &world_obs[0].actors[&actor_a.raw()],
                    &world_obs[2].actors[&actor_a.raw()],
                    false,
                );

                // Out-of-scope region mutation inside the SAME readable buffer produces identical
                // narrowed context extraction!
                let mut scoped_packs = Vec::new();
                for (prefix, suffix) in [("123456|", "|abcdef"), ("界界|", "|😀SECRET")] {
                    let full_str = format!("{prefix}VISIBLE界😀{suffix}");
                    let mut cat = BufferCatalog::new();
                    cat.register(visible, Text::from_str(&full_str)).unwrap();
                    let snap = VersionedWorkspace::new(cat).capture();
                    let txt = snap.catalog().get(visible).unwrap();
                    let span = ByteSpan::validate_in(
                        txt,
                        prefix.len(),
                        prefix.len() + "VISIBLE界😀".len(),
                    )
                    .unwrap();
                    let view = BufferView::narrowed(snap.catalog(), visible, span).unwrap();
                    let scope = ContextScope::from_views([view]);
                    let pack = extract_context(
                        &snap,
                        &registry,
                        &restricted_prod,
                        actor_a,
                        &scope,
                        &ContextRequest::new(
                            vec![ContextTarget::Buffer(visible)],
                            ContextBudget::with_limits(10, Some(10), Some(2)).unwrap(),
                        ),
                    )
                    .unwrap();
                    scoped_packs.push(pack);
                }
                assert_eq!(scoped_packs[0].fragments.len(), 1);
                assert_eq!(scoped_packs[1].fragments.len(), 1);
                assert_eq!(
                    scoped_packs[0].fragments[0].as_str(),
                    scoped_packs[1].fragments[0].as_str()
                );
                assert_eq!(
                    (
                        scoped_packs[0].total_bytes,
                        scoped_packs[0].truncated,
                        scoped_packs[0].omitted_fragments,
                        scoped_packs[0].fragments[0].omitted_bytes,
                    ),
                    (
                        scoped_packs[1].total_bytes,
                        scoped_packs[1].truncated,
                        scoped_packs[1].omitted_fragments,
                        scoped_packs[1].fragments[0].omitted_bytes,
                    )
                );
            },
        );
    }

    // -------------------------------------------------------------------------
    // Test 5: Exhaustive failure-atomicity matrix across all workspace surfaces
    // -------------------------------------------------------------------------

    #[test]
    fn exhaustive_failure_atomicity_matrix() {
        let a = BufferId::new(0);
        let b = BufferId::new(1);
        let closed = BufferId::new(2);
        let unknown = BufferId::new(3);
        let actor1 = ActorId::new(1);
        let actor2 = ActorId::new(2);
        let revoked = ActorId::new(3);
        let stranger = ActorId::new(4);

        let mut registry = ActorRegistry::new();
        registry.register(actor1, ActorRole::Human).unwrap();
        registry.register(actor2, ActorRole::Agent).unwrap();
        registry.register(revoked, ActorRole::Agent).unwrap();
        registry.revoke(revoked).unwrap();

        // 1) ActorRegistry rejections leave registry strictly unchanged.
        let reg_before = registry.clone();
        assert_eq!(
            registry.register(actor1, ActorRole::Agent),
            Err(RegistryError::RoleConflict {
                actor: actor1,
                existing: ActorRole::Human,
                requested: ActorRole::Agent,
            })
        );
        assert_eq!(
            registry.register(revoked, ActorRole::Human),
            Err(RegistryError::ActorRevoked(revoked))
        );
        assert_eq!(
            registry.revoke(stranger),
            Err(RegistryError::UnknownActor(stranger))
        );
        assert_eq!(
            registry.revoke(revoked),
            Err(RegistryError::ActorRevoked(revoked))
        );
        assert_eq!(registry, reg_before);

        // 2) BufferCatalog & VersionedWorkspace atomic rejections (including mixed valid+invalid batches).
        let mut catalog = BufferCatalog::new();
        catalog.register(a, Text::from_str("a界")).unwrap();
        catalog.register(b, Text::from_str("😀z")).unwrap();
        catalog.register(closed, Text::from_str("gone")).unwrap();
        catalog.remove(closed).unwrap();
        let cat_before = catalog.clone();
        assert_eq!(
            catalog.register(a, Text::empty()),
            Err(CatalogError::DuplicateBufferId(a))
        );
        assert_eq!(
            catalog.register(closed, Text::empty()),
            Err(CatalogError::TombstonedBufferId(closed))
        );
        assert_eq!(
            catalog.update(closed, Text::empty()),
            Err(CatalogError::UnknownBuffer(closed))
        );
        assert_eq!(
            catalog.update(unknown, Text::empty()),
            Err(CatalogError::UnknownBuffer(unknown))
        );
        assert_eq!(
            catalog.remove(unknown),
            Err(CatalogError::UnknownBuffer(unknown))
        );
        assert_eq!(catalog, cat_before);

        let mut vw = VersionedWorkspace::new(catalog.clone());
        let vw_before = vw.capture();
        assert_eq!(
            vw.update_batch([(a, Text::from_str("new")), (closed, Text::empty())]),
            Err(WorkspaceError::Catalog(CatalogError::TombstonedBufferId(
                closed
            )))
        );
        assert_eq!(
            vw.update_batch([(a, Text::from_str("new")), (unknown, Text::empty())]),
            Err(WorkspaceError::Catalog(CatalogError::UnknownBuffer(
                unknown
            )))
        );
        assert_eq!(
            vw.update_batch([(a, Text::from_str("1")), (a, Text::from_str("2"))]),
            Err(WorkspaceError::Catalog(CatalogError::DuplicateBufferId(a)))
        );
        assert_eq!(vw.capture(), vw_before);
        assert_eq!(Revision::MAX.next(), Err(RevisionOverflow));

        // 3) SnapshotStore & DirtyTracker rejections leave state unchanged.
        let mut store = SnapshotStore::new();
        store.record(vw_before.clone()).unwrap();
        let store_before = store.clone();
        assert_eq!(
            store.record(vw_before.clone()),
            Err(SnapshotError::NonIncreasingRevision {
                last: Revision::ZERO,
                attempted: Revision::ZERO,
            })
        );
        let foreign_vw = VersionedWorkspace::new(catalog.clone());
        assert_eq!(
            store.record(foreign_vw.capture()),
            Err(SnapshotError::DifferentWorkspace)
        );
        assert_eq!(store, store_before);

        let mut dirty = DirtyTracker::new();
        dirty
            .acknowledge(SaveReceipt {
                buffer: a,
                version: Revision::new(2),
                content: Text::from_str("a界"),
            })
            .unwrap();
        let dirty_before = dirty.clone();
        assert_eq!(
            dirty.acknowledge(SaveReceipt {
                buffer: a,
                version: Revision::new(1),
                content: Text::from_str("a界"),
            }),
            Err(SaveError::Stale {
                acknowledged: Revision::new(2),
                attempted: Revision::new(1),
            })
        );
        assert_eq!(
            dirty.acknowledge(SaveReceipt {
                buffer: a,
                version: Revision::new(2),
                content: Text::from_str("other"),
            }),
            Err(SaveError::ConflictingVersion {
                version: Revision::new(2),
            })
        );
        assert_eq!(dirty, dirty_before);

        // 4) BufferLifecycle rejections leave snapshot, dirty state, and events unchanged.
        let mut lc = BufferLifecycle::new();
        lc.open(a, Text::from_str("a界")).unwrap();
        lc.update(a, Text::from_str("dirty")).unwrap();
        let lc_snap = lc.snapshot();
        let lc_events = lc.events().to_vec();
        assert_eq!(
            lc.close(a, ClosePolicy::RequireClean),
            Err(LifecycleError::DirtyBuffer(a))
        );
        assert_eq!(
            lc.open(a, Text::empty()),
            Err(LifecycleError::Workspace(WorkspaceError::Catalog(
                CatalogError::DuplicateBufferId(a)
            )))
        );
        assert_eq!(
            lc.update(unknown, Text::empty()),
            Err(LifecycleError::Workspace(WorkspaceError::Catalog(
                CatalogError::UnknownBuffer(unknown)
            )))
        );
        assert_eq!(lc.snapshot(), lc_snap);
        assert_eq!(lc.events(), lc_events.as_slice());

        // 5) View, ViewMapping, Selection, BookmarkStore, AnnotationStore, AnnotationMapping rejections.
        let foreign_ascii = Text::from_str("012345");
        let mid_char_span = ByteSpan::validate_in(&foreign_ascii, 0, 2).unwrap(); // offset 2 is interior of '界' in "a界"!
        assert_eq!(
            BufferView::narrowed(&catalog, a, mid_char_span),
            Err(ViewError::InvalidSpan(SpanError::NotCharBoundary {
                offset: 2
            }))
        );
        let full_a = BufferView::full(&catalog, a).unwrap();
        let narrow_a = BufferView::narrowed(
            &catalog,
            a,
            ByteSpan::validate_in(catalog.get(a).unwrap(), 0, 1).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            narrow_a.narrow(&catalog, ByteSpan::full_of(catalog.get(a).unwrap())),
            Err(ViewError::SpanOutsideVisible { .. })
        ));
        let pmap_bad = PositionMap::identity(99);
        assert!(matches!(
            map_view(
                &catalog,
                &full_a,
                &pmap_bad,
                4,
                Affinity::Left,
                Affinity::Right
            ),
            Err(ViewMappingError::SourceLengthMismatch { .. })
        ));
        let full_b = BufferView::full(&catalog, b).unwrap();
        let sel_a = Selection::new(
            &catalog,
            &full_a,
            a,
            catalog.get(a).unwrap().validate_offset(0).unwrap(),
            catalog.get(a).unwrap().validate_offset(1).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            clip_selection(&catalog, sel_a, &full_b),
            Err(ViewMappingError::BufferMismatch { .. })
        ));

        let mut bms = BookmarkStore::new();
        assert_eq!(BookmarkName::new(""), Err(BookmarkError::InvalidName));
        let bms_before = bms.clone();
        assert_eq!(
            bms.set(
                &vw_before,
                BookmarkKey {
                    actor: actor1,
                    name: BookmarkName::new("k").unwrap(),
                },
                Bookmark {
                    buffer: a,
                    anchor: Anchor {
                        offset: foreign_ascii.validate_offset(2).unwrap(),
                        affinity: Affinity::Left,
                    },
                    revision: vw_before.revision(),
                },
            ),
            Err(BookmarkError::InvalidOffset(Utf8Error::NotCharBoundary {
                offset: 2
            }))
        );
        assert_eq!(
            bms.close(
                &vw_before,
                &vw_before,
                LifecycleEvent {
                    buffer: a,
                    kind: LifecycleKind::Opened,
                    previous: Revision::ZERO,
                    revision: Revision::ZERO,
                },
            ),
            Err(BookmarkError::InvalidLifecycleEvent)
        );
        let empty_steps: [BookmarkStep<'_>; 0] = [];
        assert_eq!(bms.advance_sequence(&empty_steps), Ok(()));
        assert_eq!(bms, bms_before);

        let mut anns = AnnotationStore::new();
        let valid_ann = Annotation {
            key: AnnotationKey {
                buffer: a,
                namespace: NamespaceId::new(1),
                id: AnnotationId::new(1),
            },
            owner: actor1,
            span: ByteSpan::full_of(catalog.get(a).unwrap()),
            start_affinity: Affinity::Left,
            end_affinity: Affinity::Right,
            priority: 1,
            collapse: CollapsePolicy::Evict,
            label: "ok".into(),
        };
        anns.set(&catalog, valid_ann.clone()).unwrap();
        let anns_before = anns.clone();
        let mut bad_ann = valid_ann.clone();
        bad_ann.span = mid_char_span;
        assert_eq!(
            anns.set(&catalog, bad_ann),
            Err(AnnotationError::InvalidSpan(SpanError::NotCharBoundary {
                offset: 2
            }))
        );
        assert!(anns
            .transport(
                a,
                ReplacementEffect {
                    span: ByteSpan::full_of(catalog.get(a).unwrap()),
                    inserted_len: 1,
                },
                catalog.get(a).unwrap(),
                &Text::from_str("wrong_len"),
            )
            .is_err());
        assert_eq!(anns, anns_before);
        assert!(matches!(
            map_store(
                &anns,
                a,
                catalog.get(a).unwrap(),
                &catalog,
                &RevisionedMap {
                    map: &PositionMap::identity(4),
                    source: Revision::new(1),
                    target: Revision::new(2),
                },
                MappingRevisions {
                    base: Revision::ZERO,
                    target: Revision::new(2),
                },
            ),
            Err(AnnotationMappingError::RevisionMismatch { .. })
        ));

        // 6) WorkspaceSession comprehensive rejection atomicity (including full observation check).
        let mut policy = AccessPolicy::new();
        policy.grant_buffer(actor1, a, Capability::Read);
        policy.grant_actor(actor2, Capability::Read);
        let mut session =
            WorkspaceSession::new(vw_before.clone(), registry.clone(), policy.clone());
        session.set_view(actor1, full_a).unwrap();
        session.set_selection(actor1, sel_a).unwrap();
        let sess_before = session.clone();

        let probe = ObservationProbe {
            actors: vec![actor1.raw(), actor2.raw(), revoked.raw()],
            buffers: vec![a.raw(), b.raw(), closed.raw(), unknown.raw()],
            bookmark_names: vec!["k".into()],
            namespaces: vec![1],
            overlay_ids: vec![1],
            register_names: vec!["r".into()],
            search_patterns: PROBE_PATTERNS.to_vec(),
            budget: ContextBudget::new(32).unwrap(),
        };
        let dirty_fn = |id: BufferId| {
            vw_before
                .catalog()
                .get(id)
                .is_none_or(|t| dirty.is_dirty(id, t))
        };
        let obs_before = observe_live(&LiveObservationInput {
            snapshot: &vw_before,
            registry: &registry,
            policy: &policy,
            session: &session,
            dirty: &dirty_fn,
            actor_props: &BTreeMap::new(),
            probe: &probe,
        });

        // Narrowing view past existing selection fails atomically.
        let tail_view = BufferView::narrowed(
            &catalog,
            a,
            ByteSpan::validate_in(catalog.get(a).unwrap(), 1, 4).unwrap(),
        )
        .unwrap();
        assert_eq!(
            session.set_view(actor1, tail_view),
            Err(SessionError::InvalidState)
        );
        // Setting selection without view fails atomically.
        let sel_b = Selection::new(
            &catalog,
            &full_b,
            b,
            catalog.get(b).unwrap().validate_offset(0).unwrap(),
            catalog.get(b).unwrap().validate_offset(4).unwrap(),
        )
        .unwrap();
        assert_eq!(
            session.set_selection(actor2, sel_b),
            Err(SessionError::InvalidState)
        );
        // Wrong overlay owner fails atomically.
        let mut wrong_owner = valid_ann;
        wrong_owner.owner = actor2;
        assert_eq!(
            session.set_overlay(actor1, wrong_owner),
            Err(SessionError::InvalidState)
        );
        // Unauthorized buffer, closed buffer, unknown buffer, revoked/stranger actor fail atomically.
        assert_eq!(
            session.set_view(actor1, full_b),
            Err(SessionError::Access(AccessError::Denied {
                actor: actor1,
                buffer: b,
                capability: Capability::Read,
            }))
        );
        assert_eq!(
            session.put_register(
                stranger,
                RegisterName::new("r").unwrap(),
                RegisterEntry {
                    text: Text::from_str("x"),
                    kind: RegisterKind::Charwise,
                },
            ),
            Err(SessionError::InvalidState)
        );
        assert_eq!(
            session.push_kill(
                revoked,
                RegisterEntry {
                    text: Text::from_str("x"),
                    kind: RegisterKind::Charwise,
                },
            ),
            Err(SessionError::InvalidState)
        );
        // Invalid transitions (same snapshot, foreign snapshot, wrong buffer id) fail atomically.
        assert_eq!(
            session.open(vw_before.clone(), unknown),
            Err(SessionError::InvalidTransition)
        );
        assert_eq!(
            session.close(vw_before.clone(), a),
            Err(SessionError::InvalidTransition)
        );
        assert_eq!(
            session.update(foreign_vw.capture(), a),
            Err(SessionError::InvalidTransition)
        );

        let obs_after = observe_live(&LiveObservationInput {
            snapshot: &vw_before,
            registry: &registry,
            policy: &policy,
            session: &session,
            dirty: &dirty_fn,
            actor_props: &BTreeMap::new(),
            probe: &probe,
        });
        assert_atomicity_preserved(
            "exhaustive_session_matrix",
            &sess_before,
            &session,
            &obs_before,
            &obs_after,
        );

        // 7) Patch/Transaction/Search/Context input-validation rejections.
        let touching = Patch::new(vec![
            Splice::insert(
                catalog.get(a).unwrap().validate_offset(0).unwrap(),
                Text::from_str("x"),
            ),
            Splice::insert(
                catalog.get(a).unwrap().validate_offset(0).unwrap(),
                Text::from_str("y"),
            ),
        ]);
        assert!(matches!(touching, Err(PatchError::Contact { .. })));
        let dup_txn = Transaction::new(
            EditIntent::for_actor(actor1, Nonce::new(1), Revision::ZERO),
            vec![(a, Patch::default()), (a, Patch::default())],
        );
        assert_eq!(dup_txn, Err(TransactionError::DuplicateBufferId(a)));
        assert_eq!(
            search_workspace(&vw_before, &registry, &policy, actor1, ""),
            Err(WorkspaceSearchError::Search(SearchError::EmptyPattern))
        );
        assert_eq!(ContextBudget::new(0), Err(BudgetError::ZeroTotalBytes));
        assert_eq!(
            ContextBudget::with_limits(4, Some(0), None),
            Err(BudgetError::ZeroFragmentBytes)
        );
        assert_eq!(
            ContextBudget::with_limits(4, Some(5), None),
            Err(BudgetError::FragmentExceedsTotal {
                max_fragment_bytes: 5,
                max_total_bytes: 4,
            })
        );
        assert_eq!(
            ContextBudget::with_limits(4, None, Some(0)),
            Err(BudgetError::ZeroFragmentLimit)
        );
        assert_eq!(
            extract_context(
                &vw_before,
                &registry,
                &policy,
                actor1,
                &ContextScope::all_readable(),
                &ContextRequest::new(
                    vec![ContextTarget::ByteRange {
                        buffer: a,
                        start: 4,
                        end: 1,
                    }],
                    ContextBudget::new(8).unwrap(),
                ),
            ),
            Err(ContextError::InvalidSpan {
                buffer: a,
                error: SpanError::InvertedRange { start: 4, end: 1 },
            })
        );
        assert_eq!(
            extract_context(
                &vw_before,
                &registry,
                &policy,
                actor1,
                &ContextScope::all_readable(),
                &ContextRequest::new(
                    vec![ContextTarget::LineRange {
                        buffer: a,
                        start_line: 3,
                        end_line: 1,
                    }],
                    ContextBudget::new(8).unwrap(),
                ),
            ),
            Err(ContextError::InvalidLineRange {
                buffer: a,
                error: ClipError::InvertedLines { start: 3, end: 1 },
            })
        );
        assert_eq!(
            extract_context(
                &vw_before,
                &registry,
                &policy,
                actor1,
                &ContextScope::all_readable(),
                &ContextRequest::new(
                    vec![ContextTarget::Buffer(a)],
                    ContextBudget::new(8).unwrap()
                )
                .at_revision(Revision::new(7)),
            ),
            Err(ContextError::RevisionMismatch {
                expected: Revision::new(7),
                actual: Revision::ZERO,
            })
        );
        assert_eq!(
            extract_shared_context(
                &vw_before,
                &registry,
                &policy,
                &[],
                &ContextRequest::workspace(ContextBudget::new(8).unwrap()),
            ),
            Err(ContextError::EmptyParticipants)
        );
        assert_eq!(
            extract_session_context(
                &session,
                actor1,
                &ContextScope::all_readable(),
                &ContextRequest::workspace(ContextBudget::new(8).unwrap()),
            ),
            Err(ContextError::WorkspaceTargetUnsupportedForSession)
        );
        // Also verify BufferScope & ContextScope lattice sanity for Coverage.
        let s_empty = ContextScope::empty();
        assert!(s_empty.is_empty());
        assert!(BufferScope::empty().is_empty());
    }

    // -------------------------------------------------------------------------
    // Test 6: Negative self-checks proving comparators & drift detectors fail
    // -------------------------------------------------------------------------

    fn assert_panics_with(label: &str, needle: &str, f: impl FnOnce()) {
        let err = catch_unwind(AssertUnwindSafe(f)).expect_err(&format!(
            "negative self-check '{label}' did NOT fail on injected divergence!"
        ));
        let msg = err
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| err.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            msg.contains(needle),
            "negative self-check '{label}' panicked with unexpected message {msg:?}, expected substring {needle:?}"
        );
    }

    #[test]
    fn negative_self_checks_catch_injected_divergence_and_drift() {
        let actor_specs = [(1, ShadowRole::Human), (2, ShadowRole::Agent)];
        let initial_buffers = [(0, "a界b", true)];
        let mut policy = ShadowPolicy::default();
        policy.grant_default(ShadowCap::Read);
        let mut shadow = ShadowWorkspace::new(&initial_buffers, &actor_specs, policy.clone());
        shadow.apply_event(ShadowEvent::SetView {
            actor: 1,
            buffer: 0,
            start: 0,
            end: 5,
        });
        shadow.apply_event(ShadowEvent::SetSelection {
            actor: 1,
            buffer: 0,
            anchor: 0,
            head: 1,
        });
        shadow.apply_event(ShadowEvent::SetBookmark {
            actor: 1,
            name: "bm".into(),
            buffer: 0,
            offset: 1,
            affinity: ShadowAffinity::Right,
        });
        shadow.apply_event(ShadowEvent::SetOverlay {
            actor: 1,
            overlay: ShadowOverlay {
                buffer: 0,
                namespace: 1,
                id: 1,
                owner: 1,
                start: 0,
                end: 1,
                start_affinity: ShadowAffinity::Left,
                end_affinity: ShadowAffinity::Right,
                priority: 1,
                collapse: ShadowCollapse::RetainEmpty,
                label: "ov".into(),
            },
        });
        shadow.apply_event(ShadowEvent::SetOverlayProps {
            actor: 1,
            buffer: 0,
            namespace: 1,
            id: 1,
            props: [("k".into(), "v".into())].into_iter().collect(),
        });
        shadow.apply_event(ShadowEvent::PutRegister {
            actor: 1,
            name: "r".into(),
            entry: ShadowRegister {
                text: "reg".into(),
                kind: ShadowRegKind::Charwise,
            },
        });
        shadow.apply_event(ShadowEvent::PushKill {
            actor: 1,
            entry: ShadowRegister {
                text: "kill".into(),
                kind: ShadowRegKind::Linewise,
            },
        });

        let probe = ObservationProbe {
            actors: vec![1, 2],
            buffers: vec![0, 5],
            bookmark_names: vec!["bm".into()],
            namespaces: vec![1],
            overlay_ids: vec![1],
            register_names: vec!["r".into()],
            search_patterns: vec!["a", "界"],
            budget: ContextBudget::new(16).unwrap(),
        };
        let baseline = shadow.observe(&probe);

        // 1) Prefix-replay comparator catches EVERY field of observation divergence!
        let mut div_rev = baseline.clone();
        div_rev.revision += 1;
        assert_panics_with("revision_drift", "revision mismatch", || {
            assert_observations_equal("probe", &baseline, &div_rev);
        });

        let mut div_live = baseline.clone();
        div_live.live_ids.push(99);
        assert_panics_with("live_ids_drift", "live buffer ids mismatch", || {
            assert_observations_equal("probe", &baseline, &div_live);
        });

        let mut div_tomb = baseline.clone();
        div_tomb.tombstoned_ids.push(0);
        assert_panics_with("tombstone_drift", "tombstoned buffer ids mismatch", || {
            assert_observations_equal("probe", &baseline, &div_tomb);
        });

        let mut div_text = baseline.clone();
        div_text.buffers.get_mut(&0).unwrap().0.push('!');
        assert_panics_with(
            "buffer_text_drift",
            "shared buffer text/dirty mismatch",
            || {
                assert_observations_equal("probe", &baseline, &div_text);
            },
        );

        let mut div_dirty = baseline.clone();
        div_dirty.buffers.get_mut(&0).unwrap().1 = true;
        assert_panics_with(
            "dirty_flag_drift",
            "shared buffer text/dirty mismatch",
            || {
                assert_observations_equal("probe", &baseline, &div_dirty);
            },
        );

        let mut div_read = baseline.clone();
        div_read.actors.get_mut(&1).unwrap().reads.insert(
            0,
            Err(ObsAccessErr::Denied {
                actor: 1,
                buffer: 0,
            }),
        );
        assert_panics_with("read_access_drift", "reads mismatch", || {
            assert_observations_equal("probe", &baseline, &div_read);
        });

        let mut div_view = baseline.clone();
        div_view
            .actors
            .get_mut(&1)
            .unwrap()
            .views
            .insert(0, Ok(Some((0, 1, "a".into()))));
        assert_panics_with("view_drift", "views mismatch", || {
            assert_observations_equal("probe", &baseline, &div_view);
        });

        let mut div_sel = baseline.clone();
        div_sel
            .actors
            .get_mut(&1)
            .unwrap()
            .selections
            .insert(0, Ok(Some((1, 0, 2))));
        assert_panics_with("selection_drift", "selections mismatch", || {
            assert_observations_equal("probe", &baseline, &div_sel);
        });

        let mut div_bm = baseline.clone();
        div_bm
            .actors
            .get_mut(&1)
            .unwrap()
            .bookmarks
            .insert((0, "bm".into()), Ok(Some((4, ShadowAffinity::Left, 0))));
        assert_panics_with("bookmark_drift", "bookmarks mismatch", || {
            assert_observations_equal("probe", &baseline, &div_bm);
        });

        let mut div_ov = baseline.clone();
        div_ov
            .actors
            .get_mut(&1)
            .unwrap()
            .overlays
            .insert((0, 1, 1), Ok(None));
        assert_panics_with("overlay_drift", "overlays mismatch", || {
            assert_observations_equal("probe", &baseline, &div_ov);
        });

        let mut div_props = baseline.clone();
        div_props
            .actors
            .get_mut(&1)
            .unwrap()
            .property_projections
            .get_mut(&(0, 1))
            .unwrap()
            .clear();
        assert_panics_with("property_drift", "property projections mismatch", || {
            assert_observations_equal("probe", &baseline, &div_props);
        });

        let mut div_reg = baseline.clone();
        div_reg
            .actors
            .get_mut(&1)
            .unwrap()
            .registers
            .insert("r".into(), None);
        assert_panics_with("register_drift", "registers mismatch", || {
            assert_observations_equal("probe", &baseline, &div_reg);
        });

        let mut div_kill = baseline.clone();
        div_kill.actors.get_mut(&1).unwrap().current_kill = None;
        assert_panics_with("kill_drift", "current kill mismatch", || {
            assert_observations_equal("probe", &baseline, &div_kill);
        });

        let mut div_search = baseline.clone();
        div_search
            .actors
            .get_mut(&1)
            .unwrap()
            .search_workspace
            .insert("a".into(), Ok(Vec::new()));
        assert_panics_with("search_drift", "workspace search mismatch", || {
            assert_observations_equal("probe", &baseline, &div_search);
        });

        let mut div_ctx = baseline.clone();
        div_ctx
            .actors
            .get_mut(&1)
            .unwrap()
            .snapshot_context_filter
            .as_mut()
            .unwrap()
            .truncated = true;
        assert_panics_with("snapshot_ctx_drift", "snapshot context mismatch", || {
            assert_observations_equal("probe", &baseline, &div_ctx);
        });

        let mut div_sctx = baseline.clone();
        div_sctx
            .actors
            .get_mut(&1)
            .unwrap()
            .session_context_filter
            .as_mut()
            .unwrap()
            .omitted_fragments = 1;
        assert_panics_with("session_ctx_drift", "session context mismatch", || {
            assert_observations_equal("probe", &baseline, &div_sctx);
        });

        // 2) Failure-atomicity drift detector catches mutated WorkspaceSession even if obs were copied.
        let mut cat = BufferCatalog::new();
        cat.register(BufferId::new(0), Text::from_str("a界b"))
            .unwrap();
        let snap = VersionedWorkspace::new(cat).capture();
        let reg = build_registry(&actor_specs);
        let sess_before = WorkspaceSession::new(snap, reg, policy.to_access_policy());
        let mut sess_mutated = sess_before.clone();
        sess_mutated
            .put_register(
                ActorId::new(2),
                RegisterName::new("leak").unwrap(),
                RegisterEntry {
                    text: Text::from_str("mutated"),
                    kind: RegisterKind::Charwise,
                },
            )
            .unwrap();
        assert_panics_with(
            "atomicity_session_drift",
            "rejected operation mutated WorkspaceSession state",
            || {
                assert_atomicity_preserved(
                    "probe",
                    &sess_before,
                    &sess_mutated,
                    &baseline,
                    &baseline,
                );
            },
        );

        // 3) Private-isolation detector catches shared text mutation AND cross-actor leakage!
        assert_panics_with(
            "isolation_shared_drift",
            "private update changed shared text or dirty flags",
            || {
                assert_private_isolation("probe", 1, &baseline, &div_text);
            },
        );
        let mut cross_actor = baseline.clone();
        cross_actor
            .actors
            .get_mut(&2)
            .unwrap()
            .registers
            .insert("r".into(), Some(("leaked".into(), ShadowRegKind::Charwise)));
        assert_panics_with(
            "isolation_cross_actor",
            "actor 1 private update leaked into actor 2",
            || {
                assert_private_isolation("probe", 1, &baseline, &cross_actor);
            },
        );

        // 4) Non-interference detector catches leaked bookmarks, search matches, context content,
        //    and exact-revision divergence!
        assert_panics_with(
            "non_interference_bm_content",
            "bookmark content drifted",
            || {
                assert_non_interference(
                    "probe",
                    1,
                    &baseline.actors[&1],
                    &div_bm.actors[&1],
                    false,
                );
            },
        );
        assert_panics_with(
            "non_interference_bm_exact",
            "bookmark non-interference violated",
            || {
                assert_non_interference("probe", 1, &baseline.actors[&1], &div_bm.actors[&1], true);
            },
        );
        let mut div_bm_err = baseline.clone();
        div_bm_err
            .actors
            .get_mut(&1)
            .unwrap()
            .bookmarks
            .insert((5, "bm".into()), Err(ObsAccessErr::BufferClosed(5)));
        assert_panics_with(
            "non_interference_bm_access_err",
            "bookmark content drifted",
            || {
                assert_non_interference(
                    "probe",
                    1,
                    &baseline.actors[&1],
                    &div_bm_err.actors[&1],
                    false,
                );
            },
        );
        let mut div_bm_buf = baseline.clone();
        div_bm_buf
            .actors
            .get_mut(&1)
            .unwrap()
            .bookmarks
            .insert((99, "bm".into()), Ok(Some((1, ShadowAffinity::Right, 0))));
        assert_panics_with(
            "non_interference_bm_buffer_id",
            "bookmark content drifted",
            || {
                assert_non_interference(
                    "probe",
                    1,
                    &baseline.actors[&1],
                    &div_bm_buf.actors[&1],
                    false,
                );
            },
        );
        let mut div_bm_rev = baseline.clone();
        match div_bm_rev
            .actors
            .get_mut(&1)
            .unwrap()
            .bookmarks
            .get_mut(&(0, "bm".into()))
            .unwrap()
        {
            Ok(Some((_, _, rev))) => *rev += 1,
            other => panic!("expected baseline bookmark on buffer 0, got {other:?}"),
        }
        assert_non_interference(
            "probe",
            1,
            &baseline.actors[&1],
            &div_bm_rev.actors[&1],
            false,
        );
        assert_panics_with(
            "non_interference_bm_rev_exact",
            "bookmark non-interference violated",
            || {
                assert_non_interference(
                    "probe",
                    1,
                    &baseline.actors[&1],
                    &div_bm_rev.actors[&1],
                    true,
                );
            },
        );
        let mut div_search_rev = baseline.clone();
        div_search_rev
            .actors
            .get_mut(&1)
            .unwrap()
            .search_workspace
            .get_mut("a")
            .unwrap()
            .as_mut()
            .unwrap()[0]
            .0 += 1;
        assert_non_interference(
            "probe",
            1,
            &baseline.actors[&1],
            &div_search_rev.actors[&1],
            false,
        );
        assert_panics_with(
            "non_interference_search_rev_exact",
            "workspace search non-interference violated",
            || {
                assert_non_interference(
                    "probe",
                    1,
                    &baseline.actors[&1],
                    &div_search_rev.actors[&1],
                    true,
                );
            },
        );
        let mut div_ctx_rev = baseline.clone();
        div_ctx_rev
            .actors
            .get_mut(&1)
            .unwrap()
            .snapshot_context_filter
            .as_mut()
            .unwrap()
            .revision += 1;
        assert_non_interference(
            "probe",
            1,
            &baseline.actors[&1],
            &div_ctx_rev.actors[&1],
            false,
        );
        assert_panics_with(
            "non_interference_ctx_rev_exact",
            "snapshot context non-interference violated",
            || {
                assert_non_interference(
                    "probe",
                    1,
                    &baseline.actors[&1],
                    &div_ctx_rev.actors[&1],
                    true,
                );
            },
        );
        assert_panics_with(
            "non_interference_search",
            "workspace search matches drifted",
            || {
                assert_non_interference(
                    "probe",
                    1,
                    &baseline.actors[&1],
                    &div_search.actors[&1],
                    false,
                );
            },
        );
        assert_panics_with(
            "non_interference_ctx",
            "snapshot context content drifted",
            || {
                assert_non_interference(
                    "probe",
                    1,
                    &baseline.actors[&1],
                    &div_ctx.actors[&1],
                    false,
                );
            },
        );
    }
}
