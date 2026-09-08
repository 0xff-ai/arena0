//! Typed inputs for fresh guest invocations.
//!
//! These values deliberately separate shared, local, and read-only calls.
//! Constructing a call does not allocate a Wasmtime instance. The admitted
//! program creates and destroys one instance when the call is executed.

use arena0_program::{
    InitInput, JsonBytes, LocalInput, LocalStateBytes, MAX_CALL_ENVELOPE_BYTES, MAX_HOST_BYTES,
    MAX_RANDOM_DRAW_BYTES, MAX_RANDOM_DRAWS, OutcomeInput, QueryInput, SharedInput,
    SharedStateBytes, ViewInput, WriterInput,
};
use arena0_protocol::{Committed, Ensemble, Event, PeerId};
use borsh::BorshSerialize;

/// Shared event surface. A local event cannot be constructed for a shared call.
#[derive(Debug, Clone)]
pub enum SharedEvent {
    /// A public message at its canonical position.
    MessageReceived {
        message_id: arena0_protocol::MessageId,
        from: PeerId,
        position: u64,
        pre_state: arena0_protocol::StateHash,
        msg: Vec<u8>,
    },
}

/// Local event surface. A shared event cannot be constructed for a local call.
#[derive(Debug, Clone)]
pub enum LocalEvent {
    /// An answer to a pending callout, encoded as validated agent-facing JSON.
    InputReceived {
        callout_index: u32,
        data: JsonBytes,
        continuation_tag: Option<u32>,
    },
    /// An untyped timer fired.
    TimerFired,
    /// A typed timer fired.
    TypedTimerFired {
        timer: arena0_protocol::TimerPayload,
    },
    /// A signing operation completed.
    Signed {
        signature: Vec<u8>,
        continuation_tag: Option<u32>,
    },
    /// Run local decision code after a public entry.
    React,
}

impl SharedEvent {
    pub(crate) fn into_protocol(self) -> Event<Vec<u8>> {
        match self {
            Self::MessageReceived {
                message_id,
                from,
                position,
                pre_state,
                msg,
            } => Event::MessageReceived {
                message_id,
                from,
                position,
                pre_state,
                msg,
            },
        }
    }
}

impl LocalEvent {
    pub(crate) fn into_protocol(self) -> Event<Vec<u8>> {
        match self {
            Self::InputReceived {
                callout_index,
                data,
                continuation_tag,
            } => Event::InputReceived {
                callout_index,
                data: data.into_bytes(),
                continuation_tag,
            },
            Self::TimerFired => Event::TimerFired,
            Self::TypedTimerFired { timer } => Event::TypedTimerFired { timer },
            Self::Signed {
                signature,
                continuation_tag,
            } => Event::Signed {
                signature,
                continuation_tag,
            },
            Self::React => Event::React,
        }
    }
}

/// Replay evidence supplied to a local call.
#[derive(Debug, Clone, Default)]
pub struct RandomReplay(Vec<Vec<u8>>);

/// Failure to construct bounded replay evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RandomReplayError {
    /// The replay contained too many draws.
    #[error("random replay contains {actual} draws; maximum is {max}")]
    TooManyDraws { actual: usize, max: u32 },
    /// One draw exceeded the per-draw bound.
    #[error("random replay draw is {actual} bytes; maximum is {max}")]
    DrawTooLarge { actual: usize, max: u64 },
    /// All replay bytes exceeded the per-call host budget.
    #[error("random replay contains {actual} bytes; maximum is {max}")]
    TooManyBytes { actual: usize, max: u32 },
}

impl RandomReplay {
    /// Use recorded draws as the guest's deterministic entropy source.
    pub fn new(draws: Vec<Vec<u8>>) -> Result<Self, RandomReplayError> {
        if draws.len() > MAX_RANDOM_DRAWS as usize {
            return Err(RandomReplayError::TooManyDraws {
                actual: draws.len(),
                max: MAX_RANDOM_DRAWS,
            });
        }
        let mut total = 0usize;
        for draw in &draws {
            if draw.len() as u64 > MAX_RANDOM_DRAW_BYTES {
                return Err(RandomReplayError::DrawTooLarge {
                    actual: draw.len(),
                    max: MAX_RANDOM_DRAW_BYTES,
                });
            }
            total = total
                .checked_add(draw.len())
                .ok_or(RandomReplayError::TooManyBytes {
                    actual: usize::MAX,
                    max: MAX_HOST_BYTES,
                })?;
        }
        if total > MAX_HOST_BYTES as usize {
            return Err(RandomReplayError::TooManyBytes {
                actual: total,
                max: MAX_HOST_BYTES,
            });
        }
        Ok(Self(draws))
    }

    pub(crate) fn as_slice(&self) -> &[Vec<u8>] {
        &self.0
    }
}

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

/// Apply one shared/public event against explicit state bytes.
#[derive(Debug, Clone)]
pub enum SharedCall {
    /// The session-start boundary, whose ensemble is the sole session source.
    SessionStarted {
        shared: SharedStateBytes,
        ensemble: Ensemble<Committed>,
    },
    /// A later shared event, requiring the already committed ensemble.
    Event {
        shared: SharedStateBytes,
        session: Ensemble<Committed>,
        event: SharedEvent,
    },
}

impl SharedCall {
    /// Construct a shared event call.
    #[must_use]
    pub fn new(shared: SharedStateBytes, session: Ensemble<Committed>, event: SharedEvent) -> Self {
        Self::Event {
            shared,
            session,
            event,
        }
    }

    /// Construct the session-start boundary. The event's ensemble is the sole
    /// source of session context for this call.
    #[must_use]
    pub fn session_started(shared: SharedStateBytes, ensemble: Ensemble<Committed>) -> Self {
        Self::SessionStarted { shared, ensemble }
    }
}

/// Apply one local/private event against explicit state bytes.
#[derive(Debug, Clone)]
pub struct LocalCall {
    pub(crate) peer_id: PeerId,
    pub(crate) shared: SharedStateBytes,
    pub(crate) local: LocalStateBytes,
    pub(crate) event: LocalEvent,
    pub(crate) session: Ensemble<Committed>,
    pub(crate) random_replay: Option<RandomReplay>,
}

impl LocalCall {
    /// Construct a local event call.
    #[must_use]
    pub fn new(
        peer_id: PeerId,
        shared: SharedStateBytes,
        local: LocalStateBytes,
        session: Ensemble<Committed>,
        event: LocalEvent,
    ) -> Self {
        Self {
            peer_id,
            shared,
            local,
            event,
            session,
            random_replay: None,
        }
    }

    /// Replay recorded entropy during this local call.
    #[must_use]
    pub fn with_random_replay(mut self, replay: RandomReplay) -> Self {
        self.random_replay = Some(replay);
        self
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

pub(crate) fn shared_input(
    shared: SharedStateBytes,
    event: Vec<u8>,
    session: Option<Ensemble<Committed>>,
) -> Result<SharedInput, crate::SandboxError> {
    let session = serialize(&session)?;
    SharedInput::try_new(shared, session, event)
        .map_err(|error| crate::SandboxError::input_limit(error.to_string()))
}

impl LocalCall {
    pub(crate) fn into_input(
        self,
    ) -> Result<(LocalInput, SharedStateBytes, Option<RandomReplay>), crate::SandboxError> {
        let Self {
            peer_id,
            shared,
            local,
            event,
            session,
            random_replay,
        } = self;
        let event = serialize(&event.into_protocol())?;
        let session = serialize(&session)?;
        // ponytail: clone only for the input; return the original shared state.
        let input = LocalInput::try_new(peer_id.0, shared.clone(), local, session, event)
            .map_err(|error| crate::SandboxError::input_limit(error.to_string()))?;
        Ok((input, shared, random_replay))
    }
}

// ponytail: one bounded path keeps call-byte limits and faults consistent.
pub(crate) fn serialize<T: BorshSerialize>(value: &T) -> Result<Vec<u8>, crate::SandboxError> {
    let bytes = borsh::to_vec(value)
        .map_err(|error| crate::SandboxError::SerializationFailed(error.to_string()))?;
    if bytes.len() > MAX_CALL_ENVELOPE_BYTES as usize {
        return Err(crate::SandboxError::input_limit(format!(
            "ABI envelope is {} bytes; maximum is {}",
            bytes.len(),
            MAX_CALL_ENVELOPE_BYTES
        )));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_start_input_has_no_duplicate_session_context() {
        let shared = SharedStateBytes::try_new(Vec::new()).unwrap();
        let input = shared_input(shared, Vec::new(), None).unwrap();
        let session: Option<Ensemble<Committed>> = borsh::from_slice(&input.session).unwrap();
        assert!(session.is_none());
    }

    #[test]
    fn random_replay_is_bounded_at_construction() {
        let replay = RandomReplay::new(vec![vec![0; MAX_RANDOM_DRAW_BYTES as usize + 1]]);
        assert!(replay.is_err());
    }
}
