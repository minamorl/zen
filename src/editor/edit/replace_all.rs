//! Literal, leftmost-first replacement planning within a buffer view.
//!
//! `ByteSpan`, `ByteOffset`, and `BufferView` are value types, not proofs of
//! origin. Their coordinates are revalidated against the snapshot's target
//! text and the requested span is checked against the view. Values originating
//! elsewhere with valid coordinates cannot be distinguished. A planned patch
//! should be applied to the same text used for planning; coordinates alone do
//! not establish text or snapshot identity.

use std::fmt;

use crate::editor::edit::{
    intent::EditIntent,
    patch::{Patch, PatchError},
    splice::Splice,
    transaction::{Transaction, TransactionError},
};
use crate::editor::text::{
    search::{self, SearchError},
    span::ByteSpan,
    utf8::Text,
};
use crate::editor::workspace::{
    snapshots::WorkspaceSnapshot,
    view::{BufferView, ViewError},
};

/// A replacement plan could not be constructed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplaceAllError {
    /// The view or requested span is invalid for the snapshot.
    View(ViewError),
    /// The pattern or search span is invalid.
    Search(SearchError),
    /// The generated patch could not be normalized.
    Patch(PatchError),
    /// The transaction could not be constructed or its base is stale.
    Transaction(TransactionError),
}

impl fmt::Display for ReplaceAllError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::View(error) => error.fmt(f),
            Self::Search(error) => error.fmt(f),
            Self::Patch(error) => error.fmt(f),
            Self::Transaction(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for ReplaceAllError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::View(error) => error,
            Self::Search(error) => error,
            Self::Patch(error) => error,
            Self::Transaction(error) => error,
        })
    }
}

/// Plans replacements contained in `span`, which must lie in `view`.
///
/// Matches are selected in ascending order without overlap. Adjacent matches
/// become one splice, with their replacements concatenated. Bytes outside the
/// selected matches, including all bytes outside the view, remain unchanged.
pub fn plan_patch(
    snapshot: &WorkspaceSnapshot,
    view: BufferView,
    span: ByteSpan,
    pattern: &str,
    replacement: &str,
) -> Result<Patch, ReplaceAllError> {
    let catalog = snapshot.catalog();
    view.visible_text(catalog).map_err(ReplaceAllError::View)?;
    let text = catalog.get(view.buffer()).expect("validated live buffer");
    span.slice_in(text)
        .map_err(|e| ReplaceAllError::View(ViewError::InvalidSpan(e)))?;
    view.check_target(view.buffer(), span)
        .map_err(ReplaceAllError::View)?;
    let matches = search::find_in_span(text, span, pattern).map_err(ReplaceAllError::Search)?;
    let mut splices = Vec::new();
    let mut run: Option<(ByteSpan, String)> = None;
    for matched in matches {
        if let Some((current, inserted)) = &mut run {
            if matched.start() < current.end() {
                continue;
            }
            if matched.start() == current.end() {
                *current = ByteSpan::new(current.start(), matched.end()).expect("ordered matches");
                inserted.push_str(replacement);
                continue;
            }
        }
        if let Some((span, inserted)) = run.take() {
            splices.push(Splice::new(span, Text::from_str(&inserted)));
        }
        run = Some((matched, replacement.to_owned()));
    }
    if let Some((span, inserted)) = run {
        splices.push(Splice::new(span, Text::from_str(&inserted)));
    }
    Patch::new(splices).map_err(ReplaceAllError::Patch)
}

/// Plans a single-buffer transaction based on the supplied snapshot revision.
pub fn plan_transaction(
    snapshot: &WorkspaceSnapshot,
    view: BufferView,
    span: ByteSpan,
    pattern: &str,
    replacement: &str,
    intent: EditIntent,
) -> Result<Transaction, ReplaceAllError> {
    if intent.base() != snapshot.revision() {
        return Err(ReplaceAllError::Transaction(
            TransactionError::RevisionMismatch {
                expected: intent.base(),
                actual: snapshot.revision(),
            },
        ));
    }
    let patch = plan_patch(snapshot, view, span, pattern, replacement)?;
    Transaction::new(intent, vec![(view.buffer(), patch)]).map_err(ReplaceAllError::Transaction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        edit::ids::{ActorId, BufferId, Nonce},
        test_support::{assert_law, texts},
        text::span::SpanError,
        workspace::{catalog::BufferCatalog, snapshots::VersionedWorkspace, version::Revision},
    };

    fn fixture(source: &str) -> (WorkspaceSnapshot, BufferId) {
        let id = BufferId::new(1);
        let mut catalog = BufferCatalog::new();
        catalog.register(id, Text::from_str(source)).unwrap();
        (VersionedWorkspace::new(catalog).capture(), id)
    }
    fn check(source: &str, start: usize, end: usize, pattern: &str, replacement: &str) {
        let (snapshot, id) = fixture(source);
        let text = snapshot.catalog().get(id).unwrap();
        let span = ByteSpan::validate_in(text, start, end).unwrap();
        let view = BufferView::narrowed(snapshot.catalog(), id, span).unwrap();
        let patch = plan_patch(&snapshot, view, span, pattern, replacement).unwrap();
        let expected = format!(
            "{}{}{}",
            &source[..start],
            source[start..end].replace(pattern, replacement),
            &source[end..]
        );
        let (actual, inverse) = patch.apply_with_inverse(text).unwrap();
        assert_eq!(actual.as_str(), expected);
        assert_eq!(inverse.apply(&actual).unwrap(), *text);
        assert!(patch
            .splices()
            .windows(2)
            .all(|p| p[0].span().end() < p[1].span().start()));
        let intent = EditIntent::for_actor(ActorId::new(1), Nonce::new(2), snapshot.revision());
        let transaction =
            plan_transaction(&snapshot, view, span, pattern, replacement, intent).unwrap();
        assert_eq!(
            transaction
                .apply_with_inverse(&snapshot)
                .unwrap()
                .catalog()
                .get(id)
                .unwrap()
                .as_str(),
            expected
        );
    }

    #[test]
    fn edge_cases() {
        for (source, start, end, pattern, replacement) in [
            ("aaaa", 0, 4, "aa", "x"),
            ("aaaaa", 0, 5, "aa", ""),
            ("zaaabaaaZ", 1, 8, "a", "界"),
            ("😀界界😀", 4, 10, "界", "x"),
            ("abc", 1, 2, "bc", "X"),
            ("abc", 1, 1, "a", "X"),
            ("aXa", 0, 3, "a", ""),
        ] {
            check(source, start, end, pattern, replacement);
        }
        let (snapshot, id) = fixture("aaaa");
        let view = BufferView::full(snapshot.catalog(), id).unwrap();
        let patch = plan_patch(&snapshot, view, view.visible(), "aa", "x").unwrap();
        assert_eq!(patch.splices().len(), 1);
        assert_eq!(patch.splices()[0].replacement().as_str(), "xx");
    }

    #[test]
    fn generated_reference_and_inverse() {
        assert_law(&[0, 1, 42, 999, u64::MAX], texts, |corpus| {
            for source in corpus {
                let boundaries: Vec<_> = source
                    .char_indices()
                    .map(|(i, _)| i)
                    .chain(std::iter::once(source.len()))
                    .collect();
                for &start in boundaries.iter().step_by(3) {
                    for &end in boundaries
                        .iter()
                        .step_by(3)
                        .chain(std::iter::once(&source.len()))
                    {
                        if start > end {
                            continue;
                        }
                        for pattern in ["a", "aa", "界", "😀", "\n", "e\u{301}"] {
                            for replacement in ["", "x", "界"] {
                                check(source, start, end, pattern, replacement);
                            }
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn search_span_is_narrower_than_view() {
        let (snapshot, id) = fixture("😀a😀z😀");
        let text = snapshot.catalog().get(id).unwrap();
        let view = BufferView::full(snapshot.catalog(), id).unwrap();
        let span = ByteSpan::validate_in(text, 5, 9).unwrap();
        let patch = plan_patch(&snapshot, view, span, "😀", "X").unwrap();
        assert_eq!(patch.apply(text).unwrap().as_str(), "😀aXz😀");
        let intent = EditIntent::for_actor(ActorId::new(1), Nonce::new(2), snapshot.revision());
        let transaction = plan_transaction(&snapshot, view, span, "😀", "X", intent).unwrap();
        assert_eq!(
            transaction
                .apply_with_inverse(&snapshot)
                .unwrap()
                .catalog()
                .get(id)
                .unwrap()
                .as_str(),
            "😀aXz😀"
        );
    }

    #[test]
    fn rejects_invalid_values_and_stale_revision() {
        let (snapshot, id) = fixture("a😀z");
        let text = snapshot.catalog().get(id).unwrap();
        let view = BufferView::narrowed(
            snapshot.catalog(),
            id,
            ByteSpan::validate_in(text, 1, 5).unwrap(),
        )
        .unwrap();
        assert_eq!(
            plan_patch(&snapshot, view, view.visible(), "", "x"),
            Err(ReplaceAllError::Search(SearchError::EmptyPattern))
        );
        let outside = ByteSpan::full_of(text);
        assert!(matches!(
            plan_patch(&snapshot, view, outside, "a", ""),
            Err(ReplaceAllError::View(ViewError::SpanOutsideVisible { .. }))
        ));
        let foreign = Text::from_str("abcdefg");
        let invalid = ByteSpan::validate_in(&foreign, 2, 3).unwrap();
        assert_eq!(
            plan_patch(&snapshot, view, invalid, "a", ""),
            Err(ReplaceAllError::View(ViewError::InvalidSpan(
                SpanError::NotCharBoundary { offset: 2 }
            )))
        );
        let stale_view =
            BufferView::narrowed(&fixture("abcdefg").0.catalog().clone(), id, invalid).unwrap();
        assert!(matches!(
            plan_patch(&snapshot, stale_view, invalid, "a", ""),
            Err(ReplaceAllError::View(ViewError::InvalidSpan(_)))
        ));
        let valid = ByteSpan::validate_in(text, 1, 1).unwrap();
        assert!(matches!(
            plan_patch(&snapshot, stale_view, valid, "a", ""),
            Err(ReplaceAllError::View(ViewError::InvalidSpan(_)))
        ));
        let unknown_view = BufferView::full(snapshot.catalog(), id).unwrap();
        let inner = ByteSpan::validate_in(text, 1, 5).unwrap();
        let patch = plan_patch(&snapshot, unknown_view, inner, "😀", "X").unwrap();
        assert_eq!(patch.apply(text).unwrap().as_str(), "aXz");
        let (other, _) = fixture("");
        let intent = EditIntent::for_actor(ActorId::new(1), Nonce::new(1), Revision::new(1));
        assert!(matches!(
            plan_transaction(
                &other,
                unknown_view,
                unknown_view.visible(),
                "a",
                "",
                intent
            ),
            Err(ReplaceAllError::Transaction(
                TransactionError::RevisionMismatch { .. }
            ))
        ));
    }
}
