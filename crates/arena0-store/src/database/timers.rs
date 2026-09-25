use super::*;

impl Database {
    pub(crate) fn due_timers(
        &mut self,
        execution_id: ExecId,
        now_ms: u64,
        limit: usize,
    ) -> Result<Vec<ActiveTimer>, StoreError> {
        self.require_execution(execution_id)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit)
            .map_err(|_| StoreError::InvalidConfiguration("timer limit is too large"))?;
        let mut statement = self.connection.prepare(
            "SELECT timer_id, deadline_ms, payload, armed_version
             FROM active_timers WHERE execution_id = ?1 AND deadline_ms <= ?2
             ORDER BY deadline_ms, timer_id LIMIT ?3",
        )?;
        let mut rows =
            statement.query(params![execution_id.0.to_vec(), sqlite_u64(now_ms)?, limit])?;
        let mut timers = Vec::new();
        let mut response_bytes = 0;
        while let Some(row) = rows.next()? {
            let payload = open_envelope(
                EnvelopeKind::Timer,
                &row.get::<_, Vec<u8>>(2)?,
                MAX_TIMER_RECORD_BYTES,
            )?;
            account_response(&mut response_bytes, payload.len())?;
            timers.push(ActiveTimer {
                timer_id: TimerId::from_bytes(array32(&row.get::<_, Vec<u8>>(0)?, "timer id")?),
                deadline_ms: sqlite_i64(row.get::<_, i64>(1)?)?,
                timer: decode_borsh(&payload, "active timer payload")?,
                armed_version: ExecutionVersion::new(sqlite_i64(row.get::<_, i64>(3)?)?),
            });
        }
        Ok(timers)
    }

    /// Persist committed program effects without introducing another effect enum.
    pub(super) fn persist_effects(
        &mut self,
        execution_id: ExecId,
        event_position: u64,
        version: ExecutionVersion,
        effects: &[(u32, Effect)],
        now_ms: u64,
        _state: &ExecutionState,
    ) -> Result<(), StoreError> {
        for (ordinal, effect) in effects {
            match effect {
                Effect::SetTimer { delay_ms, timer } => self.persist_timer(
                    execution_id,
                    event_position,
                    version,
                    *ordinal,
                    *delay_ms,
                    timer,
                    now_ms,
                )?,
                // Lifecycle effects and broadcasts have no side rows: the
                // lifecycle effect is in the trace entry and the broadcast is
                // in the durable outgoing queue carried by the state blob.
                Effect::SessionEnd { .. }
                | Effect::SessionAbort { .. }
                | Effect::Fail { .. }
                | Effect::Broadcast { .. } => {}
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn persist_timer(
        &mut self,
        execution_id: ExecId,
        event_position: u64,
        version: ExecutionVersion,
        ordinal: u32,
        delay_ms: u64,
        timer: &TimerPayload,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let label = borsh::to_vec(&(execution_id, event_position, ordinal))
            .map_err(|error| StoreError::Corruption(format!("timer id encode: {error}")))?;
        let timer_id = TimerId::derive(&label);
        let deadline_ms = now_ms
            .checked_add(delay_ms)
            .ok_or_else(|| StoreError::Corruption("timer deadline overflows u64".into()))?;
        let payload = borsh::to_vec(timer)
            .map_err(|error| StoreError::Corruption(format!("timer payload encode: {error}")))?;
        if payload.len() > MAX_TIMER_RECORD_BYTES {
            return Err(StoreError::Corruption(
                "timer payload exceeds store bound".into(),
            ));
        }
        let payload = envelope(EnvelopeKind::Timer, &payload)?;
        let existing = self
            .connection
            .query_row(
                "SELECT deadline_ms, payload, armed_version FROM active_timers
             WHERE execution_id = ?1 AND timer_id = ?2",
                params![execution_id.0.to_vec(), timer_id.as_bytes().to_vec()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((old_deadline, old_payload, old_version)) = existing {
            if sqlite_i64(old_deadline)? != deadline_ms
                || old_payload != payload
                || sqlite_i64(old_version)? != version.get()
            {
                return Err(StoreError::Corruption(
                    "timer identity was reused with different evidence".into(),
                ));
            }
            return Ok(());
        }
        let count = sqlite_i64(self.connection.query_row(
            "SELECT COUNT(*) FROM active_timers WHERE execution_id = ?1",
            params![execution_id.0.to_vec()],
            |row| row.get::<_, i64>(0),
        )?)?;
        if count >= arena0_protocol::MAX_ACTIVE_TIMERS as u64 {
            return Err(StoreError::Protocol(ProtocolError::CollectionTooLarge {
                kind: "active timers",
                actual: usize::try_from(count)
                    .unwrap_or(usize::MAX)
                    .saturating_add(1),
                max: arena0_protocol::MAX_ACTIVE_TIMERS,
            }));
        }
        self.connection.execute(
            "INSERT INTO active_timers
             (execution_id, timer_id, deadline_ms, payload, armed_version)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                execution_id.0.to_vec(),
                timer_id.as_bytes().to_vec(),
                sqlite_u64(deadline_ms)?,
                payload,
                sqlite_u64(version.get())?
            ],
        )?;
        Ok(())
    }
}
