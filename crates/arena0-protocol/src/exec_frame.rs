//! Validated execution frames and their canonical wire encoding.
//!
//! Execution streams carry one durable fact per frame. Session routing is
//! established by the transport when the stream is opened, so the frame
//! itself contains a message, a commitment signature or certificate, or an
//! authenticated abort occurrence. The first Borsh byte is the frame kind;
//! [`arena0_wire::Codec`] adds the versioned length-prefixed envelope.

use arena0_crypto::BlsSignature;
use arena0_wire::{WireDecode, read_bounded_bytes, serialize_bounded_bytes};
use borsh::{BorshDeserialize, BorshSerialize};
use std::io;

use crate::trace::StepCommitment;
use crate::{
    AbortOccurrence, AggregateAttestation, MAX_EFFECT_PAYLOAD_BYTES, SignerSet, StepCertificate,
};

/// Frame kind of [`ExecFrame::Message`].
const EXEC_KIND_MESSAGE: u8 = 0x00;
/// Frame kind of [`ExecFrame::StepSignature`].
const EXEC_KIND_STEP_SIGNATURE: u8 = 0x01;
/// Frame kind of [`ExecFrame::Abort`].
const EXEC_KIND_ABORT: u8 = 0x02;
/// Frame kind of [`ExecFrame::StepCertificate`].
const EXEC_KIND_STEP_CERTIFICATE: u8 = 0x03;
/// Maximum signer bitmap size for the protocol's participant bound.
const MAX_EXEC_SIGNER_BYTES: usize = crate::negotiation::MAX_PARTICIPANTS.div_ceil(8);

/// A validated execution fact used by protocol machinery.
///
/// Session routing is part of the transport stream metadata. Every value in
/// this enum represents a message, a signature or certificate over a complete
/// commitment, or an authenticated abort occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecFrame {
    /// Broadcast one program payload at an agreed trace position.
    ///
    /// The commitment carries the author's step, pre/post shared hashes, entry
    /// hash, and chain link. A receiver rebuilds the entry itself and compares
    /// its own commitment before signing.
    Message {
        /// The author's complete commitment for this message step.
        commitment: StepCommitment,
        /// Opaque guest payload, bounded like the trace's message event.
        data: Vec<u8>,
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

// Frame fields use the wire crate's bounded codec rather than
// `arena0_program::bounded` so an oversize field surfaces as a typed
// `WireError` that transports classify as `PayloadTooLarge`.
impl BorshSerialize for ExecFrame {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Self::Message { commitment, data } => {
                BorshSerialize::serialize(&EXEC_KIND_MESSAGE, writer)?;
                write_commitment(commitment, writer)?;
                serialize_bounded_bytes(writer, data, MAX_EFFECT_PAYLOAD_BYTES, "exec.data")
            }
            Self::StepSignature {
                commitment,
                signature,
            } => {
                BorshSerialize::serialize(&EXEC_KIND_STEP_SIGNATURE, writer)?;
                write_commitment(commitment, writer)?;
                BorshSerialize::serialize(signature, writer)
            }
            Self::Abort { occurrence } => {
                BorshSerialize::serialize(&EXEC_KIND_ABORT, writer)?;
                BorshSerialize::serialize(occurrence, writer)
            }
            Self::StepCertificate { certificate } => {
                BorshSerialize::serialize(&EXEC_KIND_STEP_CERTIFICATE, writer)?;
                write_commitment(certificate.commitment(), writer)?;
                let agreement = certificate.agreement();
                serialize_bounded_bytes(
                    writer,
                    &agreement.signers.0,
                    MAX_EXEC_SIGNER_BYTES,
                    "exec.signers",
                )?;
                BorshSerialize::serialize(&agreement.aggregate, writer)
            }
        }
    }
}

impl BorshDeserialize for ExecFrame {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            EXEC_KIND_MESSAGE => Ok(Self::Message {
                commitment: read_commitment(reader)?,
                data: read_bounded_bytes(reader, MAX_EFFECT_PAYLOAD_BYTES, "exec.data")?,
            }),
            EXEC_KIND_STEP_SIGNATURE => Ok(Self::StepSignature {
                commitment: read_commitment(reader)?,
                signature: BlsSignature::deserialize_reader(reader)?,
            }),
            EXEC_KIND_ABORT => Ok(Self::Abort {
                occurrence: AbortOccurrence::deserialize_reader(reader)?,
            }),
            EXEC_KIND_STEP_CERTIFICATE => {
                let commitment = read_commitment(reader)?;
                let signers = read_bounded_bytes(reader, MAX_EXEC_SIGNER_BYTES, "exec.signers")?;
                let aggregate = BlsSignature::deserialize_reader(reader)?;
                Ok(Self::StepCertificate {
                    certificate: StepCertificate {
                        commitment,
                        agreement: AggregateAttestation {
                            aggregate,
                            signers: SignerSet(signers),
                        },
                    },
                })
            }
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown execution frame tag {tag}"),
            )),
        }
    }
}

// Every variable-length field is bounded before allocation.
impl WireDecode for ExecFrame {}

fn write_commitment<W: io::Write>(commitment: &StepCommitment, writer: &mut W) -> io::Result<()> {
    if commitment.domain != crate::STEP_COMMIT_DOMAIN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "step commitment has an unknown domain",
        ));
    }
    BorshSerialize::serialize(commitment, writer)
}

fn read_commitment<R: io::Read>(reader: &mut R) -> io::Result<StepCommitment> {
    let commitment = StepCommitment::deserialize_reader(reader)?;
    if commitment.domain != crate::STEP_COMMIT_DOMAIN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "step commitment has an unknown domain",
        ));
    }
    Ok(commitment)
}

#[cfg(test)]
mod tests {
    use super::*;

    use arena0_crypto::Ed25519Signature;
    use arena0_wire::{Codec, WireError};

    use crate::{AbortKind, SessionHash, StateHash};

    /// The stock-Borsh layout the hand-written frame codec must reproduce.
    #[derive(BorshSerialize)]
    enum DerivedExecFrame {
        Message {
            commitment: StepCommitment,
            data: Vec<u8>,
        },
        StepSignature {
            commitment: StepCommitment,
            signature: BlsSignature,
        },
        Abort {
            occurrence: AbortOccurrence,
        },
        StepCertificate {
            commitment: StepCommitment,
            signers: Vec<u8>,
            aggregate: BlsSignature,
        },
    }

    fn commitment() -> StepCommitment {
        StepCommitment {
            domain: crate::STEP_COMMIT_DOMAIN,
            session_id: SessionHash([1; 32]),
            step: 8,
            entry_hash: [2; 32],
            pre_state: StateHash([3; 32]),
            post_state: StateHash([4; 32]),
            link: [5; 32],
        }
    }

    fn occurrence() -> AbortOccurrence {
        AbortOccurrence::new(
            SessionHash([1; 32]),
            crate::PeerId([2; 32]),
            AbortKind::Abort,
            3,
            "reason",
            crate::StepCursor::new(0, StateHash([4; 32]), crate::CHAIN_START),
            Ed25519Signature([5; 64]),
        )
        .expect("test abort occurrence has a valid shape")
    }

    fn frames() -> Vec<(ExecFrame, DerivedExecFrame)> {
        let certificate = StepCertificate {
            commitment: commitment(),
            agreement: AggregateAttestation {
                aggregate: BlsSignature([7; 48]),
                signers: SignerSet(vec![3]),
            },
        };
        vec![
            (
                ExecFrame::Message {
                    commitment: commitment(),
                    data: vec![3, 4, 5],
                },
                DerivedExecFrame::Message {
                    commitment: commitment(),
                    data: vec![3, 4, 5],
                },
            ),
            (
                ExecFrame::StepSignature {
                    commitment: commitment(),
                    signature: BlsSignature([6; 48]),
                },
                DerivedExecFrame::StepSignature {
                    commitment: commitment(),
                    signature: BlsSignature([6; 48]),
                },
            ),
            (
                ExecFrame::Abort {
                    occurrence: occurrence(),
                },
                DerivedExecFrame::Abort {
                    occurrence: occurrence(),
                },
            ),
            (
                ExecFrame::StepCertificate { certificate },
                DerivedExecFrame::StepCertificate {
                    commitment: commitment(),
                    signers: vec![3],
                    aggregate: BlsSignature([7; 48]),
                },
            ),
        ]
    }

    fn message_body(len: usize) -> Vec<u8> {
        let mut body = vec![EXEC_KIND_MESSAGE];
        body.extend(borsh::to_vec(&commitment()).unwrap());
        body.extend_from_slice(&u32::try_from(len).unwrap().to_le_bytes());
        body.resize(body.len() + len, 0);
        body
    }

    #[test]
    fn frames_preserve_borsh_layout_and_round_trip() {
        for (frame, derived) in frames() {
            let bytes = borsh::to_vec(&frame).expect("frame serializes");
            assert_eq!(bytes, borsh::to_vec(&derived).unwrap());
            assert_eq!(borsh::from_slice::<ExecFrame>(&bytes).unwrap(), frame);
            let encoded = Codec::default().encode(&frame).unwrap();
            assert_eq!(
                Codec::default().decode::<ExecFrame>(&encoded).unwrap(),
                frame
            );
        }
    }

    #[test]
    fn message_payload_is_bounded_like_the_trace_event() {
        assert!(borsh::from_slice::<ExecFrame>(&message_body(MAX_EFFECT_PAYLOAD_BYTES)).is_ok());
        let oversized = Codec::default().encode(&ExecFrame::Message {
            commitment: commitment(),
            data: vec![0; MAX_EFFECT_PAYLOAD_BYTES + 1],
        });
        assert!(matches!(
            oversized,
            Err(WireError::ValueTooLarge {
                field: "exec.data",
                ..
            })
        ));

        let mut frame = (u32::try_from(message_body(MAX_EFFECT_PAYLOAD_BYTES + 1).len()).unwrap()
            + 2)
        .to_le_bytes()
        .to_vec();
        frame.extend_from_slice(&arena0_wire::FRAME_VERSION.to_le_bytes());
        frame.extend(message_body(MAX_EFFECT_PAYLOAD_BYTES + 1));
        assert!(matches!(
            Codec::default().decode::<ExecFrame>(&frame),
            Err(WireError::ValueTooLarge {
                field: "exec.data",
                ..
            })
        ));
    }

    #[test]
    fn decode_rejects_oversized_message_before_allocating() {
        let mut bytes = vec![EXEC_KIND_MESSAGE];
        bytes.extend(borsh::to_vec(&commitment()).unwrap());
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        let error = borsh::from_slice::<ExecFrame>(&bytes).expect_err("oversized data is invalid");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn certificate_signer_bitmap_is_bounded_before_allocation() {
        let mut bytes = vec![EXEC_KIND_STEP_CERTIFICATE];
        bytes.extend(borsh::to_vec(&commitment()).unwrap());
        bytes.extend_from_slice(&(MAX_EXEC_SIGNER_BYTES as u32 + 1).to_le_bytes());
        assert_eq!(
            borsh::from_slice::<ExecFrame>(&bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn unknown_tags_and_commitment_domains_are_rejected() {
        assert!(borsh::from_slice::<ExecFrame>(&[0xff]).is_err());
        let mut foreign = commitment();
        foreign.domain[0] ^= 1;
        let frame = ExecFrame::StepSignature {
            commitment: foreign.clone(),
            signature: BlsSignature([6; 48]),
        };
        assert!(borsh::to_vec(&frame).is_err());
        let mut bytes = vec![EXEC_KIND_STEP_SIGNATURE];
        bytes.extend(borsh::to_vec(&foreign).unwrap());
        bytes.extend(borsh::to_vec(&BlsSignature([6; 48])).unwrap());
        assert!(borsh::from_slice::<ExecFrame>(&bytes).is_err());
    }
}
