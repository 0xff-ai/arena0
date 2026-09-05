use super::super::ExecutionStatus;
use super::super::PlanDraft;
use super::super::{ExecutionState, ProtocolError, build_plan, illegal};
use crate::exec::ExecLifecycle;
pub(crate) fn reduce(state: &ExecutionState) -> Result<PlanDraft, ProtocolError> {
    if state.lifecycle() != ExecLifecycle::Activating {
        return Err(illegal(state, "activate"));
    }
    let mut next = state.clone();
    next.status = ExecutionStatus::active();
    build_plan(state, next, None, None, None, Vec::new(), Vec::new())
}
