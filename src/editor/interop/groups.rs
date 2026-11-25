//! Plans one undoable change group for each committed receipt.
//!
//! Sequence metadata identifies recorded-command steps without joining their
//! individual commits. Callers supply it because receipts do not contain it.
//! Plans describe frontend operations; they do not execute editor commands.

use std::fmt;

use crate::editor::collab::receipt::{CommitReceipt, SubmitOutcome};
use crate::editor::edit::ids::TxnId;

/// A recorded sequence identifier and the zero-based committed step index.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SequenceStep {
    /// Stable caller-assigned identifier for this recorded sequence.
    pub sequence: TxnId,
    /// Index in the original recorded command sequence.
    pub index: usize,
}

/// One committed transaction and its frontend undo boundaries.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ChangeGroup {
    /// The receipt is the deterministic group identifier, including revision.
    pub id: CommitReceipt,
    /// The original commit evidence.
    pub receipt: CommitReceipt,
    /// Optional recorded-command origin; separate steps remain separate groups.
    pub sequence: Option<SequenceStep>,
}

impl ChangeGroup {
    /// First insert an undo boundary in every affected buffer, then prepare a
    /// change group per buffer and combine their handles. Activate the combined
    /// group, apply changes, amalgamate its undo records and accept it; on
    /// failure cancel it. Finally insert an undo boundary in each buffer.
    pub const fn emacs(&self) -> EmacsPlan {
        EmacsPlan {
            boundary_before: true,
            atomic_change_group: true,
            prepare_each_buffer_and_combine: true,
            amalgamate_before_accept: true,
            boundary_after: true,
        }
    }

    /// Start a new Neovim undo block in each affected buffer. Join only
    /// subsequent edits to that same buffer within this receipt. Capture each
    /// buffer's `endofline` before and after applying the receipt. On apply,
    /// associate each affected buffer's undo-block sequence number and both
    /// `endofline` values with `self.id`. On undo or redo of that group, step
    /// every buffer recorded for `self.id` together and restore its captured
    /// before or after `endofline`, respectively; Neovim undo does not do so.
    pub const fn neovim(&self) -> NeovimPlan {
        NeovimPlan {
            new_undo_block_per_buffer: true,
            join_subsequent_edits_in_same_buffer: true,
            map_undo_blocks_to_group_id: true,
            coordinate_multi_buffer_undo_redo: true,
            restore_endofline_on_undo_redo: true,
        }
    }
}

/// Emacs boundary instructions for one receipt.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct EmacsPlan {
    pub boundary_before: bool,
    pub atomic_change_group: bool,
    /// Prepare a handle for each buffer and combine handles before activation.
    pub prepare_each_buffer_and_combine: bool,
    /// Remove internal undo boundaries with undo-amalgamate-change-group.
    pub amalgamate_before_accept: bool,
    pub boundary_after: bool,
}

/// Neovim undo-block instructions for one receipt.
///
/// On apply, the frontend records each affected buffer's undo-block sequence
/// number and before/after `endofline` values under [`ChangeGroup::id`]. Undo
/// or redo of that ID must step all recorded buffers together and restore the
/// corresponding before or after `endofline` values as one group operation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct NeovimPlan {
    pub new_undo_block_per_buffer: bool,
    pub join_subsequent_edits_in_same_buffer: bool,
    /// Associate each affected buffer's undo-block sequence number with the group ID.
    pub map_undo_blocks_to_group_id: bool,
    /// Step all buffers recorded under the group ID together on undo or redo.
    pub coordinate_multi_buffer_undo_redo: bool,
    /// Capture per-buffer before/after values and restore them on undo/redo.
    pub restore_endofline_on_undo_redo: bool,
}

/// Contradictory commit evidence or sequence metadata.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GroupError {
    /// A committed revision already identifies another receipt.
    RevisionConflict,
    /// A receipt was redelivered with different sequence metadata.
    SequenceConflict(TxnId),
    /// A sequence step already identifies another receipt.
    StepConflict(SequenceStep),
}

impl fmt::Display for GroupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "change group conflict: {self:?}")
    }
}
impl std::error::Error for GroupError {}

/// An ordered projection of observed commits.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupPlan {
    groups: Vec<ChangeGroup>,
}

impl GroupPlan {
    /// Creates an empty projection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Groups in first-observation order.
    pub fn groups(&self) -> &[ChangeGroup] {
        &self.groups
    }

    /// Projects an outcome. An exact redelivery returns its existing group;
    /// contradictory evidence returns an error without mutating the plan.
    /// Rejections and no-ops leave the plan unchanged.
    pub fn project(
        &mut self,
        outcome: SubmitOutcome,
        sequence: Option<SequenceStep>,
    ) -> Result<Option<ChangeGroup>, GroupError> {
        let Some(receipt) = outcome.committed_receipt() else {
            return Ok(None);
        };
        let mut existing = None;
        for (index, group) in self.groups.iter().enumerate() {
            if group.id == receipt {
                if sequence.is_some() && group.sequence.is_some() && group.sequence != sequence {
                    return Err(GroupError::SequenceConflict(receipt.txn()));
                }
                existing = Some(index);
                continue;
            }
            if group.receipt.committed() == receipt.committed() {
                return Err(GroupError::RevisionConflict);
            }
            if let Some(step) = sequence {
                if group.sequence == Some(step) {
                    return Err(GroupError::StepConflict(step));
                }
            }
        }
        if let Some(index) = existing {
            if self.groups[index].sequence.is_none() {
                self.groups[index].sequence = sequence;
            }
            return Ok(Some(self.groups[index]));
        }
        let group = ChangeGroup {
            id: receipt,
            receipt,
            sequence,
        };
        self.groups.push(group);
        Ok(Some(group))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::collab::receipt::RejectReason;
    use crate::editor::collab::{registry::ActorRegistry, role::ActorRole, sequencer::Sequencer};
    use crate::editor::edit::{
        command::EditCommand,
        ids::{ActorId, BufferId, Nonce},
        intent::EditIntent,
        recorded_commands::{self, RecordedAction, RecordedCommand},
    };
    use crate::editor::interop::{
        batch::{decode_batch, ExternalEdit},
        coordinates::CoordKind,
        region::{EndpointMode, OrientedRegion},
    };
    use crate::editor::test_support::assert_law;
    use crate::editor::text::utf8::Text;
    use crate::editor::workspace::{
        catalog::BufferCatalog, selections::Selection, snapshots::VersionedWorkspace,
        version::Revision, view::BufferView,
    };

    fn txn(actor: u64, nonce: u64) -> TxnId {
        TxnId::new(ActorId::new(actor), Nonce::new(nonce))
    }
    fn receipt(nonce: u64, previous: u64) -> CommitReceipt {
        CommitReceipt::new(
            EditIntent::new(txn(1, nonce), Revision::new(previous)),
            Revision::new(previous),
        )
        .unwrap()
    }

    #[test]
    fn projection_laws_and_deterministic_replay() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng| {
                (0..128)
                    .map(|i| (rng.next_u64(), i, rng.gen_bool()))
                    .collect::<Vec<_>>()
            },
            |input| {
                let mut plan = GroupPlan::new();
                let mut replay = GroupPlan::new();
                for &(nonce, i, recorded) in input {
                    let intent = EditIntent::new(txn(nonce, i), Revision::new(i));
                    let receipt = CommitReceipt::new(intent, Revision::new(i)).unwrap();
                    let step = recorded.then_some(SequenceStep {
                        sequence: txn(2, i / 4),
                        index: (i % 4) as usize,
                    });
                    let outcome = SubmitOutcome::Committed(receipt);
                    let group = plan.project(outcome, step).unwrap().unwrap();
                    assert_eq!(
                        (group.id, group.receipt, group.sequence),
                        (receipt, receipt, step)
                    );
                    assert_eq!(
                        group.emacs(),
                        EmacsPlan {
                            boundary_before: true,
                            atomic_change_group: true,
                            prepare_each_buffer_and_combine: true,
                            amalgamate_before_accept: true,
                            boundary_after: true
                        }
                    );
                    assert_eq!(
                        group.neovim(),
                        NeovimPlan {
                            new_undo_block_per_buffer: true,
                            join_subsequent_edits_in_same_buffer: true,
                            map_undo_blocks_to_group_id: true,
                            coordinate_multi_buffer_undo_redo: true,
                            restore_endofline_on_undo_redo: true
                        }
                    );
                    assert_eq!(plan.groups().len(), i as usize + 1);
                    assert_eq!(plan.project(outcome, step), Ok(Some(group)));
                    assert_eq!(plan.project(outcome, None), Ok(Some(group)));
                    for reason in RejectReason::ALL {
                        assert_eq!(
                            plan.project(
                                SubmitOutcome::Rejected {
                                    intent,
                                    revision: Revision::new(i),
                                    reason: *reason
                                },
                                step
                            ),
                            Ok(None)
                        );
                    }
                    assert_eq!(
                        plan.project(
                            SubmitOutcome::NoOp {
                                intent,
                                revision: Revision::new(i)
                            },
                            step
                        ),
                        Ok(None)
                    );
                    assert_eq!(plan.groups().len(), i as usize + 1);
                    replay.project(outcome, step).unwrap();
                }
                assert_eq!(plan, replay);
                for &(nonce, i, recorded) in input {
                    let group = plan.groups()[i as usize];
                    assert_eq!(group.id.txn(), txn(nonce, i));
                    assert_eq!(
                        group.sequence,
                        recorded.then_some(SequenceStep {
                            sequence: txn(2, i / 4),
                            index: (i % 4) as usize
                        })
                    );
                }
            },
        );
    }

    #[test]
    fn multi_buffer_batch_coordinates_neovim_undo_by_group() {
        use std::collections::HashMap;

        let buffers = [BufferId::new(1), BufferId::new(2)];
        let mut catalog = BufferCatalog::new();
        for buffer in buffers {
            catalog.register(buffer, Text::from_str("before")).unwrap();
        }
        let mut actors = ActorRegistry::new();
        actors.register(ActorId::new(1), ActorRole::Human).unwrap();
        let mut sequencer = Sequencer::new(catalog, actors);
        let snapshot = sequencer.snapshot();
        let intent = EditIntent::new(txn(1, 7), snapshot.revision());
        let edits: Vec<_> = buffers
            .iter()
            .map(|&buffer| {
                let text = snapshot.catalog().get(buffer).unwrap();
                let view = BufferView::full(snapshot.catalog(), buffer).unwrap();
                let selection = Selection::new(
                    snapshot.catalog(),
                    &view,
                    buffer,
                    text.validate_offset(0).unwrap(),
                    text.validate_offset(6).unwrap(),
                )
                .unwrap();
                ExternalEdit {
                    buffer,
                    intent,
                    region: OrientedRegion::from_selection(
                        snapshot.catalog(),
                        &view,
                        selection,
                        CoordKind::EmacsChar,
                        EndpointMode::Exclusive,
                    )
                    .unwrap(),
                    replacement: Text::from_str("after"),
                }
            })
            .collect();
        let transaction = decode_batch(&snapshot, intent, &edits).unwrap();
        assert_eq!(transaction.patches().len(), 2);
        let outcome = sequencer.submit(transaction);
        assert!(outcome.is_committed());
        let mut groups = GroupPlan::new();
        let group = groups.project(outcome, None).unwrap().unwrap();
        assert_eq!(groups.groups(), &[group]);
        let plan = group.neovim();
        assert!(plan.map_undo_blocks_to_group_id);
        assert!(plan.coordinate_multi_buffer_undo_redo);
        assert!(plan.restore_endofline_on_undo_redo);

        // A frontend model stores each buffer's undo sequence and both states
        // under the receipt ID, then publishes the whole group state at once.
        let entries = [
            (buffers[0], 11, ("before", false), ("after", true)),
            (buffers[1], 27, ("before", true), ("after", false)),
        ];
        let records = HashMap::from([(group.id, entries)]);
        let mut states = [("after", true), ("after", false)];
        assert_eq!(states, [entries[0].3, entries[1].3]);
        for (redo, expected) in [
            (false, [("before", false), ("before", true)]),
            (true, [("after", true), ("after", false)]),
        ] {
            let recorded = records.get(&group.id).unwrap();
            assert_eq!((recorded[0].1, recorded[1].1), (11, 27));
            assert_eq!([recorded[0].0, recorded[1].0], buffers);
            let next = recorded.map(|entry| if redo { entry.3 } else { entry.2 });
            states = next;
            assert_eq!(states, expected);
        }
        for buffer in buffers {
            assert_eq!(
                sequencer.snapshot().catalog().get(buffer).unwrap().as_str(),
                "after"
            );
        }
    }

    #[test]
    fn conflicting_evidence_is_atomic() {
        let mut plan = GroupPlan::new();
        let first = receipt(1, 0);
        let step = SequenceStep {
            sequence: txn(9, 1),
            index: 0,
        };
        plan.project(SubmitOutcome::Committed(first), Some(step))
            .unwrap();
        let before = plan.clone();
        assert_eq!(
            plan.project(
                SubmitOutcome::Committed(first),
                Some(SequenceStep {
                    sequence: txn(9, 2),
                    index: 0
                })
            ),
            Err(GroupError::SequenceConflict(first.txn()))
        );
        assert_eq!(
            plan.project(SubmitOutcome::Committed(receipt(2, 0)), None),
            Err(GroupError::RevisionConflict)
        );
        assert_eq!(
            plan.project(SubmitOutcome::Committed(receipt(2, 1)), Some(step)),
            Err(GroupError::StepConflict(step))
        );
        assert_eq!(plan, before);
    }

    #[test]
    fn late_sequence_metadata_is_preserved_and_conflicts_are_atomic() {
        let mut plan = GroupPlan::new();
        let first = SubmitOutcome::Committed(receipt(1, 0));
        let second = SubmitOutcome::Committed(receipt(2, 1));
        let step = SequenceStep {
            sequence: txn(9, 1),
            index: 0,
        };
        let mut reversed = GroupPlan::new();
        reversed.project(second, None).unwrap();
        reversed.project(first, Some(step)).unwrap();
        let before = reversed.clone();
        assert_eq!(
            reversed.project(second, Some(step)),
            Err(GroupError::StepConflict(step))
        );
        assert_eq!(reversed, before);
        plan.project(first, None).unwrap();
        let tagged = plan.project(first, Some(step)).unwrap().unwrap();
        assert_eq!(tagged.sequence, Some(step));
        assert_eq!(plan.project(first, None), Ok(Some(tagged)));
        plan.project(second, None).unwrap();
        let before = plan.clone();
        assert_eq!(
            plan.project(second, Some(step)),
            Err(GroupError::StepConflict(step))
        );
        assert_eq!(plan, before);
    }

    #[test]
    fn recorded_fold_reused_transaction_ids_keep_each_step() {
        let buffer = BufferId::new(1);
        let mut catalog = BufferCatalog::new();
        catalog.register(buffer, Text::from_str("base")).unwrap();
        let mut workspace = VersionedWorkspace::new(catalog);
        let view = BufferView::full(workspace.capture().catalog(), buffer).unwrap();
        let point = Text::from_str("base").validate_offset(0).unwrap();
        let mut plan = GroupPlan::new();
        for sequence in 0..2 {
            let commands: Vec<_> = (0..2)
                .map(|index| RecordedCommand {
                    intent: EditIntent::new(txn(1, 1), Revision::new(sequence * 2 + index)),
                    view,
                    command: RecordedAction::Edit(EditCommand::Insert {
                        at: point,
                        text: Text::from_str("x"),
                    }),
                })
                .collect();
            let result = recorded_commands::fold(&mut workspace, &commands);
            assert_eq!(result.failure, None);
            assert_eq!(result.edits.len(), 2);
            for (index, edit) in result.edits.iter().enumerate() {
                let receipt = CommitReceipt::new(edit.intent(), edit.intent().base()).unwrap();
                let step = SequenceStep {
                    sequence: txn(9, sequence),
                    index,
                };
                let group = plan
                    .project(SubmitOutcome::Committed(receipt), Some(step))
                    .unwrap()
                    .unwrap();
                assert_eq!(group.sequence, Some(step));
            }
        }
        assert_eq!(workspace.capture().revision(), Revision::new(4));
        assert_eq!(plan.groups().len(), 4);
        assert!(plan
            .groups()
            .windows(2)
            .all(|pair| pair[0].id != pair[1].id));
    }
}
