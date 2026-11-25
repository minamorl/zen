//! Append-only journal of committed edits.

use std::fmt;

use crate::editor::collab::receipt::CommitReceipt;
use crate::editor::edit::ids::TxnId;
use crate::editor::workspace::version::Revision;

/// A rejected append or invalid range query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalError {
    /// The receipt does not follow the current tip.
    NonContiguous { tip: Revision, previous: Revision },
    /// The transaction was already committed with a different receipt.
    TxnIdReuse { txn: TxnId },
    /// The range boundaries are inverted or outside the journal span.
    InvalidRange { start: Revision, end: Revision },
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonContiguous { tip, previous } => {
                write!(
                    f,
                    "receipt previous revision {previous} does not match tip {tip}"
                )
            }
            Self::TxnIdReuse { txn } => write!(f, "transaction ID {txn} already committed"),
            Self::InvalidRange { start, end } => write!(f, "invalid journal range {start}..{end}"),
        }
    }
}

impl std::error::Error for JournalError {}

/// An append-only sequence of receipts beginning at a revision boundary.
///
/// # Laws
///
/// * **Contiguity:** Initially the tip equals the base. A new receipt is
///   appended only when its previous revision equals the tip. Since receipt
///   construction advances exactly one revision, each append advances the tip
///   by one. A rejected append changes nothing.
/// * **Range composition:** A range contains exactly the receipts after its
///   start boundary through its end boundary. The corresponding contiguous
///   slices satisfy `range(a, c) == range(a, b) ++ range(b, c)` for every
///   `base <= a <= b <= c <= tip`, including empty slices.
/// * **Identity and lookup:** Each transaction occurs at most once. Appending
///   an identical receipt again succeeds without changing the journal; a
///   different receipt with the same transaction is rejected without change.
///   Before writing, append searches by the complete transaction ID and
///   compares the entire receipt, returning immediately on a match or reuse.
///   Transaction lookup uses that same ID comparison; contiguity puts the
///   receipt for a committed revision at offset `revision - base - 1`.
///   Both lookups therefore return the exact stored receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitJournal {
    base: Revision,
    receipts: Vec<CommitReceipt>,
}

impl Default for CommitJournal {
    fn default() -> Self {
        Self::new(Revision::ZERO)
    }
}

impl CommitJournal {
    /// Creates an empty journal at `base`.
    pub const fn new(base: Revision) -> Self {
        Self {
            base,
            receipts: Vec::new(),
        }
    }

    /// Returns the initial revision boundary.
    pub const fn base(&self) -> Revision {
        self.base
    }

    /// Returns the latest committed revision, or the base when empty.
    pub fn tip(&self) -> Revision {
        self.receipts
            .last()
            .map_or(self.base, |receipt| receipt.committed())
    }

    /// Returns the number of distinct committed receipts.
    pub fn len(&self) -> usize {
        self.receipts.len()
    }

    /// Returns whether the journal has no receipts.
    pub fn is_empty(&self) -> bool {
        self.receipts.is_empty()
    }

    /// Appends a contiguous receipt, or accepts an identical retry unchanged.
    ///
    /// Transaction reuse is checked before contiguity so a conflicting retry
    /// always reports [`JournalError::TxnIdReuse`]. Errors leave all state intact.
    pub fn append(&mut self, receipt: CommitReceipt) -> Result<(), JournalError> {
        if let Some(recorded) = self.lookup_txn(receipt.txn()) {
            return if *recorded == receipt {
                Ok(())
            } else {
                Err(JournalError::TxnIdReuse { txn: receipt.txn() })
            };
        }
        let tip = self.tip();
        if receipt.previous() != tip {
            return Err(JournalError::NonContiguous {
                tip,
                previous: receipt.previous(),
            });
        }
        self.receipts.push(receipt);
        Ok(())
    }

    /// Finds the exact receipt for a committed transaction.
    pub fn lookup_txn(&self, txn: TxnId) -> Option<&CommitReceipt> {
        self.receipts.iter().find(|receipt| receipt.txn() == txn)
    }

    /// Finds the receipt committed at `revision`; the base has no receipt.
    pub fn lookup_revision(&self, revision: Revision) -> Option<&CommitReceipt> {
        let offset = revision.steps_since(self.base)?.checked_sub(1)?;
        self.receipts.get(usize::try_from(offset).ok()?)
    }

    /// Returns receipts whose committed revisions are in `(start, end]`.
    ///
    /// Both boundaries must lie in the inclusive span `base..=tip` and start
    /// must not exceed end. Equal boundaries yield an empty slice. Invalid
    /// boundaries return [`JournalError::InvalidRange`], without changing state.
    pub fn range(&self, start: Revision, end: Revision) -> Result<&[CommitReceipt], JournalError> {
        if start < self.base || start > end || end > self.tip() {
            return Err(JournalError::InvalidRange { start, end });
        }
        // Contiguity makes both offsets bounded by the vector length.
        let start = usize::try_from(start.steps_since(self.base).unwrap()).unwrap();
        let end = usize::try_from(end.steps_since(self.base).unwrap()).unwrap();
        Ok(&self.receipts[start..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::edit::ids::{ActorId, Nonce};
    use crate::editor::edit::intent::EditIntent;

    fn receipt(nonce: u64, intent_base: u64, previous: u64) -> CommitReceipt {
        CommitReceipt::new(
            EditIntent::for_actor(
                ActorId::new(1),
                Nonce::new(nonce),
                Revision::new(intent_base),
            ),
            Revision::new(previous),
        )
        .unwrap()
    }

    #[test]
    fn same_nonce_for_different_actors_is_distinct() {
        let mut journal = CommitJournal::default();
        let first = receipt(7, 0, 0);
        let second = CommitReceipt::new(
            EditIntent::for_actor(ActorId::new(2), Nonce::new(7), Revision::ZERO),
            Revision::new(1),
        )
        .unwrap();
        assert_eq!(journal.append(first), Ok(()));
        assert_eq!(journal.append(second), Ok(()));
        assert_eq!(journal.lookup_txn(first.txn()), Some(&first));
        assert_eq!(journal.lookup_txn(second.txn()), Some(&second));
        assert_eq!(journal.len(), 2);
    }

    #[test]
    fn descending_txn_order_supports_lookup_and_retry() {
        let mut journal = CommitJournal::default();
        let first = receipt(7, 0, 0);
        let second = receipt(2, 0, 1);
        assert_eq!(journal.append(first), Ok(()));
        assert_eq!(journal.append(second), Ok(()));
        assert_eq!(journal.lookup_txn(first.txn()), Some(&first));
        assert_eq!(journal.lookup_txn(second.txn()), Some(&second));
        let before = journal.clone();
        assert_eq!(journal.append(first), Ok(()));
        assert_eq!(journal, before);
    }

    #[test]
    fn fresh_txn_cannot_append_at_base_after_commit() {
        let mut journal = CommitJournal::default();
        assert_eq!(journal.append(receipt(7, 0, 0)), Ok(()));
        let before = journal.clone();
        assert_eq!(
            journal.append(receipt(2, 0, 0)),
            Err(JournalError::NonContiguous {
                tip: Revision::new(1),
                previous: Revision::ZERO,
            })
        );
        assert_eq!(journal, before);
    }

    #[test]
    fn exhaustive_small_sequences_and_ranges() {
        assert_eq!(CommitJournal::default().base(), Revision::ZERO);
        for base in [0, 1, 20, u64::MAX - 4] {
            for count in 0..=4_u64 {
                let mut journal = CommitJournal::new(Revision::new(base));
                let mut expected = Vec::new();
                for index in 0..count {
                    let item = receipt(index, base, base + index);
                    assert_eq!(journal.append(item), Ok(()));
                    expected.push(item);
                    assert_eq!(journal.tip(), item.committed());
                    assert_eq!(journal.len(), expected.len());
                    assert_eq!(journal.lookup_txn(item.txn()), Some(&item));
                    for recorded in &expected {
                        assert_eq!(
                            journal.lookup_revision(recorded.committed()),
                            Some(recorded)
                        );
                    }
                }
                assert_eq!(journal.is_empty(), count == 0);
                assert_eq!(journal.lookup_revision(Revision::new(base)), None);
                for a in 0..=count {
                    for b in a..=count {
                        for c in b..=count {
                            let boundary = |offset| Revision::new(base + offset);
                            let whole = journal.range(boundary(a), boundary(c)).unwrap();
                            let left = journal.range(boundary(a), boundary(b)).unwrap();
                            let right = journal.range(boundary(b), boundary(c)).unwrap();
                            assert_eq!(whole, &expected[a as usize..c as usize]);
                            assert_eq!(whole, [left, right].concat());
                        }
                    }
                }
                for item in &expected {
                    let before = journal.clone();
                    assert_eq!(journal.append(*item), Ok(()));
                    assert_eq!(journal, before);
                    let changed_intent = receipt(
                        item.txn().nonce().raw(),
                        base.wrapping_add(1),
                        item.previous().raw(),
                    );
                    assert_eq!(
                        journal.append(changed_intent),
                        Err(JournalError::TxnIdReuse { txn: item.txn() })
                    );
                    assert_eq!(journal, before);
                    let changed_revision = receipt(
                        item.txn().nonce().raw(),
                        base,
                        if item.previous().raw() == base {
                            base + 1
                        } else {
                            base
                        },
                    );
                    assert_eq!(
                        journal.append(changed_revision),
                        Err(JournalError::TxnIdReuse { txn: item.txn() })
                    );
                    assert_eq!(journal, before);
                }
                assert_eq!(journal.lookup_txn(receipt(99, base, base).txn()), None);
                if base > 0 {
                    assert_eq!(journal.lookup_revision(Revision::new(base - 1)), None);
                    assert!(journal
                        .range(Revision::new(base - 1), journal.tip())
                        .is_err());
                }
                if journal.tip() < Revision::MAX {
                    assert_eq!(journal.lookup_revision(journal.tip().next().unwrap()), None);
                    assert!(journal
                        .range(journal.base(), journal.tip().next().unwrap())
                        .is_err());
                }
                if count > 0 {
                    assert!(journal.range(journal.tip(), journal.base()).is_err());
                }
                let before = journal.clone();
                if let Ok(future) = journal.tip().next() {
                    if future < Revision::MAX {
                        let bad = receipt(99, base, future.raw());
                        assert_eq!(
                            journal.append(bad),
                            Err(JournalError::NonContiguous {
                                tip: journal.tip(),
                                previous: future,
                            })
                        );
                        assert_eq!(journal, before);
                    }
                }
                let previous = if base == 0 { base + 1 } else { base - 1 };
                let bad = receipt(99, base, previous);
                if bad.previous() != journal.tip() {
                    assert_eq!(
                        journal.append(bad),
                        Err(JournalError::NonContiguous {
                            tip: journal.tip(),
                            previous: bad.previous()
                        })
                    );
                    assert_eq!(journal, before);
                }
            }
        }
    }
}
