//! The [`Harness`] contract for program testing, plus the result and error
//! types it exposes. [`crate::testing::TestHarness`] implements it for native
//! program tests, so test code can drive lifecycle dispatch and inspect the
//! resulting [`HandlerResult`].

use arena0_protocol::PendingId;
use arena0_protocol::{
    DivergenceDiagnostic, DivergenceKind, Effect, Event, OpenCallout, PeerId, StateHash,
};
use borsh::BorshSerialize;

use crate::{CalloutSpec, Program};

// ---------------------------------------------------------------------------
// Fault status
// ---------------------------------------------------------------------------

/// The fault status from a handler invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FaultStatus {
    /// Handler returned Ok.
    None,
    /// Session abort from a program fault.
    Abort(String),
    /// The input was rejected without consuming its callout continuation.
    Rejected(String),
}

/// Why a harness-local pending continuation was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedPendingReason {
    /// The pending result resumed successfully.
    Resolved,
    /// The pending point was aborted or replaced before a result arrived.
    Cancelled,
}

/// Closed pending callout retained for exact-once diagnostics in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedPendingRecord {
    pub pending: OpenCallout,
    pub reason: ClosedPendingReason,
}

/// Validation error for a submitted pending continuation result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingHarnessError {
    NoPending,
    Closed {
        pending_id: PendingId,
        reason: ClosedPendingReason,
    },
    MissingPendingId {
        pending_id: PendingId,
    },
    Stale {
        submitted_id: PendingId,
        pending_id: PendingId,
    },
    PendingIdMismatch {
        submitted_id: PendingId,
        pending_id: PendingId,
    },
    CalloutIndexMismatch {
        submitted: u32,
        pending: u32,
    },
}

impl std::fmt::Display for PendingHarnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPending => write!(f, "no pending continuation"),
            Self::Closed { pending_id, reason } => {
                write!(
                    f,
                    "pending continuation {pending_id} already closed as {reason:?}"
                )
            }
            Self::MissingPendingId { pending_id } => {
                write!(
                    f,
                    "missing pending_id for current pending continuation {pending_id}"
                )
            }
            Self::Stale {
                submitted_id,
                pending_id,
            } => write!(
                f,
                "stale pending_id: submitted {submitted_id}, current {pending_id}"
            ),
            Self::PendingIdMismatch {
                submitted_id,
                pending_id,
            } => write!(
                f,
                "pending_id mismatch: submitted {submitted_id}, pending {pending_id}"
            ),
            Self::CalloutIndexMismatch { submitted, pending } => {
                write!(
                    f,
                    "callout_index mismatch: submitted {submitted:?}, pending {pending:?}"
                )
            }
        }
    }
}

impl std::error::Error for PendingHarnessError {}

/// Shared exact-once ledger for pending callout continuations.
///
/// Native and Wasm harnesses differ in how they dispatch a result, but they
/// must agree on stale ids, callout-index checks, rejected-input preservation,
/// and closed continuation history. This type owns that policy so both
/// backends use the same transitions.
#[derive(Debug, Default)]
pub struct PendingLedger {
    active: Option<OpenCallout>,
    closed: Vec<ClosedPendingRecord>,
}

impl PendingLedger {
    /// Create a ledger with an optionally restored active callout.
    #[must_use]
    pub fn with_active(active: Option<OpenCallout>) -> Self {
        Self {
            active,
            closed: Vec::new(),
        }
    }

    /// Clear the active callout and its closed-history diagnostics.
    pub fn clear(&mut self) {
        self.active = None;
        self.closed.clear();
    }

    /// Set the active callout, primarily for snapshot-based test setup.
    pub fn set_active(&mut self, active: Option<OpenCallout>) {
        self.active = active;
    }

    /// Return the currently active callout, if any.
    #[must_use]
    pub fn active(&self) -> Option<&OpenCallout> {
        self.active.as_ref()
    }

    /// Return closed callout diagnostics retained by this ledger.
    #[must_use]
    pub fn closed(&self) -> &[ClosedPendingRecord] {
        &self.closed
    }

    /// Record a closed callout for a focused ledger test.
    pub fn record_closed(&mut self, pending: OpenCallout, reason: ClosedPendingReason) {
        self.closed.push(ClosedPendingRecord { pending, reason });
    }

    /// Validate a submitted answer's identity and callout index against the
    /// active/closed state, preserving the harness error distinctions.
    pub fn validate(
        &self,
        pending_id: Option<PendingId>,
        callout_index: u32,
    ) -> Result<(), PendingHarnessError> {
        let Some(active) = self.active.as_ref() else {
            if let Some(submitted_id) = pending_id
                && let Some(closed) = self
                    .closed
                    .iter()
                    .rev()
                    .find(|closed| closed.pending.id == submitted_id)
            {
                return Err(PendingHarnessError::Closed {
                    pending_id: submitted_id,
                    reason: closed.reason,
                });
            }
            return Err(PendingHarnessError::NoPending);
        };

        match pending_id {
            Some(submitted_id) if submitted_id != active.id => {
                if submitted_id < active.id {
                    return Err(PendingHarnessError::Stale {
                        submitted_id,
                        pending_id: active.id,
                    });
                }
                return Err(PendingHarnessError::PendingIdMismatch {
                    submitted_id,
                    pending_id: active.id,
                });
            }
            Some(_) => {}
            None => {
                return Err(PendingHarnessError::MissingPendingId {
                    pending_id: active.id,
                });
            }
        }

        if callout_index != active.callout_index {
            return Err(PendingHarnessError::CalloutIndexMismatch {
                submitted: callout_index,
                pending: active.callout_index,
            });
        }

        Ok(())
    }

    /// Apply the callout transition produced by one handler invocation.
    pub fn update(
        &mut self,
        previous_pending: Option<OpenCallout>,
        pending_close: Option<ClosedPendingReason>,
        fault: &FaultStatus,
        new_pending: Option<OpenCallout>,
    ) {
        if matches!(fault, FaultStatus::Rejected(_)) {
            self.active = previous_pending;
            return;
        }

        if let Some(previous_pending) = previous_pending
            && new_pending.as_ref().map(|pending| pending.id) != Some(previous_pending.id)
        {
            let reason = if matches!(fault, FaultStatus::None) {
                pending_close.unwrap_or(ClosedPendingReason::Cancelled)
            } else {
                ClosedPendingReason::Cancelled
            };
            self.closed.push(ClosedPendingRecord {
                pending: previous_pending,
                reason,
            });
        }
        self.active = new_pending;
    }
}

// ---------------------------------------------------------------------------
// Effects result wrapper
// ---------------------------------------------------------------------------

/// One native dispatch record.
///
/// Native tests use the same flat event/effect vocabulary as the guest ABI:
/// every accepted dispatch, including a reaction or an external answer,
/// records its complete event and effect list in one sequence. The shared
/// hashes remain useful convergence diagnostics; local state is intentionally
/// not hashed because it is participant-specific.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DispatchRecord {
    /// Zero-based native event-position sequence.
    pub event_position: u64,
    /// Event delivered to the program.
    pub event: Event,
    /// Effects emitted by this dispatch, including lifecycle effects.
    pub effects: Vec<Effect>,
    /// Shared hash before the dispatch.
    pub pre_state: StateHash,
    /// Shared hash after the dispatch or rollback.
    pub post_state: StateHash,
    /// Open callout left by this dispatch, if the program asked a question.
    pub pending: Option<OpenCallout>,
}

impl DispatchRecord {
    /// Whether this dispatch emitted a terminal lifecycle effect.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.effects.iter().any(|effect| {
            matches!(
                effect,
                Effect::SessionEnd { .. } | Effect::SessionAbort { .. } | Effect::Fail { .. }
            )
        })
    }

    /// The open callout derived from the resulting state, if any.
    #[must_use]
    pub fn open_callout(&self) -> Option<&OpenCallout> {
        self.pending.as_ref()
    }

    /// Verify the native event-position sequence and shared-state hash chain.
    pub fn verify_chain(trace: &[Self]) -> Result<(), DivergenceDiagnostic> {
        for (idx, record) in trace.iter().enumerate() {
            let expected_event_position = idx as u64;
            if record.event_position != expected_event_position {
                return Err(DivergenceDiagnostic::new_at(
                    record.event_position,
                    DivergenceKind::StepIndexMismatch,
                    "event_position",
                    expected_event_position,
                    record.event_position,
                ));
            }
            if let Some(next) = trace.get(idx + 1)
                && record.post_state != next.pre_state
            {
                return Err(DivergenceDiagnostic::new_at(
                    next.event_position,
                    DivergenceKind::ChainMismatch,
                    "pre_state",
                    record.post_state,
                    next.pre_state,
                ));
            }
        }
        Ok(())
    }

    /// Compare two native dispatch records, including effects and
    /// continuation metadata.
    pub fn compare_event(left: &Self, right: &Self) -> Result<(), DivergenceDiagnostic> {
        if left.event_position != right.event_position {
            return Err(DivergenceDiagnostic::new_at(
                left.event_position,
                DivergenceKind::StepIndexMismatch,
                "event_position",
                left.event_position,
                right.event_position,
            ));
        }
        if left.event != right.event {
            return Err(DivergenceDiagnostic::new_at(
                left.event_position,
                DivergenceKind::EventMismatch,
                "event",
                &left.event,
                &right.event,
            ));
        }
        if left.pre_state != right.pre_state {
            return Err(DivergenceDiagnostic::new_at(
                left.event_position,
                DivergenceKind::PreStateMismatch,
                "pre_state",
                left.pre_state,
                right.pre_state,
            ));
        }
        if left.effects != right.effects {
            return Err(DivergenceDiagnostic::new_at(
                left.event_position,
                DivergenceKind::EffectMismatch,
                "effects",
                &left.effects,
                &right.effects,
            ));
        }
        if left.pending != right.pending {
            return Err(DivergenceDiagnostic::new_at(
                left.event_position,
                DivergenceKind::EffectMismatch,
                "pending",
                &left.pending,
                &right.pending,
            ));
        }
        if left.post_state != right.post_state {
            return Err(DivergenceDiagnostic::new_at(
                left.event_position,
                DivergenceKind::PostStateMismatch,
                "post_state",
                left.post_state,
                right.post_state,
            ));
        }
        Ok(())
    }

    /// Compare two native dispatch histories for exact replay equivalence.
    pub fn compare_traces(left: &[Self], right: &[Self]) -> Result<(), DivergenceDiagnostic> {
        if left.len() != right.len() {
            return Err(DivergenceDiagnostic::new_at(
                left.len().max(right.len()) as u64,
                DivergenceKind::StepCountMismatch,
                "len",
                left.len(),
                right.len(),
            ));
        }
        Self::verify_chain(left)?;
        Self::verify_chain(right)?;
        for (left, right) in left.iter().zip(right) {
            Self::compare_event(left, right)?;
        }
        Ok(())
    }
}

/// The effects emitted by a single handler invocation, plus the fault
/// status if the handler returned `Err`.
#[derive(Debug)]
pub struct HandlerResult {
    pub effects: Vec<Effect>,
    pub logs: Vec<(String, String)>,
    pub fault: FaultStatus,
    /// Flat dispatch records produced by this harness call. An agreed event
    /// followed by its reaction therefore returns two records in order.
    pub records: Vec<DispatchRecord>,
    /// True when a message dispatch returned `ApplyDecision::Reject`:
    /// the state was restored and no trace entry was recorded.
    pub rejected: bool,
}

/// A typed open callout captured from a handler invocation.
#[derive(Debug, Clone)]
pub struct TypedCalloutRecord<A> {
    pub request: A,
}

impl HandlerResult {
    /// True if any effect is a `Broadcast`.
    #[must_use]
    pub fn has_broadcast(&self) -> bool {
        self.effects
            .iter()
            .any(|e| matches!(e, Effect::Broadcast { .. }))
    }

    /// True if the invocation left an open callout.
    #[must_use]
    pub fn has_callout(&self) -> bool {
        self.records
            .last()
            .is_some_and(|record| record.pending.is_some())
    }

    /// True if any effect is a `SessionEnd`.
    #[must_use]
    pub fn has_session_end(&self) -> bool {
        self.effects
            .iter()
            .any(|e| matches!(e, Effect::SessionEnd { .. }))
    }

    /// True if any effect is a `SetTimer`.
    #[must_use]
    pub fn has_set_timer(&self) -> bool {
        self.effects
            .iter()
            .any(|e| matches!(e, Effect::SetTimer { .. }))
    }

    #[must_use]
    pub fn has_input_rejection(&self) -> bool {
        matches!(&self.fault, FaultStatus::Rejected(_))
    }

    #[must_use]
    pub fn has_state_fault(&self) -> bool {
        matches!(&self.fault, FaultStatus::Abort(_))
    }

    /// Collect all `Broadcast` effects, deserializing the message payload.
    #[must_use]
    pub fn messages<T: borsh::BorshDeserialize>(&self) -> Vec<T> {
        self.effects
            .iter()
            .filter_map(|e| match e {
                Effect::Broadcast { data } => borsh::from_slice(data).ok(),
                _ => None,
            })
            .collect()
    }

    /// Return the final open callout if it matches the generated request type.
    #[must_use]
    pub fn typed_callouts<P, A>(&self) -> Vec<TypedCalloutRecord<A>>
    where
        P: Program,
        A: CalloutSpec<P> + serde::de::DeserializeOwned,
    {
        self.records
            .last()
            .into_iter()
            .filter_map(|record| {
                let callout = record.pending.as_ref()?;
                if callout.callout_index != A::CALLOUT_INDEX {
                    return None;
                }
                let request = serde_json::from_slice(&callout.context).ok()?;
                Some(TypedCalloutRecord { request })
            })
            .collect()
    }

    /// Return the single matching typed callout, panicking with context otherwise.
    #[must_use]
    pub fn expect_callout<P, A>(&self) -> TypedCalloutRecord<A>
    where
        P: Program,
        A: CalloutSpec<P> + serde::de::DeserializeOwned,
    {
        let mut callouts = self.typed_callouts::<P, A>();
        assert_eq!(
            callouts.len(),
            1,
            "expected exactly one callout named {}, found {}",
            A::CALLOUT_NAME,
            callouts.len(),
        );
        callouts.remove(0)
    }

    /// Return the primary dispatch record for this handler invocation.
    ///
    /// An agreed event may be followed by an explicit reaction, in which case
    /// this returns the first record and [`Self::records`] exposes both.
    #[must_use]
    pub fn record(&self) -> &DispatchRecord {
        self.records
            .first()
            .expect("handler result did not include a dispatch record")
    }
}

/// Build a flat dispatch record for native harnesses.
#[doc(hidden)]
#[must_use]
pub fn __dispatch_record(
    event_position: u64,
    event: Event,
    effects: Vec<Effect>,
    pre_state: StateHash,
    post_state: StateHash,
    pending: Option<OpenCallout>,
) -> DispatchRecord {
    DispatchRecord {
        event_position,
        event,
        effects,
        pre_state,
        post_state,
        pending,
    }
}

// ---------------------------------------------------------------------------
// Harness trait
// ---------------------------------------------------------------------------

/// Trait abstracting lifecycle dispatch for program testing.
///
/// [`crate::testing::TestHarness`] implements this for native execution.
///
/// The `resolve_*` methods are default methods: implementations dispatch
/// pending callout/sign results through the identical validate-then-resolve
/// wrapper below, so only backend-specific `*_with_pending_id` and
/// `active_pending` plumbing needs a per-backend body.
pub trait Harness<P: Program>
where
    P::Message: BorshSerialize,
{
    /// The local peer identity this harness was created with.
    fn peer_id(&self) -> PeerId;
    fn shared(&self) -> &P::Shared;
    fn local(&self) -> &P::Local;
    fn peer(&self) -> Option<&PeerId>;
    fn session_started(&mut self, peer: PeerId) -> HandlerResult;
    fn message(&mut self, from: PeerId, msg: P::Message) -> HandlerResult;
    fn input(&mut self, input: P::Input) -> HandlerResult;
    fn timer(&mut self) -> HandlerResult;
    fn typed_timer(&mut self, timer: crate::TimerPayload) -> HandlerResult;
    fn timer_value<T>(&mut self, timer: T) -> HandlerResult
    where
        T: borsh::BorshSerialize + borsh::BorshDeserialize + Clone + 'static,
    {
        self.typed_timer(crate::timer_payload(timer))
    }

    /// Currently open callout, if the last step left one.
    fn active_pending(&self) -> Option<&OpenCallout>;

    /// Resolve a generated typed callout with an explicit pending id.
    ///
    /// Backend-specific because each implementation owns its open-callout
    /// representation and dispatch path.
    fn resolve_callout_with_pending_id<A>(
        &mut self,
        pending_id: Option<PendingId>,
        output: A::Output,
    ) -> Result<HandlerResult, PendingHarnessError>
    where
        A: CalloutSpec<P>;

    /// Resolve a generated typed callout by delivering its exact output type.
    fn resolve_callout<A>(&mut self, output: A::Output) -> HandlerResult
    where
        A: CalloutSpec<P>,
    {
        self.try_resolve_callout::<A>(output)
            .expect("pending callout validation failed")
    }

    /// Resolve a generated typed callout after validating the open callout id.
    fn try_resolve_callout<A>(
        &mut self,
        output: A::Output,
    ) -> Result<HandlerResult, PendingHarnessError>
    where
        A: CalloutSpec<P>,
    {
        let pending_id = self.active_pending().map(|pending| pending.id);
        self.resolve_callout_with_pending_id::<A>(pending_id, output)
    }
}
