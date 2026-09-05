use crate::PendingId;
use arena0_program::{LocalStateBytes, SharedStateBytes};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::trace::{PendingKind, PendingRecord, PrivateRecord, TraceEntry};
use crate::{ExecId, MessageId, PrivateEffect, PrivateEvent, PublicEffect, StateHash};

use super::{
    BroadcastFrame, DurableEffect, ExecutionState, GuestSignData, ProtocolError, TerminalOutcome,
    TimerFiring, TimerId, TimerMutation, validate_private_record, validate_trace_entry,
};

/// The explicit local cause paired with one private guest execution.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateCause {
    /// A normal local reaction or input that is not a timer firing.
    React,
    /// A specific active one-shot timer generation was consumed.
    Timer(TimerFiring),
    /// A response resumes one specific stored continuation.
    Resume {
        pending_id: PendingId,
        kind: PendingKind,
    },
}

impl BorshSerialize for PrivateCause {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        match self {
            Self::React => BorshSerialize::serialize(&0u8, writer),
            Self::Timer(firing) => {
                BorshSerialize::serialize(&1u8, writer)?;
                BorshSerialize::serialize(firing, writer)
            }
            Self::Resume { pending_id, kind } => {
                BorshSerialize::serialize(&2u8, writer)?;
                BorshSerialize::serialize(pending_id, writer)?;
                BorshSerialize::serialize(kind, writer)
            }
        }
    }
}

impl BorshDeserialize for PrivateCause {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => Ok(Self::React),
            1 => Ok(Self::Timer(TimerFiring::deserialize_reader(reader)?)),
            2 => Ok(Self::Resume {
                pending_id: PendingId::deserialize_reader(reader)?,
                kind: PendingKind::deserialize_reader(reader)?,
            }),
            tag => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown private cause tag {tag}"),
            )),
        }
    }
}

impl PrivateCause {
    /// Construct a normal local cause.
    #[must_use]
    pub const fn react() -> Self {
        Self::React
    }

    /// Construct a timer cause.
    #[must_use]
    pub const fn timer(firing: TimerFiring) -> Self {
        Self::Timer(firing)
    }

    /// Construct a correlated continuation-resume cause.
    #[must_use]
    pub const fn resume(pending_id: PendingId, kind: PendingKind) -> Self {
        Self::Resume { pending_id, kind }
    }
}

/// A public delta proposed by local execution. It becomes public state only
/// after every participant has supplied a valid signature.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SharedDelta {
    pub(crate) entry: TraceEntry,
    pub(crate) shared_state: SharedStateBytes,
    pub(crate) terminal_outcome: Option<TerminalOutcome>,
}

impl SharedDelta {
    /// Construct a shared delta from one trace entry, opaque shared state, and
    /// the optional pair of guest-produced terminal projections.
    pub fn new(
        entry: TraceEntry,
        shared_state: SharedStateBytes,
        terminal_outcome: Option<TerminalOutcome>,
    ) -> Result<Self, ProtocolError> {
        let delta = Self {
            entry,
            shared_state,
            terminal_outcome,
        };
        delta.validate()?;
        Ok(delta)
    }

    /// Validate the delta's local shape before it is bound to an aggregate.
    pub(crate) fn validate(&self) -> Result<(), ProtocolError> {
        validate_trace_entry(&self.entry)?;
        if self.entry.post_state != StateHash::of(self.shared_state.as_bytes()) {
            return Err(ProtocolError::PublicStateHashMismatch);
        }
        let session_end_count = self
            .entry
            .effects
            .iter()
            .filter(|effect| matches!(effect, PublicEffect::SessionEnd { .. }))
            .count();
        match (&self.terminal_outcome, session_end_count) {
            (Some(outcome), 1) => {
                outcome.validate()?;
                let encoded = self
                    .entry
                    .completed_outcome()
                    .ok_or(ProtocolError::TerminalOutcomeRequired)?;
                if outcome.borsh() != encoded {
                    return Err(ProtocolError::OutcomeProjectionMismatch);
                }
            }
            (Some(_), _) => return Err(ProtocolError::TerminalOutcomeMismatch),
            (None, 0) => {}
            (None, _) => return Err(ProtocolError::TerminalOutcomeRequired),
        }
        Ok(())
    }

    /// Borrow the proposed public trace entry.
    #[must_use]
    pub const fn entry(&self) -> &TraceEntry {
        &self.entry
    }

    /// Borrow the proposed opaque shared state.
    #[must_use]
    pub const fn shared_state(&self) -> &SharedStateBytes {
        &self.shared_state
    }

    /// Borrow the guest-produced terminal projections, if this is a successful
    /// terminal shared delta.
    #[must_use]
    pub const fn terminal_outcome(&self) -> Option<&TerminalOutcome> {
        self.terminal_outcome.as_ref()
    }
}

/// The deterministic context needed to derive consequences of a private
/// guest record. It contains no pre-built durable effects.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PrivateContext {
    observed_now_ms: u64,
}

impl PrivateContext {
    /// Construct deterministic timer context. The reducer computes every timer
    /// identity from execution/private coordinates and every deadline from this
    /// explicit base time plus the guest delay.
    #[must_use]
    pub const fn new(observed_now_ms: u64) -> Self {
        Self { observed_now_ms }
    }

    /// Return the deterministic base time supplied by the timer adapter.
    #[must_use]
    pub const fn observed_now_ms(&self) -> u64 {
        self.observed_now_ms
    }

    pub(crate) const fn validate(&self) -> Result<(), ProtocolError> {
        Ok(())
    }
}

/// A local delta. Its durable consequences are derived from the validated
/// guest record and explicit deterministic context; callers cannot inject an
/// unrelated effect or timer list.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PrivateDelta {
    pub(crate) execution_id: ExecId,
    pub(crate) record: PrivateRecord,
    pub(crate) local_state: LocalStateBytes,
    pub(crate) context: PrivateContext,
    pub(crate) cause: PrivateCause,
}

impl PrivateDelta {
    /// Construct a private delta with its explicit local cause.
    pub fn from_record(
        execution_id: ExecId,
        record: PrivateRecord,
        local_state: LocalStateBytes,
        context: PrivateContext,
        cause: PrivateCause,
    ) -> Result<Self, ProtocolError> {
        validate_private_record(&record)?;
        validate_cause_event(&record.event, cause)?;
        validate_pending_shape(execution_id, &record)?;
        Ok(Self {
            execution_id,
            record,
            local_state,
            context,
            cause,
        })
    }

    /// Return the execution identity to which this private input is bound.
    #[must_use]
    pub const fn execution_id(&self) -> ExecId {
        self.execution_id
    }

    pub(crate) fn validate(&self) -> Result<(), ProtocolError> {
        validate_private_record(&self.record)?;
        validate_cause_event(&self.record.event, self.cause)?;
        validate_pending_shape(self.execution_id, &self.record)
    }

    /// Borrow the private trace record.
    #[must_use]
    pub const fn record(&self) -> &PrivateRecord {
        &self.record
    }

    /// Borrow replacement private state.
    #[must_use]
    pub const fn local_state(&self) -> &LocalStateBytes {
        &self.local_state
    }

    /// Borrow deterministic consequence context.
    #[must_use]
    pub const fn context(&self) -> &PrivateContext {
        &self.context
    }

    /// Return the explicit event cause.
    #[must_use]
    pub const fn cause(&self) -> PrivateCause {
        self.cause
    }

    pub(crate) fn derive_consequences(
        &self,
        state: &ExecutionState,
    ) -> Result<(Vec<DurableEffect>, Vec<TimerMutation>), ProtocolError> {
        if self.execution_id != state.execution_id() {
            return Err(ProtocolError::BindingMismatch);
        }
        self.context.validate()?;
        validate_cause_event(&self.record.event, self.cause)?;
        validate_pending_state(state, self.cause)?;

        let witness = self.record.witness_commitment();
        let mut effects = Vec::new();
        let mut timers = Vec::new();
        let retry_count = self
            .record
            .effects
            .iter()
            .filter(|effect| matches!(effect, PrivateEffect::RetryInput { .. }))
            .count();
        if retry_count != 0 {
            if retry_count != 1
                || self.record.effects.len() != 1
                || !matches!(
                    self.cause,
                    PrivateCause::Resume {
                        kind: PendingKind::Callout,
                        ..
                    }
                )
                || self.record.pending.is_some()
                || self.local_state != *state.local_state()
            {
                return Err(ProtocolError::InvalidPendingContinuation);
            }
            let pending = state
                .status()
                .pending()
                .map(|(pending, _)| pending.clone())
                .ok_or(ProtocolError::PendingContinuationMismatch)?;
            // PendingRecord is the durable continuation authority.  The
            // original callout context is intentionally not reconstructed
            // from guest input; the adapter replays the typed continuation.
            effects.push(DurableEffect::request_callout(pending, Vec::new())?);
            return Ok((effects, timers));
        }
        let broadcast_count = self
            .record
            .effects
            .iter()
            .filter(|effect| matches!(effect, PrivateEffect::Broadcast { .. }))
            .count();
        if broadcast_count > 1 {
            return Err(ProtocolError::MultipleBroadcasts);
        }
        if let PrivateCause::Timer(firing) = self.cause {
            let active = state
                .timers
                .iter()
                .find(|timer| timer.id == firing.timer_id)
                .ok_or(ProtocolError::StaleTimerFiring)?;
            timers.push(TimerMutation::cancel(active.id));
        }

        let mut pending_count = 0usize;
        for (effect_index, guest_effect) in self.record.effects.iter().enumerate() {
            match guest_effect {
                PrivateEffect::Broadcast { data } => {
                    let message_id = MessageId::derive(
                        state.binding.session_id(),
                        state.producer,
                        self.record.after_position,
                        state.public.state_hash,
                        data,
                        witness,
                    );
                    let frame = BroadcastFrame::new(
                        message_id,
                        self.record.after_position,
                        state.public.state_hash,
                        data.clone(),
                        witness,
                    )?;
                    for (destination, _) in state.binding.participant_keys()? {
                        if destination == state.producer {
                            effects.push(DurableEffect::apply_broadcast(frame.clone())?);
                        } else {
                            effects
                                .push(DurableEffect::send_broadcast(destination, frame.clone())?);
                        }
                    }
                }
                PrivateEffect::SetTimer {
                    timer: guest_timer,
                    delay_ms,
                } => {
                    let timer_id = derive_timer_id(
                        state.execution_id,
                        state.private.next_record,
                        effect_index,
                    );
                    let payload = guest_timer
                        .as_ref()
                        .map(|timer| timer.data.clone())
                        .unwrap_or_default();
                    let deadline_ms = self
                        .context
                        .observed_now_ms
                        .checked_add(*delay_ms)
                        .ok_or(ProtocolError::TimerDeadlineOverflow)?;
                    timers.push(TimerMutation::arm(timer_id, deadline_ms, payload)?);
                }
                PrivateEffect::Callout { context, .. } => {
                    pending_count += 1;
                    let pending = self
                        .record
                        .pending
                        .clone()
                        .ok_or(ProtocolError::InvalidPendingContinuation)?;
                    if pending.id
                        != pending_id(state.execution_id, state.private.next_record, effect_index)
                    {
                        return Err(ProtocolError::InvalidPendingContinuation);
                    }
                    effects.push(DurableEffect::request_callout(pending, context.clone())?);
                }
                PrivateEffect::Sign { scheme, data, .. } => {
                    pending_count += 1;
                    let pending = self
                        .record
                        .pending
                        .clone()
                        .ok_or(ProtocolError::InvalidPendingContinuation)?;
                    if pending.id
                        != pending_id(state.execution_id, state.private.next_record, effect_index)
                    {
                        return Err(ProtocolError::InvalidPendingContinuation);
                    }
                    let sign_data = GuestSignData::new(
                        state.binding.session_id(),
                        state.binding.program_hash(),
                        state.execution_id(),
                        state.private.next_record,
                        u32::try_from(effect_index).map_err(|_| {
                            ProtocolError::CollectionTooLarge {
                                kind: "private effects",
                                actual: effect_index,
                                max: super::MAX_PRIVATE_EFFECTS,
                            }
                        })?,
                        *scheme,
                        data.clone(),
                    )?;
                    effects.push(DurableEffect::request_signature(pending, sign_data)?);
                }
                PrivateEffect::RetryInput { reason } => {
                    let Some((pending, _)) = state.status().pending() else {
                        return Err(ProtocolError::PendingContinuationMismatch);
                    };
                    effects.push(DurableEffect::retry_input(pending.id, reason.clone())?);
                }
            }
        }
        if pending_count > 1 {
            return Err(ProtocolError::InvalidPendingContinuation);
        }
        Ok((effects, timers))
    }
}

fn validate_cause_event(event: &PrivateEvent, cause: PrivateCause) -> Result<(), ProtocolError> {
    match cause {
        PrivateCause::React => {
            if !matches!(event, PrivateEvent::React) {
                return Err(ProtocolError::PrivateCauseMismatch);
            }
        }
        PrivateCause::Timer(_) => {
            if !matches!(
                event,
                PrivateEvent::TimerFired | PrivateEvent::TypedTimerFired { .. }
            ) {
                return Err(ProtocolError::PrivateCauseMismatch);
            }
        }
        PrivateCause::Resume { kind, .. } => {
            let actual = match event {
                PrivateEvent::InputReceived { .. } => PendingKind::Callout,
                PrivateEvent::Signed { .. } => PendingKind::Sign,
                _ => return Err(ProtocolError::PrivateCauseMismatch),
            };
            if actual != kind {
                return Err(ProtocolError::PendingContinuationMismatch);
            }
        }
    }
    Ok(())
}

fn validate_pending_shape(
    execution_id: ExecId,
    record: &PrivateRecord,
) -> Result<(), ProtocolError> {
    let pending_effects = record
        .effects
        .iter()
        .filter(|effect| {
            matches!(
                effect,
                PrivateEffect::Callout { .. } | PrivateEffect::Sign { .. }
            )
        })
        .collect::<Vec<_>>();
    if pending_effects.len() > 1 {
        return Err(ProtocolError::InvalidPendingContinuation);
    }
    match (pending_effects.first().copied(), record.pending.as_ref()) {
        (Some(effect), Some(pending)) => {
            let effect_index = record
                .effects
                .iter()
                .position(|candidate| std::ptr::eq(candidate, effect))
                .ok_or(ProtocolError::InvalidPendingContinuation)?;
            let expected_id = derive_pending_id(execution_id, record.seq, effect_index);
            if pending.id != expected_id {
                return Err(ProtocolError::InvalidPendingContinuation);
            }
            let expected = PendingRecord::from_effect(expected_id, effect)
                .ok_or(ProtocolError::InvalidPendingContinuation)?;
            if expected != *pending {
                return Err(ProtocolError::InvalidPendingContinuation);
            }
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(ProtocolError::InvalidPendingContinuation);
        }
        (None, None) => {}
    }
    Ok(())
}

pub(crate) fn pending_effect_index(record: &PrivateRecord) -> Option<u32> {
    record
        .effects
        .iter()
        .position(|effect| {
            matches!(
                effect,
                PrivateEffect::Callout { .. } | PrivateEffect::Sign { .. }
            )
        })
        .and_then(|index| u32::try_from(index).ok())
}

fn validate_pending_state(
    state: &ExecutionState,
    cause: PrivateCause,
) -> Result<(), ProtocolError> {
    match cause {
        PrivateCause::Resume { pending_id, kind } => {
            let pending = state
                .status()
                .pending()
                .map(|(pending, _)| pending)
                .ok_or(ProtocolError::PendingContinuationMismatch)?;
            if pending.id != pending_id || pending.operation.kind() != kind {
                return Err(ProtocolError::PendingContinuationMismatch);
            }
        }
        PrivateCause::React | PrivateCause::Timer(_) => {
            if state.status().pending().is_some() {
                return Err(ProtocolError::PendingContinuationMismatch);
            }
        }
    }
    Ok(())
}

fn derive_pending_id(
    execution_id: crate::ExecId,
    private_sequence: u64,
    effect_index: usize,
) -> PendingId {
    let bytes = borsh::to_vec(&(
        b"arena0/pending/v1",
        execution_id,
        private_sequence,
        effect_index as u32,
    ))
    .expect("pending id preimage is serializable");
    let digest = blake3::hash(&bytes);
    PendingId::new(u64::from_le_bytes(
        digest.as_bytes()[..8]
            .try_into()
            .expect("digest prefix has eight bytes"),
    ))
}

/// Derive the only legal continuation identity for one private execution
/// coordinate. The guest record may describe the continuation, but it cannot
/// choose an unrelated durable id.
pub fn pending_id(
    execution_id: crate::ExecId,
    private_sequence: u64,
    effect_index: usize,
) -> PendingId {
    derive_pending_id(execution_id, private_sequence, effect_index)
}

fn derive_timer_id(
    execution_id: crate::ExecId,
    private_sequence: u64,
    effect_index: usize,
) -> TimerId {
    let preimage = borsh::to_vec(&(
        b"arena0/timer-coordinate/v1",
        execution_id,
        private_sequence,
        u32::try_from(effect_index).expect("bounded private effects fit in u32"),
    ))
    .expect("timer coordinate is serializable");
    TimerId::derive(&preimage)
}
