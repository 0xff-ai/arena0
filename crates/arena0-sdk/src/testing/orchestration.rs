//! In-process orchestration for bilateral program tests: [`BilateralPair`]
//! drives two native [`TestHarness`] replicas with deterministic peer-message
//! delivery (drop/duplicate/reorder), and [`Scenario`] records a replayable
//! sequence of such steps so property tests can generate, run, and shrink
//! whole interaction schedules instead of hand-rolling them per test.

use std::collections::{BTreeMap, VecDeque};

use arena0_protocol::{Effect, Participant, PeerId, StateHash};
use borsh::{BorshDeserialize, BorshSerialize};

use crate::Program;

use super::diagnostics::{CoverageReport, PairTrace};
use super::fixtures::TestHarness;
use super::harness::{HandlerResult, Harness};

/// Canonical participant 0 in bilateral scenario tests.
pub const ALICE: Participant = Participant::new(0);

/// Canonical participant 1 in bilateral scenario tests.
pub const BOB: Participant = Participant::new(1);

#[derive(Debug, Clone)]
struct QueuedMessage {
    from: PeerId,
    to: PeerId,
    data: Vec<u8>,
}

/// Two native replicas running the same bilateral program.
///
/// `BilateralPair` is a higher-level harness for tests that need to prove
/// convergence across both sides instead of inspecting one local handler at a
/// time. It keeps peer-message delivery deterministic and lets tests inject
/// drops or duplicates before asserting shared and transition alignment.
pub struct BilateralPair<P: Program>
where
    P::Message: BorshSerialize + BorshDeserialize,
{
    alice: TestHarness<P>,
    bob: TestHarness<P>,
    outbox: VecDeque<QueuedMessage>,
}

impl<P> std::fmt::Debug for BilateralPair<P>
where
    P: Program,
    P::Message: BorshSerialize + BorshDeserialize,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BilateralPair")
            .field("alice_peer", &self.alice.peer_id())
            .field("bob_peer", &self.bob.peer_id())
            .field("queued_messages", &self.outbox.len())
            .finish()
    }
}

impl<P> BilateralPair<P>
where
    P: Program,
    P::Message: BorshSerialize + BorshDeserialize,
{
    /// Start a local bilateral session with deterministic peer identities.
    #[must_use]
    pub fn start(params: P::Params) -> Self
    where
        P::Params: Clone,
    {
        let alice_peer = PeerId([0u8; 32]);
        let bob_peer = PeerId([1u8; 32]);
        let mut pair = Self {
            alice: TestHarness::with_peer_id(alice_peer, params.clone()),
            bob: TestHarness::with_peer_id(bob_peer, params),
            outbox: VecDeque::new(),
        };
        let alice_fx = pair.alice.session_started(bob_peer);
        pair.queue_effects(alice_peer, &alice_fx.effects);
        let bob_fx = pair.bob.session_started(alice_peer);
        pair.queue_effects(bob_peer, &bob_fx.effects);
        pair
    }

    /// Immutable access to Alice's local harness.
    #[must_use]
    pub fn alice(&self) -> &TestHarness<P> {
        &self.alice
    }

    /// Immutable access to Bob's local harness.
    #[must_use]
    pub fn bob(&self) -> &TestHarness<P> {
        &self.bob
    }

    fn dispatch_participant(
        &mut self,
        participant: Participant,
        dispatch: impl FnOnce(&mut TestHarness<P>) -> HandlerResult,
    ) -> HandlerResult {
        let (peer, result) = match participant.index() {
            0 => {
                let peer = self.alice.peer_id();
                (peer, dispatch(&mut self.alice))
            }
            1 => {
                let peer = self.bob.peer_id();
                (peer, dispatch(&mut self.bob))
            }
            idx => panic!("participant {idx} is outside the bilateral pair"),
        };
        self.queue_effects(peer, &result.effects);
        result
    }

    /// Deliver local input to one participant and queue any emitted messages.
    pub fn input(&mut self, participant: Participant, input: P::Input) -> HandlerResult {
        self.dispatch_participant(participant, |harness| harness.input(input))
    }

    /// Dispatch an untyped timer event to one participant and queue any emitted messages.
    pub fn timer(&mut self, participant: Participant) -> HandlerResult {
        self.dispatch_participant(participant, TestHarness::timer)
    }

    /// Dispatch a typed timer event to one participant and queue any emitted messages.
    pub fn timer_value<T>(&mut self, participant: Participant, timer: T) -> HandlerResult
    where
        T: borsh::BorshSerialize + borsh::BorshDeserialize + Clone + 'static,
    {
        self.typed_timer(participant, crate::timer_payload(timer))
    }

    /// Dispatch a raw typed timer payload to one participant and queue any emitted messages.
    pub fn typed_timer(
        &mut self,
        participant: Participant,
        payload: crate::TimerPayload,
    ) -> HandlerResult {
        self.dispatch_participant(participant, |harness| harness.typed_timer(payload))
    }

    /// Inject a message from one participant to the other.
    pub fn message(&mut self, from: Participant, msg: P::Message) -> HandlerResult {
        let data = borsh::to_vec(&msg).expect("message serialization failed");
        let (from_peer, to_peer) = self.peer_route(from);
        self.deliver_raw(from_peer, to_peer, data)
    }

    /// Drop the next queued peer message, if any.
    pub fn drop_next_message(&mut self) -> Option<(PeerId, PeerId)> {
        self.outbox.pop_front().map(|msg| (msg.from, msg.to))
    }

    /// Duplicate the next queued peer message, if any.
    pub fn duplicate_next_message(&mut self) -> bool {
        let Some(msg) = self.outbox.front().cloned() else {
            return false;
        };
        self.outbox.insert(1.min(self.outbox.len()), msg);
        true
    }

    /// Move the next queued message behind the following queued message.
    pub fn reorder_next_message(&mut self) -> bool {
        if self.outbox.len() < 2 {
            return false;
        }
        self.outbox.swap(0, 1);
        true
    }

    /// Number of queued peer messages waiting for deterministic delivery.
    #[must_use]
    pub fn queued_messages(&self) -> usize {
        self.outbox.len()
    }

    /// Simulate participant-local state loss without changing shared state.
    pub fn lose_local_state(&mut self, participant: Participant) {
        match participant.index() {
            0 => self.alice.lose_local_state(),
            1 => self.bob.lose_local_state(),
            idx => panic!("participant {idx} is outside the bilateral pair"),
        }
    }

    /// Snapshot the pair for transcript or golden-file style assertions.
    #[must_use]
    pub fn snapshot(&self) -> PairSnapshot {
        PairSnapshot {
            alice_shared: self.alice.shared_hash(),
            bob_shared: self.bob.shared_hash(),
            alice_steps: self.alice.trace().len(),
            bob_steps: self.bob.trace().len(),
            queued_messages: self.outbox.len(),
            transcript: self.trace().pretty(),
        }
    }

    /// Summarize trace coverage across both participants.
    #[must_use]
    pub fn coverage(&self) -> CoverageReport {
        CoverageReport::from_traces(self.alice.trace(), self.bob.trace())
    }

    /// Deliver all currently queued peer messages to quiescence.
    pub fn deliver_all(&mut self) {
        let mut guard = 0usize;
        while let Some(msg) = self.outbox.pop_front() {
            guard += 1;
            assert!(guard <= 1024, "bilateral delivery did not quiesce");
            let fx = self.deliver_raw(msg.from, msg.to, msg.data);
            self.queue_effects(msg.to, &fx.effects);
        }
    }

    /// Return an immutable trace/assertion view.
    #[must_use]
    pub fn trace(&self) -> PairTrace<'_, P> {
        PairTrace { pair: self }
    }

    /// Return transcript helpers for snapshot-style assertions.
    #[must_use]
    pub fn transcript(&self) -> PairTrace<'_, P> {
        self.trace()
    }

    fn queue_effects(&mut self, from: PeerId, effects: &[Effect]) {
        // Broadcast-only messaging: every message goes to both participants,
        // the sender included, so the sender applies its own message through
        // the same shared handler (self-delivery is queued first).
        for effect in effects {
            if let Effect::Broadcast { data } = effect {
                let other = if from == self.alice.peer_id() {
                    self.bob.peer_id()
                } else {
                    self.alice.peer_id()
                };
                self.outbox.push_back(QueuedMessage {
                    from,
                    to: from,
                    data: data.clone(),
                });
                self.outbox.push_back(QueuedMessage {
                    from,
                    to: other,
                    data: data.clone(),
                });
            }
        }
    }

    fn deliver_raw(&mut self, from: PeerId, to: PeerId, data: Vec<u8>) -> HandlerResult {
        let msg: P::Message = borsh::from_slice(&data).expect("message deserialization failed");
        if to == self.alice.peer_id() {
            self.alice.message(from, msg)
        } else if to == self.bob.peer_id() {
            self.bob.message(from, msg)
        } else {
            panic!("message target {to} is outside the bilateral pair");
        }
    }

    fn peer_route(&self, from: Participant) -> (PeerId, PeerId) {
        match from.index() {
            0 => (self.alice.peer_id(), self.bob.peer_id()),
            1 => (self.bob.peer_id(), self.alice.peer_id()),
            idx => panic!("participant {idx} is outside the bilateral pair"),
        }
    }
}

/// Stable snapshot of a bilateral pair's trace and convergence state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairSnapshot {
    pub alice_shared: StateHash,
    pub bob_shared: StateHash,
    pub alice_steps: usize,
    pub bob_steps: usize,
    pub queued_messages: usize,
    pub transcript: String,
}

/// Deterministic peer-message delivery perturbation for scenario/property tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DeliveryAction {
    /// Deliver all currently queued messages to quiescence.
    DeliverAll,
    /// Drop the next queued message.
    DropNext,
    /// Duplicate the next queued message.
    DuplicateNext,
    /// Reorder the next two queued messages.
    ReorderNext,
}

/// Generated delivery schedule with simple deletion-based shrinking.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeliverySchedule {
    actions: Vec<DeliveryAction>,
}

impl DeliverySchedule {
    /// Build an explicit schedule.
    #[must_use]
    pub fn new(actions: impl Into<Vec<DeliveryAction>>) -> Self {
        Self {
            actions: actions.into(),
        }
    }

    /// Generate a deterministic schedule from a seed.
    #[must_use]
    pub fn generated(mut seed: u64, len: usize) -> Self {
        let mut actions = Vec::with_capacity(len);
        for _ in 0..len {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            actions.push(match (seed >> 32) & 0b11 {
                0 => DeliveryAction::DeliverAll,
                1 => DeliveryAction::DropNext,
                2 => DeliveryAction::DuplicateNext,
                _ => DeliveryAction::ReorderNext,
            });
        }
        Self { actions }
    }

    /// Return a schedule with drop actions removed.
    #[must_use]
    pub fn without_drops(&self) -> Self {
        Self {
            actions: self
                .actions
                .iter()
                .copied()
                .filter(|action| !matches!(action, DeliveryAction::DropNext))
                .collect(),
        }
    }

    /// Schedule actions.
    #[must_use]
    pub fn actions(&self) -> &[DeliveryAction] {
        &self.actions
    }

    /// Delete actions while the supplied predicate still reports failure.
    #[must_use]
    pub fn shrink_failure(&self, mut still_fails: impl FnMut(&Self) -> bool) -> Self {
        let mut shrunk = self.clone();
        let mut idx = 0;
        while idx < shrunk.actions.len() {
            let mut candidate = shrunk.clone();
            candidate.actions.remove(idx);
            if still_fails(&candidate) {
                shrunk = candidate;
            } else {
                idx += 1;
            }
        }
        shrunk
    }
}

/// Replayable scenario builder for bilateral program tests.
pub struct Scenario<P: Program>
where
    P::Message: BorshSerialize + BorshDeserialize,
{
    name: String,
    steps: Vec<ScenarioStep<P>>,
}

/// Completed scenario run plus named pair snapshots captured during execution.
pub struct ScenarioRun<P: Program>
where
    P::Message: BorshSerialize + BorshDeserialize,
{
    pair: BilateralPair<P>,
    snapshots: BTreeMap<String, PairSnapshot>,
}

impl<P> std::fmt::Debug for ScenarioRun<P>
where
    P: Program,
    P::Message: BorshSerialize + BorshDeserialize,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScenarioRun")
            .field("snapshot_names", &self.snapshots.keys().collect::<Vec<_>>())
            .field("final_snapshot", &self.pair.snapshot())
            .finish()
    }
}

impl<P> ScenarioRun<P>
where
    P: Program,
    P::Message: BorshSerialize + BorshDeserialize,
{
    /// Final pair after all scenario steps ran.
    #[must_use]
    pub fn pair(&self) -> &BilateralPair<P> {
        &self.pair
    }

    /// Consume the report and return the final pair.
    #[must_use]
    pub fn into_pair(self) -> BilateralPair<P> {
        self.pair
    }

    /// Retrieve a named snapshot captured by `Scenario::snapshot`.
    #[must_use]
    pub fn snapshot(&self, name: &str) -> Option<&PairSnapshot> {
        self.snapshots.get(name)
    }

    /// Assert a named snapshot exactly matches an expected snapshot.
    pub fn assert_snapshot(&self, name: &str, expected: &PairSnapshot) {
        assert_eq!(self.snapshot(name), Some(expected));
    }
}

impl<P> std::fmt::Debug for Scenario<P>
where
    P: Program,
    P::Message: BorshSerialize + BorshDeserialize,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scenario")
            .field("name", &self.name)
            .field("steps", &self.steps.len())
            .finish()
    }
}

enum ScenarioStep<P: Program> {
    Input {
        participant: Participant,
        input: P::Input,
    },
    Message {
        from: Participant,
        msg: P::Message,
    },
    Timer {
        participant: Participant,
        timer: Option<crate::TimerPayload>,
    },
    DeliverAll,
    DropNext,
    DuplicateNext,
    ReorderNext,
    LoseLocalState {
        participant: Participant,
    },
    Snapshot {
        name: String,
    },
}

impl<P> Scenario<P>
where
    P: Program,
    P::Message: BorshSerialize + BorshDeserialize,
{
    /// Create a named scenario.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            steps: Vec::new(),
        }
    }

    /// Scenario name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Add a local input step.
    #[must_use]
    pub fn input(mut self, participant: Participant, input: P::Input) -> Self {
        self.steps.push(ScenarioStep::Input { participant, input });
        self
    }

    /// Add an injected message step.
    #[must_use]
    pub fn message(mut self, from: Participant, msg: P::Message) -> Self {
        self.steps.push(ScenarioStep::Message { from, msg });
        self
    }

    /// Add an untyped timer step.
    #[must_use]
    pub fn timer(mut self, participant: Participant) -> Self {
        self.steps.push(ScenarioStep::Timer {
            participant,
            timer: None,
        });
        self
    }

    /// Add a typed timer step.
    #[must_use]
    pub fn timer_value<T>(mut self, participant: Participant, timer: T) -> Self
    where
        T: borsh::BorshSerialize + borsh::BorshDeserialize + Clone + 'static,
    {
        self.steps.push(ScenarioStep::Timer {
            participant,
            timer: Some(crate::timer_payload(timer)),
        });
        self
    }

    /// Deliver all queued messages to quiescence.
    #[must_use]
    pub fn deliver_all(mut self) -> Self {
        self.steps.push(ScenarioStep::DeliverAll);
        self
    }

    /// Drop the next queued message.
    #[must_use]
    pub fn drop_next_message(mut self) -> Self {
        self.steps.push(ScenarioStep::DropNext);
        self
    }

    /// Duplicate the next queued message.
    #[must_use]
    pub fn duplicate_next_message(mut self) -> Self {
        self.steps.push(ScenarioStep::DuplicateNext);
        self
    }

    /// Reorder the next two queued messages.
    #[must_use]
    pub fn reorder_next_message(mut self) -> Self {
        self.steps.push(ScenarioStep::ReorderNext);
        self
    }

    /// Apply a generated delivery schedule.
    #[must_use]
    pub fn delivery_schedule(mut self, schedule: DeliverySchedule) -> Self {
        for action in schedule.actions {
            self = match action {
                DeliveryAction::DeliverAll => self.deliver_all(),
                DeliveryAction::DropNext => self.drop_next_message(),
                DeliveryAction::DuplicateNext => self.duplicate_next_message(),
                DeliveryAction::ReorderNext => self.reorder_next_message(),
            };
        }
        self
    }

    /// Simulate losing participant-local state.
    #[must_use]
    pub fn lose_local_state(mut self, participant: Participant) -> Self {
        self.steps
            .push(ScenarioStep::LoseLocalState { participant });
        self
    }

    /// Capture a named pair snapshot at this point in the scenario.
    #[must_use]
    pub fn snapshot(mut self, name: impl Into<String>) -> Self {
        self.steps
            .push(ScenarioStep::Snapshot { name: name.into() });
        self
    }

    /// Run the scenario from a fresh pair.
    pub fn run(self, params: P::Params) -> BilateralPair<P>
    where
        P::Params: Clone,
    {
        self.run_with_snapshots(params).into_pair()
    }

    /// Run the scenario and keep named snapshots captured along the way.
    pub fn run_with_snapshots(self, params: P::Params) -> ScenarioRun<P>
    where
        P::Params: Clone,
    {
        let mut pair = BilateralPair::start(params);
        let mut snapshots = BTreeMap::new();
        for step in self.steps {
            match step {
                ScenarioStep::Input { participant, input } => {
                    pair.input(participant, input);
                }
                ScenarioStep::Message { from, msg } => {
                    pair.message(from, msg);
                }
                ScenarioStep::Timer { participant, timer } => match timer {
                    Some(timer) => {
                        pair.typed_timer(participant, timer);
                    }
                    None => {
                        pair.timer(participant);
                    }
                },
                ScenarioStep::DeliverAll => pair.deliver_all(),
                ScenarioStep::DropNext => {
                    pair.drop_next_message();
                }
                ScenarioStep::DuplicateNext => {
                    pair.duplicate_next_message();
                }
                ScenarioStep::ReorderNext => {
                    pair.reorder_next_message();
                }
                ScenarioStep::LoseLocalState { participant } => {
                    pair.lose_local_state(participant);
                }
                ScenarioStep::Snapshot { name } => {
                    snapshots.insert(name, pair.snapshot());
                }
            }
        }
        ScenarioRun { pair, snapshots }
    }

    /// Run the scenario and assert final shared convergence.
    pub fn assert_converges(self, params: P::Params) -> BilateralPair<P>
    where
        P::Params: Clone,
    {
        let pair = self.run(params);
        pair.trace().assert_shared_aligned();
        pair
    }

    /// Run the scenario and assert both replicas reach a terminal phase.
    pub fn assert_terminal(self, params: P::Params, phase: P::Phase) -> BilateralPair<P>
    where
        P::Params: Clone,
        P::Phase: PartialEq + std::fmt::Debug + Copy,
    {
        let pair = self.run(params);
        pair.trace().assert_phase_terminal(phase);
        pair
    }
}
