use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crate::ExecId;

use super::{
    DurableEffect, ExecutionState, ExecutionVersion, MAX_COMMIT_PLAN_BYTES, MAX_OUTBOX_OCCURRENCES,
    MAX_TIMER_MUTATIONS, OccurrenceEvidence, OutboxIntent, PLAN_DOMAIN, PrivateCommit,
    ProtocolError, SharedCommit, TerminalPublication, TimerMutation, ensure_encoded,
};

/// The only durable result a reducer can return. Its constructor is private to
/// this module; adapters can inspect a plan through getters but cannot forge it.
#[derive(BorshSerialize, Serialize, Debug, Clone, PartialEq)]
pub struct CommitPlan {
    plan_id: PlanId,
    execution_id: ExecId,
    expected_version: ExecutionVersion,
    next_version: ExecutionVersion,
    next_state: ExecutionState,
    shared: Option<SharedCommit>,
    private: Option<PrivateCommit>,
    terminal: Option<TerminalPublication>,
    timers: Vec<TimerMutation>,
    outbox: Vec<OutboxIntent>,
    occurrence: OccurrenceEvidence,
}

/// Reducer-owned semantic plan parts. Identity is added only after the
/// top-level transition has computed the input occurrence from the exact
/// input/pre-state pair.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlanDraft {
    execution_id: ExecId,
    expected_version: ExecutionVersion,
    next_version: ExecutionVersion,
    next_state: ExecutionState,
    shared: Option<SharedCommit>,
    private: Option<PrivateCommit>,
    terminal: Option<TerminalPublication>,
    timers: Vec<TimerMutation>,
    outbox: Vec<OutboxIntent>,
}

/// Result of applying one explicit execution input.
///
/// A duplicate signature that already exists in durable aggregate state is a
/// successful redelivery acknowledgement. It does not allocate a new version
/// or plan. Every other accepted input returns a complete commit plan.
#[derive(Debug, Clone, PartialEq)]
pub enum TransitionOutcome {
    /// A new durable plan was derived.
    Commit(Box<CommitPlan>),
    /// The exact evidence was already durably applied.
    AlreadyApplied,
}

impl TransitionOutcome {
    /// Borrow a newly derived plan, or `None` for an already-applied retry.
    #[must_use]
    pub const fn as_commit(&self) -> Option<&CommitPlan> {
        match self {
            Self::Commit(plan) => Some(plan),
            Self::AlreadyApplied => None,
        }
    }

    /// Whether this result acknowledges exact evidence already in durable
    /// aggregate state without changing it.
    #[must_use]
    pub const fn is_already_applied(&self) -> bool {
        matches!(self, Self::AlreadyApplied)
    }
}

/// Identity of one complete commit plan.
#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
#[serde(transparent)]
pub struct PlanId([u8; 32]);

impl PlanId {
    /// Construct an id from persisted bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow id bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl CommitPlan {
    /// Return the plan identity.
    #[must_use]
    pub const fn id(&self) -> PlanId {
        self.plan_id
    }

    /// Return the owning execution identity.
    #[must_use]
    pub const fn execution_id(&self) -> ExecId {
        self.execution_id
    }

    /// Return the version expected by the atomic store CAS.
    #[must_use]
    pub const fn expected_version(&self) -> ExecutionVersion {
        self.expected_version
    }

    /// Return the version persisted by this plan.
    #[must_use]
    pub const fn next_version(&self) -> ExecutionVersion {
        self.next_version
    }

    /// Borrow the complete post-commit state.
    #[must_use]
    pub const fn next_state(&self) -> &ExecutionState {
        &self.next_state
    }

    /// Borrow public commit material, if this plan certifies a shared step.
    #[must_use]
    pub const fn shared(&self) -> Option<&SharedCommit> {
        self.shared.as_ref()
    }

    /// Borrow private commit material, if this plan records a local step.
    #[must_use]
    pub const fn private(&self) -> Option<&PrivateCommit> {
        self.private.as_ref()
    }

    /// Borrow terminal publication material, if this plan publishes proof.
    #[must_use]
    pub const fn terminal(&self) -> Option<&TerminalPublication> {
        self.terminal.as_ref()
    }

    /// Borrow timer mutations committed atomically with the plan.
    #[must_use]
    pub fn timers(&self) -> &[TimerMutation] {
        &self.timers
    }

    /// Borrow durable outbox occurrences committed atomically with the plan.
    #[must_use]
    pub fn outbox(&self) -> &[OutboxIntent] {
        &self.outbox
    }

    /// Return the semantic input occurrence committed by this plan.
    #[must_use]
    pub const fn occurrence(&self) -> OccurrenceEvidence {
        self.occurrence
    }

    /// Encode a plan for diagnostics or an audit record. Decoding a plan is
    /// intentionally not part of the public API.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        let bytes =
            borsh::to_vec(self).map_err(|error| ProtocolError::Serialization(error.to_string()))?;
        ensure_encoded("commit plan", bytes.len(), MAX_COMMIT_PLAN_BYTES)?;
        Ok(bytes)
    }

    /// Finalize reducer-owned plan parts with the occurrence computed from
    /// the exact input and pre-state.
    pub(crate) fn finalize(
        draft: PlanDraft,
        occurrence: OccurrenceEvidence,
    ) -> Result<Self, ProtocolError> {
        if occurrence.key().execution_id() != draft.execution_id {
            return Err(ProtocolError::BindingMismatch);
        }
        let next_state = draft.next_state;
        next_state.validate_recovered()?;
        let identity = PlanIdentity {
            execution_id: draft.execution_id,
            expected_version: draft.expected_version,
            next_version: draft.next_version,
            next_state: &next_state,
            shared: &draft.shared,
            private: &draft.private,
            terminal: &draft.terminal,
            timers: &draft.timers,
            outbox: &draft.outbox,
            occurrence,
        };
        let plan_id = derive_plan_id(&identity)?;
        let plan = Self {
            plan_id,
            execution_id: draft.execution_id,
            expected_version: draft.expected_version,
            next_version: draft.next_version,
            next_state,
            shared: draft.shared,
            private: draft.private,
            terminal: draft.terminal,
            timers: draft.timers,
            outbox: draft.outbox,
            occurrence,
        };
        let encoded_len = borsh::to_vec(&plan)
            .map_err(|error| ProtocolError::Serialization(error.to_string()))?
            .len();
        ensure_encoded("commit plan", encoded_len, MAX_COMMIT_PLAN_BYTES)?;
        Ok(plan)
    }
}

/// Construct one validated plan and derive every identity from its content.
/// This is crate-private so external adapters cannot manufacture a plan.
pub(crate) fn build_plan(
    state: &ExecutionState,
    mut next_state: ExecutionState,
    shared: Option<SharedCommit>,
    private: Option<PrivateCommit>,
    terminal: Option<TerminalPublication>,
    timers: Vec<TimerMutation>,
    effects: Vec<DurableEffect>,
) -> Result<PlanDraft, ProtocolError> {
    if next_state.execution_id != state.execution_id
        || next_state.binding != state.binding
        || next_state.producer != state.producer
    {
        return Err(ProtocolError::BindingMismatch);
    }
    if timers.len() > MAX_TIMER_MUTATIONS {
        return Err(ProtocolError::CollectionTooLarge {
            kind: "timer mutations",
            actual: timers.len(),
            max: MAX_TIMER_MUTATIONS,
        });
    }
    if effects.len() > MAX_OUTBOX_OCCURRENCES {
        return Err(ProtocolError::CollectionTooLarge {
            kind: "outbox occurrences",
            actual: effects.len(),
            max: MAX_OUTBOX_OCCURRENCES,
        });
    }

    let expected_version = state.version;
    let next_version = expected_version
        .next()
        .ok_or(ProtocolError::VersionExhausted)?;
    next_state.version = next_version;

    let mut outbox = Vec::with_capacity(effects.len());
    for (ordinal, effect) in effects.into_iter().enumerate() {
        let ordinal = u32::try_from(ordinal).map_err(|_| ProtocolError::CollectionTooLarge {
            kind: "outbox occurrences",
            actual: ordinal,
            max: MAX_OUTBOX_OCCURRENCES,
        })?;
        outbox.push(OutboxIntent::new(
            state.execution_id,
            next_version,
            ordinal,
            effect,
        )?);
    }

    Ok(PlanDraft {
        execution_id: state.execution_id,
        expected_version,
        next_version,
        next_state,
        shared,
        private,
        terminal,
        timers,
        outbox,
    })
}

#[derive(BorshSerialize)]
struct PlanIdentity<'a> {
    execution_id: ExecId,
    expected_version: ExecutionVersion,
    next_version: ExecutionVersion,
    next_state: &'a ExecutionState,
    shared: &'a Option<SharedCommit>,
    private: &'a Option<PrivateCommit>,
    terminal: &'a Option<TerminalPublication>,
    timers: &'a [TimerMutation],
    outbox: &'a [OutboxIntent],
    occurrence: OccurrenceEvidence,
}

fn derive_plan_id(identity: &PlanIdentity<'_>) -> Result<PlanId, ProtocolError> {
    let encoded =
        borsh::to_vec(identity).map_err(|error| ProtocolError::Serialization(error.to_string()))?;
    ensure_encoded("commit plan", encoded.len(), MAX_COMMIT_PLAN_BYTES)?;
    let mut preimage = Vec::with_capacity(PLAN_DOMAIN.len() + encoded.len());
    preimage.extend_from_slice(PLAN_DOMAIN);
    preimage.extend_from_slice(&encoded);
    Ok(PlanId(*blake3::hash(&preimage).as_bytes()))
}
