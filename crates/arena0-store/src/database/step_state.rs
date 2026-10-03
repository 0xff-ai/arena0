//! Stored post-states of agreed steps.

use super::*;

impl ReadDb {
    /// The shared state after agreed step `step`: one primary-key read joined
    /// with its `agreed_steps` row. Opens the `StepState` envelope (bounded by
    /// `MAX_SHARED_STATE_BYTES`) and checks `StateHash::of_shared` of the bytes
    /// against `agreed_steps.post_state` on every read, so a damaged row is a
    /// `StoreError::Corruption`, never a wrong view (read-trust ruling:
    /// archived rows are checked when read). A missing `agreed_steps` or
    /// `step_states` row is also `Corruption`: callers only ask for agreed
    /// steps. Decodes no execution state, activation or trace entry.
    pub(crate) fn step_state(
        &self,
        execution_id: ExecId,
        step: u64,
    ) -> Result<SharedStateBytes, StoreError> {
        let (stored, post_state) = self
            .connection
            .query_row(
                "SELECT s.shared_state, a.post_state FROM step_states s JOIN agreed_steps a
             ON a.execution_id = s.execution_id AND a.step = s.step
             WHERE s.execution_id = ?1 AND s.step = ?2",
                params![execution_id.0.as_slice(), sqlite_u64(step)?],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?
            .ok_or_else(|| StoreError::Corruption("agreed step has no stored state".into()))?;
        let bytes = DurableEnvelope::open(
            EnvelopeKind::StepState,
            &stored,
            arena0_program::MAX_SHARED_STATE_BYTES,
        )?;
        let state = SharedStateBytes::try_new(bytes)
            .map_err(|error| StoreError::Corruption(error.to_string()))?;
        if arena0_protocol::StateHash::of_shared(&state).0.as_slice() != post_state {
            return Err(StoreError::Corruption(
                "stored step state does not match its post-state".into(),
            ));
        }
        Ok(state)
    }
}
