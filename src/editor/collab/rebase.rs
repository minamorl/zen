//! Rebase transactions through a sequence of committed revisions.
//!
//! # Laws
//!
//! Closed footprints reject overlap, endpoint contact and coincident insertions.
//! For disjoint edits, each endpoint lies strictly outside every other edit;
//! both anchor affinities therefore give the same position. Transport preserves
//! the untouched gaps and replacement bytes, so applying either edit first
//! produces the same concatenation in each buffer and the same catalog.
//! A chain applies this argument in each intermediate coordinate frame: an
//! induction over the steps gives the same result and first error as repeated
//! single-step rebases when adjacent snapshots have the same revision and
//! catalog. Workspace identity does not affect coordinate continuity; a gap
//! in either revision or catalog is rejected.

use std::fmt;

use crate::editor::collab::footprints::{self, Footprint, SpanConflict};
use crate::editor::edit::{
    ids::BufferId,
    patch::{Patch, PatchError},
    splice::Splice,
    transaction::{Transaction, TransactionError},
};
use crate::editor::text::{
    anchors::{Affinity, Anchor, AnchorError, ReplacementEffect},
    span::{ByteSpan, SpanError},
};
use crate::editor::workspace::{snapshots::WorkspaceSnapshot, version::Revision};

/// A committed transaction and the snapshots immediately before and after it.
#[derive(Clone, Copy, Debug)]
pub struct RebaseStep<'a> {
    /// Snapshot at the committed transaction's base.
    pub before: &'a WorkspaceSnapshot,
    /// The committed transaction.
    pub committed: &'a Transaction,
    /// Snapshot at the next revision, containing the committed result.
    pub after: &'a WorkspaceSnapshot,
}

/// A rebase could not be performed safely.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RebaseError {
    /// A transaction or snapshot has an unexpected revision.
    RevisionMismatch {
        expected: Revision,
        actual: Revision,
    },
    /// The base revision cannot advance.
    RevisionOverflow,
    /// Two edits intersect as closed intervals.
    Conflict(SpanConflict),
    /// A transaction is invalid for its snapshot.
    Transaction(TransactionError),
    /// The committed result does not match the target catalog.
    TargetMismatch,
    /// Consecutive snapshots differ in revision or catalog.
    DisconnectedChain,
    /// An anchor could not be transported.
    Anchor(AnchorError),
    /// A transported span is invalid.
    Span(SpanError),
    /// The transported patch is invalid.
    Patch { buffer: BufferId, error: PatchError },
}

impl fmt::Display for RebaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "rebase failed: {self:?}")
    }
}
impl std::error::Error for RebaseError {}

/// Rebases a proposal over one committed step, validating both snapshots.
pub fn rebase_over_step(
    proposal: &Transaction,
    step: RebaseStep<'_>,
) -> Result<Transaction, RebaseError> {
    let base = proposal.intent().base();
    for actual in [step.committed.intent().base(), step.before.revision()] {
        if actual != base {
            return Err(RebaseError::RevisionMismatch {
                expected: base,
                actual,
            });
        }
    }
    let next = base.next().map_err(|_| RebaseError::RevisionOverflow)?;
    if step.after.revision() != next {
        return Err(RebaseError::RevisionMismatch {
            expected: next,
            actual: step.after.revision(),
        });
    }
    let left = Footprint::from_patches(proposal.patches().iter().map(|(id, patch)| (*id, patch)));
    let right = Footprint::from_patches(
        step.committed
            .patches()
            .iter()
            .map(|(id, patch)| (*id, patch)),
    );
    if footprints::conflicts(&left, &right) {
        return Err(RebaseError::Conflict(
            footprints::first_conflict(&left, &right).expect("conflict has a witness"),
        ));
    }
    proposal
        .apply_with_inverse(step.before)
        .map_err(RebaseError::Transaction)?;
    let committed = step
        .committed
        .apply_with_inverse(step.before)
        .map_err(RebaseError::Transaction)?;
    if committed.catalog() != step.after.catalog() {
        return Err(RebaseError::TargetMismatch);
    }
    let mut patches = Vec::new();
    for (id, patch) in proposal.patches() {
        let mut spans: Vec<_> = patch.splices().iter().map(|s| s.span()).collect();
        if let Some((_, other)) = step
            .committed
            .patches()
            .iter()
            .find(|(other_id, _)| other_id == id)
        {
            let mut text = step
                .before
                .catalog()
                .get(*id)
                .expect("proposal validated")
                .clone();
            // Right-to-left application keeps all remaining committed spans in base coordinates.
            for splice in other.splices().iter().rev() {
                let after = splice.apply(&text).map_err(|error| RebaseError::Patch {
                    buffer: *id,
                    error: PatchError::InvalidSplice(error),
                })?;
                let effect = ReplacementEffect {
                    span: splice.span(),
                    inserted_len: splice.replacement().len_bytes(),
                };
                for span in &mut spans {
                    let start = effect
                        .transport(
                            Anchor {
                                offset: span.start(),
                                affinity: Affinity::Left,
                            },
                            &text,
                            &after,
                        )
                        .map_err(RebaseError::Anchor)?;
                    let end = effect
                        .transport(
                            Anchor {
                                offset: span.end(),
                                affinity: Affinity::Right,
                            },
                            &text,
                            &after,
                        )
                        .map_err(RebaseError::Anchor)?;
                    *span = ByteSpan::new(start.offset, end.offset).map_err(RebaseError::Span)?;
                }
                text = after;
            }
        }
        let target = step
            .after
            .catalog()
            .get(*id)
            .ok_or(RebaseError::TargetMismatch)?;
        let mut splices = Vec::new();
        for (span, original) in spans.into_iter().zip(patch.splices()) {
            let span =
                ByteSpan::validate_in(target, span.start().as_usize(), span.end().as_usize())
                    .map_err(RebaseError::Span)?;
            splices.push(Splice::new(span, original.replacement().clone()));
        }
        patches.push((
            *id,
            Patch::new(splices).map_err(|error| RebaseError::Patch { buffer: *id, error })?,
        ));
    }
    let result = Transaction::new(proposal.intent().with_base(next), patches)
        .map_err(RebaseError::Transaction)?;
    result
        .apply_with_inverse(step.after)
        .map_err(RebaseError::Transaction)?;
    Ok(result)
}

/// Rebases in order, checking intermediate revisions and catalog contents.
pub fn rebase_over_steps(
    proposal: &Transaction,
    steps: &[RebaseStep<'_>],
) -> Result<Transaction, RebaseError> {
    let mut result = proposal.clone();
    for (index, step) in steps.iter().copied().enumerate() {
        if index > 0 {
            let previous = steps[index - 1].after;
            if previous.revision() != step.before.revision()
                || previous.catalog() != step.before.catalog()
            {
                return Err(RebaseError::DisconnectedChain);
            }
        }
        result = rebase_over_step(&result, step)?;
    }
    Ok(result)
}

/// Alias for rebasing over a sequence of committed steps.
pub fn rebase_transaction(
    proposal: &Transaction,
    steps: &[RebaseStep<'_>],
) -> Result<Transaction, RebaseError> {
    rebase_over_steps(proposal, steps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::edit::{
        ids::{ActorId, Nonce},
        intent::EditIntent,
    };
    use crate::editor::text::utf8::Text;
    use crate::editor::workspace::{catalog::BufferCatalog, snapshots::VersionedWorkspace};

    fn transaction(
        text: &Text,
        start: usize,
        end: usize,
        replacement: &str,
        base: Revision,
        nonce: u64,
    ) -> Transaction {
        let span = ByteSpan::validate_in(text, start, end).unwrap();
        let patch = Patch::new(vec![Splice::new(span, Text::from_str(replacement))]).unwrap();
        Transaction::new(
            EditIntent::for_actor(ActorId::new(1), Nonce::new(nonce), base),
            vec![(BufferId::new(0), patch)],
        )
        .unwrap()
    }

    fn workspace(text: &Text) -> VersionedWorkspace {
        let mut catalog = BufferCatalog::new();
        catalog.register(BufferId::new(0), text.clone()).unwrap();
        VersionedWorkspace::new(catalog)
    }

    fn advance(workspace: &mut VersionedWorkspace, transaction: &Transaction) {
        let result = transaction
            .apply_with_inverse(&workspace.capture())
            .unwrap();
        workspace
            .update(
                BufferId::new(0),
                result.catalog().get(BufferId::new(0)).unwrap().clone(),
            )
            .unwrap();
    }

    #[test]
    fn exhaustive_diamond_and_conflicts() {
        for source in ["abc", "a界b"] {
            let text = Text::from_str(source);
            let boundaries = text.char_boundaries();
            for (i, &a) in boundaries.iter().enumerate() {
                for &b in &boundaries[i..] {
                    for (j, &c) in boundaries.iter().enumerate() {
                        for &d in &boundaries[j..] {
                            for replacement in ["", "x", "😀"] {
                                let p = transaction(
                                    &text,
                                    a.as_usize(),
                                    b.as_usize(),
                                    replacement,
                                    Revision::ZERO,
                                    1,
                                );
                                let q = transaction(
                                    &text,
                                    c.as_usize(),
                                    d.as_usize(),
                                    "界",
                                    Revision::ZERO,
                                    2,
                                );
                                let mut left = workspace(&text);
                                let before = left.capture();
                                advance(&mut left, &p);
                                let after = left.capture();
                                let rebased = rebase_over_step(
                                    &q,
                                    RebaseStep {
                                        before: &before,
                                        committed: &p,
                                        after: &after,
                                    },
                                );
                                if a <= d && c <= b {
                                    assert!(matches!(rebased, Err(RebaseError::Conflict(_))));
                                } else {
                                    let rebased = rebased.unwrap();
                                    assert_eq!(rebased.intent().txn(), q.intent().txn());
                                    let first = rebased.apply_with_inverse(&after).unwrap();
                                    let mut right = workspace(&text);
                                    let right_before = right.capture();
                                    advance(&mut right, &q);
                                    let right_after = right.capture();
                                    let reverse = rebase_over_step(
                                        &p,
                                        RebaseStep {
                                            before: &right_before,
                                            committed: &q,
                                            after: &right_after,
                                        },
                                    )
                                    .unwrap();
                                    assert_eq!(
                                        first.catalog(),
                                        reverse.apply_with_inverse(&right_after).unwrap().catalog()
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn chains_and_alignment() {
        let text = Text::from_str("abcdef");
        let proposal = transaction(&text, 5, 6, "Z", Revision::ZERO, 7);
        let first = transaction(&text, 0, 1, "界", Revision::ZERO, 1);
        let mut live = workspace(&text);
        let zero = live.capture();
        advance(&mut live, &first);
        let one = live.capture();
        let next_text = one.catalog().get(BufferId::new(0)).unwrap();
        let second = transaction(next_text, 3, 4, "x", Revision::new(1), 2);
        advance(&mut live, &second);
        let two = live.capture();
        let a = RebaseStep {
            before: &zero,
            committed: &first,
            after: &one,
        };
        let b = RebaseStep {
            before: &one,
            committed: &second,
            after: &two,
        };
        let direct = rebase_transaction(&proposal, &[a, b]).unwrap();
        assert_eq!(
            direct,
            rebase_over_step(&rebase_over_step(&proposal, a).unwrap(), b).unwrap()
        );
        direct.apply_with_inverse(&two).unwrap();
        assert!(matches!(
            rebase_over_step(&proposal, b),
            Err(RebaseError::RevisionMismatch { .. })
        ));
        assert!(matches!(
            rebase_over_steps(&proposal, &[a, a]),
            Err(RebaseError::DisconnectedChain)
        ));
        let stale = Transaction::new(
            second.intent().with_base(Revision::ZERO),
            second.patches().to_vec(),
        )
        .unwrap();
        let stale_step = RebaseStep {
            before: &one,
            committed: &stale,
            after: &two,
        };
        assert_eq!(
            rebase_over_steps(&proposal, &[a, stale_step]),
            Err(RebaseError::RevisionMismatch {
                expected: Revision::new(1),
                actual: Revision::ZERO
            })
        );
        let colliding = transaction(next_text, 7, 8, "q", Revision::new(1), 3);
        let mut other = workspace(&text);
        advance(&mut other, &first);
        advance(&mut other, &colliding);
        let collision_target = other.capture();
        let collision = RebaseStep {
            before: &one,
            committed: &colliding,
            after: &collision_target,
        };
        assert!(matches!(
            rebase_over_steps(&proposal, &[a, collision]),
            Err(RebaseError::Conflict(_))
        ));
    }

    #[test]
    fn chain_accepts_equivalent_snapshots_from_different_workspaces() {
        let text = Text::from_str("abcdefg");
        let proposal = transaction(&text, 6, 7, "P", Revision::ZERO, 7);
        let first = transaction(&text, 0, 1, "A", Revision::ZERO, 1);
        let mut original = workspace(&text);
        let zero = original.capture();
        advance(&mut original, &first);
        let one = original.capture();

        let mut other = workspace(&text);
        advance(&mut other, &first);
        let other_one = other.capture();
        assert_ne!(one, other_one);
        assert_eq!(one.revision(), other_one.revision());
        assert_eq!(one.catalog(), other_one.catalog());
        let middle_text = other_one.catalog().get(BufferId::new(0)).unwrap();
        let second = transaction(middle_text, 3, 4, "D", Revision::new(1), 2);
        advance(&mut other, &second);
        let two = other.capture();
        let first_step = RebaseStep {
            before: &zero,
            committed: &first,
            after: &one,
        };
        let second_step = RebaseStep {
            before: &other_one,
            committed: &second,
            after: &two,
        };
        let sequential = rebase_over_step(
            &rebase_over_step(&proposal, first_step).unwrap(),
            second_step,
        )
        .unwrap();
        let batch = rebase_over_steps(&proposal, &[first_step, second_step]).unwrap();
        assert_eq!(batch, sequential);
        assert_eq!(
            batch
                .apply_with_inverse(&two)
                .unwrap()
                .catalog()
                .get(BufferId::new(0))
                .unwrap()
                .as_str(),
            "AbcDefP"
        );

        let mut different = workspace(&text);
        different
            .update(BufferId::new(0), Text::from_str("AbcXefg"))
            .unwrap();
        let different_one = different.capture();
        let disconnected = RebaseStep {
            before: &different_one,
            committed: &second,
            after: &two,
        };
        assert!(rebase_over_step(
            &rebase_over_step(&proposal, first_step).unwrap(),
            disconnected
        )
        .is_ok());
        assert_eq!(
            rebase_over_steps(&proposal, &[first_step, disconnected]),
            Err(RebaseError::DisconnectedChain)
        );
    }

    #[test]
    fn generated_multisplice_multibuffer_and_chains() {
        let text = Text::from_str("abcdefghij");
        for first_text in ["", "界", "AB"] {
            for second_text in ["", "😀", "X"] {
                let mut catalog = BufferCatalog::new();
                for id in 0..2 {
                    catalog.register(BufferId::new(id), text.clone()).unwrap();
                }
                let mut live = VersionedWorkspace::new(catalog);
                let zero = live.capture();
                let mk = |start, end, value: &str| {
                    Splice::new(
                        ByteSpan::validate_in(&text, start, end).unwrap(),
                        Text::from_str(value),
                    )
                };
                let proposal = Transaction::new(
                    EditIntent::for_actor(ActorId::new(4), Nonce::new(5), Revision::ZERO),
                    vec![
                        (
                            BufferId::new(0),
                            Patch::new(vec![mk(4, 5, "P"), mk(8, 9, "Q")]).unwrap(),
                        ),
                        (BufferId::new(1), Patch::new(vec![mk(0, 1, "R")]).unwrap()),
                    ],
                )
                .unwrap();
                let first = Transaction::new(
                    EditIntent::for_actor(ActorId::new(1), Nonce::new(1), Revision::ZERO),
                    vec![
                        (
                            BufferId::new(0),
                            Patch::new(vec![mk(0, 1, first_text), mk(2, 3, second_text)]).unwrap(),
                        ),
                        (BufferId::new(1), Patch::new(vec![mk(4, 5, "S")]).unwrap()),
                    ],
                )
                .unwrap();
                let first_result = first.apply_with_inverse(&zero).unwrap();
                // One workspace update creates the target revision; construct a separate
                // two-buffer diamond below using catalog application.
                let mut one_catalog = first_result.catalog().clone();
                let mut one_live = VersionedWorkspace::new(one_catalog.clone());
                one_live
                    .update(
                        BufferId::new(0),
                        one_catalog.get(BufferId::new(0)).unwrap().clone(),
                    )
                    .unwrap();
                let one = one_live.capture();
                let step = RebaseStep {
                    before: &zero,
                    committed: &first,
                    after: &one,
                };
                let rebased = rebase_over_step(&proposal, step).unwrap();
                let left = rebased.apply_with_inverse(&one).unwrap();
                let proposal_result = proposal.apply_with_inverse(&zero).unwrap();
                let mut reverse_live = VersionedWorkspace::new(proposal_result.catalog().clone());
                reverse_live
                    .update(
                        BufferId::new(0),
                        proposal_result
                            .catalog()
                            .get(BufferId::new(0))
                            .unwrap()
                            .clone(),
                    )
                    .unwrap();
                let reverse_target = reverse_live.capture();
                let reverse = rebase_over_step(
                    &first,
                    RebaseStep {
                        before: &zero,
                        committed: &proposal,
                        after: &reverse_target,
                    },
                )
                .unwrap();
                assert_eq!(
                    left.catalog(),
                    reverse
                        .apply_with_inverse(&reverse_target)
                        .unwrap()
                        .catalog()
                );

                let current = one.catalog().get(BufferId::new(0)).unwrap();
                let at = first_text.len();
                let second = transaction(current, at, at, second_text, Revision::new(1), 9);
                let next_catalog = second.apply_with_inverse(&one).unwrap().catalog().clone();
                one_catalog = next_catalog;
                let mut two_live = VersionedWorkspace::new(one_catalog.clone());
                two_live
                    .update(
                        BufferId::new(0),
                        one_catalog.get(BufferId::new(0)).unwrap().clone(),
                    )
                    .unwrap();
                two_live
                    .update(
                        BufferId::new(0),
                        one_catalog.get(BufferId::new(0)).unwrap().clone(),
                    )
                    .unwrap();
                let two = two_live.capture();
                let second_step = RebaseStep {
                    before: &one,
                    committed: &second,
                    after: &two,
                };
                let chain = rebase_over_steps(&proposal, &[step, second_step]).unwrap();
                assert_eq!(chain, rebase_over_step(&rebased, second_step).unwrap());
                let expected = format!("{first_text}{second_text}b{second_text}dPfghQj");
                assert_eq!(
                    chain
                        .apply_with_inverse(&two)
                        .unwrap()
                        .catalog()
                        .get(BufferId::new(0))
                        .unwrap()
                        .as_str(),
                    expected
                );
                let collision = transaction(
                    current,
                    current.len_bytes() - 2,
                    current.len_bytes() - 1,
                    "!",
                    Revision::new(1),
                    10,
                );
                let collision_span = ByteSpan::validate_in(
                    current,
                    current.len_bytes() - 2,
                    current.len_bytes() - 1,
                )
                .unwrap();
                let witness = SpanConflict {
                    buffer: BufferId::new(0),
                    left: collision_span,
                    right: collision_span,
                };
                let collision_catalog = collision
                    .apply_with_inverse(&one)
                    .unwrap()
                    .catalog()
                    .clone();
                let mut collision_live = VersionedWorkspace::new(collision_catalog.clone());
                collision_live
                    .update(
                        BufferId::new(0),
                        collision_catalog.get(BufferId::new(0)).unwrap().clone(),
                    )
                    .unwrap();
                collision_live
                    .update(
                        BufferId::new(0),
                        collision_catalog.get(BufferId::new(0)).unwrap().clone(),
                    )
                    .unwrap();
                let collision_target = collision_live.capture();
                assert_eq!(
                    rebase_over_steps(
                        &proposal,
                        &[
                            step,
                            RebaseStep {
                                before: &one,
                                committed: &collision,
                                after: &collision_target
                            }
                        ]
                    ),
                    Err(RebaseError::Conflict(witness))
                );
                // Different buffers at identical positions must not conflict.
                let separate = Transaction::new(
                    proposal.intent(),
                    vec![(BufferId::new(1), Patch::new(vec![mk(0, 1, "T")]).unwrap())],
                )
                .unwrap();
                let only_first = Transaction::new(
                    first.intent(),
                    vec![(BufferId::new(0), Patch::new(vec![mk(0, 1, "U")]).unwrap())],
                )
                .unwrap();
                let target_catalog = only_first
                    .apply_with_inverse(&zero)
                    .unwrap()
                    .catalog()
                    .clone();
                live.update(
                    BufferId::new(0),
                    target_catalog.get(BufferId::new(0)).unwrap().clone(),
                )
                .unwrap();
                assert!(rebase_over_step(
                    &separate,
                    RebaseStep {
                        before: &zero,
                        committed: &only_first,
                        after: &live.capture()
                    }
                )
                .is_ok());
            }
        }
    }
}
