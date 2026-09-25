//! Typed inputs for loaded-program invocations.
//!
//! A loaded program owns one non-cloneable resident instance per actor for
//! dispatches. Read-only projections continue to use fresh instances.

use arena0_program::{
    DispatchInput, InitInput, JsonBytes, OutcomeInput, QueryInput, SharedStateBytes, ViewInput,
    WriterInput,
};
use arena0_protocol::{Committed, Ensemble, Event, PeerId};
use borsh::BorshSerialize;
use std::sync::Arc;

use crate::GuestSigner;

/// Initialize a fresh program's shared and local state.
#[derive(Debug, Clone)]
pub struct InitializeCall {
    pub(crate) params: JsonBytes,
}

impl InitializeCall {
    /// Construct an initialization call from the agent-facing parameter JSON.
    #[must_use]
    pub fn new(params: JsonBytes) -> Self {
        Self { params }
    }

    pub(crate) fn into_input(self) -> Result<InitInput, crate::SandboxError> {
        InitInput::try_new(self.params.into_bytes())
            .map_err(|error| crate::SandboxError::input_limit(error.to_string()))
    }
}

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

/// Execute one read-only query, whose request is validated agent-facing JSON.
#[derive(Debug, Clone)]
pub struct QueryCall {
    pub(crate) shared: SharedStateBytes,
    pub(crate) query: JsonBytes,
    /// Exact advertised query schema selected by the caller.
    pub(crate) query_index: u32,
    pub(crate) session: Ensemble<Committed>,
}

impl QueryCall {
    /// Construct a query from agent-facing JSON bytes.
    #[must_use]
    pub fn new(
        shared: SharedStateBytes,
        session: Ensemble<Committed>,
        query: JsonBytes,
        query_index: u32,
    ) -> Self {
        Self {
            shared,
            query,
            query_index,
            session,
        }
    }

    pub(crate) fn into_input(self) -> Result<QueryInput, crate::SandboxError> {
        let session = serialize(&self.session)?;
        QueryInput::try_new(
            self.shared,
            session,
            self.query_index,
            self.query.into_bytes(),
        )
        .map_err(|error| crate::SandboxError::input_limit(error.to_string()))
    }
}

/// Execute one read-only viewport projection, whose request is validated
/// agent-facing JSON.
#[derive(Debug, Clone)]
pub struct ViewCall {
    pub(crate) shared: SharedStateBytes,
    pub(crate) viewport: JsonBytes,
    pub(crate) session: Ensemble<Committed>,
}

impl ViewCall {
    /// Construct a viewport projection call from validated JSON bytes.
    #[must_use]
    pub fn new(
        shared: SharedStateBytes,
        session: Ensemble<Committed>,
        viewport: JsonBytes,
    ) -> Self {
        Self {
            shared,
            viewport,
            session,
        }
    }

    pub(crate) fn into_input(self) -> Result<ViewInput, crate::SandboxError> {
        let session = serialize(&self.session)?;
        ViewInput::try_new(self.shared, session, self.viewport.into_bytes())
            .map_err(|error| crate::SandboxError::input_limit(error.to_string()))
    }
}

/// Execute the pure terminal-outcome projection against explicit state bytes.
#[derive(Debug, Clone)]
pub struct OutcomeCall {
    pub(crate) shared: SharedStateBytes,
    pub(crate) session: Ensemble<Committed>,
}

/// Execute the pure next-writer projection against explicit shared state.
#[derive(Debug, Clone)]
pub struct WriterCall {
    pub(crate) shared: SharedStateBytes,
    pub(crate) session: Ensemble<Committed>,
}

impl WriterCall {
    /// Construct a next-writer projection call.
    #[must_use]
    pub fn new(shared: SharedStateBytes, session: Ensemble<Committed>) -> Self {
        Self { shared, session }
    }

    pub(crate) fn into_input(self) -> WriterInput {
        WriterInput {
            shared: self.shared,
        }
    }
}

impl OutcomeCall {
    /// Construct an outcome projection call.
    #[must_use]
    pub fn new(shared: SharedStateBytes, session: Ensemble<Committed>) -> Self {
        Self { shared, session }
    }

    pub(crate) fn into_input(self) -> Result<OutcomeInput, crate::SandboxError> {
        let session = serialize(&self.session)?;
        OutcomeInput::try_new(self.shared, session)
            .map_err(|error| crate::SandboxError::input_limit(error.to_string()))
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
