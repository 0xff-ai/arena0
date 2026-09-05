use super::*;

impl Database {
    /// Recover the exact agent-facing request for the current pending
    /// continuation. Acknowledged outbox rows remain durable, so a process
    /// crash after delivery but before the answer can safely re-emit the same
    /// context or signing preimage.
    pub(super) fn list_pending_requests(
        &mut self,
        execution_id: ExecId,
    ) -> Result<Vec<PendingRequest>, StoreError> {
        let state = self
            .load_execution_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
        let Some((pending, _)) = state.status().pending() else {
            return Ok(Vec::new());
        };
        let pending_id = pending.id;
        let expected_operation = pending.operation;
        let expected_type = pending.expected_type.clone();
        let session_id = state.binding().session_id();
        let mut statement = self.connection.prepare(
            "SELECT outbox_id, status, effect FROM outbox
             WHERE execution_id = ?1 ORDER BY version, ordinal",
        )?;
        let mut rows = statement.query(params![execution_id.0.to_vec()])?;
        let mut requests = Vec::new();
        let mut response_bytes = 0;
        while let Some(row) = rows.next()? {
            let outbox_id = OutboxId::from_bytes(array32(
                &row.get::<_, Vec<u8>>(0)?,
                "pending request outbox id",
            )?);
            let status = parse_outbox_status(&row.get::<_, String>(1)?)?;
            let effect = decode_effect(&row.get::<_, Vec<u8>>(2)?)?;
            match effect {
                DurableEffect::RequestCallout { pending, context } if pending.id == pending_id => {
                    let arena0_protocol::PendingOperation::Callout { callout_index } =
                        pending.operation
                    else {
                        return Err(StoreError::Corruption(
                            "callout request names a signing continuation".into(),
                        ));
                    };
                    if pending.operation != expected_operation
                        || pending.expected_type != expected_type
                    {
                        return Err(StoreError::Corruption(
                            "durable callout request disagrees with pending continuation".into(),
                        ));
                    }
                    account_response(
                        &mut response_bytes,
                        context
                            .len()
                            .checked_add(128)
                            .ok_or(StoreError::CommandTooLarge {
                                required: usize::MAX,
                                capacity: MAX_RESPONSE_BYTES,
                            })?,
                    )?;
                    requests.push(PendingRequest::Callout {
                        outbox_id,
                        status,
                        pending_id,
                        callout_index,
                        context,
                        expected_type: pending.expected_type,
                    });
                }
                DurableEffect::RequestSignature { pending, data } if pending.id == pending_id => {
                    if expected_operation != arena0_protocol::PendingOperation::Sign
                        || pending.operation != expected_operation
                        || data.execution_id() != execution_id
                        || data.session_id() != session_id
                    {
                        return Err(StoreError::Corruption(
                            "durable signature request disagrees with pending continuation".into(),
                        ));
                    }
                    account_response(
                        &mut response_bytes,
                        borsh::to_vec(&data)
                            .map_err(|error| {
                                StoreError::Corruption(format!(
                                    "pending signature response encode: {error}"
                                ))
                            })?
                            .len()
                            .checked_add(128)
                            .ok_or(StoreError::CommandTooLarge {
                                required: usize::MAX,
                                capacity: MAX_RESPONSE_BYTES,
                            })?,
                    )?;
                    requests.push(PendingRequest::Signature {
                        outbox_id,
                        status,
                        pending_id,
                        data,
                    });
                }
                _ => {}
            }
        }
        drop(rows);
        drop(statement);
        if requests.len() != 1 {
            return Err(StoreError::Corruption(format!(
                "pending continuation has {} durable request effects",
                requests.len()
            )));
        }
        Ok(requests)
    }

    pub(super) fn lease_next_outbox(
        &mut self,
        execution_id: ExecId,
        now_ms: u64,
    ) -> Result<Option<LeasedOutbox>, StoreError> {
        // Most actor progress ticks have no ready outbox work. Validate the
        // execution and inspect only the causal head before opening the write
        // transaction; a pending item scheduled for later or a live lease
        // cannot be delivered yet. Expired leases still enter the transaction
        // so recovery and leasing remain one durable boundary.
        let now_sql = sqlite_u64(now_ms)?;
        let _ = self
            .load_execution_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
        let earliest: Option<(String, i64, Option<i64>)> = self
            .connection
            .query_row(
                "SELECT status, available_at_ms, lease_until_ms
                 FROM outbox WHERE execution_id = ?1 AND status <> 'acknowledged'
                 ORDER BY version, ordinal LIMIT 1",
                params![execution_id.0.to_vec()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let needs_transaction = match earliest {
            None => false,
            Some((status, available, lease_until)) => match status.as_str() {
                "pending" => sqlite_i64(available)? <= now_ms,
                "leased" => lease_until.is_some_and(|until| until <= now_sql),
                _ => true,
            },
        };
        if !needs_transaction {
            return Ok(None);
        }
        self.begin()?;
        let result = self.lease_next_outbox_in_transaction(execution_id, now_ms);
        match result {
            Ok(outcome) => self.commit_result(outcome),
            Err(error) => self.rollback_result(error),
        }
    }

    pub(super) fn lease_next_outbox_in_transaction(
        &mut self,
        execution_id: ExecId,
        now_ms: u64,
    ) -> Result<Option<LeasedOutbox>, StoreError> {
        let _ = self
            .load_execution_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
        self.recover_expired_leases_in_transaction(Some(execution_id), now_ms)?;
        let earliest: Option<(String, i64)> = self.connection.query_row(
            "SELECT status, available_at_ms FROM outbox WHERE execution_id = ?1 AND status <> 'acknowledged' ORDER BY version, ordinal LIMIT 1",
            params![execution_id.0.to_vec()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        if let Some((status, available)) = earliest
            && (status == "leased" || sqlite_i64(available)? > now_ms)
        {
            return Ok(None);
        }
        let mut statement = self.connection.prepare(
            "SELECT outbox_id, version, ordinal, effect, attempts, status,
                    available_at_ms, lease_id, lease_until_ms
             FROM outbox
             WHERE execution_id = ?1 AND status = 'pending'
             ORDER BY version, ordinal LIMIT 1",
        )?;
        let mut rows = statement.query(params![execution_id.0.to_vec()])?;
        let mut candidates = Vec::new();
        while let Some(row) = rows.next()? {
            candidates.push((
                array32(&row.get::<_, Vec<u8>>(0)?, "outbox id")?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, Option<Vec<u8>>>(7)?,
                row.get::<_, Option<i64>>(8)?,
            ));
        }
        drop(rows);
        drop(statement);
        let until = now_ms
            .checked_add(self.lease_duration_ms)
            .ok_or_else(|| StoreError::Corruption("outbox lease deadline overflows u64".into()))?;
        let mut leased = Vec::with_capacity(candidates.len());
        let mut response_bytes = 0;
        for (
            id_bytes,
            version,
            ordinal,
            effect_bytes,
            attempts,
            status,
            available,
            old_lease,
            old_until,
        ) in candidates
        {
            if parse_outbox_status(&status)? != OutboxStatus::Pending
                || old_lease.is_some()
                || old_until.is_some()
            {
                return Err(StoreError::Corruption(
                    "pending outbox row carries a lease".into(),
                ));
            }
            let version = ExecutionVersion::new(sqlite_i64(version)?);
            let ordinal = u32::try_from(sqlite_i64(ordinal)?)
                .map_err(|_| StoreError::Corruption("outbox ordinal exceeds u32".into()))?;
            let attempts = u32::try_from(sqlite_i64(attempts)?)
                .map_err(|_| StoreError::Corruption("outbox attempts exceeds u32".into()))?;
            let available_at_ms = sqlite_i64(available)?;
            // Outbox effects are causal: a later ordinal/version may not pass
            // an earlier effect that is still waiting for its availability.
            if available_at_ms > now_ms {
                break;
            }
            let effect = decode_effect(&effect_bytes)?;
            account_response(&mut response_bytes, effect_bytes.len())?;
            let outbox_id = OutboxId::from_bytes(id_bytes);
            let expected = OutboxId::derive(execution_id, version, ordinal, &effect)?;
            if expected != outbox_id {
                return Err(StoreError::Corruption(
                    "outbox identity does not match effect".into(),
                ));
            }
            let next_attempts = attempts
                .checked_add(1)
                .ok_or_else(|| StoreError::Corruption("outbox attempt counter exhausted".into()))?;
            let lease_id = derive_lease_id(outbox_id, next_attempts, now_ms);
            let changed = self.connection.execute(
                "UPDATE outbox SET attempts = ?1, status = 'leased',
                        lease_id = ?2, lease_until_ms = ?3
                 WHERE execution_id = ?4 AND outbox_id = ?5 AND status = 'pending'
                   AND available_at_ms <= ?6",
                params![
                    sqlite_u64(u64::from(next_attempts))?,
                    lease_id.as_bytes().to_vec(),
                    sqlite_u64(until)?,
                    execution_id.0.to_vec(),
                    id_bytes.to_vec(),
                    sqlite_u64(now_ms)?,
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::Corruption(
                    "outbox lease compare-and-set failed".into(),
                ));
            }
            leased.push(LeasedOutbox {
                item: OutboxItem {
                    outbox_id,
                    execution_id,
                    version,
                    ordinal,
                    effect,
                    attempts: next_attempts,
                    status: OutboxStatus::Leased,
                    available_at_ms,
                },
                lease_id,
                lease_until_ms: until,
            });
        }
        Ok(leased.into_iter().next())
    }

    pub(super) fn acknowledge_outbox(
        &mut self,
        execution_id: ExecId,
        outbox_id: OutboxId,
        lease_id: LeaseId,
    ) -> Result<OutboxDeliveryOutcome, StoreError> {
        self.begin()?;
        let result = self.acknowledge_outbox_in_transaction(execution_id, outbox_id, lease_id);
        match result {
            Ok(outcome) => self.commit_result(outcome),
            Err(error) => self.rollback_result(error),
        }
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
                        lease_id.as_bytes().to_vec(),
                    ],
                )?;
                if changed != 1 {
                    return Ok(OutboxDeliveryOutcome::LeaseMismatch);
                }
                Ok(OutboxDeliveryOutcome::Acknowledged)
            }
        }
    }

    pub(super) fn retry_outbox(
        &mut self,
        execution_id: ExecId,
        outbox_id: OutboxId,
        lease_id: LeaseId,
        now_ms: u64,
        reason: String,
    ) -> Result<OutboxDeliveryOutcome, StoreError> {
        self.begin()?;
        let result =
            self.retry_outbox_in_transaction(execution_id, outbox_id, lease_id, now_ms, reason);
        match result {
            Ok(outcome) => self.commit_result(outcome),
            Err(error) => self.rollback_result(error),
        }
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
            OutboxStatus::Pending => Ok(OutboxDeliveryOutcome::NotLeased),
            OutboxStatus::Leased => {
                if stored_lease.as_deref() != Some(lease_id.as_bytes()) {
                    return Ok(OutboxDeliveryOutcome::LeaseMismatch);
                }
                let available = now_ms.checked_add(self.retry_delay_ms).ok_or_else(|| {
                    StoreError::Corruption("outbox retry deadline overflows u64".into())
                })?;
                let changed = self.connection.execute(
                    "UPDATE outbox SET status = 'pending', available_at_ms = ?1,
                            lease_id = NULL, lease_until_ms = NULL, last_error = ?2
                     WHERE execution_id = ?3 AND outbox_id = ?4 AND status = 'leased'
                       AND lease_id = ?5",
                    params![
                        sqlite_u64(available)?,
                        reason,
                        execution_id.0.to_vec(),
                        outbox_id.as_bytes().to_vec(),
                        lease_id.as_bytes().to_vec(),
                    ],
                )?;
                if changed != 1 {
                    return Ok(OutboxDeliveryOutcome::LeaseMismatch);
                }
                let attempts = u32::try_from(sqlite_i64(attempts)?)
                    .map_err(|_| StoreError::Corruption("outbox attempts exceeds u32".into()))?;
                Ok(OutboxDeliveryOutcome::Retried {
                    available_at_ms: available,
                    attempts,
                })
            }
        }
    }

    pub(super) fn recover_expired_leases(
        &mut self,
        execution_id: ExecId,
        now_ms: u64,
    ) -> Result<RecoveryReport, StoreError> {
        self.begin()?;
        let result = self.recover_expired_leases_in_transaction(Some(execution_id), now_ms);
        match result {
            Ok(outcome) => self.commit_result(outcome),
            Err(error) => self.rollback_result(error),
        }
    }

    pub(super) fn recover_all_expired_leases(
        &mut self,
        now_ms: u64,
    ) -> Result<RecoveryReport, StoreError> {
        self.begin()?;
        let result = self.recover_expired_leases_in_transaction(None, now_ms);
        match result {
            Ok(outcome) => self.commit_result(outcome),
            Err(error) => self.rollback_result(error),
        }
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
                 WHERE status = 'leased' AND lease_until_ms IS NOT NULL AND lease_until_ms <= ?1",
                params![sqlite_u64(now_ms)?],
            )?,
        };
        Ok(RecoveryReport {
            expired_outbox_leases: u64::try_from(changed)
                .map_err(|_| StoreError::Corruption("lease recovery count overflow".into()))?,
        })
    }

    pub(super) fn due_timers(
        &mut self,
        execution_id: ExecId,
        now_ms: u64,
        limit: usize,
    ) -> Result<Vec<ActiveTimer>, StoreError> {
        let _ = self
            .load_execution(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
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
                arena0_protocol::MAX_TIMER_PAYLOAD_BYTES,
            )?;
            account_response(&mut response_bytes, payload.len())?;
            timers.push(ActiveTimer {
                timer_id: TimerId::from_bytes(array32(&row.get::<_, Vec<u8>>(0)?, "timer id")?),
                deadline_ms: sqlite_i64(row.get::<_, i64>(1)?)?,
                payload,
                armed_version: ExecutionVersion::new(sqlite_i64(row.get::<_, i64>(3)?)?),
            });
        }
        Ok(timers)
    }

    pub(super) fn persist_timers(
        &mut self,
        execution_id: ExecId,
        version: ExecutionVersion,
        plan: &CommitPlan,
    ) -> Result<(), StoreError> {
        for mutation in plan.timers() {
            match mutation {
                TimerMutation::Arm {
                    timer_id,
                    deadline_ms,
                    payload,
                } => {
                    if payload.len() > arena0_protocol::MAX_TIMER_PAYLOAD_BYTES {
                        return Err(StoreError::Corruption(
                            "timer payload exceeds protocol bound".into(),
                        ));
                    }
                    let encoded = envelope(EnvelopeKind::Timer, payload)?;
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
                    if let Some((deadline, old_payload, old_version)) = existing {
                        if sqlite_i64(deadline)? != *deadline_ms
                            || old_payload != encoded
                            || sqlite_i64(old_version)? != version.get()
                        {
                            return Err(StoreError::Corruption(
                                "timer identity was reused with different evidence".into(),
                            ));
                        }
                    } else {
                        self.connection.execute(
                            "INSERT INTO active_timers
                             (execution_id, timer_id, deadline_ms, payload, armed_version)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![
                                execution_id.0.to_vec(),
                                timer_id.as_bytes().to_vec(),
                                sqlite_u64(*deadline_ms)?,
                                encoded,
                                sqlite_u64(version.get())?,
                            ],
                        )?;
                    }
                }
                TimerMutation::Cancel { timer_id } => {
                    self.connection.execute(
                        "DELETE FROM active_timers WHERE execution_id = ?1 AND timer_id = ?2",
                        params![execution_id.0.to_vec(), timer_id.as_bytes().to_vec()],
                    )?;
                }
            }
        }
        let expected: std::collections::BTreeSet<_> = plan.next_state().active_timers().collect();
        let mut actual = std::collections::BTreeSet::new();
        let mut statement = self.connection.prepare(
            "SELECT timer_id FROM active_timers WHERE execution_id = ?1 ORDER BY timer_id",
        )?;
        let mut rows = statement.query(params![execution_id.0.to_vec()])?;
        while let Some(row) = rows.next()? {
            actual.insert(TimerId::from_bytes(array32(
                &row.get::<_, Vec<u8>>(0)?,
                "timer id",
            )?));
        }
        if expected != actual {
            return Err(StoreError::Corruption(
                "timer projection does not match next execution state".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn persist_outbox(
        &mut self,
        execution_id: ExecId,
        version: ExecutionVersion,
        plan: &CommitPlan,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        for intent in plan.outbox() {
            intent.validate()?;
            if intent.execution_id() != execution_id || intent.version() != version {
                return Err(StoreError::Corruption(
                    "outbox intent is not bound to its commit".into(),
                ));
            }
            let effect_bytes = borsh::to_vec(intent.effect()).map_err(|error| {
                StoreError::Corruption(format!("outbox effect encode: {error}"))
            })?;
            let outbox_id = intent.id();
            let expected =
                OutboxId::derive(execution_id, version, intent.ordinal(), intent.effect())?;
            if outbox_id != expected {
                return Err(StoreError::Corruption(
                    "outbox intent id is not derived from its effect".into(),
                ));
            }
            let existing = self
                .connection
                .query_row(
                    "SELECT version, ordinal, effect, attempts, status, available_at_ms,
                            lease_id, lease_until_ms, last_error
                     FROM outbox WHERE execution_id = ?1 AND outbox_id = ?2",
                    params![execution_id.0.to_vec(), outbox_id.as_bytes().to_vec()],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Vec<u8>>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, Option<Vec<u8>>>(6)?,
                            row.get::<_, Option<i64>>(7)?,
                            row.get::<_, Option<String>>(8)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((
                old_version,
                old_ordinal,
                old_effect,
                attempts,
                status,
                available,
                lease,
                until,
                error,
            )) = existing
            {
                if sqlite_i64(old_version)? != version.get()
                    || sqlite_i64(old_ordinal)? != u64::from(intent.ordinal())
                    || open_envelope(
                        EnvelopeKind::Effect,
                        &old_effect,
                        arena0_protocol::MAX_EFFECT_PAYLOAD_BYTES,
                    )? != effect_bytes
                    || sqlite_i64(attempts)? != 0
                    || parse_outbox_status(&status)? != OutboxStatus::Pending
                    || sqlite_i64(available)? != now_ms
                    || lease.is_some()
                    || until.is_some()
                    || error.is_some()
                {
                    return Err(StoreError::Corruption(
                        "outbox identity was reused with different evidence".into(),
                    ));
                }
            } else {
                self.connection.execute(
                    "INSERT INTO outbox
                     (execution_id, outbox_id, version, ordinal, effect, attempts, status,
                      available_at_ms, lease_id, lease_until_ms, last_error)
                     VALUES (?1, ?2, ?3, ?4, ?5, 0, 'pending', ?6, NULL, NULL, NULL)",
                    params![
                        execution_id.0.to_vec(),
                        outbox_id.as_bytes().to_vec(),
                        sqlite_u64(version.get())?,
                        sqlite_u64(u64::from(intent.ordinal()))?,
                        envelope(EnvelopeKind::Effect, &effect_bytes)?,
                        sqlite_u64(now_ms)?,
                    ],
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn validate_outbox_rows(&mut self) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT execution_id, outbox_id, version, ordinal, effect, attempts, status,
                    available_at_ms, lease_id, lease_until_ms, last_error FROM outbox",
        )?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let execution_id = ExecId(array32(&row.get::<_, Vec<u8>>(0)?, "outbox execution")?);
            let outbox_id = OutboxId::from_bytes(array32(&row.get::<_, Vec<u8>>(1)?, "outbox id")?);
            let version = ExecutionVersion::new(sqlite_i64(row.get::<_, i64>(2)?)?);
            let ordinal = u32::try_from(sqlite_i64(row.get::<_, i64>(3)?)?)
                .map_err(|_| StoreError::Corruption("outbox ordinal exceeds u32".into()))?;
            let effect = decode_effect(&row.get::<_, Vec<u8>>(4)?)?;
            if OutboxId::derive(execution_id, version, ordinal, &effect)? != outbox_id {
                return Err(StoreError::Corruption("outbox identity mismatch".into()));
            }
            let status = parse_outbox_status(&row.get::<_, String>(6)?)?;
            let lease = row.get::<_, Option<Vec<u8>>>(8)?;
            let until = row.get::<_, Option<i64>>(9)?;
            match status {
                OutboxStatus::Pending | OutboxStatus::Acknowledged
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
            let _ = u32::try_from(sqlite_i64(row.get::<_, i64>(5)?)?)
                .map_err(|_| StoreError::Corruption("outbox attempts exceeds u32".into()))?;
            let _ = sqlite_i64(row.get::<_, i64>(7)?)?;
            if let Some(error) = row.get::<_, Option<String>>(10)?
                && error.len() > MAX_ERROR_BYTES
            {
                return Err(StoreError::Corruption("outbox error exceeds bound".into()));
            }
        }
        Ok(())
    }
}
