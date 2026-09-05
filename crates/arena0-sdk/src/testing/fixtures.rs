//! [`TestHarness`]: the native [`Harness`] fixture. It drives a [`Program`]
//! through its lifecycle on the host target with no Wasm sandbox, capturing
//! effects issued via [`Context`] into a thread-local collector and returning
//! them to the test after each handler call.

use arena0_protocol::PendingId;
use std::cell::RefCell;

use arena0_protocol::trace::JsonDiffExt;
use arena0_protocol::{
    DivergenceDiagnostic, DivergenceKind, Effect, Ensemble, Event, MessageId, Participant, PeerId,
    PendingRecord, PrivateEffect, PrivateEvent, PrivateRecord, PublicEffect, PublicEvent,
    StateHash, TraceEntry, View, Viewport,
};
use borsh::{BorshDeserialize, BorshSerialize};

use crate::{
    ApplyDecision, Arena0Callout, CalloutSpec, Context, InputFault, MessageApply, Program,
    ProgramFault, ProgramTransition, ProgramView, SharedContext,
};

use super::diagnostics::event_name;
use super::harness::{
    __step_record, ClosedPendingReason, ClosedPendingRecord, FaultStatus, HandlerResult, Harness,
    PendingHarnessError, PendingLedger,
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
    StateHash(*blake3::hash(&shared_snapshot(shared)).as_bytes())
}

fn shared_snapshot<C: crate::Primitive>(shared: &C) -> Vec<u8> {
    borsh::to_vec(shared).expect("shared serialization failed")
}

fn restore_shared<C: crate::Primitive>(shared: &mut C, snapshot: &[u8]) {
    *shared = borsh::from_slice(snapshot).expect("shared rollback failed");
}

fn fault_effects(fault: &FaultStatus) -> Vec<Effect> {
    match fault {
        FaultStatus::None => Vec::new(),
        FaultStatus::Abort(reason) => vec![Effect::Fail {
            reason: reason.clone(),
        }],
        FaultStatus::Retryable(reason) => vec![Effect::RetryInput {
            reason: reason.clone(),
        }],
    }
}

fn terminal_pending_close(step: &TraceEntry) -> Option<ClosedPendingReason> {
    step.is_terminal().then_some(ClosedPendingReason::Cancelled)
}

fn compare_replayed_step(
    expected: &TraceEntry,
    actual: &TraceEntry,
    participant: Option<Participant>,
) -> Result<(), DivergenceDiagnostic> {
    TraceEntry::compare_step(expected, actual).map_err(|err| {
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
    pub steps: usize,
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
    peer: Option<PeerId>,
    /// The confirmed session ensemble, captured at `SessionStarted` so the program
    /// can read it via `ctx.ensemble()` on later steps, mirroring the runtime.
    committed_ensemble: Option<Ensemble>,
    step: u64,
    trace: Vec<TraceEntry>,
    private_seq: u64,
    private_trace: Vec<PrivateRecord>,
    pending: PendingLedger,
}

impl<P: Program> TestHarness<P> {
    /// Create a new harness and call `Program::initialize` with the given params.
    pub fn new(params: P::Params) -> Self {
        Self::with_peer_id(PeerId([0u8; 32]), params)
    }

    /// Create a new harness with a specific local peer identity.
    pub fn with_peer_id(peer_id: PeerId, params: P::Params) -> Self {
        let shared = P::Shared::default();
        let mut ctx = SharedContext::__new(shared, None);

        // Drain any stale effects/logs from a prior test.
        drain_effects();
        drain_logs();

        match P::initialize(&mut ctx, params) {
            Ok(()) => {}
            Err(ProgramFault(e)) => {
                crate::effects::host_fail(&format!("{e:#}"));
            }
        }

        let shared = ctx.__into_shared();
        let local = P::Local::default();

        // Discard initialize effects; caller can use new_raw to inspect them.
        drain_effects();
        drain_logs();

        Self {
            shared,
            local,
            peer_id,
            peer: None,
            committed_ensemble: None,
            step: 0,
            trace: Vec::new(),
            private_seq: 0,
            private_trace: Vec::new(),
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
            peer: None,
            committed_ensemble: None,
            step: 0,
            trace: Vec::new(),
            private_seq: 0,
            private_trace: Vec::new(),
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
    /// The context is assembled with the same participant and committed
    /// ensemble metadata used by lifecycle dispatch, then its state is put
    /// back into the harness after rendering. This keeps native view tests on
    /// the same context surface as the runtime without requiring each program
    /// to reimplement the setup.
    pub fn view(&mut self, viewport: Viewport) -> View
    where
        P: ProgramView,
    {
        let shared = std::mem::take(&mut self.shared);
        let mut ctx = SharedContext::__new(shared, None);
        if let Some(ref ensemble) = self.committed_ensemble {
            ctx.__set_ensemble(ensemble.clone());
        }

        let view = P::view(&ctx, &viewport);
        self.shared = ctx.__into_shared();
        view
    }

    /// Simulate loss of participant-local state while preserving shared state.
    pub fn lose_local_state(&mut self) {
        self.local = P::Local::default();
        self.pending.clear();
    }

    /// Recorded state-machine steps emitted by this harness.
    #[must_use]
    pub fn trace(&self) -> &[TraceEntry] {
        &self.trace
    }

    /// Clear and return the recorded state-machine steps.
    pub fn take_trace(&mut self) -> Vec<TraceEntry> {
        std::mem::take(&mut self.trace)
    }

    /// Private local records produced by callouts, timers, signatures, and
    /// reaction handlers.
    #[must_use]
    pub fn private_trace(&self) -> &[PrivateRecord] {
        &self.private_trace
    }

    /// Verify the recorded trace as a replayable hash chain.
    pub fn verify_trace(&self) -> Result<(), DivergenceDiagnostic> {
        TraceEntry::verify_chain(&self.trace)
    }

    /// Compare this harness trace against another local replica.
    pub fn compare_trace(&self, other: &Self) -> Result<(), DivergenceDiagnostic> {
        TraceEntry::compare_traces(&self.trace, &other.trace)
    }

    /// Re-drive a saved trace through the native program implementation.
    ///
    /// Unlike [`TraceEntry::verify_chain`], this executes every saved event
    /// again and compares the effects, pending metadata, and shared hashes
    /// produced by the current program.
    pub fn replay_trace(
        peer_id: PeerId,
        params: P::Params,
        trace: &[TraceEntry],
    ) -> Result<ReplayReport, DivergenceDiagnostic>
    where
        P::Message: BorshDeserialize + BorshSerialize,
    {
        TraceEntry::verify_chain(trace)?;
        let mut harness = Self::with_peer_id(peer_id, params);
        for expected in trace {
            let participant = harness.participant_for_replay_event(&expected.event);
            let actual = harness.dispatch_replay_event(&expected.event)?;
            let actual = actual.step();
            compare_replayed_step(expected, actual, participant)?;
        }
        Ok(ReplayReport {
            steps: trace.len(),
            final_state: trace.last().map(|step| step.post_state),
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
                self.step.max(other.step),
                DivergenceKind::SharedMismatch,
                format!("shared{}", left_value.first_difference_path(right_value)),
                left_value,
                right_value,
            ));
        }
        Err(DivergenceDiagnostic::new_at(
            self.step.max(other.step),
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
        F: FnOnce(&mut Context<P::Shared, P::Local>) -> Result<(), E>,
    {
        let previous_pending = self.pending.active().cloned();
        let pending_close = match &event {
            Event::InputReceived { .. } | Event::Signed { .. } => {
                Some(ClosedPendingReason::Resolved)
            }
            _ => None,
        };
        let public_event = PublicEvent::try_from(event.clone()).ok();
        let private_event = PrivateEvent::try_from(event.clone()).ok();
        let pre_snapshot = shared_snapshot(&self.shared);
        let pre_state = StateHash(*blake3::hash(&pre_snapshot).as_bytes());
        let shared = std::mem::take(&mut self.shared);
        let local = std::mem::take(&mut self.local);
        let mut ctx = Context::__new(shared, local, self.peer_id);
        if let Some(ref peer) = self.peer {
            ctx.__set_participant(Participant::of(&self.peer_id, peer));
            ctx.__set_remote_peer(*peer);
        }
        if let Some(ref ensemble) = self.committed_ensemble {
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

        let (mut shared, local, _) = ctx.__into_parts();
        let mut effects = drain_effects();
        if failed {
            restore_shared(&mut shared, &pre_snapshot);
            effects = fault_effects(&fault);
        }
        let post_state = shared_hash(&shared);
        self.shared = shared;
        self.local = local;

        let private_effects: Vec<PrivateEffect> = effects
            .iter()
            .cloned()
            .filter_map(|effect| PrivateEffect::try_from(effect).ok())
            .collect();
        let new_pending = private_event.as_ref().and_then(|_| {
            PendingRecord::from_effects(PendingId::new(self.private_seq), &private_effects)
        });

        let private_record = private_event.map(|event| {
            let record = PrivateRecord {
                seq: self.private_seq,
                after_position: self.step,
                event,
                effects: private_effects,
                draws: Vec::new(),
                fuel_used: 0,
                pending: new_pending.clone(),
            };
            self.private_seq += 1;
            self.private_trace.push(record.clone());
            record
        });

        let public_record = public_event.map(|event| {
            let public_effects: Vec<PublicEffect> = effects
                .iter()
                .cloned()
                .filter_map(|effect| PublicEffect::try_from(effect).ok())
                .collect();
            let record = __step_record(self.step, event, public_effects, pre_state, post_state);
            self.step += 1;
            self.trace.push(record.clone());
            record
        });
        let pending_close =
            pending_close.or_else(|| public_record.as_ref().and_then(terminal_pending_close));
        self.pending
            .update(previous_pending, pending_close, &fault, new_pending);

        HandlerResult {
            effects,
            logs: drain_logs(),
            fault,
            step: public_record,
            private_record,
            rejected: false,
        }
    }

    fn run_program(
        &mut self,
        event: Event,
        f: impl FnOnce(&mut Context<P::Shared, P::Local>) -> Result<(), ProgramFault>,
    ) -> HandlerResult {
        self.run(event, f, |ProgramFault(e)| {
            FaultStatus::Abort(format!("{e:#}"))
        })
    }

    fn run_shared(
        &mut self,
        event: Event,
        f: impl FnOnce(&mut SharedContext<P::Shared>) -> Result<ProgramTransition<P>, ProgramFault>,
    ) -> HandlerResult {
        let previous_pending = self.pending.active().cloned();
        let pre_snapshot = shared_snapshot(&self.shared);
        let pre_state = StateHash(*blake3::hash(&pre_snapshot).as_bytes());
        let shared = std::mem::take(&mut self.shared);
        let mut ctx = SharedContext::__new(shared, self.committed_ensemble.clone());

        drain_effects();
        drain_logs();
        let mut failed = false;
        let fault = match f(&mut ctx) {
            Ok(transition) => {
                ctx.__apply_transition::<P>(transition);
                FaultStatus::None
            }
            Err(ProgramFault(error)) => {
                failed = true;
                FaultStatus::Abort(format!("{error:#}"))
            }
        };

        let mut shared = ctx.__into_shared();
        let mut effects = drain_effects();
        if failed {
            restore_shared(&mut shared, &pre_snapshot);
            effects = fault_effects(&fault);
        }
        let post_state = shared_hash(&shared);
        self.shared = shared;

        let event = PublicEvent::try_from(event).expect("shared event must be public");
        let public_effects: Vec<PublicEffect> = effects
            .iter()
            .cloned()
            .filter_map(|effect| PublicEffect::try_from(effect).ok())
            .collect();
        let record = __step_record(self.step, event, public_effects, pre_state, post_state);
        self.step += 1;
        self.trace.push(record.clone());
        let pending_close = terminal_pending_close(&record);
        self.pending
            .update(previous_pending, pending_close, &fault, None);

        HandlerResult {
            effects,
            logs: drain_logs(),
            fault,
            step: Some(record),
            private_record: None,
            rejected: false,
        }
    }

    fn run_input(
        &mut self,
        event: Event,
        f: impl FnOnce(&mut Context<P::Shared, P::Local>) -> Result<(), InputFault>,
    ) -> HandlerResult {
        self.run(event, f, |e| match e {
            InputFault::Unrecoverable(e) => FaultStatus::Abort(format!("{e:#}")),
            InputFault::Retryable(e) => FaultStatus::Retryable(format!("{e:#}")),
        })
    }
    /// Run a shared message apply transactionally, mirroring the sandbox layer
    /// model: `Accept` records a step, `Reject` restores the committed
    /// shared-visible bytes and records nothing.
    fn run_apply(
        &mut self,
        event: Event,
        f: impl FnOnce(&mut SharedContext<P::Shared>) -> MessageApply<P>,
    ) -> HandlerResult {
        let previous_pending = self.pending.active().cloned();
        let pre_snapshot = shared_snapshot(&self.shared);
        let pre_state = StateHash(*blake3::hash(&pre_snapshot).as_bytes());
        let shared = std::mem::take(&mut self.shared);
        let mut ctx = SharedContext::__new(shared, self.committed_ensemble.clone());

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

        let mut shared = ctx.__into_shared();
        let mut effects = drain_effects();

        if rejected {
            // Roll the candidate layer back: restore the committed
            // shared-visible bytes, discard effects, record nothing.
            restore_shared(&mut shared, &pre_snapshot);
            self.shared = shared;
            return HandlerResult {
                effects: Vec::new(),
                logs: drain_logs(),
                fault: FaultStatus::None,
                step: None,
                private_record: None,
                rejected: true,
            };
        }

        if failed {
            restore_shared(&mut shared, &pre_snapshot);
            effects = fault_effects(&fault);
        }
        let post_state = shared_hash(&shared);
        self.shared = shared;

        let event = PublicEvent::try_from(event).expect("apply event must be public");
        let public_effects: Vec<PublicEffect> = effects
            .iter()
            .cloned()
            .filter_map(|effect| PublicEffect::try_from(effect).ok())
            .collect();
        let record = __step_record(self.step, event, public_effects, pre_state, post_state);
        let new_pending = None;
        self.step += 1;
        self.trace.push(record.clone());
        let pending_close = terminal_pending_close(&record);
        self.pending
            .update(previous_pending, pending_close, &fault, new_pending);

        HandlerResult {
            effects,
            logs: drain_logs(),
            fault,
            step: Some(record),
            private_record: None,
            rejected: false,
        }
    }

    /// Run the local decision hook, as the runtime does after every applied
    /// public entry (skipped while a callout is pending or after a fault).
    fn react(&mut self) -> HandlerResult {
        self.run(
            Event::React,
            |ctx| P::on_react(ctx),
            |ProgramFault(e)| FaultStatus::Abort(format!("{e:#}")),
        )
    }

    /// Merge a shared apply with the react run that follows it, mirroring the
    /// runtime: one result carrying both effect sets, keyed on the shared step.
    fn with_react(&mut self, result: HandlerResult) -> HandlerResult {
        let terminal = result.step.as_ref().is_some_and(TraceEntry::is_terminal);
        if !matches!(result.fault, FaultStatus::None)
            || result.rejected
            || terminal
            || self.pending.active().is_some()
        {
            return result;
        }
        let react = self.react();
        let mut effects = result.effects;
        effects.extend(react.effects);
        let mut logs = result.logs;
        logs.extend(react.logs);
        HandlerResult {
            effects,
            logs,
            fault: react.fault,
            step: result.step,
            private_record: react.private_record.or(result.private_record),
            rejected: result.rejected,
        }
    }

    fn participant_for_replay_event(&self, event: &PublicEvent) -> Option<Participant> {
        match event {
            PublicEvent::SessionStarted { ensemble } => ensemble
                .others(&self.peer_id)
                .next()
                .map(|peer| Participant::of(&peer, &self.peer_id)),
            PublicEvent::MessageReceived { from: peer, .. } => {
                Some(Participant::of(peer, &self.peer_id))
            }
        }
    }

    fn dispatch_replay_event(
        &mut self,
        event: &PublicEvent,
    ) -> Result<HandlerResult, DivergenceDiagnostic>
    where
        P::Message: BorshDeserialize + BorshSerialize,
    {
        Ok(match event.clone() {
            PublicEvent::SessionStarted { ensemble } => {
                self.peer = ensemble.others(&self.peer_id).next();
                self.committed_ensemble = Some(ensemble.clone());
                let result = self.run_shared(
                    Event::SessionStarted {
                        ensemble: ensemble.clone(),
                    },
                    |ctx| P::on_session_started(ctx, &ensemble),
                );
                self.with_react(result)
            }
            PublicEvent::MessageReceived {
                message_id,
                from,
                position,
                pre_state,
                msg,
            } => {
                let decoded: P::Message = borsh::from_slice(&msg).map_err(|err| {
                    DivergenceDiagnostic::new_at(
                        self.step,
                        DivergenceKind::EventMismatch,
                        "event.msg",
                        "encoded Message",
                        err.to_string(),
                    )
                })?;
                // React is a private record attached to this public step, so
                // replay the same public dispatch followed by its local hook.
                let result = self.run_apply(
                    Event::MessageReceived {
                        message_id,
                        from,
                        position,
                        pre_state,
                        msg,
                    },
                    |ctx| {
                        let from = ctx.participant_for_peer(from);
                        P::on_message(ctx, from, decoded)
                    },
                );
                self.with_react(result)
            }
        })
    }

    fn session_started_with_ensemble(&mut self, ensemble: Ensemble) -> HandlerResult
    where
        P::Message: BorshSerialize,
    {
        self.peer = ensemble.others(&self.peer_id).next();
        self.committed_ensemble = Some(ensemble.clone());
        let result = self.run_shared(
            Event::SessionStarted {
                ensemble: ensemble.clone(),
            },
            |ctx| P::on_session_started(ctx, &ensemble),
        );
        self.with_react(result)
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
        self.peer.as_ref()
    }

    fn session_started(&mut self, peer: PeerId) -> HandlerResult {
        let ensemble =
            Ensemble::from_peers(vec![self.peer_id, peer]).expect("bilateral ensemble is valid");
        self.session_started_with_ensemble(ensemble)
    }

    fn message(&mut self, from: PeerId, msg: P::Message) -> HandlerResult {
        let data = borsh::to_vec(&msg).expect("message serialization failed");
        let result = self.run_apply(
            Event::MessageReceived {
                message_id: MessageId([0u8; 32]),
                from,
                position: 0,
                pre_state: StateHash([0u8; 32]),
                msg: data,
            },
            |ctx| {
                let from = ctx.participant_for_peer(from);
                P::on_message(ctx, from, msg)
            },
        );
        self.with_react(result)
    }

    fn input(&mut self, input: P::Input) -> HandlerResult {
        let (callout_index, data) = P::Callout::to_event_data(&input);
        self.run_input(
            Event::InputReceived {
                callout_index,
                data,
                continuation_tag: None,
            },
            |ctx| P::on_input(ctx, input),
        )
    }

    fn timer(&mut self) -> HandlerResult {
        self.run_program(Event::TimerFired, |ctx| P::on_timer(ctx))
    }

    fn typed_timer(&mut self, timer: crate::TimerPayload) -> HandlerResult {
        self.run_program(
            Event::TypedTimerFired {
                timer: timer.clone(),
            },
            |ctx| P::__arena0_on_typed_timer(ctx, timer),
        )
    }

    /// Currently active pending continuation, if the last step suspended.
    fn active_pending(&self) -> Option<&PendingRecord> {
        self.pending.active()
    }

    /// Resolve a generated typed callout with an explicit pending id.
    ///
    /// Re-dispatches through the program's native `on_input` handler; the
    /// continuation tag is not threaded (native harnesses don't restart, so
    /// there is nothing to resume across).
    fn resolve_callout_with_pending_id<A>(
        &mut self,
        pending_id: Option<PendingId>,
        output: A::Output,
    ) -> Result<HandlerResult, PendingHarnessError>
    where
        A: CalloutSpec<P>,
    {
        self.pending.validate(
            pending_id,
            arena0_protocol::PendingOperation::Callout {
                callout_index: A::CALLOUT_INDEX,
            },
        )?;
        let input = A::into_input(output);
        let (callout_index, data) = P::Callout::to_event_data(&input);
        Ok(self.run_input(
            Event::InputReceived {
                callout_index,
                data,
                continuation_tag: None,
            },
            |ctx| P::on_input(ctx, input),
        ))
    }

    /// Resolve a generated signing continuation with an explicit pending id.
    ///
    /// Re-dispatches through the program's native signed-continuation handler.
    fn resolve_sign_with_pending_id(
        &mut self,
        pending_id: Option<PendingId>,
        signature: Vec<u8>,
    ) -> Result<HandlerResult, PendingHarnessError> {
        self.pending
            .validate(pending_id, arena0_protocol::PendingOperation::Sign)?;
        Ok(self.run_program(
            Event::Signed {
                signature: signature.clone(),
                continuation_tag: None,
            },
            |ctx| P::__arena0_on_signed(ctx, signature),
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
    use crate::{ManagedPhase, PhasedSharedState, ProgramQuery, ProgramValue, SharedState};
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

    struct FaultyCallout;

    impl Arena0Callout for FaultyCallout {
        type Request = ();
        type Response = ();

        fn schemas() -> Vec<arena0_program::CalloutSchema> {
            Vec::new()
        }

        fn from_raw(_callout_index: u32, _data: Vec<u8>) -> Self::Response {}

        fn to_event_data(_response: &Self::Response) -> (u32, Vec<u8>) {
            (0, Vec::new())
        }
    }

    struct FaultyProgram;

    impl Program for FaultyProgram {
        type Shared = FaultyShared;
        type Local = ();
        type Phase = TestPhase;
        type Message = ();
        type Callout = FaultyCallout;
        type Input = ();
        type Params = ();
        type Outcome = ();

        fn outcome(_shared: &Self::Shared) -> Self::Outcome {}

        fn writer(_shared: &Self::Shared) -> Option<Participant> {
            None
        }

        fn on_message(
            ctx: &mut SharedContext<Self::Shared>,
            _from: Participant,
            _msg: Self::Message,
        ) -> MessageApply<Self> {
            ctx.shared_mut().value = 7;
            Err(anyhow!("bad message").into())
        }

        fn on_input(
            ctx: &mut Context<Self::Shared, Self::Local>,
            _input: Self::Input,
        ) -> Result<(), InputFault> {
            ctx.effects().broadcast(&());
            Err(InputFault::Retryable(anyhow!("try again")))
        }
    }

    impl ProgramQuery for FaultyProgram {
        type Query = ();

        fn query(_ctx: &SharedContext<Self::Shared>, _query: Self::Query) {}
    }

    fn peer_b() -> PeerId {
        PeerId([2; 32])
    }

    fn pending(id: u64, callout_index: u32) -> PendingRecord {
        PendingRecord {
            id: PendingId::new(id),
            operation: arena0_protocol::PendingOperation::Callout { callout_index },
            label: Some("choosing".into()),
            expected_type: Some("()".into()),
            continuation_tag: Some(7),
        }
    }

    #[test]
    fn program_fault_rolls_back_shared_and_filters_effects() {
        let mut h = TestHarness::<FaultyProgram>::new(());
        h.session_started(peer_b());

        let result = h.message(peer_b(), ());

        assert_eq!(h.shared().value, 0);
        assert!(result.has_state_fault());
        assert!(matches!(result.effects.as_slice(), [Effect::Fail { .. }]));
        assert_eq!(result.step().pre_state, result.step().post_state);
    }

    #[test]
    fn retryable_input_rolls_back_shared_and_filters_effects() {
        let mut h = TestHarness::<FaultyProgram>::new(());
        h.session_started(peer_b());

        let result = h.input(());

        assert_eq!(h.shared().value, 0);
        assert!(result.has_input_fault());
        assert!(matches!(
            result.effects.as_slice(),
            [Effect::RetryInput { .. }]
        ));
        assert!(result.step.is_none());
        assert!(matches!(
            result.private_record().map(|record| &record.event),
            Some(PrivateEvent::InputReceived { .. })
        ));
    }

    #[test]
    fn pending_validation_reports_wrong_type_stale_and_duplicate_results() {
        let mut h = TestHarness::<FaultyProgram>::new(());
        h.pending.set_active(Some(pending(10, 1)));

        h.pending
            .validate(
                Some(PendingId::new(10)),
                arena0_protocol::PendingOperation::Callout { callout_index: 1 },
            )
            .unwrap();
        let err = h
            .pending
            .validate(
                Some(PendingId::new(10)),
                arena0_protocol::PendingOperation::Callout { callout_index: 2 },
            )
            .unwrap_err();
        assert_eq!(
            err,
            PendingHarnessError::CalloutIndexMismatch {
                submitted: Some(2),
                pending: Some(1),
            }
        );
        let err = h
            .pending
            .validate(
                Some(PendingId::new(9)),
                arena0_protocol::PendingOperation::Callout { callout_index: 1 },
            )
            .unwrap_err();
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
        let err = h
            .pending
            .validate(
                Some(PendingId::new(10)),
                arena0_protocol::PendingOperation::Callout { callout_index: 1 },
            )
            .unwrap_err();
        assert_eq!(
            err,
            PendingHarnessError::Closed {
                pending_id: PendingId::new(10),
                reason: ClosedPendingReason::Resolved,
            }
        );
    }

    #[test]
    fn retryable_input_preserves_active_pending_continuation() {
        let mut h = TestHarness::<FaultyProgram>::new(());
        h.session_started(peer_b());
        h.pending.set_active(Some(pending(10, 0)));

        let result = h.input(());

        assert!(result.has_input_fault());
        assert_eq!(
            h.active_pending().map(|pending| pending.id),
            Some(PendingId::new(10))
        );
        assert!(h.closed_pending().is_empty());
    }

    #[test]
    fn harness_verifies_trace_replay_chain() {
        let mut h = TestHarness::<FaultyProgram>::new(());
        h.session_started(peer_b());
        h.message(peer_b(), ());

        h.verify_trace().unwrap();
    }

    #[test]
    fn harness_replays_trace_through_program() {
        let mut h = TestHarness::<FaultyProgram>::new(());
        h.session_started(peer_b());
        h.message(peer_b(), ());

        let report =
            TestHarness::<FaultyProgram>::replay_trace(PeerId([0; 32]), (), h.trace()).unwrap();
        // Public transcript: the boundary and message. Local reaction is in
        // the harness's private trace instead of the public transcript.
        assert_eq!(report.steps, 2);
        assert_eq!(
            report.final_state,
            h.trace().last().map(|step| step.post_state)
        );
    }

    #[test]
    fn harness_replay_reports_effect_pending_and_state_mismatches() {
        let mut h = TestHarness::<FaultyProgram>::new(());
        h.session_started(peer_b());
        h.message(peer_b(), ());
        let trace = h.trace().to_vec();

        let mut effect_mismatch = trace.clone();
        effect_mismatch[1].effects.clear();
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
