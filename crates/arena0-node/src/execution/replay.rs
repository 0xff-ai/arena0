//! Reproduce a session's shared state at a past agreed step.
//!
//! Only agreed steps change shared state: local events write back the shared
//! bytes they were given, and every participant attests each step's
//! `pre_state` and `post_state`. Replaying the agreed events `0..=k` in a fresh
//! instance therefore reproduces the state after step `k` on any Host, without
//! the Host having stored it.
//!
//! The same replay is full receipt verification: [`verify_full`] replays a
//! receipt's whole trace and checks its ending and outcome against the program.

use arena0_program::{CallStatus, JsonBytes, SharedStateBytes};
use arena0_protocol::{
    ExecutionBinding, PeerId, ReceiptArtifact, StateHash, StepTerminal, TraceEntry,
};
use arena0_sandbox::{DispatchCall, LoadedProgram, SandboxError};

use super::guest::DispatchVerifier;

/// Reproduce the shared state after the last entry of `steps` by replaying
/// them in a fresh instance of `program`.
///
/// The offer params, the committed ensemble and the initial state come from
/// `binding`. `steps` must be the session's agreed steps `0..=k` in order.
/// Each step's event is dispatched exactly as every participant dispatched it.
/// Each resulting state hash must equal the entry's `post_state`, and the
/// step's terminal, derived from the dispatch's lifecycle effect with
/// [`StepTerminal::from_effect`] (or `None` without one), must equal the
/// entry's `terminal`. The first mismatch is reported, never skipped.
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

/// What full verification established beyond light verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullVerification {
    /// The program's JSON projection of the outcome. Present exactly when the
    /// receipt records a completion; a stop has no outcome to project.
    pub outcome_json: Option<JsonBytes>,
}

/// Fully verify an authenticated receipt by replaying it in `program`.
///
/// `artifact` already passed light verification (every `ReceiptArtifact` is
/// authenticated at construction). This adds what light verification cannot:
/// [`replay_shared`] over the artifact's whole trace, so every agreed step must
/// be accepted, reach its `post_state` and produce its `terminal`; and, for a
/// completion, the program's outcome projection of the final shared state must
/// equal the receipt's outcome bytes byte for byte. A stop report or shared
/// stop replays its certified prefix and has no outcome to check.
///
/// The replay dispatches as the first member of the committed ensemble: every
/// participant certified each shared transition, so any one of them
/// reproduces it. The verifying Host need not be a participant.
///
/// Caller obligations: `program` is the program the activation names (its
/// hash equals the activation's program hash) and was loaded under the
/// execution profile the activation names. Passing any other program is a
/// programming error and panics.
pub fn verify_full(
    program: &LoadedProgram,
    artifact: &ReceiptArtifact,
) -> Result<FullVerification, ReplayError> {
    let _ = (program, artifact);
    todo!("STUB(FV1)")
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
    /// The step ended the session differently from the agreed entry: a
    /// different lifecycle effect, or one where the entry has none, or none
    /// where the entry has one.
    #[error("step {step} replayed to terminal {actual:?}, but the agreed terminal is {expected:?}")]
    Terminal {
        step: u64,
        expected: Option<StepTerminal>,
        actual: Option<StepTerminal>,
    },
    /// The program's outcome projection failed on the final shared state.
    #[error("outcome projection: {source}")]
    OutcomeProjection {
        #[source]
        source: SandboxError,
    },
    /// The program projects outcome bytes that differ from the receipt's.
    #[error("the program's outcome projection differs from the receipt's outcome bytes")]
    Outcome,
    #[error("expected step {expected}, found step {found}")]
    OutOfOrder { expected: u64, found: u64 },
}
