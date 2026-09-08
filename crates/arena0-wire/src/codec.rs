//! Canonical length-prefixed Borsh framing.
//!
//! The wire format for one frame is
//! `[length: u32 LE][version: u16 LE][borsh body]`. The length covers the
//! version and Borsh body. The first Borsh byte is the typed message-kind
//! discriminant emitted by the raw frame value. A codec always enforces its
//! body limit before allocation of the final frame and rejects unsupported
//! versions, truncation, or trailing bytes.

use crate::{WireDecode, WireError};
use std::io::{self, Write};

/// Maximum allowed wire frame payload (1 MiB).
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 1_048_576;

/// Size of the frame header: four bytes containing a little-endian body size.
pub const FRAME_HEADER_SIZE: usize = 4;
/// The current version of the canonical transport-frame envelope.
pub const FRAME_VERSION: u16 = 1;
/// Size of the little-endian frame-version field.
pub const FRAME_VERSION_SIZE: usize = std::mem::size_of::<u16>();

/// A length-prefixed Borsh codec carrying its own maximum frame size.
#[derive(Debug, Clone, Copy)]
pub struct Codec {
    max_size: usize,
}

impl Default for Codec {
    fn default() -> Self {
        Self {
            max_size: DEFAULT_MAX_MESSAGE_SIZE,
        }
    }
}

impl Codec {
    /// Construct a codec with a custom maximum body size.
    #[must_use]
    pub const fn new(max_size: usize) -> Self {
        Self { max_size }
    }

    /// Return the maximum body size accepted by this codec.
    #[must_use]
    pub const fn max_size(&self) -> usize {
        self.max_size
    }

    /// Encode a Borsh-serializable value as one length-prefixed frame.
    pub fn encode<T: borsh::BorshSerialize>(&self, value: &T) -> Result<Vec<u8>, WireError> {
        let mut writer = BoundedWriter::new(self.max_size);
        let result = value.serialize(&mut writer);
        if writer.overflowed {
            return Err(WireError::PayloadTooLarge {
                size: writer.attempted_size,
                max: self.max_size,
            });
        }
        if let Err(error) = result {
            return Err(
                match error
                    .get_ref()
                    .and_then(|source| source.downcast_ref::<WireError>())
                {
                    Some(error) => error.clone(),
                    None => WireError::Encode(error.to_string()),
                },
            );
        }
        self.frame(writer.into_inner())
    }

    /// Decode one exact length-prefixed frame into a Borsh value.
    pub fn decode<T: WireDecode>(&self, bytes: &[u8]) -> Result<T, WireError> {
        let body = self.decode_frame(bytes)?;
        borsh::from_slice(body).map_err(|error| {
            match error
                .get_ref()
                .and_then(|source| source.downcast_ref::<WireError>())
            {
                Some(error) => error.clone(),
                None => WireError::Decode(error.to_string()),
            }
        })
    }

    /// Decode one exact frame and return its typed Borsh body without
    /// interpreting it. The returned slice excludes the validated version
    /// field but retains the raw frame's message-kind discriminant.
    pub fn decode_frame<'a>(&self, bytes: &'a [u8]) -> Result<&'a [u8], WireError> {
        if bytes.len() < FRAME_HEADER_SIZE {
            return Err(WireError::FrameTooShort {
                expected: FRAME_HEADER_SIZE,
                actual: bytes.len(),
            });
        }

        let body_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        if body_len < FRAME_VERSION_SIZE {
            return Err(WireError::FrameTooShort {
                expected: FRAME_HEADER_SIZE + FRAME_VERSION_SIZE,
                actual: FRAME_HEADER_SIZE + body_len,
            });
        }
        self.check_body_size(body_len - FRAME_VERSION_SIZE)?;

        let expected =
            FRAME_HEADER_SIZE
                .checked_add(body_len)
                .ok_or(WireError::PayloadTooLarge {
                    size: body_len,
                    max: self.max_size,
                })?;
        if bytes.len() < expected {
            return Err(WireError::Truncated {
                declared: body_len,
                actual: bytes.len() - FRAME_HEADER_SIZE,
            });
        }
        if bytes.len() > expected {
            return Err(WireError::TrailingBytes {
                expected,
                actual: bytes.len(),
            });
        }

        let version_start = FRAME_HEADER_SIZE;
        let version_end = version_start + FRAME_VERSION_SIZE;
        let actual_version = u16::from_le_bytes([bytes[version_start], bytes[version_start + 1]]);
        if actual_version != FRAME_VERSION {
            return Err(WireError::UnsupportedVersion {
                expected: FRAME_VERSION,
                actual: actual_version,
            });
        }

        Ok(&bytes[version_end..expected])
    }

    fn frame(&self, body: Vec<u8>) -> Result<Vec<u8>, WireError> {
        self.check_body_size(body.len())?;
        let framed_body_len =
            body.len()
                .checked_add(FRAME_VERSION_SIZE)
                .ok_or(WireError::PayloadTooLarge {
                    size: body.len(),
                    max: self.max_size,
                })?;
        let body_len = u32::try_from(framed_body_len).map_err(|_| WireError::PayloadTooLarge {
            size: framed_body_len,
            max: u32::MAX as usize,
        })?;

        let capacity =
            FRAME_HEADER_SIZE
                .checked_add(framed_body_len)
                .ok_or(WireError::PayloadTooLarge {
                    size: body.len(),
                    max: self.max_size,
                })?;
        let mut frame = Vec::with_capacity(capacity);
        frame.extend_from_slice(&body_len.to_le_bytes());
        frame.extend_from_slice(&FRAME_VERSION.to_le_bytes());
        frame.extend_from_slice(&body);
        Ok(frame)
    }

    fn check_body_size(&self, size: usize) -> Result<(), WireError> {
        if size > self.max_size {
            return Err(WireError::PayloadTooLarge {
                size,
                max: self.max_size,
            });
        }
        Ok(())
    }
}

struct BoundedWriter {
    bytes: Vec<u8>,
    max: usize,
    overflowed: bool,
    attempted_size: usize,
}

impl BoundedWriter {
    fn new(max: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max,
            overflowed: false,
            attempted_size: 0,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let attempted_size = self.bytes.len().checked_add(bytes.len());
        if attempted_size.is_none_or(|size| size > self.max) {
            self.overflowed = true;
            self.attempted_size = attempted_size.unwrap_or(usize::MAX);
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "wire body exceeds codec bound",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        EXEC_KIND_ABORT, EXEC_KIND_MESSAGE, ExecFrame, FetchFrame, MAX_EXEC_REASON_BYTES,
        MAX_FETCH_RESPONSE_BYTES, MessageIdBytes, PeerIdBytes, SessionHashBytes, StateHashBytes,
        StreamProtocol, WireAbortCoordinate, WireAbortOccurrence, WireError, WireStepCommitment,
        WireTerminalCommitment, WitnessCommitmentBytes,
    };
    use arena0_crypto::BlsSignature;
    use borsh::BorshSerialize;

    #[derive(BorshSerialize)]
    enum DerivedExecFrame {
        Message {
            message_id: MessageIdBytes,
            seq: u64,
            prestate: StateHashBytes,
            data: Vec<u8>,
            witness: WitnessCommitmentBytes,
        },
        StepSignature {
            commitment: WireStepCommitment,
            signature: BlsSignature,
        },
        End {
            commitment: WireTerminalCommitment,
            signature: BlsSignature,
        },
        Abort {
            occurrence: WireAbortOccurrence,
        },
    }

    #[derive(BorshSerialize)]
    enum DerivedFetchFrame {
        FetchActivationTickets {
            session_hash: SessionHashBytes,
        },
        ActivationTickets {
            session_hash: SessionHashBytes,
            tickets: Vec<Vec<u8>>,
        },
    }

    fn codec() -> Codec {
        Codec::default()
    }

    fn raw_frame(body: &[u8]) -> Vec<u8> {
        let body_len = (FRAME_VERSION_SIZE + body.len()) as u32;
        [
            body_len.to_le_bytes().to_vec(),
            FRAME_VERSION.to_le_bytes().to_vec(),
            body.to_vec(),
        ]
        .concat()
    }

    fn abort_occurrence(reason: &[u8]) -> WireAbortOccurrence {
        WireAbortOccurrence {
            domain: *b"arena0/abort-occurrence\0",
            version: 1,
            session_hash: SessionHashBytes([0x11; 32]),
            sender: PeerIdBytes([0x22; 32]),
            kind: crate::ABORT_KIND_ABORT,
            code: 7,
            reason: reason.to_vec(),
            coordinate: WireAbortCoordinate {
                next_step: 3,
                state_hash: StateHashBytes([0x33; 32]),
                chain_hash: [0x44; 32],
            },
            signature: arena0_crypto::Ed25519Signature([0x55; 64]),
        }
    }

    fn step_commitment() -> WireStepCommitment {
        WireStepCommitment {
            domain: *b"arena0/step-commit/v2\0\0\0",
            session_id: SessionHashBytes([0x11; 32]),
            step: 3,
            entry_hash: [0x22; 32],
            pre_state: StateHashBytes([0x33; 32]),
            post_state: StateHashBytes([0x44; 32]),
            link: [0x55; 32],
        }
    }

    fn terminal_commitment() -> WireTerminalCommitment {
        WireTerminalCommitment {
            domain: *b"arena0/terminal/v1\0\0\0\0\0\0",
            session_id: SessionHashBytes([0x11; 32]),
            final_step: 3,
            final_state: StateHashBytes([0x44; 32]),
            outcome_hash: [0x66; 32],
        }
    }

    #[test]
    fn payload_too_large() {
        let message = ExecFrame::Message {
            message_id: MessageIdBytes([0; 32]),
            seq: 0,
            prestate: StateHashBytes([0; 32]),
            data: vec![0u8; 100],
            witness: WitnessCommitmentBytes([0; 32]),
        };
        let result = Codec::new(10).encode(&message);
        assert!(matches!(result, Err(WireError::PayloadTooLarge { .. })));
    }

    #[test]
    fn generic_encode_caps_intermediate_borsh_values() {
        let value = vec![0u8; 64];
        assert!(matches!(
            Codec::new(8).encode(&value),
            Err(WireError::PayloadTooLarge { size, max: 8 }) if size > 8
        ));
    }

    #[test]
    fn stream_header_round_trip() {
        for protocol in [StreamProtocol::Fetch, StreamProtocol::Exec] {
            assert_eq!(
                StreamProtocol::from_header_byte(protocol.header_byte()).unwrap(),
                protocol
            );
        }
        assert_eq!(crate::PROTO_FETCH, 0x01);
        assert!(matches!(
            StreamProtocol::from_header_byte(0xFF),
            Err(WireError::UnknownProtocol(0xFF))
        ));
    }

    #[test]
    fn frames_preserve_borsh_layout_and_round_trip() {
        let step_commitment = step_commitment();
        let terminal_commitment = terminal_commitment();
        let step_signature = BlsSignature([0xCC; 48]);
        let terminal_signature = BlsSignature([0xDD; 48]);
        let frames = [
            (
                ExecFrame::Message {
                    message_id: MessageIdBytes([3; 32]),
                    seq: 4,
                    prestate: StateHashBytes([5; 32]),
                    data: vec![6, 7],
                    witness: WitnessCommitmentBytes([8; 32]),
                },
                DerivedExecFrame::Message {
                    message_id: MessageIdBytes([3; 32]),
                    seq: 4,
                    prestate: StateHashBytes([5; 32]),
                    data: vec![6, 7],
                    witness: WitnessCommitmentBytes([8; 32]),
                },
            ),
            (
                ExecFrame::StepSignature {
                    commitment: step_commitment.clone(),
                    signature: step_signature,
                },
                DerivedExecFrame::StepSignature {
                    commitment: step_commitment,
                    signature: step_signature,
                },
            ),
            (
                ExecFrame::End {
                    commitment: terminal_commitment.clone(),
                    signature: terminal_signature,
                },
                DerivedExecFrame::End {
                    commitment: terminal_commitment,
                    signature: terminal_signature,
                },
            ),
            (
                ExecFrame::Abort {
                    occurrence: abort_occurrence(b"reason"),
                },
                DerivedExecFrame::Abort {
                    occurrence: abort_occurrence(b"reason"),
                },
            ),
        ];
        for (manual, derived) in frames {
            assert_eq!(
                borsh::to_vec(&manual).unwrap(),
                borsh::to_vec(&derived).unwrap()
            );
            let encoded = codec().encode(&manual).unwrap();
            assert_eq!(codec().decode::<ExecFrame>(&encoded).unwrap(), manual);
        }

        let fetch_frames = [
            (
                FetchFrame::ActivationTickets {
                    session_hash: SessionHashBytes([14; 32]),
                    tickets: vec![],
                },
                DerivedFetchFrame::ActivationTickets {
                    session_hash: SessionHashBytes([14; 32]),
                    tickets: vec![],
                },
            ),
            (
                FetchFrame::FetchActivationTickets {
                    session_hash: SessionHashBytes([13; 32]),
                },
                DerivedFetchFrame::FetchActivationTickets {
                    session_hash: SessionHashBytes([13; 32]),
                },
            ),
            (
                FetchFrame::ActivationTickets {
                    session_hash: SessionHashBytes([14; 32]),
                    tickets: vec![vec![15, 16]],
                },
                DerivedFetchFrame::ActivationTickets {
                    session_hash: SessionHashBytes([14; 32]),
                    tickets: vec![vec![15, 16]],
                },
            ),
        ];
        for (manual, derived) in fetch_frames {
            assert_eq!(
                borsh::to_vec(&manual).unwrap(),
                borsh::to_vec(&derived).unwrap()
            );
            let encoded = codec().encode(&manual).unwrap();
            assert_eq!(codec().decode::<FetchFrame>(&encoded).unwrap(), manual);
        }
    }

    #[test]
    fn abort_occurrence_is_canonical_and_bounded() {
        let occurrence = abort_occurrence(b"because");
        let frame = ExecFrame::Abort {
            occurrence: occurrence.clone(),
        };
        let mut body = vec![EXEC_KIND_ABORT];
        body.extend(borsh::to_vec(&occurrence).unwrap());
        let encoded = codec().encode(&frame).unwrap();
        assert_eq!(encoded, raw_frame(&body));
        assert_eq!(codec().decode::<ExecFrame>(&encoded).unwrap(), frame);

        let oversized = ExecFrame::Abort {
            occurrence: WireAbortOccurrence {
                reason: vec![0; MAX_EXEC_REASON_BYTES + 1],
                ..abort_occurrence(b"")
            },
        };
        assert!(matches!(
            codec().encode(&oversized),
            Err(WireError::ValueTooLarge {
                field: "exec.abort.reason",
                ..
            })
        ));
    }

    #[test]
    fn variable_lengths_are_checked_before_allocation() {
        let mut message_body = vec![EXEC_KIND_MESSAGE];
        message_body.extend_from_slice(&[0; 32]);
        message_body.extend_from_slice(&0u64.to_le_bytes());
        message_body.extend_from_slice(&[0; 32]);
        message_body.extend_from_slice(&u32::MAX.to_le_bytes());
        let message_frame = raw_frame(&message_body);
        assert!(matches!(
            codec().decode::<ExecFrame>(&message_frame),
            Err(WireError::ValueTooLarge {
                field: "exec.data",
                ..
            })
        ));

        let mut fetch_body = vec![1];
        fetch_body.extend_from_slice(&[0; 32]);
        fetch_body.extend_from_slice(&1u32.to_le_bytes());
        fetch_body.extend_from_slice(&u32::MAX.to_le_bytes());
        let fetch_frame = raw_frame(&fetch_body);
        assert!(matches!(
            codec().decode::<FetchFrame>(&fetch_frame),
            Err(WireError::ValueTooLarge {
                field: "fetch.ticket",
                ..
            })
        ));

        let unknown_frame = raw_frame(&[0xff]);
        assert!(matches!(
            codec().decode::<ExecFrame>(&unknown_frame),
            Err(WireError::Decode(_))
        ));
    }

    #[test]
    fn frame_envelope_is_versioned_and_exact_length() {
        let encoded = Codec::new(1).encode(&7u8).unwrap();
        assert_eq!(encoded, [3, 0, 0, 0, 1, 0, 7]);
        assert_eq!(Codec::new(1).decode_frame(&encoded).unwrap(), &[7]);

        for (bytes, error) in [
            (
                vec![0, 0, 0],
                WireError::FrameTooShort {
                    expected: 4,
                    actual: 3,
                },
            ),
            (
                vec![3, 0, 0, 0, 1],
                WireError::Truncated {
                    declared: 3,
                    actual: 1,
                },
            ),
            (
                vec![1, 0, 0, 0, 0],
                WireError::FrameTooShort {
                    expected: 6,
                    actual: 5,
                },
            ),
            (
                vec![3, 0, 0, 0, 1, 0, 7, 8],
                WireError::TrailingBytes {
                    expected: 7,
                    actual: 8,
                },
            ),
            (
                vec![3, 0, 0, 0, 255, 255, 7],
                WireError::UnsupportedVersion {
                    expected: 1,
                    actual: u16::MAX,
                },
            ),
            (
                vec![4, 0, 0, 0],
                WireError::PayloadTooLarge { size: 2, max: 1 },
            ),
        ] {
            assert_eq!(
                Codec::new(1).decode_frame(&bytes).unwrap_err(),
                error,
                "{bytes:?}"
            );
        }
    }

    #[test]
    fn protocol_frame_body_caps_are_canonical() {
        assert_eq!(
            StreamProtocol::Fetch.max_frame_body(),
            MAX_FETCH_RESPONSE_BYTES
        );
        assert_eq!(
            StreamProtocol::Exec.max_frame_body(),
            DEFAULT_MAX_MESSAGE_SIZE
        );
    }
}
