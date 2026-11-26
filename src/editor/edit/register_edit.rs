//! Register operations tied to snapshot-validated edits.
//!
//! Multiple selections are ordered by position and duplicate ranges collapse.
//! Their exact slices are concatenated without separators. Paste inserts that
//! same payload at every selection. Adjacent replacements are combined into
//! one splice; overlapping selections are rejected.

use std::collections::BTreeMap;

use crate::editor::collab::receipt::{CommitReceipt, SubmitOutcome};
use crate::editor::collab::sequencer::Sequencer;
use crate::editor::edit::{
    command::EditCommand,
    ids::{ActorId, TxnId},
    intent::EditIntent,
    multi_cursor::{self, MultiCursorError},
    patch::PatchError,
    transaction::Transaction,
};
use crate::editor::text::{span::ByteSpan, utf8::Text};
use crate::editor::workspace::{
    events::WorkspaceEvent,
    registers::{KillRings, RegisterEntry, RegisterKind, RegisterName, RegisterStore},
    selections::Selection,
    snapshots::{SnapshotStore, WorkspaceSnapshot},
    view::BufferView,
};

/// A register operation could not be validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegisterEditError {
    /// The multi-cursor edit is invalid.
    Plan(MultiCursorError),
    /// No payload exists at this actor and name.
    MissingRegister,
    /// The outcome or committed snapshot does not match the prepared cut.
    MismatchedCommit,
    /// The transaction identifier was already used with different content.
    ConflictingReplay,
}

/// A prepared cut. Its fragments cannot be changed independently of its edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CutPlan {
    base: WorkspaceSnapshot,
    transaction: Transaction,
    name: RegisterName,
    entry: RegisterEntry,
}

/// The name and interpretation to assign to copied text.
pub struct CopyTarget {
    /// Actor-local register name.
    pub name: RegisterName,
    /// Interpretation of the copied text.
    pub kind: RegisterKind,
}

impl CutPlan {
    /// Returns the transaction for inspection. Submit via [`submit_cut`].
    pub fn transaction(&self) -> &Transaction {
        &self.transaction
    }

    /// Returns the exact concatenated fragments in source order.
    pub fn entry(&self) -> &RegisterEntry {
        &self.entry
    }
}

/// Submits a cut at its base, or retries an already committed identical cut.
pub fn submit_cut(
    receipts: &mut CutReceipts,
    sequencer: &mut Sequencer,
    plan: &CutPlan,
) -> Result<SubmitOutcome, RegisterEditError> {
    let id = plan.transaction.intent().txn();
    if let Some((recorded, _)) = receipts.submitted.get(&id) {
        if recorded != plan {
            return Err(RegisterEditError::ConflictingReplay);
        }
    } else if sequencer.journal().lookup_txn(id).is_some() {
        return Err(RegisterEditError::MismatchedCommit);
    }
    let current = sequencer.snapshot();
    if current != plan.base {
        let receipt = sequencer
            .journal()
            .lookup_txn(plan.transaction.intent().txn())
            .copied()
            .ok_or(RegisterEditError::MismatchedCommit)?;
        let event = WorkspaceEvent::from_receipt(sequencer, receipt)
            .ok_or(RegisterEditError::MismatchedCommit)?;
        if receipt.intent() != plan.transaction.intent()
            || receipt.previous() != plan.base.revision()
            || event.patches() != plan.transaction.patches()
            || !same_workspace(&plan.base, &current)
        {
            return Err(RegisterEditError::MismatchedCommit);
        }
    }
    let outcome = sequencer.submit(plan.transaction.clone());
    if let Some(receipt) = outcome.committed_receipt() {
        receipts.submitted.insert(id, (plan.clone(), receipt));
    }
    Ok(outcome)
}

fn same_workspace(older: &WorkspaceSnapshot, later: &WorkspaceSnapshot) -> bool {
    if older.revision() == later.revision() {
        return older == later;
    }
    let mut history = SnapshotStore::new();
    history.record(older.clone()).is_ok() && history.record(later.clone()).is_ok()
}

fn extract(
    snapshot: &WorkspaceSnapshot,
    transaction: &Transaction,
    kind: RegisterKind,
) -> RegisterEntry {
    let (buffer, patch) = &transaction.patches()[0];
    let text = snapshot.catalog().get(*buffer).expect("validated buffer");
    let mut fragments = String::new();
    for splice in patch.splices() {
        fragments.push_str(splice.span().slice_in(text).expect("validated span"));
    }
    RegisterEntry {
        text: Text::from_str(&fragments),
        kind,
    }
}

fn deletions(
    intent: EditIntent,
    view: &BufferView,
    snapshot: &WorkspaceSnapshot,
    selections: &[Selection],
) -> Result<Transaction, RegisterEditError> {
    let mut spans = Vec::with_capacity(selections.len());
    for &selection in selections {
        multi_cursor::plan(
            intent,
            view,
            snapshot,
            vec![EditCommand::DeleteSelection { selection }],
        )
        .map_err(RegisterEditError::Plan)?;
        if !selection.range().is_empty() {
            spans.push(selection.range());
        }
    }
    spans.sort_by_key(|span| (span.start(), span.end()));
    spans.dedup();
    let mut merged: Vec<ByteSpan> = Vec::new();
    for span in spans {
        if let Some(previous) = merged.last_mut() {
            if span.start() < previous.end() {
                return Err(RegisterEditError::Plan(MultiCursorError::Patch(
                    PatchError::Contact {
                        previous: *previous,
                        next: span,
                    },
                )));
            }
            if span.start() == previous.end() {
                *previous = ByteSpan::new(previous.start(), span.end()).expect("ordered spans");
                continue;
            }
        }
        merged.push(span);
    }
    let commands = merged
        .into_iter()
        .map(|span| EditCommand::Delete { span })
        .collect();
    multi_cursor::plan(intent, view, snapshot, commands).map_err(RegisterEditError::Plan)
}

/// Copies selections into one actor's named register and kill history.
fn copy(
    registers: &mut RegisterStore,
    rings: &mut KillRings,
    intent: EditIntent,
    view: &BufferView,
    snapshot: &WorkspaceSnapshot,
    selections: &[Selection],
    target: CopyTarget,
) -> Result<RegisterEntry, RegisterEditError> {
    view.visible_text(snapshot.catalog())
        .map_err(|error| RegisterEditError::Plan(MultiCursorError::View(error)))?;
    if intent.base() != snapshot.revision() {
        return Err(RegisterEditError::Plan(MultiCursorError::Transaction(
            crate::editor::edit::transaction::TransactionError::RevisionMismatch {
                expected: intent.base(),
                actual: snapshot.revision(),
            },
        )));
    }
    let text = snapshot
        .catalog()
        .get(view.buffer())
        .expect("validated view");
    let mut ranges = Vec::new();
    for selection in selections {
        view.check_target(selection.buffer(), selection.range())
            .map_err(|error| RegisterEditError::Plan(MultiCursorError::View(error)))?;
        selection.range().slice_in(text).map_err(|error| {
            RegisterEditError::Plan(MultiCursorError::View(
                crate::editor::workspace::view::ViewError::InvalidSpan(error),
            ))
        })?;
        ranges.push(selection.range());
    }
    ranges.sort_by_key(|span| (span.start(), span.end()));
    ranges.dedup();
    let mut fragments = String::new();
    for span in ranges {
        fragments.push_str(span.slice_in(text).expect("validated span"));
    }
    let entry = RegisterEntry {
        text: Text::from_str(&fragments),
        kind: target.kind,
    };
    registers.put(intent.actor(), target.name, entry.clone());
    rings.get_mut(intent.actor()).push(entry.clone());
    Ok(entry)
}

/// Prepares a cut without changing registers or the snapshot.
pub fn cut(
    intent: EditIntent,
    view: &BufferView,
    snapshot: &WorkspaceSnapshot,
    selections: &[Selection],
    name: RegisterName,
    kind: RegisterKind,
) -> Result<CutPlan, RegisterEditError> {
    let transaction = deletions(intent, view, snapshot, selections)?;
    let entry = extract(snapshot, &transaction, kind);
    Ok(CutPlan {
        base: snapshot.clone(),
        transaction,
        name,
        entry,
    })
}

/// Owns the register store, kill rings, and their receipt replay state.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CutReceipts {
    registers: RegisterStore,
    rings: KillRings,
    submitted: BTreeMap<TxnId, (CutPlan, CommitReceipt)>,
    applied: BTreeMap<TxnId, (CommitReceipt, CutPlan, WorkspaceSnapshot)>,
}

impl CutReceipts {
    /// Returns the owned register store.
    pub fn registers(&self) -> &RegisterStore {
        &self.registers
    }

    /// Returns the owned kill rings.
    pub fn rings(&self) -> &KillRings {
        &self.rings
    }

    /// Copies selections into the owned register store and kill rings.
    pub fn copy(
        &mut self,
        intent: EditIntent,
        view: &BufferView,
        snapshot: &WorkspaceSnapshot,
        selections: &[Selection],
        target: CopyTarget,
    ) -> Result<RegisterEntry, RegisterEditError> {
        copy(
            &mut self.registers,
            &mut self.rings,
            intent,
            view,
            snapshot,
            selections,
            target,
        )
    }

    /// Applies a committed cut exactly once after checking both snapshots.
    /// Returns `true` only when this call writes the register and kill ring.
    pub fn apply(
        &mut self,
        actor: ActorId,
        plan: &CutPlan,
        outcome: SubmitOutcome,
        sequencer: &Sequencer,
    ) -> Result<bool, RegisterEditError> {
        let receipt = outcome
            .committed_receipt()
            .ok_or(RegisterEditError::MismatchedCommit)?;
        if actor != plan.transaction.intent().actor()
            || receipt.intent() != plan.transaction.intent()
        {
            return Err(RegisterEditError::MismatchedCommit);
        }
        match self.submitted.get(&receipt.txn()) {
            Some((recorded, original)) if recorded == plan && *original == receipt => {}
            Some(_) => return Err(RegisterEditError::ConflictingReplay),
            None => return Err(RegisterEditError::MismatchedCommit),
        }
        if let Some((old_receipt, old_plan, old_snapshot)) = self.applied.get(&receipt.txn()) {
            let event = WorkspaceEvent::from_receipt(sequencer, receipt)
                .ok_or(RegisterEditError::MismatchedCommit)?;
            if event.patches() != plan.transaction.patches()
                || !same_workspace(old_snapshot, &sequencer.snapshot())
            {
                return Err(RegisterEditError::MismatchedCommit);
            }
            return if *old_receipt == receipt && old_plan == plan {
                Ok(false)
            } else {
                Err(RegisterEditError::ConflictingReplay)
            };
        }
        let event = WorkspaceEvent::from_receipt(sequencer, receipt)
            .ok_or(RegisterEditError::MismatchedCommit)?;
        if event.patches() != plan.transaction.patches() {
            return Err(RegisterEditError::MismatchedCommit);
        }
        let recorded = sequencer
            .history()
            .get_by_revision(receipt.committed())
            .ok_or(RegisterEditError::MismatchedCommit)?;
        let result = plan
            .transaction
            .apply_with_inverse(&plan.base)
            .map_err(|_| RegisterEditError::MismatchedCommit)?;
        if result.is_noop()
            || receipt.previous() != plan.base.revision()
            || recorded.inverse() != result.inverse()
            || !same_workspace(&plan.base, &sequencer.snapshot())
        {
            return Err(RegisterEditError::MismatchedCommit);
        }
        self.registers
            .put(actor, plan.name.clone(), plan.entry.clone());
        self.rings.get_mut(actor).push(plan.entry.clone());
        self.applied
            .insert(receipt.txn(), (receipt, plan.clone(), plan.base.clone()));
        Ok(true)
    }
}

/// Plans replacement of each selection with the actor's register payload.
pub fn paste(
    registers: &RegisterStore,
    intent: EditIntent,
    view: &BufferView,
    snapshot: &WorkspaceSnapshot,
    selections: &[Selection],
    name: &RegisterName,
) -> Result<Transaction, RegisterEditError> {
    let entry = registers
        .get(intent.actor(), name)
        .ok_or(RegisterEditError::MissingRegister)?;
    paste_entry(intent, view, snapshot, selections, entry)
}

/// Plans an Emacs-style yank from the actor's currently selected kill.
pub fn yank(
    rings: &KillRings,
    intent: EditIntent,
    view: &BufferView,
    snapshot: &WorkspaceSnapshot,
    selections: &[Selection],
) -> Result<Transaction, RegisterEditError> {
    let entry = rings
        .get(intent.actor())
        .and_then(|ring| ring.current())
        .ok_or(RegisterEditError::MissingRegister)?;
    paste_entry(intent, view, snapshot, selections, entry)
}

fn paste_entry(
    intent: EditIntent,
    view: &BufferView,
    snapshot: &WorkspaceSnapshot,
    selections: &[Selection],
    entry: &RegisterEntry,
) -> Result<Transaction, RegisterEditError> {
    view.visible_text(snapshot.catalog())
        .map_err(|error| RegisterEditError::Plan(MultiCursorError::View(error)))?;
    let live = snapshot
        .catalog()
        .get(view.buffer())
        .expect("validated view");
    let mut spans = Vec::new();
    for selection in selections {
        let span = selection.range();
        view.check_target(selection.buffer(), span)
            .map_err(|error| RegisterEditError::Plan(MultiCursorError::View(error)))?;
        span.slice_in(live).map_err(|error| {
            RegisterEditError::Plan(MultiCursorError::View(
                crate::editor::workspace::view::ViewError::InvalidSpan(error),
            ))
        })?;
        spans.push(span);
    }
    spans.sort_by_key(|span| (span.start(), span.end()));
    spans.dedup();
    let mut edits: Vec<(ByteSpan, Text)> = Vec::new();
    for span in spans {
        if let Some((previous, replacement)) = edits.last_mut() {
            if span.start() < previous.end() {
                return Err(RegisterEditError::Plan(MultiCursorError::Patch(
                    PatchError::Contact {
                        previous: *previous,
                        next: span,
                    },
                )));
            }
            if span.start() == previous.end() {
                *previous = ByteSpan::new(previous.start(), span.end()).expect("ordered spans");
                *replacement = replacement.concat(&entry.text);
                continue;
            }
        }
        edits.push((span, entry.text.clone()));
    }
    let commands = edits
        .into_iter()
        .map(|(span, text)| EditCommand::Replace { span, text })
        .collect();
    multi_cursor::plan(intent, view, snapshot, commands).map_err(RegisterEditError::Plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::collab::{receipt::RejectReason, registry::ActorRegistry, role::ActorRole};
    use crate::editor::edit::ids::{BufferId, Nonce};
    use crate::editor::test_support::{assert_law, texts};
    use crate::editor::workspace::catalog::BufferCatalog;

    #[test]
    fn cut_adjacent_utf8_selections() {
        let mut receipts = CutReceipts::default();

        let id = BufferId::new(1);
        let actor = ActorId::new(1);
        let text = Text::from_str("a界b");
        let mut catalog = BufferCatalog::new();
        catalog.register(id, text.clone()).unwrap();
        let mut actors = ActorRegistry::new();
        actors.register(actor, ActorRole::Human).unwrap();
        let mut sequencer = Sequencer::new(catalog, actors);
        let snapshot = sequencer.snapshot();
        let view = BufferView::full(snapshot.catalog(), id).unwrap();
        let selection = |a, b| {
            Selection::new(
                snapshot.catalog(),
                &view,
                id,
                text.validate_offset(a).unwrap(),
                text.validate_offset(b).unwrap(),
            )
            .unwrap()
        };
        let intent = EditIntent::for_actor(actor, Nonce::new(1), snapshot.revision());
        let name = RegisterName::new("a").unwrap();
        let plan = cut(
            intent,
            &view,
            &snapshot,
            &[selection(0, 1), selection(1, 4)],
            name.clone(),
            RegisterKind::Charwise,
        )
        .unwrap();

        let outcome = submit_cut(&mut receipts, &mut sequencer, &plan).unwrap();
        assert_eq!(
            sequencer.snapshot().catalog().get(id).unwrap().as_str(),
            "b"
        );

        assert_eq!(receipts.apply(actor, &plan, outcome, &sequencer), Ok(true));
        assert_eq!(
            receipts.registers.get(actor, &name).unwrap().text.as_str(),
            "a界"
        );
        assert_eq!(
            receipts
                .rings
                .get(actor)
                .unwrap()
                .current()
                .unwrap()
                .text
                .as_str(),
            "a界"
        );
    }

    #[test]
    fn copy_cut_replay_paste_and_isolation_laws() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let text = Text::from_str(source);
                for pair in text.char_boundaries().windows(2) {
                    let mut receipts = CutReceipts::default();
                    let id = BufferId::new(1);
                    let actor = ActorId::new(1);
                    let other = ActorId::new(2);
                    let name = RegisterName::new("a").unwrap();
                    let mut catalog = BufferCatalog::new();
                    catalog.register(id, text.clone()).unwrap();
                    let mut actors = ActorRegistry::new();
                    actors.register(actor, ActorRole::Human).unwrap();
                    let mut sequencer = Sequencer::new(catalog, actors);
                    let snapshot = sequencer.snapshot();
                    let view = BufferView::full(snapshot.catalog(), id).unwrap();
                    let selection =
                        Selection::new(snapshot.catalog(), &view, id, pair[0], pair[1]).unwrap();
                    let intent = EditIntent::for_actor(actor, Nonce::new(1), snapshot.revision());
                    let expected = &source[pair[0].as_usize()..pair[1].as_usize()];

                    let sentinel = RegisterEntry {
                        text: Text::from_str("other"),
                        kind: RegisterKind::Linewise,
                    };
                    receipts
                        .registers
                        .put(other, name.clone(), sentinel.clone());
                    receipts.rings.get_mut(other).push(sentinel.clone());
                    let copied = receipts
                        .copy(
                            intent,
                            &view,
                            &snapshot,
                            &[selection],
                            CopyTarget {
                                name: name.clone(),
                                kind: RegisterKind::Charwise,
                            },
                        )
                        .unwrap();
                    assert_eq!(copied.text.as_str(), expected);
                    assert_eq!(receipts.registers.get(actor, &name), Some(&copied));
                    assert_eq!(receipts.rings.get(actor).unwrap().current(), Some(&copied));
                    assert_eq!(sequencer.snapshot(), snapshot);
                    let plan = cut(
                        intent,
                        &view,
                        &snapshot,
                        &[selection],
                        name.clone(),
                        RegisterKind::Charwise,
                    )
                    .unwrap();
                    assert_eq!(plan.entry(), &copied);
                    receipts
                        .registers
                        .put(actor, name.clone(), sentinel.clone());
                    let saved = (
                        receipts.registers.clone(),
                        receipts.rings.clone(),
                        (receipts.submitted.clone(), receipts.applied.clone()),
                    );

                    let forged = SubmitOutcome::Committed(
                        CommitReceipt::new(intent, snapshot.revision()).unwrap(),
                    );
                    assert!(receipts.apply(actor, &plan, forged, &sequencer).is_err());
                    assert_eq!(
                        (
                            receipts.registers.clone(),
                            receipts.rings.clone(),
                            (receipts.submitted.clone(), receipts.applied.clone())
                        ),
                        saved
                    );
                    let outcome = submit_cut(&mut receipts, &mut sequencer, &plan).unwrap();
                    let saved = (
                        receipts.registers.clone(),
                        receipts.rings.clone(),
                        (receipts.submitted.clone(), receipts.applied.clone()),
                    );
                    let committed = sequencer.snapshot();
                    assert!(receipts.apply(other, &plan, outcome, &sequencer).is_err());
                    assert_eq!(
                        (
                            receipts.registers.clone(),
                            receipts.rings.clone(),
                            (receipts.submitted.clone(), receipts.applied.clone())
                        ),
                        saved
                    );
                    assert_eq!(receipts.apply(actor, &plan, outcome, &sequencer), Ok(true));
                    assert_eq!(receipts.registers.get(actor, &name), Some(&copied));
                    assert_eq!(receipts.rings.get(actor).unwrap().current(), Some(&copied));
                    let saved = (
                        receipts.registers.clone(),
                        receipts.rings.clone(),
                        (receipts.submitted.clone(), receipts.applied.clone()),
                    );
                    assert_eq!(receipts.apply(actor, &plan, outcome, &sequencer), Ok(false));
                    assert_eq!(
                        (
                            receipts.registers.clone(),
                            receipts.rings.clone(),
                            (receipts.submitted.clone(), receipts.applied.clone())
                        ),
                        saved
                    );
                    let changed = cut(
                        intent,
                        &view,
                        &snapshot,
                        &[selection],
                        RegisterName::new("b").unwrap(),
                        RegisterKind::Charwise,
                    )
                    .unwrap();
                    assert_eq!(
                        receipts.apply(actor, &changed, outcome, &sequencer),
                        Err(RegisterEditError::ConflictingReplay)
                    );
                    assert_eq!(
                        (
                            receipts.registers.clone(),
                            receipts.rings.clone(),
                            (receipts.submitted.clone(), receipts.applied.clone())
                        ),
                        saved
                    );
                    assert_eq!(receipts.registers.get(other, &name), Some(&sentinel));
                    assert_eq!(
                        receipts.rings.get(other).unwrap().current(),
                        Some(&sentinel)
                    );
                    let current_view = BufferView::full(committed.catalog(), id).unwrap();
                    let current_text = committed.catalog().get(id).unwrap();
                    let at = current_text.validate_offset(pair[0].as_usize()).unwrap();
                    let caret =
                        Selection::new(committed.catalog(), &current_view, id, at, at).unwrap();
                    let paste_intent =
                        EditIntent::for_actor(actor, Nonce::new(2), committed.revision());
                    let pasted = paste(
                        &receipts.registers,
                        paste_intent,
                        &current_view,
                        &committed,
                        &[caret],
                        &name,
                    )
                    .unwrap();
                    assert_eq!(
                        pasted
                            .apply_with_inverse(&committed)
                            .unwrap()
                            .catalog()
                            .get(id)
                            .unwrap()
                            .as_str(),
                        source
                    );
                }
            }
        });
    }

    #[test]
    fn cut_ignores_valid_empty_caret_but_rejects_invalid_boundary() {
        let id = BufferId::new(1);
        let actor = ActorId::new(1);
        let text = Text::from_str("a界b!");
        let mut catalog = BufferCatalog::new();
        catalog.register(id, text.clone()).unwrap();
        let mut actors = ActorRegistry::new();
        actors.register(actor, ActorRole::Human).unwrap();
        let mut sequencer = Sequencer::new(catalog, actors);
        let snapshot = sequencer.snapshot();
        let view = BufferView::full(snapshot.catalog(), id).unwrap();
        let selection = |a, b| {
            Selection::new(
                snapshot.catalog(),
                &view,
                id,
                text.validate_offset(a).unwrap(),
                text.validate_offset(b).unwrap(),
            )
            .unwrap()
        };
        let mut ascii = BufferCatalog::new();
        let ascii_text = Text::from_str("aaaaaa");
        ascii.register(id, ascii_text.clone()).unwrap();
        let ascii_view = BufferView::full(&ascii, id).unwrap();
        let invalid = ascii_text.validate_offset(2).unwrap();
        let invalid = Selection::new(&ascii, &ascii_view, id, invalid, invalid).unwrap();
        let intent = EditIntent::for_actor(actor, Nonce::new(1), snapshot.revision());
        let name = RegisterName::new("a").unwrap();
        assert!(matches!(
            cut(
                intent,
                &view,
                &snapshot,
                &[selection(0, 4), invalid],
                name.clone(),
                RegisterKind::Charwise
            ),
            Err(RegisterEditError::Plan(MultiCursorError::View(
                crate::editor::workspace::view::ViewError::InvalidSpan(_)
            )))
        ));
        let empty = cut(
            intent,
            &view,
            &snapshot,
            &[selection(1, 1)],
            name.clone(),
            RegisterKind::Charwise,
        )
        .unwrap();
        assert!(empty
            .transaction()
            .apply_with_inverse(&snapshot)
            .unwrap()
            .is_noop());
        let plan = cut(
            intent,
            &view,
            &snapshot,
            &[selection(0, 4), selection(1, 1)],
            name.clone(),
            RegisterKind::Charwise,
        )
        .unwrap();
        assert_eq!(plan.entry().text.as_str(), "a界");
        let mut receipts = CutReceipts::default();
        let outcome = submit_cut(&mut receipts, &mut sequencer, &plan).unwrap();
        assert_eq!(
            sequencer.snapshot().catalog().get(id).unwrap().as_str(),
            "b!"
        );
        assert_eq!(receipts.apply(actor, &plan, outcome, &sequencer), Ok(true));
        assert_eq!(receipts.registers().get(actor, &name), Some(plan.entry()));
        assert_eq!(
            receipts.rings().get(actor).unwrap().nth(0),
            Some(plan.entry())
        );
        assert_eq!(receipts.rings().get(actor).unwrap().nth(1), None);
        let before = (receipts.registers().clone(), receipts.rings().clone());
        assert_eq!(receipts.apply(actor, &plan, outcome, &sequencer), Ok(false));
        assert_eq!(
            (receipts.registers().clone(), receipts.rings().clone()),
            before
        );
        assert_eq!(receipts.rings().get(actor).unwrap().nth(1), None);
    }

    #[test]
    fn adjacent_copy_and_distinct_committed_patch() {
        let mut receipts = CutReceipts::default();

        let id = BufferId::new(1);
        let actor = ActorId::new(1);
        let name = RegisterName::new("a").unwrap();
        let text = Text::from_str("abab");
        let mut catalog = BufferCatalog::new();
        catalog.register(id, text.clone()).unwrap();
        let mut actors = ActorRegistry::new();
        actors.register(actor, ActorRole::Human).unwrap();
        let mut sequencer = Sequencer::new(catalog, actors);
        let base = sequencer.snapshot();
        let view = BufferView::full(base.catalog(), id).unwrap();
        let selection = |a, b| {
            Selection::new(
                base.catalog(),
                &view,
                id,
                text.validate_offset(a).unwrap(),
                text.validate_offset(b).unwrap(),
            )
            .unwrap()
        };
        let intent = EditIntent::for_actor(actor, Nonce::new(1), base.revision());

        let copied = receipts
            .copy(
                intent,
                &view,
                &base,
                &[selection(0, 1), selection(1, 2)],
                CopyTarget {
                    name: name.clone(),
                    kind: RegisterKind::Charwise,
                },
            )
            .unwrap();
        assert_eq!(copied.text.as_str(), "ab");
        let actual = cut(
            intent,
            &view,
            &base,
            &[selection(2, 4)],
            name.clone(),
            RegisterKind::Charwise,
        )
        .unwrap();
        let different = cut(
            intent,
            &view,
            &base,
            &[selection(1, 3)],
            name,
            RegisterKind::Charwise,
        )
        .unwrap();

        let outcome = submit_cut(&mut receipts, &mut sequencer, &actual).unwrap();
        let saved = (
            receipts.registers.clone(),
            receipts.rings.clone(),
            (receipts.submitted.clone(), receipts.applied.clone()),
        );
        assert_eq!(
            receipts.apply(actor, &different, outcome, &sequencer),
            Err(RegisterEditError::ConflictingReplay)
        );
        assert_eq!(
            (
                receipts.registers,
                receipts.rings,
                (receipts.submitted.clone(), receipts.applied.clone())
            ),
            saved
        );
    }

    #[test]
    fn failures_replay_origin_and_multicursor_paste() {
        let mut receipts = CutReceipts::default();

        let id = BufferId::new(1);
        let actor = ActorId::new(1);
        let name = RegisterName::new("a").unwrap();
        let text = Text::from_str("abab");
        let mut catalog = BufferCatalog::new();
        catalog.register(id, text.clone()).unwrap();
        let mut actors = ActorRegistry::new();
        actors.register(actor, ActorRole::Human).unwrap();
        let mut sequencer = Sequencer::new(catalog.clone(), actors.clone());
        let mut foreign = Sequencer::new(catalog, actors);
        let base = sequencer.snapshot();
        let foreign_base = foreign.snapshot();
        let view = BufferView::full(base.catalog(), id).unwrap();
        let selection = |snapshot: &WorkspaceSnapshot, a, b| {
            Selection::new(
                snapshot.catalog(),
                &view,
                id,
                text.validate_offset(a).unwrap(),
                text.validate_offset(b).unwrap(),
            )
            .unwrap()
        };
        let intent = EditIntent::for_actor(actor, Nonce::new(1), base.revision());
        let plan = cut(
            intent,
            &view,
            &base,
            &[selection(&base, 2, 4)],
            name.clone(),
            RegisterKind::Charwise,
        )
        .unwrap();
        let foreign_plan = cut(
            intent,
            &view,
            &foreign_base,
            &[selection(&foreign_base, 1, 3)],
            name.clone(),
            RegisterKind::Charwise,
        )
        .unwrap();

        let saved = (
            receipts.registers.clone(),
            receipts.rings.clone(),
            (receipts.submitted.clone(), receipts.applied.clone()),
        );
        for outcome in [
            SubmitOutcome::Rejected {
                intent,
                revision: base.revision(),
                reason: RejectReason::Conflict,
            },
            SubmitOutcome::NoOp {
                intent,
                revision: base.revision(),
            },
        ] {
            assert_eq!(
                receipts.apply(actor, &plan, outcome, &sequencer),
                Err(RegisterEditError::MismatchedCommit)
            );
            assert_eq!(
                (
                    receipts.registers.clone(),
                    receipts.rings.clone(),
                    (receipts.submitted.clone(), receipts.applied.clone())
                ),
                saved
            );
        }
        let stale_intent = EditIntent::for_actor(actor, Nonce::new(9), base.revision());
        let stale_plan = cut(
            stale_intent,
            &view,
            &base,
            &[selection(&base, 0, 1)],
            name.clone(),
            RegisterKind::Charwise,
        )
        .unwrap();
        let outcome = submit_cut(&mut receipts, &mut sequencer, &plan).unwrap();
        let foreign_outcome = foreign.submit(foreign_plan.transaction().clone());
        assert_eq!(outcome, foreign_outcome);
        assert_eq!(
            submit_cut(&mut receipts, &mut sequencer, &plan),
            Ok(outcome)
        );
        sequencer.prune_before(sequencer.revision());
        assert_eq!(receipts.apply(actor, &plan, outcome, &sequencer), Ok(true));
        let saved = (
            receipts.registers.clone(),
            receipts.rings.clone(),
            (receipts.submitted.clone(), receipts.applied.clone()),
        );
        assert_eq!(
            receipts.apply(actor, &plan, foreign_outcome, &foreign),
            Err(RegisterEditError::MismatchedCommit)
        );
        assert_eq!(
            (
                receipts.registers.clone(),
                receipts.rings.clone(),
                (receipts.submitted.clone(), receipts.applied.clone())
            ),
            saved
        );
        let current = sequencer.snapshot();
        assert_eq!(
            submit_cut(&mut receipts, &mut sequencer, &stale_plan),
            Err(RegisterEditError::MismatchedCommit)
        );
        assert_eq!(sequencer.snapshot(), current);
        assert_eq!(
            (
                receipts.registers.clone(),
                receipts.rings.clone(),
                (receipts.submitted.clone(), receipts.applied.clone())
            ),
            saved
        );
        assert!(cut(
            intent,
            &view,
            &current,
            &[selection(&base, 0, 1)],
            name.clone(),
            RegisterKind::Charwise
        )
        .is_err());
        let current_view = BufferView::full(current.catalog(), id).unwrap();
        let current_text = current.catalog().get(id).unwrap();
        let caret = |n| {
            let at = current_text.validate_offset(n).unwrap();
            Selection::new(current.catalog(), &current_view, id, at, at).unwrap()
        };
        let paste_intent = EditIntent::for_actor(actor, Nonce::new(2), current.revision());
        let result = paste(
            &receipts.registers,
            paste_intent,
            &current_view,
            &current,
            &[caret(0), caret(2)],
            &name,
        )
        .unwrap();
        assert_eq!(
            yank(
                &receipts.rings,
                paste_intent,
                &current_view,
                &current,
                &[caret(0), caret(2)]
            ),
            Ok(result.clone())
        );
        assert_eq!(
            result
                .apply_with_inverse(&current)
                .unwrap()
                .catalog()
                .get(id)
                .unwrap()
                .as_str(),
            "ababab"
        );
        assert!(sequencer.submit(result).is_committed());
        sequencer.prune_before(sequencer.revision());
        assert_eq!(receipts.apply(actor, &plan, outcome, &sequencer), Ok(false));
        assert_eq!(
            (
                receipts.registers,
                receipts.rings,
                (receipts.submitted.clone(), receipts.applied.clone())
            ),
            saved
        );
    }

    #[test]
    fn first_receipt_after_later_commit_and_pruning() {
        let mut receipts = CutReceipts::default();

        let id = BufferId::new(1);
        let actor = ActorId::new(1);
        let text = Text::from_str("a界b");
        let mut catalog = BufferCatalog::new();
        catalog.register(id, text.clone()).unwrap();
        let mut actors = ActorRegistry::new();
        actors.register(actor, ActorRole::Human).unwrap();
        let mut sequencer = Sequencer::new(catalog, actors);
        let base = sequencer.snapshot();
        let view = BufferView::full(base.catalog(), id).unwrap();
        let selection = Selection::new(
            base.catalog(),
            &view,
            id,
            text.validate_offset(1).unwrap(),
            text.validate_offset(4).unwrap(),
        )
        .unwrap();
        let intent = EditIntent::for_actor(actor, Nonce::new(1), base.revision());
        let name = RegisterName::new("a").unwrap();
        let plan = cut(
            intent,
            &view,
            &base,
            &[selection],
            name.clone(),
            RegisterKind::Charwise,
        )
        .unwrap();

        let outcome = submit_cut(&mut receipts, &mut sequencer, &plan).unwrap();
        let next = sequencer.snapshot();
        let next_view = BufferView::full(next.catalog(), id).unwrap();
        let at = next.catalog().get(id).unwrap().validate_offset(2).unwrap();
        let later_intent = EditIntent::for_actor(actor, Nonce::new(2), next.revision());
        let later = multi_cursor::plan(
            later_intent,
            &next_view,
            &next,
            vec![EditCommand::Insert {
                at,
                text: Text::from_str("!"),
            }],
        )
        .unwrap();
        assert!(sequencer.submit(later).is_committed());
        sequencer.prune_before(sequencer.revision());
        assert_eq!(
            submit_cut(&mut receipts, &mut sequencer, &plan),
            Ok(outcome)
        );

        assert_eq!(receipts.apply(actor, &plan, outcome, &sequencer), Ok(true));
        assert_eq!(
            receipts.registers.get(actor, &name).unwrap().text.as_str(),
            "界"
        );
        assert_eq!(
            receipts
                .rings
                .get(actor)
                .unwrap()
                .current()
                .unwrap()
                .text
                .as_str(),
            "界"
        );
    }

    #[test]
    fn submitted_target_is_bound_before_first_apply_and_adjacent_paste() {
        let mut receipts = CutReceipts::default();

        let id = BufferId::new(1);
        let actor = ActorId::new(1);
        let text = Text::from_str("a界b");
        let mut catalog = BufferCatalog::new();
        catalog.register(id, text.clone()).unwrap();
        let mut actors = ActorRegistry::new();
        actors.register(actor, ActorRole::Human).unwrap();
        let mut sequencer = Sequencer::new(catalog, actors);
        let snapshot = sequencer.snapshot();
        let view = BufferView::full(snapshot.catalog(), id).unwrap();
        let selection = |start, end| {
            Selection::new(
                snapshot.catalog(),
                &view,
                id,
                text.validate_offset(start).unwrap(),
                text.validate_offset(end).unwrap(),
            )
            .unwrap()
        };
        let intent = EditIntent::for_actor(actor, Nonce::new(1), snapshot.revision());
        let name = RegisterName::new("a").unwrap();
        let original = cut(
            intent,
            &view,
            &snapshot,
            &[selection(1, 4)],
            name.clone(),
            RegisterKind::Charwise,
        )
        .unwrap();
        let other_name = cut(
            intent,
            &view,
            &snapshot,
            &[selection(1, 4)],
            RegisterName::new("b").unwrap(),
            RegisterKind::Charwise,
        )
        .unwrap();
        let other_kind = cut(
            intent,
            &view,
            &snapshot,
            &[selection(1, 4)],
            name.clone(),
            RegisterKind::Linewise,
        )
        .unwrap();

        let outcome = submit_cut(&mut receipts, &mut sequencer, &original).unwrap();

        let before = (
            receipts.registers.clone(),
            receipts.rings.clone(),
            (receipts.submitted.clone(), receipts.applied.clone()),
        );
        for changed in [&other_name, &other_kind] {
            assert_eq!(
                submit_cut(&mut receipts, &mut sequencer, changed),
                Err(RegisterEditError::ConflictingReplay)
            );
            assert_eq!(
                receipts.apply(actor, changed, outcome, &sequencer),
                Err(RegisterEditError::ConflictingReplay)
            );
            assert_eq!(
                (
                    receipts.registers.clone(),
                    receipts.rings.clone(),
                    (receipts.submitted.clone(), receipts.applied.clone())
                ),
                before
            );
        }
        assert_eq!(
            receipts.apply(actor, &original, outcome, &sequencer),
            Ok(true)
        );
        assert_eq!(
            receipts.registers.get(actor, &name).unwrap().text.as_str(),
            "界"
        );
        let mut source_registers = RegisterStore::new();
        source_registers.put(
            actor,
            name.clone(),
            RegisterEntry {
                text: Text::from_str("X"),
                kind: RegisterKind::Charwise,
            },
        );
        let pasted = paste(
            &source_registers,
            intent,
            &view,
            &snapshot,
            &[selection(0, 1), selection(1, 4)],
            &name,
        )
        .unwrap();
        assert_eq!(
            pasted
                .apply_with_inverse(&snapshot)
                .unwrap()
                .catalog()
                .get(id)
                .unwrap()
                .as_str(),
            "XXb"
        );
    }
}
