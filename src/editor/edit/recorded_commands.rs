//! Ordered, individually committed editor commands.
//!
//! Every command is planned against the current workspace and committed as its
//! own revision, even when it leaves text unchanged. Failure retains the
//! committed prefix and prevents later commands from being inspected.

use std::fmt;

use crate::editor::edit::{
    command::{self, CommandError, EditCommand},
    intent::EditIntent,
    motion::{self, MotionError},
    transaction::{Transaction, TransactionError},
};
use crate::editor::text::utf8::{ByteOffset, Text};
use crate::editor::workspace::{
    snapshots::{VersionedWorkspace, WorkspaceError},
    view::BufferView,
};

/// A command and the exact intent and view to use when it is reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedCommand {
    pub intent: EditIntent,
    pub view: BufferView,
    pub command: RecordedAction,
}

/// An edit, optionally locating its insertion point with a live scalar motion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordedAction {
    /// Plan an existing edit directly.
    Edit(EditCommand),
    /// Move from a point in the current text and insert at the result.
    InsertAfterMotion {
        point: ByteOffset,
        delta: isize,
        text: Text,
    },
}

/// The operation that prevented a command from being committed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordedCommandError {
    /// The motion could not be evaluated.
    Motion(MotionError),
    /// The command could not be planned.
    Plan(CommandError),
    /// The planned transaction could not be evaluated.
    Transaction(TransactionError),
    /// The evaluated edit could not be published.
    Commit(WorkspaceError),
}

impl fmt::Display for RecordedCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Motion(error) => error.fmt(f),
            Self::Plan(error) => error.fmt(f),
            Self::Transaction(error) => error.fmt(f),
            Self::Commit(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for RecordedCommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Motion(error) => Some(error),
            Self::Plan(error) => Some(error),
            Self::Transaction(error) => Some(error),
            Self::Commit(error) => Some(error),
        }
    }
}

/// The committed prefix and, when present, the index and cause of failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoldResult {
    pub edits: Vec<Transaction>,
    pub failure: Option<(usize, RecordedCommandError)>,
}

/// Plans and commits each command in order, stopping at the first error.
pub fn fold(workspace: &mut VersionedWorkspace, commands: &[RecordedCommand]) -> FoldResult {
    let mut edits = Vec::new();
    for (index, recorded) in commands.iter().enumerate() {
        let snapshot = workspace.capture();
        let result = (|| {
            let command = match &recorded.command {
                RecordedAction::Edit(command) => command.clone(),
                RecordedAction::InsertAfterMotion { point, delta, text } => {
                    let at =
                        motion::move_scalars(snapshot.catalog(), &recorded.view, *point, *delta)
                            .map_err(RecordedCommandError::Motion)?;
                    EditCommand::Insert {
                        at,
                        text: text.clone(),
                    }
                }
            };
            let transaction = command::plan(recorded.intent, &recorded.view, &snapshot, command)
                .map_err(RecordedCommandError::Plan)?;
            let evaluated = transaction
                .apply_with_inverse(&snapshot)
                .map_err(RecordedCommandError::Transaction)?;
            let updates = transaction.patches().iter().map(|(id, _)| {
                (
                    *id,
                    evaluated
                        .catalog()
                        .get(*id)
                        .expect("evaluated buffer")
                        .clone(),
                )
            });
            workspace
                .update_batch(updates)
                .map_err(RecordedCommandError::Commit)?;
            Ok(transaction)
        })();
        match result {
            Ok(transaction) => edits.push(transaction),
            Err(error) => {
                return FoldResult {
                    edits,
                    failure: Some((index, error)),
                }
            }
        }
    }
    FoldResult {
        edits,
        failure: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::edit::ids::{ActorId, BufferId, Nonce};
    use crate::editor::test_support::{assert_law, Rng};
    use crate::editor::text::utf8::Text;
    use crate::editor::workspace::{catalog::BufferCatalog, version::Revision};

    fn workspace() -> VersionedWorkspace {
        let mut catalog = BufferCatalog::new();
        catalog
            .register(BufferId::new(1), Text::from_str("base"))
            .unwrap();
        VersionedWorkspace::new(catalog)
    }

    fn commands(workspace: &VersionedWorkspace, values: &[bool]) -> Vec<RecordedCommand> {
        let view = BufferView::full(workspace.catalog(), BufferId::new(1)).unwrap();
        let at = workspace
            .catalog()
            .get(BufferId::new(1))
            .unwrap()
            .validate_offset(0)
            .unwrap();
        values
            .iter()
            .enumerate()
            .map(|(i, &change)| RecordedCommand {
                intent: EditIntent::for_actor(
                    ActorId::new(1),
                    Nonce::new(i as u64),
                    Revision::new(i as u64),
                ),
                view,
                command: RecordedAction::Edit(EditCommand::Insert {
                    at,
                    text: Text::from_str(if change { "x" } else { "" }),
                }),
            })
            .collect()
    }

    #[test]
    fn deterministic_fold_laws_and_direct_commits() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| {
                (0..128)
                    .map(|_| {
                        let len = rng.gen_range(0..12);
                        let values = (0..len).map(|_| rng.gen_bool()).collect::<Vec<_>>();
                        let split = rng.gen_range(0..len + 1);
                        (values, split)
                    })
                    .collect::<Vec<_>>()
            },
            |cases| {
                for (values, &split) in cases.iter().map(|(v, s)| (v, s)) {
                    let mut whole = workspace();
                    let sequence = commands(&whole, values);
                    let before = whole.capture();
                    let empty = fold(&mut whole, &[]);
                    assert_eq!(
                        empty,
                        FoldResult {
                            edits: vec![],
                            failure: None
                        }
                    );
                    assert_eq!(whole.capture(), before);
                    let all = fold(&mut whole, &sequence);
                    assert_eq!(all.failure, None);
                    let mut parts = workspace();
                    let first = fold(&mut parts, &sequence[..split]);
                    let second = fold(&mut parts, &sequence[split..]);
                    assert_eq!(first.failure, None);
                    assert_eq!(second.failure, None);
                    assert_eq!([first.edits, second.edits].concat(), all.edits);
                    assert_eq!(parts.revision(), whole.revision());
                    assert_eq!(parts.catalog(), whole.catalog());
                    let mut direct = workspace();
                    for (i, recorded) in sequence.iter().enumerate() {
                        let snapshot = direct.capture();
                        let txn = command::plan(
                            recorded.intent,
                            &recorded.view,
                            &snapshot,
                            match &recorded.command {
                                RecordedAction::Edit(command) => command.clone(),
                                _ => unreachable!(),
                            },
                        )
                        .unwrap();
                        let result = txn.apply_with_inverse(&snapshot).unwrap();
                        let updates = txn
                            .patches()
                            .iter()
                            .map(|(id, _)| (*id, result.catalog().get(*id).unwrap().clone()));
                        direct.update_batch(updates).unwrap();
                        assert_eq!(txn, all.edits[i]);
                        assert_eq!(direct.revision(), Revision::new(i as u64 + 1));
                    }
                    assert_eq!(direct.catalog(), whole.catalog());
                }
            },
        );
    }

    #[test]
    fn failure_preserves_prefix_and_ignores_suffix() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| {
                (0..64)
                    .map(|_| {
                        let len = rng.gen_range(1..12);
                        let values = (0..len).map(|_| rng.gen_bool()).collect::<Vec<_>>();
                        let failure = rng.gen_range(0..len);
                        (values, failure)
                    })
                    .collect::<Vec<_>>()
            },
            |cases| {
                for (values, &index) in cases.iter().map(|(v, i)| (v, i)) {
                    let mut actual = workspace();
                    let mut sequence = commands(&actual, values);
                    sequence[index].intent = sequence[index].intent.with_base(Revision::MAX);
                    let mut executable = sequence[index].clone();
                    executable.intent = executable.intent.with_base(Revision::new(index as u64));
                    executable.command = RecordedAction::Edit(EditCommand::Insert {
                        at: actual
                            .catalog()
                            .get(BufferId::new(1))
                            .unwrap()
                            .validate_offset(0)
                            .unwrap(),
                        text: Text::from_str("suffix"),
                    });
                    sequence.insert(index + 1, executable.clone());
                    let mut expected = workspace();
                    let prefix = fold(&mut expected, &sequence[..index]);
                    let mut probe = workspace();
                    assert_eq!(fold(&mut probe, &sequence[..index]).failure, None);
                    assert_eq!(fold(&mut probe, &[executable]).failure, None);
                    let result = fold(&mut actual, &sequence);
                    assert_eq!(result.edits, prefix.edits);
                    assert_eq!(result.failure.as_ref().map(|(i, _)| *i), Some(index));
                    assert_eq!(actual.revision(), expected.revision());
                    assert_eq!(actual.catalog(), expected.catalog());
                    let mut again = workspace();
                    assert_eq!(fold(&mut again, &sequence), result);
                    assert_eq!(again.catalog(), actual.catalog());
                }
            },
        );
    }

    #[test]
    fn motion_observes_the_preceding_commit() {
        let mut workspace = workspace();
        let mut sequence = commands(&workspace, &[true, true]);
        let point = workspace
            .catalog()
            .get(BufferId::new(1))
            .unwrap()
            .validate_offset(0)
            .unwrap();
        sequence[0].command = RecordedAction::Edit(EditCommand::Insert {
            at: point,
            text: Text::from_str("界"),
        });
        sequence[1].command = RecordedAction::InsertAfterMotion {
            point,
            delta: 1,
            text: Text::from_str("y"),
        };
        let result = fold(&mut workspace, &sequence);
        assert_eq!(result.failure, None);
        assert_eq!(workspace.revision(), Revision::new(2));
        assert_eq!(
            workspace.catalog().get(BufferId::new(1)).unwrap().as_str(),
            "界ybase"
        );
    }
}
