//! Lossless external representations of one captured buffer.
//!
//! Emacs and Agent carry UTF-8 text directly. Neovim carries the canonical
//! line array and end-of-line flag defined by `line_endings`: an empty array
//! with no end-of-line is empty text, and one empty line with end-of-line is
//! a single LF. CR is content. Decoding validates this representation.
//!
//! Each encoding copies the captured buffer ID, workspace revision and text.
//! Direct text copies are identities; `split_lines` and `join_lines` are
//! inverses for canonical line arrays. Therefore all three encodings decode
//! to the same buffer, revision and exact text.

use super::error::{BoundaryError, ErrorCode};
use crate::editor::edit::ids::BufferId;
use crate::editor::text::line_endings::{join_lines, split_lines, LineBreakdown};
use crate::editor::text::utf8::Text;
use crate::editor::workspace::{snapshots::WorkspaceSnapshot, version::Revision};

/// Error returned by snapshot encoding or decoding.
pub type InteropError = BoundaryError;

/// A decoded buffer and the revision of the workspace it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferSnapshot {
    /// Buffer identifier.
    pub buffer: BufferId,
    /// Captured workspace revision.
    pub revision: Revision,
    /// Exact UTF-8 contents.
    pub text: Text,
}

/// An Emacs buffer snapshot containing the complete text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmacsSnapshot {
    /// Buffer identifier.
    pub buffer: BufferId,
    /// Captured workspace revision.
    pub revision: Revision,
    /// Exact UTF-8 contents.
    pub text: String,
}

/// A Neovim buffer snapshot in canonical line-array form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeovimSnapshot {
    /// Buffer identifier.
    pub buffer: BufferId,
    /// Captured workspace revision.
    pub revision: Revision,
    /// Line contents without LF separators.
    pub lines: Vec<String>,
    /// Whether the text ends in LF (`endofline`/`eol`).
    pub endofline: bool,
}

/// An Agent buffer snapshot containing the complete text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSnapshot {
    /// Buffer identifier.
    pub buffer: BufferId,
    /// Captured workspace revision.
    pub revision: Revision,
    /// Exact UTF-8 contents.
    pub text: String,
}

fn captured(
    snapshot: &WorkspaceSnapshot,
    buffer: BufferId,
) -> Result<BufferSnapshot, InteropError> {
    let catalog = snapshot.catalog();
    let text = catalog.get(buffer).ok_or_else(|| {
        let code = if catalog.is_tombstoned(buffer) {
            ErrorCode::BufferClosed
        } else {
            ErrorCode::UnknownBuffer
        };
        BoundaryError::new(code, format!("buffer {buffer} is not live"))
    })?;
    Ok(BufferSnapshot {
        buffer,
        revision: snapshot.revision(),
        text: text.clone(),
    })
}

/// Encodes a live captured buffer as an Emacs snapshot.
pub fn encode_emacs(
    snapshot: &WorkspaceSnapshot,
    buffer: BufferId,
) -> Result<EmacsSnapshot, InteropError> {
    let value = captured(snapshot, buffer)?;
    Ok(EmacsSnapshot {
        buffer: value.buffer,
        revision: value.revision,
        text: value.text.as_str().to_owned(),
    })
}

/// Decodes an Emacs snapshot.
pub fn decode_emacs(snapshot: &EmacsSnapshot) -> BufferSnapshot {
    BufferSnapshot {
        buffer: snapshot.buffer,
        revision: snapshot.revision,
        text: Text::from_str(&snapshot.text),
    }
}

/// Encodes a live captured buffer as a canonical Neovim snapshot.
pub fn encode_neovim(
    snapshot: &WorkspaceSnapshot,
    buffer: BufferId,
) -> Result<NeovimSnapshot, InteropError> {
    let value = captured(snapshot, buffer)?;
    let lines = split_lines(&value.text);
    Ok(NeovimSnapshot {
        buffer: value.buffer,
        revision: value.revision,
        lines: lines.lines,
        endofline: lines.trailing_lf,
    })
}

/// Decodes a canonical Neovim snapshot, rejecting invalid line arrays.
pub fn decode_neovim(snapshot: &NeovimSnapshot) -> Result<BufferSnapshot, InteropError> {
    let text = join_lines(&LineBreakdown {
        lines: snapshot.lines.clone(),
        trailing_lf: snapshot.endofline,
    })
    .map_err(|error| BoundaryError::new(ErrorCode::InvalidLineEndings, error.to_string()))?;
    Ok(BufferSnapshot {
        buffer: snapshot.buffer,
        revision: snapshot.revision,
        text,
    })
}

/// Encodes a live captured buffer as an Agent snapshot.
pub fn encode_agent(
    snapshot: &WorkspaceSnapshot,
    buffer: BufferId,
) -> Result<AgentSnapshot, InteropError> {
    let value = captured(snapshot, buffer)?;
    Ok(AgentSnapshot {
        buffer: value.buffer,
        revision: value.revision,
        text: value.text.as_str().to_owned(),
    })
}

/// Decodes an Agent snapshot.
pub fn decode_agent(snapshot: &AgentSnapshot) -> BufferSnapshot {
    BufferSnapshot {
        buffer: snapshot.buffer,
        revision: snapshot.revision,
        text: Text::from_str(&snapshot.text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts};
    use crate::editor::workspace::{catalog::BufferCatalog, snapshots::VersionedWorkspace};

    fn check(source: &str, id: BufferId) {
        let mut workspace = VersionedWorkspace::new(BufferCatalog::new());
        workspace.register(id, Text::from_str("before")).unwrap();
        workspace.update(id, Text::from_str(source)).unwrap();
        let snapshot = workspace.capture();
        let expected = BufferSnapshot {
            buffer: id,
            revision: Revision::new(2),
            text: Text::from_str(source),
        };
        let emacs = encode_emacs(&snapshot, id).unwrap();
        let neovim = encode_neovim(&snapshot, id).unwrap();
        let agent = encode_agent(&snapshot, id).unwrap();
        assert_eq!(decode_emacs(&emacs), expected);
        assert_eq!(decode_neovim(&neovim), Ok(expected.clone()));
        assert_eq!(decode_agent(&agent), expected);
        assert_eq!(encode_neovim(&snapshot, id).unwrap(), neovim);
        workspace.update(id, Text::from_str("after")).unwrap();
        assert_eq!(
            decode_emacs(&encode_emacs(&snapshot, id).unwrap()),
            expected
        );
    }

    #[test]
    fn exact_cases_and_neovim_representation() {
        for (source, lines, endofline) in [
            ("", vec![], false),
            ("\n", vec![""], true),
            ("a", vec!["a"], false),
            ("a\n", vec!["a"], true),
            ("\n\n", vec!["", ""], true),
            ("\r", vec!["\r"], false),
            ("a\r\nb\r", vec!["a\r", "b\r"], false),
            ("界\ne\u{301}\n😀\n", vec!["界", "e\u{301}", "😀"], true),
        ] {
            let id = BufferId::new(u64::MAX);
            check(source, id);
            let mut catalog = BufferCatalog::new();
            catalog.register(id, Text::from_str(source)).unwrap();
            let encoded = encode_neovim(&VersionedWorkspace::new(catalog).capture(), id).unwrap();
            assert_eq!(encoded.lines, lines);
            assert_eq!(encoded.endofline, endofline);
            assert_eq!(encoded.revision, Revision::ZERO);
        }
    }

    #[test]
    fn generated_cross_format_roundtrips() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for (i, source) in corpus.iter().enumerate() {
                check(source, BufferId::new(i as u64));
            }
        });
    }

    #[test]
    fn invalid_lines_are_rejected() {
        for (lines, endofline, message) in [
            (vec!["a\nb"], false, "line 0 contains LF"),
            (vec!["a", "\n"], true, "line 1 contains LF"),
            (vec![], true, "empty line array has trailing LF"),
            (vec![""], false, "final empty line requires trailing LF"),
            (
                vec!["a", ""],
                false,
                "final empty line requires trailing LF",
            ),
        ] {
            let value = NeovimSnapshot {
                buffer: BufferId::new(1),
                revision: Revision::MAX,
                lines: lines.into_iter().map(str::to_owned).collect(),
                endofline,
            };
            let error = decode_neovim(&value).unwrap_err();
            assert_eq!(error.code(), ErrorCode::InvalidLineEndings);
            assert_eq!(error.message(), message);
        }
    }

    #[test]
    fn unknown_and_tombstoned_buffers_are_rejected_by_every_encoder() {
        let dead = BufferId::new(1);
        let missing = BufferId::new(2);
        let mut workspace = VersionedWorkspace::new(BufferCatalog::new());
        workspace.register(dead, Text::empty()).unwrap();
        let live_snapshot = workspace.capture();
        workspace.remove(dead).unwrap();
        let snapshot = workspace.capture();
        assert!(encode_emacs(&live_snapshot, dead).is_ok());
        for (id, code) in [
            (dead, ErrorCode::BufferClosed),
            (missing, ErrorCode::UnknownBuffer),
        ] {
            assert_eq!(encode_emacs(&snapshot, id).unwrap_err().code(), code);
            assert_eq!(encode_neovim(&snapshot, id).unwrap_err().code(), code);
            assert_eq!(encode_agent(&snapshot, id).unwrap_err().code(), code);
        }
    }
}
