//! [`TestHarness`]: the native [`Harness`] fixture. It drives a [`Program`]
//! through its lifecycle on the host target with no Wasm sandbox, capturing
//! effects issued via [`Context`] into a thread-local collector and returning
//! them to the test after each handler call.

use arena0_protocol::PendingId;
use std::cell::RefCell;

use arena0_protocol::trace::JsonDiffExt;
use arena0_protocol::{
    DivergenceDiagnostic, DivergenceKind, Effect, Ensemble, Event, OpenCallout, Participant,
    PeerId, StateHash, View, Viewport,
};
use borsh::{BorshDeserialize, BorshSerialize};

use crate::{
    ApplyDecision, Arena0Callout, CalloutSpec, Context, LocalContext, MessageApply, Program,
    ProgramFault, ProgramTransition, ProgramView,
};

use super::diagnostics::event_name;
use super::harness::{
    __dispatch_record, ClosedPendingReason, ClosedPendingRecord, DispatchRecord, FaultStatus,
    HandlerResult, Harness, PendingHarnessError, PendingLedger,
};

// ---------------------------------------------------------------------------
// Thread-local effect and log collectors
// ---------------------------------------------------------------------------

thread_local! {
    static EFFECTS: RefCell<Vec<Effect>> = const { RefCell::new(Vec::new()) };
    static LOGS: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn push_effect(effect: Effect) {
    EFFECTS.with(|cell| cell.borrow_mut().push(effect));
}

pub(crate) fn push_log(level: String, msg: String) {
    LOGS.with(|cell| cell.borrow_mut().push((level, msg)));
}

pub(crate) fn drain_effects() -> Vec<Effect> {
    EFFECTS.with(|cell| std::mem::take(&mut *cell.borrow_mut()))
}

fn drain_logs() -> Vec<(String, String)> {
    LOGS.with(|cell| std::mem::take(&mut *cell.borrow_mut()))
}

fn shared_hash<C: crate::Primitive>(shared: &C) -> StateHash {
    shared_hash_snapshot(&shared_snapshot(shared))
}

fn shared_snapshot<C: crate::Primitive>(shared: &C) -> Vec<u8> {
    borsh::to_vec(shared).expect("shared serialization failed")
}

fn shared_hash_snapshot(snapshot: &[u8]) -> StateHash {
    let shared = arena0_program::SharedStateBytes::try_new(snapshot.to_vec())
        .expect("shared state exceeds canonical state bound");
    StateHash::of_shared(&shared)
}

fn restore_shared<C: crate::Primitive>(shared: &mut C, snapshot: &[u8]) {
    *shared = borsh::from_slice(snapshot).expect("shared rollback failed");
}

fn restore_local<L: BorshDeserialize>(local: &mut L, snapshot: &[u8]) {
    *local = borsh::from_slice(snapshot).expect("local rollback failed");
}

fn fault_effects(fault: &FaultStatus) -> Vec<Effect> {
    match fault {
        FaultStatus::None => Vec::new(),
        FaultStatus::Abort(reason) => vec![Effect::Fail {
            reason: reason.clone(),
        }],
        FaultStatus::Rejected(_) => Vec::new(),
    }
}

fn derive_open_callout<C>(
    previous: Option<&OpenCallout>,
    id: PendingId,
    request: Option<C>,
) -> Option<OpenCallout>
where
    C: crate::Arena0CalloutRequest + serde::Serialize,
{
    let request = request?;
    let callout_index = request.callout_index();
    let context = serde_json::to_vec(&request).expect("callout context serialization failed");
    if let Some(previous) = previous
        && previous.callout_index == callout_index
        && previous.context == context
    {
        return Some(previous.clone());
    }
    Some(OpenCallout {
        id,
        callout_index,
        context,
    })
}

fn terminal_pending_close(record: &DispatchRecord) -> Option<ClosedPendingReason> {
    record
        .is_terminal()
        .then_some(ClosedPendingReason::Cancelled)
}

fn compare_replayed_event(
    expected: &DispatchRecord,
    actual: &DispatchRecord,
    participant: Option<Participant>,
) -> Result<(), DivergenceDiagnostic> {
    DispatchRecord::compare_event(expected, actual).map_err(|err| {
        let err = err.with_event(event_name(&expected.event));
        if let Some(participant) = participant {
            err.with_participant(participant)
        } else {
            err
        }
    })
}

/// Result of re-driving a saved trace through a native [`TestHarness`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayReport {
    pub event_count: usize,
    pub final_state: Option<StateHash>,
}

// ---------------------------------------------------------------------------
// TestHarness
// ---------------------------------------------------------------------------

/// Drives a [`Program`] through its lifecycle natively (no Wasm sandbox).
///
/// The harness creates a [`Context`] with no flush function (state is
/// never serialized to a guest buffer). Effects are captured via a
/// thread-local collector and returned after each handler call.
pub struct TestHarness<P: Program> {
    shared: P::Shared,
    local: P::Local,
    peer_id: PeerId,
    /// The confirmed session ensemble, captured at `SessionStarted` so the program
    /// can read it via `ctx.ensemble()` on later dispatches, mirroring the runtime.
    committed_ensemble: Option<Ensemble>,
    event_position: u64,
    trace: Vec<DispatchRecord>,
    pending: PendingLedger,
}

impl<P: Program> TestHarness<P> {
    /// Create a new harness and call `Program::initialize` with the given params.
    pub fn new(params: P::Params) -> Self {
        Self::with_peer_id(PeerId([0u8; 32]), params)
    }

    /// Create a new harness with a specific local peer identity.
    pub fn with_peer_id(peer_id: PeerId, params: P::Params) -> Self {
        // Drain any stale effects/logs from a prior test.
        drain_effects();
        drain_logs();

        let mut shared = P::Shared::default();
        match P::initialize(&mut shared, params) {
            Ok(()) => {}
            Err(ProgramFault(e)) => {
                crate::effects::host_fail(&format!("{e:#}"));
            }
        }

        let local = P::Local::default();

        // Discard initialize effects; caller can use new_raw to inspect them.
        drain_effects();
        drain_logs();

        Self {
            shared,
            local,
            peer_id,
            committed_ensemble: None,
            event_position: 0,
            trace: Vec::new(),
            pending: PendingLedger::default(),
        }
    }

    /// Create a harness without calling initialize.
    #[must_use]
    pub fn new_raw(peer_id: PeerId) -> Self {
        drain_effects();
        drain_logs();
        Self {
            shared: P::Shared::default(),
            local: P::Local::default(),
            peer_id,
            committed_ensemble: None,
            event_position: 0,
            trace: Vec::new(),
            pending: PendingLedger::default(),
        }
    }

    /// The local peer identity this harness was created with.
    #[must_use]
    pub fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    /// Mutable access to the current shared state (for test setup).
    pub fn shared_mut(&mut self) -> &mut P::Shared {
        &mut self.shared
    }

    /// Mutable access to the current local state (for test setup).
    pub fn local_mut(&mut self) -> &mut P::Local {
        &mut self.local
    }

    /// Render the program's read-only view against the current harness state.
    ///
    /// Views are pure projections over the current replicated state.
    pub fn view(&self, viewport: Viewport) -> View
    where
        P: ProgramView,
    {
        let ensemble = self
            .committed_ensemble
            .as_ref()
            .expect("view requested before the session started");
        P::view(&self.shared, ensemble, &viewport)
    }

    /// Simulate loss of participant-local state while preserving shared state.
    pub fn lose_local_state(&mut self) {
        self.local = P::Local::default();
        self.pending.clear();
    }

    /// Flat dispatch records emitted by this harness.
    #[must_use]
    pub fn trace(&self) -> &[DispatchRecord] {
        &self.trace
    }

    /// Clear and return the recorded dispatch history.
    pub fn take_trace(&mut self) -> Vec<DispatchRecord> {
        std::mem::take(&mut self.trace)
    }

    /// Verify the recorded trace as a replayable hash chain.
    pub fn verify_trace(&self) -> Result<(), DivergenceDiagnostic> {
        DispatchRecord::verify_chain(&self.trace)
    }

    /// Compare this harness trace against another local replica.
    pub fn compare_trace(&self, other: &Self) -> Result<(), DivergenceDiagnostic> {
        DispatchRecord::compare_traces(&self.trace, &other.trace)
    }

    /// Re-drive a saved trace through the native program implementation.
    ///
    /// This executes every saved event again and compares the effects, pending
    /// metadata, and shared hashes produced by the current program.
    pub fn replay_trace(
        peer_id: PeerId,
        params: P::Params,
        trace: &[DispatchRecord],
    ) -> Result<ReplayReport, DivergenceDiagnostic>
    where
        P::Message: BorshDeserialize + BorshSerialize,
    {
        DispatchRecord::verify_chain(trace)?;
        let mut harness = Self::with_peer_id(peer_id, params);
        for expected in trace {
            let (actual, participant) = harness.dispatch_replay_event(&expected.event)?;
            let actual = actual.record();
            compare_replayed_event(expected, actual, participant)?;
        }
        Ok(ReplayReport {
            event_count: trace.len(),
            final_state: trace.last().map(|record| record.post_state),
        })
    }

    /// Current shared hash for convergence assertions.
    #[must_use]
    pub fn shared_hash(&self) -> StateHash {
        shared_hash(&self.shared)
    }

    /// Compare final shared hashes and return a structured diagnostic on mismatch.
    pub fn assert_shared_aligned_with(&self, other: &Self) -> Result<(), DivergenceDiagnostic> {
        let left = self.shared_hash();
        let right = other.shared_hash();
        if left == right {
            return Ok(());
        }
        let left_value = serde_json::to_value(&self.shared).ok();
        let right_value = serde_json::to_value(&other.shared).ok();
        if let (Some(left_value), Some(right_value)) = (&left_value, &right_value)
            && left_value != right_value
        {
            return Err(DivergenceDiagnostic::new_at(
                self.event_position.max(other.event_position),
                DivergenceKind::SharedMismatch,
                format!("shared{}", left_value.first_difference_path(right_value)),
                left_value,
                right_value,
            ));
        }
        Err(DivergenceDiagnostic::new_at(
            self.event_position.max(other.event_position),
            DivergenceKind::PostStateMismatch,
            "shared_hash",
            left,
            right,
        ))
    }

    /// Closed pending continuations retained for exact-once assertions.
    #[must_use]
    pub fn closed_pending(&self) -> &[ClosedPendingRecord] {
        self.pending.closed()
    }

    // -- Internal -----------------------------------------------------------

    fn run<F, E>(
        &mut self,
        event: Event,
        f: F,
        map_err: impl FnOnce(E) -> FaultStatus,
    ) -> HandlerResult
    where
        F: FnOnce(&mut Context<P::Shared, P::Local>) -> Result<ProgramTransition<P>, E>,
    {
        let previous_pending = self.pending.active().cloned();
        let pending_close = match &event {
            Event::InputReceived { .. } => Some(ClosedPendingReason::Resolved),
            _ => None,
        };
        let pre_snapshot = shared_snapshot(&self.shared);
        let local_snapshot = borsh::to_vec(&self.local).expect("local serialization failed");
        let pre_state = shared_hash_snapshot(&pre_snapshot);
        let shared = std::mem::take(&mut self.shared);
        let local = std::mem::take(&mut self.local);
        let mut ctx = Context::__new(shared, local, self.peer_id);
        if let Some(ref ensemble) = self.committed_ensemble {
            let participant = ensemble
                .participant_of(&self.peer_id)
                .expect("local peer is not in the committed ensemble");
            ctx.__set_participant(participant);
            if let Some(peer) = self.peer() {
                ctx.__set_remote_peer(*peer);
            }
            ctx.__set_committed_ensemble(ensemble.clone());
        }
        drain_effects();
        drain_logs();
        let mut failed = false;
        let fault = match f(&mut ctx) {
            Ok(transition) => {
                ctx.__apply_transition::<P>(transition);
                FaultStatus::None
            }
            Err(e) => {
                failed = true;
                map_err(e)
            }
        };

        let callout_request = if failed {
            None
        } else {
            P::callout(&ctx.__callout_context())
        };
        let (mut shared, mut local, _) = ctx.__into_parts();
        let mut effects = drain_effects();
        let logs = if failed {
            // Faults roll back the complete dispatch observation. In
            // particular, provisional logs must not outlive the state/effect
            // rollback that turns this call into a fault result.
            drain_logs();
            Vec::new()
        } else {
            drain_logs()
        };
        if failed {
            restore_shared(&mut shared, &pre_snapshot);
            restore_local(&mut local, &local_snapshot);
            if matches!(&fault, FaultStatus::Rejected(_)) {
                self.shared = shared;
                self.local = local;
                return HandlerResult {
                    effects: Vec::new(),
                    logs: Vec::new(),
                    fault,
                    records: Vec::new(),
                    rejected: true,
                };
            }
            effects = fault_effects(&fault);
        }
        let post_state = shared_hash(&shared);
        self.shared = shared;
        self.local = local;

        let terminal = effects.iter().any(|effect| {
            matches!(
                effect,
                Effect::SessionEnd { .. } | Effect::SessionAbort { .. } | Effect::Fail { .. }
            )
        });
        let pending = derive_open_callout(
            previous_pending
                .as_ref()
                .filter(|_| !matches!(event, Event::InputReceived { .. })),
            PendingId::new(self.event_position),
            if terminal { None } else { callout_request },
        );
        let record = __dispatch_record(
            self.event_position,
            event,
            effects.clone(),
            pre_state,
            post_state,
            pending.clone(),
        );
        self.event_position += 1;
        self.trace.push(record.clone());
        let pending_close = pending_close.or_else(|| terminal_pending_close(&record));
        self.pending
            .update(previous_pending, pending_close, &fault, pending);

        HandlerResult {
            effects,
            logs,
            fault,
            records: vec![record],
            rejected: false,
        }
    }

    /// Run a local handler against a read-only shared context.
    ///
    /// A local handler returns no transition: it may update local state and
    /// emit effects, but the agreed shared image must not change.
    fn run_local<F, E>(
        &mut self,
        event: Event,
        f: F,
        map_err: impl FnOnce(E) -> FaultStatus,
    ) -> HandlerResult
    where
        F: FnOnce(&mut LocalContext<P::Shared, P::Local>) -> Result<(), E>,
    {
        let previous_pending = self.pending.active().cloned();
        let pending_close = match &event {
            Event::InputReceived { .. } => Some(ClosedPendingReason::Resolved),
            _ => None,
        };
        let pre_snapshot = shared_snapshot(&self.shared);
        let local_snapshot = borsh::to_vec(&self.local).expect("local serialization failed");
        let pre_state = shared_hash_snapshot(&pre_snapshot);
        let shared = std::mem::take(&mut self.shared);
        let local = std::mem::take(&mut self.local);
        // SAFETY: the fixture is the native dispatch harness; the images are
        // the committed shared image and the durable local image.
        let mut ctx = unsafe { LocalContext::__new(shared, local, self.peer_id) };
        if let Some(ref ensemble) = self.committed_ensemble {
            let participant = ensemble
                .participant_of(&self.peer_id)
                .expect("local peer is not in the committed ensemble");
            ctx.__set_participant(participant);
            if let Some(peer) = self.peer() {
                ctx.__set_remote_peer(*peer);
            }
            ctx.__set_committed_ensemble(ensemble.clone());
        }
        drain_effects();
        drain_logs();
        let mut failed = false;
        let fault = match f(&mut ctx) {
            Ok(()) => FaultStatus::None,
            Err(e) => {
                failed = true;
                map_err(e)
            }
        };
        // A local handler must leave the agreed shared image unchanged, the
        // same byte-level boundary the Host enforces. Compare the serialized
        // image so interior-mutable shared DTOs are caught too. Serialize only
        // a successful handler, and treat a serialization error as a changed
        // image, matching the generated guest glue.
        let shared_changed = !failed
            && borsh::to_vec(ctx.shared())
                .map(|bytes| bytes != pre_snapshot)
                .unwrap_or(true);
        // A changed shared image rejects the dispatch before any callout is
        // derived.
        let callout_request = if failed || shared_changed {
            None
        } else {
            P::callout(&ctx.__callout_context())
        };
        let mut local = ctx.__into_local();
        let mut shared: P::Shared =
            borsh::from_slice(&pre_snapshot).expect("shared state deserialization");
        let mut effects = drain_effects();
        let logs = if failed {
            drain_logs();
            Vec::new()
        } else {
            drain_logs()
        };
        if !failed && shared_changed {
            // Restore both images; a rejected dispatch leaves no state change.
            self.shared = shared;
            self.local = borsh::from_slice(&local_snapshot).expect("local deserialization");
            return HandlerResult {
                effects: Vec::new(),
                logs: Vec::new(),
                fault: FaultStatus::Rejected(
                    "a local handler changed the agreed shared state".into(),
                ),
                records: Vec::new(),
                rejected: true,
            };
        }
        if failed {
            restore_shared(&mut shared, &pre_snapshot);
            restore_local(&mut local, &local_snapshot);
            if matches!(&fault, FaultStatus::Rejected(_)) {
                self.shared = shared;
                self.local = local;
                return HandlerResult {
                    effects: Vec::new(),
                    logs: Vec::new(),
                    fault,
                    records: Vec::new(),
                    rejected: true,
                };
            }
            effects = fault_effects(&fault);
        }
        let post_state = shared_hash(&shared);
        self.shared = shared;
        self.local = local;

        let terminal = effects.iter().any(|effect| {
            matches!(
                effect,
                Effect::SessionEnd { .. } | Effect::SessionAbort { .. } | Effect::Fail { .. }
            )
        });
        let pending = derive_open_callout(
            previous_pending
                .as_ref()
                .filter(|_| !matches!(event, Event::InputReceived { .. })),
            PendingId::new(self.event_position),
            if terminal { None } else { callout_request },
        );
        let record = __dispatch_record(
            self.event_position,
            event,
            effects.clone(),
            pre_state,
            post_state,
            pending.clone(),
        );
        self.event_position += 1;
        self.trace.push(record.clone());
        let pending_close = pending_close.or_else(|| terminal_pending_close(&record));
        self.pending
            .update(previous_pending, pending_close, &fault, pending);

        HandlerResult {
            effects,
            logs,
            fault,
            records: vec![record],
            rejected: false,
        }
    }

    fn run_program(
        &mut self,
        event: Event,
        f: impl FnOnce(&mut LocalContext<P::Shared, P::Local>) -> Result<(), ProgramFault>,
    ) -> HandlerResult {
        self.run_local(event, f, |ProgramFault(e)| {
            FaultStatus::Abort(format!("{e:#}"))
        })
    }

    fn run_input(
        &mut self,
        event: Event,
        f: impl FnOnce(&mut LocalContext<P::Shared, P::Local>) -> anyhow::Result<()>,
    ) -> HandlerResult {
        self.run_local(event, f, |e| FaultStatus::Rejected(format!("{e:#}")))
    }
    /// Run a message apply transactionally, mirroring the sandbox layer model:
    /// `Accept` commits both state values, while `Reject` restores both values
    /// and records nothing.
    fn run_apply(
        &mut self,
        event: Event,
        f: impl FnOnce(&mut Context<P::Shared, P::Local>) -> MessageApply<P>,
    ) -> HandlerResult {
        let previous_pending = self.pending.active().cloned();
        let pre_snapshot = shared_snapshot(&self.shared);
        let local_snapshot = borsh::to_vec(&self.local).expect("local serialization failed");
        let pre_state = shared_hash_snapshot(&pre_snapshot);
        let shared = std::mem::take(&mut self.shared);
        let local = std::mem::take(&mut self.local);
        let mut ctx = Context::__new(shared, local, self.peer_id);
        if let Some(ref ensemble) = self.committed_ensemble {
            let participant = ensemble
                .participant_of(&self.peer_id)
                .expect("local peer is not in the committed ensemble");
            ctx.__set_participant(participant);
            if let Some(peer) = self.peer() {
                ctx.__set_remote_peer(*peer);
            }
            ctx.__set_committed_ensemble(ensemble.clone());
        }

        drain_effects();
        drain_logs();
        let mut failed = false;
        let mut rejected = false;
        let fault = match f(&mut ctx) {
            Ok(ApplyDecision::Accept(transition)) => {
                ctx.__apply_transition::<P>(transition);
                FaultStatus::None
            }
            Ok(ApplyDecision::Reject) => {
                rejected = true;
                FaultStatus::None
            }
            Err(e) => {
                failed = true;
                FaultStatus::Abort(format!("{e:#}"))
            }
        };

        let callout_request = if failed || rejected {
            None
        } else {
            P::callout(&ctx.__callout_context())
        };
        let (mut shared, mut local, _) = ctx.__into_parts();
        let mut effects = drain_effects();

        if rejected {
            // Roll the dispatch result back: restore both state values,
            // discard effects, and record nothing.
            drain_logs();
            restore_shared(&mut shared, &pre_snapshot);
            restore_local(&mut local, &local_snapshot);
            self.shared = shared;
            self.local = local;
            return HandlerResult {
                effects: Vec::new(),
                logs: Vec::new(),
                fault: FaultStatus::None,
                records: Vec::new(),
                rejected: true,
            };
        }

        let logs = if failed {
            // A faulting message has the same rollback observation as every
            // other failed dispatch: provisional logs are discarded.
            drain_logs();
            Vec::new()
        } else {
            drain_logs()
        };
        if failed {
            restore_shared(&mut shared, &pre_snapshot);
            restore_local(&mut local, &local_snapshot);
            effects = fault_effects(&fault);
        }
        let post_state = shared_hash(&shared);
        self.shared = shared;
        self.local = local;

        let terminal = effects.iter().any(|effect| {
            matches!(
                effect,
                Effect::SessionEnd { .. } | Effect::SessionAbort { .. } | Effect::Fail { .. }
            )
        });
        let pending = derive_open_callout(
            previous_pending
                .as_ref()
                .filter(|_| !matches!(event, Event::InputReceived { .. })),
            PendingId::new(self.event_position),
            if terminal { None } else { callout_request },
        );
        let record = __dispatch_record(
            self.event_position,
            event,
            effects.clone(),
            pre_state,
            post_state,
            pending.clone(),
        );
        self.event_position += 1;
        self.trace.push(record.clone());
        let pending_close = terminal_pending_close(&record);
        self.pending
            .update(previous_pending, pending_close, &fault, pending);

        HandlerResult {
            effects,
            logs,
            fault,
            records: vec![record],
            rejected: false,
        }
    }

    fn dispatch_replay_event(
        &mut self,
        event: &Event,
    ) -> Result<(HandlerResult, Option<Participant>), DivergenceDiagnostic>
    where
        P::Message: BorshDeserialize + BorshSerialize,
    {
        Ok(match event.clone() {
            Event::SessionStarted { ensemble } => {
                if !ensemble.contains(&self.peer_id) {
                    return Err(DivergenceDiagnostic::new_at(
                        self.event_position,
                        DivergenceKind::EventMismatch,
                        "event.ensemble",
                        format!("ensemble containing replay participant {}", self.peer_id),
                        ensemble.peers(),
                    )
                    .with_event(event_name(event)));
                }
                (self.start_session_raw(ensemble), None)
            }
            Event::MessageReceived { from, msg } => {
                let participant = self
                    .committed_ensemble
                    .as_ref()
                    .and_then(|ensemble| ensemble.participant_of(&from))
                    .ok_or_else(|| {
                        DivergenceDiagnostic::new_at(
                            self.event_position,
                            DivergenceKind::EventMismatch,
                            "event.from",
                            "sender in the committed ensemble",
                            from,
                        )
                        .with_event(event_name(event))
                    })?;
                let decoded: P::Message = borsh::from_slice(&msg).map_err(|err| {
                    DivergenceDiagnostic::new_at(
                        self.event_position,
                        DivergenceKind::EventMismatch,
                        "event.msg",
                        "encoded Message",
                        err.to_string(),
                    )
                })?;
                let result = self.run_apply(Event::MessageReceived { from, msg }, |ctx| {
                    P::on_message(ctx, participant, decoded)
                });
                (result, Some(participant))
            }
            Event::InputReceived {
                callout_index,
                data,
            } => {
                let input = P::Callout::from_raw(callout_index, data.clone()).map_err(|error| {
                    DivergenceDiagnostic::new_at(
                        self.event_position,
                        DivergenceKind::EventMismatch,
                        "event.input",
                        "valid callout input",
                        error.to_string(),
                    )
                    .with_event(event_name(event))
                })?;
                let result = self.run_input(
                    Event::InputReceived {
                        callout_index,
                        data,
                    },
                    |ctx| P::on_input(ctx, input),
                );
                (result, None)
            }
            Event::TimerFired { timer } => (
                self.run_program(
                    Event::TimerFired {
                        timer: timer.clone(),
                    },
                    |ctx| P::on_timer(ctx, timer),
                ),
                None,
            ),
        })
    }

    /// Apply the broadcasts a dispatch queued back to this harness through
    /// `on_message`, as the runtime authors its own queued messages.
    ///
    /// Queued messages are authored in order, including any broadcasts those
    /// dispatches queue in turn, until the harness has no queued work.
    pub fn author_queued(&mut self, effects: &[Effect]) -> Vec<HandlerResult>
    where
        P::Message: BorshSerialize + BorshDeserialize,
    {
        let mut queue: std::collections::VecDeque<Vec<u8>> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Broadcast { data } => Some(data.clone()),
                _ => None,
            })
            .collect();
        let mut results = Vec::new();
        let mut guard = 0usize;
        while let Some(data) = queue.pop_front() {
            guard += 1;
            assert!(guard <= 1024, "queued authoring did not quiesce");
            let msg: P::Message =
                borsh::from_slice(&data).expect("queued message deserialization failed");
            let result = self.message(self.peer_id, msg);
            for effect in &result.effects {
                if let Effect::Broadcast { data } = effect {
                    queue.push_back(data.clone());
                }
            }
            results.push(result);
        }
        results
    }

    /// Start a native harness with an explicit committed participant ensemble.
    ///
    /// [`Harness::session_started`](super::harness::Harness::session_started)
    /// remains the convenient bilateral entry point. N-party program tests use
    /// this method when the full participant set is part of the behavior under
    /// test.
    ///
    /// # Panics
    /// Panics if the ensemble does not contain this harness's local peer.
    pub fn session_started_with_ensemble(&mut self, ensemble: Ensemble) -> HandlerResult
    where
        P::Message: BorshSerialize,
    {
        assert!(
            ensemble.contains(&self.peer_id),
            "local peer is not in the committed ensemble"
        );
        self.start_session(ensemble)
    }

    fn start_session(&mut self, ensemble: Ensemble) -> HandlerResult {
        self.start_session_raw(ensemble)
    }

    fn start_session_raw(&mut self, ensemble: Ensemble) -> HandlerResult {
        self.committed_ensemble = Some(ensemble.clone());
        self.run(
            Event::SessionStarted {
                ensemble: ensemble.clone(),
            },
            |ctx| P::on_session_started(ctx, &ensemble),
            |ProgramFault(e)| FaultStatus::Abort(format!("{e:#}")),
        )
    }
}

impl<P: Program> Harness<P> for TestHarness<P>
where
    P::Message: BorshSerialize,
{
    fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    fn shared(&self) -> &P::Shared {
        &self.shared
    }

    fn local(&self) -> &P::Local {
        &self.local
    }

    fn peer(&self) -> Option<&PeerId> {
        let ensemble = self.committed_ensemble.as_ref()?;
        if ensemble.len() != 2 {
            return None;
        }
        ensemble.peers().iter().find(|peer| **peer != self.peer_id)
    }

    fn session_started(&mut self, peer: PeerId) -> HandlerResult {
        let ensemble =
            Ensemble::from_peers(vec![self.peer_id, peer]).expect("bilateral ensemble is valid");
        self.session_started_with_ensemble(ensemble)
    }

    fn message(&mut self, from: PeerId, msg: P::Message) -> HandlerResult {
        let data = borsh::to_vec(&msg).expect("message serialization failed");
        self.run_apply(Event::MessageReceived { from, msg: data }, |ctx| {
            let from = ctx.participant_for_peer(from);
            P::on_message(ctx, from, msg)
        })
    }

    fn input(&mut self, input: P::Input) -> HandlerResult {
        let (callout_index, data) = P::Callout::to_event_data(&input);
        self.run_input(
            Event::InputReceived {
                callout_index,
                data,
            },
            |ctx| P::on_input(ctx, input),
        )
    }

    fn timer(&mut self) -> HandlerResult {
        self.typed_timer(crate::TimerPayload::unit())
    }

    fn typed_timer(&mut self, timer: crate::TimerPayload) -> HandlerResult {
        self.run_program(
            Event::TimerFired {
                timer: timer.clone(),
            },
            |ctx| P::on_timer(ctx, timer),
        )
    }

    /// Currently open callout, if the last step left one.
    fn active_pending(&self) -> Option<&OpenCallout> {
        self.pending.active()
    }

    /// Resolve a generated typed callout with an explicit pending id.
    ///
    /// Re-dispatches through the program's native `on_input` handler; the
    /// Native harnesses re-dispatch directly, so no restart marker is needed.
    fn resolve_callout_with_pending_id<A>(
        &mut self,
        pending_id: Option<PendingId>,
        output: A::Output,
    ) -> Result<HandlerResult, PendingHarnessError>
    where
        A: CalloutSpec<P>,
    {
        self.pending.validate(pending_id, A::CALLOUT_INDEX)?;
        let input = A::into_input(output);
        let (callout_index, data) = P::Callout::to_event_data(&input);
        Ok(self.run_input(
            Event::InputReceived {
                callout_index,
                data,
            },
            |ctx| P::on_input(ctx, input),
        ))
    }
}

impl<P: Program> std::fmt::Debug for TestHarness<P>
where
    P::Shared: std::fmt::Debug,
    P::Local: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestHarness")
            .field("peer_id", &self.peer_id)
            .field("shared", &self.shared)
            .field("local", &self.local)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::{
        Arena0CalloutRequest, ManagedPhase, PhasedSharedState, ProgramQuery, ProgramValue,
        SharedState,
    };
    use anyhow::anyhow;

    use super::super::orchestration::{DeliveryAction, DeliverySchedule};

    #[derive(
        Debug,
        Clone,
        Copy,
        Default,
        PartialEq,
        Eq,
        serde::Serialize,
        serde::Deserialize,
        borsh::BorshSerialize,
        borsh::BorshDeserialize,
        schemars::JsonSchema,
    )]
    enum TestPhase {
        #[default]
        Idle,
    }

    impl crate::Arena0Phase for TestPhase {
        const DECLS: &'static [crate::PhaseDecl] = &[crate::PhaseDecl {
            name: "idle",
            description: "Idle",
            is_default: true,
            is_terminal: false,
        }];

        fn as_str(self) -> &'static str {
            "idle"
        }

        fn is_terminal(self) -> bool {
            false
        }

        fn is_default(self) -> bool {
            true
        }
    }

    impl ProgramValue for TestPhase {}

    #[derive(
        Debug,
        Default,
        serde::Serialize,
        serde::Deserialize,
        borsh::BorshSerialize,
        borsh::BorshDeserialize,
        schemars::JsonSchema,
    )]
    struct FaultyShared {
        phase: ManagedPhase<TestPhase>,
        value: u32,
    }

    impl crate::Primitive for FaultyShared {}

    impl ProgramValue for FaultyShared {}

    impl SharedState for FaultyShared {
        const STATE_MAX: usize = 16;
    }

    impl PhasedSharedState for FaultyShared {
        type Phase = TestPhase;

        fn phase(&self) -> Self::Phase {
            self.phase.get()
        }

        fn __set_phase(&mut self, phase: Self::Phase) {
            self.phase = ManagedPhase::__new(phase);
        }
    }

    #[derive(serde::Serialize)]
    struct FaultyCallout;

    impl Arena0Callout for FaultyCallout {
        type Request = ();
        type Response = ();

        fn schemas() -> Vec<arena0_program::CalloutSchema> {
            Vec::new()
        }

        fn from_raw(_callout_index: u32, _data: Vec<u8>) -> anyhow::Result<Self::Response> {
            Ok(())
        }

        fn to_event_data(_response: &Self::Response) -> (u32, Vec<u8>) {
            (0, Vec::new())
        }
    }

    impl Arena0CalloutRequest for FaultyCallout {
        fn callout_index(&self) -> u32 {
            0
        }
    }

    #[derive(Debug, Default, borsh::BorshSerialize, borsh::BorshDeserialize)]
    struct FaultyLocal {
        value: u32,
    }

    struct FaultyProgram;

    impl Program for FaultyProgram {
        type Shared = FaultyShared;
        type Local = FaultyLocal;
        type Phase = TestPhase;
        type Message = bool;
        type Callout = FaultyCallout;
        type Input = ();
        type Params = ();
        type Outcome = ();

        fn outcome(_shared: &Self::Shared) -> Self::Outcome {}

        fn writer(_shared: &Self::Shared) -> Option<Participant> {
            None
        }

        fn on_message(
            ctx: &mut Context<Self::Shared, Self::Local>,
            _from: Participant,
            msg: Self::Message,
        ) -> MessageApply<Self> {
            ctx.shared_mut().value = 7;
            ctx.local_mut().value = 9;
            ctx.log("provisional message log");
            ctx.effects().broadcast(&msg);
            if msg {
                Err(anyhow!("bad message").into())
            } else {
                Ok(ApplyDecision::Reject)
            }
        }

        fn on_input(
            ctx: &mut LocalContext<Self::Shared, Self::Local>,
            _input: Self::Input,
        ) -> anyhow::Result<()> {
            ctx.local_mut().value = 9;
            ctx.log("provisional rejection log");
            let _ = ctx.effects().broadcast(&());
            Err(anyhow!("try again"))
        }
    }

    impl ProgramQuery for FaultyProgram {
        type Query = ();

        fn query(_shared: &Self::Shared, _ensemble: &Ensemble, _query: Self::Query) {}
    }

    fn peer_b() -> PeerId {
        PeerId([2; 32])
    }

    fn pending(id: u64, callout_index: u32) -> OpenCallout {
        OpenCallout {
            id: PendingId::new(id),
            callout_index,
            context: vec![b'n', b'u', b'l', b'l'],
        }
    }

    #[test]
    fn program_fault_rolls_back_shared_and_filters_effects() {
        let mut h = TestHarness::<FaultyProgram>::new(());
        h.session_started(peer_b());

        let result = h.message(peer_b(), true);

        assert_eq!(h.shared().value, 0);
        assert_eq!(h.local().value, 0);
        assert!(result.has_state_fault());
        assert!(result.logs.is_empty());
        assert!(matches!(result.effects.as_slice(), [Effect::Fail { .. }]));
        assert_eq!(result.record().pre_state, result.record().post_state);
    }

    #[test]
    fn message_reject_rolls_back_shared_and_local_without_a_trace_or_effect() {
        let mut h = TestHarness::<FaultyProgram>::new(());
        h.session_started(peer_b());
        let trace_len = h.trace().len();

        let result = h.message(peer_b(), false);

        assert_eq!(h.shared().value, 0);
        assert_eq!(h.local().value, 0);
        assert!(result.rejected);
        assert!(result.logs.is_empty());
        assert!(result.effects.is_empty());
        assert!(result.records.is_empty());
        assert_eq!(h.trace().len(), trace_len);
    }

    #[test]
    fn rejected_input_rolls_back_shared_without_a_trace_or_effect() {
        let mut h = TestHarness::<FaultyProgram>::new(());
        h.session_started(peer_b());
        h.pending.set_active(Some(pending(10, 0)));

        let result = h.input(());

        assert_eq!(h.shared().value, 0);
        assert_eq!(h.local().value, 0);
        assert!(result.has_input_rejection());
        assert!(result.logs.is_empty());
        assert!(result.effects.is_empty());
        assert!(result.records.is_empty());
        assert_eq!(
            h.active_pending().map(|pending| pending.id),
            Some(PendingId::new(10))
        );
        assert!(h.closed_pending().is_empty());
    }

    #[test]
    fn pending_validation_reports_wrong_type_stale_and_duplicate_results() {
        let mut h = TestHarness::<FaultyProgram>::new(());
        h.pending.set_active(Some(pending(10, 1)));

        h.pending.validate(Some(PendingId::new(10)), 1).unwrap();
        let err = h.pending.validate(Some(PendingId::new(10)), 2).unwrap_err();
        assert_eq!(
            err,
            PendingHarnessError::CalloutIndexMismatch {
                submitted: 2,
                pending: 1,
            }
        );
        let err = h.pending.validate(Some(PendingId::new(9)), 1).unwrap_err();
        assert_eq!(
            err,
            PendingHarnessError::Stale {
                submitted_id: PendingId::new(9),
                pending_id: PendingId::new(10),
            }
        );

        h.pending
            .record_closed(pending(10, 1), ClosedPendingReason::Resolved);
        h.pending.set_active(None);
        let err = h.pending.validate(Some(PendingId::new(10)), 1).unwrap_err();
        assert_eq!(
            err,
            PendingHarnessError::Closed {
                pending_id: PendingId::new(10),
                reason: ClosedPendingReason::Resolved,
            }
        );
    }

    #[test]
    fn harness_replays_trace_through_program() {
        let mut h = TestHarness::<FaultyProgram>::new(());
        h.session_started(peer_b());
        h.message(peer_b(), true);

        let report =
            TestHarness::<FaultyProgram>::replay_trace(PeerId([0; 32]), (), h.trace()).unwrap();
        // The flat transcript includes the session boundary and the message
        // dispatch.
        assert_eq!(report.event_count, 2);
        assert_eq!(
            report.final_state,
            h.trace().last().map(|record| record.post_state)
        );
    }

    #[test]
    fn harness_replay_reports_effect_pending_and_state_mismatches() {
        let mut h = TestHarness::<FaultyProgram>::new(());
        h.session_started(peer_b());
        h.message(peer_b(), true);
        let trace = h.trace().to_vec();

        let mut effect_mismatch = trace.clone();
        let message_index = effect_mismatch
            .iter()
            .position(|record| matches!(record.event, Event::MessageReceived { .. }))
            .expect("message dispatch is recorded");
        effect_mismatch[message_index].effects.clear();
        let err = TestHarness::<FaultyProgram>::replay_trace(PeerId([0; 32]), (), &effect_mismatch)
            .unwrap_err();
        assert_eq!(err.kind, DivergenceKind::EffectMismatch);
        assert_eq!(err.event.as_deref(), Some("MessageReceived"));

        let mut post_state_mismatch = vec![trace[0].clone()];
        post_state_mismatch[0].post_state = StateHash([9; 32]);
        let err =
            TestHarness::<FaultyProgram>::replay_trace(PeerId([0; 32]), (), &post_state_mismatch)
                .unwrap_err();
        assert_eq!(err.kind, DivergenceKind::PostStateMismatch);
    }

    #[test]
    fn harness_reports_shared_hash_divergence() {
        let left = TestHarness::<FaultyProgram>::new(());
        let mut right = TestHarness::<FaultyProgram>::new(());
        right.shared_mut().value = 9;

        let err = left.assert_shared_aligned_with(&right).unwrap_err();
        assert_eq!(err.kind, DivergenceKind::SharedMismatch);
        assert_eq!(err.field_path, "shared.value");
    }

    #[test]
    fn delivery_schedule_generates_and_shrinks() {
        let mut covered = BTreeSet::new();
        for seed in 0..32 {
            covered.extend(
                DeliverySchedule::generated(seed, 4)
                    .actions()
                    .iter()
                    .copied(),
            );
        }
        assert!(covered.contains(&DeliveryAction::DeliverAll));
        assert!(covered.contains(&DeliveryAction::DropNext));
        assert!(covered.contains(&DeliveryAction::DuplicateNext));
        assert!(covered.contains(&DeliveryAction::ReorderNext));

        let schedule = DeliverySchedule::new(vec![
            DeliveryAction::DeliverAll,
            DeliveryAction::DuplicateNext,
            DeliveryAction::ReorderNext,
        ]);
        let shrunk = schedule
            .shrink_failure(|candidate| candidate.actions().contains(&DeliveryAction::ReorderNext));
        assert_eq!(shrunk.actions(), &[DeliveryAction::ReorderNext]);
    }
}
