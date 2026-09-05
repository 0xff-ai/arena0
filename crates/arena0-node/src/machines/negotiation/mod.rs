//! Session negotiation: the book, the driver, and the shared
//! contracts.
//!
//! A [`NegotiationBook`] tracks every offer of a negotiation (one current
//! signed ticket per `PeerId` per offer `(negotiation_id, offer_seq)`) and
//! one non-binding counteroffer per `(negotiation_id, signer)`. It enforces
//! the monotonic revision discipline and the 64-peer bound. Expired tickets
//! are **not** discarded: they remain valid evidence (temporal validation
//! modes are the caller's decision — pre-commit admission vs post-commit
//! certificate verification). Expired counteroffers are discarded.
//!
//! The book also owns the local node's negotiation identity: it issues the
//! local ticket for any tracked offer (only after the offer is authenticated
//! by the creator's Active ticket, or for the creator itself) and emits the
//! local counteroffer. The driver runs one local node through the
//! negotiation flow until it durably commits the activation or gives up.

mod driver;
mod support;

use std::collections::HashMap;

use arena0_crypto::{ExecutionKey, NodeKeys};
use arena0_protocol::{
    Counteroffer, CounterofferData, CounterofferHash, NegotiationId, Offer, OfferHash, PeerId,
    PeerIdSource, Ticket, TicketAction, TicketData,
};
use thiserror::Error;

pub(crate) use driver::NegotiationDriver;
pub use support::{
    DurableOutcome, FETCH_TIMEOUT, LocalTicketWithdrawal, NegotiationAttempt,
    NegotiationDriveError, NegotiationEffects, NegotiationStart, NegotiationSupervision,
    PersistActivationEffect, PrepareEffect, PrepareOutcome, RecomputeInitialStateEffect,
    serve_fetch_evidence, unix_time_ms,
};

/// At most 64 peers in one book, matching
/// [`MAX_PARTICIPANTS`](arena0_protocol::MAX_PARTICIPANTS).
pub(crate) const MAX_HEADS: usize = 64;

/// At most this many offer slots per negotiation. The creator's own re-offers
/// are bounded by `REOFFER_MAX_ATTEMPTS` (8), so 16 leaves headroom while
/// bounding a malicious creator's unbounded re-offer sequence on the
/// participant side.
pub(crate) const MAX_OFFERS_PER_NEGOTIATION: usize = 16;

/// What an accepted [`NegotiationBook::apply_ticket`] or
/// [`NegotiationBook::apply_counteroffer`] did to the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// A brand-new signer started.
    Inserted,
    /// A strictly newer value superseded the current one.
    Replaced,
    /// The exact current value was re-applied; no state changed.
    Duplicate,
    /// A lower-revision (or lower-priority) value than the current one.
    Stale,
}

/// Why [`NegotiationBook`] rejected a value or could not issue one.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ApplyError {
    /// The ticket names an offer the book does not track.
    #[error("ticket names unknown offer ({negotiation_id}, {offer_seq})")]
    UnknownOffer {
        /// The ticket's negotiation.
        negotiation_id: NegotiationId,
        /// The ticket's offer sequence.
        offer_seq: u64,
    },
    /// The counteroffer names a negotiation the book does not track as live.
    #[error("counteroffer names unknown negotiation {negotiation_id}")]
    UnknownNegotiation {
        /// The counteroffer's negotiation.
        negotiation_id: NegotiationId,
    },
    /// The ticket's revision is lower than the current ticket's.
    #[error(
        "stale revision: ticket revision {revision} is below current revision {current_revision}"
    )]
    StaleRevision {
        /// The rejected ticket's revision.
        revision: u64,
        /// The current ticket's revision.
        current_revision: u64,
    },
    /// The book already holds `MAX_HEADS` tickets for this offer and the
    /// ticket is a brand-new signer.
    #[error("ticket capacity reached: at most {max} tickets per offer")]
    Capacity {
        /// The per-offer ticket bound.
        max: usize,
    },
    /// Re-registering an offer slot with a different content hash.
    #[error("offer slot ({negotiation_id}, {offer_seq}) already holds a different offer hash")]
    OfferHashConflict {
        /// The offer's negotiation.
        negotiation_id: NegotiationId,
        /// The offer's sequence.
        offer_seq: u64,
    },
    /// The ticket failed structural validation or signature verification.
    #[error("invalid ticket: {0}")]
    InvalidTicket(String),
    /// The counteroffer failed structural validation or signature verification.
    #[error("invalid counteroffer: {0}")]
    InvalidCounteroffer(String),
    /// The counteroffer is outside the clock-skew or lifetime bounds.
    #[error("counteroffer out of time: issued {issued_at}, now {now}, valid for {valid_for}")]
    CounterofferOutOfTime {
        /// The counteroffer's issue time.
        issued_at: u64,
        /// The current time.
        now: u64,
        /// The counteroffer's validity window.
        valid_for: u64,
    },
    /// The book already holds `MAX_HEADS` counteroffers for this
    /// negotiation and the counteroffer is a brand-new signer.
    #[error("counteroffer capacity reached: at most {max} per negotiation")]
    CounterofferCapacity {
        /// The per-negotiation counteroffer bound.
        max: usize,
    },
    /// The offer's own identity does not match the slot it was registered
    /// under: the book would otherwise store an offer hash and creator under
    /// a false slot and could issue a locally valid-looking ticket for it.
    #[error("offer identity does not match the registered slot")]
    OfferSlotMismatch,
    /// The negotiation already holds `MAX_OFFERS_PER_NEGOTIATION` offer
    /// slots and the offer is a brand-new slot.
    #[error("offer capacity reached: at most {max} offers per negotiation")]
    OfferCapacity {
        /// The per-negotiation offer bound.
        max: usize,
    },
    /// The local peer is not the creator and the creator's Active ticket for
    /// the offer has not been applied: the offer is not yet authenticated.
    #[error("offer ({negotiation_id}, {offer_seq}) is not authenticated by the creator's ticket")]
    UnauthenticatedOffer {
        /// The offer's negotiation.
        negotiation_id: NegotiationId,
        /// The offer's sequence.
        offer_seq: u64,
    },
    /// Signing the local ticket or counteroffer failed.
    #[error("local signing failed: {0}")]
    Signing(String),
}

/// One offer slot: the tracked offer's identity and its current tickets.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OfferBook {
    offer_hash: OfferHash,
    creator: PeerId,
    tickets: HashMap<PeerId, Ticket>,
}

/// One negotiation's tracked state: every offer slot plus the per-signer
/// counteroffers (counteroffers are negotiation-scoped, not offer-scoped).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct NegotiationState {
    offers: HashMap<u64, OfferBook>,
    counteroffers: HashMap<PeerId, Counteroffer>,
}

/// Deterministic per-negotiation bookkeeping plus local ticket issuance and
/// counteroffer emission.
///
/// The book tracks every offer of a negotiation — re-offers accumulate as
/// distinct slots, and old slots are retained as evidence. It applies inbound
/// tickets and counteroffers, and it issues the local node's own artifacts:
/// [`issue_ticket`](Self::issue_ticket) signs the local Active ticket for any
/// tracked offer (gated on the offer's authentication), and
/// [`emit_counteroffer`](Self::emit_counteroffer) signs the local preference
/// announcement.
#[derive(Debug, Clone)]
pub struct NegotiationBook<'a> {
    identity: &'a NodeKeys,
    execution: &'a ExecutionKey,
    negotiations: HashMap<NegotiationId, NegotiationState>,
}

impl<'a> NegotiationBook<'a> {
    /// An empty book bound to the local node's keys.
    #[must_use]
    pub fn new(identity: &'a NodeKeys, execution: &'a ExecutionKey) -> Self {
        Self {
            identity,
            execution,
            negotiations: HashMap::new(),
        }
    }

    /// Register the offer the book tracks. `apply_ticket` rejects tickets for
    /// any other offer, and `apply_counteroffer` rejects counteroffers for
    /// any other negotiation. Re-registering the same offer with the same
    /// content hash is idempotent; a conflicting hash for an existing slot is
    /// an error (the slot identity must be unambiguous).
    pub fn register_offer(
        &mut self,
        negotiation_id: NegotiationId,
        offer_seq: u64,
        offer: &Offer,
    ) -> Result<(), ApplyError> {
        if offer.data().negotiation_id != negotiation_id || offer.data().offer_seq != offer_seq {
            return Err(ApplyError::OfferSlotMismatch);
        }
        let state = self.negotiations.entry(negotiation_id).or_default();
        if !state.offers.contains_key(&offer_seq)
            && state.offers.len() >= MAX_OFFERS_PER_NEGOTIATION
        {
            return Err(ApplyError::OfferCapacity {
                max: MAX_OFFERS_PER_NEGOTIATION,
            });
        }
        match state.offers.entry(offer_seq) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(OfferBook {
                    offer_hash: OfferHash::of(offer.data()),
                    creator: offer.data().creator,
                    tickets: HashMap::new(),
                });
                Ok(())
            }
            std::collections::hash_map::Entry::Occupied(entry) => {
                if entry.get().offer_hash == OfferHash::of(offer.data()) {
                    Ok(())
                } else {
                    Err(ApplyError::OfferHashConflict {
                        negotiation_id,
                        offer_seq,
                    })
                }
            }
        }
    }

    /// Number of current tickets for one offer.
    #[must_use]
    pub fn len(&self, negotiation_id: NegotiationId, offer_seq: u64) -> usize {
        self.negotiations
            .get(&negotiation_id)
            .and_then(|state| state.offers.get(&offer_seq))
            .map_or(0, |offer| offer.tickets.len())
    }

    /// The current signed ticket for `signer` under one offer, if any.
    #[must_use]
    pub fn current_ticket(
        &self,
        negotiation_id: NegotiationId,
        offer_seq: u64,
        signer: PeerId,
    ) -> Option<&Ticket> {
        self.negotiations
            .get(&negotiation_id)
            .and_then(|state| state.offers.get(&offer_seq))
            .and_then(|offer| offer.tickets.get(&signer))
    }

    /// The current `Active` tickets for one offer in canonical order: the
    /// stored creator's ticket first, the rest ascending `PeerId`. `Withdrawn`
    /// tickets are filtered out. The creator's ticket is included only if it
    /// has been applied.
    #[must_use]
    pub fn ticket_set(&self, negotiation_id: NegotiationId, offer_seq: u64) -> Vec<Ticket> {
        let Some(offer) = self
            .negotiations
            .get(&negotiation_id)
            .and_then(|state| state.offers.get(&offer_seq))
        else {
            return Vec::new();
        };
        let creator = offer.creator;
        let mut tickets: Vec<Ticket> = offer
            .tickets
            .values()
            .filter(|ticket| matches!(ticket.data.action, TicketAction::Active { .. }))
            .cloned()
            .collect();
        tickets.sort_by_key(|ticket| (ticket.data.signer != creator, ticket.data.signer));
        tickets
    }

    /// The current counteroffer for `signer` under one negotiation, if any.
    /// The entry may be expired; use [`valid_counteroffers`](Self::valid_counteroffers)
    /// for selection.
    #[must_use]
    pub fn current_counteroffer(
        &self,
        negotiation_id: NegotiationId,
        signer: PeerId,
    ) -> Option<&Counteroffer> {
        self.negotiations
            .get(&negotiation_id)
            .and_then(|state| state.counteroffers.get(&signer))
    }

    /// The unexpired counteroffers for one negotiation as of `now_ms`
    /// (Unix milliseconds), sorted by signer.
    #[must_use]
    pub fn valid_counteroffers(
        &self,
        negotiation_id: NegotiationId,
        now_ms: u64,
    ) -> Vec<&Counteroffer> {
        let Some(state) = self.negotiations.get(&negotiation_id) else {
            return Vec::new();
        };
        let mut entries: Vec<&Counteroffer> = state
            .counteroffers
            .iter()
            .filter(|(_, counteroffer)| !expired(counteroffer, now_ms))
            .map(|(_, counteroffer)| counteroffer)
            .collect();
        entries.sort_by_key(|counteroffer| counteroffer.data.signer);
        entries
    }

    /// Number of retained counteroffers for one negotiation.
    #[must_use]
    pub fn counteroffer_len(&self, negotiation_id: NegotiationId) -> usize {
        self.negotiations
            .get(&negotiation_id)
            .map_or(0, |state| state.counteroffers.len())
    }

    /// Apply one inbound ticket. Rules per signer:
    /// - the ticket must validate (domain/version/size), verify its ed25519
    ///   identity signature, and — when `Active` — verify its scope-bound
    ///   `key_binding` against the tracked offer's hash;
    /// - the ticket must name a registered offer;
    /// - any revision starts a new signer (bounded by `MAX_HEADS`);
    /// - the exact current ticket is idempotent;
    /// - the same revision with different bytes is inert (the current ticket
    ///   stands — no equivocation state);
    /// - a lower revision is stale;
    /// - a higher revision replaces the current ticket. A `Withdrawn` ticket
    ///   is a valid successor (it simply does not show up in
    ///   [`ticket_set`](Self::ticket_set)); revocation is a later `Active`
    ///   revision.
    pub fn apply_ticket(&mut self, ticket: &Ticket) -> Result<ApplyOutcome, ApplyError> {
        ticket
            .validate()
            .map_err(|error| ApplyError::InvalidTicket(error.to_string()))?;

        let negotiation_id = ticket.data.negotiation_id;
        let offer_seq = ticket.data.offer_seq;
        let Some(offer) = self
            .negotiations
            .get_mut(&negotiation_id)
            .and_then(|state| state.offers.get_mut(&offer_seq))
        else {
            return Err(ApplyError::UnknownOffer {
                negotiation_id,
                offer_seq,
            });
        };

        match ticket.verify_for_offer(&offer.offer_hash) {
            Ok(()) => {}
            Err(arena0_protocol::TicketVerificationError::IdentityMismatch) => {
                return Err(ApplyError::InvalidTicket(
                    "identity signature mismatch".into(),
                ));
            }
            Err(arena0_protocol::TicketVerificationError::KeyBindingMismatch) => {
                return Err(ApplyError::InvalidTicket("key binding mismatch".into()));
            }
            Err(error) => return Err(ApplyError::InvalidTicket(error.to_string())),
        }

        let signer = ticket.data.signer;
        let revision = ticket.data.revision;
        let Some(current) = offer.tickets.get(&signer) else {
            if offer.tickets.len() >= MAX_HEADS {
                return Err(ApplyError::Capacity { max: MAX_HEADS });
            }
            offer.tickets.insert(signer, ticket.clone());
            return Ok(ApplyOutcome::Inserted);
        };
        if current.data.revision == revision {
            return Ok(ApplyOutcome::Duplicate);
        }
        if current.data.revision > revision {
            return Err(ApplyError::StaleRevision {
                revision,
                current_revision: current.data.revision,
            });
        }
        offer.tickets.insert(signer, ticket.clone());
        Ok(ApplyOutcome::Replaced)
    }

    /// Apply one inbound counteroffer as of `now_ms` (Unix milliseconds).
    /// Rules:
    /// - the counteroffer must validate (domain/version/params bound) and
    ///   verify its ed25519 identity signature;
    /// - it must be within the clock-skew and lifetime bounds: not issued in
    ///   the future beyond [`MAX_CLOCK_SKEW_MS`](arena0_protocol::MAX_CLOCK_SKEW_MS)
    ///   and not already expired;
    /// - it must name a live negotiation (one with a registered offer);
    /// - expired entries for the negotiation are pruned first;
    /// - one entry per signer: a higher `issued_at` replaces, an equal
    ///   `issued_at` resolves to the lowest [`CounterofferHash`], a lower
    ///   `issued_at` is stale;
    /// - a brand-new signer is bounded by `MAX_HEADS` per negotiation.
    pub fn apply_counteroffer(
        &mut self,
        counteroffer: &Counteroffer,
        now_ms: u64,
    ) -> Result<ApplyOutcome, ApplyError> {
        counteroffer
            .validate()
            .map_err(|error| ApplyError::InvalidCounteroffer(error.to_string()))?;
        counteroffer
            .verify_identity()
            .map_err(|error| ApplyError::InvalidCounteroffer(error.to_string()))?
            .then_some(())
            .ok_or_else(|| ApplyError::InvalidCounteroffer("identity signature mismatch".into()))?;

        let issued_at = counteroffer.data.issued_at_unix_ms;
        let valid_for = u64::from(counteroffer.data.valid_for_ms);
        if issued_at > now_ms.saturating_add(arena0_protocol::MAX_CLOCK_SKEW_MS)
            || expired(counteroffer, now_ms)
        {
            return Err(ApplyError::CounterofferOutOfTime {
                issued_at,
                now: now_ms,
                valid_for,
            });
        }

        let negotiation_id = counteroffer.data.negotiation_id;
        let Some(state) = self.negotiations.get_mut(&negotiation_id) else {
            return Err(ApplyError::UnknownNegotiation { negotiation_id });
        };
        let signer = counteroffer.data.signer;
        // Prune expired entries for this negotiation so the book stays bounded.
        state
            .counteroffers
            .retain(|_, entry| !expired(entry, now_ms));

        let Some(current) = state.counteroffers.get(&signer) else {
            if state.counteroffers.len() >= MAX_HEADS {
                return Err(ApplyError::CounterofferCapacity { max: MAX_HEADS });
            }
            state.counteroffers.insert(signer, counteroffer.clone());
            return Ok(ApplyOutcome::Inserted);
        };
        if current.data.issued_at_unix_ms > issued_at {
            return Ok(ApplyOutcome::Stale);
        }
        if current.data.issued_at_unix_ms == issued_at {
            let current_hash = CounterofferHash::of(&current.data);
            let new_hash = CounterofferHash::of(&counteroffer.data);
            if new_hash == current_hash {
                return Ok(ApplyOutcome::Duplicate);
            }
            if new_hash > current_hash {
                return Ok(ApplyOutcome::Stale);
            }
        }
        state.counteroffers.insert(signer, counteroffer.clone());
        Ok(ApplyOutcome::Replaced)
    }

    /// Issue the local Active ticket for one tracked offer at `revision`,
    /// as of `now_ms` (Unix milliseconds): the scope-bound `key_binding`
    /// commits to the offer hash, and the ed25519 identity signature binds
    /// the ticket to the local peer.
    ///
    /// The offer must be authenticated first: the local peer is the offer's
    /// creator, or the creator's Active ticket for the offer is already
    /// applied. An unauthenticated offer is rejected — the local consent
    /// never precedes the creator's. The issued ticket is applied to the
    /// book and returned.
    pub fn issue_ticket(
        &mut self,
        negotiation_id: NegotiationId,
        offer_seq: u64,
        revision: u64,
        now_ms: u64,
    ) -> Result<Ticket, ApplyError> {
        let Some(state) = self.negotiations.get(&negotiation_id) else {
            return Err(ApplyError::UnknownNegotiation { negotiation_id });
        };
        let Some(offer) = state.offers.get(&offer_seq) else {
            return Err(ApplyError::UnknownOffer {
                negotiation_id,
                offer_seq,
            });
        };
        let local = self.identity.peer_id();
        // The creator's Active ticket authenticates the offer; a Withdrawn
        // creator ticket means the creator withdrew and the offer is dead.
        let creator_authenticates = offer
            .tickets
            .get(&offer.creator)
            .is_some_and(|ticket| matches!(ticket.data.action, TicketAction::Active { .. }));
        if local != offer.creator && !creator_authenticates {
            return Err(ApplyError::UnauthenticatedOffer {
                negotiation_id,
                offer_seq,
            });
        }
        let execution_bls = self.execution.public_key();
        let key_binding = self
            .execution
            .key_binding(&offer.offer_hash.0, &self.identity.ed25519_public_key().0);
        let data = TicketData::new(
            negotiation_id,
            offer_seq,
            local,
            revision,
            TicketAction::Active {
                execution_bls,
                key_binding,
                issued_at_unix_ms: now_ms,
                valid_for_ms: u32::try_from(arena0_protocol::negotiation::MAX_TICKET_LIFETIME_MS)
                    .expect("60_000 fits in u32"),
            },
        )
        .map_err(|error| ApplyError::Signing(error.to_string()))?;
        let ticket = Ticket {
            signature: self.identity.sign(&data.signing_bytes()),
            data,
        };
        self.apply_ticket(&ticket)?;
        Ok(ticket)
    }

    /// Form the creator's first offer and its matching Active ticket from the
    /// same immutable offer body. The ticket hash is therefore available
    /// before the [`Offer`] is constructed; no empty or placeholder offer can
    /// cross the negotiation boundary.
    pub fn create_creator_offer(
        &self,
        data: arena0_protocol::OfferData,
        now_ms: u64,
    ) -> Result<(Offer, Ticket), ApplyError> {
        let offer_hash = OfferHash::of(&data);
        let execution_bls = self.execution.public_key();
        let key_binding = self
            .execution
            .key_binding(&offer_hash.0, &self.identity.ed25519_public_key().0);
        let ticket_data = TicketData::new(
            data.negotiation_id,
            data.offer_seq,
            self.identity.peer_id(),
            0,
            TicketAction::Active {
                execution_bls,
                key_binding,
                issued_at_unix_ms: now_ms,
                valid_for_ms: u32::try_from(arena0_protocol::negotiation::MAX_TICKET_LIFETIME_MS)
                    .expect("60_000 fits in u32"),
            },
        )
        .map_err(|error| ApplyError::Signing(error.to_string()))?;
        let ticket = Ticket {
            signature: self.identity.sign(&ticket_data.signing_bytes()),
            data: ticket_data,
        };
        let offer = Offer::new(data, vec![arena0_protocol::TicketHash::of(&ticket.data)])
            .map_err(|error| ApplyError::Signing(error.to_string()))?;
        Ok((offer, ticket))
    }

    /// Emit the local counteroffer for one live negotiation, as of `now_ms`
    /// (Unix milliseconds): signed with the local identity, applied to the
    /// book, and returned.
    pub fn emit_counteroffer(
        &mut self,
        negotiation_id: NegotiationId,
        params: Vec<u8>,
        now_ms: u64,
    ) -> Result<Counteroffer, ApplyError> {
        if !self.negotiations.contains_key(&negotiation_id) {
            return Err(ApplyError::UnknownNegotiation { negotiation_id });
        }
        let data = CounterofferData::new(
            negotiation_id,
            self.identity.peer_id(),
            params,
            now_ms,
            u32::try_from(arena0_protocol::negotiation::MAX_TICKET_LIFETIME_MS)
                .expect("60_000 fits in u32"),
        )
        .map_err(|error| ApplyError::Signing(error.to_string()))?;
        let counteroffer = Counteroffer {
            signature: self.identity.sign(&data.signing_bytes()),
            data,
        };
        self.apply_counteroffer(&counteroffer, now_ms)?;
        Ok(counteroffer)
    }
}

/// Whether a counteroffer is expired as of `now_ms`.
fn expired(counteroffer: &Counteroffer, now_ms: u64) -> bool {
    now_ms
        > counteroffer
            .data
            .issued_at_unix_ms
            .saturating_add(u64::from(counteroffer.data.valid_for_ms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_crypto::{Ed25519Signature, ExecutionSalt, SecretKey};
    use arena0_program::ProgramHash;
    use arena0_protocol::{CounterofferData, Offer, OfferData, StateHash, TicketData};

    const TEST_ISSUED_AT_MS: u64 = 1_000;
    const TEST_VALID_FOR_MS: u32 = 60_000;

    struct TestKeys {
        identity: NodeKeys,
        execution: ExecutionKey,
    }

    impl std::ops::Deref for TestKeys {
        type Target = NodeKeys;

        fn deref(&self) -> &Self::Target {
            &self.identity
        }
    }

    fn provider(seed: u8) -> TestKeys {
        TestKeys {
            identity: NodeKeys::from_secret(SecretKey::from_bytes([seed; 32])),
            execution: ExecutionKey::derive(
                &ExecutionSalt::try_from_bytes([seed; 32]).expect("non-zero test salt"),
                &[seed; 32],
                &[seed.wrapping_add(1); 32],
            )
            .expect("execution bls key"),
        }
    }

    fn negotiation_id(seed: u8) -> NegotiationId {
        NegotiationId([seed; 32])
    }

    fn offer(negotiation_id: NegotiationId, offer_seq: u64, creator: PeerId) -> Offer {
        offer_with_params(negotiation_id, offer_seq, creator, vec![0xAA; 3])
    }

    fn offer_with_params(
        negotiation_id: NegotiationId,
        offer_seq: u64,
        creator: PeerId,
        params: Vec<u8>,
    ) -> Offer {
        let params = serde_json::to_vec(&params).expect("test params are JSON");
        let data = OfferData::new(
            negotiation_id,
            offer_seq,
            creator,
            ProgramHash([0x22; 32]),
            arena0_program::ExecutionProfile::current().hash(),
            arena0_program::JsonBytes::try_new(params).expect("valid JSON params"),
            2,
            StateHash([0x33; 32]),
            1_000,
        )
        .expect("valid offer data");
        Offer::new(data, vec![arena0_protocol::TicketHash([0; 32])]).expect("valid offer")
    }

    fn active_action(p: &TestKeys, offer: &Offer) -> TicketAction {
        let execution_bls = p.execution.public_key();
        let key_binding = p.execution.key_binding(
            &OfferHash::of(offer.data()).0,
            &p.identity.ed25519_public_key().0,
        );
        TicketAction::Active {
            execution_bls,
            key_binding,
            issued_at_unix_ms: TEST_ISSUED_AT_MS,
            valid_for_ms: TEST_VALID_FOR_MS,
        }
    }

    /// A structurally valid signed ticket for `seed`'s peer with a real
    /// ed25519 identity signature and a real scope-bound key binding.
    fn ticket(p: &TestKeys, offer: &Offer, revision: u64, action: TicketAction) -> Ticket {
        let data = TicketData::new(
            offer.data().negotiation_id,
            offer.data().offer_seq,
            p.peer_id(),
            revision,
            action,
        )
        .expect("valid ticket data");
        Ticket {
            signature: p.sign(&data.signing_bytes()),
            data,
        }
    }

    fn active_ticket(p: &TestKeys, offer: &Offer, rev: u64) -> Ticket {
        ticket(p, offer, rev, active_action(p, offer))
    }

    fn withdrawn_ticket(p: &TestKeys, offer: &Offer, rev: u64) -> Ticket {
        ticket(p, offer, rev, TicketAction::Withdrawn)
    }

    fn registered_book<'a>(p: &'a TestKeys, offer: &Offer) -> NegotiationBook<'a> {
        let mut book = NegotiationBook::new(&p.identity, &p.execution);
        book.register_offer(offer.data().negotiation_id, offer.data().offer_seq, offer)
            .expect("offer slot is fresh");
        book
    }

    #[test]
    fn revision_zero_starts_a_signer() {
        let neg = negotiation_id(1);
        let offer = offer(neg, 0, provider(1).peer_id());
        let p = provider(1);
        let mut book = registered_book(&p, &offer);
        let t0 = active_ticket(&provider(1), &offer, 0);
        assert_eq!(book.apply_ticket(&t0), Ok(ApplyOutcome::Inserted));
        assert_eq!(book.len(neg, 0), 1);
        assert_eq!(
            book.current_ticket(neg, 0, provider(1).peer_id()),
            Some(&t0)
        );
    }

    #[test]
    fn valid_replacement_advances_the_current_ticket() {
        let neg = negotiation_id(1);
        let offer = offer(neg, 0, provider(1).peer_id());
        let p = provider(1);
        let mut book = registered_book(&p, &offer);
        let v0 = active_ticket(&provider(1), &offer, 0);
        book.apply_ticket(&v0).expect("start");
        let v1 = active_ticket(&provider(1), &offer, 1);
        assert_eq!(book.apply_ticket(&v1), Ok(ApplyOutcome::Replaced));
        assert_eq!(book.len(neg, 0), 1);
        assert_eq!(
            book.current_ticket(neg, 0, provider(1).peer_id()),
            Some(&v1)
        );
    }

    #[test]
    fn exact_duplicate_is_idempotent() {
        let neg = negotiation_id(1);
        let offer = offer(neg, 0, provider(1).peer_id());
        let p = provider(1);
        let mut book = registered_book(&p, &offer);
        let t0 = active_ticket(&provider(1), &offer, 0);
        book.apply_ticket(&t0).expect("start");
        assert_eq!(book.apply_ticket(&t0), Ok(ApplyOutcome::Duplicate));
        assert_eq!(book.len(neg, 0), 1);
    }

    #[test]
    fn lower_revision_is_stale() {
        let neg = negotiation_id(1);
        let offer = offer(neg, 0, provider(1).peer_id());
        let p = provider(1);
        let mut book = registered_book(&p, &offer);
        let v0 = active_ticket(&provider(1), &offer, 0);
        book.apply_ticket(&v0).expect("start");
        let v1 = active_ticket(&provider(1), &offer, 1);
        book.apply_ticket(&v1).expect("replace");
        let stale = active_ticket(&provider(1), &offer, 0);
        assert_eq!(
            book.apply_ticket(&stale),
            Err(ApplyError::StaleRevision {
                revision: 0,
                current_revision: 1
            })
        );
        assert_eq!(
            book.current_ticket(neg, 0, provider(1).peer_id()),
            Some(&v1)
        );
    }

    #[test]
    fn same_revision_fork_is_inert() {
        let neg = negotiation_id(1);
        let offer = offer(neg, 0, provider(1).peer_id());
        let p = provider(1);
        let mut book = registered_book(&p, &offer);
        let v0 = active_ticket(&provider(1), &offer, 0);
        book.apply_ticket(&v0).expect("start");
        // A different ticket at the same revision: the current stands.
        let p = provider(1);
        let mut rival = active_ticket(&p, &offer, 0);
        let TicketAction::Active {
            execution_bls,
            key_binding,
            ..
        } = rival.data.action
        else {
            unreachable!("active ticket")
        };
        rival.data.action = TicketAction::Active {
            execution_bls,
            key_binding,
            issued_at_unix_ms: TEST_ISSUED_AT_MS + 1,
            valid_for_ms: TEST_VALID_FOR_MS,
        };
        rival.signature = p.sign(&rival.data.signing_bytes());
        assert_eq!(book.apply_ticket(&rival), Ok(ApplyOutcome::Duplicate));
        assert_eq!(
            book.current_ticket(neg, 0, provider(1).peer_id()),
            Some(&v0)
        );
    }

    #[test]
    fn withdrawal_is_a_valid_successor_and_can_be_revoked() {
        let neg = negotiation_id(1);
        let creator = provider(1).peer_id();
        let offer = offer(neg, 0, creator);
        let p = provider(1);
        let mut book = registered_book(&p, &offer);
        let v0 = active_ticket(&provider(1), &offer, 0);
        book.apply_ticket(&v0).expect("start");
        let w1 = withdrawn_ticket(&provider(1), &offer, 1);
        assert_eq!(book.apply_ticket(&w1), Ok(ApplyOutcome::Replaced));
        assert!(book.ticket_set(neg, 0).is_empty());

        // A later Active revision supersedes the withdrawal.
        let v2 = active_ticket(&provider(1), &offer, 2);
        assert_eq!(book.apply_ticket(&v2), Ok(ApplyOutcome::Replaced));
        assert_eq!(book.ticket_set(neg, 0), vec![v2]);
    }

    #[test]
    fn ticket_set_is_creator_first_then_ascending() {
        let neg = negotiation_id(1);
        let creator = provider(2).peer_id();
        let offer = offer(neg, 0, creator);
        let p = provider(1);
        let mut book = registered_book(&p, &offer);
        // Apply in non-canonical order: creator last.
        for seed in [3, 1, 2] {
            let t0 = active_ticket(&provider(seed), &offer, 0);
            book.apply_ticket(&t0).expect("start");
        }
        let set = book.ticket_set(neg, 0);
        assert_eq!(set.len(), 3);
        assert_eq!(set[0].data.signer, creator);
        assert!(
            set[1..]
                .windows(2)
                .all(|w| w[0].data.signer <= w[1].data.signer)
        );
    }

    #[test]
    fn unknown_offer_is_rejected() {
        let neg = negotiation_id(1);
        let tracked = offer(neg, 0, provider(1).peer_id());
        let p = provider(1);
        let mut book = NegotiationBook::new(&p.identity, &p.execution);
        let t0 = active_ticket(&p, &tracked, 0);
        assert_eq!(
            book.apply_ticket(&t0),
            Err(ApplyError::UnknownOffer {
                negotiation_id: neg,
                offer_seq: 0
            })
        );
        // A different offer_seq is a different offer.
        let other = offer(neg, 1, provider(1).peer_id());
        book.register_offer(neg, 0, &tracked)
            .expect("offer slot is fresh");
        let t1 = active_ticket(&provider(1), &other, 0);
        assert_eq!(
            book.apply_ticket(&t1),
            Err(ApplyError::UnknownOffer {
                negotiation_id: neg,
                offer_seq: 1
            })
        );
    }

    #[test]
    fn register_offer_rejects_a_conflicting_hash_for_an_existing_slot() {
        let neg = NegotiationId([1; 32]);
        let tracked = offer(neg, 0, provider(1).peer_id());
        let p = provider(1);
        let mut book = NegotiationBook::new(&p.identity, &p.execution);
        book.register_offer(neg, 0, &tracked)
            .expect("offer slot is fresh");
        // Same slot, same hash: idempotent.
        assert!(book.register_offer(neg, 0, &tracked).is_ok());
        // Same slot, different hash: conflict.
        let other = offer(neg, 0, provider(2).peer_id());
        assert_eq!(
            book.register_offer(neg, 0, &other),
            Err(ApplyError::OfferHashConflict {
                negotiation_id: neg,
                offer_seq: 0
            })
        );
    }

    #[test]
    fn register_offer_capacity_is_bounded_per_negotiation() {
        let neg = negotiation_id(1);
        let p = provider(1);
        let mut book = NegotiationBook::new(&p.identity, &p.execution);
        for seq in 0..MAX_OFFERS_PER_NEGOTIATION {
            let offer = offer(neg, seq as u64, p.peer_id());
            book.register_offer(neg, seq as u64, &offer)
                .expect("slot is fresh");
        }
        // The next distinct slot is rejected.
        let extra = offer(neg, MAX_OFFERS_PER_NEGOTIATION as u64, p.peer_id());
        assert_eq!(
            book.register_offer(neg, MAX_OFFERS_PER_NEGOTIATION as u64, &extra),
            Err(ApplyError::OfferCapacity {
                max: MAX_OFFERS_PER_NEGOTIATION
            })
        );
        // Re-registering an existing slot at capacity stays idempotent.
        let existing = offer(neg, 0, p.peer_id());
        assert!(book.register_offer(neg, 0, &existing).is_ok());
    }

    #[test]
    fn register_offer_rejects_a_slot_mismatch() {
        let neg = negotiation_id(1);
        let p = provider(1);
        let offer = offer(neg, 0, p.peer_id());
        let mut book = NegotiationBook::new(&p.identity, &p.execution);
        // The offer's own identity must match the slot it is registered under.
        assert_eq!(
            book.register_offer(neg, 1, &offer),
            Err(ApplyError::OfferSlotMismatch)
        );
        assert_eq!(
            book.register_offer(negotiation_id(2), 0, &offer),
            Err(ApplyError::OfferSlotMismatch)
        );
        // The matching slot registers fine.
        assert!(book.register_offer(neg, 0, &offer).is_ok());
    }

    #[test]
    fn counteroffer_book_rejects_unknown_negotiations() {
        let neg = NegotiationId([1; 32]);
        let offer = offer(neg, 0, provider(1).peer_id());
        let p = provider(1);
        let mut book = NegotiationBook::new(&p.identity, &p.execution);
        let counteroffer = counteroffer(&p, neg, vec![1], 1_000, 60_000);
        // Not registered as live: rejected.
        assert_eq!(
            book.apply_counteroffer(&counteroffer, 1_000),
            Err(ApplyError::UnknownNegotiation {
                negotiation_id: neg
            })
        );
        // Registered: accepted.
        book.register_offer(neg, 0, &offer).expect("offer slot");
        assert_eq!(
            book.apply_counteroffer(&counteroffer, 1_000),
            Ok(ApplyOutcome::Inserted)
        );
    }

    #[test]
    fn invalid_signature_is_rejected() {
        let neg = negotiation_id(1);
        let offer = offer(neg, 0, provider(1).peer_id());
        let p = provider(1);
        let mut book = registered_book(&p, &offer);
        let mut t0 = active_ticket(&provider(1), &offer, 0);
        t0.signature = Ed25519Signature([0; 64]);
        assert!(matches!(
            book.apply_ticket(&t0),
            Err(ApplyError::InvalidTicket(_))
        ));
        assert!(book.ticket_set(neg, 0).is_empty());
    }

    #[test]
    fn key_binding_scope_is_enforced() {
        let neg = negotiation_id(1);
        let creator = provider(1).peer_id();
        // Same (negotiation_id, offer_seq), different params: different offer
        // hash, so a ticket bound to one does not verify against the other.
        let offer_a = offer(neg, 0, creator);
        let offer_b = offer_with_params(neg, 0, creator, vec![0xBB; 3]);
        assert_ne!(OfferHash::of(offer_a.data()), OfferHash::of(offer_b.data()));
        let p = provider(1);
        let mut book = registered_book(&p, &offer_a);
        let t0 = active_ticket(&provider(2), &offer_b, 0);
        assert!(matches!(
            book.apply_ticket(&t0),
            Err(ApplyError::InvalidTicket(_))
        ));
        assert!(book.ticket_set(neg, 0).is_empty());
    }

    #[test]
    fn head_capacity_is_64_and_replacements_do_not_consume_it() {
        let neg = negotiation_id(1);
        let offer = offer(neg, 0, provider(1).peer_id());
        let p = provider(1);
        let mut book = registered_book(&p, &offer);
        for seed in 1..=MAX_HEADS {
            let t0 = active_ticket(&provider(seed as u8), &offer, 0);
            assert_eq!(book.apply_ticket(&t0), Ok(ApplyOutcome::Inserted));
        }
        assert_eq!(book.len(neg, 0), MAX_HEADS);

        // The 65th distinct signer is rejected.
        let extra = active_ticket(&provider(65), &offer, 0);
        assert_eq!(
            book.apply_ticket(&extra),
            Err(ApplyError::Capacity { max: MAX_HEADS })
        );

        // A replacement on an existing signer still succeeds at full capacity.
        let v1 = active_ticket(&provider(1), &offer, 1);
        assert_eq!(book.apply_ticket(&v1), Ok(ApplyOutcome::Replaced));
        assert_eq!(book.len(neg, 0), MAX_HEADS);
    }

    fn counteroffer(
        p: &NodeKeys,
        negotiation_id: NegotiationId,
        params: Vec<u8>,
        issued_at: u64,
        valid_for: u32,
    ) -> Counteroffer {
        let data = CounterofferData::new(negotiation_id, p.peer_id(), params, issued_at, valid_for)
            .expect("valid counteroffer data");
        Counteroffer {
            signature: p.sign(&data.signing_bytes()),
            data,
        }
    }

    #[test]
    fn counteroffer_highest_issued_at_wins() {
        let neg = negotiation_id(1);
        let p = provider(1);
        let offer = offer(neg, 0, p.peer_id());
        let mut book = NegotiationBook::new(&p.identity, &p.execution);
        book.register_offer(neg, 0, &offer).expect("offer slot");
        let old = counteroffer(&p, neg, vec![1], 1_000, TEST_VALID_FOR_MS);
        let new = counteroffer(&p, neg, vec![2], 2_000, TEST_VALID_FOR_MS);
        assert_eq!(
            book.apply_counteroffer(&old, 2_500),
            Ok(ApplyOutcome::Inserted)
        );
        assert_eq!(
            book.apply_counteroffer(&new, 2_500),
            Ok(ApplyOutcome::Replaced)
        );
        assert_eq!(book.current_counteroffer(neg, p.peer_id()), Some(&new));
        // An older re-broadcast is stale.
        assert_eq!(
            book.apply_counteroffer(&old, 2_500),
            Ok(ApplyOutcome::Stale)
        );
        assert_eq!(book.current_counteroffer(neg, p.peer_id()), Some(&new));
    }

    #[test]
    fn counteroffer_equal_timestamp_lowest_hash_wins() {
        let neg = negotiation_id(1);
        let p = provider(1);
        let offer = offer(neg, 0, p.peer_id());
        let mut book = NegotiationBook::new(&p.identity, &p.execution);
        book.register_offer(neg, 0, &offer).expect("offer slot");
        let a = counteroffer(&p, neg, vec![1], 1_000, TEST_VALID_FOR_MS);
        let b = counteroffer(&p, neg, vec![2], 1_000, TEST_VALID_FOR_MS);
        let hash_a = CounterofferHash::of(&a.data);
        let hash_b = CounterofferHash::of(&b.data);
        assert_ne!(hash_a, hash_b);
        let (low, high) = if hash_a < hash_b { (a, b) } else { (b, a) };
        assert_eq!(
            book.apply_counteroffer(&high, 2_000),
            Ok(ApplyOutcome::Inserted)
        );
        assert_eq!(
            book.apply_counteroffer(&low, 2_000),
            Ok(ApplyOutcome::Replaced)
        );
        assert_eq!(book.current_counteroffer(neg, p.peer_id()), Some(&low));
        // The higher hash is stale against the retained lower hash.
        assert_eq!(
            book.apply_counteroffer(&high, 2_000),
            Ok(ApplyOutcome::Stale)
        );
        // The exact retained counteroffer is idempotent.
        assert_eq!(
            book.apply_counteroffer(&low, 2_000),
            Ok(ApplyOutcome::Duplicate)
        );
    }

    #[test]
    fn counteroffer_expired_is_rejected_and_pruned() {
        let neg = negotiation_id(1);
        let p = provider(1);
        let offer = offer(neg, 0, p.peer_id());
        let mut book = NegotiationBook::new(&p.identity, &p.execution);
        book.register_offer(neg, 0, &offer).expect("offer slot");
        let short = counteroffer(&p, neg, vec![1], 1_000, 1_000);
        assert_eq!(
            book.apply_counteroffer(&short, 1_500),
            Ok(ApplyOutcome::Inserted)
        );
        // Expired: rejected on re-apply and filtered from valid().
        assert!(matches!(
            book.apply_counteroffer(&short, 2_001),
            Err(ApplyError::CounterofferOutOfTime { .. })
        ));
        assert!(book.valid_counteroffers(neg, 2_001).is_empty());
        // A fresh counteroffer replaces the expired entry (the expired one is
        // pruned first, so the fresh entry is an insert).
        let fresh = counteroffer(&p, neg, vec![2], 2_000, TEST_VALID_FOR_MS);
        assert_eq!(
            book.apply_counteroffer(&fresh, 2_500),
            Ok(ApplyOutcome::Inserted)
        );
        assert_eq!(book.valid_counteroffers(neg, 2_500), vec![&fresh]);
    }

    #[test]
    fn counteroffer_future_beyond_skew_is_rejected() {
        let neg = negotiation_id(1);
        let p = provider(1);
        let offer = offer(neg, 0, p.peer_id());
        let mut book = NegotiationBook::new(&p.identity, &p.execution);
        book.register_offer(neg, 0, &offer).expect("offer slot");
        let future = counteroffer(
            &p,
            neg,
            vec![1],
            1_000 + arena0_protocol::MAX_CLOCK_SKEW_MS + 1,
            TEST_VALID_FOR_MS,
        );
        assert!(matches!(
            book.apply_counteroffer(&future, 1_000),
            Err(ApplyError::CounterofferOutOfTime { .. })
        ));
        assert!(book.valid_counteroffers(neg, 1_000).is_empty());
    }

    #[test]
    fn counteroffer_capacity_is_bounded_per_negotiation() {
        let neg = negotiation_id(1);
        let p = provider(1);
        let tracked = offer(neg, 0, p.peer_id());
        let mut book = NegotiationBook::new(&p.identity, &p.execution);
        book.register_offer(neg, 0, &tracked).expect("offer slot");
        for seed in 1..=MAX_HEADS {
            let p = provider(seed as u8);
            let c = counteroffer(&p, neg, vec![seed as u8], 1_000, TEST_VALID_FOR_MS);
            assert_eq!(
                book.apply_counteroffer(&c, 1_500),
                Ok(ApplyOutcome::Inserted)
            );
        }
        let extra = counteroffer(&provider(65), neg, vec![65], 1_000, TEST_VALID_FOR_MS);
        assert_eq!(
            book.apply_counteroffer(&extra, 1_500),
            Err(ApplyError::CounterofferCapacity { max: MAX_HEADS })
        );
        // A different negotiation is a separate bounded book.
        let other_neg = negotiation_id(2);
        let other_offer = offer(other_neg, 0, p.peer_id());
        book.register_offer(other_neg, 0, &other_offer)
            .expect("offer slot");
        let c = counteroffer(&provider(1), other_neg, vec![1], 1_000, TEST_VALID_FOR_MS);
        assert_eq!(
            book.apply_counteroffer(&c, 1_500),
            Ok(ApplyOutcome::Inserted)
        );
    }

    #[test]
    fn counteroffer_invalid_signature_is_rejected() {
        let neg = negotiation_id(1);
        let p = provider(1);
        let offer = offer(neg, 0, p.peer_id());
        let mut book = NegotiationBook::new(&p.identity, &p.execution);
        book.register_offer(neg, 0, &offer).expect("offer slot");
        let mut c = counteroffer(&p, neg, vec![1], 1_000, TEST_VALID_FOR_MS);
        c.signature = Ed25519Signature([0; 64]);
        assert!(matches!(
            book.apply_counteroffer(&c, 1_500),
            Err(ApplyError::InvalidCounteroffer(_))
        ));
        assert!(book.valid_counteroffers(neg, 1_500).is_empty());
    }

    // ── Local issuance ──────────────────────────────────────────────────

    #[test]
    fn creator_issues_its_ticket_immediately() {
        let neg = negotiation_id(1);
        let creator = provider(1);
        let offer = offer(neg, 0, creator.peer_id());
        let mut book = NegotiationBook::new(&creator.identity, &creator.execution);
        book.register_offer(neg, 0, &offer).expect("offer slot");
        let ticket = book
            .issue_ticket(neg, 0, 0, TEST_ISSUED_AT_MS)
            .expect("creator issues immediately");
        assert_eq!(ticket.data.signer, creator.peer_id());
        assert_eq!(ticket.data.revision, 0);
        // The issued ticket is applied and verifies.
        assert_eq!(
            book.current_ticket(neg, 0, creator.peer_id()),
            Some(&ticket)
        );
        assert!(ticket.verify_identity().unwrap_or(false));
    }

    #[test]
    fn participant_cannot_issue_before_the_creator_ticket_is_applied() {
        let neg = negotiation_id(1);
        let creator = provider(2);
        let participant = provider(1);
        let offer = offer(neg, 0, creator.peer_id());
        let mut book = NegotiationBook::new(&participant.identity, &participant.execution);
        book.register_offer(neg, 0, &offer).expect("offer slot");
        // The offer is not yet authenticated: the creator's Active ticket is
        // missing, so the participant's consent must not be issued.
        assert_eq!(
            book.issue_ticket(neg, 0, 0, TEST_ISSUED_AT_MS),
            Err(ApplyError::UnauthenticatedOffer {
                negotiation_id: neg,
                offer_seq: 0
            })
        );
        // The creator's Active ticket authenticates the offer.
        let creator_ticket = active_ticket(&creator, &offer, 0);
        book.apply_ticket(&creator_ticket).expect("creator ticket");
        let ticket = book
            .issue_ticket(neg, 0, 0, TEST_ISSUED_AT_MS)
            .expect("participant issues after authentication");
        assert_eq!(ticket.data.signer, participant.peer_id());
        assert_eq!(book.len(neg, 0), 2);
    }

    #[test]
    fn a_withdrawn_creator_ticket_does_not_authenticate_issuance() {
        let neg = negotiation_id(1);
        let creator = provider(2);
        let participant = provider(1);
        let offer = offer(neg, 0, creator.peer_id());
        let mut book = NegotiationBook::new(&participant.identity, &participant.execution);
        book.register_offer(neg, 0, &offer).expect("offer slot");
        // The creator's Withdrawn ticket means the creator withdrew: the
        // offer is dead, so the participant's consent must not be issued.
        let withdrawn = withdrawn_ticket(&creator, &offer, 1);
        book.apply_ticket(&withdrawn).expect("withdrawn applies");
        assert_eq!(
            book.issue_ticket(neg, 0, 0, TEST_ISSUED_AT_MS),
            Err(ApplyError::UnauthenticatedOffer {
                negotiation_id: neg,
                offer_seq: 0
            })
        );
    }

    #[test]
    fn issue_ticket_rejects_unknown_offers_and_negotiations() {
        let neg = negotiation_id(1);
        let p = provider(1);
        let offer = offer(neg, 0, p.peer_id());
        let mut book = NegotiationBook::new(&p.identity, &p.execution);
        assert_eq!(
            book.issue_ticket(neg, 0, 0, TEST_ISSUED_AT_MS),
            Err(ApplyError::UnknownNegotiation {
                negotiation_id: neg
            })
        );
        book.register_offer(neg, 0, &offer).expect("offer slot");
        assert_eq!(
            book.issue_ticket(neg, 1, 0, TEST_ISSUED_AT_MS),
            Err(ApplyError::UnknownOffer {
                negotiation_id: neg,
                offer_seq: 1
            })
        );
    }

    #[test]
    fn emit_counteroffer_applies_and_returns() {
        let neg = negotiation_id(1);
        let p = provider(1);
        let offer = offer(neg, 0, p.peer_id());
        let mut book = NegotiationBook::new(&p.identity, &p.execution);
        // Unknown negotiation: rejected.
        assert_eq!(
            book.emit_counteroffer(neg, vec![0xA0], 1_000),
            Err(ApplyError::UnknownNegotiation {
                negotiation_id: neg
            })
        );
        book.register_offer(neg, 0, &offer).expect("offer slot");
        let counteroffer = book
            .emit_counteroffer(neg, vec![0xA0], 1_000)
            .expect("emit");
        assert_eq!(counteroffer.data.signer, p.peer_id());
        assert_eq!(counteroffer.data.params, vec![0xA0]);
        assert_eq!(
            book.current_counteroffer(neg, p.peer_id()),
            Some(&counteroffer)
        );
        // A later emission replaces the earlier one.
        let newer = book
            .emit_counteroffer(neg, vec![0xB0], 2_000)
            .expect("emit again");
        assert_eq!(book.current_counteroffer(neg, p.peer_id()), Some(&newer));
    }
}
