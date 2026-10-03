//! Canonical lowercase text for arena0's 32-byte domain identifiers.
use std::fmt;
/// Failure to parse a 32-byte identifier; the text representation is lowercase only.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum IdParseError {
    /// The input contains a non-hexadecimal character or has an odd digit count.
    #[error("invalid hex: {0}")]
    InvalidHex(#[from] hex::FromHexError),
    /// The decoded value has a different byte width.
    #[error("expected {expected} bytes ({} hex chars), got {actual}", expected * 2)]
    WrongLength { expected: usize, actual: usize },
}
/// Parse the canonical lowercase representation, rejecting invalid characters
/// before decoding and requiring exactly 32 bytes. Error indices count UTF-8 bytes.
pub fn parse_id_hex(s: &str) -> Result<[u8; 32], IdParseError> {
    let _ = s;
    todo!("STUB(protocol)")
}
/// Display the first four bytes of a complete identifier for human diagnostics.
/// A short label is not a content reference and must not be parsed as one.
#[derive(Debug)]
pub struct HexShort<'a>(pub &'a [u8; 32]);
impl fmt::Display for HexShort<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = f;
        todo!("STUB(protocol)")
    }
}
