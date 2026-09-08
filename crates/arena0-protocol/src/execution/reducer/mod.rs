use super::{CommitPlan, ExecutionInput, ExecutionState, ProtocolError, TransitionOutcome};
#[cfg(feature = "performance-tracing")]
use std::time::Instant;

mod activation;
mod private;
mod shared;
mod terminal;

pub(crate) use activation::reduce as reduce_activate;
pub(crate) use private::reduce as reduce_private;
pub(crate) use shared::{
    reduce_propose as reduce_propose_shared, reduce_signature as reduce_step_signature,
};
pub(crate) use terminal::{
    reduce_abort, reduce_interrupt_terminal, reduce_receipt_body,
    reduce_signature as reduce_terminal_signature,
};

/// Apply one pure execution input to a validated durable aggregate.
pub fn transition(
    state: &ExecutionState,
    input: ExecutionInput,
) -> Result<TransitionOutcome, ProtocolError> {
    #[cfg(feature = "performance-tracing")]
    let performance_enabled = tracing::enabled!(
        target: "arena0::performance",
        tracing::Level::DEBUG
    ) && !matches!(
        &input,
        ExecutionInput::StepSignature(_) | ExecutionInput::TerminalSignature(_)
    );
    #[cfg(feature = "performance-tracing")]
    let started = performance_enabled.then(Instant::now);
    #[cfg(feature = "performance-tracing")]
    let input_kind = performance_enabled.then(|| input_kind(&input));
    let result = (|| {
        let occurrence = input.occurrence(state)?;
        if is_already_applied(state, &input) {
            return Ok(TransitionOutcome::AlreadyApplied);
        }
        let plan = match input {
            ExecutionInput::Activate => reduce_activate(state),
            ExecutionInput::ProposeShared(delta) => reduce_propose_shared(state, delta),
            ExecutionInput::StepSignature(signature) => reduce_step_signature(state, signature),
            ExecutionInput::Private(delta) => reduce_private(state, delta),
            ExecutionInput::TerminalSignature(signature) => {
                reduce_terminal_signature(state, signature)
            }
            ExecutionInput::ReceiptBody(body) => reduce_receipt_body(state, *body),
            ExecutionInput::Abort(occurrence) => reduce_abort(state, occurrence),
            ExecutionInput::InterruptTerminal(reason) => reduce_interrupt_terminal(state, reason),
        }?;
        let plan = CommitPlan::finalize(plan, occurrence)?;
        Ok(TransitionOutcome::Commit(Box::new(plan)))
    })();

    #[cfg(feature = "performance-tracing")]
    if let (Some(started), Some(input_kind)) = (started, input_kind) {
        let (success, result_class) = match &result {
            Ok(TransitionOutcome::Commit(_)) => (true, "commit"),
            Ok(TransitionOutcome::AlreadyApplied) => (true, "already_applied"),
            Err(_) => (false, "rejected"),
        };
        let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        tracing::debug!(
            target: "arena0::performance",
            operation = "execution_reducer",
            exec_id = %state.execution_id(),
            session_id = %state.binding().session_id(),
            version = state.version().get(),
            public_step = state.public().next_step(),
            input_kind,
            success,
            result_class,
            elapsed_us,
        );
    }

    result
}

#[cfg(feature = "performance-tracing")]
fn input_kind(input: &ExecutionInput) -> &'static str {
    match input {
        ExecutionInput::Activate => "activate",
        ExecutionInput::ProposeShared(_) => "propose_shared",
        ExecutionInput::StepSignature(_) => "step_signature",
        ExecutionInput::Private(_) => "private",
        ExecutionInput::TerminalSignature(_) => "terminal_signature",
        ExecutionInput::ReceiptBody(_) => "receipt_body",
        ExecutionInput::Abort(_) => "abort",
        ExecutionInput::InterruptTerminal(_) => "interrupt_terminal",
    }
}

fn is_already_applied(state: &ExecutionState, input: &ExecutionInput) -> bool {
    match input {
        ExecutionInput::Activate => state.lifecycle() != crate::ExecLifecycle::Activating,
        ExecutionInput::ProposeShared(delta) => state.proposal.as_ref().is_some_and(|proposal| {
            proposal.entry == delta.entry
                && proposal.shared_state == delta.shared_state
                && proposal.terminal_outcome == delta.terminal_outcome
        }),
        ExecutionInput::StepSignature(signature) => {
            state.proposal.as_ref().is_some_and(|proposal| {
                proposal.signatures.iter().any(|existing| {
                    existing.participant == signature.participant
                        && existing.signature == signature.signature
                })
            })
        }
        ExecutionInput::TerminalSignature(signature) => state
            .status()
            .terminal_proof()
            .and_then(super::TerminalProof::pending_parts)
            .is_some_and(|(_, _, signatures)| {
                signatures.iter().any(|existing| {
                    existing.participant == signature.participant
                        && existing.signature == signature.signature
                })
            }),
        ExecutionInput::ReceiptBody(_) => false,
        ExecutionInput::Abort(occurrence) => state
            .status()
            .terminal_cause()
            .is_some_and(|cause| {
                matches!(cause, super::super::StopCause::Authenticated(existing) if existing == occurrence)
            }),
        ExecutionInput::InterruptTerminal(reason) => matches!(
            state.status(),
            super::super::ExecutionStatus::Incomplete { reason: existing, .. }
                if existing == reason
        ),
        _ => false,
    }
}
