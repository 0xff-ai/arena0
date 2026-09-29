//! Outcome of the Host's guest-signature check.
//!
//! Guest-visible: programs receive it from `ctx.verify` across the ABI, and
//! the Host's `GuestSignData::verify` returns it.

use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

/// Why a signed guest envelope was not accepted.
///
/// The Borsh tags are part of the ABI: the `verify` import writes a Borsh
/// `Result<Vec<u8>, VerifyError>`.
#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    thiserror::Error,
)]
pub enum VerifyError {
    /// Not a guest-signing preimage: undecodable, wrong domain or version, or
    /// over its bound.
    #[error("not a guest signing preimage")]
    Malformed,
    /// Signed in another session or for another program.
    #[error("signed for another session or program")]
    OtherSession,
    /// The named signer is not a participant of this session.
    #[error("the signer is not a participant")]
    NotParticipant,
    /// The signature does not verify under the signer's key.
    #[error("the signature does not verify")]
    BadSignature,
}
