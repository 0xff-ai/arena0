//! Typed input for resident dispatch and the shared input encoder.

use arena0_program::DispatchInput;
use arena0_protocol::{Committed, Ensemble, Event, PeerId};
use borsh::BorshSerialize;
use std::sync::Arc;

use crate::GuestSigner;

/// Whether a dispatch event is an agreed event or a local event.
///
/// The distinction drives the sandbox's emission rules: only agreed events may
/// emit a lifecycle effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchKind {
    Agreed,
    Local,
}

/// Decoded inputs for one resident dispatch: the ABI envelope, the dispatch
/// kind the guest sees, the committed outgoing length, and the per-dispatch
/// signer.
type DispatchParts = (
    DispatchInput,
    DispatchKind,
    usize,
    Option<Arc<dyn GuestSigner>>,
);

/// One event dispatched through the resident Wasm instance.
#[derive(Clone)]
pub struct DispatchCall {
    pub(crate) peer_id: PeerId,
    pub(crate) session: Ensemble<Committed>,
    pub(crate) event: Event<Vec<u8>>,
    pub(crate) outgoing_len: usize,
    pub(crate) signer: Option<Arc<dyn GuestSigner>>,
}

impl std::fmt::Debug for DispatchCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DispatchCall")
            .field("peer_id", &self.peer_id)
            .field("session", &self.session)
            .field("event", &self.event)
            .field("signer", &self.signer.is_some())
            .finish()
    }
}

impl DispatchCall {
    /// Construct a dispatch call with a committed session context and flat
    /// protocol event. State is held in the resident instance, not this value.
    #[must_use]
    pub fn new(peer_id: PeerId, session: Ensemble<Committed>, event: Event<Vec<u8>>) -> Self {
        Self {
            peer_id,
            session,
            event,
            outgoing_len: 0,
            signer: None,
        }
    }

    /// Supply the number of messages already committed to the outgoing queue.
    ///
    /// The broadcast import adds the broadcasts queued in this dispatch and
    /// rejects the call once the total reaches the queue bound.
    #[must_use]
    pub fn with_outgoing_len(mut self, outgoing_len: usize) -> Self {
        self.outgoing_len = outgoing_len;
        self
    }

    /// Install the signer exposed to this dispatch's synchronous `sign` calls.
    ///
    /// Only local handler events may carry a signer; a dispatch without one
    /// traps when the guest calls `sign`.
    #[must_use]
    pub fn with_signer(mut self, signer: Arc<dyn GuestSigner>) -> Self {
        self.signer = Some(signer);
        self
    }

    pub(crate) fn into_input(self) -> Result<DispatchParts, crate::SandboxError> {
        let Self {
            peer_id,
            session,
            event,
            outgoing_len,
            signer,
        } = self;
        let session_bytes = serialize(&session)?;
        let event_bytes = serialize(&event)?;
        let input = DispatchInput::try_new(peer_id.0, session_bytes, event_bytes)
            .map_err(|error| crate::SandboxError::input_limit(error.to_string()))?;
        let dispatch = match event {
            Event::SessionStarted { .. } | Event::MessageReceived { .. } => DispatchKind::Agreed,
            Event::InputReceived { .. } | Event::TimerFired { .. } => DispatchKind::Local,
        };
        Ok((input, dispatch, outgoing_len, signer))
    }
}

// Encode one call input. Envelope size is enforced once per direction by the
// resident and fresh execution paths against the profile limit, which owns
// it; this helper only serializes.
pub(crate) fn serialize<T: BorshSerialize>(value: &T) -> Result<Vec<u8>, crate::SandboxError> {
    borsh::to_vec(value)
        .map_err(|error| crate::SandboxError::SerializationFailed(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_start_input_carries_the_committed_session_context() {
        let peer = PeerId([1; 32]);
        let session = Ensemble::from_peers(vec![peer, PeerId([2; 32])]).unwrap();
        let input = DispatchCall::new(
            peer,
            session.clone(),
            Event::SessionStarted { ensemble: session },
        )
        .into_input()
        .unwrap()
        .0;
        assert_eq!(input.peer_id, [1; 32]);
        assert!(borsh::from_slice::<Ensemble<Committed>>(&input.session).is_ok());
    }
}
