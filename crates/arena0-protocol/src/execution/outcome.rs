//! The two guest-owned terminal outcome projections.

use std::io;

use arena0_program::{JsonBytes, MAX_CALL_PAYLOAD_BYTES};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use super::{MAX_TERMINAL_OUTCOME_BYTES, ProtocolError};

/// The canonical Borsh outcome and its agent-facing JSON projection produced
/// by one fresh guest outcome call.
///
/// The protocol never interprets either representation. It validates that
/// both are bounded and that the JSON is one complete value; the Borsh bytes
/// are compared byte-for-byte with the `SessionEnd` effect before terminal
/// evidence can be collected.
#[derive(BorshSerialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TerminalOutcome {
    borsh: Vec<u8>,
    json: Vec<u8>,
}

impl TerminalOutcome {
    /// Construct a bounded pair of guest-produced outcome projections.
    pub fn new(borsh: impl Into<Vec<u8>>, json: impl Into<Vec<u8>>) -> Result<Self, ProtocolError> {
        let borsh = borsh.into();
        let json = json.into();
        validate_borsh(&borsh)?;
        validate_json(&json)?;
        Ok(Self { borsh, json })
    }

    /// Borrow the stock-Borsh outcome bytes.
    #[must_use]
    pub fn borsh(&self) -> &[u8] {
        &self.borsh
    }

    /// Borrow the agent-facing JSON projection bytes.
    #[must_use]
    pub fn json(&self) -> &[u8] {
        &self.json
    }

    pub(crate) fn validate(&self) -> Result<(), ProtocolError> {
        validate_borsh(&self.borsh)?;
        validate_json(&self.json)
    }
}

impl BorshDeserialize for TerminalOutcome {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let borsh =
            crate::bounded::read_bytes(reader, MAX_TERMINAL_OUTCOME_BYTES, "Borsh outcome")?;
        let json = crate::bounded::read_bytes(reader, MAX_TERMINAL_OUTCOME_BYTES, "JSON outcome")?;
        Self::new(borsh, json).map_err(to_io_error)
    }
}

fn validate_borsh(bytes: &[u8]) -> Result<(), ProtocolError> {
    if bytes.len() > MAX_TERMINAL_OUTCOME_BYTES || bytes.len() > MAX_CALL_PAYLOAD_BYTES {
        return Err(ProtocolError::PayloadTooLarge {
            kind: "Borsh outcome",
            actual: bytes.len(),
            max: MAX_TERMINAL_OUTCOME_BYTES.min(MAX_CALL_PAYLOAD_BYTES),
        });
    }
    // Empty outcomes are valid opaque Borsh values (for example `()`), so no
    // schema-dependent decoding belongs here.
    Ok(())
}

fn validate_json(bytes: &[u8]) -> Result<(), ProtocolError> {
    if bytes.len() > MAX_TERMINAL_OUTCOME_BYTES || bytes.len() > MAX_CALL_PAYLOAD_BYTES {
        return Err(ProtocolError::PayloadTooLarge {
            kind: "JSON outcome",
            actual: bytes.len(),
            max: MAX_TERMINAL_OUTCOME_BYTES.min(MAX_CALL_PAYLOAD_BYTES),
        });
    }
    JsonBytes::try_new(bytes.to_vec())
        .map(|_| ())
        .map_err(|error| ProtocolError::InvalidOutcomeProjection(error.to_string()))
}

fn to_io_error(error: ProtocolError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_codec_keeps_opaque_empty_values_and_rejects_oversized_prefixes() {
        let outcome = TerminalOutcome::new(Vec::new(), b"null".to_vec()).unwrap();
        let encoded = [0, 0, 0, 0, 4, 0, 0, 0, b'n', b'u', b'l', b'l'];
        assert_eq!(borsh::to_vec(&outcome).unwrap(), encoded);
        assert_eq!(TerminalOutcome::try_from_slice(&encoded).unwrap(), outcome);
        assert!(TerminalOutcome::new(vec![0; MAX_TERMINAL_OUTCOME_BYTES], b"null").is_ok());
        assert!(matches!(
            TerminalOutcome::new(vec![0; MAX_TERMINAL_OUTCOME_BYTES + 1], b"null"),
            Err(ProtocolError::PayloadTooLarge {
                kind: "Borsh outcome",
                ..
            })
        ));

        let oversized = u32::try_from(MAX_TERMINAL_OUTCOME_BYTES + 1)
            .unwrap()
            .to_le_bytes();
        // A bound failure must precede trying to read the absent payload.
        for prefix in [oversized.to_vec(), [0u32.to_le_bytes(), oversized].concat()] {
            assert_eq!(
                TerminalOutcome::try_from_slice(&prefix).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
            );
        }
        assert!(matches!(
            TerminalOutcome::new(Vec::new(), b"null true"),
            Err(ProtocolError::InvalidOutcomeProjection(_))
        ));
    }
}
