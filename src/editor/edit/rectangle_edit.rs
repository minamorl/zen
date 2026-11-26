//! Atomic plans for rectangular edits in visual columns.
//!
//! Columns and tab rounding follow `text::rectangle`. Rows are ordered from
//! top to bottom regardless of corner order. No padding is inserted on short
//! lines. Plans refer to the snapshot used to create them; the transaction API
//! checks the revision when evaluating them.

use std::fmt;

use crate::editor::edit::{
    ids::BufferId,
    intent::EditIntent,
    patch::{Patch, PatchError},
    splice::Splice,
    transaction::{Transaction, TransactionError},
};
use crate::editor::text::{
    columns::VisualPosition,
    lines::LineIndex,
    rectangle::{self, Endpoint, RectangleError},
    utf8::Text,
};
use crate::editor::workspace::snapshots::WorkspaceSnapshot;

/// Geometry and display convention for an edit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rectangle {
    pub first: VisualPosition,
    pub second: VisualPosition,
    pub endpoint: Endpoint,
    pub tab_width: usize,
}

/// What to do when a line ends before the requested right edge. Insertion
/// checks the left edge instead, because it does not modify the selected span.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShortLinePolicy {
    /// Use the line end, without adding spaces.
    Clamp,
    /// Reject the entire plan.
    Error,
}

/// Replacements are associated with rows from top to bottom.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RectangleOperation<'a> {
    Delete,
    Replace(&'a [&'a str]),
    Uniform(&'a str),
    /// Insert at the left edge without deleting selected text.
    Insert(&'a str),
}

/// A rectangle could not be planned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RectangleEditError {
    Rectangle(RectangleError),
    ShortLine {
        line: usize,
        width: u128,
        required: u128,
    },
    RowCount {
        expected: usize,
        actual: usize,
    },
    Patch(PatchError),
    Transaction(TransactionError),
}

impl fmt::Display for RectangleEditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rectangle(error) => error.fmt(f),
            Self::ShortLine {
                line,
                width,
                required,
            } => write!(f, "line {line} has width {width}, needs {required}"),
            Self::RowCount { expected, actual } => {
                write!(f, "expected {expected} replacement rows, got {actual}")
            }
            Self::Patch(error) => error.fmt(f),
            Self::Transaction(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for RectangleEditError {}

/// Builds a patch without modifying the input text.
pub fn plan_patch(
    text: &Text,
    rectangle: Rectangle,
    policy: ShortLinePolicy,
    operation: RectangleOperation<'_>,
) -> Result<Patch, RectangleEditError> {
    let Rectangle {
        first,
        second,
        endpoint,
        tab_width,
    } = rectangle;
    // Validate the original rectangle even when insertion uses only its left edge.
    let mut spans = rectangle::rectangle_spans(text, first, second, endpoint, tab_width)
        .map_err(RectangleEditError::Rectangle)?;
    if let RectangleOperation::Replace(rows) = operation {
        if rows.len() != spans.len() {
            return Err(RectangleEditError::RowCount {
                expected: spans.len(),
                actual: rows.len(),
            });
        }
    }
    let left = first.column.min(second.column);
    if policy == ShortLinePolicy::Error {
        let right = if matches!(operation, RectangleOperation::Insert(_)) {
            left
        } else {
            first.column.max(second.column)
        };
        let required = if matches!(operation, RectangleOperation::Insert(_))
            || endpoint == Endpoint::Exclusive
        {
            right as u128
        } else {
            right as u128 + 1
        };
        let index = LineIndex::from_text(text);
        for line in first.line.min(second.line)..=first.line.max(second.line) {
            let span = index.line_span(line).expect("rectangle validated");
            let start = span.start().as_usize();
            let mut end = span.end().as_usize();
            if text.as_bytes().get(end) == Some(&b'\n')
                && end > start
                && text.as_bytes()[end - 1] == b'\r'
            {
                end -= 1;
            }
            let mut width = 0u128;
            for ch in text.as_str()[start..end].chars() {
                if width >= required {
                    break;
                }
                let cells = if ch == '\t' {
                    tab_width as u128 - width % tab_width as u128
                } else {
                    1
                };
                width += cells;
            }
            if width < required {
                return Err(RectangleEditError::ShortLine {
                    line,
                    width,
                    required,
                });
            }
        }
    }
    if matches!(operation, RectangleOperation::Insert(_)) {
        spans = rectangle::rectangle_spans(
            text,
            VisualPosition {
                line: first.line,
                column: left,
            },
            VisualPosition {
                line: second.line,
                column: left,
            },
            Endpoint::Exclusive,
            tab_width,
        )
        .map_err(RectangleEditError::Rectangle)?;
    }
    let splices = spans
        .into_iter()
        .enumerate()
        .map(|(row, span)| {
            let replacement = match operation {
                RectangleOperation::Delete => "",
                RectangleOperation::Replace(rows) => rows[row],
                RectangleOperation::Uniform(value) | RectangleOperation::Insert(value) => value,
            };
            Splice::new(span, Text::from_str(replacement))
        })
        .collect();
    Patch::new(splices).map_err(RectangleEditError::Patch)
}

/// Builds a single-buffer transaction, checking the snapshot and buffer first.
pub fn plan_transaction(
    snapshot: &WorkspaceSnapshot,
    buffer: BufferId,
    rectangle: Rectangle,
    policy: ShortLinePolicy,
    operation: RectangleOperation<'_>,
    intent: EditIntent,
) -> Result<Transaction, RectangleEditError> {
    if intent.base() != snapshot.revision() {
        return Err(RectangleEditError::Transaction(
            TransactionError::RevisionMismatch {
                expected: intent.base(),
                actual: snapshot.revision(),
            },
        ));
    }
    let text = snapshot.catalog().get(buffer).ok_or_else(|| {
        RectangleEditError::Transaction(if snapshot.catalog().is_tombstoned(buffer) {
            TransactionError::BufferClosed(buffer)
        } else {
            TransactionError::UnknownBuffer(buffer)
        })
    })?;
    let patch = plan_patch(text, rectangle, policy, operation)?;
    Transaction::new(intent, vec![(buffer, patch)]).map_err(RectangleEditError::Transaction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        edit::{
            ids::{ActorId, Nonce},
            workspace_edit::WorkspaceEdit,
        },
        test_support::{assert_law, texts},
        workspace::{catalog::BufferCatalog, snapshots::VersionedWorkspace, version::Revision},
    };

    fn rectangle(last: usize, left: usize, right: usize, endpoint: Endpoint) -> Rectangle {
        Rectangle {
            first: VisualPosition {
                line: 0,
                column: left,
            },
            second: VisualPosition {
                line: last,
                column: right,
            },
            endpoint,
            tab_width: 4,
        }
    }

    #[test]
    fn generated_composition_corners_and_inverse() {
        assert_law(&[0, 1, 42, 999, u64::MAX], texts, |corpus| {
            for source in corpus
                .iter()
                .map(String::as_str)
                .chain(["\t界😀\r\n\nq\n", "a\nb\r\n\t"])
            {
                let mut catalog = BufferCatalog::new();
                let id = BufferId::new(1);
                catalog.register(id, Text::from_str(source)).unwrap();
                let workspace = VersionedWorkspace::new(catalog);
                let snapshot = workspace.capture();
                let text = snapshot.catalog().get(id).unwrap();
                let last = LineIndex::from_text(text).line_count() - 1;
                let actor = ActorId::new(1);
                let nonce = Nonce::new(2);
                let intent = EditIntent::for_actor(actor, nonce, snapshot.revision());
                for endpoint in [Endpoint::Exclusive, Endpoint::Inclusive] {
                    for (left, right) in [(0, 0), (0, 2), (1, 4), (7, 9)] {
                        let rect = rectangle(last, left, right, endpoint);
                        let reversed = Rectangle {
                            first: rect.second,
                            second: rect.first,
                            ..rect
                        };
                        let values: Vec<_> = (0..=last).map(|i| format!("界😀{i}")).collect();
                        let rows: Vec<_> = values.iter().map(String::as_str).collect();
                        for op in [
                            RectangleOperation::Delete,
                            RectangleOperation::Replace(&rows),
                            RectangleOperation::Uniform("x"),
                            RectangleOperation::Insert("!"),
                        ] {
                            let patch = plan_patch(text, rect, ShortLinePolicy::Clamp, op).unwrap();
                            assert_eq!(
                                patch,
                                plan_patch(text, reversed, ShortLinePolicy::Clamp, op).unwrap()
                            );
                            let txn = plan_transaction(
                                &snapshot,
                                id,
                                rect,
                                ShortLinePolicy::Clamp,
                                op,
                                intent,
                            )
                            .unwrap();
                            assert_eq!(txn.patches(), &[(id, patch.clone())]);
                            let mut composed = WorkspaceEdit::empty(actor, snapshot.revision());
                            let mut sequential = text.clone();
                            for line in (0..=last).rev() {
                                let single_rect = Rectangle {
                                    first: VisualPosition { line, column: left },
                                    second: VisualPosition {
                                        line,
                                        column: right,
                                    },
                                    ..rect
                                };
                                let (a, b, mode) = if matches!(op, RectangleOperation::Insert(_)) {
                                    (
                                        single_rect.first,
                                        VisualPosition { line, column: left },
                                        Endpoint::Exclusive,
                                    )
                                } else {
                                    (single_rect.first, single_rect.second, endpoint)
                                };
                                let span =
                                    rectangle::rectangle_spans(text, a, b, mode, 4).unwrap()[0];
                                let replacement = match op {
                                    RectangleOperation::Delete => "",
                                    RectangleOperation::Replace(_) => rows[line],
                                    RectangleOperation::Uniform(value)
                                    | RectangleOperation::Insert(value) => value,
                                };
                                let single = Patch::new(vec![Splice::new(
                                    span,
                                    Text::from_str(replacement),
                                )])
                                .unwrap();
                                sequential = single.apply(&sequential).unwrap();
                                let part = WorkspaceEdit::new(
                                    actor,
                                    snapshot.revision(),
                                    vec![(id, single)],
                                    &snapshot,
                                )
                                .unwrap();
                                composed = composed.compose(&part).unwrap();
                            }
                            assert_eq!(composed.to_transaction(nonce, &snapshot).unwrap(), txn);
                            let result = txn.apply_with_inverse(&snapshot).unwrap();
                            assert_eq!(result.catalog().get(id).unwrap(), &sequential);
                            assert_eq!(
                                result.inverse().apply_to_catalog(result.catalog()).unwrap(),
                                *snapshot.catalog()
                            );
                            assert_eq!(workspace.capture(), snapshot);
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn errors_are_atomic() {
        let id = BufferId::new(1);
        let closed = BufferId::new(2);
        let mut catalog = BufferCatalog::new();
        catalog
            .register(id, Text::from_str("abcdef\r\nx\n"))
            .unwrap();
        catalog.register(closed, Text::from_str("old")).unwrap();
        catalog.remove(closed).unwrap();
        let workspace = VersionedWorkspace::new(catalog);
        let snapshot = workspace.capture();
        let intent = EditIntent::for_actor(ActorId::new(1), Nonce::new(1), snapshot.revision());
        let rect = rectangle(2, 1, 3, Endpoint::Exclusive);
        for (buffer, geometry, policy, op, request) in [
            (
                id,
                rect,
                ShortLinePolicy::Error,
                RectangleOperation::Delete,
                intent,
            ),
            (
                id,
                rect,
                ShortLinePolicy::Clamp,
                RectangleOperation::Replace(&["x"]),
                intent,
            ),
            (
                id,
                rectangle(3, 0, 1, Endpoint::Exclusive),
                ShortLinePolicy::Clamp,
                RectangleOperation::Delete,
                intent,
            ),
            (
                closed,
                rect,
                ShortLinePolicy::Clamp,
                RectangleOperation::Delete,
                intent,
            ),
            (
                BufferId::new(3),
                rect,
                ShortLinePolicy::Clamp,
                RectangleOperation::Delete,
                intent,
            ),
            (
                id,
                rect,
                ShortLinePolicy::Clamp,
                RectangleOperation::Delete,
                intent.with_base(Revision::new(1)),
            ),
        ] {
            assert!(plan_transaction(&snapshot, buffer, geometry, policy, op, request).is_err());
            assert_eq!(workspace.capture(), snapshot);
        }
        assert!(matches!(
            plan_patch(
                snapshot.catalog().get(id).unwrap(),
                rect,
                ShortLinePolicy::Error,
                RectangleOperation::Delete
            ),
            Err(RectangleEditError::ShortLine { line: 1, .. })
        ));
        assert!(matches!(
            plan_patch(
                snapshot.catalog().get(id).unwrap(),
                rect,
                ShortLinePolicy::Clamp,
                RectangleOperation::Replace(&["x"])
            ),
            Err(RectangleEditError::RowCount {
                expected: 3,
                actual: 1
            })
        ));
    }

    #[test]
    fn generated_errors_and_stale_workspace() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let id = BufferId::new(1);
                let closed = BufferId::new(2);
                let mut catalog = BufferCatalog::new();
                catalog.register(id, Text::from_str(source)).unwrap();
                catalog.register(closed, Text::from_str("closed")).unwrap();
                catalog.remove(closed).unwrap();
                let mut workspace = VersionedWorkspace::new(catalog);
                let original = workspace.capture();
                let intent =
                    EditIntent::for_actor(ActorId::new(1), Nonce::new(1), original.revision());
                let last =
                    LineIndex::from_text(original.catalog().get(id).unwrap()).line_count() - 1;
                let rect = rectangle(last, 0, 0, Endpoint::Exclusive);
                let missing: &[&str] = &[];
                let extra = vec!["x"; last + 2];
                assert_eq!(
                    plan_transaction(
                        &original,
                        id,
                        rect,
                        ShortLinePolicy::Clamp,
                        RectangleOperation::Replace(&extra),
                        intent
                    ),
                    Err(RectangleEditError::RowCount {
                        expected: last + 1,
                        actual: last + 2
                    })
                );
                assert!(matches!(
                    plan_transaction(
                        &original,
                        id,
                        rect,
                        ShortLinePolicy::Clamp,
                        RectangleOperation::Replace(missing),
                        intent
                    ),
                    Err(RectangleEditError::RowCount { .. })
                ));
                assert!(matches!(
                    plan_transaction(
                        &original,
                        id,
                        rectangle(last + 1, 0, 0, Endpoint::Exclusive),
                        ShortLinePolicy::Clamp,
                        RectangleOperation::Delete,
                        intent
                    ),
                    Err(RectangleEditError::Rectangle(_))
                ));
                assert_eq!(
                    plan_transaction(
                        &original,
                        closed,
                        rect,
                        ShortLinePolicy::Clamp,
                        RectangleOperation::Delete,
                        intent
                    ),
                    Err(RectangleEditError::Transaction(
                        TransactionError::BufferClosed(closed)
                    ))
                );
                let unknown = BufferId::new(3);
                assert_eq!(
                    plan_transaction(
                        &original,
                        unknown,
                        rect,
                        ShortLinePolicy::Clamp,
                        RectangleOperation::Delete,
                        intent
                    ),
                    Err(RectangleEditError::Transaction(
                        TransactionError::UnknownBuffer(unknown)
                    ))
                );
                let short = rectangle(last, usize::MAX, usize::MAX, Endpoint::Exclusive);
                assert!(matches!(
                    plan_transaction(
                        &original,
                        id,
                        short,
                        ShortLinePolicy::Error,
                        RectangleOperation::Delete,
                        intent
                    ),
                    Err(RectangleEditError::ShortLine { .. })
                ));
                assert_eq!(workspace.capture(), original);
                let txn = plan_transaction(
                    &original,
                    id,
                    rect,
                    ShortLinePolicy::Clamp,
                    RectangleOperation::Insert("x"),
                    intent,
                )
                .unwrap();
                workspace.update(id, Text::from_str("new\ntext")).unwrap();
                let current = workspace.capture();
                assert!(matches!(
                    txn.apply_with_inverse(&current),
                    Err(TransactionError::RevisionMismatch { .. })
                ));
                assert!(matches!(
                    plan_transaction(
                        &current,
                        id,
                        rect,
                        ShortLinePolicy::Clamp,
                        RectangleOperation::Delete,
                        intent
                    ),
                    Err(RectangleEditError::Transaction(
                        TransactionError::RevisionMismatch { .. }
                    ))
                ));
                assert_eq!(workspace.capture(), current);
                assert_eq!(original.catalog().get(id).unwrap().as_str(), source);
            }
        });
    }

    #[test]
    fn huge_tab_does_not_overflow_short_line_check() {
        let text = Text::from_str("\tX");
        let inclusive = Rectangle {
            tab_width: usize::MAX,
            ..rectangle(0, usize::MAX, usize::MAX, Endpoint::Inclusive)
        };
        assert_eq!(
            plan_patch(
                &text,
                inclusive,
                ShortLinePolicy::Error,
                RectangleOperation::Delete
            )
            .unwrap()
            .apply(&text)
            .unwrap()
            .as_str(),
            "\t"
        );
        let rect = Rectangle {
            tab_width: usize::MAX,
            ..rectangle(0, 0, 1, Endpoint::Exclusive)
        };
        assert_eq!(
            plan_patch(
                &text,
                rect,
                ShortLinePolicy::Error,
                RectangleOperation::Delete
            ),
            plan_patch(
                &text,
                rect,
                ShortLinePolicy::Clamp,
                RectangleOperation::Delete
            )
        );
    }

    #[test]
    fn insertion_checks_left_edge() {
        let text = Text::from_str("a\r\nb");
        let rect = rectangle(1, 1, 3, Endpoint::Exclusive);
        assert!(matches!(
            plan_patch(
                &text,
                rect,
                ShortLinePolicy::Error,
                RectangleOperation::Delete
            ),
            Err(RectangleEditError::ShortLine {
                line: 0,
                width: 1,
                required: 3
            })
        ));
        assert_eq!(
            plan_patch(
                &text,
                rect,
                ShortLinePolicy::Error,
                RectangleOperation::Insert("!")
            )
            .unwrap()
            .apply(&text)
            .unwrap()
            .as_str(),
            "a!\r\nb!"
        );
        let beyond = rectangle(1, 2, 3, Endpoint::Exclusive);
        assert!(matches!(
            plan_patch(
                &text,
                beyond,
                ShortLinePolicy::Error,
                RectangleOperation::Insert("!")
            ),
            Err(RectangleEditError::ShortLine {
                line: 0,
                width: 1,
                required: 2
            })
        ));
    }

    #[test]
    fn generated_interior_rows_compose() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for source in corpus {
                let text = Text::from_str(&format!("outside\n{source}\nlast\noutside"));
                let last = LineIndex::from_text(&text).line_count() - 2;
                let rect = Rectangle {
                    first: VisualPosition { line: 1, column: 0 },
                    second: VisualPosition {
                        line: last,
                        column: 2,
                    },
                    endpoint: Endpoint::Exclusive,
                    tab_width: 4,
                };
                let values: Vec<_> = (1..=last).map(|line| format!("界😀{line}")).collect();
                let rows: Vec<_> = values.iter().map(String::as_str).collect();
                let patch = plan_patch(
                    &text,
                    rect,
                    ShortLinePolicy::Clamp,
                    RectangleOperation::Replace(&rows),
                )
                .unwrap();
                let mut expected = Vec::new();
                for (row, line) in (1..=last).enumerate() {
                    let a = VisualPosition { line, column: 0 };
                    let b = VisualPosition { line, column: 2 };
                    let span =
                        rectangle::rectangle_spans(&text, a, b, Endpoint::Exclusive, 4).unwrap()[0];
                    expected.push(Splice::new(span, Text::from_str(rows[row])));
                }
                assert_eq!(patch, Patch::new(expected).unwrap());
                let reversed = Rectangle {
                    first: rect.second,
                    second: rect.first,
                    ..rect
                };
                assert_eq!(
                    patch,
                    plan_patch(
                        &text,
                        reversed,
                        ShortLinePolicy::Clamp,
                        RectangleOperation::Replace(&rows)
                    )
                    .unwrap()
                );
                let (next, inverse) = patch.apply_with_inverse(&text).unwrap();
                assert_eq!(inverse.apply(&next).unwrap(), text);
            }
        });
    }
}
