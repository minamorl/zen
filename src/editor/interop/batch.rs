//! Decode external edits against one immutable workspace snapshot.
//!
//! # Laws
//!
//! Equivalent coordinates decode to the same byte boundaries through `region`,
//! so equivalent edits yield identical splices, patches and transactions.
//! Success requires every edit to decode; an invalid coordinate,
//! revision, buffer or contacting pair returns a typed error instead of a
//! partial result. No input is mutated. Patches sort strictly separated spans
//! and transactions sort buffer IDs, so permutations have the same result.
//! Each patch inverse restores its base bytes, and independent buffers cannot
//! interfere; the transaction inverse consequently restores the base catalog.
//! Inclusive endpoints include the upper scalar, saturating at EOF, as in
//! [`OrientedRegion`]. Empty batches produce an empty transaction.

use std::collections::BTreeMap;

use super::error::{BoundaryError, ErrorCode};
use super::region::OrientedRegion;
use crate::editor::edit::{
    ids::BufferId, intent::EditIntent, patch::Patch, splice::Splice, transaction::Transaction,
};
use crate::editor::text::utf8::Text;
use crate::editor::workspace::{snapshots::WorkspaceSnapshot, view::BufferView};

/// One replacement, with its own asserted base revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalEdit {
    /// Target buffer.
    pub buffer: BufferId,
    /// The edit's base revision; its transaction identity is not used.
    pub intent: EditIntent,
    /// Absolute endpoints in the base buffer.
    pub region: OrientedRegion,
    /// Replacement text.
    pub replacement: Text,
}

/// Compiles a batch into a canonical transaction without mutating the snapshot.
///
/// All edit and transaction base revisions must match the snapshot. Contacting
/// spans, including touching insertions, return [`ErrorCode::Conflict`].
/// The resulting per-buffer patches are available through `Transaction::patches`.
pub fn decode_batch(
    snapshot: &WorkspaceSnapshot,
    intent: EditIntent,
    edits: &[ExternalEdit],
) -> Result<Transaction, BoundaryError> {
    if intent.base() != snapshot.revision()
        || edits
            .iter()
            .any(|edit| edit.intent.base() != snapshot.revision())
    {
        return Err(BoundaryError::new(
            ErrorCode::RevisionMismatch,
            "batch revision does not match snapshot",
        ));
    }
    let mut buffers: BTreeMap<BufferId, Vec<Splice>> = BTreeMap::new();
    for edit in edits {
        let catalog = snapshot.catalog();
        let view = BufferView::full(catalog, edit.buffer).map_err(|error| {
            let code = if catalog.is_tombstoned(edit.buffer) {
                ErrorCode::BufferClosed
            } else {
                ErrorCode::UnknownBuffer
            };
            BoundaryError::new(code, error.to_string())
        })?;
        let selection = edit.region.to_selection(catalog, &view, edit.buffer)?;
        buffers
            .entry(edit.buffer)
            .or_default()
            .push(Splice::new(selection.range(), edit.replacement.clone()));
    }
    let mut patches = Vec::with_capacity(buffers.len());
    for (buffer, splices) in buffers {
        let patch = Patch::new(splices)
            .map_err(|error| BoundaryError::new(ErrorCode::Conflict, error.to_string()))?;
        patches.push((buffer, patch));
    }
    Ok(Transaction::new(intent, patches).expect("buffer map contains distinct IDs"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::edit::ids::{ActorId, Nonce};
    use crate::editor::interop::agent_position::AgentPosition;
    use crate::editor::interop::coordinates::{
        BytePos, CoordKind, EmacsCharPos, LineColByte, LineColUtf16,
    };
    use crate::editor::interop::region::{EndpointMode, RegionCoord};
    use crate::editor::text::span::ByteSpan;
    use crate::editor::workspace::{
        catalog::BufferCatalog,
        selections::{Direction, Selection},
        snapshots::VersionedWorkspace,
    };

    fn fixture(source: &str) -> (WorkspaceSnapshot, EditIntent) {
        let mut catalog = BufferCatalog::new();
        for id in [1, 2] {
            catalog
                .register(BufferId::new(id), Text::from_str(source))
                .unwrap();
        }
        let snapshot = VersionedWorkspace::new(catalog).capture();
        let intent = EditIntent::for_actor(ActorId::new(1), Nonce::new(2), snapshot.revision());
        (snapshot, intent)
    }

    fn edit(
        snapshot: &WorkspaceSnapshot,
        intent: EditIntent,
        id: u64,
        start: usize,
        end: usize,
        kind: CoordKind,
        mode: EndpointMode,
    ) -> ExternalEdit {
        let buffer = BufferId::new(id);
        let catalog = snapshot.catalog();
        let text = catalog.get(buffer).unwrap();
        let view = BufferView::full(catalog, buffer).unwrap();
        let selection = Selection::new(
            catalog,
            &view,
            buffer,
            text.validate_offset(start).unwrap(),
            text.validate_offset(end).unwrap(),
        )
        .unwrap();
        ExternalEdit {
            buffer,
            intent,
            region: OrientedRegion::from_selection(catalog, &view, selection, kind, mode).unwrap(),
            replacement: Text::from_str("界"),
        }
    }

    #[test]
    fn distinct_buffers_use_their_own_text() {
        let mut catalog = BufferCatalog::new();
        catalog
            .register(BufferId::new(1), Text::from_str("a😀z"))
            .unwrap();
        catalog
            .register(BufferId::new(2), Text::from_str("😀ab"))
            .unwrap();
        let snapshot = VersionedWorkspace::new(catalog).capture();
        let intent = EditIntent::for_actor(ActorId::new(1), Nonce::new(2), snapshot.revision());
        let first = edit(
            &snapshot,
            intent,
            1,
            1,
            5,
            CoordKind::EmacsChar,
            EndpointMode::Inclusive,
        );
        let second = edit(
            &snapshot,
            intent,
            2,
            4,
            5,
            CoordKind::LineColUtf16,
            EndpointMode::Exclusive,
        );
        let mut patches = Vec::new();
        for (id, start, end) in [(1, 1, 5), (2, 4, 5)] {
            let text = snapshot.catalog().get(BufferId::new(id)).unwrap();
            let span = ByteSpan::validate_in(text, start, end).unwrap();
            patches.push((
                BufferId::new(id),
                Patch::new(vec![Splice::new(span, Text::from_str("界"))]).unwrap(),
            ));
        }
        let expected = Transaction::new(intent, patches).unwrap();
        for edits in [
            [first.clone(), second.clone()],
            [second.clone(), first.clone()],
        ] {
            let tx = decode_batch(&snapshot, intent, &edits).unwrap();
            assert_eq!(tx, expected);
            let result = tx.apply_with_inverse(&snapshot).unwrap();
            assert_eq!(
                result.catalog().get(BufferId::new(1)).unwrap().as_str(),
                "a界z"
            );
            assert_eq!(
                result.catalog().get(BufferId::new(2)).unwrap().as_str(),
                "😀界b"
            );
            assert_eq!(
                result.inverse().apply_to_catalog(result.catalog()).unwrap(),
                *snapshot.catalog()
            );
        }
    }

    #[test]
    fn stale_revision_is_rejected_against_updated_snapshot() {
        let mut catalog = BufferCatalog::new();
        let buffer = BufferId::new(1);
        catalog.register(buffer, Text::from_str("abc")).unwrap();
        let mut workspace = VersionedWorkspace::new(catalog);
        let old = workspace.capture();
        workspace.update(buffer, Text::from_str("abc")).unwrap();
        let snapshot = workspace.capture();
        let intent = EditIntent::for_actor(ActorId::new(1), Nonce::new(2), snapshot.revision());
        let current = edit(
            &snapshot,
            intent,
            1,
            1,
            2,
            CoordKind::Byte,
            EndpointMode::Exclusive,
        );
        let result = decode_batch(&snapshot, intent, std::slice::from_ref(&current))
            .unwrap()
            .apply_with_inverse(&snapshot)
            .unwrap();
        assert_eq!(result.catalog().get(buffer).unwrap().as_str(), "a界c");
        assert_eq!(
            result.inverse().apply_to_catalog(result.catalog()).unwrap(),
            *snapshot.catalog()
        );
        let stale_intent = intent.with_base(old.revision());
        let mut stale = current.clone();
        stale.intent = stale_intent;
        for edits in [
            [current.clone(), stale.clone()],
            [stale.clone(), current.clone()],
        ] {
            assert_eq!(
                decode_batch(&snapshot, intent, &edits).unwrap_err().code(),
                ErrorCode::RevisionMismatch
            );
        }
        assert_eq!(
            decode_batch(&snapshot, stale_intent, &[current])
                .unwrap_err()
                .code(),
            ErrorCode::RevisionMismatch
        );
    }

    #[test]
    fn four_disjoint_insertions_all_permutations() {
        let (snapshot, intent) = fixture("abc");
        let text = snapshot.catalog().get(BufferId::new(1)).unwrap();
        let replacements = ["A", "B", "C", "D"];
        let edits: Vec<_> = (0..4)
            .map(|i| {
                let mut item = edit(
                    &snapshot,
                    intent,
                    1,
                    i,
                    i,
                    CoordKind::Byte,
                    EndpointMode::Exclusive,
                );
                item.replacement = Text::from_str(replacements[i]);
                item
            })
            .collect();
        let patch = Patch::new(
            (0..4)
                .map(|i| {
                    Splice::new(
                        ByteSpan::validate_in(text, i, i).unwrap(),
                        Text::from_str(replacements[i]),
                    )
                })
                .collect(),
        )
        .unwrap();
        let expected = Transaction::new(intent, vec![(BufferId::new(1), patch)]).unwrap();
        let mut count = 0;
        for a in 0..4 {
            for b in 0..4 {
                for c in 0..4 {
                    for d in 0..4 {
                        if a == b || a == c || a == d || b == c || b == d || c == d {
                            continue;
                        }
                        let tx = decode_batch(
                            &snapshot,
                            intent,
                            &[
                                edits[a].clone(),
                                edits[b].clone(),
                                edits[c].clone(),
                                edits[d].clone(),
                            ],
                        )
                        .unwrap();
                        assert_eq!(tx, expected);
                        let result = tx.apply_with_inverse(&snapshot).unwrap();
                        assert_eq!(
                            result.catalog().get(BufferId::new(1)).unwrap().as_str(),
                            "AaBbCcD"
                        );
                        assert_eq!(
                            result.inverse().apply_to_catalog(result.catalog()).unwrap(),
                            *snapshot.catalog()
                        );
                        count += 1;
                    }
                }
            }
        }
        assert_eq!(count, 24);
    }

    #[test]
    fn three_disjoint_ranges_all_permutations() {
        for source in ["abc", "a界😀b", "e\u{301}\nx"] {
            let (snapshot, intent) = fixture(source);
            let text = snapshot.catalog().get(BufferId::new(1)).unwrap();
            let offsets = text.char_boundaries();
            let spans: Vec<_> = offsets
                .iter()
                .enumerate()
                .flat_map(|(i, &start)| {
                    offsets[i..]
                        .iter()
                        .map(move |&end| ByteSpan::new(start, end).unwrap())
                })
                .collect();
            for &a in &spans {
                for &b in &spans {
                    for &c in &spans {
                        if a.end() >= b.start() || b.end() >= c.start() {
                            continue;
                        }
                        let ranges = [a, b, c];
                        let replacements = ["X", "界", "😀"];
                        let expected = Transaction::new(
                            intent,
                            vec![(
                                BufferId::new(1),
                                Patch::new(
                                    (0..3)
                                        .map(|i| {
                                            Splice::new(ranges[i], Text::from_str(replacements[i]))
                                        })
                                        .collect(),
                                )
                                .unwrap(),
                            )],
                        )
                        .unwrap();
                        let mut expected_text = String::new();
                        let mut cursor = 0;
                        for i in 0..3 {
                            expected_text.push_str(&source[cursor..ranges[i].start().as_usize()]);
                            expected_text.push_str(replacements[i]);
                            cursor = ranges[i].end().as_usize();
                        }
                        expected_text.push_str(&source[cursor..]);
                        let edits: Vec<_> = (0..3)
                            .map(|i| {
                                let mut e = edit(
                                    &snapshot,
                                    intent,
                                    1,
                                    ranges[i].start().as_usize(),
                                    ranges[i].end().as_usize(),
                                    CoordKind::Byte,
                                    EndpointMode::Exclusive,
                                );
                                e.replacement = Text::from_str(replacements[i]);
                                e
                            })
                            .collect();
                        for order in [
                            [0, 1, 2],
                            [0, 2, 1],
                            [1, 0, 2],
                            [1, 2, 0],
                            [2, 0, 1],
                            [2, 1, 0],
                        ] {
                            let tx =
                                decode_batch(&snapshot, intent, &order.map(|i| edits[i].clone()))
                                    .unwrap();
                            assert_eq!(tx, expected);
                            let result = tx.apply_with_inverse(&snapshot).unwrap();
                            assert_eq!(
                                result.catalog().get(BufferId::new(1)).unwrap().as_str(),
                                expected_text
                            );
                            assert_eq!(
                                result.inverse().apply_to_catalog(result.catalog()).unwrap(),
                                *snapshot.catalog()
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn equivalent_disjoint_spans_and_permutations_roundtrip() {
        let kinds = [
            CoordKind::EmacsChar,
            CoordKind::LineColByte,
            CoordKind::Byte,
            CoordKind::LineColUtf16,
        ];
        for source in ["", "a", "abc", "界a😀", "e\u{301}\n😀x", "\r\n"] {
            let (snapshot, intent) = fixture(source);
            let offsets = snapshot
                .catalog()
                .get(BufferId::new(1))
                .unwrap()
                .char_boundaries();
            for &start in &offsets {
                for &end in offsets.iter().filter(|&&end| end >= start) {
                    for &second in offsets.iter().filter(|&&second| second > end) {
                        for &last in offsets.iter().filter(|&&last| last >= second) {
                            let a = edit(
                                &snapshot,
                                intent,
                                1,
                                start.as_usize(),
                                end.as_usize(),
                                kinds[0],
                                EndpointMode::Exclusive,
                            );
                            let b = edit(
                                &snapshot,
                                intent,
                                1,
                                second.as_usize(),
                                last.as_usize(),
                                kinds[0],
                                EndpointMode::Exclusive,
                            );
                            let c = edit(
                                &snapshot,
                                intent,
                                2,
                                start.as_usize(),
                                end.as_usize(),
                                kinds[0],
                                EndpointMode::Exclusive,
                            );
                            let text = snapshot.catalog().get(BufferId::new(1)).unwrap();
                            let span = ByteSpan::new(start, end).unwrap();
                            let point = ByteSpan::new(second, last).unwrap();
                            let expected = Transaction::new(
                                intent,
                                vec![
                                    (
                                        BufferId::new(1),
                                        Patch::new(vec![
                                            Splice::new(span, Text::from_str("界")),
                                            Splice::new(point, Text::from_str("界")),
                                        ])
                                        .unwrap(),
                                    ),
                                    (
                                        BufferId::new(2),
                                        Patch::new(vec![Splice::new(span, Text::from_str("界"))])
                                            .unwrap(),
                                    ),
                                ],
                            )
                            .unwrap();
                            assert_eq!(
                                decode_batch(&snapshot, intent, &[a, b.clone(), c.clone()])
                                    .unwrap(),
                                expected
                            );
                            let expected_first = format!(
                                "{}界{}界{}",
                                &source[..start.as_usize()],
                                &source[end.as_usize()..second.as_usize()],
                                &source[last.as_usize()..]
                            );
                            let expected_second = format!(
                                "{}界{}",
                                &source[..start.as_usize()],
                                &source[end.as_usize()..]
                            );
                            assert_eq!(text.as_str(), source);
                            for &first_kind in &kinds {
                                for first_mode in [EndpointMode::Exclusive, EndpointMode::Inclusive]
                                {
                                    if first_mode == EndpointMode::Inclusive
                                        && start == end
                                        && end.as_usize() != source.len()
                                    {
                                        continue;
                                    }
                                    for &second_kind in &kinds {
                                        for second_mode in
                                            [EndpointMode::Exclusive, EndpointMode::Inclusive]
                                        {
                                            if second_mode == EndpointMode::Inclusive
                                                && second == last
                                                && last.as_usize() != source.len()
                                            {
                                                continue;
                                            }
                                            let first = edit(
                                                &snapshot,
                                                intent,
                                                1,
                                                start.as_usize(),
                                                end.as_usize(),
                                                first_kind,
                                                first_mode,
                                            );
                                            let next = edit(
                                                &snapshot,
                                                intent,
                                                1,
                                                second.as_usize(),
                                                last.as_usize(),
                                                second_kind,
                                                second_mode,
                                            );
                                            assert_eq!(
                                                decode_batch(
                                                    &snapshot,
                                                    intent,
                                                    &[first, next, c.clone()]
                                                )
                                                .unwrap(),
                                                expected
                                            );
                                        }
                                    }
                                }
                            }
                            for &kind in &kinds {
                                for mode in [EndpointMode::Exclusive, EndpointMode::Inclusive] {
                                    if mode == EndpointMode::Inclusive
                                        && start == end
                                        && end.as_usize() != source.len()
                                    {
                                        continue;
                                    }
                                    let first = edit(
                                        &snapshot,
                                        intent,
                                        1,
                                        start.as_usize(),
                                        end.as_usize(),
                                        kind,
                                        mode,
                                    );
                                    let b_kind = edit(
                                        &snapshot,
                                        intent,
                                        1,
                                        second.as_usize(),
                                        last.as_usize(),
                                        kind,
                                        EndpointMode::Exclusive,
                                    );
                                    let c_kind = edit(
                                        &snapshot,
                                        intent,
                                        2,
                                        start.as_usize(),
                                        end.as_usize(),
                                        kind,
                                        mode,
                                    );
                                    for edits in [
                                        [first.clone(), b_kind, c_kind],
                                        [first.clone(), b.clone(), c.clone()],
                                    ] {
                                        for permutation in [
                                            [0, 1, 2],
                                            [0, 2, 1],
                                            [1, 0, 2],
                                            [1, 2, 0],
                                            [2, 0, 1],
                                            [2, 1, 0],
                                        ] {
                                            let actual = decode_batch(
                                                &snapshot,
                                                intent,
                                                &permutation.map(|i| edits[i].clone()),
                                            )
                                            .unwrap();
                                            assert_eq!(actual, expected);
                                            assert_eq!(actual.patches(), expected.patches());
                                            let result =
                                                actual.apply_with_inverse(&snapshot).unwrap();
                                            assert_eq!(
                                                result
                                                    .catalog()
                                                    .get(BufferId::new(1))
                                                    .unwrap()
                                                    .as_str(),
                                                expected_first
                                            );
                                            assert_eq!(
                                                result
                                                    .catalog()
                                                    .get(BufferId::new(2))
                                                    .unwrap()
                                                    .as_str(),
                                                expected_second
                                            );
                                            assert_eq!(
                                                result
                                                    .inverse()
                                                    .apply_to_catalog(result.catalog())
                                                    .unwrap(),
                                                *snapshot.catalog()
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let (snapshot, intent) = fixture("");
        let insertion = edit(
            &snapshot,
            intent,
            1,
            0,
            0,
            CoordKind::Byte,
            EndpointMode::Exclusive,
        );
        let result = decode_batch(&snapshot, intent, &[insertion])
            .unwrap()
            .apply_with_inverse(&snapshot)
            .unwrap();
        assert_eq!(
            result.catalog().get(BufferId::new(1)).unwrap().as_str(),
            "界"
        );
        assert_eq!(
            result.inverse().apply_to_catalog(result.catalog()).unwrap(),
            *snapshot.catalog()
        );
    }

    #[test]
    fn independent_encodings_and_replacements() {
        let (snapshot, intent) = fixture("a😀z");
        let kinds = [
            CoordKind::EmacsChar,
            CoordKind::LineColByte,
            CoordKind::Byte,
            CoordKind::LineColUtf16,
        ];
        let mut forms = Vec::new();
        for &kind in &kinds {
            for mode in [EndpointMode::Exclusive, EndpointMode::Inclusive] {
                forms.push((kind, mode));
            }
        }
        let make = |id, start, end, (kind, mode), replacement| {
            let mut result = edit(&snapshot, intent, id, start, end, kind, mode);
            result.replacement = Text::from_str(replacement);
            result
        };
        let expected = decode_batch(
            &snapshot,
            intent,
            &[
                make(1, 0, 1, forms[0], "A"),
                make(1, 5, 6, forms[0], "B"),
                make(2, 1, 5, forms[0], "C"),
            ],
        )
        .unwrap();
        for &a in &forms {
            for &b in &forms {
                for &c in &forms {
                    let edits = [
                        make(1, 0, 1, a, "A"),
                        make(1, 5, 6, b, "B"),
                        make(2, 1, 5, c, "C"),
                    ];
                    for order in [
                        [0, 1, 2],
                        [0, 2, 1],
                        [1, 0, 2],
                        [1, 2, 0],
                        [2, 0, 1],
                        [2, 1, 0],
                    ] {
                        let tx = decode_batch(&snapshot, intent, &order.map(|i| edits[i].clone()))
                            .unwrap();
                        assert_eq!(tx, expected);
                        let result = tx.apply_with_inverse(&snapshot).unwrap();
                        assert_eq!(
                            result.catalog().get(BufferId::new(1)).unwrap().as_str(),
                            "A😀B"
                        );
                        assert_eq!(
                            result.catalog().get(BufferId::new(2)).unwrap().as_str(),
                            "aCz"
                        );
                        assert_eq!(
                            result.inverse().apply_to_catalog(result.catalog()).unwrap(),
                            *snapshot.catalog()
                        );
                    }
                }
            }
        }
        let (empty, intent) = fixture("");
        for (kind, mode) in forms {
            let item = edit(&empty, intent, 1, 0, 0, kind, mode);
            let result = decode_batch(&empty, intent, &[item])
                .unwrap()
                .apply_with_inverse(&empty)
                .unwrap();
            assert_eq!(
                result.catalog().get(BufferId::new(1)).unwrap().as_str(),
                "界"
            );
            assert_eq!(
                result.inverse().apply_to_catalog(result.catalog()).unwrap(),
                *empty.catalog()
            );
        }
    }

    #[test]
    fn corpus_rejection_cases() {
        for source in ["", "abc", "界a😀", "e\u{301}\n😀x", "\r\n"] {
            let (snapshot, intent) = fixture(source);
            let valid = edit(
                &snapshot,
                intent,
                2,
                0,
                0,
                CoordKind::Byte,
                EndpointMode::Exclusive,
            );
            let region = |coord| OrientedRegion {
                anchor: coord,
                head: coord,
                direction: Direction::Forward,
                mode: EndpointMode::Exclusive,
            };
            let mut invalid = vec![(
                RegionCoord::Emacs(EmacsCharPos::new(source.chars().count() + 2).unwrap()),
                ErrorCode::OutOfRange,
            )];
            invalid.push((
                RegionCoord::Agent(AgentPosition::Byte(BytePos::new(source.len() + 1))),
                ErrorCode::OutOfRange,
            ));
            invalid.push((
                RegionCoord::Neovim(LineColByte::new(source.split('\n').count(), 0)),
                ErrorCode::OutOfRange,
            ));
            invalid.push((
                RegionCoord::Agent(AgentPosition::LineColUtf16(LineColUtf16::new(
                    source.split('\n').count(),
                    0,
                ))),
                ErrorCode::OutOfRange,
            ));
            let mut offset = 0;
            for (line, content) in source.split('\n').enumerate() {
                invalid.push((
                    RegionCoord::Neovim(LineColByte::new(line, content.len() + 1)),
                    ErrorCode::OutOfRange,
                ));
                invalid.push((
                    RegionCoord::Agent(AgentPosition::LineColUtf16(LineColUtf16::new(
                        line,
                        content.encode_utf16().count() + 1,
                    ))),
                    ErrorCode::OutOfRange,
                ));
                for col in 0..content.len() {
                    if !content.is_char_boundary(col) {
                        invalid.push((
                            RegionCoord::Neovim(LineColByte::new(line, col)),
                            ErrorCode::InvalidBoundary,
                        ));
                        invalid.push((
                            RegionCoord::Agent(AgentPosition::Byte(BytePos::new(offset + col))),
                            ErrorCode::InvalidBoundary,
                        ));
                    }
                }
                let mut units = 0;
                for ch in content.chars() {
                    if ch.len_utf16() == 2 {
                        invalid.push((
                            RegionCoord::Agent(AgentPosition::LineColUtf16(LineColUtf16::new(
                                line,
                                units + 1,
                            ))),
                            ErrorCode::InvalidUtf16Boundary,
                        ));
                    }
                    units += ch.len_utf16();
                }
                offset += content.len() + 1;
            }
            for (coord, code) in invalid {
                let mut bad = valid.clone();
                bad.buffer = BufferId::new(1);
                bad.region = region(coord);
                for bad in [
                    bad.clone(),
                    {
                        let mut x = bad.clone();
                        x.region.anchor = RegionCoord::Agent(AgentPosition::Byte(BytePos::new(0)));
                        x
                    },
                    {
                        let mut x = bad;
                        x.region.head = RegionCoord::Agent(AgentPosition::Byte(BytePos::new(0)));
                        x
                    },
                ] {
                    for edits in [[valid.clone(), bad.clone()], [bad.clone(), valid.clone()]] {
                        assert_eq!(
                            decode_batch(&snapshot, intent, &edits).unwrap_err().code(),
                            code
                        );
                    }
                }
            }
            let boundaries = snapshot
                .catalog()
                .get(BufferId::new(1))
                .unwrap()
                .char_boundaries();
            for &a in &boundaries {
                for &b in boundaries.iter().filter(|&&b| b >= a) {
                    for &c in &boundaries {
                        for &d in boundaries.iter().filter(|&&d| d >= c) {
                            if a <= d && c <= b {
                                let first = edit(
                                    &snapshot,
                                    intent,
                                    1,
                                    a.as_usize(),
                                    b.as_usize(),
                                    CoordKind::Byte,
                                    EndpointMode::Exclusive,
                                );
                                let second = edit(
                                    &snapshot,
                                    intent,
                                    1,
                                    c.as_usize(),
                                    d.as_usize(),
                                    CoordKind::Byte,
                                    EndpointMode::Exclusive,
                                );
                                for edits in [
                                    [first.clone(), second.clone(), valid.clone()],
                                    [valid.clone(), second.clone(), first.clone()],
                                ] {
                                    assert_eq!(
                                        decode_batch(&snapshot, intent, &edits).unwrap_err().code(),
                                        ErrorCode::Conflict
                                    );
                                }
                            }
                        }
                    }
                }
            }
            for pair in boundaries.windows(2) {
                let start = pair[0].as_usize();
                let end = pair[1].as_usize();
                let mut first = edit(
                    &snapshot,
                    intent,
                    1,
                    start,
                    start,
                    CoordKind::Byte,
                    EndpointMode::Exclusive,
                );
                first.region.mode = EndpointMode::Inclusive;
                let second = edit(
                    &snapshot,
                    intent,
                    1,
                    end,
                    end,
                    CoordKind::Byte,
                    EndpointMode::Exclusive,
                );
                for edits in [
                    [first.clone(), second.clone()],
                    [second.clone(), first.clone()],
                ] {
                    assert_eq!(
                        decode_batch(&snapshot, intent, &edits).unwrap_err().code(),
                        ErrorCode::Conflict
                    );
                }
            }
        }
    }

    #[test]
    fn rejects_bad_edit_anywhere_without_mutation() {
        let (snapshot, intent) = fixture("a😀z");
        let before = snapshot.clone();
        let valid = edit(
            &snapshot,
            intent,
            1,
            0,
            1,
            CoordKind::Byte,
            EndpointMode::Exclusive,
        );
        let byte = |raw| RegionCoord::Agent(AgentPosition::Byte(BytePos::new(raw)));
        let region = |coord| OrientedRegion {
            anchor: coord,
            head: coord,
            direction: Direction::Forward,
            mode: EndpointMode::Exclusive,
        };
        let mut cases = Vec::new();
        let mut bad = valid.clone();
        bad.region = region(byte(2));
        cases.push((bad, ErrorCode::InvalidBoundary));
        let mut bad = valid.clone();
        bad.region = region(byte(99));
        cases.push((bad, ErrorCode::OutOfRange));
        let mut bad = valid.clone();
        bad.region = region(RegionCoord::Agent(AgentPosition::LineColUtf16(
            LineColUtf16::new(0, 2),
        )));
        cases.push((bad, ErrorCode::InvalidUtf16Boundary));
        let mut bad = valid.clone();
        bad.buffer = BufferId::new(99);
        cases.push((bad, ErrorCode::UnknownBuffer));
        let mut bad = valid.clone();
        bad.intent = intent.with_base(intent.base().next().unwrap());
        cases.push((bad, ErrorCode::RevisionMismatch));
        cases.push((valid.clone(), ErrorCode::Conflict));
        cases.push((
            edit(
                &snapshot,
                intent,
                1,
                1,
                5,
                CoordKind::Byte,
                EndpointMode::Exclusive,
            ),
            ErrorCode::Conflict,
        ));
        for (bad, code) in cases {
            for edits in [[valid.clone(), bad.clone()], [bad.clone(), valid.clone()]] {
                assert_eq!(
                    decode_batch(&snapshot, intent, &edits).unwrap_err().code(),
                    code
                );
                assert_eq!(snapshot, before);
            }
        }
        let overlap_a = edit(
            &snapshot,
            intent,
            1,
            0,
            5,
            CoordKind::Byte,
            EndpointMode::Exclusive,
        );
        let overlap_b = edit(
            &snapshot,
            intent,
            1,
            1,
            6,
            CoordKind::Byte,
            EndpointMode::Exclusive,
        );
        for edits in [
            [overlap_a.clone(), overlap_b.clone()],
            [overlap_b, overlap_a],
        ] {
            assert_eq!(
                decode_batch(&snapshot, intent, &edits).unwrap_err().code(),
                ErrorCode::Conflict
            );
        }
        let outer = edit(
            &snapshot,
            intent,
            1,
            0,
            6,
            CoordKind::Byte,
            EndpointMode::Exclusive,
        );
        let inner = edit(
            &snapshot,
            intent,
            1,
            1,
            5,
            CoordKind::Byte,
            EndpointMode::Exclusive,
        );
        for edits in [[outer.clone(), inner.clone()], [inner, outer]] {
            assert_eq!(
                decode_batch(&snapshot, intent, &edits).unwrap_err().code(),
                ErrorCode::Conflict
            );
        }
        assert_eq!(
            decode_batch(
                &snapshot,
                intent.with_base(intent.base().next().unwrap()),
                &[]
            )
            .unwrap_err()
            .code(),
            ErrorCode::RevisionMismatch
        );
        let mut catalog = snapshot.catalog().clone();
        catalog.remove(BufferId::new(2)).unwrap();
        let closed = VersionedWorkspace::new(catalog).capture();
        let mut bad = valid.clone();
        bad.buffer = BufferId::new(2);
        for edits in [[valid.clone(), bad.clone()], [bad.clone(), valid.clone()]] {
            assert_eq!(
                decode_batch(&closed, intent, &edits).unwrap_err().code(),
                ErrorCode::BufferClosed
            );
        }
    }
}
