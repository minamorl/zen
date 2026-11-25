//! Versioned per-buffer changes in base-snapshot coordinates.
//!
//! Every edit is decoded against the same base, then applied as a patch. The
//! Neovim plan is emitted in descending order for direct set_text replay.
//! Errors leave the supplied snapshot unchanged and require resynchronization.

use super::agent_position::{self, AgentPosition};
use super::coordinates::{BytePos, EmacsCharPos, LineColByte};
use super::emacs_position;
use super::error::{BoundaryError, ErrorCode};
use super::neovim_position;
use super::snapshot::{self, AgentSnapshot, BufferSnapshot, EmacsSnapshot, NeovimSnapshot};
use crate::editor::collab::receipt::CommitReceipt;
use crate::editor::edit::{
    history::HistoryEntry, ids::BufferId, patch::Patch, splice::Splice, transaction::Transaction,
};
use crate::editor::text::{line_endings::split_lines, span::ByteSpan, utf8::Text};
use crate::editor::workspace::version::Revision;

/// Metadata shared by all representations of a committed buffer change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeltaVersion {
    /// Target buffer.
    pub buffer: BufferId,
    /// Revision from which all coordinates are measured.
    pub base_revision: Revision,
    /// Revision after applying the delta.
    pub target_revision: Revision,
}

/// An Emacs half-open scalar region and its replacement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmacsEdit {
    /// One-based start position.
    pub start: EmacsCharPos,
    /// One-based exclusive end position.
    pub end: EmacsCharPos,
    /// Replacement text.
    pub replacement: String,
}

/// A Neovim set_text operation, in zero-based row and byte columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeovimEdit {
    /// Start position.
    pub start: LineColByte,
    /// Exclusive end position.
    pub end: LineColByte,
    /// Replacement lines, split on LF, including empty final lines.
    pub replacement: Vec<String>,
}

/// An agent edit with explicitly typed endpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEdit {
    /// Start position.
    pub start: AgentPosition,
    /// Exclusive end position.
    pub end: AgentPosition,
    /// Replacement text.
    pub replacement: String,
}

/// An Emacs delta, with edits in ascending base order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmacsDelta {
    /// Version metadata.
    pub version: DeltaVersion,
    /// Base-coordinate edits.
    pub edits: Vec<EmacsEdit>,
}
/// A Neovim delta, with set_text edits in descending base order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeovimDelta {
    /// Version metadata.
    pub version: DeltaVersion,
    /// Base-coordinate operations.
    pub edits: Vec<NeovimEdit>,
    /// End-of-line option after applying the operations.
    pub endofline: bool,
}
/// An agent delta, with edits in ascending base order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentDelta {
    /// Version metadata.
    pub version: DeltaVersion,
    /// Base-coordinate edits.
    pub edits: Vec<AgentEdit>,
}

fn mismatch(message: &str) -> BoundaryError {
    BoundaryError::new(
        ErrorCode::RevisionMismatch,
        format!("resync required: {message}"),
    )
}
fn validate(version: DeltaVersion, base: &BufferSnapshot) -> Result<(), BoundaryError> {
    if version.buffer != base.buffer {
        return Err(mismatch("buffer mismatch"));
    }
    if version.base_revision != base.revision || version.target_revision <= version.base_revision {
        return Err(mismatch("revision mismatch or non-advancing target"));
    }
    Ok(())
}
fn invalid(error: impl std::fmt::Display) -> BoundaryError {
    BoundaryError::new(
        ErrorCode::InvalidCoordinate,
        format!("resync required: {error}"),
    )
}
fn patch(
    base: &BufferSnapshot,
    version: DeltaVersion,
    edits: Vec<(
        crate::editor::text::utf8::ByteOffset,
        crate::editor::text::utf8::ByteOffset,
        String,
    )>,
) -> Result<BufferSnapshot, BoundaryError> {
    validate(version, base)?;
    let splices = edits
        .into_iter()
        .map(|(start, end, replacement)| {
            let span = ByteSpan::new(start, end).map_err(invalid)?;
            Ok(Splice::new(span, Text::from_str(&replacement)))
        })
        .collect::<Result<Vec<_>, BoundaryError>>()?;
    let patch = Patch::new(splices).map_err(invalid)?;
    let text = patch.apply(&base.text).map_err(invalid)?;
    Ok(BufferSnapshot {
        buffer: base.buffer,
        revision: version.target_revision,
        text,
    })
}

/// Three equivalent encodings of one buffer's committed change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeltaFormats {
    /// Emacs representation.
    pub emacs: EmacsDelta,
    /// Neovim representation.
    pub neovim: NeovimDelta,
    /// Agent representation.
    pub agent: AgentDelta,
}

/// Encodes a recorded commit against its matching base buffer snapshot.
pub fn emit_history(
    entry: &HistoryEntry,
    base: &BufferSnapshot,
) -> Result<DeltaFormats, BoundaryError> {
    emit(entry.forward(), entry.previous(), entry.committed(), base)
}

/// Encodes a transaction accompanied by its matching commit receipt.
pub fn emit_receipt(
    receipt: &CommitReceipt,
    transaction: &Transaction,
    base: &BufferSnapshot,
) -> Result<DeltaFormats, BoundaryError> {
    if receipt.txn() != transaction.intent().txn()
        || transaction.intent().base() != receipt.previous()
    {
        return Err(mismatch("receipt and transaction differ"));
    }
    emit(transaction, receipt.previous(), receipt.committed(), base)
}

fn emit(
    transaction: &Transaction,
    previous: Revision,
    committed: Revision,
    base: &BufferSnapshot,
) -> Result<DeltaFormats, BoundaryError> {
    let version = DeltaVersion {
        buffer: base.buffer,
        base_revision: previous,
        target_revision: committed,
    };
    validate(version, base)?;
    let mut emacs = Vec::new();
    let mut result_text = base.text.clone();
    let mut neovim = Vec::new();
    let mut agent = Vec::new();
    if let Some((_, patch)) = transaction
        .patches()
        .iter()
        .find(|(id, _)| *id == base.buffer)
    {
        // Validate the entire patch before producing any output.
        result_text = patch.apply(&base.text).map_err(invalid)?;
        let logical = base
            .text
            .as_str()
            .strip_suffix('\n')
            .unwrap_or(base.text.as_str());
        let logical_text = Text::from_str(logical);
        for splice in patch.splices() {
            let span = splice.span();
            let replacement = splice.replacement().as_str();
            emacs.push(EmacsEdit {
                start: emacs_position::byte_to_emacs(&base.text, span.start()).map_err(invalid)?,
                end: emacs_position::byte_to_emacs(&base.text, span.end()).map_err(invalid)?,
                replacement: replacement.to_owned(),
            });
            let mut start = span.start().as_usize().min(logical.len());
            let end = span.end().as_usize().min(logical.len());
            let mut nvim_replacement = String::new();
            nvim_replacement.push_str(&base.text.as_str()[start..span.start().as_usize()]);
            nvim_replacement.push_str(replacement);
            if span.end().as_usize() == base.text.len_bytes()
                && result_text.as_str().ends_with('\n')
            {
                if nvim_replacement.ends_with('\n') {
                    nvim_replacement.pop();
                } else {
                    start -= 1;
                }
            }
            let start_offset = logical_text.validate_offset(start).map_err(invalid)?;
            let end_offset = logical_text.validate_offset(end).map_err(invalid)?;
            neovim.push(NeovimEdit {
                start: neovim_position::byte_to_line_col(&logical_text, start_offset)
                    .map_err(invalid)?,
                end: neovim_position::byte_to_line_col(&logical_text, end_offset)
                    .map_err(invalid)?,
                replacement: nvim_replacement.split('\n').map(str::to_owned).collect(),
            });
            agent.push(AgentEdit {
                start: AgentPosition::Byte(BytePos::new(span.start().as_usize())),
                end: AgentPosition::Byte(BytePos::new(span.end().as_usize())),
                replacement: replacement.to_owned(),
            });
        }
    }
    neovim.reverse();
    Ok(DeltaFormats {
        emacs: EmacsDelta {
            version,
            edits: emacs,
        },
        neovim: NeovimDelta {
            version,
            edits: neovim,
            endofline: result_text.as_str().ends_with('\n'),
        },
        agent: AgentDelta {
            version,
            edits: agent,
        },
    })
}

/// Applies an Emacs delta atomically, returning a resync error on failure.
pub fn apply_emacs(snapshot: &mut EmacsSnapshot, delta: &EmacsDelta) -> Result<(), BoundaryError> {
    let base = snapshot::decode_emacs(snapshot);
    validate(delta.version, &base)?;
    let edits = delta
        .edits
        .iter()
        .map(|edit| {
            Ok((
                emacs_position::emacs_to_byte(&base.text, edit.start).map_err(invalid)?,
                emacs_position::emacs_to_byte(&base.text, edit.end).map_err(invalid)?,
                edit.replacement.clone(),
            ))
        })
        .collect::<Result<Vec<_>, BoundaryError>>()?;
    let next = patch(&base, delta.version, edits)?;
    snapshot.text = next.text.as_str().to_owned();
    snapshot.revision = next.revision;
    Ok(())
}
/// Applies a Neovim delta atomically, returning a resync error on failure.
pub fn apply_neovim(
    snapshot: &mut NeovimSnapshot,
    delta: &NeovimDelta,
) -> Result<(), BoundaryError> {
    let base = snapshot::decode_neovim(snapshot).map_err(invalid)?;
    validate(delta.version, &base)?;
    let logical = base
        .text
        .as_str()
        .strip_suffix('\n')
        .unwrap_or(base.text.as_str());
    let logical_base = BufferSnapshot {
        buffer: base.buffer,
        revision: base.revision,
        text: Text::from_str(logical),
    };
    let edits = delta
        .edits
        .iter()
        .map(|edit| {
            if edit.replacement.is_empty()
                || edit.replacement.iter().any(|line| line.contains('\n'))
            {
                return Err(invalid("invalid replacement lines"));
            }
            Ok((
                neovim_position::line_col_to_byte(&logical_base.text, edit.start)
                    .map_err(invalid)?,
                neovim_position::line_col_to_byte(&logical_base.text, edit.end).map_err(invalid)?,
                edit.replacement.join("\n"),
            ))
        })
        .collect::<Result<Vec<_>, BoundaryError>>()?;
    let spans = edits
        .into_iter()
        .map(|(start, end, replacement)| {
            Ok(Splice::new(
                ByteSpan::new(start, end).map_err(invalid)?,
                Text::from_str(&replacement),
            ))
        })
        .collect::<Result<Vec<_>, BoundaryError>>()?;
    for pair in spans.windows(2) {
        let a = pair[0].span();
        let b = pair[1].span();
        if a.start() < b.end() {
            return Err(invalid("overlapping Neovim edits"));
        }
    }
    let mut text = logical_base.text.clone();
    for edit in &spans {
        text = edit.apply(&text).map_err(invalid)?;
    }
    if !delta.endofline && text.as_str().ends_with('\n') {
        return Err(invalid("noncanonical final empty Neovim line"));
    }
    let mut next = BufferSnapshot {
        buffer: base.buffer,
        revision: delta.version.target_revision,
        text,
    };
    if delta.endofline {
        next.text = Text::from_str(&format!("{}\n", next.text.as_str()));
    }
    let lines = split_lines(&next.text);
    snapshot.lines = lines.lines;
    snapshot.endofline = lines.trailing_lf;
    snapshot.revision = next.revision;
    Ok(())
}
/// Applies an agent delta atomically, returning a resync error on failure.
pub fn apply_agent(snapshot: &mut AgentSnapshot, delta: &AgentDelta) -> Result<(), BoundaryError> {
    let base = snapshot::decode_agent(snapshot);
    validate(delta.version, &base)?;
    let edits = delta
        .edits
        .iter()
        .map(|edit| {
            Ok((
                agent_position::position_to_byte(&base.text, edit.start).map_err(invalid)?,
                agent_position::position_to_byte(&base.text, edit.end).map_err(invalid)?,
                edit.replacement.clone(),
            ))
        })
        .collect::<Result<Vec<_>, BoundaryError>>()?;
    let next = patch(&base, delta.version, edits)?;
    snapshot.text = next.text.as_str().to_owned();
    snapshot.revision = next.revision;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::edit::{
        history::EditHistory,
        ids::{ActorId, Nonce},
        intent::EditIntent,
    };
    use crate::editor::test_support::{self, Rng};
    use crate::editor::workspace::{catalog::BufferCatalog, snapshots::VersionedWorkspace};

    fn transaction(base: &BufferSnapshot, spans: &[(usize, usize, &str)]) -> Transaction {
        let intent = EditIntent::for_actor(
            ActorId::new(1),
            Nonce::new(base.revision.raw()),
            base.revision,
        );
        let splices = spans
            .iter()
            .map(|(a, b, s)| {
                Splice::new(
                    ByteSpan::validate_in(&base.text, *a, *b).unwrap(),
                    Text::from_str(s),
                )
            })
            .collect();
        Transaction::new(intent, vec![(base.buffer, Patch::new(splices).unwrap())]).unwrap()
    }
    fn snapshots(base: &BufferSnapshot) -> (EmacsSnapshot, NeovimSnapshot, AgentSnapshot) {
        let lines = split_lines(&base.text);
        (
            EmacsSnapshot {
                buffer: base.buffer,
                revision: base.revision,
                text: base.text.as_str().into(),
            },
            NeovimSnapshot {
                buffer: base.buffer,
                revision: base.revision,
                lines: lines.lines,
                endofline: lines.trailing_lf,
            },
            AgentSnapshot {
                buffer: base.buffer,
                revision: base.revision,
                text: base.text.as_str().into(),
            },
        )
    }
    #[test]
    fn deterministic_generated_patches_and_chains() {
        test_support::assert_law(&[0, 1, 42, u64::MAX], test_support::texts, |corpus| {
            let mut rng = Rng::new(42);
            for source in corpus {
                let mut base = BufferSnapshot {
                    buffer: BufferId::new(7),
                    revision: Revision::ZERO,
                    text: Text::from_str(source),
                };
                let (mut e, mut n, mut a) = snapshots(&base);
                for _ in 0..8 {
                    let bounds = base.text.char_boundaries();
                    let i = rng.gen_range(0..bounds.len());
                    let j = rng.gen_range(0..bounds.len());
                    let start = bounds[i.min(j)].as_usize();
                    let end = bounds[i.max(j)].as_usize();
                    let replacement = *rng.choose(&["", "界\n😀", "\r\n", "x"]).unwrap();
                    let txn = transaction(&base, &[(start, end, replacement)]);
                    let receipt = CommitReceipt::new(txn.intent(), base.revision).unwrap();
                    let delta = emit_receipt(&receipt, &txn, &base).unwrap();
                    let expected = txn.patches()[0].1.apply(&base.text).unwrap();
                    apply_emacs(&mut e, &delta.emacs).unwrap();
                    apply_neovim(&mut n, &delta.neovim).unwrap();
                    apply_agent(&mut a, &delta.agent).unwrap();
                    base.text = expected;
                    base.revision = receipt.committed();
                    assert_eq!(snapshot::decode_emacs(&e), base);
                    assert_eq!(snapshot::decode_neovim(&n).unwrap(), base);
                    assert_eq!(snapshot::decode_agent(&a), base);
                }
            }
        });
    }
    #[test]
    fn history_multiple_edits_and_rejections_are_atomic() {
        let id = BufferId::new(7);
        let mut catalog = BufferCatalog::new();
        catalog.register(id, Text::from_str("a😀\nb界")).unwrap();
        let workspace = VersionedWorkspace::new(catalog);
        let captured = workspace.capture();
        let base = BufferSnapshot {
            buffer: id,
            revision: Revision::ZERO,
            text: Text::from_str("a😀\nb界"),
        };
        let txn = transaction(&base, &[(0, 1, "z\n"), (6, 7, "!")]);
        let result = txn.apply_with_inverse(&captured).unwrap();
        let mut history = EditHistory::new();
        history
            .record(&captured, Revision::new(1), result.catalog(), txn.clone())
            .unwrap();
        let delta = emit_history(&history.entries()[0], &base).unwrap();
        assert_eq!(
            delta,
            emit_receipt(
                &CommitReceipt::new(txn.intent(), base.revision).unwrap(),
                &txn,
                &base
            )
            .unwrap()
        );
        assert_eq!(delta.neovim.edits.len(), 2);
        assert!(delta.neovim.edits[0].start > delta.neovim.edits[1].start);
        let (mut e, mut n, mut a) = snapshots(&base);
        apply_emacs(&mut e, &delta.emacs).unwrap();
        apply_neovim(&mut n, &delta.neovim).unwrap();
        apply_agent(&mut a, &delta.agent).unwrap();
        assert_eq!(
            snapshot::decode_emacs(&e).text,
            *result.catalog().get(id).unwrap()
        );
        assert_eq!(
            snapshot::decode_neovim(&n).unwrap().text,
            snapshot::decode_agent(&a).text
        );
        let saved = (e.clone(), n.clone(), a.clone());
        assert_eq!(
            apply_emacs(&mut e, &delta.emacs).unwrap_err().code(),
            ErrorCode::RevisionMismatch
        );
        assert!(apply_neovim(&mut n, &delta.neovim).is_err());
        assert!(apply_agent(&mut a, &delta.agent).is_err());
        assert_eq!((e, n, a), saved);
        let (mut e, mut n, mut a) = snapshots(&base);
        let saved = (e.clone(), n.clone(), a.clone());
        let mut bad = delta.clone();
        bad.emacs.edits[1].end = EmacsCharPos::new(999).unwrap();
        bad.neovim.edits[0].end = LineColByte::new(999, 0);
        bad.agent.edits[1].end = AgentPosition::Byte(BytePos::new(2));
        assert!(apply_emacs(&mut e, &bad.emacs)
            .unwrap_err()
            .message()
            .contains("resync"));
        assert!(apply_neovim(&mut n, &bad.neovim).is_err());
        assert!(apply_agent(&mut a, &bad.agent).is_err());
        assert_eq!((e, n, a), saved);
        let mut bad = delta.emacs.clone();
        bad.version.buffer = BufferId::new(8);
        let mut e = snapshots(&base).0;
        assert!(apply_emacs(&mut e, &bad).is_err());
        bad.version.buffer = id;
        bad.version.target_revision = Revision::ZERO;
        assert!(apply_emacs(&mut e, &bad).is_err());
        assert_eq!(e, snapshots(&base).0);
    }

    #[test]
    fn rebased_receipt_and_neovim_final_lf() {
        for (source, start, end, replacement) in [("a\n", 2, 2, "x"), ("a\n", 1, 2, "")] {
            let base = BufferSnapshot {
                buffer: BufferId::new(7),
                revision: Revision::new(1),
                text: Text::from_str(source),
            };
            let txn = transaction(&base, &[(start, end, replacement)]);
            let receipt =
                CommitReceipt::new(txn.intent().with_base(Revision::ZERO), base.revision).unwrap();
            let delta = emit_receipt(&receipt, &txn, &base).unwrap();
            for edit in &delta.neovim.edits {
                assert_eq!(edit.start.line(), 0);
                assert_eq!(edit.end.line(), 0);
            }
            let mut n = snapshots(&base).1;
            apply_neovim(&mut n, &delta.neovim).unwrap();
            assert_eq!(
                snapshot::decode_neovim(&n).unwrap().text,
                txn.patches()[0].1.apply(&base.text).unwrap()
            );
        }
    }

    #[test]
    fn separated_edits_around_final_lf() {
        for source in ["a\n", "a\nb\n", "😀\n界\n", "\n\n", "abc"] {
            let base = BufferSnapshot {
                buffer: BufferId::new(7),
                revision: Revision::ZERO,
                text: Text::from_str(source),
            };
            let bounds = base.text.char_boundaries();
            for &a in &bounds {
                for &b in &bounds {
                    for &c in &bounds {
                        for &d in &bounds {
                            if !(a <= b && b < c && c <= d) {
                                continue;
                            }
                            for (x, y) in [("X", "Y"), ("", ""), ("\n", "界\n")] {
                                let txn = transaction(
                                    &base,
                                    &[
                                        (a.as_usize(), b.as_usize(), x),
                                        (c.as_usize(), d.as_usize(), y),
                                    ],
                                );
                                let receipt =
                                    CommitReceipt::new(txn.intent(), base.revision).unwrap();
                                let delta = emit_receipt(&receipt, &txn, &base).unwrap();
                                let mut n = snapshots(&base).1;
                                apply_neovim(&mut n, &delta.neovim).unwrap_or_else(|e| {
                                    panic!("{source:?} {a:?} {b:?} {c:?} {d:?} {x:?} {y:?}: {e}")
                                });
                                assert_eq!(
                                    snapshot::decode_neovim(&n).unwrap().text,
                                    txn.patches()[0].1.apply(&base.text).unwrap(),
                                    "{source:?} {a:?} {b:?} {c:?} {d:?} {x:?} {y:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn noncanonical_neovim_result_requires_resync() {
        let base = BufferSnapshot {
            buffer: BufferId::new(7),
            revision: Revision::ZERO,
            text: Text::from_str("a"),
        };
        let mut snapshot = snapshots(&base).1;
        let original = snapshot.clone();
        let delta = NeovimDelta {
            version: DeltaVersion {
                buffer: base.buffer,
                base_revision: base.revision,
                target_revision: Revision::new(1),
            },
            edits: vec![NeovimEdit {
                start: LineColByte::new(0, 1),
                end: LineColByte::new(0, 1),
                replacement: vec![String::new(), String::new()],
            }],
            endofline: false,
        };
        assert!(apply_neovim(&mut snapshot, &delta)
            .unwrap_err()
            .message()
            .contains("resync"));
        assert_eq!(snapshot, original);
    }
}
