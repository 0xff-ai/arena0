use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use super::TimerId;
/// A timer firing supplied by the timer adapter.  No clock is read by the
/// reducer; the timer identity was already authenticated by storage.
#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
pub struct TimerFiring {
    pub(crate) timer_id: TimerId,
}

impl TimerFiring {
    /// Construct a timer firing input.
    #[must_use]
    pub const fn new(timer_id: TimerId) -> Self {
        Self { timer_id }
    }

    /// Return the timer identity.
    #[must_use]
    pub const fn timer_id(self) -> TimerId {
        self.timer_id
    }
}
