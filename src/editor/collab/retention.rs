//! Bounded committed edit history with durable identity records.

use std::collections::BTreeMap;
use std::fmt;

use super::{
    proposal::RequestId,
    rebase::{rebase_over_step, RebaseError, RebaseStep},
    receipt::CommitReceipt,
};
use crate::editor::{
    edit::{ids::TxnId, transaction::Transaction},
    workspace::{
        catalog::BufferCatalog,
        snapshots::{SnapshotError, SnapshotStore, WorkspaceSnapshot},
        version::Revision,
    },
};

/// A rejected history operation. Rejections leave the history unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetentionError {
    /// The requested base has been discarded.
    BaseExpired { base: Revision, floor: Revision },
    /// The requested revision has not been committed.
    FutureRevision { revision: Revision, tip: Revision },
    /// A transaction identifier was reused with different content.
    TxnIdReuse(TxnId),
    /// A request identifier was reused with different content.
    RequestIdReuse(RequestId),
    /// A supplied committed transition does not match its receipt or snapshots.
    InvalidCommit,
    /// A retained edit could not be rebased.
    Rebase(RebaseError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{
        collab::{
            journal::CommitJournal, registry::ActorRegistry, role::ActorRole, sequencer::Sequencer,
        },
        edit::{
            history::EditHistory,
            ids::{ActorId, BufferId, Nonce},
            intent::EditIntent,
            patch::Patch,
            splice::Splice,
        },
        test_support::assert_law,
        text::{span::ByteSpan, utf8::Text},
        workspace::snapshots::VersionedWorkspace,
    };

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256(b"".to_vec()),
            [
                0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
                0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
                0x78, 0x52, 0xb8, 0x55,
            ]
        );
        assert_eq!(
            sha256(b"abc".to_vec()),
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
        );
    }

    #[test]
    fn digest_domains_and_snapshot_content_are_distinct() {
        let intent = EditIntent::for_actor(ActorId::new(8), Nonce::new(1), Revision::new(7));
        let transaction = Transaction::new(intent, vec![]).unwrap();
        let id = BufferId::new(7);
        let mut catalog = BufferCatalog::new();
        catalog
            .register(id, Text::from_str("\0\0\0\0\0\0\0\x08"))
            .unwrap();
        let mut workspace = VersionedWorkspace::new(catalog.clone());
        let mut altered = VersionedWorkspace::new(catalog);
        for _ in 0..8 {
            workspace
                .update(id, Text::from_str("\0\0\0\0\0\0\0\x08"))
                .unwrap();
            altered
                .update(id, Text::from_str("\0\0\0\0\0\0\0\x09"))
                .unwrap();
        }
        let snapshot = workspace.capture();
        assert_eq!(snapshot.revision(), Revision::new(8));
        assert_ne!(transaction_digest(&transaction), snapshot_digest(&snapshot));
        assert_ne!(
            snapshot_digest(&snapshot),
            snapshot_digest(&altered.capture())
        );
    }

    #[test]
    fn compacted_identity_discards_payload_and_validates_retries() {
        let id = BufferId::new(0);
        let marker = "distinctive superseded marker 界";
        let mut catalog = BufferCatalog::new();
        catalog.register(id, Text::from_str("start")).unwrap();
        let mut workspace = VersionedWorkspace::new(catalog.clone());
        let mut retained = RetainedHistory::new(workspace.capture());
        let mut records = Vec::new();
        for (i, text) in [marker, "final"].into_iter().enumerate() {
            let before = workspace.capture();
            let intent =
                EditIntent::for_actor(ActorId::new(1), Nonce::new(i as u64), before.revision());
            let patch = Patch::new(vec![Splice::new(
                ByteSpan::full_of(before.catalog().get(id).unwrap()),
                Text::from_str(text),
            )])
            .unwrap();
            let txn = Transaction::new(intent, vec![(id, patch)]).unwrap();
            workspace.update(id, Text::from_str(text)).unwrap();
            let after = workspace.capture();
            let receipt = CommitReceipt::new(intent, before.revision()).unwrap();
            let request = RequestId::new(i as u64);
            assert_eq!(
                retained.record(request, txn.clone(), txn.clone(), receipt, after.clone()),
                Ok(receipt)
            );
            records.push((request, txn, receipt, after));
        }
        let (request, original, receipt, after) = &records[0];
        let altered_forward = Transaction::new(original.intent(), vec![]).unwrap();
        let mut foreign = VersionedWorkspace::new(catalog.clone());
        foreign.update(id, Text::from_str(marker)).unwrap();
        let foreign_same = foreign.capture();
        let mut altered = VersionedWorkspace::new(catalog);
        altered.update(id, Text::from_str("different")).unwrap();
        let foreign_altered = altered.capture();
        assert_eq!(foreign_same.catalog(), after.catalog());
        assert_eq!(snapshot_digest(&foreign_same), snapshot_digest(after));
        assert_ne!(snapshot_digest(&foreign_altered), snapshot_digest(after));
        for compact in [false, true] {
            if compact {
                retained.compact(workspace.revision()).unwrap();
                assert!(!format!("{retained:?}").contains(marker));
            }
            for (forward, candidate) in [
                (altered_forward.clone(), after.clone()),
                (original.clone(), foreign_altered.clone()),
                (original.clone(), foreign_same.clone()),
            ] {
                let unchanged = retained.clone();
                assert_eq!(
                    retained.record(*request, original.clone(), forward, *receipt, candidate),
                    Err(RetentionError::InvalidCommit)
                );
                assert_eq!(retained, unchanged);
            }
            for (request, txn, receipt, _) in &records {
                assert_eq!(retained.receipt_by_txn(txn.intent().txn()), Some(*receipt));
                assert_eq!(retained.receipt_by_request(*request), Some(*receipt));
            }
        }
    }

    #[test]
    fn content_preserving_published_commit_survives_compaction() {
        let id = BufferId::new(0);
        let text = Text::from_str("unchanged 界");
        let mut catalog = BufferCatalog::new();
        catalog.register(id, text.clone()).unwrap();
        let mut workspace = VersionedWorkspace::new(catalog);
        let before = workspace.capture();
        let intent = EditIntent::for_actor(ActorId::new(1), Nonce::new(7), before.revision());
        let patch = Patch::new(vec![Splice::new(ByteSpan::full_of(&text), text.clone())]).unwrap();
        let txn = Transaction::new(intent, vec![(id, patch)]).unwrap();
        assert!(txn.apply_with_inverse(&before).unwrap().is_noop());
        workspace.update(id, text).unwrap();
        let after = workspace.capture();
        let receipt = CommitReceipt::new(intent, before.revision()).unwrap();
        let mut journal = CommitJournal::new(before.revision());
        journal.append(receipt).unwrap();
        let mut history = EditHistory::new();
        history
            .record(&before, after.revision(), after.catalog(), txn.clone())
            .unwrap();
        let mut retained = RetainedHistory::new(before.clone());
        let request = RequestId::new(8);
        assert_eq!(
            retained.record(request, txn.clone(), txn.clone(), receipt, after.clone()),
            Ok(receipt)
        );
        assert_eq!(
            retained.replay(before.revision()).unwrap(),
            *after.catalog()
        );
        assert_eq!(retained.replay(after.revision()).unwrap(), *after.catalog());
        retained.compact(after.revision()).unwrap();
        assert_eq!(retained.base_snapshot(), &after);
        assert_eq!(retained.receipt_by_txn(intent.txn()), Some(receipt));
        assert_eq!(retained.receipt_by_request(request), Some(receipt));
        let invalid_forward = Transaction::new(intent.with_base(after.revision()), vec![]).unwrap();
        let unchanged = retained.clone();
        assert_eq!(
            retained.record(
                request,
                txn.clone(),
                invalid_forward,
                receipt,
                before.clone()
            ),
            Err(RetentionError::InvalidCommit)
        );
        assert_eq!(retained, unchanged);
        assert_eq!(
            retained.record(request, txn.clone(), txn, receipt, after),
            Ok(receipt)
        );
        assert_eq!(
            retained.replay(before.revision()),
            Err(RetentionError::BaseExpired {
                base: before.revision(),
                floor: retained.floor()
            })
        );
    }

    #[test]
    fn generated_compaction_laws() {
        assert_law(
            &[0, 1, 42, 1234, u64::MAX],
            |rng| (0..20).map(|_| rng.gen_range(0..8)).collect::<Vec<_>>(),
            |choices| {
                let id = BufferId::new(0);
                let actor = ActorId::new(1);
                let mut catalog = BufferCatalog::new();
                catalog.register(id, Text::from_str("start")).unwrap();
                let mut actors = ActorRegistry::new();
                actors.register(actor, ActorRole::Human).unwrap();
                let mut sequencer = Sequencer::new(catalog, actors);
                let mut full = RetainedHistory::new(sequencer.snapshot());
                let mut records = Vec::new();
                for (i, choice) in choices.iter().enumerate() {
                    let before = sequencer.snapshot();
                    let old = before.catalog().get(id).unwrap();
                    let text = Text::from_str(&format!("{i}:{choice}:界"));
                    let patch =
                        Patch::new(vec![Splice::new(ByteSpan::full_of(old), text)]).unwrap();
                    let txn = Transaction::new(
                        EditIntent::for_actor(actor, Nonce::new(i as u64), before.revision()),
                        vec![(id, patch)],
                    )
                    .unwrap();
                    let receipt = sequencer.submit(txn.clone()).committed_receipt().unwrap();
                    let after = sequencer.snapshot();
                    let request = RequestId::new(i as u64);
                    let wrong = Transaction::new(txn.intent(), vec![]).unwrap();
                    let before_record = full.clone();
                    assert_eq!(
                        full.record(request, wrong, txn.clone(), receipt, after.clone()),
                        Err(RetentionError::InvalidCommit)
                    );
                    assert_eq!(full, before_record);
                    assert_eq!(
                        full.record(request, txn.clone(), txn.clone(), receipt, after.clone()),
                        Ok(receipt)
                    );
                    records.push((request, txn, receipt, after));
                }
                let mut compacted = full.clone();
                for target in choices
                    .iter()
                    .map(|choice| Revision::new((*choice * 3) as u64))
                    .chain(std::iter::once(full.tip()))
                {
                    let previous = compacted.clone();
                    if target > compacted.tip() {
                        assert_eq!(
                            compacted.compact(target),
                            Err(RetentionError::FutureRevision {
                                revision: target,
                                tip: compacted.tip()
                            })
                        );
                        assert_eq!(compacted, previous);
                        continue;
                    }
                    compacted.compact(target).unwrap();
                    assert_eq!(compacted.floor(), previous.floor().max(target));
                    if target <= previous.floor() {
                        assert_eq!(compacted, previous);
                    }
                    let fresh = Transaction::new(
                        EditIntent::for_actor(actor, Nonce::new(1000), Revision::ZERO),
                        vec![],
                    )
                    .unwrap();
                    let fresh_receipt =
                        CommitReceipt::new(fresh.intent(), compacted.tip()).unwrap();
                    let unchanged = compacted.clone();
                    assert_eq!(
                        compacted.record(
                            records[0].0,
                            fresh.clone(),
                            fresh.clone(),
                            fresh_receipt,
                            sequencer.snapshot()
                        ),
                        Err(RetentionError::RequestIdReuse(records[0].0))
                    );
                    assert_eq!(compacted, unchanged);
                    if compacted.floor() > Revision::ZERO {
                        assert_eq!(
                            compacted.record(
                                RequestId::new(1000),
                                fresh.clone(),
                                fresh.clone(),
                                fresh_receipt,
                                sequencer.snapshot()
                            ),
                            Err(RetentionError::BaseExpired {
                                base: Revision::ZERO,
                                floor: compacted.floor()
                            })
                        );
                        assert_eq!(compacted, unchanged);
                    }
                    for n in 0..=compacted.tip().raw() {
                        let base = Revision::new(n);
                        let proposal = Transaction::new(
                            EditIntent::for_actor(actor, Nonce::new(999), base),
                            vec![],
                        )
                        .unwrap();
                        if base < compacted.floor() {
                            let error = RetentionError::BaseExpired {
                                base,
                                floor: compacted.floor(),
                            };
                            assert_eq!(compacted.replay(base), Err(error.clone()));
                            assert_eq!(compacted.rebase(&proposal), Err(error));
                        } else {
                            assert_eq!(compacted.replay(base), full.replay(base));
                            assert_eq!(
                                compacted.replay(base).unwrap(),
                                *sequencer.snapshot().catalog()
                            );
                            assert_eq!(compacted.rebase(&proposal), full.rebase(&proposal));
                        }
                    }
                    for (request, txn, receipt, after) in &records {
                        assert_eq!(compacted.receipt_by_txn(txn.intent().txn()), Some(*receipt));
                        assert_eq!(compacted.receipt_by_request(*request), Some(*receipt));
                        let before = compacted.clone();
                        assert_eq!(
                            compacted.record(
                                *request,
                                txn.clone(),
                                txn.clone(),
                                *receipt,
                                after.clone()
                            ),
                            Ok(*receipt)
                        );
                        assert_eq!(compacted, before);
                        let changed = Transaction::new(txn.intent(), vec![]).unwrap();
                        assert_eq!(
                            compacted.record(
                                *request,
                                changed,
                                txn.clone(),
                                *receipt,
                                after.clone()
                            ),
                            Err(RetentionError::TxnIdReuse(txn.intent().txn()))
                        );
                        assert_eq!(compacted, before);
                    }
                }
            },
        );
    }
}

impl fmt::Display for RetentionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "retention operation failed: {self:?}")
    }
}
impl std::error::Error for RetentionError {}

// Scalars and lengths are encoded as big-endian u64; sequences carry a count.
fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_len(bytes: &mut Vec<u8>, value: usize) {
    put_u64(bytes, u64::try_from(value).expect("length fits u64"));
}

fn transaction_digest(transaction: &Transaction) -> [u8; 32] {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"zen:retention:txn:v1\0");
    let intent = transaction.intent();
    put_u64(&mut bytes, intent.actor().raw());
    put_u64(&mut bytes, intent.nonce().raw());
    put_u64(&mut bytes, intent.base().raw());
    put_u64(&mut bytes, intent.txn().actor().raw());
    put_u64(&mut bytes, intent.txn().nonce().raw());
    put_len(&mut bytes, transaction.patches().len());
    for (id, patch) in transaction.patches() {
        put_u64(&mut bytes, id.raw());
        put_len(&mut bytes, patch.splices().len());
        for splice in patch.splices() {
            put_len(&mut bytes, splice.span().start().as_usize());
            put_len(&mut bytes, splice.span().end().as_usize());
            let replacement = splice.replacement().as_bytes();
            put_len(&mut bytes, replacement.len());
            bytes.extend_from_slice(replacement);
        }
    }
    sha256(bytes)
}

fn snapshot_digest(snapshot: &WorkspaceSnapshot) -> [u8; 32] {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"zen:retention:snapshot:v1\0");
    put_u64(&mut bytes, snapshot.revision().raw());
    let catalog = snapshot.catalog();
    let live = catalog.live_ids();
    put_len(&mut bytes, live.len());
    for id in live {
        put_u64(&mut bytes, id.raw());
        let text = catalog.get(id).expect("live buffer").as_bytes();
        put_len(&mut bytes, text.len());
        bytes.extend_from_slice(text);
    }
    let tombstones = catalog.tombstoned_ids();
    put_len(&mut bytes, tombstones.len());
    for id in tombstones {
        put_u64(&mut bytes, id.raw());
    }
    sha256(bytes)
}

// FIPS 180-4 SHA-256, implemented here to keep retention independent of dependencies.
fn sha256(mut bytes: Vec<u8>) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    bytes.push(0x80);
    while bytes.len() % 64 != 56 {
        bytes.push(0);
    }
    bytes.extend_from_slice(&bit_len.to_be_bytes());
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    for block in bytes.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (word, chunk) in w.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*chunk);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (value, add) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *value = value.wrapping_add(add);
        }
    }
    let mut digest = [0; 32];
    for (chunk, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        *chunk = word.to_be_bytes();
    }
    digest
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    request: RequestId,
    receipt: CommitReceipt,
    original_digest: [u8; 32],
    forward_digest: [u8; 32],
    after_digest: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Step {
    forward: Transaction,
    after: WorkspaceSnapshot,
}

/// Retains a floor snapshot and validated transitions after it.
///
/// Compaction removes only transitions and snapshots below the new floor;
/// fixed-size digests of submitted transactions and committed artifacts, along
/// with receipts, remain in the identity indexes.
/// Each admitted transition is checked against its before and after snapshots,
/// so replay of any retained suffix gives the same catalog as the full chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedHistory {
    base: WorkspaceSnapshot,
    steps: Vec<Step>,
    snapshots: SnapshotStore,
    identities: BTreeMap<TxnId, Identity>,
    requests: BTreeMap<RequestId, TxnId>,
}

impl RetainedHistory {
    /// Starts with an actual workspace capture at the initial floor.
    pub fn new(base: WorkspaceSnapshot) -> Self {
        let mut snapshots = SnapshotStore::new();
        snapshots
            .record(base.clone())
            .expect("empty snapshot store");
        Self {
            base,
            steps: Vec::new(),
            snapshots,
            identities: BTreeMap::new(),
            requests: BTreeMap::new(),
        }
    }

    /// Returns the oldest available revision.
    pub fn floor(&self) -> Revision {
        self.base.revision()
    }
    /// Returns the current committed revision.
    pub fn tip(&self) -> Revision {
        self.steps
            .last()
            .map_or(self.floor(), |step| step.after.revision())
    }
    /// Returns the capture at the retention floor.
    pub fn base_snapshot(&self) -> &WorkspaceSnapshot {
        &self.base
    }
    /// Finds a committed transaction's original receipt, including after compaction.
    pub fn receipt_by_txn(&self, txn: TxnId) -> Option<CommitReceipt> {
        self.identities.get(&txn).map(|item| item.receipt)
    }
    /// Finds a committed request's original receipt, including after compaction.
    pub fn receipt_by_request(&self, request: RequestId) -> Option<CommitReceipt> {
        self.requests
            .get(&request)
            .and_then(|txn| self.receipt_by_txn(*txn))
    }

    fn index(&self, revision: Revision) -> Result<usize, RetentionError> {
        if revision < self.floor() {
            return Err(RetentionError::BaseExpired {
                base: revision,
                floor: self.floor(),
            });
        }
        if revision > self.tip() {
            return Err(RetentionError::FutureRevision {
                revision,
                tip: self.tip(),
            });
        }
        Ok(
            usize::try_from(revision.steps_since(self.floor()).expect("checked floor"))
                .expect("bounded by steps"),
        )
    }

    fn snapshot_at_index(&self, index: usize) -> &WorkspaceSnapshot {
        if index == 0 {
            &self.base
        } else {
            &self.steps[index - 1].after
        }
    }

    fn same_origin(&self, snapshot: &WorkspaceSnapshot) -> bool {
        let mut probe = SnapshotStore::new();
        probe
            .record(self.base.clone())
            .expect("empty snapshot store");
        !matches!(
            probe.record(snapshot.clone()),
            Err(SnapshotError::DifferentWorkspace)
        )
    }

    /// Records an already published commit; the caller guarantees publication.
    /// An identical retry returns its original receipt before checking the floor.
    pub fn record(
        &mut self,
        request: RequestId,
        original: Transaction,
        forward: Transaction,
        receipt: CommitReceipt,
        after: WorkspaceSnapshot,
    ) -> Result<CommitReceipt, RetentionError> {
        let txn = original.intent().txn();
        if let Some(existing) = self.identities.get(&txn) {
            if receipt.committed() != after.revision()
                || forward.intent() != original.intent().with_base(receipt.previous())
            {
                return Err(RetentionError::InvalidCommit);
            }
            if existing.request != request
                || existing.original_digest != transaction_digest(&original)
                || existing.receipt != receipt
            {
                return Err(RetentionError::TxnIdReuse(txn));
            }
            if !self.same_origin(&after)
                || existing.forward_digest != transaction_digest(&forward)
                || existing.after_digest != snapshot_digest(&after)
            {
                return Err(RetentionError::InvalidCommit);
            }
            return Ok(existing.receipt);
        }
        if self.requests.contains_key(&request) {
            return Err(RetentionError::RequestIdReuse(request));
        }
        self.index(original.intent().base())?;
        if self
            .rebase(&original)
            .map_err(|_| RetentionError::InvalidCommit)?
            != forward
        {
            return Err(RetentionError::InvalidCommit);
        }
        let before = self.snapshot_at_index(self.steps.len());
        if receipt.intent() != original.intent()
            || receipt.previous() != self.tip()
            || receipt.committed() != after.revision()
            || forward.intent() != original.intent().with_base(self.tip())
        {
            return Err(RetentionError::InvalidCommit);
        }
        let result = forward
            .apply_with_inverse(before)
            .map_err(|_| RetentionError::InvalidCommit)?;
        if result.catalog() != after.catalog() {
            return Err(RetentionError::InvalidCommit);
        }
        let mut snapshots = self.snapshots.clone();
        snapshots
            .record(after.clone())
            .map_err(|_| RetentionError::InvalidCommit)?;
        let identity = Identity {
            request,
            receipt,
            original_digest: transaction_digest(&original),
            forward_digest: transaction_digest(&forward),
            after_digest: snapshot_digest(&after),
        };
        self.steps.push(Step { forward, after });
        self.snapshots = snapshots;
        self.identities.insert(txn, identity);
        self.requests.insert(request, txn);
        Ok(receipt)
    }

    /// Replays all commits after `base` and returns the resulting catalog.
    pub fn replay(&self, base: Revision) -> Result<BufferCatalog, RetentionError> {
        let index = self.index(base)?;
        let mut catalog = self.snapshot_at_index(index).catalog().clone();
        for step in &self.steps[index..] {
            catalog = step
                .forward
                .apply_to_catalog(&catalog)
                .map_err(|_| RetentionError::InvalidCommit)?;
        }
        Ok(catalog)
    }

    /// Rebases a proposal from its base over all retained intervening commits.
    pub fn rebase(&self, proposal: &Transaction) -> Result<Transaction, RetentionError> {
        let index = self.index(proposal.intent().base())?;
        let mut rebased = proposal.clone();
        for i in index..self.steps.len() {
            rebased = rebase_over_step(
                &rebased,
                RebaseStep {
                    before: self.snapshot_at_index(i),
                    committed: &self.steps[i].forward,
                    after: &self.steps[i].after,
                },
            )
            .map_err(RetentionError::Rebase)?;
        }
        Ok(rebased)
    }

    /// Advances the floor, discarding older transitions but never identity records.
    pub fn compact(&mut self, floor: Revision) -> Result<(), RetentionError> {
        if floor <= self.floor() {
            return Ok(());
        }
        let index = self.index(floor)?;
        self.base = self.snapshot_at_index(index).clone();
        self.steps.drain(..index);
        self.snapshots.prune_before(floor);
        Ok(())
    }
}
