//! Session negotiation domain values and the confirmed ensemble evidence.
//!
//! Domain negotiation values (offers, tickets, activation signatures,
//! counteroffers, and the validated convergence-fetch payload) and the
//! session-level [`Activation`] live here. Raw bounded fetch frames and their
//! canonical transport encoding belong to `arena0-wire`.
//!
//! `SessionHash = BLAKE3(Borsh(ActivationData))`; the committed activation's
//! aggregate (BLS over the same `ActivationData`) is the collective signature.
//! The verification path is a certificate-chain check: the aggregate verifies
//! against Σ BLS keys; each key is certified by its key certificate (ed25519
//! sig: PeerId → key); each key's possession is proven by its scope-bound
//! `key_binding`.
//!
//! Everything is periodic broadcast on the program topic — no pull, no direct
//! streams, no blobs for terms — with one bounded exception: the convergence
//! fetch (a peer pulls missed tickets from the creator before producing its
//! activation signature). Signatures live **outside** `data`: `data` is the
//! signed content, never a signature container. Identity signatures (ed25519)
//! cover their own `data`; every BLS signature covers the `ActivationData`
//! (the exact activation preimage) — except `key_binding`, which covers its
//! dedicated binding preimage (`BLS_BINDING_DOMAIN || OfferHash || signer ||
//! execution_bls`).
//!
//! Hashes are hashes: `OfferHash`/`TicketHash`/`CounterofferHash` are BLAKE3
//! of the canonical Borsh bytes of their `data`; `SessionHash` is BLAKE3 of
//! the canonical `ActivationData`. `NegotiationId` is random, creator-chosen —
//! not a hash.

id_type!(
    /// A 32-byte negotiation identity, assigned randomly by the negotiation
    /// creator. One shared thread for compatible execution requests; distinct
    /// from any `ExecId`, which stays local to one agent. Not a hash.
    pub struct Id
);

/// At most 64 participants in one activation, and therefore at most 64 tickets
/// in one offer and one convergence-fetch response.
pub const MAX_PARTICIPANTS: usize = 64;

/// Maximum accepted difference between a receiver's clock and a ticket issue
/// time, in milliseconds.
pub const MAX_CLOCK_SKEW_MS: u64 = 5_000;

/// Maximum lifetime of one signed ticket, in milliseconds.
pub const MAX_TICKET_LIFETIME_MS: u64 = 60_000;

/// Required expiry margin before the creator can select a ticket, in
/// milliseconds.
pub const PREPARE_WINDOW_MS: u64 = 10_000;

/// The exact unsigned activation evidence durably fixed before the host signs.
///
/// Construction validates a complete frozen [`Offer`], the exact full ticket
/// set it names, every ticket's certificate chain, and the canonical creator-
/// first ordering. The derived activation preimage and session identity are
/// retained with the evidence so consumers cannot accidentally derive them
/// from a different representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedActivation {
    offer: Offer,
    tickets: Vec<Ticket>,
    activation_data: ActivationData,
    session_hash: SessionHash,
}

/// A committed activation: prepared evidence plus the mandatory N-of-N
/// collective signature over its exact activation preimage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activation {
    prepared: PreparedActivation,
    aggregate: BlsSignature,
}

/// The bounded gossip announcement for a committed activation.
///
/// An announcement contains the complete frozen offer and its aggregate, but
/// not the full ticket bodies. It is only structurally valid on its own;
/// receivers combine it with their exact durable [`PreparedActivation`] before
/// an [`Activation`] can be constructed and cryptographically validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationAnnouncement {
    offer: Offer,
    aggregate: BlsSignature,
}

/// Why an offer and ticket set could not form an activation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActivationError {
    /// The offer failed bounded structural validation.
    #[error("invalid activation offer: {0}")]
    InvalidOffer(#[from] crate::NegotiationError),
    /// The ticket set does not match the offer's ticket list exactly.
    #[error("activation ticket set does not match the offer's ticket list")]
    TicketSetMismatch,
    /// The ticket set is not in canonical order (creator first, rest ascending
    /// `PeerId`) or repeats a signer.
    #[error("activation ticket set is not in canonical order")]
    InvalidOrdering,
    /// A selected ticket is not `Active`.
    #[error("activation ticket {0} is not active")]
    WithdrawnTicket(TicketHash),
    /// A selected ticket failed structural or signature verification.
    #[error("activation ticket {0} is invalid: {1}")]
    InvalidTicket(TicketHash, String),
    /// The announcement names different frozen facts than the durable
    /// preparation.
    #[error("activation announcement does not match prepared activation")]
    AnnouncementMismatch,
    /// The collective signature failed verification.
    #[error("activation collective signature failed verification: {0}")]
    InvalidAttestations(String),
}

impl Activation {
    /// Construct and cryptographically verify a committed activation from
    /// exact prepared evidence and its N-of-N aggregate.
    pub fn new(
        prepared: PreparedActivation,
        aggregate: BlsSignature,
    ) -> Result<Self, ActivationError> {
        let activation = Self {
            prepared,
            aggregate,
        };
        activation.validate()?;
        Ok(activation)
    }

    /// Combine a bounded gossip announcement with exact durable preparation
    /// evidence. The announcement cannot introduce ticket bodies or alter the
    /// preimage fixed by preparation.
    pub fn from_announcement(
        prepared: PreparedActivation,
        announcement: ActivationAnnouncement,
    ) -> Result<Self, ActivationError> {
        if prepared.offer != announcement.offer {
            return Err(ActivationError::AnnouncementMismatch);
        }
        Self::new(prepared, announcement.aggregate)
    }

    /// The exact prepared evidence owned by this activation.
    #[must_use]
    pub const fn prepared(&self) -> &PreparedActivation {
        &self.prepared
    }

    /// Borrow the frozen unsigned offer owned by the prepared evidence.
    #[must_use]
    pub const fn offer(&self) -> &Offer {
        self.prepared.offer()
    }

    /// Borrow the full ticket evidence in canonical order.
    #[must_use]
    pub fn tickets(&self) -> &[Ticket] {
        self.prepared.tickets()
    }

    /// The mandatory collective signature over [`Self::activation_data`].
    #[must_use]
    pub const fn aggregate(&self) -> &BlsSignature {
        &self.aggregate
    }

    /// The activation preimage fixed during preparation.
    #[must_use]
    pub const fn activation_data(&self) -> &ActivationData {
        self.prepared.activation_data()
    }

    /// The session identity: BLAKE3 over the canonical `ActivationData`.
    /// Fixed at activation.
    #[must_use]
    pub const fn session_hash(&self) -> SessionHash {
        self.prepared.session_hash()
    }

    /// Structural checks without cryptography: offer bounds, exact ticket-set
    /// match (hashes and order), canonical ordering (creator first, rest
    /// ascending `PeerId`, no repeated signer), and Active-only tickets.
    pub fn check_structure(&self) -> Result<(), ActivationError> {
        self.prepared.check_structure()
    }

    /// Check the certificate chain: every ticket's ed25519 identity signature,
    /// every Active ticket's scope-bound `key_binding` over the offer hash,
    /// and the collective signature over the canonical `ActivationData`
    /// against the tickets' execution BLS keys in order. Host-only.
    pub fn validate(&self) -> Result<(), ActivationError> {
        self.check_structure()?;
        let keys = self
            .prepared
            .tickets
            .iter()
            .map(|ticket| match &ticket.data.action {
                TicketAction::Active { execution_bls, .. } => *execution_bls,
                TicketAction::Withdrawn => unreachable!("Withdrawn rejected by check_structure"),
            })
            .collect::<Vec<BlsPublicKey>>();
        if !self
            .aggregate
            .fast_aggregate_verify(&self.activation_data().signing_bytes(), &keys)
            .map_err(|error| ActivationError::InvalidAttestations(error.to_string()))?
        {
            return Err(ActivationError::InvalidAttestations(
                "collective signature does not verify against the ticket set".into(),
            ));
        }
        Ok(())
    }
}

impl PreparedActivation {
    /// Validate and construct unsigned preparation evidence.
    pub fn new(offer: Offer, tickets: Vec<Ticket>) -> Result<Self, ActivationError> {
        let activation_data = check_activation_structure(&offer, &tickets)?;
        let session_hash = SessionHash::of(&activation_data);
        Ok(Self {
            offer,
            tickets,
            activation_data,
            session_hash,
        })
    }

    /// Borrow the frozen unsigned offer.
    #[must_use]
    pub const fn offer(&self) -> &Offer {
        &self.offer
    }

    /// Borrow the full tickets in exact canonical order.
    #[must_use]
    pub fn tickets(&self) -> &[Ticket] {
        &self.tickets
    }

    /// Borrow the activation preimage fixed during preparation.
    #[must_use]
    pub const fn activation_data(&self) -> &ActivationData {
        &self.activation_data
    }

    /// Return the session identity fixed during preparation.
    #[must_use]
    pub const fn session_hash(&self) -> SessionHash {
        self.session_hash
    }

    /// Revalidate prepared evidence after decoding or crossing a trust
    /// boundary. Normal code cannot construct an invalid value because all
    /// fields are private and [`Self::new`] is validating.
    pub fn validate(&self) -> Result<(), ActivationError> {
        let activation_data = check_activation_structure(&self.offer, &self.tickets)?;
        if activation_data != self.activation_data
            || SessionHash::of(&self.activation_data) != self.session_hash
        {
            return Err(ActivationError::TicketSetMismatch);
        }
        Ok(())
    }

    /// Whether a committed activation retains these exact prepared facts,
    /// including the full signed tickets.
    #[must_use]
    pub fn matches(&self, activation: &Activation) -> bool {
        self == &activation.prepared
    }

    fn check_structure(&self) -> Result<(), ActivationError> {
        check_activation_structure(&self.offer, &self.tickets).map(|_| ())
    }
}

fn check_activation_structure(
    offer: &Offer,
    tickets: &[Ticket],
) -> Result<ActivationData, ActivationError> {
    offer.validate()?;
    if !offer.is_complete() {
        return Err(ActivationError::TicketSetMismatch);
    }
    if tickets.len() != offer.tickets.len() || tickets.len() != usize::from(offer.data.target_size)
    {
        return Err(ActivationError::TicketSetMismatch);
    }
    for (ticket, hash) in tickets.iter().zip(&offer.tickets) {
        if TicketHash::of(&ticket.data) != *hash {
            return Err(ActivationError::TicketSetMismatch);
        }
        ticket.validate().map_err(|error| {
            ActivationError::InvalidTicket(TicketHash::of(&ticket.data), error.to_string())
        })?;
        if !matches!(ticket.data.action, TicketAction::Active { .. }) {
            return Err(ActivationError::WithdrawnTicket(TicketHash::of(
                &ticket.data,
            )));
        }
    }
    if tickets.first().map(|ticket| ticket.data.signer) != Some(offer.data.creator) {
        return Err(ActivationError::InvalidOrdering);
    }
    let mut seen = std::collections::HashSet::with_capacity(tickets.len());
    for ticket in tickets {
        if !seen.insert(ticket.data.signer) {
            return Err(ActivationError::InvalidOrdering);
        }
    }
    for pair in tickets[1..].windows(2) {
        if pair[0].data.signer >= pair[1].data.signer {
            return Err(ActivationError::InvalidOrdering);
        }
    }
    let activation_data = ActivationData::new(OfferHash::of(&offer.data), offer.tickets.clone())?;
    verify_activation_tickets(offer, tickets)?;
    Ok(activation_data)
}

fn verify_activation_tickets(offer: &Offer, tickets: &[Ticket]) -> Result<(), ActivationError> {
    let offer_hash = OfferHash::of(&offer.data);
    for ticket in tickets {
        let hash = TicketHash::of(&ticket.data);
        match ticket.verify_for_offer(&offer_hash) {
            Ok(()) => {}
            Err(TicketVerificationError::IdentityMismatch) => {
                return Err(ActivationError::InvalidTicket(
                    hash,
                    "identity signature mismatch".into(),
                ));
            }
            Err(TicketVerificationError::KeyBindingMismatch) => {
                return Err(ActivationError::InvalidTicket(
                    hash,
                    "key binding mismatch".into(),
                ));
            }
            Err(error) => {
                return Err(ActivationError::InvalidTicket(hash, error.to_string()));
            }
        }
    }
    Ok(())
}

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
use std::io;
use thiserror::Error;

use crate::id::id_type;
use arena0_crypto::{BlsPublicKey, BlsSignature, CryptoError, Ed25519Signature, SignScheme};

use crate::{NegotiationId, PeerId, SessionHash, StateHash};
use arena0_program::{ExecutionProfileHash, JsonBytes, ProgramHash};
use arena0_wire::{MAX_FETCH_RESPONSE_BYTES, MAX_FETCH_TICKETS};

id_type!(
    /// The offer identity: BLAKE3 of the canonical [`OfferData`] bytes.
    pub struct OfferHash
);

id_type!(
    /// The ticket identity: BLAKE3 of the canonical [`TicketData`] bytes.
    /// The signature is not part of this hash.
    pub struct TicketHash
);

id_type!(
    /// The counteroffer identity: BLAKE3 of the canonical
    /// [`CounterofferData`] bytes. Derived, not on the wire.
    pub struct CounterofferHash
);

/// Maximum encoded size of one negotiation gossip frame (the fact bound).
pub const MAX_NEGOTIATION_FACT_BYTES: usize = 4 * 1024;

/// Maximum encoded size of one offer `params` payload (the params bound).
pub const MAX_PARAMS_LEN: usize = 1024;

/// Fixed domain for a creator offer. The value is zero padded to 24 bytes.
pub const OFFER_DOMAIN: [u8; 24] = *b"arena0/offer/v1\0\0\0\0\0\0\0\0\0";
/// Offer format version.
pub const OFFER_VERSION: u16 = 1;

/// Fixed domain for a ticket (key certificate). The value is zero padded to
/// 24 bytes.
pub const TICKET_DOMAIN: [u8; 24] = *b"arena0/ticket/v2\0\0\0\0\0\0\0\0";
/// Ticket format version.
pub const TICKET_VERSION: u16 = 2;

/// Fixed domain for the derived activation preimage. The value is zero padded
/// to 24 bytes. `ActivationData` is derived, never on the wire.
pub const ACTIVATION_DOMAIN: [u8; 24] = *b"arena0/activation/v1\0\0\0\0";
/// ActivationData format version.
pub const ACTIVATION_VERSION: u16 = 1;

/// Fixed domain for an activation signature. The value is zero padded to
/// 24 bytes.
pub const ACTIVATION_SIG_DOMAIN: [u8; 24] = *b"arena0/activation-sig/v1";
/// Activation signature format version.
pub const ACTIVATION_SIG_VERSION: u16 = 1;

/// Fixed domain for a counteroffer. The value is zero padded to 24 bytes.
pub const COUNTEROFFER_DOMAIN: [u8; 24] = *b"arena0/counteroffer/v1\0\0";
/// Counteroffer format version.
pub const COUNTEROFFER_VERSION: u16 = 1;

const _: () = assert!(OFFER_DOMAIN.len() == 24);
const _: () = assert!(TICKET_DOMAIN.len() == 24);
const _: () = assert!(ACTIVATION_DOMAIN.len() == 24);
const _: () = assert!(ACTIVATION_SIG_DOMAIN.len() == 24);
const _: () = assert!(COUNTEROFFER_DOMAIN.len() == 24);

/// Why a negotiation value failed bounded structural validation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NegotiationError {
    /// A signed or immutable body has the wrong domain tag.
    #[error("{kind} uses domain {domain:?}")]
    WrongDomain {
        /// The body or envelope whose domain failed validation.
        kind: &'static str,
        /// The received domain.
        domain: [u8; 24],
    },
    /// A signed or immutable body has the wrong format version.
    #[error("{kind} uses version {version}, expected 1")]
    WrongVersion {
        /// The body or envelope whose version failed validation.
        kind: &'static str,
        /// The received version.
        version: u16,
    },
    /// A target size is outside the protocol participant bound.
    #[error("negotiation target size {target_size} outside allowed range 2..={max}")]
    InvalidTargetSize { target_size: u16, max: u16 },
    /// An offer `params` payload exceeds the params bound.
    #[error("offer params exceed {max} bytes, got {len}")]
    ParamsTooLarge { max: usize, len: usize },
    /// The offer names a deterministic execution environment unavailable locally.
    #[error("execution profile mismatch: offered {offered}, local {local}")]
    ExecutionProfileMismatch {
        offered: ExecutionProfileHash,
        local: ExecutionProfileHash,
    },
    /// A negotiation value exceeds the gossip frame bound.
    #[error("negotiation fact exceeds {max} bytes, got {len}")]
    FactTooLarge { max: usize, len: usize },
    /// A ticket lifetime exceeds the protocol cap.
    #[error("ticket lifetime {lifetime_ms} ms exceeds the {max_lifetime_ms} ms cap")]
    TicketLifetimeTooLong {
        lifetime_ms: u64,
        max_lifetime_ms: u64,
    },
    /// An activation ticket set has too few or too many tickets.
    #[error("activation ticket count {actual} outside allowed range {min}..={max}")]
    TicketCount {
        actual: usize,
        min: usize,
        max: usize,
    },
    /// The same ticket hash occurs more than once in one set.
    #[error("duplicate ticket hash in negotiation value: {0}")]
    DuplicateTicketHash(crate::TicketHash),
    /// A fetch response carries more tickets than the protocol bound.
    #[error("activation fetch has too many tickets: {len}, maximum {max}")]
    FetchTooManyTickets { max: usize, len: usize },
    /// A fetch response exceeds the bounded response size.
    #[error("activation fetch response exceeds {max} bytes, got {len}")]
    FetchResponseTooLarge { max: usize, len: usize },
    /// The fact scope does not match its enclosing gossip scope.
    #[error("{kind} scope does not match its enclosing scope")]
    ScopeMismatch {
        /// The nested fact kind.
        kind: &'static str,
    },
    /// A bounded Borsh frame could not be decoded.
    #[error("invalid negotiation Borsh frame: {0}")]
    Decode(String),
}

fn check_domain(
    kind: &'static str,
    actual: [u8; 24],
    expected: [u8; 24],
) -> Result<(), NegotiationError> {
    if actual != expected {
        return Err(NegotiationError::WrongDomain {
            kind,
            domain: actual,
        });
    }
    Ok(())
}

fn check_version(kind: &'static str, actual: u16, expected: u16) -> Result<(), NegotiationError> {
    if actual != expected {
        return Err(NegotiationError::WrongVersion {
            kind,
            version: actual,
        });
    }
    Ok(())
}

fn check_target_size(target_size: u16) -> Result<(), NegotiationError> {
    let max = u16::try_from(crate::negotiation::MAX_PARTICIPANTS).expect("64 fits in u16");
    if !(2..=max).contains(&target_size) {
        return Err(NegotiationError::InvalidTargetSize { target_size, max });
    }
    Ok(())
}

fn check_params_len(len: usize) -> Result<(), NegotiationError> {
    if len > MAX_PARAMS_LEN {
        return Err(NegotiationError::ParamsTooLarge {
            max: MAX_PARAMS_LEN,
            len,
        });
    }
    Ok(())
}

fn check_fact_size(len: usize) -> Result<(), NegotiationError> {
    if len > MAX_NEGOTIATION_FACT_BYTES {
        return Err(NegotiationError::FactTooLarge {
            max: MAX_NEGOTIATION_FACT_BYTES,
            len,
        });
    }
    Ok(())
}

fn decode_bounded<T, V, L>(
    bytes: &[u8],
    max_len: usize,
    limit_error: L,
    validate: V,
) -> Result<T, NegotiationError>
where
    T: BorshDeserialize,
    V: FnOnce(&T) -> Result<(), NegotiationError>,
    L: FnOnce(usize, usize) -> NegotiationError,
{
    if bytes.len() > max_len {
        return Err(limit_error(max_len, bytes.len()));
    }
    let frame =
        borsh::from_slice(bytes).map_err(|error| NegotiationError::Decode(error.to_string()))?;
    validate(&frame)?;
    Ok(frame)
}

fn fact_size_error(max: usize, len: usize) -> NegotiationError {
    NegotiationError::FactTooLarge { max, len }
}

fn fetch_response_size_error(max: usize, len: usize) -> NegotiationError {
    NegotiationError::FetchResponseTooLarge { max, len }
}

/// Verify an ed25519 signature with an explicit `PeerId` signer.
///
/// `PeerId` is the persistent ed25519 public key in this protocol. The helper
/// returns `Ok(false)` for a valid key with a non-matching signature and an
/// error for a malformed key.
pub(crate) fn verify_identity_signature(
    signer: &PeerId,
    message: &[u8],
    signature: &Ed25519Signature,
) -> Result<bool, CryptoError> {
    arena0_crypto::verify(SignScheme::Ed25519, &signer.0, message, &signature.0)
}

// ── Offer — creator, broadcast every N s ──────────────────────────────

/// A creator offer: the terms plus the growing ticket-hash list.
///
/// An offer is unsigned. During formation the list may be partial;
/// [`PreparedActivation::new`] is the only transition that accepts an offer as
/// frozen activation evidence, and it requires exactly `target_size` hashes
/// plus the matching full tickets. The committed collective signature is
/// validated only on [`Activation`]; [`ActivationAnnouncement`] carries that
/// signature for bounded gossip without the full ticket bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    data: OfferData,
    /// Creator ticket first, rest ascending `PeerId`; freezes at `target_size`.
    tickets: Vec<TicketHash>,
}

/// The signed offer body. `OfferHash = BLAKE3(Borsh(OfferData))`.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct OfferData {
    /// The fixed offer domain [`OFFER_DOMAIN`].
    pub domain: [u8; 24],
    /// The offer format version [`OFFER_VERSION`].
    pub version: u16,
    /// Random, creator-chosen; spans re-offers.
    pub negotiation_id: NegotiationId,
    /// Re-offer counter: a deserted offer is replaced by a new `offer_seq`.
    pub offer_seq: u64,
    /// Binds the offer to the creator (ed25519 does not recover keys).
    pub creator: PeerId,
    /// The program's content hash.
    pub program_hash: ProgramHash,
    /// Hash of the complete deterministic execution environment.
    pub execution_profile: ExecutionProfileHash,
    /// Exact agent-facing JSON parameter bytes, ≤ [`MAX_PARAMS_LEN`]. The
    /// guest owns JSON decoding into its concrete parameter DTO.
    pub params: JsonBytes,
    /// The ensemble size the creator is collecting.
    pub target_size: u16,
    /// The program's initial state this offer activates.
    pub initial_state: StateHash,
    /// Offer deadline in Unix milliseconds; past it, the offer is deserted.
    pub deadline_unix_ms: u64,
}

impl OfferData {
    /// Construct an offer body with its fixed domain and version.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        negotiation_id: NegotiationId,
        offer_seq: u64,
        creator: PeerId,
        program_hash: ProgramHash,
        execution_profile: ExecutionProfileHash,
        params: JsonBytes,
        target_size: u16,
        initial_state: StateHash,
        deadline_unix_ms: u64,
    ) -> Result<Self, NegotiationError> {
        let body = Self {
            domain: OFFER_DOMAIN,
            version: OFFER_VERSION,
            negotiation_id,
            offer_seq,
            creator,
            program_hash,
            execution_profile,
            params,
            target_size,
            initial_state,
            deadline_unix_ms,
        };
        body.validate()?;
        Ok(body)
    }

    /// Validate the body structure without checking signatures.
    pub fn validate(&self) -> Result<(), NegotiationError> {
        check_domain("offer", self.domain, OFFER_DOMAIN)?;
        check_version("offer", self.version, OFFER_VERSION)?;
        check_params_len(self.params.len())?;
        check_target_size(self.target_size)
    }

    /// Validate the offer and require an exact local execution environment.
    pub fn validate_for_profile(
        &self,
        local: ExecutionProfileHash,
    ) -> Result<(), NegotiationError> {
        self.validate()?;
        if self.execution_profile != local {
            return Err(NegotiationError::ExecutionProfileMismatch {
                offered: self.execution_profile,
                local,
            });
        }
        Ok(())
    }

    /// Canonical Borsh bytes: the `OfferHash` preimage.
    #[must_use]
    pub fn signing_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("OfferData is serializable")
    }
}

impl Offer {
    /// Construct a validated offer with the supplied ticket-hash list. The
    /// creator's ticket hash must be present before an offer can cross the
    /// wire; later formation creates a new value with the expanded list.
    pub fn new(data: OfferData, tickets: Vec<TicketHash>) -> Result<Self, NegotiationError> {
        let offer = Self { data, tickets };
        offer.validate()?;
        Ok(offer)
    }

    /// Borrow the offer terms.
    #[must_use]
    pub const fn data(&self) -> &OfferData {
        &self.data
    }

    /// Borrow ticket hashes in their current formation order.
    #[must_use]
    pub fn tickets(&self) -> &[TicketHash] {
        &self.tickets
    }

    /// Whether the ticket-hash list has reached the exact ensemble size.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.tickets.len() == self.data.target_size as usize
    }

    /// Validate the offer body, ticket-set bounds, and bounded canonical size.
    /// This does not verify ticket signatures or a collective signature.
    pub fn validate(&self) -> Result<(), NegotiationError> {
        self.data.validate()?;
        let max = usize::from(self.data.target_size);
        if !(1..=max).contains(&self.tickets.len()) {
            return Err(NegotiationError::TicketCount {
                actual: self.tickets.len(),
                min: 1,
                max,
            });
        }
        let mut seen = std::collections::HashSet::with_capacity(self.tickets.len());
        for ticket in &self.tickets {
            if !seen.insert(*ticket) {
                return Err(NegotiationError::DuplicateTicketHash(*ticket));
            }
        }
        check_fact_size(borsh::to_vec(self).expect("Offer is serializable").len())
    }

    /// Decode one bounded offer frame and validate its structure.
    pub fn decode(bytes: &[u8]) -> Result<Self, NegotiationError> {
        decode_bounded(
            bytes,
            MAX_NEGOTIATION_FACT_BYTES,
            fact_size_error,
            Self::validate,
        )
    }
}

#[derive(Serialize, Deserialize)]
struct OfferSerde {
    data: OfferData,
    tickets: Vec<TicketHash>,
}

impl Serialize for Offer {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        OfferSerde {
            data: self.data.clone(),
            tickets: self.tickets.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Offer {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = OfferSerde::deserialize(deserializer)?;
        Self::new(wire.data, wire.tickets).map_err(serde::de::Error::custom)
    }
}

impl BorshSerialize for Offer {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        BorshSerialize::serialize(&self.data, writer)?;
        BorshSerialize::serialize(&self.tickets, writer)
    }
}

impl BorshDeserialize for Offer {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        let data = OfferData::deserialize_reader(reader)?;
        let tickets = Vec::<TicketHash>::deserialize_reader(reader)?;
        Self::new(data, tickets).map_err(invalid_data)
    }
}

const PREPARED_ACTIVATION_ENCODING_VERSION: u8 = 1;
const ACTIVATION_ENCODING_VERSION: u8 = 1;
const ACTIVATION_ANNOUNCEMENT_ENCODING_VERSION: u8 = 1;

#[derive(Serialize, Deserialize)]
struct PreparedActivationSerde {
    version: u8,
    offer: Offer,
    tickets: Vec<Ticket>,
}

impl Serialize for PreparedActivation {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        PreparedActivationSerde {
            version: PREPARED_ACTIVATION_ENCODING_VERSION,
            offer: self.offer.clone(),
            tickets: self.tickets.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PreparedActivation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = PreparedActivationSerde::deserialize(deserializer)?;
        if wire.version != PREPARED_ACTIVATION_ENCODING_VERSION {
            return Err(serde::de::Error::custom(format!(
                "prepared activation uses encoding version {}, expected {}",
                wire.version, PREPARED_ACTIVATION_ENCODING_VERSION
            )));
        }
        Self::new(wire.offer, wire.tickets).map_err(serde::de::Error::custom)
    }
}

impl BorshSerialize for PreparedActivation {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        BorshSerialize::serialize(&PREPARED_ACTIVATION_ENCODING_VERSION, writer)?;
        BorshSerialize::serialize(&self.offer, writer)?;
        BorshSerialize::serialize(&self.tickets, writer)
    }
}

impl BorshDeserialize for PreparedActivation {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        let version = u8::deserialize_reader(reader)?;
        if version != PREPARED_ACTIVATION_ENCODING_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "prepared activation uses encoding version {version}, expected {}",
                    PREPARED_ACTIVATION_ENCODING_VERSION
                ),
            ));
        }
        let offer = Offer::deserialize_reader(reader)?;
        let tickets = Vec::<Ticket>::deserialize_reader(reader)?;
        Self::new(offer, tickets).map_err(invalid_data)
    }
}

#[derive(Serialize, Deserialize)]
struct ActivationSerde {
    version: u8,
    prepared: PreparedActivation,
    aggregate: BlsSignature,
}

impl Serialize for Activation {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ActivationSerde {
            version: ACTIVATION_ENCODING_VERSION,
            prepared: self.prepared.clone(),
            aggregate: self.aggregate,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Activation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ActivationSerde::deserialize(deserializer)?;
        if wire.version != ACTIVATION_ENCODING_VERSION {
            return Err(serde::de::Error::custom(format!(
                "activation uses encoding version {}, expected {}",
                wire.version, ACTIVATION_ENCODING_VERSION
            )));
        }
        Self::new(wire.prepared, wire.aggregate).map_err(serde::de::Error::custom)
    }
}

impl BorshSerialize for Activation {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        BorshSerialize::serialize(&ACTIVATION_ENCODING_VERSION, writer)?;
        BorshSerialize::serialize(&self.prepared, writer)?;
        BorshSerialize::serialize(&self.aggregate, writer)
    }
}

impl BorshDeserialize for Activation {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        let version = u8::deserialize_reader(reader)?;
        if version != ACTIVATION_ENCODING_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "activation uses encoding version {version}, expected {}",
                    ACTIVATION_ENCODING_VERSION
                ),
            ));
        }
        let prepared = PreparedActivation::deserialize_reader(reader)?;
        let aggregate = BlsSignature::deserialize_reader(reader)?;
        Self::new(prepared, aggregate).map_err(invalid_data)
    }
}

impl ActivationAnnouncement {
    /// Construct an announcement from a complete frozen offer and aggregate.
    /// Ticket bodies remain local durable evidence and are never included in
    /// this gossip value. The aggregate is not verified until paired with the
    /// exact prepared activation.
    fn from_parts(offer: Offer, aggregate: BlsSignature) -> Result<Self, ActivationError> {
        let announcement = Self { offer, aggregate };
        announcement.validate()?;
        Ok(announcement)
    }

    /// Build the bounded gossip announcement for a verified activation.
    #[must_use]
    pub fn from_activation(activation: &Activation) -> Self {
        Self {
            offer: activation.prepared.offer.clone(),
            aggregate: activation.aggregate,
        }
    }

    /// Borrow the complete frozen offer named by this announcement.
    #[must_use]
    pub const fn offer(&self) -> &Offer {
        &self.offer
    }

    /// Borrow the mandatory collective signature.
    #[must_use]
    pub const fn aggregate(&self) -> &BlsSignature {
        &self.aggregate
    }

    /// Validate the complete frozen offer and bounded gossip representation.
    /// This checks structure and size only; the aggregate is verified when the
    /// announcement is paired with exact prepared activation evidence.
    pub fn validate(&self) -> Result<(), ActivationError> {
        self.offer.validate()?;
        if !self.offer.is_complete() {
            return Err(ActivationError::TicketSetMismatch);
        }
        check_fact_size(
            borsh::to_vec(self)
                .expect("ActivationAnnouncement is serializable")
                .len(),
        )?;
        Ok(())
    }

    /// Decode one bounded activation announcement and validate its structure.
    pub fn decode(bytes: &[u8]) -> Result<Self, NegotiationError> {
        decode_bounded(
            bytes,
            MAX_NEGOTIATION_FACT_BYTES,
            fact_size_error,
            |frame: &Self| {
                frame
                    .validate()
                    .map_err(|error| NegotiationError::Decode(error.to_string()))
            },
        )
    }
}

#[derive(Serialize, Deserialize)]
struct ActivationAnnouncementSerde {
    version: u8,
    offer: Offer,
    aggregate: BlsSignature,
}

impl Serialize for ActivationAnnouncement {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ActivationAnnouncementSerde {
            version: ACTIVATION_ANNOUNCEMENT_ENCODING_VERSION,
            offer: self.offer.clone(),
            aggregate: self.aggregate,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ActivationAnnouncement {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = ActivationAnnouncementSerde::deserialize(deserializer)?;
        if wire.version != ACTIVATION_ANNOUNCEMENT_ENCODING_VERSION {
            return Err(serde::de::Error::custom(format!(
                "activation announcement uses encoding version {}, expected {}",
                wire.version, ACTIVATION_ANNOUNCEMENT_ENCODING_VERSION
            )));
        }
        Self::from_parts(wire.offer, wire.aggregate).map_err(serde::de::Error::custom)
    }
}

impl BorshSerialize for ActivationAnnouncement {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        BorshSerialize::serialize(&ACTIVATION_ANNOUNCEMENT_ENCODING_VERSION, writer)?;
        BorshSerialize::serialize(&self.offer, writer)?;
        BorshSerialize::serialize(&self.aggregate, writer)
    }
}

impl BorshDeserialize for ActivationAnnouncement {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        let version = u8::deserialize_reader(reader)?;
        if version != ACTIVATION_ANNOUNCEMENT_ENCODING_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "activation announcement uses encoding version {version}, expected {}",
                    ACTIVATION_ANNOUNCEMENT_ENCODING_VERSION
                ),
            ));
        }
        let offer = Offer::deserialize_reader(reader)?;
        let aggregate = BlsSignature::deserialize_reader(reader)?;
        Self::from_parts(offer, aggregate).map_err(invalid_data)
    }
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

// ── Ticket (key certificate) — participant, broadcast every N s ──────

/// A signed key certificate: "`signer` owns `execution_bls`" for one offer.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub data: TicketData,
    /// ed25519 over `TicketData`, verified with `data.signer`.
    pub signature: Ed25519Signature,
}

/// Why a ticket failed cryptographic authentication for one offer.
#[derive(Debug, Clone, thiserror::Error)]
pub enum TicketVerificationError {
    /// The ticket body failed bounded protocol validation.
    #[error("invalid ticket structure: {0}")]
    Structural(NegotiationError),
    /// The identity signature could not be processed.
    #[error("identity signature verification failed: {0}")]
    Identity(CryptoError),
    /// The identity signature was validly formed but did not match the ticket.
    #[error("identity signature mismatch")]
    IdentityMismatch,
    /// The scope-bound execution-key proof could not be processed.
    #[error("key binding verification failed: {0}")]
    KeyBinding(CryptoError),
    /// The execution key was not bound to this offer and signer.
    #[error("key binding mismatch")]
    KeyBindingMismatch,
}

/// The signed ticket body. `TicketHash = BLAKE3(Borsh(TicketData))`.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct TicketData {
    /// The fixed ticket domain [`TICKET_DOMAIN`].
    pub domain: [u8; 24],
    /// The ticket format version [`TICKET_VERSION`].
    pub version: u16,
    pub negotiation_id: NegotiationId,
    /// Which offer this ticket is for.
    pub offer_seq: u64,
    /// The ticket issuer's identity.
    pub signer: PeerId,
    /// Monotonic current-head revision for this issuer.
    pub revision: u64,
    pub action: TicketAction,
}

/// The ticket action. Validity lives only on `Active`: a withdrawal is a
/// revision tombstone, not temporary consent.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum TicketAction {
    Active {
        /// The per-execution BLS public key the activation aggregate verifies
        /// against.
        execution_bls: BlsPublicKey,
        /// BLS over `BLS_BINDING_DOMAIN || OfferHash || signer || execution_bls`
        /// with the dedicated binding ciphersuite: scope-bound possession
        /// proof (stops copy-squatting and the rogue-key attack).
        key_binding: BlsSignature,
        /// Issue timestamp in Unix milliseconds.
        issued_at_unix_ms: u64,
        /// Validity window in milliseconds, capped at
        /// [`MAX_TICKET_LIFETIME_MS`].
        valid_for_ms: u32,
    },
    Withdrawn,
}

impl BorshSerialize for TicketAction {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Self::Active {
                execution_bls,
                key_binding,
                issued_at_unix_ms,
                valid_for_ms,
            } => {
                BorshSerialize::serialize(&0u8, writer)?;
                BorshSerialize::serialize(execution_bls, writer)?;
                BorshSerialize::serialize(key_binding, writer)?;
                BorshSerialize::serialize(issued_at_unix_ms, writer)?;
                BorshSerialize::serialize(valid_for_ms, writer)
            }
            Self::Withdrawn => BorshSerialize::serialize(&1u8, writer),
        }
    }
}

impl BorshDeserialize for TicketAction {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => Ok(Self::Active {
                execution_bls: BlsPublicKey::deserialize_reader(reader)?,
                key_binding: BlsSignature::deserialize_reader(reader)?,
                issued_at_unix_ms: u64::deserialize_reader(reader)?,
                valid_for_ms: u32::deserialize_reader(reader)?,
            }),
            1 => Ok(Self::Withdrawn),
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown ticket action tag {tag}"),
            )),
        }
    }
}

impl TicketData {
    /// Construct a ticket body with its fixed domain and version.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        negotiation_id: NegotiationId,
        offer_seq: u64,
        signer: PeerId,
        revision: u64,
        action: TicketAction,
    ) -> Result<Self, NegotiationError> {
        let body = Self {
            domain: TICKET_DOMAIN,
            version: TICKET_VERSION,
            negotiation_id,
            offer_seq,
            signer,
            revision,
            action,
        };
        body.validate()?;
        Ok(body)
    }

    /// Validate the body structure: fixed domain/version and the Active
    /// lifetime cap. Neither `Active` nor `Withdrawn` is rejected: a
    /// withdrawal is a signed ticket and must still validate.
    pub fn validate(&self) -> Result<(), NegotiationError> {
        check_domain("ticket", self.domain, TICKET_DOMAIN)?;
        check_version("ticket", self.version, TICKET_VERSION)?;
        if let TicketAction::Active { valid_for_ms, .. } = self.action {
            let lifetime_ms = u64::from(valid_for_ms);
            if lifetime_ms > crate::negotiation::MAX_TICKET_LIFETIME_MS {
                return Err(NegotiationError::TicketLifetimeTooLong {
                    lifetime_ms,
                    max_lifetime_ms: crate::negotiation::MAX_TICKET_LIFETIME_MS,
                });
            }
        }
        Ok(())
    }

    /// Canonical Borsh bytes: the `TicketHash` preimage and the ed25519
    /// signature message.
    #[must_use]
    pub fn signing_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("TicketData is serializable")
    }
}

impl Ticket {
    /// Validate the body and bounded canonical size. This does not verify the
    /// identity signature.
    pub fn validate(&self) -> Result<(), NegotiationError> {
        self.data.validate()?;
        check_fact_size(borsh::to_vec(self).expect("Ticket is serializable").len())
    }

    /// Verify the ed25519 identity signature over the canonical ticket data.
    /// `Ok(false)` for a valid identity with a non-matching signature, `Err`
    /// for a malformed identity.
    pub fn verify_identity(&self) -> Result<bool, CryptoError> {
        verify_identity_signature(
            &self.data.signer,
            &self.data.signing_bytes(),
            &self.signature,
        )
    }

    /// Verify this ticket's identity signature and, when it is active, the
    /// scope-bound execution-key proof for `offer_hash`.
    ///
    /// A withdrawn ticket has no execution key to bind, so this method checks
    /// only its identity signature. Callers retain the policy decision about
    /// whether withdrawn tickets are acceptable in their context.
    pub fn verify_for_offer(&self, offer_hash: &OfferHash) -> Result<(), TicketVerificationError> {
        self.validate()
            .map_err(TicketVerificationError::Structural)?;
        if !self
            .verify_identity()
            .map_err(TicketVerificationError::Identity)?
        {
            return Err(TicketVerificationError::IdentityMismatch);
        }
        match &self.data.action {
            TicketAction::Active {
                execution_bls,
                key_binding,
                ..
            } => {
                if !arena0_crypto::verify_key_binding(
                    &offer_hash.0,
                    &self.data.signer.0,
                    execution_bls,
                    key_binding,
                )
                .map_err(TicketVerificationError::KeyBinding)?
                {
                    return Err(TicketVerificationError::KeyBindingMismatch);
                }
            }
            TicketAction::Withdrawn => {}
        }
        Ok(())
    }

    /// Decode one bounded ticket frame and validate its structure.
    pub fn decode(bytes: &[u8]) -> Result<Self, NegotiationError> {
        decode_bounded(
            bytes,
            MAX_NEGOTIATION_FACT_BYTES,
            fact_size_error,
            Self::validate,
        )
    }
}

// ── ActivationData — the exact activation preimage (derived, never on the wire)

/// The exact activation preimage: the offer plus the exact ticket set.
/// `SessionHash = BLAKE3(Borsh(ActivationData))`. Derived, never on the wire.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct ActivationData {
    /// The fixed activation domain [`ACTIVATION_DOMAIN`].
    pub domain: [u8; 24],
    /// The activation format version [`ACTIVATION_VERSION`].
    pub version: u16,
    /// Binds the exact terms.
    pub offer_hash: OfferHash,
    /// Creator first, rest ascending `PeerId` — the exact ensemble.
    pub tickets: Vec<TicketHash>,
}

impl ActivationData {
    /// Construct the activation preimage for one offer and exact ticket set.
    pub fn new(offer_hash: OfferHash, tickets: Vec<TicketHash>) -> Result<Self, NegotiationError> {
        let data = Self {
            domain: ACTIVATION_DOMAIN,
            version: ACTIVATION_VERSION,
            offer_hash,
            tickets,
        };
        data.validate()?;
        Ok(data)
    }

    /// Validate the fixed domain/version and the ticket-set bounds.
    pub fn validate(&self) -> Result<(), NegotiationError> {
        check_domain("activation", self.domain, ACTIVATION_DOMAIN)?;
        check_version("activation", self.version, ACTIVATION_VERSION)?;
        let max = crate::negotiation::MAX_PARTICIPANTS;
        if !(2..=max).contains(&self.tickets.len()) {
            return Err(NegotiationError::TicketCount {
                actual: self.tickets.len(),
                min: 2,
                max,
            });
        }
        let mut seen = std::collections::HashSet::with_capacity(self.tickets.len());
        for ticket in &self.tickets {
            if !seen.insert(*ticket) {
                return Err(NegotiationError::DuplicateTicketHash(*ticket));
            }
        }
        Ok(())
    }

    /// Canonical Borsh bytes: the `SessionHash` preimage and the BLS
    /// signature message for the aggregate and every activation signature.
    #[must_use]
    pub fn signing_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("ActivationData is serializable")
    }
}

impl SessionHash {
    /// The session identity: BLAKE3 over the canonical [`ActivationData`]
    /// bytes. Fixed at activation.
    #[must_use]
    pub fn of(activation: &ActivationData) -> Self {
        Self(*blake3::hash(&activation.signing_bytes()).as_bytes())
    }
}

// ── ActivationSignature — the confirmation, broadcast at activation ──

/// One signer's confirmation: BLS over the exact `ActivationData`.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct ActivationSignature {
    /// The fixed activation-signature domain [`ACTIVATION_SIG_DOMAIN`].
    pub domain: [u8; 24],
    /// The activation-signature format version [`ACTIVATION_SIG_VERSION`].
    pub version: u16,
    pub session_hash: SessionHash,
    /// Selects the identity-bound key — no signer field needed.
    pub ticket_hash: TicketHash,
    /// BLS over `ActivationData`.
    pub signature: BlsSignature,
}

impl ActivationSignature {
    /// Construct an activation signature with its fixed domain and version.
    pub fn new(
        session_hash: SessionHash,
        ticket_hash: TicketHash,
        signature: BlsSignature,
    ) -> Result<Self, NegotiationError> {
        let frame = Self {
            domain: ACTIVATION_SIG_DOMAIN,
            version: ACTIVATION_SIG_VERSION,
            session_hash,
            ticket_hash,
            signature,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// Validate the fixed domain/version and bounded canonical size.
    pub fn validate(&self) -> Result<(), NegotiationError> {
        check_domain("activation signature", self.domain, ACTIVATION_SIG_DOMAIN)?;
        check_version("activation signature", self.version, ACTIVATION_SIG_VERSION)?;
        check_fact_size(
            borsh::to_vec(self)
                .expect("ActivationSignature is serializable")
                .len(),
        )
    }

    /// Decode one bounded activation-signature frame and validate it.
    pub fn decode(bytes: &[u8]) -> Result<Self, NegotiationError> {
        decode_bounded(
            bytes,
            MAX_NEGOTIATION_FACT_BYTES,
            fact_size_error,
            Self::validate,
        )
    }
}

// ── Counteroffer — participant preference announcement, broadcast every N s ──

/// A non-binding preference announcement: the params the signer would accept.
/// The creator tunes re-offers to the plurality. Never enters activation
/// evidence.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct Counteroffer {
    pub data: CounterofferData,
    /// ed25519 over `CounterofferData`, verified with `data.signer`.
    pub signature: Ed25519Signature,
}

/// The signed counteroffer body. `CounterofferHash = BLAKE3(Borsh(CounterofferData))`;
/// derived, not on the wire.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct CounterofferData {
    /// The fixed counteroffer domain [`COUNTEROFFER_DOMAIN`].
    pub domain: [u8; 24],
    /// The counteroffer format version [`COUNTEROFFER_VERSION`].
    pub version: u16,
    pub negotiation_id: NegotiationId,
    /// Required — self-contained verification.
    pub signer: PeerId,
    /// Exact offer-ready bytes, ≤ [`MAX_PARAMS_LEN`].
    pub params: Vec<u8>,
    /// Issue timestamp in Unix milliseconds.
    pub issued_at_unix_ms: u64,
    /// Bounded validity — 60 s cap, 5 s skew.
    pub valid_for_ms: u32,
}

impl CounterofferData {
    /// Construct a counteroffer body with its fixed domain and version.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        negotiation_id: NegotiationId,
        signer: PeerId,
        params: Vec<u8>,
        issued_at_unix_ms: u64,
        valid_for_ms: u32,
    ) -> Result<Self, NegotiationError> {
        let body = Self {
            domain: COUNTEROFFER_DOMAIN,
            version: COUNTEROFFER_VERSION,
            negotiation_id,
            signer,
            params,
            issued_at_unix_ms,
            valid_for_ms,
        };
        body.validate()?;
        Ok(body)
    }

    /// Validate the body structure: fixed domain/version, params bound, and
    /// the validity cap.
    pub fn validate(&self) -> Result<(), NegotiationError> {
        check_domain("counteroffer", self.domain, COUNTEROFFER_DOMAIN)?;
        check_version("counteroffer", self.version, COUNTEROFFER_VERSION)?;
        check_params_len(self.params.len())?;
        let lifetime_ms = u64::from(self.valid_for_ms);
        if lifetime_ms > crate::negotiation::MAX_TICKET_LIFETIME_MS {
            return Err(NegotiationError::TicketLifetimeTooLong {
                lifetime_ms,
                max_lifetime_ms: crate::negotiation::MAX_TICKET_LIFETIME_MS,
            });
        }
        Ok(())
    }

    /// Canonical Borsh bytes: the `CounterofferHash` preimage and the ed25519
    /// signature message.
    #[must_use]
    pub fn signing_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("CounterofferData is serializable")
    }
}

impl Counteroffer {
    /// Validate the body and bounded canonical size. This does not verify the
    /// identity signature.
    pub fn validate(&self) -> Result<(), NegotiationError> {
        self.data.validate()?;
        check_fact_size(
            borsh::to_vec(self)
                .expect("Counteroffer is serializable")
                .len(),
        )
    }

    /// Verify the ed25519 identity signature over the canonical counteroffer
    /// data. `Ok(false)` for a valid identity with a non-matching signature,
    /// `Err` for a malformed identity.
    pub fn verify_identity(&self) -> Result<bool, CryptoError> {
        verify_identity_signature(
            &self.data.signer,
            &self.data.signing_bytes(),
            &self.signature,
        )
    }

    /// Decode one bounded counteroffer frame and validate its structure.
    pub fn decode(bytes: &[u8]) -> Result<Self, NegotiationError> {
        decode_bounded(
            bytes,
            MAX_NEGOTIATION_FACT_BYTES,
            fact_size_error,
            Self::validate,
        )
    }
}

// ── The convergence fetch — the one bounded pull in the design ──

/// A signer's one-shot pull of any missed tickets from the creator before
/// producing its activation signature. The `session_hash` keying works
/// because the offer carries the `TicketHash`es — the peer can compute the
/// hash without the full tickets.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct FetchActivationTickets {
    pub session_hash: SessionHash,
}

/// The creator's bounded response: the exact frozen ticket set, in the frozen
/// order. Served from frozen evidence, never the mutable book.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct ActivationTickets {
    pub session_hash: SessionHash,
    /// Exact frozen order.
    pub tickets: Vec<Ticket>,
}

impl ActivationTickets {
    /// Validate the ticket count and the bounded encoded response size.
    pub fn validate(&self) -> Result<(), NegotiationError> {
        if self.tickets.len() > MAX_FETCH_TICKETS {
            return Err(NegotiationError::FetchTooManyTickets {
                max: MAX_FETCH_TICKETS,
                len: self.tickets.len(),
            });
        }
        for ticket in &self.tickets {
            ticket.validate()?;
        }
        let len = borsh::to_vec(self)
            .expect("ActivationTickets is serializable")
            .len();
        if len > MAX_FETCH_RESPONSE_BYTES {
            return Err(NegotiationError::FetchResponseTooLarge {
                max: MAX_FETCH_RESPONSE_BYTES,
                len,
            });
        }
        Ok(())
    }

    /// Decode one bounded fetch response and validate it.
    pub fn decode(bytes: &[u8]) -> Result<Self, NegotiationError> {
        decode_bounded(
            bytes,
            MAX_FETCH_RESPONSE_BYTES,
            fetch_response_size_error,
            Self::validate,
        )
    }
}

// ── Program-topic envelope ────────────────────────────────────────────

/// Fixed domain for a negotiation gossip envelope. The value is zero padded to
/// 24 bytes. The short wire tag keeps the fixed domain width; the version is a
/// separate field.
pub const NEGOTIATION_GOSSIP_DOMAIN: [u8; 24] = *b"arena0/form-gossip/v1\0\0\0";

/// Negotiation gossip envelope format version.
pub const NEGOTIATION_GOSSIP_VERSION: u16 = 1;

const _: () = assert!(NEGOTIATION_GOSSIP_DOMAIN.len() == 24);

/// One immutable program-topic negotiation fact.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum NegotiationFact {
    /// A creator-authored unsigned offer (data + ticket hashes).
    Offer(Offer),
    /// A participant's key certificate (ed25519 consent to one offer).
    Ticket(Ticket),
    /// A signer's BLS confirmation over the exact `ActivationData`.
    ActivationSignature(ActivationSignature),
    /// The creator's bounded announcement for exact prepared activation facts.
    ActivationAnnouncement(ActivationAnnouncement),
    /// A participant's non-binding preference announcement.
    Counteroffer(Counteroffer),
}

const NEGOTIATION_FACT_OFFER: u8 = 0;
const NEGOTIATION_FACT_TICKET: u8 = 1;
const NEGOTIATION_FACT_ACTIVATION_SIGNATURE: u8 = 2;
const NEGOTIATION_FACT_COUNTEROFFER: u8 = 3;
const NEGOTIATION_FACT_ACTIVATION_ANNOUNCEMENT: u8 = 4;

impl BorshSerialize for NegotiationFact {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Self::Offer(offer) => {
                BorshSerialize::serialize(&NEGOTIATION_FACT_OFFER, writer)?;
                BorshSerialize::serialize(offer, writer)
            }
            Self::Ticket(ticket) => {
                BorshSerialize::serialize(&NEGOTIATION_FACT_TICKET, writer)?;
                BorshSerialize::serialize(ticket, writer)
            }
            Self::ActivationSignature(signature) => {
                BorshSerialize::serialize(&NEGOTIATION_FACT_ACTIVATION_SIGNATURE, writer)?;
                BorshSerialize::serialize(signature, writer)
            }
            Self::ActivationAnnouncement(announcement) => {
                BorshSerialize::serialize(&NEGOTIATION_FACT_ACTIVATION_ANNOUNCEMENT, writer)?;
                BorshSerialize::serialize(announcement, writer)
            }
            Self::Counteroffer(counteroffer) => {
                BorshSerialize::serialize(&NEGOTIATION_FACT_COUNTEROFFER, writer)?;
                BorshSerialize::serialize(counteroffer, writer)
            }
        }
    }
}

impl BorshDeserialize for NegotiationFact {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            NEGOTIATION_FACT_OFFER => Ok(Self::Offer(Offer::deserialize_reader(reader)?)),
            NEGOTIATION_FACT_TICKET => Ok(Self::Ticket(Ticket::deserialize_reader(reader)?)),
            NEGOTIATION_FACT_ACTIVATION_SIGNATURE => Ok(Self::ActivationSignature(
                ActivationSignature::deserialize_reader(reader)?,
            )),
            NEGOTIATION_FACT_ACTIVATION_ANNOUNCEMENT => Ok(Self::ActivationAnnouncement(
                ActivationAnnouncement::deserialize_reader(reader)?,
            )),
            NEGOTIATION_FACT_COUNTEROFFER => Ok(Self::Counteroffer(
                Counteroffer::deserialize_reader(reader)?,
            )),
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown negotiation fact tag {tag}"),
            )),
        }
    }
}

impl NegotiationFact {
    /// The negotiation scope carried by this fact, when the fact carries one.
    /// `ActivationSignature` has no `negotiation_id` (it is keyed by
    /// `session_hash`), so it returns `None`.
    #[must_use]
    pub fn negotiation_id(&self) -> Option<NegotiationId> {
        match self {
            Self::Offer(offer) => Some(offer.data.negotiation_id),
            Self::Ticket(ticket) => Some(ticket.data.negotiation_id),
            Self::Counteroffer(counteroffer) => Some(counteroffer.data.negotiation_id),
            Self::ActivationSignature(_) => None,
            Self::ActivationAnnouncement(announcement) => {
                Some(announcement.offer().data().negotiation_id)
            }
        }
    }
}

/// The program-topic envelope around one negotiation fact. The 4 KiB fact bound
/// applies to the complete frame.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct NegotiationGossip {
    /// The fixed negotiation-gossip domain.
    pub domain: [u8; 24],
    /// The negotiation-gossip format version.
    pub version: u16,
    /// The program scope (routing hint; the topic is program-scoped).
    pub program_id: ProgramHash,
    /// The negotiation scope (routing hint; checked against the fact when the
    /// fact carries one).
    pub negotiation_id: NegotiationId,
    /// The nested negotiation fact.
    pub fact: NegotiationFact,
}

impl NegotiationGossip {
    /// Construct an envelope for one negotiation fact.
    #[must_use]
    pub fn new(
        program_id: ProgramHash,
        negotiation_id: NegotiationId,
        fact: NegotiationFact,
    ) -> Self {
        Self {
            domain: NEGOTIATION_GOSSIP_DOMAIN,
            version: NEGOTIATION_GOSSIP_VERSION,
            program_id,
            negotiation_id,
            fact,
        }
    }

    /// Validate the envelope: fixed domain/version, nested fact, scope match
    /// when the fact carries a negotiation id, and the bounded frame size.
    pub fn validate(&self) -> Result<(), NegotiationError> {
        check_domain("negotiation gossip", self.domain, NEGOTIATION_GOSSIP_DOMAIN)?;
        check_version(
            "negotiation gossip",
            self.version,
            NEGOTIATION_GOSSIP_VERSION,
        )?;
        match &self.fact {
            NegotiationFact::Offer(offer) => offer.validate()?,
            NegotiationFact::Ticket(ticket) => ticket.validate()?,
            NegotiationFact::ActivationSignature(signature) => signature.validate()?,
            NegotiationFact::ActivationAnnouncement(announcement) => announcement
                .validate()
                .map_err(|error| NegotiationError::Decode(error.to_string()))?,
            NegotiationFact::Counteroffer(counteroffer) => counteroffer.validate()?,
        }
        if let Some(fact_negotiation_id) = self.fact.negotiation_id()
            && fact_negotiation_id != self.negotiation_id
        {
            return Err(NegotiationError::ScopeMismatch {
                kind: "negotiation fact",
            });
        }
        check_fact_size(self.signing_bytes().len())
    }

    /// Canonical Borsh bytes for the immutable gossip frame.
    #[must_use]
    pub fn signing_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("NegotiationGossip is serializable")
    }

    /// Decode one bounded gossip frame and validate its structure.
    pub fn decode(bytes: &[u8]) -> Result<Self, NegotiationError> {
        decode_bounded(
            bytes,
            MAX_NEGOTIATION_FACT_BYTES,
            fact_size_error,
            Self::validate,
        )
    }
}

// ── Hash constructors ─────────────────────────────────────────────────

impl OfferHash {
    /// The offer identity: BLAKE3 over the canonical [`OfferData`] bytes.
    #[must_use]
    pub fn of(data: &OfferData) -> Self {
        Self(*blake3::hash(&data.signing_bytes()).as_bytes())
    }
}

impl TicketHash {
    /// The ticket identity: BLAKE3 over the canonical [`TicketData`] bytes.
    /// The signature is not part of this hash.
    #[must_use]
    pub fn of(data: &TicketData) -> Self {
        Self(*blake3::hash(&data.signing_bytes()).as_bytes())
    }
}

impl CounterofferHash {
    /// The counteroffer identity: BLAKE3 over the canonical
    /// [`CounterofferData`] bytes. Derived, not on the wire.
    #[must_use]
    pub fn of(data: &CounterofferData) -> Self {
        Self(*blake3::hash(&data.signing_bytes()).as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_crypto::{BlsPublicKey, BlsSignature, Ed25519Signature};

    use crate::{NegotiationId, PeerId, StateHash};
    use arena0_program::{JsonBytes, ProgramHash};

    const CREATOR_BLS_SEED: [u8; 32] = [11; 32];

    fn json(bytes: impl Into<Vec<u8>>) -> JsonBytes {
        JsonBytes::try_new(bytes).expect("valid JSON parameters")
    }

    /// A fully valid activation: real ed25519 ticket signatures, real
    /// scope-bound key bindings, and a real BLS aggregate over the exact
    /// `ActivationData`.
    fn valid_activation() -> Activation {
        use arena0_crypto::bls::BlsSecretKey;
        use arena0_crypto::{NodeKeys, SecretKey, key_binding_message};

        let creator_keys = NodeKeys::from_secret(SecretKey::from_bytes([1; 32]));
        let other_keys = NodeKeys::from_secret(SecretKey::from_bytes([2; 32]));
        let creator_bls = BlsSecretKey::from_seed(&CREATOR_BLS_SEED).expect("bls key");
        let other_bls = BlsSecretKey::from_seed(&[12; 32]).expect("bls key");
        let creator_peer = PeerId::from_ed25519(&creator_keys.ed25519_public_key());
        let offer_data = crate::OfferData::new(
            NegotiationId([0x11; 32]),
            0,
            creator_peer,
            ProgramHash([0x22; 32]),
            arena0_program::ExecutionProfile::current().hash(),
            json(br#"{}"#),
            2,
            StateHash([0x33; 32]),
            1_000_000,
        )
        .expect("valid offer data");
        let offer_hash = OfferHash::of(&offer_data);
        let make_ticket = |keys: &NodeKeys, bls: &BlsSecretKey| {
            let peer = PeerId::from_ed25519(&keys.ed25519_public_key());
            let pk = bls.public_key();
            let binding = bls.sign_binding(&key_binding_message(&offer_hash.0, &peer.0, &pk));
            let data = crate::TicketData::new(
                NegotiationId([0x11; 32]),
                0,
                peer,
                0,
                TicketAction::Active {
                    execution_bls: pk,
                    key_binding: binding,
                    issued_at_unix_ms: 1,
                    valid_for_ms: 60_000,
                },
            )
            .expect("valid ticket data");
            Ticket {
                signature: keys.sign(&data.signing_bytes()),
                data,
            }
        };
        let creator_ticket = make_ticket(&creator_keys, &creator_bls);
        let other_ticket = make_ticket(&other_keys, &other_bls);
        let tickets = vec![creator_ticket, other_ticket];
        let ticket_hashes = tickets
            .iter()
            .map(|ticket| TicketHash::of(&ticket.data))
            .collect::<Vec<_>>();
        let offer = Offer::new(offer_data, ticket_hashes).expect("complete offer");
        let prepared = PreparedActivation::new(offer, tickets).expect("prepared activation");
        let aggregate = BlsSignature::aggregate(&[
            creator_bls.sign(&prepared.activation_data().signing_bytes()),
            other_bls.sign(&prepared.activation_data().signing_bytes()),
        ])
        .expect("aggregate");
        Activation::new(prepared, aggregate).expect("valid activation")
    }

    #[test]
    fn activation_requires_every_selected_participant_to_sign() {
        let activation = valid_activation();
        activation.validate().expect("full chain verifies");
        let offer_hash = OfferHash::of(&activation.offer().data);
        for ticket in activation.tickets() {
            ticket
                .verify_for_offer(&offer_hash)
                .expect("ticket certificate verifies");
        }

        let creator_bls = arena0_crypto::bls::BlsSecretKey::from_seed(&CREATOR_BLS_SEED).unwrap();
        let partial = creator_bls.sign(&activation.activation_data().signing_bytes());
        assert!(matches!(
            Activation::new(activation.prepared().clone(), partial),
            Err(ActivationError::InvalidAttestations(_))
        ));
    }

    #[test]
    fn prepared_activation_requires_complete_exact_ordered_set() {
        let activation = valid_activation();
        let mut hashes = activation.offer().tickets().to_vec();
        hashes.swap(0, 1);
        let offer = Offer::new(activation.offer().data.clone(), hashes).expect("offer shape");
        assert!(matches!(
            PreparedActivation::new(offer, activation.tickets().to_vec()),
            Err(ActivationError::InvalidOrdering | ActivationError::TicketSetMismatch)
        ));
    }

    #[test]
    fn announcement_preserves_exact_prepared_evidence() {
        let activation = valid_activation();
        let prepared = activation.prepared().clone();
        let announcement = ActivationAnnouncement::from_activation(&activation);
        let committed = Activation::from_announcement(prepared.clone(), announcement)
            .expect("valid announcement");
        assert!(prepared.matches(&committed));
        assert_eq!(prepared.session_hash(), committed.session_hash());
    }

    #[test]
    fn announcement_cannot_be_combined_with_different_preparation() {
        let activation = valid_activation();
        let mut data = activation.offer().data.clone();
        data.offer_seq = 1;
        let other_offer = Offer::new(data, activation.offer().tickets().to_vec()).expect("offer");
        let announcement = ActivationAnnouncement {
            offer: other_offer,
            aggregate: *activation.aggregate(),
        };
        let other = activation.prepared().clone();
        assert!(matches!(
            Activation::from_announcement(other, announcement),
            Err(ActivationError::AnnouncementMismatch)
        ));
    }

    #[test]
    fn activation_round_trips_borsh_and_serde() {
        let activation = valid_activation();
        let bytes = borsh::to_vec(&activation).expect("encode activation");
        assert_eq!(
            borsh::from_slice::<Activation>(&bytes).expect("decode activation"),
            activation
        );
        let json = serde_json::to_vec(&activation).expect("encode JSON");
        assert_eq!(
            serde_json::from_slice::<Activation>(&json).expect("decode JSON"),
            activation
        );
    }

    #[test]
    fn activation_announcement_round_trips_borsh_and_serde() {
        let activation = valid_activation();
        let announcement = ActivationAnnouncement::from_activation(&activation);
        let bytes = borsh::to_vec(&announcement).expect("encode announcement");
        assert_eq!(
            borsh::from_slice::<ActivationAnnouncement>(&bytes).expect("decode announcement"),
            announcement
        );
        let json = serde_json::to_vec(&announcement).expect("encode JSON");
        assert_eq!(
            serde_json::from_slice::<ActivationAnnouncement>(&json).expect("decode JSON"),
            announcement
        );
        let frame = NegotiationGossip::new(
            announcement.offer().data().program_hash,
            announcement.offer().data().negotiation_id,
            NegotiationFact::ActivationAnnouncement(announcement.clone()),
        );
        assert_eq!(
            NegotiationGossip::decode(&frame.signing_bytes()).unwrap(),
            frame
        );
    }

    #[test]
    fn activation_announcement_decode_revalidates_structure() {
        let activation = valid_activation();
        let mut ticket_hashes = activation.offer().tickets().to_vec();
        ticket_hashes.pop();
        let incomplete_offer = Offer::new(activation.offer().data().clone(), ticket_hashes)
            .expect("partial offer is valid during negotiation");
        let announcement = ActivationAnnouncement {
            offer: incomplete_offer,
            aggregate: *activation.aggregate(),
        };
        let bytes = borsh::to_vec(&announcement).expect("encode malformed announcement");

        assert!(matches!(
            ActivationAnnouncement::decode(&bytes),
            Err(NegotiationError::Decode(_))
        ));
    }

    fn offer_data(seq: u64, params_len: usize) -> OfferData {
        let mut params = vec![b' '; params_len.saturating_sub(2)];
        params.extend_from_slice(br#"{}"#);
        OfferData::new(
            NegotiationId([0x11; 32]),
            seq,
            PeerId([1; 32]),
            ProgramHash([0x22; 32]),
            arena0_program::ExecutionProfile::current().hash(),
            JsonBytes::try_new(params).expect("valid JSON parameters"),
            2,
            StateHash([0x33; 32]),
            1_000_000,
        )
        .expect("valid offer data")
    }

    fn ticket_data(seed: u8, seq: u64, revision: u64) -> TicketData {
        TicketData::new(
            NegotiationId([0x11; 32]),
            seq,
            PeerId([seed; 32]),
            revision,
            TicketAction::Active {
                execution_bls: BlsPublicKey([seed; 96]),
                key_binding: BlsSignature([0; 48]),
                issued_at_unix_ms: 1,
                valid_for_ms: 60_000,
            },
        )
        .expect("valid ticket data")
    }

    #[test]
    fn offer_round_trips_borsh_and_hashes_deterministically() {
        let data = offer_data(0, 3);
        let offer = Offer::new(data.clone(), vec![TicketHash([1; 32]), TicketHash([2; 32])])
            .expect("valid offer");
        offer.validate().expect("valid offer");
        let bytes = borsh::to_vec(&offer).unwrap();
        assert_eq!(borsh::from_slice::<Offer>(&bytes).unwrap(), offer);
        assert_eq!(
            OfferHash::of(&data).0,
            *blake3::hash(&data.signing_bytes()).as_bytes()
        );
    }

    #[test]
    fn offer_rejects_wrong_domain_version_params_and_oversize() {
        let mut data = offer_data(0, 3);
        data.domain = [0; 24];
        assert!(matches!(
            data.validate(),
            Err(NegotiationError::WrongDomain { .. })
        ));
        let mut data = offer_data(0, 3);
        data.version = 99;
        assert!(matches!(
            data.validate(),
            Err(NegotiationError::WrongVersion { .. })
        ));
        assert!(matches!(
            OfferData::new(
                NegotiationId([0x11; 32]),
                0,
                PeerId([1; 32]),
                ProgramHash([0x22; 32]),
                arena0_program::ExecutionProfile::current().hash(),
                JsonBytes::try_new([vec![b' '; MAX_PARAMS_LEN - 1], br#"{}"#.to_vec()].concat(),)
                    .expect("JSON remains valid"),
                2,
                StateHash([0x33; 32]),
                1_000_000,
            ),
            Err(NegotiationError::ParamsTooLarge { .. })
        ));
        let mut data = offer_data(0, 3);
        data.target_size = 1;
        assert!(matches!(
            data.validate(),
            Err(NegotiationError::InvalidTargetSize { .. })
        ));
    }

    #[test]
    fn offer_forms_from_creator_hash_and_rejects_incomplete_activation() {
        let data = offer_data(0, 3);
        let offer = Offer::new(data.clone(), vec![TicketHash([1; 32])]).expect("forming offer");
        assert!(!offer.is_complete());
        assert!(matches!(
            PreparedActivation::new(offer, Vec::new()),
            Err(ActivationError::TicketSetMismatch)
        ));
        let mut complete_data = data;
        complete_data.target_size = 3;
        let complete = Offer::new(
            complete_data,
            vec![
                TicketHash([1; 32]),
                TicketHash([2; 32]),
                TicketHash([3; 32]),
            ],
        )
        .expect("complete offer");
        assert!(complete.is_complete());
    }

    #[test]
    fn offer_rejects_duplicate_ticket_hashes() {
        let offer = Offer::new(
            offer_data(0, 3),
            vec![TicketHash([1; 32]), TicketHash([1; 32])],
        )
        .expect_err("duplicate hash must be rejected");
        assert!(matches!(offer, NegotiationError::DuplicateTicketHash(_)));
    }

    #[test]
    fn ticket_round_trips_borsh_and_hashes_signing_data() {
        let data = ticket_data(1, 0, 0);
        let ticket = Ticket {
            data: data.clone(),
            signature: Ed25519Signature([0; 64]),
        };
        ticket.validate().expect("valid ticket");
        let bytes = borsh::to_vec(&ticket).unwrap();
        assert_eq!(borsh::from_slice::<Ticket>(&bytes).unwrap(), ticket);
        assert_eq!(
            TicketHash::of(&data).0,
            *blake3::hash(&data.signing_bytes()).as_bytes()
        );
    }

    #[test]
    fn ticket_rejects_wrong_domain_version_and_lifetime() {
        let mut data = ticket_data(1, 0, 0);
        data.domain = [0; 24];
        assert!(matches!(
            data.validate(),
            Err(NegotiationError::WrongDomain { .. })
        ));
        let mut data = ticket_data(1, 0, 0);
        data.version = 99;
        assert!(matches!(
            data.validate(),
            Err(NegotiationError::WrongVersion { .. })
        ));
        let data = TicketData::new(
            NegotiationId([0x11; 32]),
            0,
            PeerId([1; 32]),
            0,
            TicketAction::Active {
                execution_bls: BlsPublicKey([1; 96]),
                key_binding: BlsSignature([0; 48]),
                issued_at_unix_ms: 1,
                valid_for_ms: 60_001,
            },
        );
        assert!(matches!(
            data,
            Err(NegotiationError::TicketLifetimeTooLong { .. })
        ));
    }

    #[test]
    fn withdrawn_ticket_validates() {
        let data = TicketData::new(
            NegotiationId([0x11; 32]),
            0,
            PeerId([1; 32]),
            1,
            TicketAction::Withdrawn,
        )
        .expect("withdrawn ticket data");
        let ticket = Ticket {
            data,
            signature: Ed25519Signature([0; 64]),
        };
        assert!(ticket.validate().is_ok());
    }

    #[test]
    fn activation_data_derives_session_hash() {
        let data = ActivationData::new(
            OfferHash([1; 32]),
            vec![TicketHash([2; 32]), TicketHash([3; 32])],
        )
        .expect("valid activation data");
        assert_eq!(
            SessionHash::of(&data).0,
            *blake3::hash(&data.signing_bytes()).as_bytes()
        );
        // A different ticket set derives a different session hash.
        let other = ActivationData::new(
            OfferHash([1; 32]),
            vec![TicketHash([2; 32]), TicketHash([4; 32])],
        )
        .expect("valid activation data");
        assert_ne!(SessionHash::of(&data), SessionHash::of(&other));
    }

    #[test]
    fn execution_profile_is_bound_into_offer_and_session_identity() {
        let offer = offer_data(0, 3);
        let mut other_offer = offer.clone();
        other_offer.execution_profile = ExecutionProfileHash([0xA5; 32]);

        let offer_hash = OfferHash::of(&offer);
        let other_offer_hash = OfferHash::of(&other_offer);
        assert_ne!(offer_hash, other_offer_hash);

        let tickets = vec![TicketHash([2; 32]), TicketHash([3; 32])];
        let activation = ActivationData::new(offer_hash, tickets.clone()).unwrap();
        let other_activation = ActivationData::new(other_offer_hash, tickets).unwrap();
        assert_ne!(
            SessionHash::of(&activation),
            SessionHash::of(&other_activation)
        );
        assert!(matches!(
            offer.validate_for_profile(other_offer.execution_profile),
            Err(NegotiationError::ExecutionProfileMismatch { .. })
        ));
    }

    #[test]
    fn activation_data_rejects_bad_sets() {
        assert!(matches!(
            ActivationData::new(OfferHash([1; 32]), vec![TicketHash([2; 32])]),
            Err(NegotiationError::TicketCount { .. })
        ));
        assert!(matches!(
            ActivationData::new(
                OfferHash([1; 32]),
                vec![TicketHash([2; 32]), TicketHash([2; 32])]
            ),
            Err(NegotiationError::DuplicateTicketHash(_))
        ));
    }

    #[test]
    fn activation_signature_round_trips_borsh() {
        let frame = ActivationSignature::new(
            SessionHash([1; 32]),
            TicketHash([2; 32]),
            BlsSignature([3; 48]),
        )
        .expect("valid activation signature");
        let bytes = borsh::to_vec(&frame).unwrap();
        assert_eq!(
            borsh::from_slice::<ActivationSignature>(&bytes).unwrap(),
            frame
        );
        let mut bad = frame.clone();
        bad.domain = [0; 24];
        assert!(matches!(
            bad.validate(),
            Err(NegotiationError::WrongDomain { .. })
        ));
    }

    #[test]
    fn activation_signature_verifies_over_activation_data() {
        use arena0_crypto::bls::BlsSecretKey;
        let sk = BlsSecretKey::from_seed(&[7; 32]).expect("bls key");
        let pk = sk.public_key();
        let data = ActivationData::new(
            OfferHash([1; 32]),
            vec![TicketHash([2; 32]), TicketHash([3; 32])],
        )
        .expect("valid activation data");
        let session_hash = SessionHash::of(&data);
        let sig = sk.sign(&data.signing_bytes());
        let frame = ActivationSignature::new(session_hash, TicketHash([2; 32]), sig)
            .expect("valid activation signature");
        assert!(
            pk.verify(&data.signing_bytes(), &frame.signature)
                .expect("verify")
        );
        // A different preimage does not verify.
        let other = ActivationData::new(
            OfferHash([1; 32]),
            vec![TicketHash([2; 32]), TicketHash([4; 32])],
        )
        .expect("valid activation data");
        assert!(
            !pk.verify(&other.signing_bytes(), &frame.signature)
                .expect("verify")
        );
    }

    #[test]
    fn counteroffer_round_trips_borsh_and_validates() {
        let data = CounterofferData::new(
            NegotiationId([0x11; 32]),
            PeerId([1; 32]),
            vec![0xAA; 3],
            1,
            60_000,
        )
        .expect("valid counteroffer data");
        let counteroffer = Counteroffer {
            data: data.clone(),
            signature: Ed25519Signature([0; 64]),
        };
        counteroffer.validate().expect("valid counteroffer");
        let bytes = borsh::to_vec(&counteroffer).unwrap();
        assert_eq!(
            borsh::from_slice::<Counteroffer>(&bytes).unwrap(),
            counteroffer
        );
        assert!(matches!(
            CounterofferData::new(
                NegotiationId([0x11; 32]),
                PeerId([1; 32]),
                vec![0; MAX_PARAMS_LEN + 1],
                1,
                60_000,
            ),
            Err(NegotiationError::ParamsTooLarge { .. })
        ));
        assert!(matches!(
            CounterofferData::new(
                NegotiationId([0x11; 32]),
                PeerId([1; 32]),
                vec![],
                1,
                60_001,
            ),
            Err(NegotiationError::TicketLifetimeTooLong { .. })
        ));
    }

    #[test]
    fn fetch_frames_round_trip_and_bounds() {
        let fetch = FetchActivationTickets {
            session_hash: SessionHash([1; 32]),
        };
        let bytes = borsh::to_vec(&fetch).unwrap();
        assert_eq!(
            borsh::from_slice::<FetchActivationTickets>(&bytes).unwrap(),
            fetch
        );

        let response = ActivationTickets {
            session_hash: SessionHash([1; 32]),
            tickets: vec![
                Ticket {
                    data: ticket_data(1, 0, 0),
                    signature: Ed25519Signature([0; 64]),
                },
                Ticket {
                    data: ticket_data(2, 0, 0),
                    signature: Ed25519Signature([0; 64]),
                },
            ],
        };
        response.validate().expect("valid fetch response");
        let bytes = borsh::to_vec(&response).unwrap();
        assert_eq!(
            borsh::from_slice::<ActivationTickets>(&bytes).unwrap(),
            response
        );

        let too_many = ActivationTickets {
            session_hash: SessionHash([1; 32]),
            tickets: (0..=MAX_FETCH_TICKETS)
                .map(|i| Ticket {
                    data: ticket_data(i as u8, 0, 0),
                    signature: Ed25519Signature([0; 64]),
                })
                .collect(),
        };
        assert!(matches!(
            too_many.validate(),
            Err(NegotiationError::FetchTooManyTickets { .. })
        ));
        let oversize = vec![0; MAX_FETCH_RESPONSE_BYTES + 1];
        assert!(matches!(
            ActivationTickets::decode(&oversize),
            Err(NegotiationError::FetchResponseTooLarge { .. })
        ));
    }

    #[test]
    fn decode_rejects_oversize_before_borsh() {
        let bytes = vec![0; MAX_NEGOTIATION_FACT_BYTES + 1];
        assert!(matches!(
            Offer::decode(&bytes),
            Err(NegotiationError::FactTooLarge { .. })
        ));
        assert!(matches!(
            Ticket::decode(&bytes),
            Err(NegotiationError::FactTooLarge { .. })
        ));
        assert!(matches!(
            ActivationSignature::decode(&bytes),
            Err(NegotiationError::FactTooLarge { .. })
        ));
        assert!(matches!(
            Counteroffer::decode(&bytes),
            Err(NegotiationError::FactTooLarge { .. })
        ));
        assert!(matches!(
            ActivationAnnouncement::decode(&bytes),
            Err(NegotiationError::FactTooLarge { .. })
        ));
    }

    #[test]
    fn gossip_envelope_round_trips_and_checks_scope() {
        let offer = Offer::new(
            OfferData::new(
                NegotiationId([1; 32]),
                0,
                PeerId([2; 32]),
                ProgramHash([3; 32]),
                arena0_program::ExecutionProfile::current().hash(),
                json(br#"[1,2,3]"#),
                2,
                StateHash([4; 32]),
                1_000,
            )
            .expect("valid offer data"),
            vec![TicketHash([5; 32])],
        )
        .expect("valid offer");
        let frame = NegotiationGossip::new(
            ProgramHash([3; 32]),
            NegotiationId([1; 32]),
            NegotiationFact::Offer(offer),
        );
        assert!(frame.validate().is_ok());
        let bytes = frame.signing_bytes();
        assert_eq!(NegotiationGossip::decode(&bytes).unwrap(), frame);

        // A mismatched envelope negotiation scope is rejected.
        let wrong_scope = NegotiationGossip::new(
            ProgramHash([3; 32]),
            NegotiationId([9; 32]),
            NegotiationFact::Offer(
                Offer::new(
                    OfferData::new(
                        NegotiationId([1; 32]),
                        0,
                        PeerId([2; 32]),
                        ProgramHash([3; 32]),
                        arena0_program::ExecutionProfile::current().hash(),
                        json(br#"[1,2,3]"#),
                        2,
                        StateHash([4; 32]),
                        1_000,
                    )
                    .expect("valid offer data"),
                    vec![TicketHash([5; 32])],
                )
                .expect("valid offer"),
            ),
        );
        assert!(matches!(
            wrong_scope.validate(),
            Err(NegotiationError::ScopeMismatch { .. })
        ));

        // The complete frame is bounded by the fact bound.
        let oversize = vec![0; MAX_NEGOTIATION_FACT_BYTES + 1];
        assert!(matches!(
            NegotiationGossip::decode(&oversize),
            Err(NegotiationError::FactTooLarge { .. })
        ));
    }
}
