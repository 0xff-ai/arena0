//! The [`Harness`] contract for program testing, plus the result and error
//! types it exposes. [`crate::testing::TestHarness`] implements it for native
//! program tests, so test code can drive lifecycle dispatch and inspect the
//! resulting [`HandlerResult`].

use arena0_protocol::{
    AggregateAttestation, Effect, PeerId, PendingKind, PendingRecord, PrivateRecord, PublicEffect,
    PublicEvent, StateHash, TRACE_FORMAT_VERSION, TraceEntry,
};
use arena0_protocol::{PendingId, PendingOperation};
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
    /// Session abort (ProgramFault::Abort or InputFault::Unrecoverable).
    Abort(String),
    /// Retryable input error (InputFault::Retryable, on_input only).
    Retryable(String),
}

/// Why a harness-local pending continuation was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedPendingReason {
    /// The pending result resumed successfully.
    Resolved,
    /// The pending point was aborted or replaced before a result arrived.
    Cancelled,
}

/// Closed pending continuation retained for exact-once diagnostics in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedPendingRecord {
    pub pending: PendingRecord,
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
    PendingKindMismatch {
        submitted: PendingKind,
        pending: PendingKind,
    },
    CalloutIndexMismatch {
        submitted: Option<u32>,
        pending: Option<u32>,
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
            Self::PendingKindMismatch { submitted, pending } => {
                write!(
                    f,
                    "pending kind mismatch: submitted {submitted:?}, pending {pending:?}"
                )
            }
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

/// Shared exact-once ledger for pending callout and signing continuations.
///
/// Native and Wasm harnesses differ in how they dispatch a result, but they
/// must agree on stale ids, kind/index checks, retry preservation, and closed
/// continuation history. This type owns that policy so both backends use the
/// same transitions.
#[derive(Debug, Default)]
pub struct PendingLedger {
    active: Option<PendingRecord>,
    closed: Vec<ClosedPendingRecord>,
}

impl PendingLedger {
    /// Create a ledger with an optionally restored active continuation.
    #[must_use]
    pub fn with_active(active: Option<PendingRecord>) -> Self {
        Self {
            active,
            closed: Vec::new(),
        }
    }

    /// Clear the active continuation and its closed-history diagnostics.
    pub fn clear(&mut self) {
        self.active = None;
        self.closed.clear();
    }

    /// Set the active continuation, primarily for snapshot-based test setup.
    pub fn set_active(&mut self, active: Option<PendingRecord>) {
        self.active = active;
    }

    /// Return the currently active continuation, if any.
    #[must_use]
    pub fn active(&self) -> Option<&PendingRecord> {
        self.active.as_ref()
    }

    /// Return closed continuation diagnostics retained by this ledger.
    #[must_use]
    pub fn closed(&self) -> &[ClosedPendingRecord] {
        &self.closed
    }

    /// Record a closed continuation for a focused ledger test.
    pub fn record_closed(&mut self, pending: PendingRecord, reason: ClosedPendingReason) {
        self.closed.push(ClosedPendingRecord { pending, reason });
    }

    /// Validate a submitted continuation result against the active/closed
    /// state, preserving the harness error distinctions.
    pub fn validate(
        &self,
        pending_id: Option<PendingId>,
        operation: PendingOperation,
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

        if operation.kind() != active.operation.kind() {
            return Err(PendingHarnessError::PendingKindMismatch {
                submitted: operation.kind(),
                pending: active.operation.kind(),
            });
        }

        if let (
            PendingOperation::Callout {
                callout_index: submitted,
            },
            PendingOperation::Callout {
                callout_index: pending,
            },
        ) = (operation, active.operation)
            && submitted != pending
        {
            return Err(PendingHarnessError::CalloutIndexMismatch {
                submitted: Some(submitted),
                pending: Some(pending),
            });
        }

        Ok(())
    }

    /// Apply the pending transition produced by one handler invocation.
    pub fn update(
        &mut self,
        previous_pending: Option<PendingRecord>,
        pending_close: Option<ClosedPendingReason>,
        fault: &FaultStatus,
        new_pending: Option<PendingRecord>,
    ) {
        if let Some(new_pending) = new_pending {
            self.active = Some(new_pending);
            return;
        }

        if matches!(fault, FaultStatus::Retryable(_))
            && previous_pending
                .as_ref()
                .is_some_and(|pending| pending.operation.kind() == PendingKind::Callout)
        {
            self.active = previous_pending;
            return;
        }

        if let Some(previous_pending) = previous_pending {
            if pending_close.is_some() || matches!(fault, FaultStatus::Abort(_)) {
                let reason = if matches!(fault, FaultStatus::None) {
                    pending_close.unwrap_or(ClosedPendingReason::Cancelled)
                } else {
                    ClosedPendingReason::Cancelled
                };
                self.closed.push(ClosedPendingRecord {
                    pending: previous_pending,
                    reason,
                });
            } else {
                self.active = Some(previous_pending);
                return;
            }
        }
        self.active = None;
    }
}

// ---------------------------------------------------------------------------
// Effects result wrapper
// ---------------------------------------------------------------------------

/// The effects emitted by a single handler invocation, plus the fault
/// status if the handler returned `Err`.
#[derive(Debug)]
pub struct HandlerResult {
    pub effects: Vec<Effect>,
    pub logs: Vec<(String, String)>,
    pub fault: FaultStatus,
    /// Public trace entry, present only for a canonical shared event.
    pub step: Option<TraceEntry>,
    /// Private trace record, present only for a local handler run.
    pub private_record: Option<PrivateRecord>,
    /// True when a shared message apply returned `ApplyDecision::Reject`:
    /// the state was restored and no trace entry was recorded.
    pub rejected: bool,
}

/// A typed callout effect captured from a handler invocation.
#[derive(Debug, Clone)]
pub struct TypedCalloutRecord<A> {
    pub request: A,
    pub pending_label: Option<String>,
    pub expected_type: Option<String>,
}

impl HandlerResult {
    /// True if any effect is a `Broadcast`.
    #[must_use]
    pub fn has_broadcast(&self) -> bool {
        self.effects
            .iter()
            .any(|e| matches!(e, Effect::Broadcast { .. }))
    }

    /// True if any effect is a `Callout`.
    #[must_use]
    pub fn has_callout(&self) -> bool {
        self.effects
            .iter()
            .any(|e| matches!(e, Effect::Callout { .. }))
    }

    /// True if any effect is a `Sign`.
    #[must_use]
    pub fn has_sign(&self) -> bool {
        self.effects
            .iter()
            .any(|e| matches!(e, Effect::Sign { .. }))
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
    pub fn has_input_fault(&self) -> bool {
        matches!(&self.fault, FaultStatus::Retryable(_))
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

    /// Collect `Callout` effects matching a concrete generated callout request type.
    #[must_use]
    pub fn typed_callouts<P, A>(&self) -> Vec<TypedCalloutRecord<A>>
    where
        P: Program,
        A: CalloutSpec<P> + serde::de::DeserializeOwned,
    {
        self.effects
            .iter()
            .filter_map(|e| match e {
                Effect::Callout {
                    callout_index,
                    context,
                    pending_label,
                    expected_type,
                    ..
                } if *callout_index == A::CALLOUT_INDEX => {
                    let request = serde_json::from_slice(context).ok()?;
                    Some(TypedCalloutRecord {
                        request,
                        pending_label: pending_label.clone(),
                        expected_type: expected_type.clone(),
                    })
                }
                _ => None,
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

    /// Return the recorded state-machine step for this handler invocation.
    ///
    /// A harness populates this for successful dispatch plumbing. Tests that only
    /// care about effects can continue using the convenience predicates above.
    #[must_use]
    pub fn step(&self) -> &TraceEntry {
        self.step
            .as_ref()
            .expect("handler result did not include a step record")
    }

    /// Return the private record for this local handler invocation, when one
    /// was produced.
    #[must_use]
    pub fn private_record(&self) -> Option<&PrivateRecord> {
        self.private_record.as_ref()
    }
}

/// Build a runtime-shaped step record for harnesses.
#[doc(hidden)]
#[must_use]
pub fn __step_record(
    step: u64,
    event: PublicEvent,
    effects: Vec<PublicEffect>,
    pre_state: StateHash,
    post_state: StateHash,
) -> TraceEntry {
    TraceEntry {
        trace_version: TRACE_FORMAT_VERSION,
        step,
        event,
        effects,
        pre_state,
        post_state,
        fuel_used: 0,
        witness: None,
        agreement: AggregateAttestation::empty(),
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

    /// Currently active pending continuation, if the last step suspended.
    fn active_pending(&self) -> Option<&PendingRecord>;

    /// Resolve a generated typed callout with an explicit pending id.
    ///
    /// Backend-specific because each implementation owns its pending
    /// continuation representation and dispatch path.
    fn resolve_callout_with_pending_id<A>(
        &mut self,
        pending_id: Option<PendingId>,
        output: A::Output,
    ) -> Result<HandlerResult, PendingHarnessError>
    where
        A: CalloutSpec<P>;

    /// Resolve a generated signing continuation with an explicit pending id.
    ///
    /// Backend-specific for the same reason as [`resolve_callout_with_pending_id`](Self::resolve_callout_with_pending_id).
    fn resolve_sign_with_pending_id(
        &mut self,
        pending_id: Option<PendingId>,
        signature: Vec<u8>,
    ) -> Result<HandlerResult, PendingHarnessError>;

    /// Resolve a generated typed callout by delivering its exact output type.
    fn resolve_callout<A>(&mut self, output: A::Output) -> HandlerResult
    where
        A: CalloutSpec<P>,
    {
        self.try_resolve_callout::<A>(output)
            .expect("pending callout validation failed")
    }

    /// Resolve a generated typed callout after validating the active pending id.
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

    /// Resolve a generated signing continuation by delivering raw signature bytes.
    fn resolve_sign(&mut self, signature: Vec<u8>) -> HandlerResult {
        self.try_resolve_sign(signature)
            .expect("pending sign validation failed")
    }

    /// Resolve a generated signing continuation after validating the active pending id.
    fn try_resolve_sign(
        &mut self,
        signature: Vec<u8>,
    ) -> Result<HandlerResult, PendingHarnessError> {
        let pending_id = self.active_pending().map(|pending| pending.id);
        self.resolve_sign_with_pending_id(pending_id, signature)
    }
}
