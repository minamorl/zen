//! Actor-private typed registers and kill histories.
//!
//! A register update touches exactly one `(actor, name)` key: putting a value
//! makes it observable at that key, and removing it leaves all other keys
//! unchanged. Consequently operations on distinct keys commute, including
//! keys belonging to different actors. At one key, the last put or remove
//! determines the value. These laws follow from the corresponding map operations.
//! Entry equality compares both text and kind, so identical text in different
//! kinds remains distinct. Kill concatenation uses `old.concat(new)` for a
//! forward kill and `new.concat(old)` for a backward kill, preserving byte order.

use std::collections::BTreeMap;

use crate::editor::edit::ids::ActorId;
use crate::editor::text::utf8::Text;

/// A nonempty register identifier. Names are case-sensitive and may be Unicode.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RegisterName(String);

impl RegisterName {
    /// Returns `None` for an empty identifier.
    pub fn new(name: impl Into<String>) -> Option<Self> {
        let name = name.into();
        (!name.is_empty()).then_some(Self(name))
    }

    /// Borrows the identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The interpretation of a register's text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RegisterKind {
    /// Character-oriented text.
    Charwise,
    /// Whole-line text.
    Linewise,
    /// Rectangular text.
    Blockwise,
}

/// Text together with its register interpretation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisterEntry {
    /// The exact text stored.
    pub text: Text,
    /// The interpretation of that text.
    pub kind: RegisterKind,
}

/// Named registers isolated by actor.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RegisterStore {
    entries: BTreeMap<(ActorId, RegisterName), RegisterEntry>,
}

impl RegisterStore {
    /// Creates an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the entry at this actor and name, returning the previous entry.
    pub fn put(
        &mut self,
        actor: ActorId,
        name: RegisterName,
        entry: RegisterEntry,
    ) -> Option<RegisterEntry> {
        self.entries.insert((actor, name), entry)
    }

    /// Looks up exactly this actor and name.
    pub fn get(&self, actor: ActorId, name: &RegisterName) -> Option<&RegisterEntry> {
        self.entries.get(&(actor, name.clone()))
    }

    /// Removes exactly this actor and name, returning its previous entry.
    pub fn remove(&mut self, actor: ActorId, name: &RegisterName) -> Option<RegisterEntry> {
        self.entries.remove(&(actor, name.clone()))
    }
}

/// One actor's kill history, newest first, with a selectable current entry.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct KillRing {
    entries: Vec<RegisterEntry>,
    current: usize,
}

impl KillRing {
    /// Creates an empty history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts a newest entry and selects it.
    pub fn push(&mut self, entry: RegisterEntry) {
        self.entries.insert(0, entry);
        self.current = 0;
    }

    /// Appends text to the current entry, retaining its kind; returns false if empty.
    pub fn append(&mut self, text: &Text) -> bool {
        if let Some(entry) = self.entries.get_mut(self.current) {
            entry.text = entry.text.concat(text);
            true
        } else {
            false
        }
    }

    /// Prepends text to the current entry, retaining its kind; returns false if empty.
    pub fn prepend(&mut self, text: &Text) -> bool {
        if let Some(entry) = self.entries.get_mut(self.current) {
            entry.text = text.concat(&entry.text);
            true
        } else {
            false
        }
    }

    /// Returns the selected entry, if any.
    pub fn current(&self) -> Option<&RegisterEntry> {
        self.entries.get(self.current)
    }

    /// Returns an entry by absolute age, with zero denoting the newest.
    pub fn nth(&self, index: usize) -> Option<&RegisterEntry> {
        self.entries.get(index)
    }

    /// Selects the next older entry, wrapping around; returns `None` if empty.
    pub fn rotate(&mut self) -> Option<&RegisterEntry> {
        if self.entries.is_empty() {
            return None;
        }
        self.current = (self.current + 1) % self.entries.len();
        self.current()
    }
}

/// Separate kill histories indexed by actor.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct KillRings {
    rings: BTreeMap<ActorId, KillRing>,
}

impl KillRings {
    /// Looks up an actor's history without creating it.
    pub fn get(&self, actor: ActorId) -> Option<&KillRing> {
        self.rings.get(&actor)
    }

    /// Returns an actor's history, creating it if necessary.
    pub fn get_mut(&mut self, actor: ActorId) -> &mut KillRing {
        self.rings.entry(actor).or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(text: &str, kind: RegisterKind) -> RegisterEntry {
        RegisterEntry {
            text: Text::from_str(text),
            kind,
        }
    }

    #[test]
    fn names_and_typed_payloads() {
        assert_eq!(RegisterName::new(""), None);
        assert_eq!(RegisterName::new("界").unwrap().as_str(), "界");
        let kinds = [
            RegisterKind::Charwise,
            RegisterKind::Linewise,
            RegisterKind::Blockwise,
        ];
        for (i, &a) in kinds.iter().enumerate() {
            for (j, &b) in kinds.iter().enumerate() {
                assert_eq!(entry("same", a) == entry("same", b), i == j);
            }
        }
    }

    #[test]
    fn exhaustive_register_sequences_and_commutativity() {
        let actors = [ActorId::new(0), ActorId::new(1)];
        let names = [
            RegisterName::new("a").unwrap(),
            RegisterName::new("b").unwrap(),
        ];
        let values = [
            entry("", RegisterKind::Charwise),
            entry("界\n", RegisterKind::Linewise),
            entry("x", RegisterKind::Blockwise),
        ];
        // Enumerate every sequence of three puts/removes over four keys and four actions.
        for sequence in 0..16_usize.pow(3) {
            let mut store = RegisterStore::new();
            let mut model = vec![None; 4];
            let mut code = sequence;
            for _ in 0..3 {
                let op = code % 16;
                code /= 16;
                let key = op / 4;
                let (actor, name) = (actors[key / 2], &names[key % 2]);
                if op % 4 == 0 {
                    assert_eq!(store.remove(actor, name), model[key].take());
                } else {
                    let value = values[op % 4 - 1].clone();
                    assert_eq!(
                        store.put(actor, name.clone(), value.clone()),
                        model[key].replace(value.clone())
                    );
                    assert_eq!(store.get(actor, name), Some(&value));
                }
                for k in 0..4 {
                    assert_eq!(store.get(actors[k / 2], &names[k % 2]), model[k].as_ref());
                }
            }
            // Every state after at most two operations occurs in these sequences.
            if sequence % 16 != 0 {
                continue;
            }
            for first in 0..4 {
                for second in 0..4 {
                    if first == second {
                        continue;
                    }
                    for a in 0..4 {
                        for b in 0..4 {
                            let apply = |s: &mut RegisterStore, key: usize, action: usize| {
                                let (actor, name) = (actors[key / 2], &names[key % 2]);
                                if action == 0 {
                                    s.remove(actor, name);
                                } else {
                                    s.put(actor, name.clone(), values[action - 1].clone());
                                }
                            };
                            let mut left = store.clone();
                            let mut right = store.clone();
                            apply(&mut left, first, a);
                            apply(&mut left, second, b);
                            apply(&mut right, second, b);
                            apply(&mut right, first, a);
                            assert_eq!(left, right);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn same_kind_last_write_wins_for_generated_texts() {
        let names = [
            RegisterName::new("a").unwrap(),
            RegisterName::new("b").unwrap(),
        ];
        let texts = ["old", "new", "界\n"];
        for actor in [ActorId::new(0), ActorId::new(1)] {
            for name in &names {
                for kind in [
                    RegisterKind::Charwise,
                    RegisterKind::Linewise,
                    RegisterKind::Blockwise,
                ] {
                    for first in texts {
                        for second in texts {
                            let mut store = RegisterStore::new();
                            let old = entry(first, kind);
                            let new = entry(second, kind);
                            assert_eq!(store.put(actor, name.clone(), old.clone()), None);
                            assert_eq!(store.put(actor, name.clone(), new.clone()), Some(old));
                            assert_eq!(store.get(actor, name), Some(&new));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn kill_history_order_rotation_and_isolation() {
        let mut histories = KillRings::default();
        let (a, b) = (ActorId::new(1), ActorId::new(2));
        assert_eq!(histories.get(a), None);
        let ring = histories.get_mut(a);
        assert!(!ring.append(&Text::from_str("x")));
        assert!(!ring.prepend(&Text::from_str("x")));
        assert_eq!(ring.rotate(), None);
        for kind in [
            RegisterKind::Charwise,
            RegisterKind::Linewise,
            RegisterKind::Blockwise,
        ] {
            for bits in 0..64 {
                let mut ring = KillRing::new();
                ring.push(entry("中心", kind));
                let mut expected = String::from("中心");
                for i in 0..6 {
                    let part = ["a", "界", "\n"][i % 3];
                    if bits & (1 << i) == 0 {
                        assert!(ring.append(&Text::from_str(part)));
                        expected.push_str(part);
                    } else {
                        assert!(ring.prepend(&Text::from_str(part)));
                        expected.insert_str(0, part);
                    }
                    assert_eq!(ring.current(), Some(&entry(&expected, kind)));
                }
            }
        }
        histories
            .get_mut(a)
            .push(entry("old", RegisterKind::Charwise));
        histories
            .get_mut(a)
            .push(entry("new", RegisterKind::Linewise));
        histories
            .get_mut(b)
            .push(entry("other", RegisterKind::Blockwise));
        let ring = histories.get_mut(a);
        assert_eq!(ring.nth(0), Some(&entry("new", RegisterKind::Linewise)));
        assert_eq!(ring.nth(2), None);
        assert_eq!(ring.rotate(), Some(&entry("old", RegisterKind::Charwise)));
        assert!(ring.append(&Text::from_str("+")));
        assert!(ring.prepend(&Text::from_str("-")));
        assert_eq!(
            ring.current(),
            Some(&entry("-old+", RegisterKind::Charwise))
        );
        assert_eq!(ring.rotate(), Some(&entry("new", RegisterKind::Linewise)));
        ring.push(entry("latest", RegisterKind::Blockwise));
        assert_eq!(
            ring.current(),
            Some(&entry("latest", RegisterKind::Blockwise))
        );
        assert_eq!(
            histories.get(b).unwrap().current(),
            Some(&entry("other", RegisterKind::Blockwise))
        );
    }
}
