//! Errors raised while parsing or encoding arena0 wire values.

use thiserror::Error;

/// A canonical wire representation could not be encoded or decoded.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WireError {
    /// The value could not be serialized into its canonical Borsh body.
    #[error("wire value encoding failed: {0}")]
    Encode(String),
    /// The frame body could not be deserialized as the expected value.
    #[error("wire value decoding failed: {0}")]
    Decode(String),
    /// The frame contains a variable-length field larger than its semantic bound.
    #[error("wire field {field} is too large: {size} bytes (max {max})")]
    ValueTooLarge {
        /// The field whose bound was exceeded.
        field: &'static str,
        /// The received or attempted size.
        size: usize,
        /// The maximum accepted size.
        max: usize,
    },
    /// A bounded collection contains too many elements.
    #[error("wire collection {field} is too large: {size} items (max {max})")]
    CollectionTooLarge {
        /// The collection whose bound was exceeded.
        field: &'static str,
        /// The received or attempted element count.
        size: usize,
        /// The maximum accepted element count.
        max: usize,
    },
    /// The received bytes do not contain a complete four-byte frame header.
    #[error("wire frame is too short: expected at least {expected} bytes, got {actual}")]
    FrameTooShort {
        /// Minimum frame size.
        expected: usize,
        /// Actual received size.
        actual: usize,
    },
    /// The declared frame body is larger than the codec's configured bound.
    #[error("wire frame body is too large: {size} bytes (max {max})")]
    PayloadTooLarge {
        /// Declared or encoded body size.
        size: usize,
        /// Codec body limit.
        max: usize,
    },
    /// The declared frame body is not fully present in the received bytes.
    #[error("wire frame is truncated: declared {declared} body bytes, got {actual}")]
    Truncated {
        /// Declared body size.
        declared: usize,
        /// Available body size.
        actual: usize,
    },
    /// Bytes after the declared frame body are not accepted as another frame.
    #[error("wire frame has trailing bytes: expected {expected} bytes, got {actual}")]
    TrailingBytes {
        /// Exact frame size implied by the header.
        expected: usize,
        /// Actual received size.
        actual: usize,
    },
    /// The stream discriminator is not one of the supported protocols.
    #[error("unknown stream protocol byte: 0x{0:02x}")]
    UnknownProtocol(u8),
    /// The frame version is not supported by this codec.
    #[error("unsupported wire version: expected {expected}, got {actual}")]
    UnsupportedVersion {
        /// Version understood by this crate.
        expected: u16,
        /// Version carried by the received frame.
        actual: u16,
    },
}
