//! Internal helpers for program-domain 32-byte identifiers.

use std::fmt;

/// Failure to parse a program-domain 32-byte identifier from hexadecimal.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum IdParseError {
    /// The input contains a non-hexadecimal character.
    #[error("invalid hex: {0}")]
    InvalidHex(#[from] hex::FromHexError),
    /// The decoded value does not contain exactly 32 bytes.
    #[error("expected 32 bytes (64 hex chars), got {actual}")]
    WrongLength { expected: usize, actual: usize },
}

pub(crate) fn parse_id_hex(s: &str) -> Result<[u8; 32], IdParseError> {
    if let Some((index, byte)) = s
        .bytes()
        .enumerate()
        .find(|(_, byte)| !matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(IdParseError::InvalidHex(
            hex::FromHexError::InvalidHexCharacter {
                c: char::from(byte),
                index,
            },
        ));
    }
    let bytes = hex::decode(s).map_err(IdParseError::InvalidHex)?;
    if bytes.len() != 32 {
        return Err(IdParseError::WrongLength {
            expected: 32,
            actual: bytes.len(),
        });
    }
    let mut value = [0; 32];
    value.copy_from_slice(&bytes);
    Ok(value)
}

/// Short hexadecimal display used by program-domain identifiers.
pub(crate) struct HexShort<'a>(pub &'a [u8]);

impl fmt::Display for HexShort<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0[..4] {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
