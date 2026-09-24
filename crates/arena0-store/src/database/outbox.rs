use super::*;

impl Database {
    pub(crate) fn lease_next_outbox(
        &mut self,
        execution_id: ExecId,
        now_ms: u64,
    ) -> Result<Option<LeasedOutbox>, StoreError> {
        self.transaction(|store| store.lease_next_outbox_in_transaction(execution_id, now_ms))
    }

    pub(super) fn lease_next_outbox_in_transaction(
        &mut self,
        execution_id: ExecId,
        now_ms: u64,
    ) -> Result<Option<LeasedOutbox>, StoreError> {
        self.require_execution(execution_id)?;
        self.recover_expired_leases_in_transaction(Some(execution_id), now_ms)?;
        // Only the causal head of each destination lane is eligible. A live
        // lease or retry delay for one peer cannot block another peer.
        let row = self
            .connection
            .query_row(
                "SELECT o.outbox_id, o.version, o.event_position, o.ordinal,
                    o.destination, o.payload_kind, o.payload, o.attempts,
                    o.status, o.available_at_ms, o.lease_id, o.lease_until_ms
             FROM outbox AS o
             WHERE o.execution_id = ?1 AND o.status = 'pending'
               AND o.available_at_ms <= ?2
               AND NOT EXISTS (
                 SELECT 1 FROM outbox AS prior
                 WHERE prior.execution_id = o.execution_id
                   AND prior.status NOT IN ('acknowledged', 'cancelled')
                   AND (prior.destination = o.destination
                        OR (prior.destination IS NULL AND o.destination IS NULL))
                   AND (
                     prior.event_position < o.event_position
                     OR (prior.event_position = o.event_position AND prior.version < o.version)
                     OR (prior.event_position = o.event_position AND prior.version = o.version
                         AND prior.ordinal < o.ordinal)
                     OR (prior.event_position = o.event_position AND prior.version = o.version
                         AND prior.ordinal = o.ordinal AND prior.outbox_id < o.outbox_id)
                   )
               )
             ORDER BY o.event_position, o.version, o.ordinal, o.outbox_id
             LIMIT 1",
                params![execution_id.0.to_vec(), sqlite_u64(now_ms)?],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<Vec<u8>>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Vec<u8>>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, i64>(9)?,
                        row.get::<_, Option<Vec<u8>>>(10)?,
                        row.get::<_, Option<i64>>(11)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            id_bytes,
            version,
            event_position,
            ordinal,
            destination,
            payload_kind,
            payload,
            attempts,
            status,
            available_at_ms,
            old_lease,
            old_until,
        )) = row
        else {
            return Ok(None);
        };
        if parse_outbox_status(&status)? != OutboxStatus::Pending
            || old_lease.is_some()
            || old_until.is_some()
        {
            return Err(StoreError::Corruption(
                "pending outbox row carries a lease".into(),
            ));
        }
        let outbox_id = OutboxId::from_bytes(array32(&id_bytes, "outbox id")?);
        let version = ExecutionVersion::new(sqlite_i64(version)?);
        let event_position = sqlite_i64(event_position)?;
        let ordinal = u32::try_from(sqlite_i64(ordinal)?)
            .map_err(|_| StoreError::Corruption("outbox ordinal exceeds u32".into()))?;
        let destination = destination
            .map(|bytes| array32(&bytes, "outbox destination").map(PeerId))
            .transpose()?;
        let payload_kind = parse_payload_kind(&payload_kind)?;
        validate_outbox_payload(payload_kind, &payload)?;
        let mut response_bytes = 0;
        account_response(&mut response_bytes, payload.len())?;
        if OutboxId::derive(execution_id, event_position, ordinal, destination, &payload)
            != outbox_id
        {
            return Err(StoreError::Corruption("outbox identity mismatch".into()));
        }
        let attempts = u32::try_from(sqlite_i64(attempts)?)
            .map_err(|_| StoreError::Corruption("outbox attempts exceeds u32".into()))?
            .checked_add(1)
            .ok_or_else(|| StoreError::Corruption("outbox attempt counter exhausted".into()))?;
        let available_at_ms = sqlite_i64(available_at_ms)?;
        let lease_until_ms = now_ms
            .checked_add(self.lease_duration_ms)
            .ok_or_else(|| StoreError::Corruption("outbox lease deadline overflows u64".into()))?;
        let lease_id = derive_lease_id(outbox_id, attempts, now_ms);
        let changed = self.connection.execute(
            "UPDATE outbox SET attempts = ?1, status = 'leased', lease_id = ?2,
                    lease_until_ms = ?3
             WHERE execution_id = ?4 AND outbox_id = ?5 AND status = 'pending'
               AND available_at_ms <= ?6",
            params![
                sqlite_u64(u64::from(attempts))?,
                lease_id.as_bytes().to_vec(),
                sqlite_u64(lease_until_ms)?,
                execution_id.0.to_vec(),
                outbox_id.as_bytes().to_vec(),
                sqlite_u64(now_ms)?,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Corruption(
                "outbox lease compare-and-set failed".into(),
            ));
        }
        Ok(Some(LeasedOutbox {
            item: OutboxItem {
                outbox_id,
                execution_id,
                version,
                event_position,
                ordinal,
                destination,
                payload_kind,
                payload,
                attempts,
                status: OutboxStatus::Leased,
                available_at_ms,
            },
            lease_id,
            lease_until_ms,
        }))
    }

    /// Return whether protocol-frame delivery for this execution is still
    /// unsettled. Acknowledged and cancelled rows are retained as delivery
    /// history, so only pending and leased rows count here.
    pub(crate) fn has_unsettled_frames(
        &mut self,
        execution_id: ExecId,
    ) -> Result<bool, StoreError> {
        self.require_execution(execution_id)?;
        let unsettled = self.connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM outbox
                 WHERE execution_id = ?1
                   AND payload_kind = 'frame'
                   AND status IN ('pending', 'leased')
             )",
            params![execution_id.0.to_vec()],
            |row| row.get::<_, i64>(0),
        )?;
        match unsettled {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(StoreError::Corruption(
                "outbox unsettled-frame EXISTS returned a non-boolean value".into(),
            )),
        }
    }

    pub(crate) fn acknowledge_outbox(
        &mut self,
        execution_id: ExecId,
        outbox_id: OutboxId,
        lease_id: LeaseId,
    ) -> Result<OutboxDeliveryOutcome, StoreError> {
        self.transaction(|store| {
            store.acknowledge_outbox_in_transaction(execution_id, outbox_id, lease_id)
        })
    }

    pub(super) fn acknowledge_outbox_in_transaction(
        &mut self,
        execution_id: ExecId,
        outbox_id: OutboxId,
        lease_id: LeaseId,
    ) -> Result<OutboxDeliveryOutcome, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT status, lease_id FROM outbox
             WHERE execution_id = ?1 AND outbox_id = ?2",
                params![execution_id.0.to_vec(), outbox_id.as_bytes().to_vec()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<Vec<u8>>>(1)?)),
            )
            .optional()?;
        let Some((status, stored_lease)) = row else {
            return Err(StoreError::OutboxNotFound(outbox_id));
        };
        match parse_outbox_status(&status)? {
            OutboxStatus::Acknowledged => Ok(OutboxDeliveryOutcome::AlreadyAcknowledged),
            OutboxStatus::Cancelled => Ok(OutboxDeliveryOutcome::AlreadyCancelled),
            OutboxStatus::Pending => Ok(OutboxDeliveryOutcome::NotLeased),
            OutboxStatus::Leased => {
                if stored_lease.as_deref() != Some(lease_id.as_bytes()) {
                    return Ok(OutboxDeliveryOutcome::LeaseMismatch);
                }
                let changed = self.connection.execute(
                    "UPDATE outbox SET status = 'acknowledged', lease_id = NULL,
                            lease_until_ms = NULL, last_error = NULL
                     WHERE execution_id = ?1 AND outbox_id = ?2 AND status = 'leased'
                       AND lease_id = ?3",
                    params![
                        execution_id.0.to_vec(),
                        outbox_id.as_bytes().to_vec(),
                        lease_id.as_bytes().to_vec()
                    ],
                )?;
                if changed != 1 {
                    return Ok(OutboxDeliveryOutcome::LeaseMismatch);
                }
                Ok(OutboxDeliveryOutcome::Acknowledged)
            }
        }
    }

    pub(crate) fn retry_outbox(
        &mut self,
        execution_id: ExecId,
        outbox_id: OutboxId,
        lease_id: LeaseId,
        now_ms: u64,
        reason: String,
    ) -> Result<OutboxDeliveryOutcome, StoreError> {
        self.transaction(|store| {
            store.retry_outbox_in_transaction(execution_id, outbox_id, lease_id, now_ms, reason)
        })
    }

    pub(super) fn retry_outbox_in_transaction(
        &mut self,
        execution_id: ExecId,
        outbox_id: OutboxId,
        lease_id: LeaseId,
        now_ms: u64,
        reason: String,
    ) -> Result<OutboxDeliveryOutcome, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT status, lease_id, attempts FROM outbox
             WHERE execution_id = ?1 AND outbox_id = ?2",
                params![execution_id.0.to_vec(), outbox_id.as_bytes().to_vec()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((status, stored_lease, attempts)) = row else {
            return Err(StoreError::OutboxNotFound(outbox_id));
        };
        match parse_outbox_status(&status)? {
            OutboxStatus::Acknowledged => Ok(OutboxDeliveryOutcome::AlreadyAcknowledged),
            OutboxStatus::Cancelled => Ok(OutboxDeliveryOutcome::AlreadyCancelled),
            OutboxStatus::Pending => Ok(OutboxDeliveryOutcome::NotLeased),
            OutboxStatus::Leased => {
                if stored_lease.as_deref() != Some(lease_id.as_bytes()) {
                    return Ok(OutboxDeliveryOutcome::LeaseMismatch);
                }
                let available_at_ms = now_ms.checked_add(self.retry_delay_ms).ok_or_else(|| {
                    StoreError::Corruption("outbox retry deadline overflows u64".into())
                })?;
                let changed = self.connection.execute(
                    "UPDATE outbox SET status = 'pending', available_at_ms = ?1,
                            lease_id = NULL, lease_until_ms = NULL, last_error = ?2
                     WHERE execution_id = ?3 AND outbox_id = ?4 AND status = 'leased'
                       AND lease_id = ?5",
                    params![
                        sqlite_u64(available_at_ms)?,
                        reason,
                        execution_id.0.to_vec(),
                        outbox_id.as_bytes().to_vec(),
                        lease_id.as_bytes().to_vec()
                    ],
                )?;
                if changed != 1 {
                    return Ok(OutboxDeliveryOutcome::LeaseMismatch);
                }
                Ok(OutboxDeliveryOutcome::Retried {
                    available_at_ms,
                    attempts: u32::try_from(sqlite_i64(attempts)?).map_err(|_| {
                        StoreError::Corruption("outbox attempts exceeds u32".into())
                    })?,
                })
            }
        }
    }

    pub(crate) fn recover_expired_leases(
        &mut self,
        execution_id: ExecId,
        now_ms: u64,
    ) -> Result<RecoveryReport, StoreError> {
        self.transaction(|store| {
            store.recover_expired_leases_in_transaction(Some(execution_id), now_ms)
        })
    }

    pub(crate) fn recover_all_expired_leases(
        &mut self,
        now_ms: u64,
    ) -> Result<RecoveryReport, StoreError> {
        self.transaction(|store| store.recover_expired_leases_in_transaction(None, now_ms))
    }

    pub(super) fn recover_expired_leases_in_transaction(
        &mut self,
        execution_id: Option<ExecId>,
        now_ms: u64,
    ) -> Result<RecoveryReport, StoreError> {
        let changed = match execution_id {
            Some(execution_id) => self.connection.execute(
                "UPDATE outbox SET status = 'pending', lease_id = NULL, lease_until_ms = NULL
                 WHERE execution_id = ?1 AND status = 'leased'
                   AND lease_until_ms IS NOT NULL AND lease_until_ms <= ?2",
                params![execution_id.0.to_vec(), sqlite_u64(now_ms)?],
            )?,
            None => self.connection.execute(
                "UPDATE outbox SET status = 'pending', lease_id = NULL, lease_until_ms = NULL
                 WHERE status = 'leased' AND lease_until_ms IS NOT NULL
                   AND lease_until_ms <= ?1",
                params![sqlite_u64(now_ms)?],
            )?,
        };
        Ok(RecoveryReport {
            expired_outbox_leases: u64::try_from(changed)
                .map_err(|_| StoreError::Corruption("lease recovery count overflow".into()))?,
        })
    }

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
                Effect::SessionEnd { .. } | Effect::SessionAbort { .. } | Effect::Fail { .. } => {}
                Effect::Broadcast { .. } => {
                    return Err(StoreError::Corruption(
                        "committed broadcast was not converted to a protocol frame".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Persist one frame independently for every remote participant.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn persist_frame_for_remotes(
        &mut self,
        execution_id: ExecId,
        event_position: u64,
        version: ExecutionVersion,
        ordinal: u32,
        frame: &ExecFrame,
        state: &ExecutionState,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        if state.execution_id() != execution_id || state.producer() != self.host_id {
            return Err(StoreError::Corruption(
                "outbound frame is not bound to the local execution".into(),
            ));
        }
        let payload = borsh::to_vec(frame)
            .map_err(|error| StoreError::Corruption(format!("outbox frame encode: {error}")))?;
        validate_outbox_payload(OutboxPayloadKind::Frame, &payload)?;
        for ticket in state.binding().activation().tickets() {
            let destination = ticket.data.signer;
            if destination != self.host_id {
                self.insert_outbox(
                    execution_id,
                    event_position,
                    version,
                    ordinal,
                    Some(destination),
                    OutboxPayloadKind::Frame,
                    payload.clone(),
                    now_ms,
                )?;
            }
        }
        Ok(())
    }

    /// Mark only protocol frames that identify the exact pending proposal as
    /// cancelled while stopping an unsigned proposal. A frame may already be
    /// leased or in flight; cancellation prevents another lease without
    /// claiming that a peer accepted the frame. Because the protocol rejects
    /// stopping once this Host has signed the proposal, an in-flight frame
    /// cannot acquire N-of-N agreement from this Host; the receiver still
    /// authenticates the complete commitment or message identity.
    pub(super) fn cancel_proposal_frames(
        &mut self,
        execution_id: ExecId,
        cursor: arena0_protocol::StepCursor,
    ) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT outbox_id, payload, status
             FROM outbox
             WHERE execution_id = ?1 AND payload_kind = 'frame'
               AND status IN ('pending', 'leased')",
        )?;
        let mut rows = statement.query(params![execution_id.0.to_vec()])?;
        let mut matching = Vec::new();
        while let Some(row) = rows.next()? {
            let outbox_id = OutboxId::from_bytes(array32(
                &row.get::<_, Vec<u8>>(0)?,
                "proposal frame outbox id",
            )?);
            let payload = row.get::<_, Vec<u8>>(1)?;
            let frame: ExecFrame = decode_borsh(&payload, "proposal frame")?;
            if frame_at_cursor(&frame, cursor) {
                matching.push(outbox_id);
            }
        }
        drop(rows);
        drop(statement);

        for outbox_id in matching {
            let changed = self.connection.execute(
                "UPDATE outbox SET status = 'cancelled', lease_id = NULL,
                        lease_until_ms = NULL, last_error = NULL
                 WHERE execution_id = ?1 AND outbox_id = ?2
                   AND status IN ('pending', 'leased')",
                params![execution_id.0.to_vec(), outbox_id.as_bytes().to_vec()],
            )?;
            if changed != 1 {
                return Err(StoreError::Corruption(
                    "proposal frame cancellation compare-and-set failed".into(),
                ));
            }
        }
        Ok(())
    }

    pub(super) fn validate_outbox_rows(&mut self) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT execution_id, outbox_id, version, event_position, ordinal,
                    destination, payload_kind, payload, attempts, status,
                    available_at_ms, lease_id, lease_until_ms, last_error FROM outbox",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let execution_id = ExecId(array32(&row.get::<_, Vec<u8>>(0)?, "outbox execution")?);
            let outbox_id = OutboxId::from_bytes(array32(&row.get::<_, Vec<u8>>(1)?, "outbox id")?);
            let _version = ExecutionVersion::new(sqlite_i64(row.get::<_, i64>(2)?)?);
            let event_position = sqlite_i64(row.get::<_, i64>(3)?)?;
            let ordinal = u32::try_from(sqlite_i64(row.get::<_, i64>(4)?)?)
                .map_err(|_| StoreError::Corruption("outbox ordinal exceeds u32".into()))?;
            let destination = row
                .get::<_, Option<Vec<u8>>>(5)?
                .map(|bytes| array32(&bytes, "outbox destination").map(PeerId))
                .transpose()?;
            let payload_kind = parse_payload_kind(&row.get::<_, String>(6)?)?;
            let payload = row.get::<_, Vec<u8>>(7)?;
            validate_outbox_payload(payload_kind, &payload)?;
            if OutboxId::derive(execution_id, event_position, ordinal, destination, &payload)
                != outbox_id
            {
                return Err(StoreError::Corruption("outbox identity mismatch".into()));
            }
            let _attempts = u32::try_from(sqlite_i64(row.get::<_, i64>(8)?)?)
                .map_err(|_| StoreError::Corruption("outbox attempts exceeds u32".into()))?;
            let status = parse_outbox_status(&row.get::<_, String>(9)?)?;
            let _available = sqlite_i64(row.get::<_, i64>(10)?)?;
            let lease = row.get::<_, Option<Vec<u8>>>(11)?;
            let until = row.get::<_, Option<i64>>(12)?;
            match status {
                OutboxStatus::Pending | OutboxStatus::Acknowledged | OutboxStatus::Cancelled
                    if lease.is_some() || until.is_some() =>
                {
                    return Err(StoreError::Corruption(
                        "non-leased outbox row carries lease".into(),
                    ));
                }
                OutboxStatus::Leased
                    if lease.as_deref().is_none_or(|bytes| bytes.len() != 32)
                        || until.is_none() =>
                {
                    return Err(StoreError::Corruption(
                        "leased outbox row has invalid lease".into(),
                    ));
                }
                _ => {}
            }
            if let Some(error) = row.get::<_, Option<String>>(13)?
                && error.len() > MAX_ERROR_BYTES
            {
                return Err(StoreError::Corruption("outbox error exceeds bound".into()));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_outbox(
        &mut self,
        execution_id: ExecId,
        event_position: u64,
        version: ExecutionVersion,
        ordinal: u32,
        destination: Option<PeerId>,
        payload_kind: OutboxPayloadKind,
        payload: Vec<u8>,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        validate_outbox_payload(payload_kind, &payload)?;
        let outbox_id =
            OutboxId::derive(execution_id, event_position, ordinal, destination, &payload);
        let existing = self
            .connection
            .query_row(
                "SELECT version, event_position, ordinal, destination, payload_kind, payload
             FROM outbox WHERE execution_id = ?1 AND outbox_id = ?2",
                params![execution_id.0.to_vec(), outbox_id.as_bytes().to_vec()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                    ))
                },
            )
            .optional()?;
        if let Some((
            old_version,
            old_position,
            old_ordinal,
            old_destination,
            old_kind,
            old_payload,
        )) = existing
        {
            let old_destination = old_destination
                .map(|bytes| array32(&bytes, "outbox destination").map(PeerId))
                .transpose()?;
            if sqlite_i64(old_version)? != version.get()
                || sqlite_i64(old_position)? != event_position
                || sqlite_i64(old_ordinal)? != u64::from(ordinal)
                || old_destination != destination
                || parse_payload_kind(&old_kind)? != payload_kind
                || old_payload != payload
            {
                return Err(StoreError::Corruption(
                    "outbox identity was reused with different evidence".into(),
                ));
            }
            return Ok(());
        }
        self.connection.execute(
            "INSERT INTO outbox
             (execution_id, outbox_id, version, event_position, ordinal, destination,
              payload_kind, payload, attempts, status, available_at_ms,
              lease_id, lease_until_ms, last_error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, 'pending', ?9,
                     NULL, NULL, NULL)",
            params![
                execution_id.0.to_vec(),
                outbox_id.as_bytes().to_vec(),
                sqlite_u64(version.get())?,
                sqlite_u64(event_position)?,
                sqlite_u64(u64::from(ordinal))?,
                destination.map(|peer| peer.0.to_vec()),
                payload_kind_sql(payload_kind),
                payload,
                sqlite_u64(now_ms)?
            ],
        )?;
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

fn parse_payload_kind(value: &str) -> Result<OutboxPayloadKind, StoreError> {
    match value {
        "frame" => Ok(OutboxPayloadKind::Frame),
        _ => Err(StoreError::Corruption(format!(
            "unknown outbox payload kind {value}"
        ))),
    }
}

const fn payload_kind_sql(kind: OutboxPayloadKind) -> &'static str {
    match kind {
        OutboxPayloadKind::Frame => "frame",
    }
}

fn validate_outbox_payload(kind: OutboxPayloadKind, payload: &[u8]) -> Result<(), StoreError> {
    match kind {
        OutboxPayloadKind::Frame => {
            if payload.len() > MAX_FRAME_BYTES {
                return Err(StoreError::CommandTooLarge {
                    required: payload.len(),
                    capacity: MAX_FRAME_BYTES,
                });
            }
            let _: ExecFrame = decode_borsh(payload, "outbox execution frame")?;
        }
    }
    Ok(())
}

// Stop clears the staged proposal from the resulting state. Its agreed
// cursor still identifies every uncommitted frame to cancel; frames from
// earlier certified steps must remain delivery obligations.
fn frame_at_cursor(frame: &ExecFrame, cursor: arena0_protocol::StepCursor) -> bool {
    match frame {
        ExecFrame::StepSignature { commitment, .. } => {
            commitment.step == cursor.next_step()
                && commitment.pre_state == cursor.state_hash()
                && commitment.link == cursor.chain_hash()
        }
        ExecFrame::Message { seq, prestate, .. } => {
            *seq == cursor.next_step() && *prestate == cursor.state_hash()
        }
        ExecFrame::Abort { .. } => false,
    }
}
