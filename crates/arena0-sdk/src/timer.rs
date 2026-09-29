//! Typed timer helpers for program authoring.

use anyhow::anyhow;
use borsh::{BorshDeserialize, BorshSerialize};

use crate::ProgramFault;
use crate::types::TimerPayload;

/// A timer delay in whole milliseconds, saturating at `u64::MAX`.
pub(crate) fn duration_millis(delay: std::time::Duration) -> u64 {
    u64::try_from(delay.as_millis()).unwrap_or(u64::MAX)
}

/// Encode a typed timer value into the payload carried by traces and tests.
#[must_use]
pub fn timer_payload<T>(timer: T) -> TimerPayload
where
    T: BorshSerialize + BorshDeserialize + Clone + 'static,
{
    TimerPayload {
        type_name: std::any::type_name::<T>().to_string(),
        data: borsh::to_vec(&timer).unwrap_or_default(),
    }
}

#[doc(hidden)]
pub fn decode_timer_payload<T>(payload: TimerPayload) -> Result<T, ProgramFault>
where
    T: BorshSerialize + BorshDeserialize + Clone + 'static,
{
    let expected = std::any::type_name::<T>();
    if payload.type_name != expected {
        return Err(ProgramFault(anyhow!(
            "typed timer mismatch: expected {expected}, got {}",
            payload.type_name
        )));
    }
    borsh::from_slice(&payload.data)
        .map_err(|e| ProgramFault(anyhow!("typed timer decode failed for {expected}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq)]
    enum Timer {
        TurnDeadline,
    }

    #[test]
    fn typed_timer_payload_round_trips() {
        let payload = timer_payload(Timer::TurnDeadline);

        assert_eq!(payload.type_name, std::any::type_name::<Timer>());
        assert_eq!(
            decode_timer_payload::<Timer>(payload).unwrap(),
            Timer::TurnDeadline
        );
    }

    #[test]
    fn delay_saturates_at_u64_millis() {
        assert_eq!(duration_millis(std::time::Duration::from_secs(2)), 2000);
        assert_eq!(duration_millis(std::time::Duration::MAX), u64::MAX);
    }
}
