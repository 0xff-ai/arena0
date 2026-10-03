//! Host-local discovery state. Entries and their events change under the same
//! lock, so an observer reacting to an event sees the corresponding list change.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arena0_api::{OfferClosedReason, OpenOffer};
use arena0_program::ProgramHash;
use arena0_protocol::{NegotiationId, PeerId};
use tokio::task::AbortHandle;

use crate::server::{Events, HostEvent};

pub(crate) const MAX_OPEN_OFFERS: usize = 256;
pub(crate) const OFFER_SWEEP: Duration = Duration::from_secs(5);

struct Entry {
    offer: OpenOffer,
}

/// Open offers per Host. Owned by `HostService`.
pub(crate) struct OfferBook {
    entries: Mutex<BTreeMap<(ProgramHash, PeerId, NegotiationId), Entry>>,
    /// Abort handles of watcher tasks spawned into `HostService::tasks`;
    /// unwatch aborts one, Host stop aborts all through that task set.
    pub(super) watchers: Mutex<HashMap<ProgramHash, AbortHandle>>,
}

impl OfferBook {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(BTreeMap::new()),
            watchers: Mutex::new(HashMap::new()),
        })
    }

    /// Remove expired entries before returning the current ordered projection.
    /// Every expiration emits a Host-local `OfferClosed` event.
    pub(crate) fn list(&self, now_ms: u64, events: &Events) -> Vec<OpenOffer> {
        self.expire(now_ms, events);
        self.entries
            .lock()
            .expect("offer entries")
            .values()
            .map(|entry| entry.offer.clone())
            .collect()
    }

    pub(super) fn insert(&self, mut offer: OpenOffer, events: &Events) {
        let mut entries = self.entries.lock().expect("offer entries");
        let key = (offer.program_id, offer.creator, offer.negotiation_id);
        if let Some(entry) = entries.get(&key) {
            if offer.offer_seq < entry.offer.offer_seq {
                return;
            }
            offer.first_seen_ms = entry.offer.first_seen_ms;
        } else {
            events.emit(HostEvent::OfferSeen {
                program_id: offer.program_id,
                negotiation_id: offer.negotiation_id,
                creator: offer.creator,
                offer_seq: offer.offer_seq,
            });
        }
        entries.insert(key, Entry { offer });
        if entries.len() > MAX_OPEN_OFFERS {
            // Eviction bounds local memory; it does not assert that the remote
            // negotiation closed. BTree order breaks equal-time ties stably.
            let oldest = *entries
                .iter()
                .min_by_key(|(_, entry)| entry.offer.first_seen_ms)
                .expect("over-capacity offer book is nonempty")
                .0;
            entries.remove(&oldest);
        }
    }

    pub(super) fn close(
        &self,
        key: (ProgramHash, PeerId, NegotiationId),
        reason: OfferClosedReason,
        events: &Events,
    ) {
        let mut entries = self.entries.lock().expect("offer entries");
        if entries.remove(&key).is_some() {
            events.emit(HostEvent::OfferClosed {
                program_id: key.0,
                creator: key.1,
                negotiation_id: key.2,
                reason,
            });
        }
    }

    pub(super) fn expire(&self, now_ms: u64, events: &Events) {
        self.entries
            .lock()
            .expect("offer entries")
            .retain(|key, entry| {
                if entry.offer.deadline_unix_ms > now_ms {
                    return true;
                }
                events.emit(HostEvent::OfferClosed {
                    program_id: key.0,
                    creator: key.1,
                    negotiation_id: key.2,
                    reason: OfferClosedReason::Expired,
                });
                false
            });
    }

    pub(super) fn drop_program(&self, program_id: ProgramHash, events: &Events) {
        self.entries
            .lock()
            .expect("offer entries")
            .retain(|key, _| {
                if key.0 != program_id {
                    return true;
                }
                events.emit(HostEvent::OfferClosed {
                    program_id: key.0,
                    creator: key.1,
                    negotiation_id: key.2,
                    reason: OfferClosedReason::Unwatched,
                });
                false
            });
    }
}
