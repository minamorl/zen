//! Cataloguing workspace buffers.
//!
//! Operations on distinct IDs commute, including a registration and a removal:
//! each checks and changes only its own ID, so reversing their order preserves
//! both results and the final catalog.
//!
//! Tombstones are irreversible: removal inserts an ID into the tombstone set,
//! no operation removes it, and registration checks that set before inserting.
//! Thus every later registration of that ID fails without changing the catalog.
//!
//! The catalog is equivalent to a reference pair `(live_map, tombstone_set)`
//! over any sequence of operations. Initially both pairs are empty. Each
//! successful registration, update, or removal applies the same single-ID map
//! and set changes to both pairs; errors change neither. Induction over the
//! sequence therefore preserves equality of both components.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::editor::edit::ids::BufferId;
use crate::editor::text::utf8::Text;

/// A catalog operation failed for the given buffer identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogError {
    /// The identifier already belongs to a live buffer.
    DuplicateBufferId(BufferId),
    /// The identifier belonged to a buffer that was removed.
    TombstonedBufferId(BufferId),
    /// No live buffer has the identifier.
    UnknownBuffer(BufferId),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateBufferId(id) => write!(f, "buffer {id} is already live"),
            Self::TombstonedBufferId(id) => write!(f, "buffer {id} is tombstoned"),
            Self::UnknownBuffer(id) => write!(f, "unknown buffer {id}"),
        }
    }
}

impl std::error::Error for CatalogError {}

/// Live buffers and the identifiers that can no longer be reused.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BufferCatalog {
    live: BTreeMap<BufferId, Text>,
    tombstones: BTreeSet<BufferId>,
}

impl BufferCatalog {
    /// Creates an empty catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a new identifier, rejecting live and previously removed IDs.
    pub fn register(&mut self, id: BufferId, text: Text) -> Result<(), CatalogError> {
        if self.live.contains_key(&id) {
            return Err(CatalogError::DuplicateBufferId(id));
        }
        if self.tombstones.contains(&id) {
            return Err(CatalogError::TombstonedBufferId(id));
        }
        self.live.insert(id, text);
        Ok(())
    }

    /// Removes a live buffer and permanently retires its identifier.
    pub fn remove(&mut self, id: BufferId) -> Result<Text, CatalogError> {
        let text = self
            .live
            .remove(&id)
            .ok_or(CatalogError::UnknownBuffer(id))?;
        self.tombstones.insert(id);
        Ok(text)
    }

    /// Replaces the text of a live buffer.
    pub fn update(&mut self, id: BufferId, text: Text) -> Result<(), CatalogError> {
        *self
            .live
            .get_mut(&id)
            .ok_or(CatalogError::UnknownBuffer(id))? = text;
        Ok(())
    }

    /// Returns the text of a live buffer.
    pub fn get(&self, id: BufferId) -> Option<&Text> {
        self.live.get(&id)
    }

    /// Returns whether the identifier is live.
    pub fn contains_live(&self, id: BufferId) -> bool {
        self.live.contains_key(&id)
    }

    /// Returns whether the identifier has been removed.
    pub fn is_tombstoned(&self, id: BufferId) -> bool {
        self.tombstones.contains(&id)
    }

    /// Returns live identifiers in ascending order.
    pub fn live_ids(&self) -> Vec<BufferId> {
        self.live.keys().copied().collect()
    }

    /// Returns retired identifiers in ascending order.
    pub fn tombstoned_ids(&self) -> Vec<BufferId> {
        self.tombstones.iter().copied().collect()
    }

    /// Returns the number of live buffers.
    pub fn len_live(&self) -> usize {
        self.live.len()
    }

    /// Returns whether there are no live buffers, regardless of tombstones.
    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::test_support::{assert_law, texts, Rng};

    #[derive(Debug)]
    enum Op {
        Register(BufferId, String),
        Remove(BufferId),
        Update(BufferId, String),
    }

    #[test]
    fn sequences_match_reference_model() {
        assert_law(
            &[0, 1, 42, u64::MAX],
            |rng: &mut Rng| {
                let corpus = texts(rng);
                (0..2000)
                    .map(|_| {
                        let id = BufferId::new(rng.gen_range(0..16) as u64);
                        let text = corpus[rng.gen_range(0..corpus.len())].clone();
                        match rng.gen_range(0..3) {
                            0 => Op::Register(id, text),
                            1 => Op::Remove(id),
                            _ => Op::Update(id, text),
                        }
                    })
                    .collect::<Vec<_>>()
            },
            |ops| {
                let mut catalog = BufferCatalog::new();
                let mut live = BTreeMap::<BufferId, String>::new();
                let mut dead = BTreeSet::new();
                for op in ops {
                    let before = catalog.clone();
                    match op {
                        Op::Register(id, text) => {
                            let expected = if live.contains_key(id) {
                                Err(CatalogError::DuplicateBufferId(*id))
                            } else if dead.contains(id) {
                                Err(CatalogError::TombstonedBufferId(*id))
                            } else {
                                live.insert(*id, text.clone());
                                Ok(())
                            };
                            let actual = catalog.register(*id, Text::from_str(text));
                            assert_eq!(actual, expected);
                            if actual.is_err() {
                                assert_eq!(catalog, before);
                            }
                        }
                        Op::Remove(id) => {
                            let expected = match live.remove(id) {
                                Some(text) => {
                                    dead.insert(*id);
                                    Ok(Text::from_str(&text))
                                }
                                None => Err(CatalogError::UnknownBuffer(*id)),
                            };
                            let actual = catalog.remove(*id);
                            assert_eq!(actual, expected);
                            if actual.is_err() {
                                assert_eq!(catalog, before);
                            }
                        }
                        Op::Update(id, text) => {
                            let expected = if let Some(value) = live.get_mut(id) {
                                *value = text.clone();
                                Ok(())
                            } else {
                                Err(CatalogError::UnknownBuffer(*id))
                            };
                            let actual = catalog.update(*id, Text::from_str(text));
                            assert_eq!(actual, expected);
                            if actual.is_err() {
                                assert_eq!(catalog, before);
                            }
                        }
                    }
                    assert_eq!(catalog.live_ids(), live.keys().copied().collect::<Vec<_>>());
                    assert_eq!(
                        catalog.tombstoned_ids(),
                        dead.iter().copied().collect::<Vec<_>>()
                    );
                    assert_eq!(catalog.len_live(), live.len());
                    assert_eq!(catalog.is_empty(), live.is_empty());
                    for n in 0..16 {
                        let id = BufferId::new(n);
                        assert_eq!(
                            catalog.get(id).map(Text::as_str),
                            live.get(&id).map(String::as_str)
                        );
                        assert_eq!(catalog.contains_live(id), live.contains_key(&id));
                        assert_eq!(catalog.is_tombstoned(id), dead.contains(&id));
                    }
                }
            },
        );
    }

    #[test]
    fn distinct_operations_commute_and_tombstones_persist() {
        let a = BufferId::new(0);
        let b = BufferId::new(u64::MAX);
        for a_state in 0..3 {
            for b_state in 0..3 {
                let mut initial = BufferCatalog::new();
                for (id, state) in [(a, a_state), (b, b_state)] {
                    if state != 0 {
                        initial.register(id, Text::from_str("old")).unwrap();
                    }
                    if state == 2 {
                        initial.remove(id).unwrap();
                    }
                }
                for a_register in [false, true] {
                    for b_register in [false, true] {
                        let apply = |catalog: &mut BufferCatalog, id, register| {
                            if register {
                                catalog.register(id, Text::from_str("new")).map(|()| None)
                            } else {
                                catalog.remove(id).map(Some)
                            }
                        };
                        let mut first = initial.clone();
                        let ar = apply(&mut first, a, a_register);
                        let br = apply(&mut first, b, b_register);
                        let mut second = initial.clone();
                        assert_eq!(br, apply(&mut second, b, b_register));
                        assert_eq!(ar, apply(&mut second, a, a_register));
                        assert_eq!(first, second);
                    }
                }
                for id in [a, b] {
                    if initial.is_tombstoned(id) {
                        let before = initial.clone();
                        assert_eq!(
                            initial.register(id, Text::empty()),
                            Err(CatalogError::TombstonedBufferId(id))
                        );
                        assert_eq!(initial, before);
                    }
                }
            }
        }
    }
}
