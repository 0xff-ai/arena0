use super::super::PlanDraft;
use super::super::{
    ExecutionState, ExecutionStatus, PendingCoordinate, PrivateCommit, PrivateDelta, ProtocolError,
    apply_timer_mutations, build_plan, pending_effect_index, require_active,
    validate_private_record,
};
pub(crate) fn reduce(
    state: &ExecutionState,
    delta: PrivateDelta,
) -> Result<PlanDraft, ProtocolError> {
    require_active(state, "private delta")?;
    if state.proposal.is_some() {
        return Err(ProtocolError::SharedProposalExists);
    }
    if state.terminal_proof().is_some() {
        return Err(ProtocolError::TerminalProofPending);
    }
    validate_private_record(&delta.record)?;
    if delta.execution_id() != state.execution_id() {
        return Err(ProtocolError::BindingMismatch);
    }
    if delta.record.seq != state.private.next_record {
        return Err(ProtocolError::InvalidPrivateSequence {
            expected: state.private.next_record,
            actual: delta.record.seq,
        });
    }
    if delta.record.after_position != state.public.next_step() {
        return Err(ProtocolError::StepCoordinateMismatch {
            expected: state.public.next_step(),
            actual: delta.record.after_position,
        });
    }
    let (effects, timers) = delta.derive_consequences(state)?;
    let mut next = state.clone();
    next.private = if matches!(delta.cause(), super::super::PrivateCause::React) {
        state
            .private
            .advance_reaction(delta.record.after_position)?
    } else {
        state.private.advance()?
    };
    next.local_state = delta.local_state.clone();
    apply_timer_mutations(&mut next.timers, &timers)?;
    let retrying = delta
        .record
        .effects
        .iter()
        .any(|effect| matches!(effect, crate::PrivateEffect::RetryInput { .. }));
    let pending = if retrying && matches!(delta.cause(), super::super::PrivateCause::Resume { .. })
    {
        state.status().pending().map(|(pending, _)| pending.clone())
    } else {
        delta.record.pending.clone()
    };
    let pending_coordinate =
        if retrying && matches!(delta.cause(), super::super::PrivateCause::Resume { .. }) {
            state.status().pending().map(|(_, coordinate)| coordinate)
        } else {
            pending_effect_index(&delta.record).map(|effect_index| PendingCoordinate {
                record: delta.record.seq,
                effect_index,
            })
        };
    match (pending, pending_coordinate) {
        (Some(pending), Some(coordinate)) => {
            next.status = ExecutionStatus::waiting(pending, coordinate);
        }
        (None, None) => next.status = ExecutionStatus::active(),
        _ => return Err(ProtocolError::PendingCoordinateMismatch),
    }
    let private_commit = PrivateCommit {
        record: delta.record,
        local_state: delta.local_state,
    };
    build_plan(
        state,
        next,
        None,
        Some(private_commit),
        None,
        timers,
        effects,
    )
}
