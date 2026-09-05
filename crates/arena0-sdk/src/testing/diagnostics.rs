//! Trace coverage and convergence assertions shared by the orchestration
//! framework. [`CoverageReport`] summarizes which events and effects a suite
//! exercised, and [`PairTrace`] carries the shared-hash and replay assertions
//! used to prove two [`BilateralPair`] replicas stayed in step.

use std::collections::{BTreeMap, BTreeSet};

use arena0_protocol::{DivergenceDiagnostic, PublicEffect, PublicEvent, TraceEntry};
use borsh::{BorshDeserialize, BorshSerialize};

use crate::{Arena0Phase, Program};

use super::harness::Harness;
use super::orchestration::BilateralPair;

/// Public-trace coverage summary for scenario suites.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoverageReport {
    pub events: BTreeMap<String, usize>,
    pub event_effect_pairs: BTreeSet<(String, String)>,
}

impl CoverageReport {
    pub(super) fn from_traces(left: &[TraceEntry], right: &[TraceEntry]) -> Self {
        let mut report = Self::default();
        for step in left.iter().chain(right) {
            let event = event_name(&step.event).to_string();
            *report.events.entry(event.clone()).or_insert(0) += 1;
            for effect in &step.effects {
                let effect = effect_name(effect).to_string();
                report.event_effect_pairs.insert((event.clone(), effect));
            }
        }
        report
    }

    /// Assert that at least one trace step covered `event`.
    pub fn assert_event(&self, event: &str) {
        assert!(
            self.events.contains_key(event),
            "event {event:?} was not covered; covered events: {:?}",
            self.events.keys().collect::<Vec<_>>()
        );
    }

    /// Assert that at least one trace step covered the event/effect pair.
    pub fn assert_event_effect(&self, event: &str, effect: &str) {
        let pair = (event.to_string(), effect.to_string());
        assert!(
            self.event_effect_pairs.contains(&pair),
            "event/effect pair {pair:?} was not covered"
        );
    }
}

/// Assertions and transcript helpers over a [`BilateralPair`].
pub struct PairTrace<'a, P: Program>
where
    P::Message: BorshSerialize + BorshDeserialize,
{
    pub(super) pair: &'a BilateralPair<P>,
}

impl<P> std::fmt::Debug for PairTrace<'_, P>
where
    P: Program,
    P::Message: BorshSerialize + BorshDeserialize,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairTrace")
            .field("alice_steps", &self.pair.alice().trace().len())
            .field("bob_steps", &self.pair.bob().trace().len())
            .finish()
    }
}

impl<P> PairTrace<'_, P>
where
    P: Program,
    P::Message: BorshSerialize + BorshDeserialize,
{
    /// Assert both replicas ended with the same shared hash.
    pub fn assert_shared_aligned(&self) {
        self.try_shared_aligned().expect("shared diverged");
    }

    /// Return a structured diagnostic if final shared hashes diverged.
    pub fn try_shared_aligned(&self) -> Result<(), DivergenceDiagnostic> {
        self.pair
            .alice()
            .assert_shared_aligned_with(self.pair.bob())
    }

    /// Assert both traces are individually replayable hash chains.
    pub fn assert_replayable(&self) {
        self.try_replayable().expect("trace is not replayable");
    }

    /// Return the first replay diagnostic across either participant.
    pub fn try_replayable(&self) -> Result<(), DivergenceDiagnostic> {
        self.pair.alice().verify_trace()?;
        self.pair.bob().verify_trace()
    }

    /// Assert both replicas are in the requested terminal phase.
    pub fn assert_phase_terminal(&self, phase: P::Phase)
    where
        P::Phase: PartialEq + std::fmt::Debug + Copy,
    {
        assert!(phase.is_terminal(), "expected a terminal phase");
        assert_eq!(P::__phase(self.pair.alice().shared()), Some(phase));
        assert_eq!(P::__phase(self.pair.bob().shared()), Some(phase));
    }

    /// Return a concise transcript suitable for snapshots or diagnostics.
    #[must_use]
    pub fn pretty(&self) -> String {
        let mut out = String::new();
        append_trace_summary(&mut out, "Alice", self.pair.alice().trace());
        append_trace_summary(&mut out, "Bob", self.pair.bob().trace());
        out
    }

    /// Assert the pretty transcript exactly matches a saved snapshot string.
    pub fn assert_transcript(&self, expected: &str) {
        assert_eq!(self.pretty().trim(), expected.trim());
    }

    /// Explain the first convergence, replay, or trace mismatch.
    #[must_use]
    pub fn explain_divergence(&self) -> String {
        if let Err(err) = self.try_replayable() {
            return err.to_string();
        }
        if let Err(err) = self.try_shared_aligned() {
            return err.to_string();
        }
        "no divergence detected".into()
    }
}

fn append_trace_summary(out: &mut String, label: &str, trace: &[TraceEntry]) {
    use std::fmt::Write as _;

    let _ = writeln!(out, "{label}:");
    for step in trace {
        let effect_names = step
            .effects
            .iter()
            .map(effect_name)
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            out,
            "  #{} {:?} effects=[{}]",
            step.step, step.event, effect_names
        );
    }
}

fn effect_name(effect: &PublicEffect) -> &'static str {
    match effect {
        PublicEffect::SessionEnd { .. } => "SessionEnd",
        PublicEffect::SessionAbort { .. } => "SessionAbort",
        PublicEffect::Fail { .. } => "Fail",
    }
}

/// Human-readable event name, also used by [`super::fixtures`]'s replay
/// diagnostics.
pub(super) fn event_name(event: &PublicEvent) -> &'static str {
    match event {
        PublicEvent::SessionStarted { .. } => "SessionStarted",
        PublicEvent::MessageReceived { .. } => "MessageReceived",
    }
}
