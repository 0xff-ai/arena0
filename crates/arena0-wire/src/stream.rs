//! Stream-level wire discriminators.

use crate::{PROTO_EXEC, PROTO_FETCH, WireError};

/// Which multiplexed protocol a stream carries.
///
/// The discriminator is written once at stream open and is not repeated in
/// every length-prefixed frame. Each frame still carries
/// [`crate::FRAME_VERSION`] and its typed message-kind discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamProtocol {
    /// Fetch stream (`0x01`) for bounded activation-ticket convergence.
    Fetch,
    /// Exec stream (`0x02`) for committed-session traffic.
    Exec,
}

impl StreamProtocol {
    /// The one-time wire discriminator byte written at stream open.
    #[must_use]
    pub const fn header_byte(self) -> u8 {
        match self {
            Self::Fetch => PROTO_FETCH,
            Self::Exec => PROTO_EXEC,
        }
    }

    /// Parse the one-time discriminator byte read from a new stream.
    pub fn from_header_byte(byte: u8) -> Result<Self, WireError> {
        match byte {
            PROTO_FETCH => Ok(Self::Fetch),
            PROTO_EXEC => Ok(Self::Exec),
            unknown => Err(WireError::UnknownProtocol(unknown)),
        }
    }

    /// Maximum body size for one length-prefixed frame on this protocol.
    #[must_use]
    pub const fn max_frame_body(self) -> usize {
        match self {
            Self::Fetch => super::fetch::MAX_FETCH_RESPONSE_BYTES,
            Self::Exec => super::DEFAULT_MAX_MESSAGE_SIZE,
        }
    }
}
