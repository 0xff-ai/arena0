//! The single durable owner of execution progress and terminal evidence.
//!
//! [`ExecLifecycle`](crate::ExecLifecycle) is derived from [`ExecutionStatus`]
//! for public projections. The persisted aggregate stores this enum only; it
//! does not maintain independent lifecycle, pending, proof, or terminal-cause
//! fields that could drift into an invalid cross-product.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::TraceEntry;
use crate::bounded::read_string as read_bounded_string;
use crate::exec::ExecLifecycle;
use crate::trace::{StepCommitment, TerminalCommitment};

use super::{
    AbortKind, AbortOccurrence, ExecutionBinding, MAX_PROOF_SIGNATURES, MAX_TERMINAL_REASON_BYTES,
    ParticipantTerminalSignature, ProtocolError, ReceiptId, StepCursor, TerminalCertificate,
    TerminalOutcome, ensure_payload, validate_terminal_progress,
};

/// The one persisted owner of execution lifecycle progress,
/// terminal proof progress, and terminal causes/proof identities.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ExecutionStatus {
    /// Activation is committed but the host has not injected SessionStarted.
    Activating,
    /// The execution is runnable. Any open callout is stored separately with
    /// the resulting state image, not inside this status.
    Active,
    /// Successful terminal proof collection is in progress.
    TerminalProof { proof: Box<TerminalProof> },
    /// A complete terminal proof and canonical receipt were published.
    Completed { proof: PublishedProof },
    /// An authenticated or shared stop awaits local artifact publication.
    Stopped { cause: StopCause },
    /// A canonical shared-stop receipt or unilateral report was published.
    StoppedPublished {
        cause: StopCause,

        receipt_id: ReceiptId,
    },
    /// Terminal proof work was interrupted before publication.
    Incomplete {
        proof: Box<TerminalProof>,
        reason: String,
    },
}

/// Remaining terminal work derived from the authoritative durable status.
/// This view is never persisted and grants no permission to mutate frozen proof.
#[derive(Debug, Clone, Copy)]
pub enum ReceiptWork {
    /// Execution has not reached a terminal boundary.
    NotTerminal,
    /// The N-of-N terminal agreement is still incomplete.
    CollectSignatures,
    /// Complete terminal evidence can be assembled into a local receipt body.
    Assemble,
    /// A locally produced artifact has already been persisted.
    Published,
    /// Interrupted proof is frozen and must not resume or publish.
    Incomplete,
}

/// Terminal proof progress owned by [`ExecutionStatus`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum TerminalProof {
    /// Signatures are being collected for the terminal commitment.
    Pending {
        commitment: TerminalCommitment,
        outcome: TerminalOutcome,
        signatures: Vec<ParticipantTerminalSignature>,
    },
    /// The terminal commitment has a complete participant certificate.
    Certified {
        certificate: TerminalCertificate,
        outcome: TerminalOutcome,
    },
}

/// Completion evidence and its single portable artifact identity.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PublishedProof {
    pub(crate) certificate: TerminalCertificate,
    pub(crate) outcome: TerminalOutcome,

    pub(crate) receipt_id: ReceiptId,
}

/// The one terminal-cause type used by [`ExecutionStatus`]. Local versus peer
/// authorship is derived from an authenticated occurrence's sender and the
/// execution producer; it is not a second persisted status dimension.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum StopCause {
    /// A signed local or peer abort/failure occurrence.
    Authenticated(AbortOccurrence),
    /// A certified guest-authored terminal effect.
    Shared {
        kind: AbortKind,
        commitment: StepCommitment,
        reason: String,
    },
}

impl BorshSerialize for ExecutionStatus {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        match self {
            Self::Activating => BorshSerialize::serialize(&0u8, writer),
            Self::Active => BorshSerialize::serialize(&1u8, writer),
            Self::TerminalProof { proof } => {
                BorshSerialize::serialize(&2u8, writer)?;
                BorshSerialize::serialize(proof, writer)
            }
            Self::Completed { proof } => {
                BorshSerialize::serialize(&3u8, writer)?;
                BorshSerialize::serialize(proof, writer)
            }
            Self::Stopped { cause } => {
                BorshSerialize::serialize(&4u8, writer)?;
                BorshSerialize::serialize(cause, writer)
            }
            Self::Incomplete { proof, reason } => {
                if reason.len() > MAX_TERMINAL_REASON_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "incomplete reason exceeds bound",
                    ));
                }
                BorshSerialize::serialize(&5u8, writer)?;
                BorshSerialize::serialize(proof, writer)?;
                BorshSerialize::serialize(reason, writer)
            }
            Self::StoppedPublished { cause, receipt_id } => {
                BorshSerialize::serialize(&6u8, writer)?;
                BorshSerialize::serialize(cause, writer)?;
                BorshSerialize::serialize(receipt_id, writer)
            }
        }
    }
}

impl BorshDeserialize for ExecutionStatus {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => Ok(Self::Activating),
            1 => Ok(Self::Active),
            2 => Ok(Self::TerminalProof {
                proof: Box::<TerminalProof>::deserialize_reader(reader)?,
            }),
            3 => Ok(Self::Completed {
                proof: PublishedProof::deserialize_reader(reader)?,
            }),
            4 => Ok(Self::Stopped {
                cause: StopCause::deserialize_reader(reader)?,
            }),
            5 => Ok(Self::Incomplete {
                proof: Box::<TerminalProof>::deserialize_reader(reader)?,
                reason: read_bounded_string(
                    reader,
                    MAX_TERMINAL_REASON_BYTES,
                    "incomplete reason",
                )?,
            }),
            6 => Ok(Self::StoppedPublished {
                cause: StopCause::deserialize_reader(reader)?,

                receipt_id: ReceiptId::deserialize_reader(reader)?,
            }),
            tag => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown execution status tag {tag}"),
            )),
        }
    }
}

impl BorshSerialize for TerminalProof {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        match self {
            Self::Pending {
                commitment,
                outcome,
                signatures,
            } => {
                BorshSerialize::serialize(&0u8, writer)?;
                BorshSerialize::serialize(commitment, writer)?;
                BorshSerialize::serialize(outcome, writer)?;
                BorshSerialize::serialize(signatures, writer)
            }
            Self::Certified {
                certificate,
                outcome,
            } => {
                BorshSerialize::serialize(&1u8, writer)?;
                BorshSerialize::serialize(certificate, writer)?;
                BorshSerialize::serialize(outcome, writer)
            }
        }
    }
}

impl BorshDeserialize for TerminalProof {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => Ok(Self::Pending {
                commitment: TerminalCommitment::deserialize_reader(reader)?,
                outcome: TerminalOutcome::deserialize_reader(reader)?,
                signatures: read_bounded_signatures(reader)?,
            }),
            1 => Ok(Self::Certified {
                certificate: TerminalCertificate::deserialize_reader(reader)?,
                outcome: TerminalOutcome::deserialize_reader(reader)?,
            }),
            tag => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown terminal proof tag {tag}"),
            )),
        }
    }
}

impl BorshSerialize for StopCause {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        match self {
            Self::Authenticated(occurrence) => {
                BorshSerialize::serialize(&0u8, writer)?;
                BorshSerialize::serialize(occurrence, writer)
            }
            Self::Shared {
                kind,
                commitment,
                reason,
            } => {
                if reason.len() > MAX_TERMINAL_REASON_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "terminal reason exceeds bound",
                    ));
                }
                BorshSerialize::serialize(&1u8, writer)?;
                BorshSerialize::serialize(kind, writer)?;
                BorshSerialize::serialize(commitment, writer)?;
                BorshSerialize::serialize(reason, writer)
            }
        }
    }
}

impl BorshDeserialize for StopCause {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => Ok(Self::Authenticated(AbortOccurrence::deserialize_reader(
                reader,
            )?)),
            1 => Ok(Self::Shared {
                kind: AbortKind::deserialize_reader(reader)?,
                commitment: StepCommitment::deserialize_reader(reader)?,
                reason: read_bounded_string(reader, MAX_TERMINAL_REASON_BYTES, "terminal reason")?,
            }),
            tag => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown stop cause tag {tag}"),
            )),
        }
    }
}

fn read_bounded_signatures<R: borsh::io::Read>(
    reader: &mut R,
) -> std::io::Result<Vec<ParticipantTerminalSignature>> {
    let count = u32::deserialize_reader(reader)? as usize;
    if count > MAX_PROOF_SIGNATURES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "terminal signature count exceeds bound",
        ));
    }
    let mut signatures = Vec::with_capacity(count);
    for _ in 0..count {
        signatures.push(ParticipantTerminalSignature::deserialize_reader(reader)?);
    }
    Ok(signatures)
}

impl TerminalProof {
    pub fn pending(
        commitment: TerminalCommitment,
        outcome: TerminalOutcome,
        signatures: Vec<ParticipantTerminalSignature>,
    ) -> Self {
        Self::Pending {
            commitment,
            outcome,
            signatures,
        }
    }

    pub fn certified(certificate: TerminalCertificate, outcome: TerminalOutcome) -> Self {
        Self::Certified {
            certificate,
            outcome,
        }
    }

    /// Borrow pending terminal evidence and its collected signatures.
    #[must_use]
    pub fn pending_parts(
        &self,
    ) -> Option<(
        &TerminalCommitment,
        &TerminalOutcome,
        &[ParticipantTerminalSignature],
    )> {
        match self {
            Self::Pending {
                commitment,
                outcome,
                signatures,
            } => Some((commitment, outcome, signatures)),
            Self::Certified { .. } => None,
        }
    }

    /// Borrow a complete terminal certificate and its outcome.
    #[must_use]
    pub fn certified_parts(&self) -> Option<(&TerminalCertificate, &TerminalOutcome)> {
        match self {
            Self::Pending { .. } => None,
            Self::Certified {
                certificate,
                outcome,
            } => Some((certificate, outcome)),
        }
    }
}

impl StopCause {
    /// Borrow the authenticated or shared terminal reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::Authenticated(occurrence) => occurrence.reason(),
            Self::Shared { reason, .. } => reason,
        }
    }

    /// Return the terminal kind when this cause is an abort/failure cause.
    #[must_use]
    pub const fn kind(&self) -> AbortKind {
        match self {
            Self::Authenticated(occurrence) => occurrence.kind(),
            Self::Shared { kind, .. } => *kind,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Authenticated(occurrence) => occurrence.validate_shape(),
            Self::Shared {
                kind,
                commitment,
                reason,
            } => {
                if (*kind != AbortKind::Abort && *kind != AbortKind::Fail)
                    || commitment.domain != crate::STEP_COMMIT_DOMAIN
                {
                    return Err(ProtocolError::InvalidTerminalStatus);
                }
                ensure_reason(reason)
            }
        }
    }
}

impl ExecutionStatus {
    /// Derive the next receipt operation without reconstructing optional facts.
    #[must_use]
    pub fn receipt_work(&self) -> ReceiptWork {
        match self {
            Self::Activating | Self::Active => ReceiptWork::NotTerminal,
            Self::Stopped { .. } => ReceiptWork::Assemble,
            Self::Completed { .. } | Self::StoppedPublished { .. } => ReceiptWork::Published,
            Self::Incomplete { .. } => ReceiptWork::Incomplete,
            Self::TerminalProof { proof } => match proof.as_ref() {
                TerminalProof::Pending { .. } => ReceiptWork::CollectSignatures,
                TerminalProof::Certified { .. } => ReceiptWork::Assemble,
            },
        }
    }

    /// Derive the public lifecycle projection. This value is never persisted
    /// in an execution aggregate.
    #[must_use]
    pub const fn lifecycle(&self) -> ExecLifecycle {
        match self {
            Self::Activating => ExecLifecycle::Activating,
            Self::Active => ExecLifecycle::Active,
            Self::TerminalProof { .. } => ExecLifecycle::Active,
            Self::Completed { .. } => ExecLifecycle::Completed,
            Self::Stopped { cause } => match cause.kind() {
                AbortKind::Fail => ExecLifecycle::Failed,
                AbortKind::Abort => ExecLifecycle::Aborted,
            },
            Self::StoppedPublished { cause, .. } => match cause.kind() {
                AbortKind::Fail => ExecLifecycle::Failed,
                AbortKind::Abort => ExecLifecycle::Aborted,
            },
            Self::Incomplete { .. } => ExecLifecycle::Incomplete,
        }
    }

    /// Whether this status can accept a local or peer execution input.
    #[must_use]
    pub const fn is_runnable(&self) -> bool {
        matches!(self, Self::Active | Self::TerminalProof { .. })
    }

    /// Whether this status is terminal and cannot accept further progress.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. }
                | Self::Stopped { .. }
                | Self::StoppedPublished { .. }
                | Self::Incomplete { .. }
        )
    }

    /// Borrow terminal proof progress, including incomplete proof evidence.
    #[must_use]
    pub(crate) fn terminal_proof(&self) -> Option<&TerminalProof> {
        match self {
            Self::TerminalProof { proof } | Self::Incomplete { proof, .. } => Some(proof),
            Self::Completed { .. }
            | Self::Activating
            | Self::Active
            | Self::Stopped { .. }
            | Self::StoppedPublished { .. } => None,
        }
    }

    /// Borrow the completed proof material, if publication succeeded.
    #[must_use]
    pub(crate) const fn published_proof(&self) -> Option<&PublishedProof> {
        match self {
            Self::Completed { proof } => Some(proof),
            _ => None,
        }
    }

    /// Return the terminal cause, if this status has one.
    #[must_use]
    pub const fn terminal_cause(&self) -> Option<&StopCause> {
        match self {
            Self::Stopped { cause } | Self::StoppedPublished { cause, .. } => Some(cause),
            _ => None,
        }
    }

    pub const fn active() -> Self {
        Self::Active
    }

    pub fn from_terminal_proof(proof: TerminalProof) -> Self {
        Self::TerminalProof {
            proof: Box::new(proof),
        }
    }

    pub fn completed(
        certificate: TerminalCertificate,
        outcome: TerminalOutcome,

        receipt_id: ReceiptId,
    ) -> Self {
        Self::Completed {
            proof: PublishedProof {
                certificate,
                outcome,

                receipt_id,
            },
        }
    }

    pub fn stopped_published(cause: StopCause, receipt_id: ReceiptId) -> Self {
        Self::StoppedPublished { cause, receipt_id }
    }

    /// Construct a status from an authenticated abort/failure occurrence.
    pub fn stopped(occurrence: AbortOccurrence) -> Result<Self, ProtocolError> {
        occurrence.validate_shape()?;
        Ok(Self::Stopped {
            cause: StopCause::Authenticated(occurrence),
        })
    }

    /// Construct an incomplete status retaining terminal proof evidence.
    pub fn incomplete(proof: TerminalProof, reason: String) -> Result<Self, ProtocolError> {
        ensure_reason(&reason)?;
        Ok(Self::Incomplete {
            proof: Box::new(proof),
            reason,
        })
    }

    /// Add one checked participant signature to pending terminal proof.
    ///
    /// Returns `true` when the added signature completed the N-of-N
    /// certificate and promoted this status to `Certified` proof progress.
    /// The enclosing [`ExecutionState`](super::ExecutionState) owns the
    /// durable version and should persist this mutation atomically.
    pub fn add_terminal_signature(
        &mut self,
        binding: &ExecutionBinding,
        signature: ParticipantTerminalSignature,
    ) -> Result<bool, ProtocolError> {
        let (commitment, outcome, signatures) = match self.terminal_proof() {
            Some(TerminalProof::Pending {
                commitment,
                outcome,
                signatures,
            }) => (commitment.clone(), outcome.clone(), signatures.to_vec()),
            Some(TerminalProof::Certified { .. }) => {
                return Err(ProtocolError::TerminalAlreadyPublished);
            }
            None => return Err(ProtocolError::TerminalProofMissing),
        };

        let participant = signature.participant();
        if let Some(existing) = signatures
            .iter()
            .find(|existing| existing.participant() == participant)
        {
            if existing.signature() == signature.signature() {
                return Err(ProtocolError::DuplicateTerminalSignature { participant });
            }
            return Err(ProtocolError::ConflictingTerminalSignature { participant });
        }
        let key = binding.participant_key(&participant)?;
        let valid = key
            .verify(&commitment.signing_bytes(), &signature.signature())
            .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
        if !valid {
            return Err(ProtocolError::InvalidTerminalSignature { participant });
        }
        if signatures.len() >= MAX_PROOF_SIGNATURES {
            return Err(ProtocolError::CollectionTooLarge {
                kind: "terminal signatures",
                actual: signatures.len() + 1,
                max: MAX_PROOF_SIGNATURES,
            });
        }

        let mut signatures = signatures;
        signatures.push(signature);
        signatures.sort_by_key(ParticipantTerminalSignature::participant);
        if signatures.len() == binding.activation().tickets().len() {
            let certificate =
                TerminalCertificate::from_signatures(binding, &commitment, &signatures)?;
            *self = Self::from_terminal_proof(TerminalProof::certified(certificate, outcome));
            Ok(true)
        } else {
            *self =
                Self::from_terminal_proof(TerminalProof::pending(commitment, outcome, signatures));
            Ok(false)
        }
    }

    /// Construct the status represented by one certified shared termination
    /// entry. Successful `SessionEnd` entries begin proof progress and are
    /// therefore not terminal status until the canonical receipt is published.
    pub fn from_shared_entry(
        entry: &TraceEntry,
        commitment: StepCommitment,
    ) -> Result<Option<Self>, ProtocolError> {
        let Some(reason) = entry.abort_reason() else {
            return Ok(None);
        };
        ensure_reason(reason)?;
        let kind = match entry.terminal.as_ref() {
            Some(crate::Effect::SessionAbort { .. }) => AbortKind::Abort,
            Some(crate::Effect::Fail { .. }) => AbortKind::Fail,
            _ => return Ok(None),
        };
        Ok(Some(Self::Stopped {
            cause: StopCause::Shared {
                kind,
                commitment,
                reason: reason.to_owned(),
            },
        }))
    }

    /// Validate the complete status against the aggregate binding and agreed
    /// step cursor.
    pub(crate) fn validate_binding(
        &self,
        binding: &ExecutionBinding,
        agreed: StepCursor,
    ) -> Result<(), ProtocolError> {
        match self {
            Self::Activating | Self::Active => Ok(()),
            Self::TerminalProof { proof } => validate_terminal_progress(binding, agreed, proof),
            Self::Completed { proof } => {
                let progress = TerminalProof::Certified {
                    certificate: proof.certificate.clone(),
                    outcome: proof.outcome.clone(),
                };
                validate_terminal_progress(binding, agreed, &progress)?;
                if proof.receipt_id == ReceiptId::from_bytes([0; 32]) {
                    return Err(ProtocolError::InvalidTerminalStatus);
                }
                Ok(())
            }
            Self::Stopped { cause } => {
                cause.validate()?;
                validate_cause_binding(cause, binding, agreed)
            }
            Self::StoppedPublished { cause, receipt_id } => {
                cause.validate()?;
                validate_cause_binding(cause, binding, agreed)?;
                if *receipt_id == ReceiptId::from_bytes([0; 32]) {
                    return Err(ProtocolError::InvalidTerminalStatus);
                }
                Ok(())
            }
            Self::Incomplete { proof, reason } => {
                ensure_reason(reason)?;
                validate_terminal_progress(binding, agreed, proof)?;
                Ok(())
            }
        }
    }
}

pub(crate) fn validate_cause_binding(
    cause: &StopCause,
    binding: &ExecutionBinding,
    agreed: StepCursor,
) -> Result<(), ProtocolError> {
    match cause {
        StopCause::Authenticated(occurrence) => {
            if occurrence.session_id() != binding.session_id()
                || *occurrence.coordinate() != agreed
                || !binding
                    .participant_keys()?
                    .into_iter()
                    .any(|(participant, _)| participant == occurrence.sender())
                || !occurrence
                    .verify_signature()
                    .map_err(|_| ProtocolError::InvalidTerminalStatus)?
            {
                return Err(ProtocolError::InvalidTerminalStatus);
            }
        }
        StopCause::Shared { commitment, .. } => {
            if commitment.domain != crate::STEP_COMMIT_DOMAIN
                || commitment.session_id != binding.session_id()
                || commitment.step.checked_add(1) != Some(agreed.next_step())
                || commitment.post_state != agreed.state_hash()
                || commitment.link_hash() != agreed.chain_hash()
            {
                return Err(ProtocolError::InvalidTerminalStatus);
            }
        }
    }
    Ok(())
}

fn ensure_reason(reason: &str) -> Result<(), ProtocolError> {
    ensure_payload(
        "terminal reason",
        reason.len(),
        super::MAX_TERMINAL_REASON_BYTES,
    )
}
