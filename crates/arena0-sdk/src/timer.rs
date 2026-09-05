//! Typed timer helpers for program authoring.

use std::time::Duration;

use anyhow::anyhow;
use borsh::{BorshDeserialize, BorshSerialize};

use crate::ProgramFault;
use crate::types::{TimerPayload, TimerSpec};

/// Delay policy for a typed, one-shot timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerSchedule {
    delay: Duration,
}

impl TimerSchedule {
    /// One-shot timer after `delay`.
    #[must_use]
    pub fn after(delay: Duration) -> Self {
        Self { delay }
    }

    #[must_use]
    pub(crate) fn delay_ms(self) -> u64 {
        duration_millis(self.delay)
    }
}

/// Converts `ctx.effects().set_timer(...)` arguments into a timer effect.
pub trait IntoTimerEffect {
    fn into_timer_spec(self) -> TimerSpec;
}

impl IntoTimerEffect for (u64, ()) {
    fn into_timer_spec(self) -> TimerSpec {
        TimerSpec::untyped(self.0)
    }
}

impl<T> IntoTimerEffect for (T, Duration)
where
    T: BorshSerialize + BorshDeserialize + Clone + 'static,
{
    fn into_timer_spec(self) -> TimerSpec {
        typed_timer_spec(self.0, TimerSchedule::after(self.1))
    }
}

impl<T> IntoTimerEffect for (T, TimerSchedule)
where
    T: BorshSerialize + BorshDeserialize + Clone + 'static,
{
    fn into_timer_spec(self) -> TimerSpec {
        typed_timer_spec(self.0, self.1)
    }
}

fn typed_timer_spec<T>(timer: T, schedule: TimerSchedule) -> TimerSpec
where
    T: BorshSerialize + BorshDeserialize + Clone + 'static,
{
    let payload = timer_payload(timer);
    TimerSpec::typed(schedule.delay_ms(), payload)
}

fn duration_millis(delay: Duration) -> u64 {
    let millis = delay.as_millis();
    if millis > u128::from(u64::MAX) {
        u64::MAX
    } else {
        millis as u64
    }
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
    fn duration_schedules_typed_timer_payload() {
        let spec = (Timer::TurnDeadline, Duration::from_secs(2)).into_timer_spec();

        assert_eq!(spec.delay_ms, 2000);
        let payload = spec.payload.expect("typed timer payload");
        assert_eq!(payload.type_name, std::any::type_name::<Timer>());
        assert_eq!(
            decode_timer_payload::<Timer>(payload).unwrap(),
            Timer::TurnDeadline
        );
    }
}
