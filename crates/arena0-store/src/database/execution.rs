use super::*;

impl Database {
    pub(crate) fn execution_end(
        &mut self,
        session_id: SessionHash,
    ) -> Result<Option<(ExecId, arena0_protocol::EndPhase)>, StoreError> {
        let row: Option<(Vec<u8>, i64, Vec<u8>)> = self.connection.query_row(
 "SELECT execution_id, end_phase, end_unconfirmed FROM executions WHERE session_id = ?1",
 params![session_id.0.to_vec()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
        row.map(|(id, phase, peers)| {
            Ok((
                ExecId(array32(&id, "execution id")?),
                decode_end(phase, &peers)?,
            ))
        })
        .transpose()
    }

    pub(crate) fn create_execution(
        &mut self,
        execution_id: ExecId,
        activation: Activation,
        producer: PeerId,
        shared_state: SharedStateBytes,
        local_state: LocalStateBytes,
        now_ms: u64,
    ) -> Result<CreateExecutionOutcome, StoreError> {
        let state = ExecutionState::new(
            execution_id,
            activation,
            producer,
            shared_state,
            local_state,
        )?;
        if state.producer() != self.host_id {
            return Err(StoreError::IdentityMismatch {
                database: self.host_id,
                requested: state.producer(),
            });
        }
        self.transaction(|store| store.create_execution_in_transaction(state, now_ms))
    }

    pub(super) fn create_execution_in_transaction(
        &mut self,
        state: ExecutionState,
        now_ms: u64,
    ) -> Result<CreateExecutionOutcome, StoreError> {
        let execution_id = state.execution_id();
        let activation = self
            .load_activation_in_transaction(execution_id)?
            .ok_or(StoreError::ActivationNotFound(execution_id))?;
        if !activation.is_committed() {
            return Err(StoreError::ActivationNotCommitted(execution_id));
        }
        let Some(committed) = activation.activation() else {
            return Err(StoreError::ActivationNotCommitted(execution_id));
        };
        if committed != state.binding().activation()
            || activation.session_id() != state.binding().session_id()
        {
            return Err(StoreError::Corruption(
                "execution binding does not match committed activation".into(),
            ));
        }
        if let Some(existing) = self.load_execution_in_transaction(execution_id)? {
            if existing == state {
                return Ok(CreateExecutionOutcome::AlreadyExists(existing));
            }
            return Err(StoreError::Corruption(
                "execution identity is already bound to a different state".into(),
            ));
        }
        self.insert_execution(&state, now_ms)?;
        Ok(CreateExecutionOutcome::Created(state))
    }

    pub(super) fn insert_execution(
        &mut self,
        state: &ExecutionState,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let bytes = state_bytes(state)?;
        self.connection.execute(
            "INSERT INTO executions
             (execution_id, host_id, producer, session_id, state,
              version, lifecycle, agreed_step, event_position,
              created_at_ms, updated_at_ms, end_phase, end_unconfirmed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11, ?12)",
            params![
                state.execution_id().0.to_vec(),
                self.host_id.0.to_vec(),
                state.producer().0.to_vec(),
                state.binding().session_id().0.to_vec(),
                envelope(EnvelopeKind::ExecutionState, &bytes)?,
                sqlite_u64(state.version().get())?,
                lifecycle_tag(state.lifecycle()),
                sqlite_u64(state.agreed_step())?,
                sqlite_u64(state.event_position())?,
                sqlite_u64(now_ms)?,
                end_columns(state.end_phase())?.0,
                end_columns(state.end_phase())?.1,
            ],
        )?;
        Ok(())
    }

    /// Timer scans need an execution identity, not its guest memory images.
    pub(super) fn require_execution(&self, execution_id: ExecId) -> Result<(), StoreError> {
        let exists: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM executions WHERE execution_id = ?1)",
            params![execution_id.0.to_vec()],
            |row| row.get(0),
        )?;
        if exists {
            Ok(())
        } else {
            Err(StoreError::ExecutionNotFound(execution_id))
        }
    }

    pub(crate) fn load_execution(
        &mut self,
        execution_id: ExecId,
    ) -> Result<Option<ExecutionState>, StoreError> {
        self.load_execution_in_transaction(execution_id)
    }

    pub(crate) fn list_executions(
        &mut self,
        limit: usize,
    ) -> Result<Vec<ExecutionState>, StoreError> {
        let limit = i64::try_from(limit)
            .map_err(|_| StoreError::InvalidConfiguration("execution limit is too large"))?;
        let mut statement = self
            .connection
            .prepare("SELECT execution_id FROM executions ORDER BY execution_id LIMIT ?1")?;
        let mut rows = statement.query(params![limit])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            ids.push(ExecId(array32(&row.get::<_, Vec<u8>>(0)?, "execution id")?));
        }
        drop(rows);
        drop(statement);
        let mut response_bytes = 0;
        ids.into_iter()
            .map(|id| {
                let state = self.load_execution(id)?.ok_or_else(|| {
                    StoreError::Corruption("execution disappeared while listing".into())
                })?;
                account_response(&mut response_bytes, state_bytes(&state)?.len())?;
                Ok(state)
            })
            .collect()
    }

    pub(super) fn load_execution_in_transaction(
        &mut self,
        execution_id: ExecId,
    ) -> Result<Option<ExecutionState>, StoreError> {
        self.load_execution_from_sqlite(execution_id)
    }

    pub(super) fn load_execution_from_sqlite(
        &mut self,
        execution_id: ExecId,
    ) -> Result<Option<ExecutionState>, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT host_id, producer, session_id, state,
                        version, lifecycle, agreed_step,
                        event_position
                 FROM executions WHERE execution_id = ?1",
                params![execution_id.0.to_vec()],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            host_bytes,
            producer_bytes,
            session_bytes,
            state_bytes,
            version,
            lifecycle,
            agreed_step,
            event_position,
        )) = row
        else {
            return Ok(None);
        };
        let host = peer_id_from_blob(&host_bytes, "execution host")?;
        if host != self.host_id {
            return Err(StoreError::IdentityMismatch {
                database: host,
                requested: self.host_id,
            });
        }
        self.decode_state_row(ExecutionIndexRow {
            execution_id,
            state_bytes,
            version,
            lifecycle,
            agreed_step,
            event_position,
            producer: peer_id_from_blob(&producer_bytes, "execution producer")?,
            session_id: SessionHash(array32(&session_bytes, "execution session")?),
        })
        .map(Some)
    }

    pub(super) fn decode_state_row(
        &self,
        row: ExecutionIndexRow,
    ) -> Result<ExecutionState, StoreError> {
        let ExecutionIndexRow {
            execution_id,
            state_bytes,
            version,
            lifecycle,
            agreed_step,
            event_position,
            producer,
            session_id,
        } = row;
        let state_payload = open_envelope(
            EnvelopeKind::ExecutionState,
            &state_bytes,
            arena0_protocol::MAX_EXECUTION_STATE_BYTES,
        )?;
        let state = ExecutionState::decode(&state_payload)?;
        if state.execution_id() != execution_id
            || state.producer() != producer
            || state.binding().session_id() != session_id
        {
            return Err(StoreError::Corruption(
                "execution index does not match decoded state".into(),
            ));
        }
        self.validate_state_indexes(&state, version, lifecycle, agreed_step, event_position)?;
        Ok(state)
    }

    pub(super) fn validate_state_indexes(
        &self,
        state: &ExecutionState,
        version: i64,
        lifecycle: i64,
        agreed_step: i64,
        event_position: i64,
    ) -> Result<(), StoreError> {
        let (phase, peers): (i64, Vec<u8>) = self.connection.query_row(
            "SELECT end_phase, end_unconfirmed FROM executions WHERE execution_id = ?1",
            params![state.execution_id().0.to_vec()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if &decode_end(phase, &peers)? != state.end_phase() {
            return Err(StoreError::Corruption(
                "end phase index does not match state".into(),
            ));
        }
        if sqlite_i64(version)? != state.version().get()
            || lifecycle != lifecycle_tag(state.lifecycle())
            || sqlite_i64(agreed_step)? != state.agreed_step()
            || sqlite_i64(event_position)? != state.event_position()
        {
            return Err(StoreError::Corruption(
                "execution scheduling index does not match state".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn validate_projection_rows(
        &mut self,
        state: &ExecutionState,
    ) -> Result<(), StoreError> {
        self.validate_event_rows(state)?;
        let trace = self.validate_agreed_rows(state)?;
        self.validate_timer_rows(state)?;
        self.validate_terminal_rows(state, &trace)?;
        Ok(())
    }

    pub(crate) fn persist(
        &mut self,
        execution_id: ExecId,
        record: TransitionRecord,
    ) -> Result<(), StoreError> {
        self.transaction(|store| store.persist_in_transaction(execution_id, record))
    }

    fn persist_in_transaction(
        &mut self,
        execution_id: ExecId,
        record: TransitionRecord,
    ) -> Result<(), StoreError> {
        let TransitionRecord {
            expected,
            next,
            change,
            now_ms,
        } = record;
        if next.execution_id() != execution_id || next.producer() != self.host_id {
            return Err(StoreError::Corruption(
                "transition execution binding mismatch".into(),
            ));
        }
        let version: Option<i64> = self
            .connection
            .query_row(
                "SELECT version FROM executions WHERE execution_id = ?1",
                params![execution_id.0.to_vec()],
                |row| row.get(0),
            )
            .optional()?;
        let version = version.ok_or(StoreError::ExecutionNotFound(execution_id))?;
        if sqlite_i64(version)? != expected.get() {
            return Err(StoreError::Corruption("execution version moved".into()));
        }
        self.persist_state(expected, &next, now_ms)?;
        match change {
            Change::Activate => {}
            Change::Dispatch {
                event,
                effects,
                timer_id,
            } => {
                let event_position = next.event_position().checked_sub(1).ok_or_else(|| {
                    StoreError::Corruption("dispatch event position is zero".into())
                })?;
                self.insert_event_record(
                    execution_id,
                    event_position,
                    &event_bytes(&event)?,
                    &effects_bytes(&effects)?,
                )?;
                if next.pending_shared().is_none() {
                    self.persist_effects(
                        execution_id,
                        event_position,
                        next.version(),
                        &indexed_effects(&effects)?,
                        now_ms,
                    )?;
                }
                if let Some(timer_id) = timer_id {
                    self.consume_timer(execution_id, timer_id)?;
                }
            }
            Change::StepSignature { certified } => {
                if let Some(proposal) = certified.as_ref() {
                    self.commit_staged_event(execution_id, proposal, next.version(), now_ms)?;
                }
            }
            Change::Stop | Change::End | Change::DropOutgoing => {}
            Change::Publish { artifact } => {
                self.persist_terminal_publication(execution_id, &artifact, now_ms)?;
            }
        }
        Ok(())
    }

    pub(crate) fn assemble_receipt(
        &mut self,
        execution_id: ExecId,
        state: &ExecutionState,
    ) -> Result<ReceiptArtifact, StoreError> {
        let version: i64 = self
            .connection
            .query_row(
                "SELECT version FROM executions WHERE execution_id = ?1",
                params![execution_id.0.to_vec()],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
        if state.execution_id() != execution_id || sqlite_i64(version)? != state.version().get() {
            return Err(StoreError::Corruption(
                "receipt assembly requires the committed execution version".into(),
            ));
        }
        self.assemble_terminal_artifact(state)
    }

    fn assemble_terminal_artifact(
        &mut self,
        state: &ExecutionState,
    ) -> Result<ReceiptArtifact, StoreError> {
        let activation = self
            .load_activation_in_transaction(state.execution_id())?
            .ok_or(StoreError::ActivationNotFound(state.execution_id()))?;
        let activation = activation
            .activation()
            .ok_or(StoreError::ActivationNotCommitted(state.execution_id()))?;
        let trace = self.load_agreed_trace_in_transaction(state)?;
        let termination = if let Some(cause) = state.status().terminal_cause() {
            arena0_protocol::ReceiptTermination::Stopped {
                cause: cause.clone(),
            }
        } else {
            if !matches!(
                state.status(),
                ExecutionStatus::Certified { .. } | ExecutionStatus::Completed { .. }
            ) {
                return Err(StoreError::Protocol(ProtocolError::InvalidTerminalStatus));
            }
            arena0_protocol::ReceiptTermination::Completed
        };
        let outcome = state
            .terminal_outcome()
            .map_or_else(Vec::new, |outcome| outcome.borsh().to_vec());
        let body = arena0_protocol::ReceiptBody::new(
            arena0_protocol::SessionHeader::new(activation.clone(), termination),
            outcome,
            activation.offer().data().params.as_bytes().to_vec(),
            trace,
        )?;
        Ok(ReceiptArtifact::new(body)?)
    }

    pub(crate) fn read_trace(
        &mut self,
        execution_id: ExecId,
        from: u64,
        to: u64,
    ) -> Result<Vec<arena0_protocol::TraceEntry>, StoreError> {
        if from >= to {
            return Ok(Vec::new());
        }
        let state = self
            .load_execution_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
        let trace = self.load_agreed_trace_in_transaction(&state)?;
        let end = to.min(trace.len() as u64);
        let start = usize::try_from(from.min(end))
            .map_err(|_| StoreError::Corruption("trace start overflows usize".into()))?;
        let end = usize::try_from(end)
            .map_err(|_| StoreError::Corruption("trace end overflows usize".into()))?;
        Ok(trace[start..end].to_vec())
    }

    pub(crate) fn read_event_summaries(
        &mut self,
        execution_id: ExecId,
        from: Option<u64>,
        limit: usize,
    ) -> Result<EventInspectionPage, StoreError> {
        if limit == 0 || limit > MAX_EVENT_INSPECTION_RECORDS {
            return Err(StoreError::InvalidConfiguration(
                "event inspection limit is outside the fixed bound",
            ));
        }
        let state = self
            .load_execution_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
        let total = state.event_position();
        let start = from
            .unwrap_or_else(|| total.saturating_sub(limit as u64))
            .min(total);
        let end = start.saturating_add(limit as u64).min(total);
        let mut statement = self.connection.prepare(
            "SELECT event_position, event, effects
             FROM event_records
             WHERE execution_id = ?1 AND event_position >= ?2 AND event_position < ?3
             ORDER BY event_position",
        )?;
        let mut rows = statement.query(params![
            execution_id.0.to_vec(),
            sqlite_u64(start)?,
            sqlite_u64(end)?,
        ])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            let position = sqlite_i64(row.get::<_, i64>(0)?)?;
            let event_payload = open_envelope(
                EnvelopeKind::EventRecord,
                &row.get::<_, Vec<u8>>(1)?,
                arena0_protocol::MAX_EXECUTION_STATE_BYTES,
            )?;
            let effects_payload = open_envelope(
                EnvelopeKind::Effects,
                &row.get::<_, Vec<u8>>(2)?,
                arena0_protocol::MAX_RECEIPT_BYTES,
            )?;
            records.push((position, event_payload, effects_payload));
        }
        drop(rows);
        drop(statement);
        let mut summaries = Vec::new();
        let mut response_bytes = 0;
        for (position, event_payload, effects_payload) in records {
            let event: Event<Vec<u8>> = decode_borsh(&event_payload, "event record event")?;
            let effects: Vec<Effect> = decode_borsh(&effects_payload, "event record effects")?;
            let mut agreed_statement = self.connection.prepare(
                "SELECT step FROM agreed_steps
                 WHERE execution_id = ?1 AND origin_event_position = ?2
                 ORDER BY step",
            )?;
            let agreed_steps = agreed_statement
                .query_map(
                    params![execution_id.0.to_vec(), sqlite_u64(position)?],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(StoreError::Sqlite)?
                .map(|step| step.map_err(StoreError::Sqlite).and_then(sqlite_i64))
                .collect::<Result<Vec<_>, _>>()?;
            drop(agreed_statement);
            account_response(&mut response_bytes, 256 + effects.len() * 32)?;
            summaries.push(EventRecordSummary::from_record(
                position,
                agreed_steps,
                &event,
                &effects,
            ));
        }
        let expected = usize::try_from(end - start)
            .map_err(|_| StoreError::Corruption("event range overflows usize".into()))?;
        if summaries.len() != expected {
            return Err(StoreError::Corruption(
                "event inspection range is incomplete".into(),
            ));
        }
        Ok(EventInspectionPage::new(
            start,
            summaries,
            total,
            (end < total).then_some(end),
        ))
    }

    pub(super) fn load_agreed_trace_in_transaction(
        &mut self,
        state: &ExecutionState,
    ) -> Result<Vec<arena0_protocol::TraceEntry>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT step, origin_event_position, version, artifact, entry_hash
             FROM agreed_steps WHERE execution_id = ?1 ORDER BY step",
        )?;
        let mut rows = statement.query(params![state.execution_id().0.to_vec()])?;
        let mut raw = Vec::new();
        while let Some(row) = rows.next()? {
            raw.push((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
            ));
        }
        drop(rows);
        drop(statement);
        let mut trace = Vec::with_capacity(raw.len());
        let mut trace_bytes = 0u64;
        for (index, (step, origin_event_position, version, artifact, entry_hash)) in
            raw.into_iter().enumerate()
        {
            let step = sqlite_i64(step)?;
            let expected_step = index as u64;
            if step != expected_step {
                return Err(StoreError::Corruption(
                    "agreed steps are not gapless".into(),
                ));
            }
            let origin_event_position = sqlite_i64(origin_event_position)?;
            let origin_exists = self
                .connection
                .query_row(
                    "SELECT 1 FROM event_records
                     WHERE execution_id = ?1 AND event_position = ?2",
                    params![
                        state.execution_id().0.to_vec(),
                        sqlite_u64(origin_event_position)?,
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if origin_exists != Some(1) {
                return Err(StoreError::Corruption(
                    "agreed step origin has no event record".into(),
                ));
            }
            let version = sqlite_i64(version)?;
            if version == 0 || version > state.version().get() {
                return Err(StoreError::Corruption(
                    "agreed step version is invalid".into(),
                ));
            }
            let entry: arena0_protocol::TraceEntry = decode_borsh(
                &open_envelope(
                    EnvelopeKind::AgreedStep,
                    &artifact,
                    arena0_protocol::MAX_TRACE_ENTRY_BYTES,
                )?,
                "agreed trace entry",
            )?;
            if entry.step != step || entry.entry_hash() != array32(&entry_hash, "entry hash")? {
                return Err(StoreError::Corruption(
                    "agreed step index does not match entry".into(),
                ));
            }
            trace_bytes += borsh::object_length(&entry)
                .map_err(|error| StoreError::Corruption(format!("trace size: {error}")))?
                as u64;
            trace.push(entry);
        }
        if trace.len() as u64 != state.agreed_step() || trace_bytes != state.trace_bytes() {
            return Err(StoreError::Corruption(
                "agreed trace does not match execution cursor".into(),
            ));
        }
        Ok(trace)
    }

    fn commit_staged_event(
        &mut self,
        execution_id: ExecId,
        proposal: &arena0_protocol::SharedProposal,
        version: ExecutionVersion,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let release_effects = proposal.effects().to_vec();
        let event_exists = self
            .connection
            .query_row(
                "SELECT 1 FROM event_records
                 WHERE execution_id = ?1 AND event_position = ?2",
                params![
                    execution_id.0.to_vec(),
                    sqlite_u64(proposal.event_position())?,
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if event_exists != Some(1) {
            return Err(StoreError::Corruption(
                "step signature did not find its event record".into(),
            ));
        }
        self.insert_agreed_step(
            execution_id,
            proposal.event_position(),
            version,
            proposal.entry(),
        )?;
        self.persist_effects(
            execution_id,
            proposal.event_position(),
            version,
            &release_effects,
            now_ms,
        )?;
        Ok(())
    }

    fn insert_event_record(
        &mut self,
        execution_id: ExecId,
        event_position: u64,
        event: &[u8],
        effects: &[u8],
    ) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO event_records
             (execution_id, event_position, event, effects, event_digest)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                execution_id.0.to_vec(),
                sqlite_u64(event_position)?,
                envelope(EnvelopeKind::EventRecord, event)?,
                envelope(EnvelopeKind::Effects, effects)?,
                dispatch_digest(event, effects).to_vec(),
            ],
        )?;
        Ok(())
    }

    fn insert_agreed_step(
        &mut self,
        execution_id: ExecId,
        origin_event_position: u64,
        version: ExecutionVersion,
        entry: &arena0_protocol::TraceEntry,
    ) -> Result<(), StoreError> {
        let bytes = borsh::to_vec(entry)
            .map_err(|error| StoreError::Corruption(format!("agreed entry encode: {error}")))?;
        self.connection.execute(
            "INSERT INTO agreed_steps
             (execution_id, step, origin_event_position, version, artifact, entry_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                execution_id.0.to_vec(),
                sqlite_u64(entry.step)?,
                sqlite_u64(origin_event_position)?,
                sqlite_u64(version.get())?,
                envelope(EnvelopeKind::AgreedStep, &bytes)?,
                entry.entry_hash().to_vec(),
            ],
        )?;
        Ok(())
    }

    fn persist_state(
        &mut self,
        expected: ExecutionVersion,
        next: &ExecutionState,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let bytes = state_bytes(next)?;
        let changed = self.connection.execute(
            "UPDATE executions SET state = ?1,
                    version = ?2, lifecycle = ?3, agreed_step = ?4,
                    event_position = ?5, updated_at_ms = ?6, end_phase = ?9, end_unconfirmed = ?10
             WHERE execution_id = ?7 AND version = ?8",
            params![
                envelope(EnvelopeKind::ExecutionState, &bytes)?,
                sqlite_u64(next.version().get())?,
                lifecycle_tag(next.lifecycle()),
                sqlite_u64(next.agreed_step())?,
                sqlite_u64(next.event_position())?,
                sqlite_u64(now_ms)?,
                next.execution_id().0.to_vec(),
                sqlite_u64(expected.get())?,
                end_columns(next.end_phase())?.0,
                end_columns(next.end_phase())?.1,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Corruption("execution version moved".into()));
        }
        Ok(())
    }

    pub(super) fn validate_event_rows(&mut self, state: &ExecutionState) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT event_position, event, effects, event_digest
             FROM event_records WHERE execution_id = ?1 ORDER BY event_position",
        )?;
        let mut rows = statement.query(params![state.execution_id().0.to_vec()])?;
        let mut count = 0u64;
        while let Some(row) = rows.next()? {
            let position = sqlite_i64(row.get::<_, i64>(0)?)?;
            if position != count {
                return Err(StoreError::Corruption(
                    "event positions are not gapless".into(),
                ));
            }
            count = count
                .checked_add(1)
                .ok_or_else(|| StoreError::Corruption("event count overflow".into()))?;
            let event_payload = open_envelope(
                EnvelopeKind::EventRecord,
                &row.get::<_, Vec<u8>>(1)?,
                arena0_protocol::MAX_EXECUTION_STATE_BYTES,
            )?;
            let effects_payload = open_envelope(
                EnvelopeKind::Effects,
                &row.get::<_, Vec<u8>>(2)?,
                arena0_protocol::MAX_RECEIPT_BYTES,
            )?;
            let _: Event<Vec<u8>> = decode_borsh(&event_payload, "event record event")?;
            let effects: Vec<Effect> = decode_borsh(&effects_payload, "event record effects")?;
            arena0_protocol::execution::check_effect_budget(&effects)?;
            if dispatch_digest(&event_payload, &effects_payload)
                != array32(&row.get::<_, Vec<u8>>(3)?, "event digest")?
            {
                return Err(StoreError::Corruption("event digest mismatch".into()));
            }
        }
        if count != state.event_position() {
            return Err(StoreError::Corruption(
                "event records do not match execution position".into(),
            ));
        }
        if let Some(proposal) = state.pending_shared() {
            let Some(last_position) = count.checked_sub(1) else {
                return Err(StoreError::Corruption(
                    "pending proposal has no originating event record".into(),
                ));
            };
            if proposal.event_position() != last_position {
                return Err(StoreError::Corruption(
                    "pending proposal is not paired with the last event record".into(),
                ));
            }
        }
        Ok(())
    }

    /// Load the agreed trace and authenticate it once, through the protocol's
    /// own certified-trace validation, against the execution cursor.
    pub(super) fn validate_agreed_rows(
        &mut self,
        state: &ExecutionState,
    ) -> Result<Vec<arena0_protocol::TraceEntry>, StoreError> {
        let trace = self.load_agreed_trace_in_transaction(state)?;
        let cursor = arena0_protocol::validate_agreed_trace(state.binding(), &trace)
            .map_err(|error| StoreError::Corruption(format!("agreed trace: {error}")))?;
        if cursor.state_hash() != state.agreed_state() || cursor.chain_hash() != state.agreed_link()
        {
            return Err(StoreError::Corruption(
                "agreed trace does not match execution cursor".into(),
            ));
        }
        Ok(trace)
    }

    pub(super) fn validate_timer_rows(&mut self, state: &ExecutionState) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT timer_id, deadline_ms, payload, armed_version
             FROM active_timers WHERE execution_id = ?1 ORDER BY timer_id",
        )?;
        let mut rows = statement.query(params![state.execution_id().0.to_vec()])?;
        while let Some(row) = rows.next()? {
            if row.get::<_, Vec<u8>>(0)?.len() != 32 {
                return Err(StoreError::Corruption("timer id is not 32 bytes".into()));
            }
            let _ = sqlite_i64(row.get::<_, i64>(1)?)?;
            let payload = open_envelope(
                EnvelopeKind::Timer,
                &row.get::<_, Vec<u8>>(2)?,
                MAX_TIMER_RECORD_BYTES,
            )?;
            let timer: TimerPayload = decode_borsh(&payload, "timer payload")?;
            if timer.type_name.len() > arena0_protocol::MAX_TERMINAL_REASON_BYTES
                || timer.data.len() > arena0_protocol::MAX_TIMER_PAYLOAD_BYTES
            {
                return Err(StoreError::Corruption(
                    "timer payload exceeds bounds".into(),
                ));
            }
            let armed = sqlite_i64(row.get::<_, i64>(3)?)?;
            if armed == 0 || armed > state.version().get() {
                return Err(StoreError::Corruption("timer version is invalid".into()));
            }
        }
        Ok(())
    }

    fn consume_timer(&mut self, execution_id: ExecId, timer_id: TimerId) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "DELETE FROM active_timers WHERE execution_id = ?1 AND timer_id = ?2",
            params![execution_id.0.to_vec(), timer_id.as_bytes().to_vec()],
        )?;
        if changed != 1 {
            return Err(StoreError::Corruption(
                "timer consume compare-and-set failed".into(),
            ));
        }
        Ok(())
    }
}

fn indexed_effects(effects: &[Effect]) -> Result<Vec<(u32, Effect)>, StoreError> {
    effects
        .iter()
        .enumerate()
        .map(|(index, effect)| {
            Ok((
                u32::try_from(index).map_err(|_| StoreError::PayloadTooLarge {
                    required: index,
                    capacity: u32::MAX as usize,
                })?,
                effect.clone(),
            ))
        })
        .collect()
}

fn dispatch_digest(event: &[u8], effects: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arena0/store/event/v2");
    hasher.update(event);
    hasher.update(effects);
    *hasher.finalize().as_bytes()
}
