//! Step messages: each participant's one answer for an agreed step.

/// How a received step message relates to the local execution state.
///
/// Classification is pure; only [`StepMessageArrival::New`] leads to a
/// durable write (`ExecutionState::record_step_message`), and the receiver
/// acknowledges it only after that write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepMessageArrival {
    /// For an already agreed step; acknowledge.
    Stale,
    /// For a later step; the sender retries.
    NotYet,
    /// Bound to another chain link, from a non-participant, or the execution
    /// is not collecting; refuse.
    Rejected,
    /// Equal to the message already held from this sender for this step;
    /// acknowledge.
    Duplicate,
    /// Different from the message already held from this sender for this
    /// step; refuse and keep the first.
    Conflict,
    /// The sender's first message for the current step; record it.
    New,
}
