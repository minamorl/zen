//! Plans editor commands as canonical single-buffer transactions.
//!
//! Every command becomes one splice, so an empty-span replacement is exactly
//! an insertion, an empty-text replacement is exactly a deletion, and either
//! selection deletion is the same deletion of its orientation-free range.
//! Planning uses only its arguments and deterministic constructors. It checks
//! revision, target and live UTF-8 boundaries without changing the snapshot.
//! Finally it evaluates the transaction, including revision overflow checks;
//! thus every returned transaction applies with an inverse that restores the
//! original catalog. These properties hold for no-ops as well.

use std::fmt;

use crate::editor::edit::{
    intent::EditIntent,
    patch::Patch,
    splice::Splice,
    transaction::{Transaction, TransactionError},
};
use crate::editor::text::{
    span::ByteSpan,
    utf8::{ByteOffset, Text},
};
use crate::editor::workspace::{
    selections::Selection,
    snapshots::WorkspaceSnapshot,
    view::{BufferView, ViewError},
};

/// A primitive edit addressed through a buffer view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditCommand {
    /// Insert text at an inclusive visible endpoint.
    Insert { at: ByteOffset, text: Text },
    /// Delete a half-open range.
    Delete { span: ByteSpan },
    /// Replace a half-open range.
    Replace { span: ByteSpan, text: Text },
    /// Replace the orientation-independent selection range.
    ReplaceSelection { selection: Selection, text: Text },
    /// Delete the orientation-independent selection range.
    DeleteSelection { selection: Selection },
}

/// A command cannot be planned against the supplied view and snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandError {
    /// View, buffer, visibility, or live UTF-8 validation failed.
    View(ViewError),
    /// Transaction evaluation failed, including revision mismatch or overflow.
    Transaction(TransactionError),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::View(error) => error.fmt(f),
            Self::Transaction(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for CommandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::View(error) => Some(error),
            Self::Transaction(error) => Some(error),
        }
    }
}

/// Plans an edit without mutating the view or snapshot.
///
/// Revision mismatch is checked first, then target identity and containment,
/// then the view and target endpoints against the snapshot's live text. A
/// transaction that cannot be evaluated (for example at revision overflow) is
/// rejected as well. Identical inputs produce identical results.
pub fn plan(
    intent: EditIntent,
    view: &BufferView,
    snapshot: &WorkspaceSnapshot,
    command: EditCommand,
) -> Result<Transaction, CommandError> {
    if intent.base() != snapshot.revision() {
        return Err(CommandError::Transaction(
            TransactionError::RevisionMismatch {
                expected: intent.base(),
                actual: snapshot.revision(),
            },
        ));
    }
    let (buffer, span, text) = match command {
        EditCommand::Insert { at, text } => (view.buffer(), ByteSpan::empty_at(at), text),
        EditCommand::Delete { span } => (view.buffer(), span, Text::empty()),
        EditCommand::Replace { span, text } => (view.buffer(), span, text),
        EditCommand::ReplaceSelection { selection, text } => {
            (selection.buffer(), selection.range(), text)
        }
        EditCommand::DeleteSelection { selection } => {
            (selection.buffer(), selection.range(), Text::empty())
        }
    };
    view.check_target(buffer, span)
        .map_err(CommandError::View)?;
    view.visible_text(snapshot.catalog())
        .map_err(CommandError::View)?;
    let live = snapshot
        .catalog()
        .get(buffer)
        .expect("visible_text checked live buffer");
    span.slice_in(live)
        .map_err(|error| CommandError::View(ViewError::InvalidSpan(error)))?;
    let patch =
        Patch::new(vec![Splice::new(span, text)]).expect("one splice cannot contact another");
    let transaction =
        Transaction::new(intent, vec![(buffer, patch)]).expect("one buffer cannot be duplicated");
    transaction
        .apply_with_inverse(snapshot)
        .map_err(CommandError::Transaction)?;
    Ok(transaction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::edit::ids::{ActorId, BufferId, Nonce};
    use crate::editor::text::span::SpanError;
    use crate::editor::workspace::{
        catalog::BufferCatalog, snapshots::VersionedWorkspace, version::Revision,
    };

    fn intent(revision: Revision) -> EditIntent {
        EditIntent::for_actor(ActorId::new(1), Nonce::new(2), revision)
    }

    #[test]
    fn exhaustive_utf8_equivalence_determinism_and_inverse() {
        for source in ["", "abc", "界a", "e\u{301}x", "😀a", "a😀\r"] {
            let id = BufferId::new(1);
            let text = Text::from_str(source);
            let points = text.char_boundaries();
            let mut catalog = BufferCatalog::new();
            catalog.register(id, text).unwrap();
            let workspace = VersionedWorkspace::new(catalog);
            let snapshot = workspace.capture();
            let view = BufferView::full(snapshot.catalog(), id).unwrap();
            let plan_twice = |command: EditCommand| {
                let first = plan(
                    intent(snapshot.revision()),
                    &view,
                    &snapshot,
                    command.clone(),
                )
                .unwrap();
                assert_eq!(
                    plan(intent(snapshot.revision()), &view, &snapshot, command),
                    Ok(first.clone())
                );
                first
            };
            for (i, &start) in points.iter().enumerate() {
                for &end in &points[i..] {
                    let span = ByteSpan::new(start, end).unwrap();
                    let selection =
                        Selection::new(snapshot.catalog(), &view, id, start, end).unwrap();
                    for replacement in ["", "x", "界😀"] {
                        let text = Text::from_str(replacement);
                        let command = EditCommand::Replace {
                            span,
                            text: text.clone(),
                        };
                        let planned = plan_twice(command);
                        assert_eq!(
                            plan_twice(EditCommand::ReplaceSelection {
                                selection,
                                text: text.clone()
                            }),
                            planned
                        );
                        assert_eq!(
                            plan_twice(EditCommand::ReplaceSelection {
                                selection: selection.flip(),
                                text: text.clone()
                            }),
                            planned
                        );
                        if span.is_empty() {
                            assert_eq!(
                                plan_twice(EditCommand::Insert {
                                    at: start,
                                    text: text.clone()
                                }),
                                planned
                            );
                        }
                        if text.is_empty() {
                            for command in [
                                EditCommand::Delete { span },
                                EditCommand::DeleteSelection { selection },
                                EditCommand::DeleteSelection {
                                    selection: selection.flip(),
                                },
                            ] {
                                assert_eq!(plan_twice(command), planned);
                            }
                        }
                        let result = planned.apply_with_inverse(&snapshot).unwrap();
                        let expected = format!(
                            "{}{}{}",
                            &source[..start.as_usize()],
                            replacement,
                            &source[end.as_usize()..]
                        );
                        assert_eq!(result.catalog().get(id).unwrap().as_str(), expected);
                        assert_eq!(
                            result.inverse().apply_to_catalog(result.catalog()).unwrap(),
                            *snapshot.catalog()
                        );
                        assert_eq!(workspace.capture(), snapshot);
                    }
                }
            }
        }
    }

    #[test]
    fn exhaustive_invalid_boundaries_and_narrow_views() {
        for source in ["", "abc", "界a", "e\u{301}x", "😀a", "a😀\r"] {
            let id = BufferId::new(1);
            let text = Text::from_str(source);
            let points = text.char_boundaries();
            let ascii = Text::from_str(&"a".repeat(source.len() + 1));
            let mut catalog = BufferCatalog::new();
            catalog.register(id, text.clone()).unwrap();
            let workspace = VersionedWorkspace::new(catalog);
            let snapshot = workspace.capture();
            let full = BufferView::full(snapshot.catalog(), id).unwrap();
            for start in 0..=source.len() {
                for end in start..=source.len() {
                    let span = ByteSpan::validate_in(&ascii, start, end).unwrap();
                    if let Err(error) = ByteSpan::validate_in(&text, start, end) {
                        assert_eq!(
                            plan(
                                intent(snapshot.revision()),
                                &full,
                                &snapshot,
                                EditCommand::Delete { span }
                            ),
                            Err(CommandError::View(ViewError::InvalidSpan(error)))
                        );
                    }
                }
            }
            for (i, &start) in points.iter().enumerate() {
                for &end in &points[i..] {
                    let visible = ByteSpan::new(start, end).unwrap();
                    let view = BufferView::narrowed(snapshot.catalog(), id, visible).unwrap();
                    for (j, &target_start) in points.iter().enumerate() {
                        for &target_end in &points[j..] {
                            let span = ByteSpan::new(target_start, target_end).unwrap();
                            if !visible.contains_span(span) {
                                assert_eq!(
                                    plan(
                                        intent(snapshot.revision()),
                                        &view,
                                        &snapshot,
                                        EditCommand::Delete { span }
                                    ),
                                    Err(CommandError::View(ViewError::SpanOutsideVisible {
                                        requested: span,
                                        visible
                                    }))
                                );
                            } else {
                                let command = EditCommand::Delete { span };
                                let txn = plan(
                                    intent(snapshot.revision()),
                                    &view,
                                    &snapshot,
                                    command.clone(),
                                )
                                .unwrap();
                                assert_eq!(
                                    plan(intent(snapshot.revision()), &full, &snapshot, command),
                                    Ok(txn.clone())
                                );
                                let result = txn.apply_with_inverse(&snapshot).unwrap();
                                assert_eq!(
                                    result.inverse().apply_to_catalog(result.catalog()).unwrap(),
                                    *snapshot.catalog()
                                );
                            }
                        }
                    }
                }
            }
            assert_eq!(workspace.capture(), snapshot);
        }
    }

    #[test]
    fn rejects_invalid_targets_without_mutation() {
        let id = BufferId::new(1);
        let other = BufferId::new(2);
        let ascii = Text::from_str("abcd");
        let mut catalog = BufferCatalog::new();
        catalog.register(id, ascii.clone()).unwrap();
        catalog.register(other, ascii.clone()).unwrap();
        let full = BufferView::full(&catalog, id).unwrap();
        let other_view = BufferView::full(&catalog, other).unwrap();
        let selection = Selection::new(
            &catalog,
            &other_view,
            other,
            ascii.validate_offset(0).unwrap(),
            ascii.validate_offset(1).unwrap(),
        )
        .unwrap();
        let narrow = full
            .narrow(&catalog, ByteSpan::validate_in(&ascii, 1, 3).unwrap())
            .unwrap();
        let mut workspace = VersionedWorkspace::new(catalog);
        workspace.update(id, Text::from_str("界a")).unwrap();
        let snapshot = workspace.capture();
        let at = ascii.validate_offset(1).unwrap();
        let delete = EditCommand::Delete {
            span: ByteSpan::empty_at(at),
        };
        assert_eq!(
            plan(intent(Revision::ZERO), &full, &snapshot, delete.clone()),
            Err(CommandError::Transaction(
                TransactionError::RevisionMismatch {
                    expected: Revision::ZERO,
                    actual: snapshot.revision()
                }
            ))
        );
        assert!(matches!(
            plan(
                intent(snapshot.revision()),
                &full,
                &snapshot,
                EditCommand::DeleteSelection { selection }
            ),
            Err(CommandError::View(ViewError::BufferMismatch { .. }))
        ));
        assert!(matches!(
            plan(
                intent(snapshot.revision()),
                &narrow,
                &snapshot,
                EditCommand::Insert {
                    at: ascii.validate_offset(0).unwrap(),
                    text: Text::empty()
                }
            ),
            Err(CommandError::View(ViewError::SpanOutsideVisible { .. }))
        ));
        assert_eq!(
            plan(intent(snapshot.revision()), &full, &snapshot, delete),
            Err(CommandError::View(ViewError::InvalidSpan(
                SpanError::NotCharBoundary { offset: 1 }
            )))
        );
        assert_eq!(workspace.capture(), snapshot);
        workspace.remove(id).unwrap();
        let closed = workspace.capture();
        assert_eq!(
            plan(
                intent(closed.revision()),
                &full,
                &closed,
                EditCommand::Insert {
                    at,
                    text: Text::empty()
                }
            ),
            Err(CommandError::View(ViewError::BufferClosed(id)))
        );
        let empty = VersionedWorkspace::new(BufferCatalog::new()).capture();
        assert_eq!(
            plan(
                intent(empty.revision()),
                &full,
                &empty,
                EditCommand::Insert {
                    at,
                    text: Text::empty()
                }
            ),
            Err(CommandError::View(ViewError::UnknownBuffer(id)))
        );
    }
}
