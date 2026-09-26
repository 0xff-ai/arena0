//! Content-addressed message identity.
//!
//! A [`crate::MessageId`] binds the authenticated session sender, agreed
//! position, shared pre-state and post-state hashes, and opaque program
//! payload. The advertised post-state is part of the identity so a payload
//! cannot be replayed with a different consensus result.

use crate::id::id_type;
use crate::{PeerId, SessionHash, StateHash};

/// Domain separation tag for message-id derivation.
pub const MESSAGE_ID_DOMAIN: [u8; 24] = *b"arena0/message/v2\0\0\0\0\0\0\0";
const _: () = assert!(MESSAGE_ID_DOMAIN.len() == 24);

id_type!(
    /// Content address of one authenticated program-message envelope.
    pub struct Id
);

impl Id {
    /// Derive the message id from the canonical authenticated envelope.
    #[must_use]
    pub fn derive(
        session_id: SessionHash,
        from: PeerId,
        position: u64,
        pre_state: StateHash,
        post_state: StateHash,
        data: &[u8],
    ) -> Self {
        let bytes = borsh::to_vec(&(
            MESSAGE_ID_DOMAIN,
            session_id,
            from,
            position,
            pre_state,
            post_state,
            data,
        ))
        .expect("message-id preimage is serializable");
        Self(*blake3::hash(&bytes).as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (SessionHash, PeerId, StateHash, StateHash) {
        (
            SessionHash([1u8; 32]),
            PeerId([2u8; 32]),
            StateHash([3u8; 32]),
            StateHash([4u8; 32]),
        )
    }

    #[test]
    fn derive_distinguishes_every_field() {
        let (session, from, pre, post) = sample();
        let base = Id::derive(session, from, 7, pre, post, b"payload");
        let cases = [
            Id::derive(SessionHash([9; 32]), from, 7, pre, post, b"payload"),
            Id::derive(session, PeerId([9; 32]), 7, pre, post, b"payload"),
            Id::derive(session, from, 8, pre, post, b"payload"),
            Id::derive(session, from, 7, StateHash([9; 32]), post, b"payload"),
            Id::derive(session, from, 7, pre, StateHash([9; 32]), b"payload"),
            Id::derive(session, from, 7, pre, post, b"other"),
        ];
        for case in cases {
            assert_ne!(base, case);
        }
    }
}
