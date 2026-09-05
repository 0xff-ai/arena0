//! Test harness and in-process orchestration framework for arena0 programs.
//!
//! The harness layer defines the [`Harness`] contract for program testing. The
//! fixture layer provides the native [`TestHarness`], which drives a
//! [`crate::Program`] directly on the host with no Wasm sandbox. The
//! orchestration layer adds a two-participant
//! [`BilateralPair`] and replayable [`Scenario`] builder on top of native
//! harnesses, and the diagnostics layer holds the trace and coverage helpers
//! both consume.

mod diagnostics;
mod fixtures;
mod harness;
mod orchestration;

pub use diagnostics::{CoverageReport, PairTrace};
pub use fixtures::{ReplayReport, TestHarness};
#[doc(hidden)]
pub use harness::__step_record;
#[doc(hidden)]
pub use harness::PendingLedger;
pub use harness::{
    ClosedPendingReason, ClosedPendingRecord, FaultStatus, HandlerResult, Harness,
    PendingHarnessError, TypedCalloutRecord,
};
pub use orchestration::{
    ALICE, BOB, BilateralPair, DeliveryAction, DeliverySchedule, PairSnapshot, Scenario,
    ScenarioRun,
};

pub(crate) use fixtures::{push_effect, push_log};
// Only test code (in this crate) reads back the collected effects/logs directly
// via `crate::testing::drain_effects`; production code drains through the
// running harness instead. Gate the re-export so it isn't flagged dead in a
// non-test build.
#[cfg(test)]
pub(crate) use fixtures::drain_effects;
