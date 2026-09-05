//! Raw execution-stream wire values.
//!
//! Execution streams carry one durable fact per frame. Session routing is
//! established by the transport when the stream is opened, so the frame
//! itself contains only a message, one complete signed commitment, or one
//! authenticated abort occurrence.

use arena0_crypto::{BlsSignature, Ed25519Signature};
use borsh::{BorshDeserialize, BorshSerialize};
use std::io;

use crate::{read_bounded_bytes, serialize_bounded_bytes};

/// Maximum opaque program payload in one execution message.
pub const MAX_EXEC_MESSAGE_BYTES: usize = 1_048_576;
/// Maximum reason string size in an abort frame.
pub const MAX_EXEC_REASON_BYTES: usize = 4 * 1024;
/// Maximum encoded bytes in one authenticated abort occurrence, excluding the
/// outer frame header. The fixed fields are intentionally included in this
/// bound so callers cannot smuggle an unbounded reason or commitment.
pub const MAX_EXEC_ABORT_OCCURRENCE_BYTES: usize = 16 * 1024;
const EXEC_ABORT_FIXED_BYTES: usize = 24 + 2 + 32 + 32 + 1 + 4 + 4 + 8 + 32 + 32 + 64;

/// The convergence-fetch stream discriminator.
pub const PROTO_FETCH: u8 = 0x01;
/// The committed-execution stream discriminator.
pub const PROTO_EXEC: u8 = 0x02;

/// Typed execution message kind for [`ExecFrame::Message`].
pub const EXEC_KIND_MESSAGE: u8 = 0x00;
/// Typed execution message kind for [`ExecFrame::StepSignature`].
pub const EXEC_KIND_STEP_SIGNATURE: u8 = 0x01;
/// Typed execution message kind for [`ExecFrame::End`].
pub const EXEC_KIND_END: u8 = 0x02;
/// Typed execution message kind for [`ExecFrame::Abort`].
pub const EXEC_KIND_ABORT: u8 = 0x03;

/// Stable version-1 terminal kind tag carried by an abort occurrence.
pub const ABORT_KIND_ABORT: u8 = 0x00;
/// Stable version-1 failure kind tag carried by an abort occurrence.
pub const ABORT_KIND_FAIL: u8 = 0x01;

/// A fixed-width raw session hash in a wire frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize)]
pub struct SessionHashBytes(pub [u8; 32]);

/// A fixed-width raw message identity in a wire frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize)]
pub struct MessageIdBytes(pub [u8; 32]);

/// A fixed-width raw shared-state hash in a wire frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize)]
pub struct StateHashBytes(pub [u8; 32]);

/// A fixed-width raw peer identity (an Ed25519 public key).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize)]
pub struct PeerIdBytes(pub [u8; 32]);

/// A fixed-width raw witness commitment in a wire frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, BorshSerialize, BorshDeserialize)]
pub struct WitnessCommitmentBytes(pub [u8; 32]);

/// The raw fixed-width representation of a protocol step commitment.
///
/// The protocol crate validates and converts these fields into its domain
/// commitment. Keeping the fields explicit here preserves the commitment's
/// canonical Borsh layout without making the wire crate depend on protocol
/// types.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct WireStepCommitment {
    /// Commitment domain-separation tag.
    pub domain: [u8; 24],
    /// Session this step belongs to.
    pub session_id: SessionHashBytes,
    /// Canonical public trace position.
    pub step: u64,
    /// Hash of the trace entry.
    pub entry_hash: [u8; 32],
    /// Shared state hash before the step.
    pub pre_state: StateHashBytes,
    /// Shared state hash after the step.
    pub post_state: StateHashBytes,
    /// Chain link to the previous step.
    pub link: [u8; 32],
}

/// The raw fixed-width representation of a protocol terminal commitment.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct WireTerminalCommitment {
    /// Commitment domain-separation tag.
    pub domain: [u8; 24],
    /// Session this terminal belongs to.
    pub session_id: SessionHashBytes,
    /// Final public trace position.
    pub final_step: u64,
    /// Shared state hash at completion.
    pub final_state: StateHashBytes,
    /// Hash of the terminal outcome bytes.
    pub outcome_hash: [u8; 32],
}

/// The exact public cursor authenticated by an abort occurrence.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct WireAbortCoordinate {
    /// Next public trace position.
    pub next_step: u64,
    /// Last certified shared state hash.
    pub state_hash: StateHashBytes,
    /// Hash of the last certified commitment.
    pub chain_hash: [u8; 32],
}

/// A raw authenticated abort/fail occurrence. Protocol-domain validation is
/// performed by `arena0-protocol`; this type only owns bounded framing and
/// fixed tag decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireAbortOccurrence {
    /// Domain separation tag.
    pub domain: [u8; 24],
    /// Contract version.
    pub version: u16,
    /// Session identity.
    pub session_hash: SessionHashBytes,
    /// Ed25519 sender identity signed by the occurrence.
    pub sender: PeerIdBytes,
    /// Stable abort/fail kind tag.
    pub kind: u8,
    /// Stable terminal code.
    pub code: u32,
    /// UTF-8 reason bytes, bounded before allocation.
    pub reason: Vec<u8>,
    /// Exact public chain coordinate.
    pub coordinate: WireAbortCoordinate,
    /// Ed25519 signature over the canonical occurrence body.
    pub signature: Ed25519Signature,
}

impl BorshSerialize for WireAbortOccurrence {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        if self.kind != ABORT_KIND_ABORT && self.kind != ABORT_KIND_FAIL {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown abort kind tag {}", self.kind),
            ));
        }
        if self.reason.len() > MAX_EXEC_REASON_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                crate::WireError::ValueTooLarge {
                    field: "exec.abort.reason",
                    size: self.reason.len(),
                    max: MAX_EXEC_REASON_BYTES,
                },
            ));
        }
        let encoded_size = EXEC_ABORT_FIXED_BYTES
            .checked_add(self.reason.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "abort size overflows"))?;
        if encoded_size > MAX_EXEC_ABORT_OCCURRENCE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                crate::WireError::ValueTooLarge {
                    field: "exec.abort.occurrence",
                    size: encoded_size,
                    max: MAX_EXEC_ABORT_OCCURRENCE_BYTES,
                },
            ));
        }
        BorshSerialize::serialize(&self.domain, writer)?;
        BorshSerialize::serialize(&self.version, writer)?;
        BorshSerialize::serialize(&self.session_hash, writer)?;
        BorshSerialize::serialize(&self.sender, writer)?;
        BorshSerialize::serialize(&self.kind, writer)?;
        BorshSerialize::serialize(&self.code, writer)?;
        serialize_bounded_bytes(
            writer,
            &self.reason,
            MAX_EXEC_REASON_BYTES,
            "exec.abort.reason",
        )?;
        BorshSerialize::serialize(&self.coordinate, writer)?;
        BorshSerialize::serialize(&self.signature, writer)
    }
}

impl BorshDeserialize for WireAbortOccurrence {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        let domain = <[u8; 24]>::deserialize_reader(reader)?;
        let version = u16::deserialize_reader(reader)?;
        let session_hash = SessionHashBytes::deserialize_reader(reader)?;
        let sender = PeerIdBytes::deserialize_reader(reader)?;
        let kind = u8::deserialize_reader(reader)?;
        if kind != ABORT_KIND_ABORT && kind != ABORT_KIND_FAIL {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown abort kind {kind}"),
            ));
        }
        let code = u32::deserialize_reader(reader)?;
        let reason = read_bounded_bytes(reader, MAX_EXEC_REASON_BYTES, "exec.abort.reason")?;
        let encoded_size = EXEC_ABORT_FIXED_BYTES
            .checked_add(reason.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "abort size overflows"))?;
        if encoded_size > MAX_EXEC_ABORT_OCCURRENCE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "exec.abort.occurrence exceeds bound: {encoded_size} > {MAX_EXEC_ABORT_OCCURRENCE_BYTES}"
                ),
            ));
        }
        let coordinate = WireAbortCoordinate::deserialize_reader(reader)?;
        let signature = Ed25519Signature::deserialize_reader(reader)?;
        Ok(Self {
            domain,
            version,
            session_hash,
            sender,
            kind,
            code,
            reason,
            coordinate,
            signature,
        })
    }
}

/// Raw execution protocol for a confirmed ensemble.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecFrame {
    /// Broadcast one program payload at a public trace position.
    Message {
        /// Content identity of the message envelope.
        message_id: MessageIdBytes,
        /// Public trace position.
        seq: u64,
        /// Shared state hash before applying the message.
        prestate: StateHashBytes,
        /// Opaque guest payload.
        data: Vec<u8>,
        /// Content commitment for private witness evidence.
        witness: WitnessCommitmentBytes,
    },
    /// One participant's signature over one exact shared-state commitment.
    StepSignature {
        /// The complete commitment covered by `signature`.
        commitment: WireStepCommitment,
        /// BLS signature over the canonical commitment bytes.
        signature: BlsSignature,
    },
    /// One participant's signature over one exact terminal commitment.
    End {
        /// The complete terminal commitment covered by `signature`.
        commitment: WireTerminalCommitment,
        /// BLS signature over the canonical commitment bytes.
        signature: BlsSignature,
    },
    /// Unilateral termination.
    Abort {
        /// Signed, session-bound occurrence.
        occurrence: WireAbortOccurrence,
    },
}

impl BorshSerialize for ExecFrame {
    fn serialize<W: io::Write>(&self, writer: &mut W) -> io::Result<()> {
        match self {
            Self::Message {
                message_id,
                seq,
                prestate,
                data,
                witness,
            } => {
                BorshSerialize::serialize(&EXEC_KIND_MESSAGE, writer)?;
                BorshSerialize::serialize(message_id, writer)?;
                BorshSerialize::serialize(seq, writer)?;
                BorshSerialize::serialize(prestate, writer)?;
                serialize_bounded_bytes(writer, data, MAX_EXEC_MESSAGE_BYTES, "exec.data")?;
                BorshSerialize::serialize(witness, writer)
            }
            Self::StepSignature {
                commitment,
                signature,
            } => {
                BorshSerialize::serialize(&EXEC_KIND_STEP_SIGNATURE, writer)?;
                BorshSerialize::serialize(commitment, writer)?;
                BorshSerialize::serialize(signature, writer)
            }
            Self::End {
                commitment,
                signature,
            } => {
                BorshSerialize::serialize(&EXEC_KIND_END, writer)?;
                BorshSerialize::serialize(commitment, writer)?;
                BorshSerialize::serialize(signature, writer)
            }
            Self::Abort { occurrence } => {
                BorshSerialize::serialize(&EXEC_KIND_ABORT, writer)?;
                BorshSerialize::serialize(occurrence, writer)
            }
        }
    }
}

impl BorshDeserialize for ExecFrame {
    fn deserialize_reader<R: io::Read>(reader: &mut R) -> io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            EXEC_KIND_MESSAGE => Ok(Self::Message {
                message_id: MessageIdBytes::deserialize_reader(reader)?,
                seq: u64::deserialize_reader(reader)?,
                prestate: StateHashBytes::deserialize_reader(reader)?,
                data: read_bounded_bytes(reader, MAX_EXEC_MESSAGE_BYTES, "exec.data")?,
                witness: WitnessCommitmentBytes::deserialize_reader(reader)?,
            }),
            EXEC_KIND_STEP_SIGNATURE => Ok(Self::StepSignature {
                commitment: WireStepCommitment::deserialize_reader(reader)?,
                signature: BlsSignature::deserialize_reader(reader)?,
            }),
            EXEC_KIND_END => Ok(Self::End {
                commitment: WireTerminalCommitment::deserialize_reader(reader)?,
                signature: BlsSignature::deserialize_reader(reader)?,
            }),
            EXEC_KIND_ABORT => Ok(Self::Abort {
                occurrence: WireAbortOccurrence::deserialize_reader(reader)?,
            }),
            tag => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown execution frame tag {tag}"),
            )),
        }
    }
}

impl crate::sealed::WireDecode for ExecFrame {}
