//! Atomic planning of multiple edits in one visible buffer.
//!
//! `ByteSpan`, `ByteOffset`, and `Selection` are value types, not identities
//! tied to a particular text. Their coordinates are revalidated against the
//! target snapshot's text and the view's visible bounds on every plan.
//! Identical edits are collapsed before closed-interval contact is rejected;
//! otherwise the patch is sorted independently of input order.

use std::fmt;

use crate::editor::edit::{
    command::EditCommand,
    intent::EditIntent,
    patch::{Patch, PatchError},
    splice::Splice,
    transaction::{Transaction, TransactionError},
};
use crate::editor::text::{span::ByteSpan, utf8::Text};
use crate::editor::workspace::{
    snapshots::WorkspaceSnapshot,
    view::{BufferView, ViewError},
};

/// A multi-cursor plan could not be validated or evaluated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MultiCursorError {
    /// A target or the visible range is invalid.
    View(ViewError),
    /// Distinct edits touch or overlap.
    Patch(PatchError),
    /// The transaction cannot be evaluated against the snapshot.
    Transaction(TransactionError),
}

impl fmt::Display for MultiCursorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::View(error) => error.fmt(f),
            Self::Patch(error) => error.fmt(f),
            Self::Transaction(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for MultiCursorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::View(error) => Some(error),
            Self::Patch(error) => Some(error),
            Self::Transaction(error) => Some(error),
        }
    }
}

/// Plans all commands against the same snapshot without changing it.
///
/// The revision is checked first. Every target and the view are checked even
/// for duplicate edits. An empty command list yields an empty patch for the
/// view's buffer. Conflicts and invalid inputs reject the entire plan.
pub fn plan(
    intent: EditIntent,
    view: &BufferView,
    snapshot: &WorkspaceSnapshot,
    commands: Vec<EditCommand>,
) -> Result<Transaction, MultiCursorError> {
    if intent.base() != snapshot.revision() {
        return Err(MultiCursorError::Transaction(
            TransactionError::RevisionMismatch {
                expected: intent.base(),
                actual: snapshot.revision(),
            },
        ));
    }
    view.visible_text(snapshot.catalog())
        .map_err(MultiCursorError::View)?;
    let live = snapshot
        .catalog()
        .get(view.buffer())
        .expect("view checked live buffer");
    let mut splices = Vec::with_capacity(commands.len());
    for command in commands {
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
            .map_err(MultiCursorError::View)?;
        span.slice_in(live)
            .map_err(|error| MultiCursorError::View(ViewError::InvalidSpan(error)))?;
        splices.push(Splice::new(span, text));
    }
    splices.sort_by(|a, b| {
        (a.span().start(), a.span().end(), a.replacement().as_str()).cmp(&(
            b.span().start(),
            b.span().end(),
            b.replacement().as_str(),
        ))
    });
    splices.dedup();
    let patch = Patch::new(splices).map_err(MultiCursorError::Patch)?;
    let transaction = Transaction::new(intent, vec![(view.buffer(), patch)])
        .expect("one buffer cannot be duplicated");
    transaction
        .apply_with_inverse(snapshot)
        .map_err(MultiCursorError::Transaction)?;
    Ok(transaction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::edit::ids::{ActorId, BufferId, Nonce};
    use crate::editor::test_support::{assert_law, texts, Rng};
    use crate::editor::text::span::SpanError;
    use crate::editor::workspace::{
        catalog::BufferCatalog, selections::Selection, snapshots::VersionedWorkspace,
        version::Revision,
    };

    fn setup(source: &str) -> (BufferView, WorkspaceSnapshot, EditIntent) {
        let id = BufferId::new(1);
        let mut catalog = BufferCatalog::new();
        catalog.register(id, Text::from_str(source)).unwrap();
        let snapshot = VersionedWorkspace::new(catalog).capture();
        let view = BufferView::full(snapshot.catalog(), id).unwrap();
        let intent = EditIntent::for_actor(ActorId::new(1), Nonce::new(1), snapshot.revision());
        (view, snapshot, intent)
    }

    #[test]
    fn permutation_dedup_and_inverse_property() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            let mut rng = Rng::new(91);
            for source in corpus {
                let (view, snapshot, intent) = setup(source);
                let text = snapshot.catalog().get(view.buffer()).unwrap();
                let points = text.char_boundaries();
                let mut commands = Vec::new();
                for &point in points.iter().step_by(2) {
                    commands.push(EditCommand::Insert {
                        at: point,
                        text: Text::from_str("界"),
                    });
                }
                let expected = plan(intent, &view, &snapshot, commands.clone()).unwrap();
                let result = expected.apply_with_inverse(&snapshot).unwrap();
                let mut reference = source.clone();
                for &point in points.iter().step_by(2).rev() {
                    reference.insert(point.as_usize(), '界');
                }
                assert_eq!(
                    result.catalog().get(view.buffer()).unwrap().as_str(),
                    reference
                );
                assert_eq!(
                    result.inverse().apply_to_catalog(result.catalog()).unwrap(),
                    *snapshot.catalog()
                );
                commands.extend(commands.clone());
                for _ in 0..5 {
                    rng.shuffle(&mut commands);
                    assert_eq!(
                        plan(intent, &view, &snapshot, commands.clone()),
                        Ok(expected.clone())
                    );
                }
                assert_eq!(
                    snapshot.catalog().get(view.buffer()).unwrap().as_str(),
                    source
                );
            }
        });
    }

    #[test]
    fn selections_duplicates_and_conflicts() {
        let (view, snapshot, intent) = setup("abcdef");
        let text = snapshot.catalog().get(view.buffer()).unwrap();
        let span = |a, b| ByteSpan::validate_in(text, a, b).unwrap();
        let selection = Selection::new(
            snapshot.catalog(),
            &view,
            view.buffer(),
            span(1, 1).start(),
            span(2, 2).start(),
        )
        .unwrap();
        let a = EditCommand::ReplaceSelection {
            selection,
            text: Text::from_str("X"),
        };
        let b = EditCommand::ReplaceSelection {
            selection: selection.flip(),
            text: Text::from_str("X"),
        };
        let c = EditCommand::Replace {
            span: span(1, 2),
            text: Text::from_str("X"),
        };
        let transaction = plan(intent, &view, &snapshot, vec![a, b, c]).unwrap();
        assert_eq!(transaction.patches()[0].1.splices().len(), 1);
        for (left, right) in [
            (span(0, 1), span(1, 2)),
            (span(0, 3), span(2, 4)),
            (span(1, 1), span(1, 1)),
        ] {
            let edits = vec![
                EditCommand::Replace {
                    span: left,
                    text: Text::from_str("x"),
                },
                EditCommand::Replace {
                    span: right,
                    text: Text::from_str("y"),
                },
            ];
            assert!(matches!(
                plan(intent, &view, &snapshot, edits),
                Err(MultiCursorError::Patch(PatchError::Contact { .. }))
            ));
        }
        assert_eq!(
            snapshot.catalog().get(view.buffer()).unwrap().as_str(),
            "abcdef"
        );
    }

    #[test]
    fn distinct_selections_and_invalid_selection_are_atomic() {
        let (view, snapshot, intent) = setup("abcdef");
        let text = snapshot.catalog().get(view.buffer()).unwrap();
        let selection = |a, b| {
            Selection::new(
                snapshot.catalog(),
                &view,
                view.buffer(),
                text.validate_offset(a).unwrap(),
                text.validate_offset(b).unwrap(),
            )
            .unwrap()
        };
        let edits = vec![
            EditCommand::ReplaceSelection {
                selection: selection(1, 2),
                text: Text::from_str("X"),
            },
            EditCommand::DeleteSelection {
                selection: selection(4, 5).flip(),
            },
        ];
        let planned = plan(intent, &view, &snapshot, edits.clone()).unwrap();
        assert_eq!(
            plan(intent, &view, &snapshot, edits.into_iter().rev().collect()),
            Ok(planned.clone())
        );
        assert_eq!(
            planned
                .apply_with_inverse(&snapshot)
                .unwrap()
                .catalog()
                .get(view.buffer())
                .unwrap()
                .as_str(),
            "aXcdf"
        );
        let stale = selection(1, 2);
        let (unicode_view, unicode_snapshot, unicode_intent) = setup("界abc");
        let valid = EditCommand::Insert {
            at: unicode_snapshot
                .catalog()
                .get(unicode_view.buffer())
                .unwrap()
                .validate_offset(3)
                .unwrap(),
            text: Text::from_str("x"),
        };
        assert!(matches!(
            plan(
                unicode_intent,
                &unicode_view,
                &unicode_snapshot,
                vec![valid, EditCommand::DeleteSelection { selection: stale }]
            ),
            Err(MultiCursorError::View(ViewError::InvalidSpan(_)))
        ));
        let narrow = BufferView::narrowed(
            snapshot.catalog(),
            view.buffer(),
            ByteSpan::validate_in(text, 3, 6).unwrap(),
        )
        .unwrap();
        let valid = EditCommand::Insert {
            at: text.validate_offset(4).unwrap(),
            text: Text::empty(),
        };
        assert!(matches!(
            plan(
                intent,
                &narrow,
                &snapshot,
                vec![valid, EditCommand::DeleteSelection { selection: stale }]
            ),
            Err(MultiCursorError::View(ViewError::SpanOutsideVisible { .. }))
        ));
        assert_eq!(
            unicode_snapshot
                .catalog()
                .get(unicode_view.buffer())
                .unwrap()
                .as_str(),
            "界abc"
        );
    }

    #[test]
    fn rejects_wrong_buffer_and_stale_view() {
        let id = BufferId::new(1);
        let other = BufferId::new(2);
        let text = Text::from_str("abc");
        let mut catalog = BufferCatalog::new();
        catalog.register(id, text.clone()).unwrap();
        catalog.register(other, text.clone()).unwrap();
        let view = BufferView::full(&catalog, id).unwrap();
        let other_view = BufferView::full(&catalog, other).unwrap();
        let start = text.validate_offset(0).unwrap();
        let end = text.validate_offset(1).unwrap();
        let selection = Selection::new(&catalog, &other_view, other, start, end).unwrap();
        let stale_view =
            BufferView::narrowed(&catalog, id, ByteSpan::new(start, end).unwrap()).unwrap();
        let mut workspace = VersionedWorkspace::new(catalog);
        let snapshot = workspace.capture();
        let intent = EditIntent::for_actor(ActorId::new(1), Nonce::new(1), snapshot.revision());
        assert_eq!(
            plan(
                intent,
                &view,
                &snapshot,
                vec![
                    EditCommand::Delete {
                        span: selection.range()
                    },
                    EditCommand::DeleteSelection { selection }
                ]
            ),
            Err(MultiCursorError::View(ViewError::BufferMismatch {
                expected: id,
                actual: other
            }))
        );
        workspace.update(id, Text::from_str("界a")).unwrap();
        let snapshot = workspace.capture();
        let intent = EditIntent::for_actor(ActorId::new(1), Nonce::new(1), snapshot.revision());
        assert_eq!(
            plan(
                intent,
                &stale_view,
                &snapshot,
                vec![EditCommand::Insert {
                    at: start,
                    text: Text::empty()
                }]
            ),
            Err(MultiCursorError::View(ViewError::InvalidSpan(
                SpanError::NotCharBoundary { offset: 1 }
            )))
        );
    }

    #[test]
    fn rejects_stale_values_and_invalid_views() {
        let (view, snapshot, intent) = setup("界a");
        let foreign = Text::from_str("aaaaaa");
        let insert = |n| EditCommand::Insert {
            at: foreign.validate_offset(n).unwrap(),
            text: Text::from_str("x"),
        };
        assert_eq!(
            plan(intent, &view, &snapshot, vec![insert(0), insert(1)]),
            Err(MultiCursorError::View(ViewError::InvalidSpan(
                SpanError::NotCharBoundary { offset: 1 }
            )))
        );
        assert!(matches!(
            plan(intent, &view, &snapshot, vec![insert(5)]),
            Err(MultiCursorError::View(ViewError::SpanOutsideVisible { .. }))
        ));
        let narrow = BufferView::narrowed(
            snapshot.catalog(),
            view.buffer(),
            ByteSpan::validate_in(snapshot.catalog().get(view.buffer()).unwrap(), 0, 3).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            plan(intent, &narrow, &snapshot, vec![insert(4)]),
            Err(MultiCursorError::View(ViewError::SpanOutsideVisible { .. }))
        ));
        let wrong = EditIntent::for_actor(ActorId::new(1), Nonce::new(1), Revision::new(1));
        assert!(matches!(
            plan(wrong, &view, &snapshot, vec![]),
            Err(MultiCursorError::Transaction(
                TransactionError::RevisionMismatch { .. }
            ))
        ));
        let empty = plan(intent, &view, &snapshot, vec![]).unwrap();
        assert!(empty.apply_with_inverse(&snapshot).unwrap().is_noop());
    }
}
