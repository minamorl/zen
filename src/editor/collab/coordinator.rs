//! Atomic coordination of collaborative workspace operations.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    access::{AccessError, AccessPolicy, Capability},
    checkpoints::{Checkpoint, CheckpointError, CheckpointOp, CheckpointOutcome, CheckpointState},
    proposal::{Approval, Proposal, ProposalError, RequestId},
    registry::ActorRegistry,
};
use crate::editor::{
    edit::{
        ids::{ActorId, BufferId, TxnId},
        intent::EditIntent,
        register_edit::{self, RegisterEditError},
        transaction::Transaction,
    },
    text::utf8::Text,
    workspace::{
        buffer_lifecycle::ClosePolicy,
        registers::{KillRings, RegisterEntry, RegisterKind, RegisterName, RegisterStore},
        selections::Selection,
        snapshots::WorkspaceSnapshot,
        version::Revision,
        view::BufferView,
    },
};

/// One request, including its identity and complete payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    Open {
        intent: EditIntent,
        name: String,
        text: Text,
    },
    Close {
        intent: EditIntent,
        buffer: BufferId,
        policy: ClosePolicy,
    },
    Edit(Transaction),
    Cut {
        intent: EditIntent,
        view: BufferView,
        selections: Vec<Selection>,
        name: RegisterName,
        kind: RegisterKind,
    },
    Undo {
        actor: ActorId,
        target: TxnId,
        transaction: TxnId,
    },
    Redo {
        actor: ActorId,
        target: TxnId,
        transaction: TxnId,
    },
}

fn operation_digest(operation: &Operation) -> [u8; 32] {
    fn number(bytes: &mut Vec<u8>, n: u64) {
        bytes.extend_from_slice(&n.to_be_bytes());
    }
    fn data(bytes: &mut Vec<u8>, value: &[u8]) {
        number(bytes, u64::try_from(value.len()).expect("length fits u64"));
        bytes.extend_from_slice(value);
    }
    fn intent(bytes: &mut Vec<u8>, value: EditIntent) {
        number(bytes, value.actor().raw());
        number(bytes, value.nonce().raw());
        number(bytes, value.base().raw());
    }
    fn txn_id(bytes: &mut Vec<u8>, value: TxnId) {
        number(bytes, value.actor().raw());
        number(bytes, value.nonce().raw());
    }
    let mut bytes = b"zen:coordinator:operation:v1\0".to_vec();
    match operation {
        Operation::Open {
            intent: value,
            name,
            text,
        } => {
            bytes.push(0);
            intent(&mut bytes, *value);
            data(&mut bytes, name.as_bytes());
            data(&mut bytes, text.as_bytes());
        }
        Operation::Close {
            intent: value,
            buffer,
            policy,
        } => {
            bytes.push(1);
            intent(&mut bytes, *value);
            number(&mut bytes, buffer.raw());
            bytes.push(match policy {
                ClosePolicy::RequireClean => 0,
                ClosePolicy::Discard => 1,
            });
        }
        Operation::Edit(txn) => {
            bytes.push(2);
            bytes.extend_from_slice(&super::retention::transaction_digest(txn));
        }
        Operation::Cut {
            intent: value,
            view,
            selections,
            name,
            kind,
        } => {
            bytes.push(3);
            intent(&mut bytes, *value);
            number(&mut bytes, view.buffer().raw());
            number(&mut bytes, view.visible().start().as_usize() as u64);
            number(&mut bytes, view.visible().end().as_usize() as u64);
            number(&mut bytes, selections.len() as u64);
            for selection in selections {
                number(&mut bytes, selection.buffer().raw());
                number(&mut bytes, selection.anchor().as_usize() as u64);
                number(&mut bytes, selection.head().as_usize() as u64);
            }
            data(&mut bytes, name.as_str().as_bytes());
            bytes.push(match kind {
                RegisterKind::Charwise => 0,
                RegisterKind::Linewise => 1,
                RegisterKind::Blockwise => 2,
            });
        }
        Operation::Undo {
            actor,
            target,
            transaction,
        }
        | Operation::Redo {
            actor,
            target,
            transaction,
        } => {
            bytes.push(if matches!(operation, Operation::Undo { .. }) {
                4
            } else {
                5
            });
            number(&mut bytes, actor.raw());
            txn_id(&mut bytes, *target);
            txn_id(&mut bytes, *transaction);
        }
    }
    super::retention::sha256(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::role::ActorRole,
        edit::{ids::Nonce, patch::Patch, splice::Splice},
        test_support::assert_law,
        text::span::ByteSpan,
    };

    fn intent(actor: u64, nonce: u64, base: u64) -> EditIntent {
        EditIntent::for_actor(ActorId::new(actor), Nonce::new(nonce), Revision::new(base))
    }

    fn state() -> Coordinator {
        let mut actors = ActorRegistry::new();
        actors.register(ActorId::new(1), ActorRole::Human).unwrap();
        actors.register(ActorId::new(2), ActorRole::Agent).unwrap();
        let mut access = AccessPolicy::new();
        for capability in Capability::ALL {
            access.grant_default(capability);
        }
        Coordinator::new(actors, access)
    }

    fn edit(actor: u64, nonce: u64, base: u64, id: BufferId, old: &str, new: &str) -> Transaction {
        let text = Text::from_str(old);
        let patch = Patch::new(vec![Splice::new(
            ByteSpan::full_of(&text),
            Text::from_str(new),
        )])
        .unwrap();
        Transaction::new(intent(actor, nonce, base), vec![(id, patch)]).unwrap()
    }

    #[test]
    fn direct_edit_across_unrelated_open_and_close() {
        let mut s = state();
        let a = s
            .apply(
                RequestId::new(1),
                Operation::Open {
                    intent: intent(1, 1, 0),
                    name: "a".into(),
                    text: Text::from_str("a"),
                },
            )
            .unwrap()
            .buffer
            .unwrap();
        let b = s
            .apply(
                RequestId::new(2),
                Operation::Open {
                    intent: intent(1, 2, 1),
                    name: "b".into(),
                    text: Text::from_str("b"),
                },
            )
            .unwrap()
            .buffer
            .unwrap();
        s.apply(
            RequestId::new(3),
            Operation::Close {
                intent: intent(1, 3, 2),
                buffer: b,
                policy: ClosePolicy::Discard,
            },
        )
        .unwrap();
        let mut s = Coordinator::restore(&s.checkpoint()).unwrap();
        s.apply(
            RequestId::new(4),
            Operation::Edit(edit(1, 4, 1, a, "a", "A")),
        )
        .unwrap();
        assert_eq!(s.snapshot().catalog().get(a).unwrap().as_str(), "A");
        let before = s.checkpoint();
        assert!(s
            .apply(
                RequestId::new(5),
                Operation::Edit(edit(1, 5, 2, b, "b", "B"))
            )
            .is_err());
        assert_eq!(s.checkpoint(), before);
    }

    #[test]
    fn undo_across_unrelated_close_and_redo_across_unrelated_open() {
        let mut s = state();
        let a = s
            .apply(
                RequestId::new(1),
                Operation::Open {
                    intent: intent(1, 1, 0),
                    name: "a".into(),
                    text: Text::from_str("a"),
                },
            )
            .unwrap()
            .buffer
            .unwrap();
        let b = s
            .apply(
                RequestId::new(2),
                Operation::Open {
                    intent: intent(1, 2, 1),
                    name: "b".into(),
                    text: Text::from_str("b"),
                },
            )
            .unwrap()
            .buffer
            .unwrap();
        s.apply(
            RequestId::new(3),
            Operation::Edit(edit(1, 3, 2, a, "a", "A")),
        )
        .unwrap();
        s.apply(
            RequestId::new(4),
            Operation::Close {
                intent: intent(1, 4, 3),
                buffer: b,
                policy: ClosePolicy::Discard,
            },
        )
        .unwrap();
        let actor = ActorId::new(1);
        let target = TxnId::new(actor, Nonce::new(3));
        s.apply(
            RequestId::new(5),
            Operation::Undo {
                actor,
                target,
                transaction: TxnId::new(actor, Nonce::new(5)),
            },
        )
        .unwrap();
        assert_eq!(s.snapshot().catalog().get(a).unwrap().as_str(), "a");
        s.apply(
            RequestId::new(6),
            Operation::Open {
                intent: intent(1, 6, 5),
                name: "c".into(),
                text: Text::empty(),
            },
        )
        .unwrap();
        let mut s = Coordinator::restore(&s.checkpoint()).unwrap();
        s.apply(
            RequestId::new(7),
            Operation::Redo {
                actor,
                target,
                transaction: TxnId::new(actor, Nonce::new(7)),
            },
        )
        .unwrap();
        assert_eq!(s.snapshot().catalog().get(a).unwrap().as_str(), "A");
    }

    #[test]
    fn compact_checkpoint_drops_expired_edit_payload() {
        let mut s = state();
        let id = s
            .apply(
                RequestId::new(1),
                Operation::Open {
                    intent: intent(1, 1, 0),
                    name: "a".into(),
                    text: Text::from_str("start"),
                },
            )
            .unwrap()
            .buffer
            .unwrap();
        let marker = "unique-sentinel-817261";
        let op = Operation::Edit(edit(1, 2, 1, id, "start", marker));
        let outcome = s.apply(RequestId::new(2), op.clone()).unwrap();
        s.apply(
            RequestId::new(3),
            Operation::Edit(edit(1, 3, 2, id, marker, "done")),
        )
        .unwrap();
        s.compact(s.snapshot().revision()).unwrap();
        let checkpoint = s.checkpoint();
        assert!(!format!("{checkpoint:?}").contains(marker));
        let mut restored = Coordinator::restore(&checkpoint).unwrap();
        assert_eq!(restored.apply(RequestId::new(2), op), Ok(outcome));
        let actor = ActorId::new(1);
        let before = restored.checkpoint();
        assert_eq!(
            restored.apply(
                RequestId::new(4),
                Operation::Undo {
                    actor,
                    target: TxnId::new(actor, Nonce::new(2)),
                    transaction: TxnId::new(actor, Nonce::new(4))
                }
            ),
            Err(CoordinatorError::Checkpoint(CheckpointError::Retention(
                super::super::retention::RetentionError::BaseExpired {
                    base: Revision::new(2),
                    floor: Revision::new(3),
                }
            )))
        );
        assert_eq!(restored.checkpoint(), before);
        assert_eq!(
            restored.apply(
                RequestId::new(5),
                Operation::Undo {
                    actor,
                    target: TxnId::new(actor, Nonce::new(3)),
                    transaction: TxnId::new(actor, Nonce::new(5)),
                },
            ),
            Err(CoordinatorError::Checkpoint(CheckpointError::Retention(
                super::super::retention::RetentionError::BaseExpired {
                    base: Revision::new(3),
                    floor: Revision::new(3),
                }
            )))
        );
        assert_eq!(restored.checkpoint(), before);
    }

    #[test]
    fn compact_preserves_inverse_of_edit_above_floor() {
        let mut s = state();
        let id = s
            .apply(
                RequestId::new(1),
                Operation::Open {
                    intent: intent(1, 1, 0),
                    name: "a".into(),
                    text: Text::from_str("a"),
                },
            )
            .unwrap()
            .buffer
            .unwrap();
        s.apply(
            RequestId::new(2),
            Operation::Edit(edit(1, 2, 1, id, "a", "b")),
        )
        .unwrap();
        s.apply(
            RequestId::new(3),
            Operation::Edit(edit(1, 3, 2, id, "b", "c")),
        )
        .unwrap();
        s.compact(Revision::new(2)).unwrap();
        let actor = ActorId::new(1);
        assert!(s.edits[&TxnId::new(actor, Nonce::new(3))].inverse.is_some());
        s.apply(
            RequestId::new(4),
            Operation::Undo {
                actor,
                target: TxnId::new(actor, Nonce::new(3)),
                transaction: TxnId::new(actor, Nonce::new(4)),
            },
        )
        .unwrap();
        assert_eq!(s.snapshot().catalog().get(id).unwrap().as_str(), "b");
    }

    #[test]
    fn compacted_redo_is_rejected_without_mutation() {
        let mut s = state();
        let id = s
            .apply(
                RequestId::new(1),
                Operation::Open {
                    intent: intent(1, 1, 0),
                    name: "a".into(),
                    text: Text::from_str("a"),
                },
            )
            .unwrap()
            .buffer
            .unwrap();
        s.apply(
            RequestId::new(2),
            Operation::Edit(edit(1, 2, 1, id, "a", "b")),
        )
        .unwrap();
        let actor = ActorId::new(1);
        let target = TxnId::new(actor, Nonce::new(2));
        s.apply(
            RequestId::new(3),
            Operation::Undo {
                actor,
                target,
                transaction: TxnId::new(actor, Nonce::new(3)),
            },
        )
        .unwrap();
        s.compact(Revision::new(3)).unwrap();
        let before = s.checkpoint();
        assert_eq!(
            s.apply(
                RequestId::new(4),
                Operation::Redo {
                    actor,
                    target,
                    transaction: TxnId::new(actor, Nonce::new(4)),
                },
            ),
            Err(CoordinatorError::Checkpoint(CheckpointError::Retention(
                super::super::retention::RetentionError::BaseExpired {
                    base: Revision::new(3),
                    floor: Revision::new(3),
                }
            )))
        );
        assert_eq!(s.checkpoint(), before);
    }

    #[test]
    fn generated_reference_sequence_and_atomic_rejections() {
        assert_law(
            &[0, 1, 42, 99, u64::MAX],
            |rng| (0..16).map(|_| rng.gen_bool()).collect::<Vec<_>>(),
            |choices| {
                let mut actual = state();
                let mut revision = 0;
                let mut text = "start".to_owned();
                let open = Operation::Open {
                    intent: intent(1, 1, revision),
                    name: "a".into(),
                    text: Text::from_str(&text),
                };
                let outcome = actual.apply(RequestId::new(1), open.clone()).unwrap();
                let id = outcome.buffer.unwrap();
                revision += 1;
                assert_eq!(actual.apply(RequestId::new(1), open), Ok(outcome));
                for (index, choice) in choices.iter().enumerate() {
                    let actor = if *choice { 1 } else { 2 };
                    let nonce = 10 + index as u64;
                    let next = format!("value-{index}");
                    let txn = edit(actor, nonce, revision, id, &text, &next);
                    let request = RequestId::new(nonce);
                    let op = Operation::Edit(txn.clone());
                    let outcome = actual.apply(request, op.clone()).unwrap();
                    revision += 1;
                    text = next;
                    assert_eq!(actual.snapshot().revision(), Revision::new(revision));
                    assert_eq!(actual.snapshot().catalog().get(id).unwrap().as_str(), text);
                    assert_eq!(actual.apply(request, op), Ok(outcome));
                    let before = actual.checkpoint();
                    let conflict =
                        Operation::Edit(edit(actor, nonce + 1000, revision, id, &text, "wrong"));
                    assert_eq!(
                        actual.apply(request, conflict),
                        Err(CoordinatorError::RequestReuse(request))
                    );
                    assert_eq!(actual.checkpoint(), before);
                    let inactive =
                        Operation::Edit(edit(99, nonce + 2000, revision, id, &text, "wrong"));
                    assert!(matches!(
                        actual.apply(RequestId::new(nonce + 2000), inactive),
                        Err(CoordinatorError::Access(AccessError::InactiveActor(_)))
                    ));
                    assert_eq!(actual.checkpoint(), before);
                }
                let dirty = Operation::Close {
                    intent: intent(1, 100, revision),
                    buffer: id,
                    policy: ClosePolicy::RequireClean,
                };
                let before = actual.checkpoint();
                assert_eq!(
                    actual.apply(RequestId::new(100), dirty),
                    Err(CoordinatorError::Checkpoint(CheckpointError::Dirty(id)))
                );
                assert_eq!(actual.checkpoint(), before);
                let close = Operation::Close {
                    intent: intent(1, 101, revision),
                    buffer: id,
                    policy: ClosePolicy::Discard,
                };
                actual.apply(RequestId::new(101), close).unwrap();
                assert!(actual.snapshot().catalog().is_tombstoned(id));
            },
        );
    }

    #[test]
    fn generated_two_buffer_reference() {
        use crate::editor::collab::receipt::CommitReceipt;

        #[derive(Clone)]
        struct ReferenceEdit {
            root: TxnId,
            owner: ActorId,
            before: BTreeMap<BufferId, String>,
            after: BTreeMap<BufferId, String>,
            inverse_base: u64,
            undone: Option<TxnId>,
            spans: BTreeMap<BufferId, (usize, usize)>,
        }

        #[derive(Default)]
        struct Reference {
            revision: u64,
            floor: u64,
            edits: Vec<ReferenceEdit>,
            changed_at: BTreeMap<BufferId, u64>,
            contents: BTreeMap<BufferId, String>,
            tombstones: BTreeSet<BufferId>,
            registers: BTreeMap<(ActorId, RegisterName), RegisterEntry>,
            rings: BTreeMap<ActorId, Vec<RegisterEntry>>,
            receipts: BTreeMap<RequestId, (CommitReceipt, bool)>,
            issued: Vec<(RequestId, Operation, CheckpointOutcome)>,
            active: BTreeSet<ActorId>,
            defaults: BTreeSet<Capability>,
            grants: BTreeSet<(ActorId, BufferId, Capability)>,
            pending: BTreeMap<RequestId, Option<(ActorId, u64)>>,
            steps: Vec<(u64, BufferId, usize, usize, usize)>,
        }

        impl Reference {
            fn proposal_error(
                &self,
                request: RequestId,
                buffer: BufferId,
            ) -> Option<CoordinatorError> {
                let Some((actor, revision)) = self.pending[&request] else {
                    return Some(CoordinatorError::Unapproved(request));
                };
                if revision != self.revision {
                    return Some(CoordinatorError::StaleApproval(request));
                }
                if !self.active.contains(&actor) {
                    return Some(CoordinatorError::Proposal(ProposalError::Access(
                        AccessError::InactiveActor(actor),
                    )));
                }
                if !self.defaults.contains(&Capability::Review)
                    && !self.grants.contains(&(actor, buffer, Capability::Review))
                {
                    return Some(CoordinatorError::Proposal(ProposalError::Access(
                        AccessError::Denied {
                            actor,
                            buffer,
                            capability: Capability::Review,
                        },
                    )));
                }
                None
            }

            fn commit(
                &mut self,
                request: RequestId,
                intent: EditIntent,
                edit: bool,
            ) -> CommitReceipt {
                let receipt = CommitReceipt::new(intent, Revision::new(self.revision)).unwrap();
                self.revision += 1;
                self.receipts.insert(request, (receipt, edit));
                receipt
            }

            fn check(&self, actual: &Coordinator) {
                let snapshot = actual.snapshot();
                assert_eq!(
                    actual.pending.keys().copied().collect::<Vec<_>>(),
                    self.pending.keys().copied().collect::<Vec<_>>()
                );
                for (request, approval) in &self.pending {
                    assert_eq!(
                        actual.pending[request]
                            .approval
                            .as_ref()
                            .map(|a| (a.approver(), a.revision().raw())),
                        *approval
                    );
                }
                assert_eq!(snapshot.revision(), Revision::new(self.revision));
                assert_eq!(
                    snapshot.catalog().live_ids(),
                    self.contents.keys().copied().collect::<Vec<_>>()
                );
                assert_eq!(
                    snapshot.catalog().tombstoned_ids(),
                    self.tombstones.iter().copied().collect::<Vec<_>>()
                );
                for (id, text) in &self.contents {
                    assert_eq!(snapshot.catalog().get(*id).unwrap().as_str(), text);
                }
                for actor in [ActorId::new(1), ActorId::new(2)] {
                    assert_eq!(
                        actual.workspace().actors().is_active(actor),
                        self.active.contains(&actor)
                    );
                    for buffer in [BufferId::new(0), BufferId::new(1), BufferId::new(2)] {
                        for capability in Capability::ALL {
                            assert_eq!(
                                actual.access.is_granted(actor, buffer, capability),
                                self.defaults.contains(&capability)
                                    || self.grants.contains(&(actor, buffer, capability))
                            );
                        }
                    }
                    for name in [
                        RegisterName::new("a").unwrap(),
                        RegisterName::new("b").unwrap(),
                    ] {
                        assert_eq!(
                            actual.register(actor, &name),
                            self.registers.get(&(actor, name.clone()))
                        );
                    }
                    let entries = self.rings.get(&actor).map(Vec::as_slice).unwrap_or(&[]);
                    for (index, entry) in entries.iter().enumerate() {
                        assert_eq!(
                            actual.rings().get(actor).and_then(|ring| ring.nth(index)),
                            Some(entry)
                        );
                    }
                    assert_eq!(
                        actual
                            .rings()
                            .get(actor)
                            .and_then(|ring| ring.nth(entries.len())),
                        None
                    );
                }
                for (request, (receipt, edit)) in &self.receipts {
                    assert_eq!(
                        actual.workspace().receipt_by_request(*request),
                        Some(*receipt)
                    );
                    assert_eq!(
                        actual.workspace().history().receipt_by_request(*request),
                        edit.then_some(*receipt)
                    );
                }
            }
        }

        let mut coverage = [0usize; 14];
        let mut conflicting_redo_hits = 0usize;
        let mut expired_redo_hits = 0usize;
        assert_law(
            &[0, 1, 42, 99, u64::MAX],
            |rng| {
                (0..40)
                    .map(|_| {
                        (
                            rng.gen_bool(),
                            rng.gen_bool(),
                            rng.gen_bool(),
                            rng.gen_bool(),
                            rng.next_u64(),
                        )
                    })
                    .collect::<Vec<_>>()
            },
            |choices| {
                let mut actual = state();
                let mut model = Reference::default();
                macro_rules! live_state {
                    () => {{
                        let history = actual.workspace().history();
                        let floor = history.floor();
                        (
                            actual.checkpoint(),
                            actual.snapshot(),
                            floor,
                            history.tip(),
                            history.replay(floor),
                            actual.workspace().delivery().events().to_vec(),
                            format!("{:?}", actual.workspace()),
                            actual.registers().clone(),
                            actual.rings().clone(),
                        )
                    }};
                }
                model.active.extend([ActorId::new(1), ActorId::new(2)]);
                model.defaults.extend(Capability::ALL);
                for n in 0..2 {
                    let request = RequestId::new(n + 1);
                    let edit_intent = intent(1, n + 1, model.revision);
                    let id = BufferId::new(n);
                    let receipt = model.commit(request, edit_intent, false);
                    model.contents.insert(id, format!("initial-{n}"));
                    assert_eq!(
                        actual.apply(
                            request,
                            Operation::Open {
                                intent: edit_intent,
                                name: format!("buffer-{n}"),
                                text: Text::from_str(&format!("initial-{n}")),
                            }
                        ),
                        Ok(CheckpointOutcome {
                            receipt: Some(receipt),
                            buffer: Some(id)
                        })
                    );
                    model.check(&actual);
                }
                for (index, &(both, compact, undo_or_redo, lifecycle, choice)) in
                    choices.iter().enumerate()
                {
                    let nonce = index as u64 + 10;
                    if !model.issued.is_empty() {
                        let (old_request, old_operation, old_outcome) =
                            &model.issued[choice as usize % model.issued.len()];
                        assert_eq!(
                            actual.apply(*old_request, old_operation.clone()),
                            Ok(old_outcome.clone())
                        );
                        model.check(&actual);
                    }
                    let request = RequestId::new(nonce);
                    let id = BufferId::new((index % 2) as u64);
                    let actor = ActorId::new(if both { 2 } else { 1 });
                    if choice % 16 == 0 {
                        let actor = ActorId::new(1);
                        coverage[0] += 1;
                        let old = model.contents[&id].clone();
                        let next = format!("proposal-{nonce}");
                        let edit_intent = intent(1, nonce, model.revision);
                        let proposal =
                            Proposal::new(request, edit(1, nonce, model.revision, id, &old, &next));
                        assert_eq!(actual.propose(proposal), Ok(()));
                        model.pending.insert(request, None);
                        model.check(&actual);
                        let before = live_state!();
                        assert_eq!(
                            actual.commit_proposal(request),
                            Err(CoordinatorError::Unapproved(request))
                        );
                        assert_eq!(live_state!(), before);
                        assert_eq!(actual.approve(request, actor), Ok(()));
                        model.pending.insert(request, Some((actor, model.revision)));
                        model.check(&actual);
                        if compact {
                            coverage[1] += 1;
                            let other_id = BufferId::new(1 - id.raw());
                            let old_other = model.contents[&other_id].clone();
                            let next_other = format!("advance-{nonce}");
                            let advance_intent = intent(1, nonce + 1000, model.revision);
                            let advance_request = RequestId::new(nonce + 1000);
                            let operation = Operation::Edit(edit(
                                1,
                                nonce + 1000,
                                model.revision,
                                other_id,
                                &old_other,
                                &next_other,
                            ));
                            let receipt = model.commit(advance_request, advance_intent, true);
                            model.steps.push((
                                receipt.committed().raw(),
                                other_id,
                                old_other.len(),
                                0,
                                old_other.len(),
                            ));
                            model.changed_at.insert(other_id, receipt.committed().raw());
                            model.contents.insert(other_id, next_other);
                            assert_eq!(
                                actual.apply(advance_request, operation),
                                Ok(CheckpointOutcome {
                                    receipt: Some(receipt),
                                    buffer: None
                                })
                            );
                            let before = live_state!();
                            assert_eq!(
                                actual.commit_proposal(request),
                                Err(CoordinatorError::StaleApproval(request))
                            );
                            assert_eq!(live_state!(), before);
                            model.check(&actual);
                            continue;
                        }
                        if both {
                            actual.access_mut().revoke_default(Capability::Review);
                            model.defaults.remove(&Capability::Review);
                            actual
                                .access_mut()
                                .grant_buffer(actor, id, Capability::Review);
                            model.grants.insert((actor, id, Capability::Review));
                            model.check(&actual);
                            actual
                                .access_mut()
                                .revoke_buffer(actor, id, Capability::Review);
                            model.grants.remove(&(actor, id, Capability::Review));
                            let before = live_state!();
                            assert_eq!(
                                actual.commit_proposal(request),
                                Err(model.proposal_error(request, id).unwrap())
                            );
                            assert_eq!(live_state!(), before);
                            actual.access_mut().grant_default(Capability::Review);
                            model.defaults.insert(Capability::Review);
                        }
                        let receipt = model.commit(request, edit_intent, true);
                        model
                            .steps
                            .push((receipt.committed().raw(), id, old.len(), 0, old.len()));
                        model.pending.remove(&request);
                        model.contents.insert(id, next.clone());
                        model.changed_at.insert(id, receipt.committed().raw());
                        model.edits.push(ReferenceEdit {
                            root: edit_intent.txn(),
                            owner: edit_intent.actor(),
                            before: BTreeMap::from([(id, old.clone())]),
                            after: BTreeMap::from([(id, next.clone())]),
                            inverse_base: receipt.committed().raw(),
                            undone: None,
                            spans: BTreeMap::from([(id, (old.len(), next.len()))]),
                        });
                        let expected = CheckpointOutcome {
                            receipt: Some(receipt),
                            buffer: None,
                        };
                        assert_eq!(actual.commit_proposal(request), Ok(expected.clone()));
                        assert_eq!(actual.commit_proposal(request), Ok(expected));
                        model.check(&actual);
                        continue;
                    }
                    if choice % 16 == 1 && !model.contents[&id].is_empty() {
                        coverage[2] += 1;
                        coverage[10 + actor.raw() as usize] += 1;
                        let snapshot = actual.snapshot();
                        let view = BufferView::full(snapshot.catalog(), id).unwrap();
                        let text = snapshot.catalog().get(id).unwrap();
                        let selection = Selection::new(
                            snapshot.catalog(),
                            &view,
                            id,
                            text.validate_offset(0).unwrap(),
                            text.validate_offset(1).unwrap(),
                        )
                        .unwrap();
                        let name = RegisterName::new(if both { "a" } else { "b" }).unwrap();
                        let edit_intent = intent(actor.raw(), nonce, model.revision);
                        let operation = Operation::Cut {
                            intent: edit_intent,
                            view,
                            selections: vec![selection],
                            name: name.clone(),
                            kind: RegisterKind::Charwise,
                        };
                        if compact {
                            actual.access_mut().revoke_default(Capability::Read);
                            model.defaults.remove(&Capability::Read);
                            actual
                                .access_mut()
                                .grant_buffer(actor, id, Capability::Read);
                            model.grants.insert((actor, id, Capability::Read));
                            model.check(&actual);
                            actual
                                .access_mut()
                                .revoke_buffer(actor, id, Capability::Read);
                            model.grants.remove(&(actor, id, Capability::Read));
                            let before = live_state!();
                            assert_eq!(
                                actual.apply(request, operation.clone()),
                                Err(CoordinatorError::Access(AccessError::Denied {
                                    actor,
                                    buffer: id,
                                    capability: Capability::Read
                                }))
                            );
                            assert_eq!(live_state!(), before);
                            model.check(&actual);
                            actual.access_mut().grant_default(Capability::Read);
                            model.defaults.insert(Capability::Read);
                        }
                        let old = model.contents[&id].clone();
                        let entry = RegisterEntry {
                            text: Text::from_str(&old[..1]),
                            kind: RegisterKind::Charwise,
                        };
                        model.registers.insert((actor, name), entry.clone());
                        model.rings.entry(actor).or_default().insert(0, entry);
                        model.contents.insert(id, old[1..].to_owned());
                        let receipt = model.commit(request, edit_intent, true);
                        model
                            .steps
                            .push((receipt.committed().raw(), id, old.len(), 0, 1));
                        model.changed_at.insert(id, receipt.committed().raw());
                        model.edits.push(ReferenceEdit {
                            root: edit_intent.txn(),
                            owner: actor,
                            before: BTreeMap::from([(id, old.clone())]),
                            after: BTreeMap::from([(id, old[1..].to_owned())]),
                            inverse_base: receipt.committed().raw(),
                            undone: None,
                            spans: BTreeMap::from([(id, (1, 0))]),
                        });
                        let expected = CheckpointOutcome {
                            receipt: Some(receipt),
                            buffer: None,
                        };
                        assert_eq!(
                            actual.apply(request, operation.clone()),
                            Ok(expected.clone())
                        );
                        model
                            .issued
                            .push((request, operation.clone(), expected.clone()));
                        assert_eq!(actual.apply(request, operation), Ok(expected));
                        model.check(&actual);
                        actual = Coordinator::restore(&actual.checkpoint()).unwrap();
                        model.check(&actual);
                        continue;
                    }
                    if choice % 16 == 3 {
                        coverage[3] += 1;
                        let name = RegisterName::new(if both { "a" } else { "b" }).unwrap();
                        let snapshot = actual.snapshot();
                        let view = BufferView::full(snapshot.catalog(), id).unwrap();
                        let text = snapshot.catalog().get(id).unwrap();
                        let zero = text.validate_offset(0).unwrap();
                        let caret =
                            Selection::new(snapshot.catalog(), &view, id, zero, zero).unwrap();
                        let edit_intent = intent(actor.raw(), nonce, model.revision);
                        let before = live_state!();
                        let paste = register_edit::paste(
                            actual.registers(),
                            edit_intent,
                            &view,
                            &snapshot,
                            &[caret],
                            &name,
                        );
                        if let Some(entry) = model.registers.get(&(actor, name)) {
                            coverage[4] += 1;
                            let old = model.contents[&id].clone();
                            let next = format!("{}{}", entry.text.as_str(), old);
                            let inserted_len = entry.text.as_str().len();
                            let operation = Operation::Edit(paste.unwrap());
                            let receipt = model.commit(request, edit_intent, true);
                            model
                                .steps
                                .push((receipt.committed().raw(), id, old.len(), 0, 0));
                            model.contents.insert(id, next.clone());
                            model.changed_at.insert(id, receipt.committed().raw());
                            model.edits.push(ReferenceEdit {
                                root: edit_intent.txn(),
                                owner: actor,
                                before: BTreeMap::from([(id, old)]),
                                after: BTreeMap::from([(id, next)]),
                                inverse_base: receipt.committed().raw(),
                                undone: None,
                                spans: BTreeMap::from([(id, (0, inserted_len))]),
                            });
                            let expected = CheckpointOutcome {
                                receipt: Some(receipt),
                                buffer: None,
                            };
                            assert_eq!(
                                actual.apply(request, operation.clone()),
                                Ok(expected.clone())
                            );
                            model
                                .issued
                                .push((request, operation.clone(), expected.clone()));
                            assert_eq!(actual.apply(request, operation), Ok(expected));
                        } else {
                            assert_eq!(paste, Err(RegisterEditError::MissingRegister));
                            assert_eq!(live_state!(), before);
                        }
                        model.check(&actual);
                        continue;
                    }
                    if choice % 16 == 4 {
                        coverage[5] += 1;
                        let reviewer = ActorId::new(nonce + 1000);
                        actual
                            .actors_mut()
                            .register(reviewer, ActorRole::Human)
                            .unwrap();
                        model.active.insert(reviewer);
                        let old = model.contents[&id].clone();
                        assert_eq!(
                            actual.propose(Proposal::new(
                                request,
                                edit(1, nonce, model.revision, id, &old, "revoked")
                            )),
                            Ok(())
                        );
                        model.pending.insert(request, None);
                        assert_eq!(actual.approve(request, reviewer), Ok(()));
                        model
                            .pending
                            .insert(request, Some((reviewer, model.revision)));
                        actual.actors_mut().revoke(reviewer).unwrap();
                        model.active.remove(&reviewer);
                        let before = live_state!();
                        assert_eq!(
                            actual.commit_proposal(request),
                            Err(model.proposal_error(request, id).unwrap())
                        );
                        assert_eq!(live_state!(), before);
                        assert!(!actual.workspace().actors().is_active(reviewer));
                        model.check(&actual);
                        continue;
                    }
                    if choice % 16 == 5 {
                        coverage[6] += 1;
                        use crate::editor::edit::{
                            patch::PatchError, splice::SpliceError, transaction::TransactionError,
                        };
                        let len = model.contents[&id].len();
                        let longer = Text::from_str(&"x".repeat(len + 1));
                        let span = ByteSpan::validate_in(&longer, len + 1, len + 1).unwrap();
                        let patch =
                            Patch::new(vec![Splice::new(span, Text::from_str("x"))]).unwrap();
                        let txn =
                            Transaction::new(intent(1, nonce, model.revision), vec![(id, patch)])
                                .unwrap();
                        let before = live_state!();
                        assert_eq!(
                            actual.apply(request, Operation::Edit(txn)),
                            Err(CoordinatorError::Checkpoint(CheckpointError::Retention(
                                super::super::retention::RetentionError::Rebase(
                                    super::super::rebase::RebaseError::Transaction(
                                        TransactionError::Patch {
                                            buffer: id,
                                            error: PatchError::InvalidSplice(
                                                SpliceError::OutOfBounds {
                                                    offset: len + 1,
                                                    len
                                                }
                                            )
                                        }
                                    )
                                )
                            )))
                        );
                        assert_eq!(live_state!(), before);
                        model.check(&actual);
                        continue;
                    }
                    if choice % 16 == 6 {
                        if let Some((revision, target, len, start, end)) = model
                            .steps
                            .iter()
                            .rev()
                            .find(|(revision, _, _, _, _)| *revision > model.floor)
                        {
                            coverage[7] += 1;
                            let target = *target;
                            let base = revision - 1;
                            let text = Text::from_str(&"x".repeat(*len));
                            let span = ByteSpan::validate_in(&text, *start, *end).unwrap();
                            let patch =
                                Patch::new(vec![Splice::new(span, Text::from_str("conflict"))])
                                    .unwrap();
                            let txn =
                                Transaction::new(intent(1, nonce, base), vec![(target, patch)])
                                    .unwrap();
                            let conflict = super::super::footprints::SpanConflict {
                                buffer: target,
                                left: span,
                                right: span,
                            };
                            let before = live_state!();
                            assert_eq!(
                                actual.apply(request, Operation::Edit(txn)),
                                Err(CoordinatorError::Checkpoint(CheckpointError::Retention(
                                    super::super::retention::RetentionError::Rebase(
                                        super::super::rebase::RebaseError::Conflict(conflict)
                                    )
                                )))
                            );
                            assert_eq!(live_state!(), before);
                            model.check(&actual);
                            continue;
                        }
                    }
                    if choice % 16 == 7 && model.floor > 0 {
                        coverage[8] += 1;
                        let base = model.floor - 1;
                        let old = model.contents[&id].clone();
                        let before = live_state!();
                        assert_eq!(
                            actual.apply(
                                request,
                                Operation::Edit(edit(1, nonce, base, id, &old, "expired"))
                            ),
                            Err(CoordinatorError::Checkpoint(CheckpointError::Retention(
                                super::super::retention::RetentionError::BaseExpired {
                                    base: Revision::new(base),
                                    floor: Revision::new(model.floor)
                                }
                            )))
                        );
                        assert_eq!(live_state!(), before);
                        model.check(&actual);
                        continue;
                    }
                    if choice % 16 == 2 {
                        let old = model.contents[&id].clone();
                        let operation = Operation::Edit(edit(
                            actor.raw(),
                            nonce,
                            model.revision,
                            id,
                            &old,
                            "denied",
                        ));
                        actual.access_mut().revoke_default(Capability::Commit);
                        model.defaults.remove(&Capability::Commit);
                        actual
                            .access_mut()
                            .grant_buffer(actor, id, Capability::Commit);
                        model.grants.insert((actor, id, Capability::Commit));
                        model.check(&actual);
                        actual
                            .access_mut()
                            .revoke_buffer(actor, id, Capability::Commit);
                        model.grants.remove(&(actor, id, Capability::Commit));
                        let before = live_state!();
                        assert_eq!(
                            actual.apply(request, operation),
                            Err(CoordinatorError::Access(AccessError::Denied {
                                actor,
                                buffer: id,
                                capability: Capability::Commit
                            }))
                        );
                        assert_eq!(live_state!(), before);
                        model.check(&actual);
                        actual.access_mut().grant_default(Capability::Commit);
                        model.defaults.insert(Capability::Commit);
                        model.check(&actual);
                        continue;
                    }
                    if lifecycle {
                        let edit_intent = intent(1, nonce, model.revision);
                        let extra = model.contents.keys().find(|id| id.raw() >= 2).copied();
                        let (operation, id) = if let Some(id) = extra {
                            model.contents.remove(&id);
                            model.tombstones.insert(id);
                            (
                                Operation::Close {
                                    intent: edit_intent,
                                    buffer: id,
                                    policy: ClosePolicy::Discard,
                                },
                                id,
                            )
                        } else {
                            let next = model
                                .contents
                                .keys()
                                .chain(model.tombstones.iter())
                                .map(|id| id.raw())
                                .max()
                                .unwrap()
                                + 1;
                            let id = BufferId::new(next);
                            let text = format!("extra-{nonce}");
                            model.contents.insert(id, text.clone());
                            (
                                Operation::Open {
                                    intent: edit_intent,
                                    name: text.clone(),
                                    text: Text::from_str(&text),
                                },
                                id,
                            )
                        };
                        let receipt = model.commit(request, edit_intent, false);
                        let expected = CheckpointOutcome {
                            receipt: Some(receipt),
                            buffer: Some(id),
                        };
                        assert_eq!(
                            actual.apply(request, operation.clone()),
                            Ok(expected.clone())
                        );
                        assert_eq!(actual.apply(request, operation), Ok(expected));
                        model.check(&actual);
                        continue;
                    }
                    let candidates: Vec<_> = (0..model.edits.len()).collect();
                    if undo_or_redo && !candidates.is_empty() {
                        let selected = candidates[choice as usize % candidates.len()];
                        let record = model.edits[selected].clone();
                        let transaction = TxnId::new(record.owner, Nonce::new(nonce));
                        let undo = record.undone.is_none();
                        let operation = if undo {
                            Operation::Undo {
                                actor: record.owner,
                                target: record.root,
                                transaction,
                            }
                        } else {
                            Operation::Redo {
                                actor: record.owner,
                                target: record.root,
                                transaction,
                            }
                        };
                        let before = live_state!();
                        if record.inverse_base <= model.floor {
                            if !undo {
                                expired_redo_hits += 1;
                            }
                            assert_eq!(
                                actual.apply(request, operation),
                                Err(CoordinatorError::Checkpoint(CheckpointError::Retention(
                                    super::super::retention::RetentionError::BaseExpired {
                                        base: Revision::new(record.inverse_base),
                                        floor: Revision::new(model.floor),
                                    }
                                )))
                            );
                            assert_eq!(live_state!(), before);
                        } else if let Some((_, id, len, start, end)) =
                            model.steps.iter().find(|(revision, id, _, _, _)| {
                                *revision > record.inverse_base && record.before.contains_key(id)
                            })
                        {
                            coverage[9] += 1;
                            if !undo {
                                conflicting_redo_hits += 1;
                            }
                            let left_text = if undo {
                                &record.after[id]
                            } else {
                                &record.before[id]
                            };
                            let (before_len, after_len) = record.spans[id];
                            let right_text = Text::from_str(&"x".repeat(*len));
                            let conflict = super::super::footprints::SpanConflict {
                                buffer: *id,
                                left: ByteSpan::validate_in(
                                    &Text::from_str(left_text),
                                    0,
                                    if undo { after_len } else { before_len },
                                )
                                .unwrap(),
                                right: ByteSpan::validate_in(&right_text, *start, *end).unwrap(),
                            };
                            assert_eq!(
                                actual.apply(request, operation),
                                Err(CoordinatorError::Checkpoint(CheckpointError::Retention(
                                    super::super::retention::RetentionError::Rebase(
                                        super::super::rebase::RebaseError::Conflict(conflict)
                                    )
                                )))
                            );
                            assert_eq!(live_state!(), before);
                        } else {
                            coverage[10] += 1;
                            let edit_intent =
                                EditIntent::new(transaction, Revision::new(record.inverse_base));
                            let receipt = model.commit(request, edit_intent, true);
                            for id in record.before.keys() {
                                let len = model.contents[id].len();
                                let (before_len, after_len) = record.spans[id];
                                model.steps.push((
                                    receipt.committed().raw(),
                                    *id,
                                    len,
                                    0,
                                    if undo { after_len } else { before_len },
                                ));
                            }
                            for (id, text) in if undo { &record.before } else { &record.after } {
                                model.contents.insert(*id, text.clone());
                                model.changed_at.insert(*id, receipt.committed().raw());
                            }
                            let saved = &mut model.edits[selected];
                            saved.inverse_base = receipt.committed().raw();
                            saved.undone = undo.then_some(transaction);
                            let expected = CheckpointOutcome {
                                receipt: Some(receipt),
                                buffer: None,
                            };
                            assert_eq!(
                                actual.apply(request, operation.clone()),
                                Ok(expected.clone())
                            );
                            assert_eq!(actual.apply(request, operation), Ok(expected));
                            if undo
                                && record.before.len() == 1
                                && (conflicting_redo_hits == 0 || expired_redo_hits == 0)
                            {
                                let id = *record.before.keys().next().unwrap();
                                let old = model.contents[&id].clone();
                                if !old.is_empty() && record.spans[&id].0 > 0 {
                                    let redo_nonce = 20_000 + nonce * 2;
                                    if conflicting_redo_hits == 0 {
                                        let next = format!("overlap-{nonce}");
                                        let advance_intent = intent(1, redo_nonce, model.revision);
                                        let advance_request = RequestId::new(redo_nonce);
                                        let advance = Operation::Edit(edit(
                                            1,
                                            redo_nonce,
                                            model.revision,
                                            id,
                                            &old,
                                            &next,
                                        ));
                                        let advance_receipt =
                                            model.commit(advance_request, advance_intent, true);
                                        model.steps.push((
                                            advance_receipt.committed().raw(),
                                            id,
                                            old.len(),
                                            0,
                                            old.len(),
                                        ));
                                        model
                                            .changed_at
                                            .insert(id, advance_receipt.committed().raw());
                                        model.contents.insert(id, next.clone());
                                        model.edits.push(ReferenceEdit {
                                            root: advance_intent.txn(),
                                            owner: advance_intent.actor(),
                                            before: BTreeMap::from([(id, old.clone())]),
                                            after: BTreeMap::from([(id, next.clone())]),
                                            inverse_base: advance_receipt.committed().raw(),
                                            undone: None,
                                            spans: BTreeMap::from([(id, (old.len(), next.len()))]),
                                        });
                                        assert!(actual.apply(advance_request, advance).is_ok());
                                        let conflict = super::super::footprints::SpanConflict {
                                            buffer: id,
                                            left: ByteSpan::validate_in(
                                                &Text::from_str(&old),
                                                0,
                                                record.spans[&id].0,
                                            )
                                            .unwrap(),
                                            right: ByteSpan::full_of(&Text::from_str(&old)),
                                        };
                                        let before = live_state!();
                                        assert_eq!(
                                            actual.apply(
                                                RequestId::new(redo_nonce + 1),
                                                Operation::Redo {
                                                    actor: record.owner,
                                                    target: record.root,
                                                    transaction: TxnId::new(
                                                        record.owner,
                                                        Nonce::new(redo_nonce + 1)
                                                    ),
                                                }
                                            ),
                                            Err(CoordinatorError::Checkpoint(
                                                CheckpointError::Retention(
                                                    super::super::retention::RetentionError::Rebase(
                                                        super::super::rebase::RebaseError::Conflict(
                                                            conflict
                                                        )
                                                    )
                                                )
                                            ))
                                        );
                                        assert_eq!(live_state!(), before);
                                        conflicting_redo_hits += 1;
                                    } else {
                                        actual.compact(Revision::new(model.revision)).unwrap();
                                        model.floor = model.revision;
                                        let before = live_state!();
                                        assert_eq!(actual.apply(RequestId::new(redo_nonce + 1), Operation::Redo {
                                            actor: record.owner, target: record.root,
                                            transaction: TxnId::new(record.owner, Nonce::new(redo_nonce + 1)),
                                        }), Err(CoordinatorError::Checkpoint(CheckpointError::Retention(
                                            super::super::retention::RetentionError::BaseExpired {
                                                base: receipt.committed(), floor: Revision::new(model.floor),
                                            }))));
                                        assert_eq!(live_state!(), before);
                                        expired_redo_hits += 1;
                                    }
                                }
                            }
                        }
                        model.check(&actual);
                        continue;
                    }
                    let ids = if both {
                        vec![BufferId::new(0), BufferId::new(1)]
                    } else {
                        vec![BufferId::new((index % 2) as u64)]
                    };
                    let base = if !both {
                        model
                            .floor
                            .max(*model.changed_at.get(&ids[0]).unwrap_or(&2))
                    } else {
                        model.revision
                    };
                    if base < model.revision {
                        coverage[13] += 1;
                    }
                    let edit_intent = intent(1, nonce, base);
                    let before: BTreeMap<_, _> = ids
                        .iter()
                        .map(|id| (*id, model.contents[id].clone()))
                        .collect();
                    let patches = ids
                        .iter()
                        .map(|id| {
                            let next = format!("value-{index}-{}", id.raw());
                            let txn = edit(1, nonce, base, *id, &model.contents[id], &next);
                            model.contents.insert(*id, next);
                            txn.patches()[0].clone()
                        })
                        .collect();
                    let operation =
                        Operation::Edit(Transaction::new(edit_intent, patches).unwrap());
                    let receipt = model.commit(request, edit_intent, true);
                    for id in &ids {
                        let len = before[id].len();
                        model
                            .steps
                            .push((receipt.committed().raw(), *id, len, 0, len));
                        model.changed_at.insert(*id, receipt.committed().raw());
                    }
                    model.edits.push(ReferenceEdit {
                        root: edit_intent.txn(),
                        owner: edit_intent.actor(),
                        before: before.clone(),
                        after: ids
                            .iter()
                            .map(|id| (*id, model.contents[id].clone()))
                            .collect(),
                        inverse_base: receipt.committed().raw(),
                        undone: None,
                        spans: ids
                            .iter()
                            .map(|id| (*id, (before[id].len(), model.contents[id].len())))
                            .collect(),
                    });
                    let expected = CheckpointOutcome {
                        receipt: Some(receipt),
                        buffer: None,
                    };
                    assert_eq!(
                        actual.apply(request, operation.clone()),
                        Ok(expected.clone())
                    );
                    model
                        .issued
                        .push((request, operation.clone(), expected.clone()));
                    model.check(&actual);
                    assert_eq!(actual.apply(request, operation), Ok(expected));
                    model.check(&actual);
                    let before = live_state!();
                    let conflict = Operation::Open {
                        intent: edit_intent,
                        name: "conflict".into(),
                        text: Text::empty(),
                    };
                    assert_eq!(
                        actual.apply(request, conflict),
                        Err(CoordinatorError::RequestReuse(request))
                    );
                    assert_eq!(live_state!(), before);
                    model.check(&actual);
                    if compact {
                        actual.compact(Revision::new(model.revision)).unwrap();
                        model.floor = model.revision;
                        model.check(&actual);
                        actual = Coordinator::restore(&actual.checkpoint()).unwrap();
                        model.check(&actual);
                    }
                }

                // All decisions below use only the model's strings, revision, and receipts.
                // The snapshot supplies validated positions, never expected results.
                macro_rules! reject {
                    ($expression:expr, $error:expr) => {{
                        let before = live_state!();
                        let snapshot = actual.snapshot();
                        let floor = actual.workspace().history().floor();
                        let tip = actual.workspace().history().tip();
                        let replay = actual.workspace().history().replay(floor);
                        let delivery = actual.workspace().delivery().events().to_vec();
                        let registers = actual.registers().clone();
                        let rings = actual.rings().clone();
                        assert_eq!($expression, Err($error));
                        assert_eq!(live_state!(), before);
                        assert_eq!(actual.snapshot(), snapshot);
                        assert_eq!(actual.workspace().history().floor(), floor);
                        assert_eq!(actual.workspace().history().tip(), tip);
                        assert_eq!(actual.workspace().history().replay(floor), replay);
                        assert_eq!(actual.workspace().delivery().events(), delivery);
                        assert_eq!(actual.registers(), &registers);
                        assert_eq!(actual.rings(), &rings);
                        model.check(&actual);
                    }};
                }
                macro_rules! publish {
                    ($request:expr, $intent:expr, $operation:expr, $buffer:expr, $edit:expr) => {{
                        let request = RequestId::new($request);
                        let op = $operation;
                        let receipt = model.commit(request, $intent, $edit);
                        let expected = CheckpointOutcome {
                            receipt: Some(receipt),
                            buffer: $buffer,
                        };
                        assert_eq!(actual.apply(request, op.clone()), Ok(expected.clone()));
                        model.check(&actual);
                        assert_eq!(actual.apply(request, op), Ok(expected));
                        model.check(&actual);
                    }};
                }

                let a = BufferId::new(0);
                let b = BufferId::new(1);
                let actor = ActorId::new(1);
                let other = ActorId::new(2);
                let old = model.contents[&a].clone();
                let proposal_intent = intent(2, 100, model.revision);
                let proposed = edit(2, 100, model.revision, a, &old, "proposed");
                let request = RequestId::new(100);
                model.pending.insert(request, None);
                assert_eq!(actual.propose(Proposal::new(request, proposed)), Ok(()));
                model.check(&actual);
                reject!(
                    actual.commit_proposal(request),
                    model.proposal_error(request, a).unwrap()
                );
                model.pending.insert(request, Some((actor, model.revision)));
                assert_eq!(actual.approve(request, actor), Ok(()));
                model.check(&actual);
                assert_eq!(model.proposal_error(request, a), None);
                model.pending.remove(&request);
                let receipt = model.commit(request, proposal_intent, true);
                model.contents.insert(a, "proposed".into());
                let outcome = CheckpointOutcome {
                    receipt: Some(receipt),
                    buffer: None,
                };
                assert_eq!(actual.commit_proposal(request), Ok(outcome.clone()));
                model.check(&actual);
                assert_eq!(actual.commit_proposal(request), Ok(outcome));
                model.check(&actual);

                let target = proposal_intent.txn();
                reject!(
                    actual.apply(
                        RequestId::new(101),
                        Operation::Redo {
                            actor: other,
                            target,
                            transaction: TxnId::new(other, Nonce::new(101))
                        }
                    ),
                    CoordinatorError::NotUndone(target)
                );
                let close_intent = intent(1, 102, model.revision);
                model.contents.remove(&b);
                model.tombstones.insert(b);
                publish!(
                    102,
                    close_intent,
                    Operation::Close {
                        intent: close_intent,
                        buffer: b,
                        policy: ClosePolicy::Discard
                    },
                    Some(b),
                    false
                );
                reject!(
                    actual.apply(
                        RequestId::new(103),
                        Operation::Close {
                            intent: intent(1, 103, model.revision),
                            buffer: b,
                            policy: ClosePolicy::Discard
                        }
                    ),
                    CoordinatorError::Access(AccessError::BufferClosed(b))
                );
                reject!(
                    actual.apply(
                        RequestId::new(104),
                        Operation::Edit(edit(1, 104, model.revision, b, "", "x"))
                    ),
                    CoordinatorError::Access(AccessError::BufferClosed(b))
                );
                let inverse_base = receipt.committed().raw();
                let undo_intent = intent(2, 105, inverse_base);
                model.contents.insert(a, old.clone());
                publish!(
                    105,
                    undo_intent,
                    Operation::Undo {
                        actor: other,
                        target,
                        transaction: undo_intent.txn()
                    },
                    None,
                    true
                );
                reject!(
                    actual.apply(
                        RequestId::new(106),
                        Operation::Undo {
                            actor: other,
                            target,
                            transaction: TxnId::new(other, Nonce::new(106))
                        }
                    ),
                    CoordinatorError::AlreadyUndone(target)
                );
                let c = BufferId::new(
                    model
                        .contents
                        .keys()
                        .chain(model.tombstones.iter())
                        .map(|id| id.raw())
                        .max()
                        .unwrap()
                        + 1,
                );
                let open_intent = intent(1, 107, model.revision);
                model.contents.insert(c, "unrelated".into());
                publish!(
                    107,
                    open_intent,
                    Operation::Open {
                        intent: open_intent,
                        name: "c".into(),
                        text: Text::from_str("unrelated")
                    },
                    Some(c),
                    false
                );
                let redo_intent = intent(2, 108, inverse_base + 2);
                model.contents.insert(a, "proposed".into());
                publish!(
                    108,
                    redo_intent,
                    Operation::Redo {
                        actor: other,
                        target,
                        transaction: redo_intent.txn()
                    },
                    None,
                    true
                );
                let floor = model.revision;
                actual.compact(Revision::new(floor)).unwrap();
                model.check(&actual);
                reject!(
                    actual.apply(
                        RequestId::new(109),
                        Operation::Undo {
                            actor: other,
                            target,
                            transaction: TxnId::new(other, Nonce::new(109))
                        }
                    ),
                    CoordinatorError::Checkpoint(CheckpointError::Retention(
                        super::super::retention::RetentionError::BaseExpired {
                            base: Revision::new(floor),
                            floor: Revision::new(floor)
                        }
                    ))
                );

                let pending = RequestId::new(110);
                model.pending.insert(pending, None);
                assert_eq!(
                    actual.propose(Proposal::new(
                        pending,
                        edit(2, 110, model.revision, a, "proposed", "stale")
                    )),
                    Ok(())
                );
                model.check(&actual);
                model.pending.insert(pending, Some((actor, model.revision)));
                assert_eq!(actual.approve(pending, actor), Ok(()));
                model.check(&actual);
                let advance = intent(1, 111, model.revision);
                model.contents.insert(c, "advanced".into());
                publish!(
                    111,
                    advance,
                    Operation::Edit(edit(
                        1,
                        111,
                        advance.base().raw(),
                        c,
                        "unrelated",
                        "advanced"
                    )),
                    None,
                    true
                );
                reject!(
                    actual.commit_proposal(pending),
                    model.proposal_error(pending, a).unwrap()
                );

                let pending = RequestId::new(112);
                model.pending.insert(pending, None);
                assert_eq!(
                    actual.propose(Proposal::new(
                        pending,
                        edit(2, 112, model.revision, a, "proposed", "revoked")
                    )),
                    Ok(())
                );
                model.check(&actual);
                actual.access_mut().revoke_default(Capability::Review);
                model.defaults.remove(&Capability::Review);
                actual
                    .access_mut()
                    .grant_buffer(actor, a, Capability::Review);
                model.grants.insert((actor, a, Capability::Review));
                model.check(&actual);
                model.pending.insert(pending, Some((actor, model.revision)));
                assert_eq!(actual.approve(pending, actor), Ok(()));
                model.check(&actual);
                actual
                    .access_mut()
                    .revoke_buffer(actor, a, Capability::Review);
                model.grants.remove(&(actor, a, Capability::Review));
                model.check(&actual);
                reject!(
                    actual.commit_proposal(pending),
                    model.proposal_error(pending, a).unwrap()
                );
                actual
                    .access_mut()
                    .grant_buffer(actor, a, Capability::Review);
                model.grants.insert((actor, a, Capability::Review));
                model.check(&actual);

                let snapshot = actual.snapshot();
                let view = BufferView::full(snapshot.catalog(), c).unwrap();
                let text = snapshot.catalog().get(c).unwrap();
                let selection = Selection::new(
                    snapshot.catalog(),
                    &view,
                    c,
                    text.validate_offset(0).unwrap(),
                    text.validate_offset(1).unwrap(),
                )
                .unwrap();
                let name = RegisterName::new("a").unwrap();
                let first_cut_intent = intent(1, 116, model.revision);
                let entry = RegisterEntry {
                    text: Text::from_str("a"),
                    kind: RegisterKind::Charwise,
                };
                model.registers.insert((actor, name.clone()), entry.clone());
                model.rings.entry(actor).or_default().insert(0, entry);
                model.contents.insert(c, "dvanced".into());
                publish!(
                    116,
                    first_cut_intent,
                    Operation::Cut {
                        intent: first_cut_intent,
                        view,
                        selections: vec![selection],
                        name,
                        kind: RegisterKind::Charwise
                    },
                    None,
                    true
                );
                // A new proposal is needed because the preceding cut changed the revision.
                let pending = RequestId::new(117);
                model.pending.insert(pending, None);
                assert_eq!(
                    actual.propose(Proposal::new(
                        pending,
                        edit(2, 117, model.revision, a, "proposed", "revoked")
                    )),
                    Ok(())
                );
                model.check(&actual);
                model.pending.insert(pending, Some((actor, model.revision)));
                assert_eq!(actual.approve(pending, actor), Ok(()));
                model.check(&actual);
                actual.actors_mut().revoke(actor).unwrap();
                model.active.remove(&actor);
                model.check(&actual);
                reject!(
                    actual.commit_proposal(pending),
                    model.proposal_error(pending, a).unwrap()
                );
                reject!(
                    actual.apply(
                        RequestId::new(113),
                        Operation::Edit(edit(1, 113, model.revision, a, "proposed", "denied"))
                    ),
                    CoordinatorError::Access(AccessError::InactiveActor(actor))
                );

                let snapshot = actual.snapshot();
                let view = BufferView::full(snapshot.catalog(), a).unwrap();
                let text = snapshot.catalog().get(a).unwrap();
                let selection = Selection::new(
                    snapshot.catalog(),
                    &view,
                    a,
                    text.validate_offset(0).unwrap(),
                    text.validate_offset(1).unwrap(),
                )
                .unwrap();
                let name = RegisterName::new("a").unwrap();
                let cut_intent = intent(2, 114, model.revision);
                let cut = Operation::Cut {
                    intent: cut_intent,
                    view,
                    selections: vec![selection],
                    name: name.clone(),
                    kind: RegisterKind::Charwise,
                };
                actual.access_mut().revoke_default(Capability::Read);
                model.defaults.remove(&Capability::Read);
                model.check(&actual);
                reject!(
                    actual.apply(RequestId::new(114), cut.clone()),
                    CoordinatorError::Access(AccessError::Denied {
                        actor: other,
                        buffer: a,
                        capability: Capability::Read
                    })
                );
                actual.access_mut().grant_buffer(other, a, Capability::Read);
                model.grants.insert((other, a, Capability::Read));
                model.check(&actual);
                actual.access_mut().revoke_default(Capability::Commit);
                model.defaults.remove(&Capability::Commit);
                model.check(&actual);
                reject!(
                    actual.apply(RequestId::new(114), cut.clone()),
                    CoordinatorError::Access(AccessError::Denied {
                        actor: other,
                        buffer: a,
                        capability: Capability::Commit
                    })
                );
                actual
                    .access_mut()
                    .grant_buffer(other, a, Capability::Commit);
                model.grants.insert((other, a, Capability::Commit));
                model.check(&actual);
                let entry = RegisterEntry {
                    text: Text::from_str("p"),
                    kind: RegisterKind::Charwise,
                };
                model.registers.insert((other, name.clone()), entry.clone());
                model.rings.entry(other).or_default().insert(0, entry);
                model.contents.insert(a, "roposed".into());
                publish!(114, cut_intent, cut.clone(), None, true);
                let cut_receipt = model.receipts[&RequestId::new(114)].0;
                let registers = actual.registers().clone();
                let rings = actual.rings().clone();
                actual.compact(Revision::new(model.revision)).unwrap();
                model.floor = model.revision;
                actual = Coordinator::restore(&actual.checkpoint()).unwrap();
                assert_eq!(
                    actual.apply(RequestId::new(114), cut),
                    Ok(CheckpointOutcome {
                        receipt: Some(cut_receipt),
                        buffer: None
                    })
                );
                assert_eq!(actual.registers(), &registers);
                assert_eq!(actual.rings(), &rings);
                model.check(&actual);

                let snapshot = actual.snapshot();
                let view = BufferView::full(snapshot.catalog(), a).unwrap();
                let text = snapshot.catalog().get(a).unwrap();
                let caret = Selection::new(
                    snapshot.catalog(),
                    &view,
                    a,
                    text.validate_offset(0).unwrap(),
                    text.validate_offset(0).unwrap(),
                )
                .unwrap();
                let paste_intent = intent(2, 115, model.revision);
                let before = live_state!();
                assert_eq!(
                    register_edit::paste(
                        actual.registers(),
                        paste_intent,
                        &view,
                        &snapshot,
                        &[caret],
                        &RegisterName::new("b").unwrap()
                    ),
                    Err(RegisterEditError::MissingRegister)
                );
                assert_eq!(live_state!(), before);
                model.check(&actual);
                let paste = register_edit::paste(
                    actual.registers(),
                    paste_intent,
                    &view,
                    &snapshot,
                    &[caret],
                    &name,
                )
                .unwrap();
                actual
                    .access_mut()
                    .revoke_buffer(other, a, Capability::Commit);
                model.grants.remove(&(other, a, Capability::Commit));
                model.check(&actual);
                reject!(
                    actual.apply(RequestId::new(115), Operation::Edit(paste.clone())),
                    CoordinatorError::Access(AccessError::Denied {
                        actor: other,
                        buffer: a,
                        capability: Capability::Commit
                    })
                );
                actual
                    .access_mut()
                    .grant_buffer(other, a, Capability::Commit);
                model.grants.insert((other, a, Capability::Commit));
                model.check(&actual);
                model.contents.insert(a, "proposed".into());
                publish!(115, paste_intent, Operation::Edit(paste), None, true);

                let undo_intent = intent(2, 118, model.revision);
                model.contents.insert(a, "roposed".into());
                publish!(
                    118,
                    undo_intent,
                    Operation::Undo {
                        actor: other,
                        target: paste_intent.txn(),
                        transaction: undo_intent.txn()
                    },
                    None,
                    true
                );
                let floor = model.revision;
                actual.compact(Revision::new(floor)).unwrap();
                model.check(&actual);
                reject!(
                    actual.apply(
                        RequestId::new(119),
                        Operation::Redo {
                            actor: other,
                            target: paste_intent.txn(),
                            transaction: TxnId::new(other, Nonce::new(119))
                        }
                    ),
                    CoordinatorError::Checkpoint(CheckpointError::Retention(
                        super::super::retention::RetentionError::BaseExpired {
                            base: Revision::new(floor),
                            floor: Revision::new(floor)
                        }
                    ))
                );
            },
        );
        assert!(
            coverage.iter().all(|count| *count > 0),
            "coverage: {coverage:?}"
        );
        assert!(conflicting_redo_hits > 0, "conflicting Redo not exercised");
        assert!(expired_redo_hits > 0, "expired Redo not exercised");
        eprintln!("coverage: {coverage:?}; conflicting_redo_hits: {conflicting_redo_hits}; expired_redo_hits: {expired_redo_hits}");
    }

    #[test]
    fn proposal_invalidation_and_history_round_trip() {
        assert_law(
            &[0, 7, 42, u64::MAX],
            |rng| rng.gen_bool(),
            |choice| {
                let mut s = state();
                let open = Operation::Open {
                    intent: intent(1, 1, 0),
                    name: "a".into(),
                    text: Text::from_str("a"),
                };
                let id = s.apply(RequestId::new(1), open).unwrap().buffer.unwrap();
                let proposed = edit(2, 2, 1, id, "a", "b");
                s.propose(Proposal::new(RequestId::new(2), proposed))
                    .unwrap();
                s.approve(RequestId::new(2), ActorId::new(1)).unwrap();
                if *choice {
                    let advance = edit(1, 3, 1, id, "a", "c");
                    s.apply(RequestId::new(3), Operation::Edit(advance))
                        .unwrap();
                    let before = s.checkpoint();
                    assert_eq!(
                        s.commit_proposal(RequestId::new(2)),
                        Err(CoordinatorError::StaleApproval(RequestId::new(2)))
                    );
                    assert_eq!(s.checkpoint(), before);
                } else {
                    let outcome = s.commit_proposal(RequestId::new(2)).unwrap();
                    assert_eq!(s.commit_proposal(RequestId::new(2)), Ok(outcome));
                    let target = TxnId::new(ActorId::new(2), Nonce::new(2));
                    let undo = Operation::Undo {
                        actor: ActorId::new(2),
                        target,
                        transaction: TxnId::new(ActorId::new(2), Nonce::new(4)),
                    };
                    s.apply(RequestId::new(4), undo).unwrap();
                    assert_eq!(s.snapshot().catalog().get(id).unwrap().as_str(), "a");
                    let redo = Operation::Redo {
                        actor: ActorId::new(2),
                        target,
                        transaction: TxnId::new(ActorId::new(2), Nonce::new(5)),
                    };
                    s.apply(RequestId::new(5), redo).unwrap();
                    assert_eq!(s.snapshot().catalog().get(id).unwrap().as_str(), "b");
                    let restored = Coordinator::restore(&s.checkpoint()).unwrap();
                    assert_eq!(restored.snapshot().catalog(), s.snapshot().catalog());
                }
            },
        );
    }

    #[test]
    fn proposal_reserves_request_and_repeated_undo_targets_latest_redo() {
        let mut s = state();
        let id = s
            .apply(
                RequestId::new(1),
                Operation::Open {
                    intent: intent(1, 1, 0),
                    name: "a".into(),
                    text: Text::from_str("a"),
                },
            )
            .unwrap()
            .buffer
            .unwrap();
        let proposed = edit(2, 2, 1, id, "a", "b");
        s.propose(Proposal::new(RequestId::new(2), proposed))
            .unwrap();
        let before = s.checkpoint();
        assert_eq!(
            s.apply(
                RequestId::new(2),
                Operation::Edit(edit(1, 3, 1, id, "a", "c"))
            ),
            Err(CoordinatorError::RequestReuse(RequestId::new(2)))
        );
        assert_eq!(
            s.commit_proposal(RequestId::new(2)),
            Err(CoordinatorError::Unapproved(RequestId::new(2)))
        );
        assert_eq!(s.checkpoint(), before);
        s.approve(RequestId::new(2), ActorId::new(1)).unwrap();
        s.commit_proposal(RequestId::new(2)).unwrap();
        let committed = s.checkpoint();
        assert_eq!(s.approve(RequestId::new(2), ActorId::new(1)), Ok(()));
        assert_eq!(
            s.approve(RequestId::new(2), ActorId::new(2)),
            Err(CoordinatorError::Proposal(ProposalError::ApprovalMismatch))
        );
        assert_eq!(s.checkpoint(), committed);
        let mut restored = Coordinator::restore(&committed).unwrap();
        assert_eq!(restored.approve(RequestId::new(2), ActorId::new(1)), Ok(()));
        let actor = ActorId::new(2);
        let target = TxnId::new(actor, Nonce::new(2));
        let compensation = TxnId::new(actor, Nonce::new(4));
        s.apply(
            RequestId::new(4),
            Operation::Undo {
                actor,
                target,
                transaction: compensation,
            },
        )
        .unwrap();
        s.apply(
            RequestId::new(5),
            Operation::Redo {
                actor,
                target: compensation,
                transaction: TxnId::new(actor, Nonce::new(5)),
            },
        )
        .unwrap();
        s.apply(
            RequestId::new(6),
            Operation::Undo {
                actor,
                target,
                transaction: TxnId::new(actor, Nonce::new(6)),
            },
        )
        .unwrap();
        assert_eq!(s.snapshot().catalog().get(id).unwrap().as_str(), "a");
        s.apply(
            RequestId::new(7),
            Operation::Redo {
                actor,
                target: TxnId::new(actor, Nonce::new(5)),
                transaction: TxnId::new(actor, Nonce::new(7)),
            },
        )
        .unwrap();
        assert_eq!(s.snapshot().catalog().get(id).unwrap().as_str(), "b");
    }

    #[test]
    fn cut_requires_read_and_leaves_all_state_unchanged_on_denial() {
        let mut s = state();
        let id = s
            .apply(
                RequestId::new(1),
                Operation::Open {
                    intent: intent(1, 1, 0),
                    name: "a".into(),
                    text: Text::from_str("secret"),
                },
            )
            .unwrap()
            .buffer
            .unwrap();
        let snapshot = s.snapshot();
        let view = BufferView::full(snapshot.catalog(), id).unwrap();
        let text = snapshot.catalog().get(id).unwrap();
        let selection = Selection::new(
            snapshot.catalog(),
            &view,
            id,
            text.validate_offset(0).unwrap(),
            text.validate_offset(1).unwrap(),
        )
        .unwrap();
        s.access_mut().revoke_default(Capability::Read);
        let before = s.checkpoint();
        let cut = Operation::Cut {
            intent: intent(2, 2, 1),
            view,
            selections: vec![selection],
            name: RegisterName::new("a").unwrap(),
            kind: RegisterKind::Charwise,
        };
        assert_eq!(
            s.apply(RequestId::new(2), cut),
            Err(CoordinatorError::Access(AccessError::Denied {
                actor: ActorId::new(2),
                buffer: id,
                capability: Capability::Read
            }))
        );
        assert_eq!(s.checkpoint(), before);
    }

    #[test]
    fn repeated_approval_does_not_replace_a_later_reviewer() {
        let mut s = state();
        s.actors_mut()
            .register(ActorId::new(3), ActorRole::Human)
            .unwrap();
        s.access_mut().revoke_default(Capability::Review);
        s.access_mut()
            .grant_actor(ActorId::new(1), Capability::Review);
        s.access_mut()
            .grant_actor(ActorId::new(3), Capability::Review);
        let id = s
            .apply(
                RequestId::new(1),
                Operation::Open {
                    intent: intent(1, 1, 0),
                    name: "a".into(),
                    text: Text::from_str("a"),
                },
            )
            .unwrap()
            .buffer
            .unwrap();
        s.propose(Proposal::new(
            RequestId::new(2),
            edit(2, 2, 1, id, "a", "b"),
        ))
        .unwrap();
        s.approve(RequestId::new(2), ActorId::new(1)).unwrap();
        s.approve(RequestId::new(2), ActorId::new(3)).unwrap();
        let before = s.checkpoint();
        s.approve(RequestId::new(2), ActorId::new(1)).unwrap();
        assert_eq!(s.checkpoint(), before);
        s.access_mut()
            .revoke_actor(ActorId::new(1), Capability::Review);
        s.commit_proposal(RequestId::new(2)).unwrap();
        assert_eq!(s.snapshot().catalog().get(id).unwrap().as_str(), "b");
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoordinatorError {
    Access(AccessError),
    Checkpoint(CheckpointError),
    Proposal(ProposalError),
    Register(RegisterEditError),
    RequestReuse(RequestId),
    TransactionReuse(TxnId),
    MissingProposal(RequestId),
    Unapproved(RequestId),
    StaleApproval(RequestId),
    RevisionMismatch { base: Revision, current: Revision },
    UnknownTarget(TxnId),
    NotOwner(TxnId),
    InvalidTransaction(TxnId),
    AlreadyUndone(TxnId),
    NotUndone(TxnId),
    NoChange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EditRecord {
    inverse: Option<Transaction>,
    inverse_base: Revision,
    owner: ActorId,
    undone: Option<TxnId>,
    root: TxnId,
    compensation: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Pending {
    proposal: Proposal,
    approval: Option<Approval>,
    reviewers: BTreeSet<ActorId>,
}

/// A complete, restorable coordinator image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoordinatorCheckpoint {
    workspace: Checkpoint,
    access: AccessPolicy,
    pending: BTreeMap<RequestId, Pending>,
    approved_commits: BTreeMap<RequestId, (BTreeSet<ActorId>, CheckpointOutcome)>,
    requests: BTreeMap<RequestId, ([u8; 32], CheckpointOutcome)>,
    edits: BTreeMap<TxnId, EditRecord>,
    registers: RegisterStore,
    rings: KillRings,
}

/// Owns the authoritative workspace and all associated admission state.
#[derive(Debug)]
pub struct Coordinator {
    workspace: CheckpointState,
    access: AccessPolicy,
    pending: BTreeMap<RequestId, Pending>,
    approved_commits: BTreeMap<RequestId, (BTreeSet<ActorId>, CheckpointOutcome)>,
    requests: BTreeMap<RequestId, ([u8; 32], CheckpointOutcome)>,
    edits: BTreeMap<TxnId, EditRecord>,
    registers: RegisterStore,
    rings: KillRings,
}

impl Coordinator {
    pub fn new(actors: ActorRegistry, access: AccessPolicy) -> Self {
        Self {
            workspace: CheckpointState::new(actors),
            access,
            pending: BTreeMap::new(),
            approved_commits: BTreeMap::new(),
            requests: BTreeMap::new(),
            edits: BTreeMap::new(),
            registers: RegisterStore::new(),
            rings: KillRings::default(),
        }
    }

    pub fn snapshot(&self) -> WorkspaceSnapshot {
        self.workspace.snapshot()
    }
    pub fn workspace(&self) -> &CheckpointState {
        &self.workspace
    }
    pub fn actors_mut(&mut self) -> &mut ActorRegistry {
        self.workspace.actors_mut()
    }
    pub fn access_mut(&mut self) -> &mut AccessPolicy {
        &mut self.access
    }
    pub fn registers(&self) -> &RegisterStore {
        &self.registers
    }
    pub fn rings(&self) -> &KillRings {
        &self.rings
    }

    pub fn checkpoint(&self) -> CoordinatorCheckpoint {
        CoordinatorCheckpoint {
            workspace: self.workspace.checkpoint(),
            access: self.access.clone(),
            pending: self.pending.clone(),
            approved_commits: self.approved_commits.clone(),
            requests: self.requests.clone(),
            edits: self.edits.clone(),
            registers: self.registers.clone(),
            rings: self.rings.clone(),
        }
    }

    pub fn restore(checkpoint: &CoordinatorCheckpoint) -> Result<Self, CoordinatorError> {
        Ok(Self {
            workspace: checkpoint
                .workspace
                .restore()
                .map_err(CoordinatorError::Checkpoint)?,
            access: checkpoint.access.clone(),
            pending: checkpoint.pending.clone(),
            approved_commits: checkpoint.approved_commits.clone(),
            requests: checkpoint.requests.clone(),
            edits: checkpoint.edits.clone(),
            registers: checkpoint.registers.clone(),
            rings: checkpoint.rings.clone(),
        })
    }

    fn authorize(
        &self,
        actor: ActorId,
        capability: Capability,
        buffers: impl IntoIterator<Item = BufferId>,
    ) -> Result<(), CoordinatorError> {
        self.access
            .authorize(
                self.workspace.actors(),
                self.snapshot().catalog(),
                actor,
                capability,
                buffers,
            )
            .map_err(CoordinatorError::Access)
    }

    /// Submit a proposal without publishing a workspace revision.
    pub fn propose(&mut self, proposal: Proposal) -> Result<(), CoordinatorError> {
        let request = proposal.request();
        if let Some(pending) = self.pending.get(&request) {
            return if pending.proposal == proposal {
                Ok(())
            } else {
                Err(CoordinatorError::RequestReuse(request))
            };
        }
        if let Some((operation, _)) = self.requests.get(&request) {
            if self.approved_commits.contains_key(&request)
                && *operation == operation_digest(&Operation::Edit(proposal.transaction().clone()))
            {
                return Ok(());
            }
            return Err(CoordinatorError::RequestReuse(request));
        }
        let txn = proposal.transaction();
        self.authorize(
            txn.intent().actor(),
            Capability::Propose,
            txn.patches().iter().map(|(id, _)| *id),
        )?;
        proposal
            .preview(&self.snapshot())
            .map_err(CoordinatorError::Proposal)?;
        self.pending.insert(
            request,
            Pending {
                proposal,
                approval: None,
                reviewers: BTreeSet::new(),
            },
        );
        Ok(())
    }

    pub fn approve(&mut self, request: RequestId, actor: ActorId) -> Result<(), CoordinatorError> {
        if let Some((reviewers, _)) = self.approved_commits.get(&request) {
            return if reviewers.contains(&actor) {
                Ok(())
            } else {
                Err(CoordinatorError::Proposal(ProposalError::ApprovalMismatch))
            };
        }
        let pending = self
            .pending
            .get(&request)
            .ok_or(CoordinatorError::MissingProposal(request))?;
        if pending.reviewers.contains(&actor) {
            return Ok(());
        }
        let approval = pending
            .proposal
            .approve(
                &self.snapshot(),
                self.workspace.actors(),
                &self.access,
                actor,
            )
            .map_err(CoordinatorError::Proposal)?;
        let pending = self.pending.get_mut(&request).expect("pending proposal");
        pending.approval = Some(approval);
        pending.reviewers.insert(actor);
        Ok(())
    }

    pub fn commit_proposal(
        &mut self,
        request: RequestId,
    ) -> Result<CheckpointOutcome, CoordinatorError> {
        if let Some((_, outcome)) = self.approved_commits.get(&request) {
            return Ok(outcome.clone());
        }
        let pending = self
            .pending
            .get(&request)
            .ok_or(CoordinatorError::MissingProposal(request))?;
        let approval = pending
            .approval
            .as_ref()
            .ok_or(CoordinatorError::Unapproved(request))?;
        if approval.revision() != self.workspace.revision() {
            return Err(CoordinatorError::StaleApproval(request));
        }
        pending
            .proposal
            .approve(
                &self.snapshot(),
                self.workspace.actors(),
                &self.access,
                approval.approver(),
            )
            .map_err(CoordinatorError::Proposal)?;
        let reviewers = pending.reviewers.clone();
        let operation = Operation::Edit(pending.proposal.transaction().clone());
        let result = self.apply_inner(request, operation, true)?;
        self.pending.remove(&request);
        self.approved_commits
            .insert(request, (reviewers, result.clone()));
        Ok(result)
    }

    /// Admit one operation, returning the original outcome for exact retries.
    pub fn apply(
        &mut self,
        request: RequestId,
        operation: Operation,
    ) -> Result<CheckpointOutcome, CoordinatorError> {
        self.apply_inner(request, operation, false)
    }

    fn apply_inner(
        &mut self,
        request: RequestId,
        operation: Operation,
        approved: bool,
    ) -> Result<CheckpointOutcome, CoordinatorError> {
        if self.pending.contains_key(&request) && !approved {
            return Err(CoordinatorError::RequestReuse(request));
        }
        if let Some((old, outcome)) = self.requests.get(&request) {
            return if *old == operation_digest(&operation) {
                Ok(outcome.clone())
            } else {
                Err(CoordinatorError::RequestReuse(request))
            };
        }
        let mut register_update = None;
        let (op, link) = match &operation {
            Operation::Open { intent, name, text } => {
                let catalog = self.snapshot();
                let next = catalog
                    .catalog()
                    .live_ids()
                    .into_iter()
                    .chain(catalog.catalog().tombstoned_ids())
                    .map(|id| id.raw())
                    .max()
                    .map_or(Some(0), |id| id.checked_add(1))
                    .ok_or(CoordinatorError::Checkpoint(CheckpointError::IdExhausted))?;
                let id = BufferId::new(next);
                if !self.workspace.actors().is_active(intent.actor()) {
                    return Err(CoordinatorError::Access(AccessError::InactiveActor(
                        intent.actor(),
                    )));
                }
                if !self
                    .access
                    .is_granted(intent.actor(), id, Capability::Commit)
                {
                    return Err(CoordinatorError::Access(AccessError::Denied {
                        actor: intent.actor(),
                        buffer: id,
                        capability: Capability::Commit,
                    }));
                }
                if intent.base() != self.workspace.revision() {
                    return Err(CoordinatorError::RevisionMismatch {
                        base: intent.base(),
                        current: self.workspace.revision(),
                    });
                }
                (
                    CheckpointOp::Open {
                        request,
                        intent: *intent,
                        name: name.clone(),
                        text: text.clone(),
                    },
                    None,
                )
            }
            Operation::Close {
                intent,
                buffer,
                policy,
            } => {
                self.authorize(intent.actor(), Capability::Commit, [*buffer])?;
                if intent.base() != self.workspace.revision() {
                    return Err(CoordinatorError::RevisionMismatch {
                        base: intent.base(),
                        current: self.workspace.revision(),
                    });
                }
                (
                    CheckpointOp::Close {
                        request,
                        intent: *intent,
                        buffer: *buffer,
                        policy: *policy,
                    },
                    None,
                )
            }
            Operation::Edit(txn) => (
                CheckpointOp::Edit {
                    request,
                    txn: txn.clone(),
                },
                None,
            ),
            Operation::Cut {
                intent,
                view,
                selections,
                name,
                kind,
            } => {
                self.authorize(intent.actor(), Capability::Read, [view.buffer()])?;
                let plan = register_edit::cut(
                    *intent,
                    view,
                    &self.snapshot(),
                    selections,
                    name.clone(),
                    *kind,
                )
                .map_err(CoordinatorError::Register)?;
                register_update = Some((intent.actor(), name.clone(), plan.entry().clone()));
                (
                    CheckpointOp::Edit {
                        request,
                        txn: plan.transaction().clone(),
                    },
                    None,
                )
            }
            Operation::Undo {
                actor,
                target,
                transaction,
            }
            | Operation::Redo {
                actor,
                target,
                transaction,
            } => {
                if transaction.actor() != *actor {
                    return Err(CoordinatorError::InvalidTransaction(*transaction));
                }
                let record = self
                    .edits
                    .get(target)
                    .ok_or(CoordinatorError::UnknownTarget(*target))?;
                if record.owner != *actor {
                    return Err(CoordinatorError::NotOwner(*target));
                }
                let undo = matches!(operation, Operation::Undo { .. });
                if undo && record.compensation {
                    return Err(CoordinatorError::InvalidTransaction(*target));
                }
                let compensation_target = record.compensation;
                let root = record.root;
                let record = self
                    .edits
                    .get(&root)
                    .ok_or(CoordinatorError::UnknownTarget(root))?;
                let source = if undo {
                    if record.undone.is_some() {
                        return Err(CoordinatorError::AlreadyUndone(*target));
                    }
                    record
                } else {
                    let compensation = record.undone.ok_or(CoordinatorError::NotUndone(*target))?;
                    if compensation_target && *target != compensation {
                        return Err(CoordinatorError::NotUndone(*target));
                    }
                    self.edits
                        .get(&compensation)
                        .ok_or(CoordinatorError::UnknownTarget(compensation))?
                };
                let inverse = source.inverse.as_ref().ok_or(CoordinatorError::Checkpoint(
                    CheckpointError::Retention(super::retention::RetentionError::BaseExpired {
                        base: source.inverse_base,
                        floor: self.workspace.history().floor(),
                    }),
                ))?;
                let txn = Transaction::new(
                    EditIntent::new(*transaction, inverse.intent().base()),
                    inverse.patches().to_vec(),
                )
                .expect("distinct inverse buffers");
                (
                    CheckpointOp::Edit { request, txn },
                    Some((root, undo, *transaction)),
                )
            }
        };
        let mut inverse = None;
        if let CheckpointOp::Edit { txn, .. } = &op {
            self.authorize(
                txn.intent().actor(),
                Capability::Commit,
                txn.patches().iter().map(|(id, _)| *id),
            )?;
            if self.workspace.receipt_by_txn(txn.intent().txn()).is_some() {
                return Err(CoordinatorError::TransactionReuse(txn.intent().txn()));
            }
            let applied = self
                .workspace
                .history()
                .rebase(txn)
                .map_err(|e| CoordinatorError::Checkpoint(CheckpointError::Retention(e)))?;
            let result = applied
                .apply_with_inverse(&self.snapshot())
                .map_err(|e| CoordinatorError::Checkpoint(CheckpointError::Transaction(e)))?;
            if result.is_noop() {
                return Err(CoordinatorError::NoChange);
            }
            inverse = Some((
                txn.intent().txn(),
                EditRecord {
                    inverse: Some(result.inverse().clone()),
                    inverse_base: result.inverse().intent().base(),
                    owner: txn.intent().actor(),
                    undone: None,
                    root: link.map_or(txn.intent().txn(), |(target, _, _)| target),
                    compensation: link.is_some_and(|(_, undo, _)| undo),
                },
            ));
        }
        let outcome = self
            .workspace
            .apply(op)
            .map_err(CoordinatorError::Checkpoint)?;
        if let Some((id, record)) = inverse {
            self.edits.insert(id, record);
        }
        if let Some((target, undo, transaction)) = link {
            self.edits.get_mut(&target).expect("target exists").undone =
                undo.then_some(transaction);
            if !undo {
                let inverse = self
                    .edits
                    .get(&transaction)
                    .expect("redo exists")
                    .inverse
                    .clone();
                self.edits.get_mut(&target).expect("target exists").inverse = inverse;
                self.edits
                    .get_mut(&target)
                    .expect("target exists")
                    .inverse_base = self
                    .edits
                    .get(&transaction)
                    .expect("redo exists")
                    .inverse_base;
            }
        }
        if let Some((actor, name, entry)) = register_update {
            self.registers.put(actor, name, entry.clone());
            self.rings.get_mut(actor).push(entry);
        }
        self.requests
            .insert(request, (operation_digest(&operation), outcome.clone()));
        Ok(outcome)
    }

    pub fn compact(&mut self, floor: Revision) -> Result<(), CoordinatorError> {
        self.workspace
            .compact(floor)
            .map_err(CoordinatorError::Checkpoint)?;
        for record in self.edits.values_mut() {
            if record.inverse_base <= self.workspace.history().floor() {
                record.inverse = None;
            }
        }
        Ok(())
    }

    /// Stores a register payload after an already validated operation.
    pub fn register(&self, actor: ActorId, name: &RegisterName) -> Option<&RegisterEntry> {
        self.registers.get(actor, name)
    }
}
