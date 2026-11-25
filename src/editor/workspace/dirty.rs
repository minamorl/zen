//! Saved-content baselines for buffer dirty state.
//!
//! A buffer without an acknowledged save is dirty, even if it is empty. Once
//! saved, it is dirty exactly when its current content differs from the saved
//! snapshot; the current revision does not affect that comparison. Receipts
//! must use revisions from the same monotonically increasing version history
//! for each buffer. Older receipts and conflicting receipts at the same revision
//! are rejected without changing the baseline; identical receipts are idempotent.

use std::collections::BTreeMap;
use std::fmt;

use super::version::Revision;
use crate::editor::{edit::ids::BufferId, text::utf8::Text};

/// A successfully saved snapshot and the version from which it was saved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveReceipt {
    /// The saved buffer.
    pub buffer: BufferId,
    /// The buffer's version when the content was captured.
    pub version: Revision,
    /// The exact content saved at that version.
    pub content: Text,
}

/// A receipt cannot replace the acknowledged baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveError {
    /// The receipt predates the acknowledged version.
    Stale {
        acknowledged: Revision,
        attempted: Revision,
    },
    /// The same version was acknowledged with different content.
    ConflictingVersion { version: Revision },
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stale {
                acknowledged,
                attempted,
            } => write!(
                f,
                "save version {attempted} is older than acknowledged version {acknowledged}"
            ),
            Self::ConflictingVersion { version } => {
                write!(f, "save version {version} has conflicting content")
            }
        }
    }
}

impl std::error::Error for SaveError {}

/// The latest acknowledged save for each buffer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirtyTracker {
    saved: BTreeMap<BufferId, SaveReceipt>,
}

impl DirtyTracker {
    /// Creates a tracker with no acknowledged saves.
    pub fn new() -> Self {
        Self::default()
    }

    /// Acknowledges a completed save, without regressing an existing baseline.
    pub fn acknowledge(&mut self, receipt: SaveReceipt) -> Result<(), SaveError> {
        if let Some(previous) = self.saved.get(&receipt.buffer) {
            if receipt.version < previous.version {
                return Err(SaveError::Stale {
                    acknowledged: previous.version,
                    attempted: receipt.version,
                });
            }
            if receipt.version == previous.version && receipt.content != previous.content {
                return Err(SaveError::ConflictingVersion {
                    version: receipt.version,
                });
            }
        }
        self.saved.insert(receipt.buffer, receipt);
        Ok(())
    }

    /// Returns the last acknowledged receipt, if any.
    pub fn saved(&self, buffer: BufferId) -> Option<&SaveReceipt> {
        self.saved.get(&buffer)
    }

    /// Compares current content with the saved snapshot, or returns true if unsaved.
    pub fn is_dirty(&self, buffer: BufferId, current: &Text) -> bool {
        self.saved
            .get(&buffer)
            .is_none_or(|receipt| receipt.content != *current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts, Rng};
    use crate::editor::workspace::{catalog::BufferCatalog, snapshots::VersionedWorkspace};

    #[derive(Debug)]
    struct Step {
        buffer: usize,
        version: u64,
        text: String,
        save: bool,
    }

    #[test]
    fn sequences_match_saved_content_model_and_are_independent() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| {
                let corpus = texts(rng);
                (0..1000)
                    .map(|_| Step {
                        buffer: rng.gen_range(0..4),
                        version: rng.gen_range(0..32) as u64,
                        text: corpus[rng.gen_range(0..corpus.len())].clone(),
                        save: rng.gen_bool(),
                    })
                    .collect::<Vec<_>>()
            },
            |steps| {
                let mut tracker = DirtyTracker::new();
                let mut current = [Text::empty(), Text::empty(), Text::empty(), Text::empty()];
                let mut saved: [Option<(u64, String)>; 4] = [None, None, None, None];
                for step in steps {
                    let id = BufferId::new(step.buffer as u64);
                    let before: Vec<_> = current
                        .iter()
                        .enumerate()
                        .map(|(i, text)| tracker.is_dirty(BufferId::new(i as u64), text))
                        .collect();
                    if step.save {
                        let receipt = SaveReceipt {
                            buffer: id,
                            version: Revision::new(step.version),
                            content: Text::from_str(&step.text),
                        };
                        let expected = match &saved[step.buffer] {
                            Some((version, _)) if step.version < *version => {
                                Err(SaveError::Stale {
                                    acknowledged: Revision::new(*version),
                                    attempted: receipt.version,
                                })
                            }
                            Some((version, text))
                                if step.version == *version && step.text != *text =>
                            {
                                Err(SaveError::ConflictingVersion {
                                    version: receipt.version,
                                })
                            }
                            _ => {
                                saved[step.buffer] = Some((step.version, step.text.clone()));
                                Ok(())
                            }
                        };
                        assert_eq!(tracker.acknowledge(receipt), expected);
                    } else {
                        current[step.buffer] = Text::from_str(&step.text);
                    }
                    for (i, text) in current.iter().enumerate() {
                        let id = BufferId::new(i as u64);
                        let expected = saved[i]
                            .as_ref()
                            .is_none_or(|(_, saved)| text.as_str() != saved);
                        assert_eq!(tracker.is_dirty(id, text), expected);
                        assert_eq!(
                            tracker
                                .saved(id)
                                .map(|r| (r.version.raw(), r.content.as_str())),
                            saved[i].as_ref().map(|(v, s)| (*v, s.as_str()))
                        );
                        if i != step.buffer {
                            assert_eq!(tracker.is_dirty(id, text), before[i]);
                        }
                    }
                }
            },
        );
    }

    #[test]
    fn restoring_saved_content_is_clean_despite_later_versions() {
        assert_law(&[0, 1, 42, u64::MAX], texts, |corpus| {
            for saved in corpus {
                let id = BufferId::new(7);
                let mut tracker = DirtyTracker::new();
                let original = Text::from_str(saved);
                assert!(tracker.is_dirty(id, &original));
                let mut workspace = VersionedWorkspace::new(BufferCatalog::new());
                workspace.register(id, original.clone()).unwrap();
                tracker
                    .acknowledge(SaveReceipt {
                        buffer: id,
                        version: workspace.revision(),
                        content: original.clone(),
                    })
                    .unwrap();
                let edited = Text::from_str(&(saved.to_owned() + "x"));
                workspace.update(id, edited).unwrap();
                assert!(tracker.is_dirty(id, workspace.catalog().get(id).unwrap()));
                workspace.update(id, original).unwrap();
                assert!(workspace.revision() > tracker.saved(id).unwrap().version);
                assert!(!tracker.is_dirty(id, workspace.catalog().get(id).unwrap()));
            }
        });
    }
}
