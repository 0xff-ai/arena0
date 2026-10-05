//! Commit-ordered change publication for one Host's store (design §3.2).
//!
//! Every committed transaction that writes a row a summary read returns
//! (`ExecSummaryRow`, an agreed step, `ReceiptSummaryRow`, a program, a blob)
//! names that row by [`ChangeKey`]. On `COMMIT`, while the writer mutex is
//! still held, the store assigns each key the next `seq` of this store's
//! lifetime, appends it to a fixed-capacity ring and then sets the published
//! head (a `watch` value) to the last seq. A rolled back transaction publishes
//! nothing. `seq` order is therefore commit order.
//!
//! Subscribers watch only the head and read what they missed from the ring
//! with `changes_since`, so a slow subscriber coalesces instead of lagging;
//! when the ring no longer reaches back far enough it takes a snapshot.
//!
//! The log lives in memory and starts empty at `seq` 0 each time the store is
//! opened; [`StoreHandle::boot_id`](crate::StoreHandle::boot_id) names that
//! lifetime. A cursor `(boot_id, seq)` from an earlier lifetime cannot resume.
//!
//! Keys carry no row values. A reader projects the current row when it sends,
//! so a key's row may already reflect later changes; consumers apply rows as
//! idempotent upserts.

use std::collections::VecDeque;

use arena0_program::ProgramHash;
use arena0_protocol::{BlobHash, ExecId, ReceiptId};
use tokio::sync::watch;

/// Capacity of the per-store ring of recent changes (design §3.2, ruling 2).
pub const CHANGE_RING: usize = 8_192;

/// The summary row one committed write changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ChangeKey {
    /// The execution request, activation or execution columns of this id
    /// (anything `exec_summary(id)` returns).
    Exec(ExecId),
    /// Agreed step `step` of this execution was stored. Steps are immutable
    /// once stored.
    Step { exec_id: ExecId, step: u64 },
    /// A receipt artifact row, or a production/import fact about it.
    Receipt(ReceiptId),
    /// A program was registered or removed.
    Program(ProgramHash),
    /// A blob was stored (owned copy, link or received file).
    Blob(BlobHash),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_sequences_watch_and_catchup() {
        let mut log = ChangeLog::new();
        let watch = log.subscribe();
        let a = ChangeKey::Exec(ExecId([1; 32]));
        let b = ChangeKey::Exec(ExecId([2; 32]));
        log.publish([a, b]);
        assert_eq!(*watch.borrow(), 2);
        assert_eq!(
            log.ring.iter().copied().collect::<Vec<_>>(),
            vec![(1, a), (2, b)]
        );
        log.publish([b, a]);
        assert_eq!(*watch.borrow(), 4);
        assert_eq!(
            log.since(0),
            Catchup::Keys {
                head: 4,
                keys: vec![a, b]
            }
        );
        assert_eq!(
            log.since(4),
            Catchup::Keys {
                head: 4,
                keys: vec![]
            }
        );
        assert_eq!(log.since(5), Catchup::Snapshot { head: 4 });
        log.publish([]);
        assert_eq!(*watch.borrow(), 4);
        for previous in 4..=CHANGE_RING {
            log.publish([a]);
            assert_eq!(*watch.borrow(), previous as u64 + 1);
        }
        let head = CHANGE_RING as u64 + 1;
        assert_eq!(*watch.borrow(), head);
        assert_eq!(log.ring.len(), CHANGE_RING);
        assert_eq!(log.since(0), Catchup::Snapshot { head });
        assert_eq!(
            log.since(head - CHANGE_RING as u64),
            Catchup::Keys {
                head,
                keys: vec![b, a]
            }
        );
    }
}

/// What a reader that has applied every change up to `seq` must fetch to be
/// current, from [`StoreHandle::changes_since`](crate::StoreHandle::changes_since).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Catchup {
    /// The ring holds every change after `seq`: these keys, deduplicated, in
    /// first-seen order. Fetching their current rows makes the reader current
    /// as of `head`.
    Keys { head: u64, keys: Vec<ChangeKey> },
    /// The ring no longer holds some change after `seq` (or `seq` is ahead of
    /// `head`): the reader must take a full snapshot, current as of `head`.
    Snapshot { head: u64 },
}

/// The in-memory log: the ring of `(seq, key)` and the head `watch` sender.
/// Shared by the store's `Inner` and its `Database`; `Database` publishes
/// from `commit_result` while holding the writer mutex, so publication order
/// equals commit order. The ring is updated before the head is sent, so a
/// reader woken by head `h` finds every change up to `h` in the ring.
pub(crate) struct ChangeLog {
    /// The last `CHANGE_RING` changes, oldest first, seqs consecutive.
    ring: VecDeque<(u64, ChangeKey)>,
    /// The last assigned seq, published; 0 before any change.
    head: watch::Sender<u64>,
}

impl ChangeLog {
    /// An empty log at `seq` 0 for a newly opened store.
    pub(crate) fn new() -> Self {
        Self {
            ring: VecDeque::with_capacity(CHANGE_RING),
            head: watch::Sender::new(0),
        }
    }

    /// Assign the next seqs to `keys` (in order), append them to the ring
    /// (evicting the oldest beyond [`CHANGE_RING`]) and send the new head.
    /// Called only from `Database::commit_result` after a successful `COMMIT`.
    pub(crate) fn publish(&mut self, keys: impl IntoIterator<Item = ChangeKey>) {
        let mut last = *self.head.borrow();
        let before = last;
        for key in keys {
            last += 1;
            self.ring.push_back((last, key));
            if self.ring.len() > CHANGE_RING {
                self.ring.pop_front();
            }
        }
        if last != before {
            self.head.send_replace(last);
        }
    }

    /// A receiver of the published head; its current value is the head now.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.head.subscribe()
    }

    /// What a reader at `seq` must fetch (see `Catchup`); the rules are the ones the contract gives for
    /// `StoreHandle::changes_since`.
    pub(crate) fn since(&self, seq: u64) -> Catchup {
        let head = *self.head.borrow();
        if seq == head {
            return Catchup::Keys {
                head,
                keys: Vec::new(),
            };
        }
        if seq > head || self.ring.front().is_none_or(|(first, _)| *first > seq + 1) {
            return Catchup::Snapshot { head };
        }
        let mut seen = std::collections::HashSet::new();
        let keys = self
            .ring
            .iter()
            .filter(|(s, _)| *s > seq)
            .filter_map(|(_, key)| seen.insert(*key).then_some(*key))
            .collect();
        Catchup::Keys { head, keys }
    }
}
