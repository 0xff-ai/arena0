//! Content-addressed message identity.
//!
//! A [`crate::MessageId`] is the blake3 fingerprint of one authenticated program-message
//! envelope: the session, the authenticated sender, the global trace position the
//! message was computed against (`position`), the declared pre-state hash, the opaque
//! program payload, and the sender's witness commitment. Every field is either
//! public or authenticated by the transport, so a receiver recomputes the id from
//! what it actually knows and treats a mismatch as protocol abuse.

use crate::id::id_type;
use crate::{PeerId, SessionHash, StateHash, WitnessCommitment};

/// Domain separation tag for message-id derivation.
pub const MESSAGE_ID_DOMAIN: [u8; 24] = *b"arena0/message/v1\0\0\0\0\0\0\0";
const _: () = assert!(MESSAGE_ID_DOMAIN.len() == 24);

id_type!(
    /// Content address of one authenticated program-message envelope.
    pub struct Id
);

impl Id {
    /// Derive the message id from the canonical envelope preimage.
    ///
    /// `from` must be the transport-authenticated sender (the stream peer), never
    /// a sender-claimed identity. `position` is the global trace position the message
    /// was computed against; `pre_state` is the state hash at that position.
    #[must_use]
    pub fn derive(
        session_id: SessionHash,
        from: PeerId,
        position: u64,
        pre_state: StateHash,
        data: &[u8],
        witness: WitnessCommitment,
    ) -> Self {
        let bytes = borsh::to_vec(&(
            MESSAGE_ID_DOMAIN,
            session_id,
            from,
            position,
            pre_state,
            data,
            witness,
        ))
        .expect("message-id preimage is serializable");

        Self(*blake3::hash(&bytes).as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (SessionHash, PeerId, StateHash, WitnessCommitment) {
        (
            SessionHash([1u8; 32]),
            PeerId([2u8; 32]),
            StateHash([3u8; 32]),
            WitnessCommitment([4u8; 32]),
        )
    }

    #[test]
    fn derive_distinguishes_every_field() {
        let (session, from, pre, witness) = sample();
        let base = Id::derive(session, from, 7, pre, b"payload", witness);
        let cases = [
            Id::derive(SessionHash([9u8; 32]), from, 7, pre, b"payload", witness),
            Id::derive(session, PeerId([9u8; 32]), 7, pre, b"payload", witness),
            Id::derive(session, from, 8, pre, b"payload", witness),
            Id::derive(session, from, 7, StateHash([9u8; 32]), b"payload", witness),
            Id::derive(session, from, 7, pre, b"other", witness),
            Id::derive(
                session,
                from,
                7,
                pre,
                b"payload",
                WitnessCommitment([9u8; 32]),
            ),
        ];
        for case in cases {
            assert_ne!(base, case);
        }
    }
}
