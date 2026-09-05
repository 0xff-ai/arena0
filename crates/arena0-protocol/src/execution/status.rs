//! The single durable owner of execution progress and terminal evidence.
//!
//! [`ExecLifecycle`](crate::ExecLifecycle) is derived from [`ExecutionStatus`]
//! for public projections. The persisted aggregate stores this enum only; it
//! does not maintain independent lifecycle, pending, proof, or terminal-cause
//! fields that could drift into an invalid cross-product.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::bounded::read_string as read_bounded_string;
use crate::exec::ExecLifecycle;
use crate::trace::{PendingRecord, StepCommitment, TerminalCommitment};
use crate::{ExecId, PeerId, TraceEntry};

use super::{
    AbortKind, AbortOccurrence, ExecutionBinding, MAX_PROOF_SIGNATURES, MAX_TERMINAL_REASON_BYTES,
    ParticipantTerminalSignature, PendingCoordinate, PrivateCursor, ProducerSealRequest, ProofId,
    ProtocolError, PublicCursor, ReceiptBody, ReceiptId, TerminalCertificate, TerminalOutcome,
    ensure_payload, validate_pending_record, validate_terminal_progress,
};

/// The one persisted owner of execution progress, waiting continuations,
/// terminal proof progress, and terminal causes/proof identities.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ExecutionStatus {
    /// Activation is committed but the host has not injected SessionStarted.
    Activating,
    /// The execution is runnable and has no stored private continuation.
    Active,
    /// A private continuation is awaiting a callout answer, signature, or
    /// timer resume. The continuation coordinate is retained with the status.
    Waiting {
        pending: PendingRecord,
        coordinate: PendingCoordinate,
    },
    /// Successful terminal proof collection is in progress.
    TerminalProof { proof: Box<TerminalProof> },
    /// A complete terminal proof and producer seal were published.
    Completed { proof: PublishedProof },
    /// An authenticated or shared abort/failure ended the execution before a
    /// producer receipt seal was available.
    Stopped { cause: StopCause },
    /// A stopped receipt was sealed and published atomically.
    StoppedPublished {
        cause: StopCause,
        proof_id: ProofId,
        receipt_id: ReceiptId,
        producer: PeerId,
    },
    /// Terminal proof work was interrupted before publication.
    Incomplete {
        proof: Box<TerminalProof>,
        reason: String,
    },
}

/// Remaining terminal work, borrowed from the authoritative durable status.
/// This view is never persisted and grants no permission to mutate frozen proof.
#[derive(Debug, Clone, Copy)]
pub enum ReceiptWork<'a> {
    /// Execution has not reached a terminal boundary.
    NotTerminal,
    /// The N-of-N terminal agreement is still incomplete.
    CollectSignatures,
    /// Complete terminal evidence can be assembled into a local receipt body.
    Assemble,
    /// The staged body needs its local producer seal.
    Seal(&'a ProducerSealRequest),
    /// A producer receipt has already been persisted.
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
    /// The receipt body is staged and the producer seal is requested.
    ReceiptAssembled {
        certificate: TerminalCertificate,
        outcome: TerminalOutcome,
        body: Box<ReceiptBody>,
        request: ProducerSealRequest,
    },
    /// A stopped receipt body is staged and the producer seal is requested.
    StoppedReceiptAssembled {
        cause: StopCause,
        body: Box<ReceiptBody>,
        request: ProducerSealRequest,
    },
}

/// The proof material retained after the producer publishes a sealed receipt.
/// Proof identities and producer identity live here exactly once; no published
/// terminal variant duplicates them inside [`TerminalProof`].
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PublishedProof {
    pub(crate) certificate: TerminalCertificate,
    pub(crate) outcome: TerminalOutcome,
    pub(crate) proof_id: ProofId,
    pub(crate) receipt_id: ReceiptId,
    pub(crate) producer: PeerId,
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
            Self::Waiting {
                pending,
                coordinate,
            } => {
                BorshSerialize::serialize(&2u8, writer)?;
                BorshSerialize::serialize(pending, writer)?;
                BorshSerialize::serialize(coordinate, writer)
            }
            Self::TerminalProof { proof } => {
                BorshSerialize::serialize(&3u8, writer)?;
                BorshSerialize::serialize(proof, writer)
            }
            Self::Completed { proof } => {
                BorshSerialize::serialize(&4u8, writer)?;
                BorshSerialize::serialize(proof, writer)
            }
            Self::Stopped { cause } => {
                BorshSerialize::serialize(&5u8, writer)?;
                BorshSerialize::serialize(cause, writer)
            }
            Self::Incomplete { proof, reason } => {
                if reason.len() > MAX_TERMINAL_REASON_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "incomplete reason exceeds bound",
                    ));
                }
                BorshSerialize::serialize(&6u8, writer)?;
                BorshSerialize::serialize(proof, writer)?;
                BorshSerialize::serialize(reason, writer)
            }
            Self::StoppedPublished {
                cause,
                proof_id,
                receipt_id,
                producer,
            } => {
                BorshSerialize::serialize(&7u8, writer)?;
                BorshSerialize::serialize(cause, writer)?;
                BorshSerialize::serialize(proof_id, writer)?;
                BorshSerialize::serialize(receipt_id, writer)?;
                BorshSerialize::serialize(producer, writer)
            }
        }
    }
}

impl BorshDeserialize for ExecutionStatus {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => Ok(Self::Activating),
            1 => Ok(Self::Active),
            2 => Ok(Self::Waiting {
                pending: PendingRecord::deserialize_reader(reader)?,
                coordinate: PendingCoordinate::deserialize_reader(reader)?,
            }),
            3 => Ok(Self::TerminalProof {
                proof: Box::<TerminalProof>::deserialize_reader(reader)?,
            }),
            4 => Ok(Self::Completed {
                proof: PublishedProof::deserialize_reader(reader)?,
            }),
            5 => Ok(Self::Stopped {
                cause: StopCause::deserialize_reader(reader)?,
            }),
            6 => Ok(Self::Incomplete {
                proof: Box::<TerminalProof>::deserialize_reader(reader)?,
                reason: read_bounded_string(
                    reader,
                    MAX_TERMINAL_REASON_BYTES,
                    "incomplete reason",
                )?,
            }),
            7 => Ok(Self::StoppedPublished {
                cause: StopCause::deserialize_reader(reader)?,
                proof_id: ProofId::deserialize_reader(reader)?,
                receipt_id: ReceiptId::deserialize_reader(reader)?,
                producer: PeerId::deserialize_reader(reader)?,
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
            Self::ReceiptAssembled {
                certificate,
                outcome,
                body,
                request,
            } => {
                BorshSerialize::serialize(&2u8, writer)?;
                BorshSerialize::serialize(certificate, writer)?;
                BorshSerialize::serialize(outcome, writer)?;
                BorshSerialize::serialize(body, writer)?;
                BorshSerialize::serialize(request, writer)
            }
            Self::StoppedReceiptAssembled {
                cause,
                body,
                request,
            } => {
                BorshSerialize::serialize(&3u8, writer)?;
                BorshSerialize::serialize(cause, writer)?;
                BorshSerialize::serialize(body, writer)?;
                BorshSerialize::serialize(request, writer)
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
            2 => Ok(Self::ReceiptAssembled {
                certificate: TerminalCertificate::deserialize_reader(reader)?,
                outcome: TerminalOutcome::deserialize_reader(reader)?,
                body: Box::<ReceiptBody>::deserialize_reader(reader)?,
                request: ProducerSealRequest::deserialize_reader(reader)?,
            }),
            3 => Ok(Self::StoppedReceiptAssembled {
                cause: StopCause::deserialize_reader(reader)?,
                body: Box::<ReceiptBody>::deserialize_reader(reader)?,
                request: ProducerSealRequest::deserialize_reader(reader)?,
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
    pub(crate) fn pending(
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

    pub(crate) fn certified(certificate: TerminalCertificate, outcome: TerminalOutcome) -> Self {
        Self::Certified {
            certificate,
            outcome,
        }
    }

    pub(crate) fn receipt_assembled(
        certificate: TerminalCertificate,
        outcome: TerminalOutcome,
        body: ReceiptBody,
        request: ProducerSealRequest,
    ) -> Self {
        Self::ReceiptAssembled {
            certificate,
            outcome,
            body: Box::new(body),
            request,
        }
    }

    pub(crate) fn stopped_receipt_assembled(
        cause: StopCause,
        body: ReceiptBody,
        request: ProducerSealRequest,
    ) -> Self {
        Self::StoppedReceiptAssembled {
            cause,
            body: Box::new(body),
            request,
        }
    }

    pub(crate) fn pending_parts(
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
            _ => None,
        }
    }

    pub(crate) fn certified_parts(&self) -> Option<(&TerminalCertificate, &TerminalOutcome)> {
        match self {
            Self::Certified {
                certificate,
                outcome,
            } => Some((certificate, outcome)),
            _ => None,
        }
    }

    pub(crate) fn receipt_assembled_parts(
        &self,
    ) -> Option<(
        &TerminalCertificate,
        &TerminalOutcome,
        &ReceiptBody,
        &ProducerSealRequest,
    )> {
        match self {
            Self::ReceiptAssembled {
                certificate,
                outcome,
                body,
                request,
            } => Some((certificate, outcome, body, request)),
            _ => None,
        }
    }

    pub(crate) fn stopped_receipt_assembled_parts(
        &self,
    ) -> Option<(&StopCause, &ReceiptBody, &ProducerSealRequest)> {
        match self {
            Self::StoppedReceiptAssembled {
                cause,
                body,
                request,
            } => Some((cause, body, request)),
            _ => None,
        }
    }

    pub(crate) fn seal_requested_parts(
        &self,
    ) -> Option<(
        &TerminalCertificate,
        &TerminalOutcome,
        &ReceiptBody,
        &ProducerSealRequest,
    )> {
        self.receipt_assembled_parts()
    }
}

impl StopCause {
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
    pub fn receipt_work(&self) -> ReceiptWork<'_> {
        match self {
            Self::Activating | Self::Active | Self::Waiting { .. } => ReceiptWork::NotTerminal,
            Self::Stopped { .. } => ReceiptWork::Assemble,
            Self::Completed { .. } | Self::StoppedPublished { .. } => ReceiptWork::Published,
            Self::Incomplete { .. } => ReceiptWork::Incomplete,
            Self::TerminalProof { proof } => match proof.as_ref() {
                TerminalProof::Pending { .. } => ReceiptWork::CollectSignatures,
                TerminalProof::Certified { .. } => ReceiptWork::Assemble,
                TerminalProof::ReceiptAssembled { request, .. }
                | TerminalProof::StoppedReceiptAssembled { request, .. } => {
                    ReceiptWork::Seal(request)
                }
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
            Self::Waiting { .. } => ExecLifecycle::Waiting,
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
        matches!(
            self,
            Self::Active | Self::Waiting { .. } | Self::TerminalProof { .. }
        )
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

    /// Borrow the stored private continuation, if any.
    #[must_use]
    pub const fn pending(&self) -> Option<(&PendingRecord, PendingCoordinate)> {
        match self {
            Self::Waiting {
                pending,
                coordinate,
            } => Some((pending, *coordinate)),
            _ => None,
        }
    }

    /// Borrow terminal proof progress, including incomplete proof evidence.
    #[must_use]
    pub(crate) fn terminal_proof(&self) -> Option<&TerminalProof> {
        match self {
            Self::TerminalProof { proof } | Self::Incomplete { proof, .. } => Some(proof),
            Self::Completed { .. }
            | Self::Activating
            | Self::Active
            | Self::Waiting { .. }
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

    pub(crate) fn waiting(pending: PendingRecord, coordinate: PendingCoordinate) -> Self {
        Self::Waiting {
            pending,
            coordinate,
        }
    }

    pub(crate) const fn active() -> Self {
        Self::Active
    }

    pub(crate) fn from_terminal_proof(proof: TerminalProof) -> Self {
        Self::TerminalProof {
            proof: Box::new(proof),
        }
    }

    pub(crate) fn completed(
        certificate: TerminalCertificate,
        outcome: TerminalOutcome,
        proof_id: ProofId,
        receipt_id: ReceiptId,
        producer: PeerId,
    ) -> Self {
        Self::Completed {
            proof: PublishedProof {
                certificate,
                outcome,
                proof_id,
                receipt_id,
                producer,
            },
        }
    }

    pub(crate) fn stopped_published(
        cause: StopCause,
        proof_id: ProofId,
        receipt_id: ReceiptId,
        producer: PeerId,
    ) -> Self {
        Self::StoppedPublished {
            cause,
            proof_id,
            receipt_id,
            producer,
        }
    }

    /// Construct a status from an authenticated abort/failure occurrence.
    pub(crate) fn stopped(occurrence: AbortOccurrence) -> Result<Self, ProtocolError> {
        occurrence.validate_shape()?;
        Ok(Self::Stopped {
            cause: StopCause::Authenticated(occurrence),
        })
    }

    /// Construct an incomplete status retaining terminal proof evidence.
    pub(crate) fn incomplete_from_local(
        proof: TerminalProof,
        reason: String,
    ) -> Result<Self, ProtocolError> {
        ensure_reason(&reason)?;
        Ok(Self::Incomplete {
            proof: Box::new(proof),
            reason,
        })
    }

    /// Construct the status represented by one certified shared termination
    /// entry. Successful `SessionEnd` entries begin proof progress and are
    /// therefore not terminal status until the producer seal is published.
    pub(crate) fn from_shared_entry(
        entry: &TraceEntry,
        commitment: StepCommitment,
    ) -> Result<Option<Self>, ProtocolError> {
        let Some(reason) = entry.abort_reason() else {
            return Ok(None);
        };
        ensure_reason(reason)?;
        let kind = if entry
            .effects
            .iter()
            .any(|effect| matches!(effect, crate::PublicEffect::SessionAbort { .. }))
        {
            AbortKind::Abort
        } else {
            AbortKind::Fail
        };
        Ok(Some(Self::Stopped {
            cause: StopCause::Shared {
                kind,
                commitment,
                reason: reason.to_owned(),
            },
        }))
    }

    /// Validate the complete status against the aggregate binding and public
    /// cursor. No independent lifecycle/status cross-product is consulted.
    pub(crate) fn validate_binding(
        &self,
        execution_id: ExecId,
        binding: &ExecutionBinding,
        producer: PeerId,
        public: PublicCursor,
        private: PrivateCursor,
    ) -> Result<(), ProtocolError> {
        match self {
            Self::Activating | Self::Active => Ok(()),
            Self::Waiting {
                pending,
                coordinate,
            } => {
                validate_pending_record(pending)?;
                if coordinate.record >= private.next_record
                    || pending.id
                        != super::pending_id(
                            execution_id,
                            coordinate.record,
                            coordinate.effect_index as usize,
                        )
                {
                    return Err(ProtocolError::PendingCoordinateMismatch);
                }
                Ok(())
            }
            Self::TerminalProof { proof } => {
                validate_terminal_progress(binding, producer, public, proof)
            }
            Self::Completed { proof } => {
                let progress = TerminalProof::Certified {
                    certificate: proof.certificate.clone(),
                    outcome: proof.outcome.clone(),
                };
                validate_terminal_progress(binding, producer, public, &progress)?;
                if proof.proof_id == ProofId::from_bytes([0; 32])
                    || proof.receipt_id == ReceiptId::from_bytes([0; 32])
                    || proof.producer != producer
                {
                    return Err(ProtocolError::InvalidTerminalStatus);
                }
                Ok(())
            }
            Self::Stopped { cause } => {
                cause.validate()?;
                validate_cause_binding(cause, binding, public)
            }
            Self::StoppedPublished {
                cause,
                proof_id,
                receipt_id,
                producer: published_producer,
            } => {
                cause.validate()?;
                validate_cause_binding(cause, binding, public)?;
                if *proof_id == ProofId::from_bytes([0; 32])
                    || *receipt_id == ReceiptId::from_bytes([0; 32])
                    || *published_producer != producer
                {
                    return Err(ProtocolError::InvalidTerminalStatus);
                }
                Ok(())
            }
            Self::Incomplete { proof, reason } => {
                ensure_reason(reason)?;
                validate_terminal_progress(binding, producer, public, proof)?;
                Ok(())
            }
        }
    }
}

pub(crate) fn validate_cause_binding(
    cause: &StopCause,
    binding: &ExecutionBinding,
    public: PublicCursor,
) -> Result<(), ProtocolError> {
    match cause {
        StopCause::Authenticated(occurrence) => {
            if occurrence.session_id() != binding.session_id()
                || *occurrence.coordinate() != public
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
                || commitment.step.checked_add(1) != Some(public.next_step())
                || commitment.post_state != public.state_hash()
                || commitment.link_hash() != public.chain_hash()
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
