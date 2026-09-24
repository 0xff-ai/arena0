//! Validated execution-frame domain values and their wire conversion.

use arena0_crypto::BlsSignature;
use arena0_wire::{
    ABORT_KIND_ABORT, ABORT_KIND_FAIL, ExecFrame as WireExecFrame, MAX_EXEC_MESSAGE_BYTES,
    MAX_EXEC_REASON_BYTES, MessageIdBytes, PeerIdBytes, SessionHashBytes, StateHashBytes,
    WireAbortCoordinate, WireAbortOccurrence, WireError, WireStepCommitment,
};
use borsh::{BorshDeserialize, BorshSerialize};
use std::io;
use thiserror::Error;

use crate::trace::StepCommitment;
use crate::{
    AbortKind, AbortOccurrence, AggregateAttestation, MessageId, SessionHash, SignerSet, StateHash,
    StepCertificate,
};

/// A validated execution fact used by protocol machinery.
///
/// Session routing is part of the transport stream metadata. Every value in
/// this enum represents a message, a signature or certificate over a complete
/// commitment, or an authenticated abort occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecFrame {
    /// Broadcast one program payload at an agreed trace position.
    Message {
        /// Content identity of the message envelope.
        message_id: MessageId,
        /// Agreed trace position.
        seq: u64,
        /// Shared state hash before applying the message.
        prestate: StateHash,
        /// Opaque guest payload.
        data: Vec<u8>,
        /// Shared state hash after applying the message.
        poststate: StateHash,
    },
    /// One participant's signature over one exact shared-state commitment.
    StepSignature {
        /// The complete commitment covered by `signature`.
        commitment: StepCommitment,
        /// BLS signature over the canonical commitment bytes.
        signature: BlsSignature,
    },
    /// N-of-N evidence for one shared-state commitment.
    StepCertificate {
        /// The certified commitment and its aggregate agreement.
        certificate: StepCertificate,
    },
    /// Unilateral termination.
    Abort {
        /// Signed, session-bound abort/failure occurrence.
        occurrence: AbortOccurrence,
    },
}

/// Failure converting an execution frame across the wire/domain boundary.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExecFrameError {
    /// A variable-length field exceeded the protocol's wire bound.
    #[error("invalid execution frame: {0}")]
    Wire(#[from] WireError),
    /// A raw commitment failed protocol-domain validation.
    #[error("invalid execution commitment: {0}")]
    Commitment(String),
    /// A raw abort occurrence failed protocol-domain validation.
    #[error("invalid abort occurrence: {0}")]
    Abort(String),
}

impl TryFrom<WireExecFrame> for ExecFrame {
    type Error = ExecFrameError;

    fn try_from(frame: WireExecFrame) -> Result<Self, Self::Error> {
        Ok(match frame {
            WireExecFrame::Message {
                message_id,
                seq,
                prestate,
                poststate,
                data,
            } => {
                check_data_len(data.len())?;
                Self::Message {
                    message_id: MessageId(message_id.0),
                    seq,
                    prestate: StateHash(prestate.0),
                    poststate: StateHash(poststate.0),
                    data,
                }
            }
            WireExecFrame::StepSignature {
                commitment,
                signature,
            } => Self::StepSignature {
                commitment: step_commitment_from_wire(commitment)?,
                signature,
            },
            WireExecFrame::Abort { occurrence } => {
                let occurrence = abort_from_wire(occurrence)?;
                Self::Abort { occurrence }
            }
            WireExecFrame::StepCertificate {
                commitment,
                signers,
                aggregate,
            } => {
                if signers.len() > arena0_wire::MAX_EXEC_SIGNER_BYTES {
                    return Err(WireError::ValueTooLarge {
                        field: "exec.signers",
                        size: signers.len(),
                        max: arena0_wire::MAX_EXEC_SIGNER_BYTES,
                    }
                    .into());
                }
                Self::StepCertificate {
                    certificate: StepCertificate {
                        commitment: step_commitment_from_wire(commitment)?,
                        agreement: AggregateAttestation {
                            aggregate,
                            signers: SignerSet(signers),
                        },
                    },
                }
            }
        })
    }
}

impl TryFrom<&ExecFrame> for WireExecFrame {
    type Error = ExecFrameError;

    fn try_from(frame: &ExecFrame) -> Result<Self, Self::Error> {
        Ok(match frame {
            ExecFrame::Message {
                message_id,
                seq,
                prestate,
                poststate,
                data,
            } => {
                check_data_len(data.len())?;
                Self::Message {
                    message_id: MessageIdBytes(message_id.0),
                    seq: *seq,
                    prestate: StateHashBytes(prestate.0),
                    poststate: StateHashBytes(poststate.0),
                    data: data.clone(),
                }
            }
            ExecFrame::StepSignature {
                commitment,
                signature,
            } => Self::StepSignature {
                commitment: step_commitment_to_wire(commitment)?,
                signature: *signature,
            },
            ExecFrame::Abort { occurrence } => Self::Abort {
                occurrence: abort_to_wire(occurrence)?,
            },
            ExecFrame::StepCertificate { certificate } => Self::StepCertificate {
                commitment: step_commitment_to_wire(certificate.commitment())?,
                signers: certificate.agreement().signers.0.clone(),
                aggregate: certificate.agreement().aggregate,
            },
        })
    }
}

impl TryFrom<ExecFrame> for WireExecFrame {
    type Error = ExecFrameError;

    fn try_from(frame: ExecFrame) -> Result<Self, Self::Error> {
        Self::try_from(&frame)
    }
}

impl BorshSerialize for ExecFrame {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        let wire = WireExecFrame::try_from(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        BorshSerialize::serialize(&wire, writer)
    }
}

impl BorshDeserialize for ExecFrame {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let wire = WireExecFrame::deserialize_reader(reader)?;
        Self::try_from(wire)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
    }
}

fn step_commitment_from_wire(raw: WireStepCommitment) -> Result<StepCommitment, ExecFrameError> {
    let commitment = StepCommitment {
        domain: raw.domain,
        session_id: SessionHash(raw.session_id.0),
        step: raw.step,
        entry_hash: raw.entry_hash,
        pre_state: StateHash(raw.pre_state.0),
        post_state: StateHash(raw.post_state.0),
        link: raw.link,
    };
    if commitment.domain != crate::STEP_COMMIT_DOMAIN {
        return Err(ExecFrameError::Commitment(
            "step commitment has an unknown domain".into(),
        ));
    }
    Ok(commitment)
}

fn step_commitment_to_wire(
    commitment: &StepCommitment,
) -> Result<WireStepCommitment, ExecFrameError> {
    if commitment.domain != crate::STEP_COMMIT_DOMAIN {
        return Err(ExecFrameError::Commitment(
            "step commitment has an unknown domain".into(),
        ));
    }
    Ok(WireStepCommitment {
        domain: commitment.domain,
        session_id: SessionHashBytes(commitment.session_id.0),
        step: commitment.step,
        entry_hash: commitment.entry_hash,
        pre_state: StateHashBytes(commitment.pre_state.0),
        post_state: StateHashBytes(commitment.post_state.0),
        link: commitment.link,
    })
}

fn abort_from_wire(raw: WireAbortOccurrence) -> Result<AbortOccurrence, ExecFrameError> {
    let kind = match raw.kind {
        ABORT_KIND_ABORT => AbortKind::Abort,
        ABORT_KIND_FAIL => AbortKind::Fail,
        tag => {
            return Err(ExecFrameError::Abort(format!(
                "unknown abort kind tag {tag}"
            )));
        }
    };
    let reason = String::from_utf8(raw.reason)
        .map_err(|error| ExecFrameError::Abort(format!("abort reason is not UTF-8: {error}")))?;
    let coordinate = crate::StepCursor::new(
        raw.coordinate.next_step,
        StateHash(raw.coordinate.state_hash.0),
        raw.coordinate.chain_hash,
    );
    AbortOccurrence::new(
        SessionHash(raw.session_hash.0),
        crate::PeerId(raw.sender.0),
        kind,
        raw.code,
        reason,
        coordinate,
        raw.signature,
    )
    .map_err(|error| ExecFrameError::Abort(error.to_string()))
}

fn abort_to_wire(value: &AbortOccurrence) -> Result<WireAbortOccurrence, ExecFrameError> {
    let reason = value.reason().as_bytes().to_vec();
    check_reason_len(Some(value.reason()))?;
    let coordinate = value.coordinate();
    Ok(WireAbortOccurrence {
        domain: crate::ABORT_OCCURRENCE_DOMAIN,
        version: crate::ABORT_OCCURRENCE_VERSION,
        session_hash: SessionHashBytes(value.session_id().0),
        sender: PeerIdBytes(value.sender().0),
        kind: value.kind().tag(),
        code: value.code(),
        reason,
        coordinate: WireAbortCoordinate {
            next_step: coordinate.next_step(),
            state_hash: StateHashBytes(coordinate.state_hash().0),
            chain_hash: coordinate.chain_hash(),
        },
        signature: value.signature(),
    })
}

fn check_data_len(len: usize) -> Result<(), ExecFrameError> {
    if len > MAX_EXEC_MESSAGE_BYTES {
        return Err(WireError::ValueTooLarge {
            field: "exec.data",
            size: len,
            max: MAX_EXEC_MESSAGE_BYTES,
        }
        .into());
    }
    Ok(())
}

fn check_reason_len(reason: Option<&str>) -> Result<(), ExecFrameError> {
    if let Some(reason) = reason
        && reason.len() > MAX_EXEC_REASON_BYTES
    {
        return Err(WireError::ValueTooLarge {
            field: "exec.reason",
            size: reason.len(),
            max: MAX_EXEC_REASON_BYTES,
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use arena0_crypto::Ed25519Signature;

    fn message_frame() -> ExecFrame {
        ExecFrame::Message {
            message_id: MessageId([1; 32]),
            seq: 7,
            prestate: StateHash([2; 32]),
            data: vec![3, 4, 5],
            poststate: StateHash([6; 32]),
        }
    }

    fn step_signature_frame() -> ExecFrame {
        ExecFrame::StepSignature {
            commitment: StepCommitment {
                domain: crate::STEP_COMMIT_DOMAIN,
                session_id: SessionHash([1; 32]),
                step: 8,
                entry_hash: [2; 32],
                pre_state: StateHash([3; 32]),
                post_state: StateHash([4; 32]),
                link: [5; 32],
            },
            signature: BlsSignature([6; 48]),
        }
    }

    fn abort_frame() -> ExecFrame {
        let occurrence = AbortOccurrence::new(
            SessionHash([1; 32]),
            crate::PeerId([2; 32]),
            AbortKind::Abort,
            3,
            "reason",
            crate::StepCursor::new(0, StateHash([4; 32]), crate::CHAIN_START),
            Ed25519Signature([5; 64]),
        )
        .expect("test abort occurrence has a valid shape");
        ExecFrame::Abort { occurrence }
    }

    fn assert_roundtrip(frame: ExecFrame) {
        let protocol_bytes = borsh::to_vec(&frame).expect("protocol frame serializes");
        let wire = WireExecFrame::try_from(&frame).expect("frame converts to wire");
        let wire_bytes = borsh::to_vec(&wire).expect("wire frame serializes");
        assert_eq!(protocol_bytes, wire_bytes);
        assert_eq!(
            borsh::from_slice::<ExecFrame>(&protocol_bytes).expect("protocol frame decodes"),
            frame
        );
    }

    #[test]
    fn borsh_roundtrips_every_execution_frame_variant_using_wire_encoding() {
        for frame in [message_frame(), step_signature_frame(), abort_frame()] {
            assert_roundtrip(frame);
        }
        let ExecFrame::StepSignature {
            commitment,
            signature,
        } = step_signature_frame()
        else {
            unreachable!()
        };
        assert_roundtrip(ExecFrame::StepCertificate {
            certificate: StepCertificate {
                commitment,
                agreement: AggregateAttestation {
                    aggregate: signature,
                    signers: SignerSet(vec![3]),
                },
            },
        });
    }

    #[test]
    fn certificate_signer_bitmap_is_bounded_before_allocation() {
        assert_eq!(
            arena0_wire::MAX_EXEC_SIGNER_BYTES,
            crate::negotiation::MAX_PARTICIPANTS.div_ceil(8)
        );
        let ExecFrame::StepSignature { commitment, .. } = step_signature_frame() else {
            unreachable!()
        };
        let mut bytes = vec![arena0_wire::EXEC_KIND_STEP_CERTIFICATE];
        bytes.extend(borsh::to_vec(&step_commitment_to_wire(&commitment).unwrap()).unwrap());
        bytes.extend_from_slice(&(arena0_wire::MAX_EXEC_SIGNER_BYTES as u32 + 1).to_le_bytes());
        assert_eq!(
            borsh::from_slice::<ExecFrame>(&bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn borsh_decode_rejects_oversized_message_before_allocating() {
        let mut bytes = vec![arena0_wire::EXEC_KIND_MESSAGE];
        bytes.extend_from_slice(&[0; 32]);
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(&[0; 32]);
        bytes.extend_from_slice(&[0; 32]);
        bytes.extend_from_slice(&(MAX_EXEC_MESSAGE_BYTES as u32 + 1).to_le_bytes());

        let error = borsh::from_slice::<ExecFrame>(&bytes).expect_err("oversized data is invalid");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
