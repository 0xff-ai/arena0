//! Reproduce a session's shared state at a past agreed step.
//!
//! Only agreed steps change shared state: local events write back the shared
//! bytes they were given, and every participant attests each step's
//! `pre_state` and `post_state`. Replaying the agreed events `0..=k` in a fresh
//! instance therefore reproduces the state after step `k` on any Host, without
//! the Host having stored it.

use arena0_program::{CallStatus, SharedStateBytes};
use arena0_protocol::{ExecutionBinding, PeerId, StateHash, TraceEntry};
use arena0_sandbox::{DispatchCall, LoadedProgram, SandboxError};

use super::guest::DispatchVerifier;

/// Reproduce the shared state after the last entry of `steps` by replaying
/// them in a fresh instance of `program`.
///
/// The offer params, the committed ensemble and the initial state come from
/// `binding`. `steps` must be the session's agreed steps `0..=k` in order.
/// Each step's event is dispatched exactly as every participant dispatched it,
/// and each resulting state hash must equal the entry's `post_state`; the
/// first mismatch is reported, never skipped.
///
/// Only the shared state is reproduced. The replay's local state, effects and
/// callouts are discarded.
pub fn replay_shared(
    program: &LoadedProgram,
    binding: &ExecutionBinding,
    local_peer: PeerId,
    steps: &[TraceEntry],
) -> Result<SharedStateBytes, ReplayError> {
    let offer = binding.activation().offer().data();
    let ensemble = binding
        .ensemble()
        .expect("validated activation has a committed ensemble");
    let initialized = program
        .initialize(offer.params.clone())
        .map_err(|source| ReplayError::Initialize { source })?;
    let actual = StateHash::of_shared(&initialized.shared);
    if actual != offer.initial_state {
        return Err(ReplayError::InitialState {
            expected: offer.initial_state,
            actual,
        });
    }
    let mut shared = initialized.shared.clone();
    let mut instance = program
        .resident(initialized.shared, initialized.local)
        .map_err(|source| ReplayError::Initialize { source })?;
    let verifier = DispatchVerifier::shared(binding);

    for (expected, entry) in (0..).zip(steps) {
        let step = entry.step;
        if step != expected {
            return Err(ReplayError::OutOfOrder {
                expected,
                found: step,
            });
        }
        let call = DispatchCall::new(local_peer, ensemble.clone(), entry.event.dispatch_event())
            .with_verifier(verifier.clone());
        let result = instance
            .dispatch(call)
            .map_err(|source| ReplayError::Guest { step, source })?;
        if result.status == CallStatus::Rejected {
            return Err(ReplayError::Rejected {
                step,
                status: match result.reason {
                    Some(reason) => format!("rejected: {reason}"),
                    None => "rejected".into(),
                },
            });
        }
        // A dispatch that returns no images left the shared state as it was.
        if let Some(next) = result.shared {
            shared = next;
        }
        let actual = StateHash::of_shared(&shared);
        if actual != entry.post_state {
            return Err(ReplayError::Diverged {
                step,
                expected: entry.post_state,
                actual,
            });
        }
        instance
            .commit()
            .map_err(|source| ReplayError::Guest { step, source })?;
    }
    Ok(shared)
}

/// Why a replay of agreed steps did not reproduce the agreed states.
///
/// [`Self::OutOfOrder`] is a caller error. Every other variant means this
/// runtime could not reproduce the participants' attested trace.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// Initialization or instantiation failed before any step ran.
    #[error("initialize: {source}")]
    Initialize {
        #[source]
        source: SandboxError,
    },
    #[error("initialized state {actual} does not match the offer's initial state {expected}")]
    InitialState {
        expected: StateHash,
        actual: StateHash,
    },
    #[error("step {step} replayed to {actual}, but the agreed post-state is {expected}")]
    Diverged {
        step: u64,
        expected: StateHash,
        actual: StateHash,
    },
    #[error("step {step}: {source}")]
    Guest {
        step: u64,
        #[source]
        source: SandboxError,
    },
    #[error("step {step}: the program did not accept the agreed event ({status})")]
    Rejected { step: u64, status: String },
    #[error("expected step {expected}, found step {found}")]
    OutOfOrder { expected: u64, found: u64 },
}
