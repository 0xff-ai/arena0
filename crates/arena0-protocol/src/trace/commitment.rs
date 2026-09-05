//! Step and terminal commitments: the byte-identical messages participants
//! sign each step and at session completion, and the BLS aggregate agreements
//! recorded over them.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use arena0_crypto::{BlsPublicKey, BlsSignature};

use crate::{OutcomeHash, SessionHash, StateHash};

use super::entry::TraceEntry;

/// Domain separation tag for the shared per-entry commitment participants sign.
pub const STEP_COMMIT_DOMAIN: [u8; 24] = *b"arena0/step-commit/v2\0\0\0";

/// Domain separation tag for the signed terminal boundary participants sign at
/// session completion.
pub const TERMINAL_DOMAIN: [u8; 24] = *b"arena0/terminal/v1\0\0\0\0\0\0";
const _: () = assert!(TERMINAL_DOMAIN.len() == 24);

/// The chain link at the start of the public section: position 0 links to
/// zeros. The activation boundary attestation is decoupled from trace positions,
/// so the link chain deliberately does not weld it in.
pub const CHAIN_START: [u8; 32] = [0u8; 32];

/// One participant's BLS signature over a public entry's commitment.
///
/// This is protocol evidence rather than a transport representation. Wire
/// execution frames carry it, but the signature's invariant is owned by the
/// trace and agreement model.
#[derive(
    Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
pub struct StepSig {
    /// The canonical public position this signature covers.
    pub step: u64,
    /// The BLS signature over that position's entry commitment.
    pub sig: BlsSignature,
}

/// The shared per-entry message every participant signs. Byte-identical across
/// participants and bound to the public entry itself: the canonical position,
/// the entry content hash (event bytes, effects, witness; fuel is a
/// per-node measurement and is excluded), the pre/post
/// shared state hashes, and the chain link to the previous position's
/// commitment. Signatures are therefore never interchangeable tokens: a
/// signature names exactly one entry at exactly one position in exactly one
/// chain. BLS signatures over it aggregate into one constant-size signature; a
/// participant that diverges signs a different commitment and cannot appear in
/// the honest aggregate.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct StepCommitment {
    /// Domain separation tag ([`STEP_COMMIT_DOMAIN`]).
    pub domain: [u8; 24],
    /// Session this entry belongs to.
    pub session_id: SessionHash,
    /// Canonical public position of the entry.
    pub step: u64,
    /// blake3 of the entry's canonical bytes ([`TraceEntry::entry_hash`]).
    pub entry_hash: [u8; 32],
    /// Shared state hash before the entry.
    pub pre_state: StateHash,
    /// Shared state hash after the entry.
    pub post_state: StateHash,
    /// blake3 of the previous position's commitment signing bytes
    /// ([`StepCommitment::link_hash`]); [`CHAIN_START`] at position 0.
    pub link: [u8; 32],
}

impl StepCommitment {
    /// Build the commitment for one public entry, given the previous
    /// position's chain link ([`CHAIN_START`] at position 0).
    #[must_use]
    pub fn for_entry(session_id: SessionHash, entry: &TraceEntry, link: [u8; 32]) -> Self {
        Self {
            domain: STEP_COMMIT_DOMAIN,
            session_id,
            step: entry.step,
            entry_hash: entry.entry_hash(),
            pre_state: entry.pre_state,
            post_state: entry.post_state,
            link,
        }
    }

    /// Canonical bytes participants sign (BLS hashes these to G1).
    #[must_use]
    pub fn signing_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("StepCommitment is always serializable")
    }

    /// The chain link the next position's commitment carries: blake3 of this
    /// commitment's signing bytes.
    #[must_use]
    pub fn link_hash(&self) -> [u8; 32] {
        *blake3::hash(&self.signing_bytes()).as_bytes()
    }
}

/// The signed terminal boundary every participant co-signs at session completion.
/// The end boundary paired with [`Activation`](crate::Activation): a full-bitmap aggregate over
/// this message binds the final step, the final shared state, and the blake3 of the
/// outcome bytes, so a truncated or relabeled terminal cannot pass. Byte-identical
/// across participants (they agreed on the final state and outcome), so signatures
/// aggregate the same way step commitments do.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct TerminalCommitment {
    /// Domain separation tag ([`TERMINAL_DOMAIN`]).
    pub domain: [u8; 24],
    /// Session this terminal belongs to.
    pub session_id: SessionHash,
    /// Index of the final step (the entry carrying [`Effect::SessionEnd`](crate::Effect::SessionEnd)).
    pub final_step: u64,
    /// Shared state hash at completion (the final entry's `post_state`).
    pub final_state: StateHash,
    /// `blake3` of the borsh-encoded outcome bytes.
    pub outcome_hash: OutcomeHash,
}

impl TerminalCommitment {
    /// Build the terminal commitment for a completed session.
    #[must_use]
    pub fn new(
        session_id: SessionHash,
        final_step: u64,
        final_state: StateHash,
        outcome_hash: OutcomeHash,
    ) -> Self {
        Self {
            domain: TERMINAL_DOMAIN,
            session_id,
            final_step,
            final_state,
            outcome_hash,
        }
    }

    /// Canonical bytes participants sign (BLS hashes these to G1).
    #[must_use]
    pub fn signing_bytes(&self) -> Vec<u8> {
        borsh::to_vec(self).expect("TerminalCommitment is always serializable")
    }
}

/// The recorded terminal in a [`SessionHeader`](crate::SessionHeader): the completed session's final
/// step, final state, outcome hash, and the full N-of-N aggregate over the
/// [`TerminalCommitment`] that certifies them. Present iff the session completed
/// (an [`Effect::SessionEnd`](crate::Effect::SessionEnd)).
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionTerminal {
    /// Index of the final step.
    pub final_step: u64,
    /// Shared state hash at completion.
    pub final_state: StateHash,
    /// `blake3` of the borsh-encoded outcome bytes.
    pub outcome_hash: OutcomeHash,
    /// The N-of-N aggregate over the [`TerminalCommitment`], with a full bitmap.
    pub agreement: AggregateAttestation,
}

/// A bitmap over committed participant indices (`ceil(N/8)` bytes), marking which
/// participants are in the aggregate at a step.
#[derive(
    Serialize,
    Deserialize,
    BorshSerialize,
    BorshDeserialize,
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    Hash,
)]
pub struct SignerSet(pub Vec<u8>);

impl SignerSet {
    /// An empty signer set (no bytes; no bits set).
    #[must_use]
    pub fn empty() -> Self {
        Self(Vec::new())
    }

    /// A signer set sized for `n` participants, all unset.
    #[must_use]
    pub fn with_capacity(n: usize) -> Self {
        Self(vec![0u8; n.div_ceil(8)])
    }

    /// Validate the bitmap against an explicit participant count.
    ///
    /// The count is not stored in the bitmap. Callers must provide the
    /// committed participant count whenever they consume a signer set so that
    /// trailing bytes and bits cannot select an uncommitted participant.
    pub fn validate(&self, participant_count: usize) -> Result<(), String> {
        let max = crate::negotiation::MAX_PARTICIPANTS;
        if participant_count > max {
            return Err(format!(
                "participant count {participant_count} exceeds maximum {max}"
            ));
        }

        let expected_bytes = participant_count.div_ceil(8);
        if self.0.len() > expected_bytes {
            return Err(format!(
                "signer bitmap has extra bytes: got {}, expected {expected_bytes}",
                self.0.len()
            ));
        }
        if self.0.len() < expected_bytes {
            return Err(format!(
                "signer bitmap has wrong byte length: got {}, expected {expected_bytes}",
                self.0.len()
            ));
        }

        let used_bits = participant_count % 8;
        if used_bits != 0 {
            let unused_mask = !((1u8 << used_bits) - 1);
            let last = self.0[expected_bytes - 1];
            if last & unused_mask != 0 {
                return Err(format!(
                    "signer bitmap sets unused high bits in final byte: {last:#04x}"
                ));
            }
        }
        Ok(())
    }

    /// Build a bitmap from participant indices after checking each index.
    pub fn from_indices(participant_count: usize, indices: &[usize]) -> Result<Self, String> {
        if participant_count > crate::negotiation::MAX_PARTICIPANTS {
            return Err(format!(
                "participant count {participant_count} exceeds maximum {}",
                crate::negotiation::MAX_PARTICIPANTS
            ));
        }
        let mut signers = Self::with_capacity(participant_count);
        for &index in indices {
            if index >= participant_count {
                return Err(format!(
                    "signer index {index} is outside participant count {participant_count}"
                ));
            }
            signers.0[index / 8] |= 1 << (index % 8);
        }
        Ok(signers)
    }

    /// Build a bitmap with every participant index set.
    pub fn full(participant_count: usize) -> Result<Self, String> {
        if participant_count > crate::negotiation::MAX_PARTICIPANTS {
            return Err(format!(
                "participant count {participant_count} exceeds maximum {}",
                crate::negotiation::MAX_PARTICIPANTS
            ));
        }
        let mut signers = Self(vec![u8::MAX; participant_count.div_ceil(8)]);
        if let Some(last) = signers.0.last_mut() {
            let used_bits = participant_count % 8;
            if used_bits != 0 {
                *last &= (1u8 << used_bits) - 1;
            }
        }
        Ok(signers)
    }

    /// Mark participant `i` as a signer, growing the bitmap if needed.
    pub fn set(&mut self, i: usize) {
        let byte = i / 8;
        if byte >= self.0.len() {
            self.0.resize(byte + 1, 0);
        }
        self.0[byte] |= 1 << (i % 8);
    }

    /// Whether participant `i` is a signer.
    #[must_use]
    pub fn contains(&self, i: usize) -> bool {
        let byte = i / 8;
        byte < self.0.len() && self.0[byte] & (1 << (i % 8)) != 0
    }

    /// Number of signers set.
    #[must_use]
    pub fn count(&self) -> usize {
        self.0.iter().map(|b| b.count_ones() as usize).sum()
    }

    /// Whether all `n` participants (indices `0..n`) are present.
    #[must_use]
    pub fn is_full(&self, n: usize) -> bool {
        (0..n).all(|i| self.contains(i))
    }
}

/// The recorded agreement at one step: the BLS aggregate over the signing participants
/// plus a bitmap of who signed. Computed and stored locally; never re-emitted to
/// the network. Replaces the per-co-participant `Vec<Agreement>`.
#[derive(Serialize, Deserialize, BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct AggregateAttestation {
    /// Aggregate of the signers' `StepCommitment` signatures. The separate
    /// activation aggregate signs the `ActivationData` instead.
    pub aggregate: BlsSignature,
    /// Which committed participants are in `aggregate`.
    pub signers: SignerSet,
}

impl AggregateAttestation {
    /// An empty agreement (zero aggregate, no signers).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            aggregate: BlsSignature([0u8; 48]),
            signers: SignerSet::empty(),
        }
    }

    /// Build an agreement from the ordered signatures and their participant
    /// bitmap. An empty signature list produces the canonical empty agreement.
    pub fn from_signatures(
        signers: SignerSet,
        signatures: &[BlsSignature],
    ) -> Result<Self, arena0_crypto::CryptoError> {
        if signatures.is_empty() {
            return Ok(Self::empty());
        }
        Ok(Self {
            aggregate: BlsSignature::aggregate(signatures)?,
            signers,
        })
    }

    /// The subset of `participants` (participant-ordered) that are in this aggregate.
    #[must_use]
    pub fn selected_participants(&self, participants: &[BlsPublicKey]) -> Vec<BlsPublicKey> {
        participants
            .iter()
            .enumerate()
            .filter(|(i, _)| self.signers.contains(*i))
            .map(|(_, k)| *k)
            .collect()
    }

    /// Verify this recorded agreement: the aggregate must be the same-message BLS
    /// aggregate of the bitmapped participants' signatures over `message`.
    ///
    /// `participants` are the committed participant keys in participant order (the order the
    /// bitmap indexes). `message` is [`StepCommitment::signing_bytes`] for a step, or
    /// [`ActivationData::signing_bytes`](crate::ActivationData::signing_bytes)
    /// for the separate start boundary. The consensus gate (a full bitmap at
    /// consensus-critical steps) is the verifier's concern; this checks the
    /// cryptography only. Host-only.
    pub fn verify_signatures(
        &self,
        step: u64,
        message: &[u8],
        participants: &[BlsPublicKey],
    ) -> Result<(), AttestationError> {
        self.signers
            .validate(participants.len())
            .map_err(|reason| AttestationError::InvalidSignerSet { step, reason })?;
        let signers = self.selected_participants(participants);
        if signers.is_empty() {
            return Err(AttestationError::NoSigners { step });
        }
        let ok = self
            .aggregate
            .fast_aggregate_verify(message, &signers)
            .map_err(|err| AttestationError::Crypto(err.to_string()))?;
        if !ok {
            return Err(AttestationError::BadAggregate { step });
        }
        Ok(())
    }
}

/// Failure verifying a recorded [`AggregateAttestation`] aggregate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttestationError {
    /// The signer bitmap does not match the committed participant count.
    #[error("invalid signer bitmap at step {step}: {reason}")]
    InvalidSignerSet {
        /// The step whose agreement carried the malformed bitmap.
        step: u64,
        /// Why the bitmap failed its count-aware validation.
        reason: String,
    },
    /// The signer bitmap selected no participants.
    #[error("agreement has no signers at step {step}")]
    NoSigners {
        /// The step whose agreement had no signers.
        step: u64,
    },
    /// The BLS aggregate did not verify against the bitmapped participant keys.
    #[error("agreement aggregate did not verify at step {step}")]
    BadAggregate {
        /// The step whose aggregate failed verification.
        step: u64,
    },
    /// A crypto call failed.
    #[error("crypto error: {0}")]
    Crypto(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: u8) -> StateHash {
        StateHash([byte; 32])
    }

    fn entry(step: u64, pre: StateHash, post: StateHash) -> TraceEntry {
        TraceEntry {
            trace_version: crate::TRACE_FORMAT_VERSION,
            step,
            event: crate::PublicEvent::MessageReceived {
                message_id: crate::MessageId([step as u8; 32]),
                from: crate::PeerId([1; 32]),
                position: step,
                pre_state: pre,
                msg: Vec::new(),
            },
            effects: Vec::new(),
            pre_state: pre,
            post_state: post,
            fuel_used: 0,
            witness: None,
            agreement: AggregateAttestation::empty(),
        }
    }

    #[test]
    fn step_commitment_signing_bytes_roundtrip() {
        let commit = StepCommitment::for_entry(
            SessionHash([2u8; 32]),
            &entry(4, hash(5), hash(6)),
            CHAIN_START,
        );
        let bytes = commit.signing_bytes();
        let decoded: StepCommitment = borsh::from_slice(&bytes).unwrap();
        assert_eq!(commit, decoded);
        assert_eq!(commit.domain, STEP_COMMIT_DOMAIN);
        assert_eq!(commit.link, CHAIN_START);
    }

    #[test]
    fn step_commitment_chain_links_bind_position_and_content() {
        let session = SessionHash([2u8; 32]);
        let first = StepCommitment::for_entry(session, &entry(0, hash(0), hash(1)), CHAIN_START);
        let second =
            StepCommitment::for_entry(session, &entry(1, hash(1), hash(2)), first.link_hash());
        assert_eq!(second.link, first.link_hash());

        // The same (pre, post) pair at a different position or behind a
        // different link signs different bytes: no interchangeable tokens.
        let elsewhere =
            StepCommitment::for_entry(session, &entry(1, hash(1), hash(2)), CHAIN_START);
        assert_ne!(second.signing_bytes(), elsewhere.signing_bytes());
    }

    #[test]
    fn signer_set_set_contains_full() {
        let mut s = SignerSet::with_capacity(3);
        assert!(!s.is_full(3));
        s.set(0);
        s.set(2);
        assert!(s.contains(0));
        assert!(!s.contains(1));
        assert!(s.contains(2));
        assert_eq!(s.count(), 2);
        assert!(!s.is_full(3));
        s.set(1);
        assert!(s.is_full(3));
    }

    #[test]
    fn signer_set_validation_rejects_malformed_bitmaps() {
        assert!(SignerSet::empty().validate(3).is_err());
        assert!(SignerSet(vec![0, 0]).validate(3).is_err());
        assert!(SignerSet(vec![0x08]).validate(3).is_err());
        assert!(
            SignerSet::empty()
                .validate(crate::negotiation::MAX_PARTICIPANTS + 1)
                .is_err()
        );
        assert!(SignerSet(vec![0x05]).validate(3).is_ok());
    }

    #[test]
    fn signer_set_checked_constructors_reject_out_of_range_indices() {
        assert_eq!(
            SignerSet::from_indices(3, &[0, 2]).expect("valid indices"),
            SignerSet(vec![0x05])
        );
        assert!(SignerSet::from_indices(3, &[3]).is_err());
        assert!(SignerSet::from_indices(crate::negotiation::MAX_PARTICIPANTS + 1, &[]).is_err());
        assert_eq!(SignerSet::full(3).expect("full set"), SignerSet(vec![0x07]));
        assert!(SignerSet::full(crate::negotiation::MAX_PARTICIPANTS + 1).is_err());
    }

    #[test]
    fn aggregate_from_signatures_keeps_empty_agreement_canonical() {
        let signers = SignerSet::with_capacity(2);
        assert_eq!(
            AggregateAttestation::from_signatures(signers, &[]).expect("empty aggregate"),
            AggregateAttestation::empty()
        );
    }

    #[test]
    fn verify_attestation_rejects_malformed_bitmap_before_crypto() {
        let agreement = AggregateAttestation {
            aggregate: BlsSignature([0; 48]),
            signers: SignerSet(vec![0x08]),
        };
        let error = agreement
            .verify_signatures(
                7,
                b"message",
                &[BlsPublicKey([0; 96]), BlsPublicKey([1; 96])],
            )
            .expect_err("unused signer bit must be rejected");
        assert!(matches!(
            error,
            AttestationError::InvalidSignerSet { step: 7, .. }
        ));
    }

    #[test]
    fn step_agreement_selects_bitmapped_participants() {
        let participants = vec![
            BlsPublicKey([1u8; 96]),
            BlsPublicKey([2u8; 96]),
            BlsPublicKey([3u8; 96]),
        ];
        let mut signers = SignerSet::with_capacity(3);
        signers.set(0);
        signers.set(2);
        let agreement = AggregateAttestation {
            aggregate: BlsSignature([9u8; 48]),
            signers,
        };
        let selected = agreement.selected_participants(&participants);
        assert_eq!(selected, vec![participants[0], participants[2]]);
    }
}
