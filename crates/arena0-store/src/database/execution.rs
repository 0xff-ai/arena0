use super::*;

impl Database {
    pub(super) fn create_execution(
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
        self.begin()?;
        let result = self.create_execution_in_transaction(state, now_ms);
        match result {
            Ok(outcome) => self.commit_result(outcome),
            Err(error) => self.rollback_result(error),
        }
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
        self.pending_execution = Some(PendingExecution {
            encoded_bytes: state_bytes(&state)?.len(),
            state: state.clone(),
        });
        Ok(CreateExecutionOutcome::Created(state))
    }

    pub(super) fn insert_execution(
        &mut self,
        state: &ExecutionState,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let state_bytes = state_bytes(state)?;
        let local = state.local_state().as_bytes();
        self.connection.execute(
            "INSERT INTO executions
             (execution_id, host_id, producer, session_id, state,
              local_state_checksum, version, lifecycle, public_step, private_next_record,
              created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
            params![
                state.execution_id().0.to_vec(),
                self.host_id.0.to_vec(),
                state.producer().0.to_vec(),
                state.binding().session_id().0.to_vec(),
                envelope(EnvelopeKind::ExecutionState, &state_bytes)?,
                checksum(local).to_vec(),
                sqlite_u64(state.version().get())?,
                lifecycle_tag(state.lifecycle()),
                sqlite_u64(state.public().next_step())?,
                sqlite_u64(state.private().next_record())?,
                sqlite_u64(now_ms)?,
            ],
        )?;
        Ok(())
    }

    pub(super) fn load_execution(
        &mut self,
        execution_id: ExecId,
    ) -> Result<Option<ExecutionState>, StoreError> {
        self.load_execution_in_transaction(execution_id)
    }

    pub(super) fn load_execution_by_session(
        &mut self,
        session_id: SessionHash,
    ) -> Result<Option<ExecutionState>, StoreError> {
        let Some(execution_bytes) = self
            .connection
            .query_row(
                "SELECT execution_id FROM executions WHERE session_id = ?1",
                params![session_id.0.to_vec()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
        else {
            return Ok(None);
        };
        self.load_execution(ExecId(array32(&execution_bytes, "execution id")?))
    }

    pub(super) fn list_activations(
        &mut self,
        limit: usize,
    ) -> Result<Vec<ActivationRecord>, StoreError> {
        let limit = i64::try_from(limit)
            .map_err(|_| StoreError::InvalidConfiguration("activation limit is too large"))?;
        let mut statement = self.connection.prepare(
            "SELECT execution_id FROM activation_records
             ORDER BY execution_id LIMIT ?1",
        )?;
        let mut rows = statement.query(params![limit])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            ids.push(ExecId(array32(
                &row.get::<_, Vec<u8>>(0)?,
                "activation execution id",
            )?));
        }
        drop(rows);
        drop(statement);
        let mut response_bytes = 0;
        ids.into_iter()
            .map(|id| {
                let activation = self.load_activation(id)?.ok_or_else(|| {
                    StoreError::Corruption("activation disappeared while listing".into())
                })?;
                account_response(
                    &mut response_bytes,
                    prepared_activation_bytes(activation.prepared())?.len(),
                )?;
                if let Some(committed) = activation.activation() {
                    account_response(&mut response_bytes, activation_bytes(committed)?.len())?;
                }
                Ok(activation)
            })
            .collect()
    }

    pub(super) fn list_executions(
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
        if let Some(state) = self.executions.get(execution_id) {
            return Ok(Some(state));
        }
        let state = self.load_execution_from_sqlite(execution_id)?;
        if let Some(state) = state.as_ref() {
            self.validate_projection_rows(state)?;
            self.executions
                .insert(state.clone(), state_bytes(state)?.len());
        }
        Ok(state)
    }

    pub(super) fn load_execution_from_sqlite(
        &mut self,
        execution_id: ExecId,
    ) -> Result<Option<ExecutionState>, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT host_id, producer, session_id, state,
                        local_state_checksum, version, lifecycle, public_step,
                        private_next_record
                 FROM executions WHERE execution_id = ?1",
                params![execution_id.0.to_vec()],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, Vec<u8>>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, i64>(8)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            host_bytes,
            producer_bytes,
            session_bytes,
            state_bytes,
            local_checksum,
            version,
            lifecycle,
            public_step,
            private_next_record,
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
            local_checksum,
            version,
            lifecycle,
            public_step,
            private_next_record,
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
            local_checksum,
            version,
            lifecycle,
            public_step,
            private_next_record,
            producer,
            session_id,
        } = row;
        let state_payload = open_envelope(
            EnvelopeKind::ExecutionState,
            &state_bytes,
            arena0_protocol::execution::MAX_EXECUTION_STATE_BYTES,
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
        if local_checksum != checksum(state.local_state().as_bytes()).as_slice() {
            return Err(StoreError::Corruption(
                "local state checksum does not match execution state".into(),
            ));
        }
        self.validate_state_indexes(&state, version, lifecycle, public_step, private_next_record)?;
        Ok(state)
    }

    pub(super) fn validate_state_indexes(
        &self,
        state: &ExecutionState,
        version: i64,
        lifecycle: i64,
        public_step: i64,
        private_next_record: i64,
    ) -> Result<(), StoreError> {
        let version = sqlite_i64(version)?;
        let public_step = sqlite_i64(public_step)?;
        let private_next_record = sqlite_i64(private_next_record)?;
        if version != state.version().get()
            || lifecycle != lifecycle_tag(state.lifecycle())
            || public_step != state.public().next_step()
            || private_next_record != state.private().next_record()
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
        self.validate_public_rows(state)?;
        self.validate_private_rows(state)?;
        self.validate_timer_rows(state)?;
        self.validate_terminal_rows(state)?;
        Ok(())
    }

    pub(super) fn apply_input(
        &mut self,
        execution_id: ExecId,
        input: ExecutionInput,
        inbox: Option<InboxReference>,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        self.begin()?;
        let result = self.apply_input_in_transaction(execution_id, input, inbox, now_ms);
        match result {
            Ok(outcome) => self.commit_result(outcome),
            Err(error) => self.rollback_result(error),
        }
    }

    /// Assemble the canonical receipt body from durable source rows and stage
    /// it through the protocol reducer without leaving the transaction.
    pub(super) fn assemble_and_stage_receipt_body(
        &mut self,
        execution_id: ExecId,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        self.begin()?;
        let result = self.assemble_and_stage_receipt_body_in_transaction(execution_id, now_ms);
        match result {
            Ok(outcome) => self.commit_result(outcome),
            Err(error) => self.rollback_result(error),
        }
    }

    fn assemble_and_stage_receipt_body_in_transaction(
        &mut self,
        execution_id: ExecId,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        let state = self
            .load_execution_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;

        // Once the body has been durably staged or the final artifact has been
        // published, the assembly command is an idempotent acknowledgement.
        if state.receipt_body().is_some()
            || matches!(
                state.status(),
                arena0_protocol::ExecutionStatus::Completed { .. }
                    | arena0_protocol::ExecutionStatus::StoppedPublished { .. }
                    | arena0_protocol::ExecutionStatus::Incomplete { .. }
            )
        {
            return Ok(ApplyOutcome::AlreadyApplied);
        }

        let activation_record = self
            .load_activation_in_transaction(execution_id)?
            .ok_or(StoreError::ActivationNotFound(execution_id))?;
        let activation = activation_record
            .activation()
            .ok_or(StoreError::ActivationNotCommitted(execution_id))?;
        if activation != state.binding().activation() {
            return Err(StoreError::Corruption(
                "receipt activation does not match permanent activation".into(),
            ));
        }
        let request = self
            .load_execution_request_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionRequestNotFound(execution_id))?;
        if request.program_hash() != state.binding().program_hash()
            || request.params().is_some_and(|params| {
                params.as_bytes() != activation.offer().data().params.as_bytes()
            })
        {
            return Err(StoreError::Corruption(
                "receipt request does not match permanent activation".into(),
            ));
        }

        let trace = self.load_public_trace_in_transaction(&state)?;
        let termination = if let Some(cause) = state.status().terminal_cause() {
            arena0_protocol::ReceiptTermination::Stopped {
                cause: cause.clone(),
            }
        } else if let (Some(certificate), Some(_outcome)) =
            (state.terminal_certificate(), state.terminal_outcome())
        {
            arena0_protocol::ReceiptTermination::Completed {
                terminal: arena0_protocol::SessionTerminal {
                    final_step: certificate.commitment().final_step,
                    final_state: certificate.commitment().final_state,
                    outcome_hash: certificate.commitment().outcome_hash,
                    agreement: certificate.agreement().clone(),
                },
            }
        } else {
            return Err(StoreError::Protocol(ProtocolError::TerminalProofMissing));
        };
        let outcome = match &termination {
            arena0_protocol::ReceiptTermination::Completed { .. } => state
                .terminal_outcome()
                .ok_or(StoreError::Protocol(ProtocolError::TerminalProofMissing))?
                .borsh()
                .to_vec(),
            arena0_protocol::ReceiptTermination::Stopped { .. } => Vec::new(),
        };
        let body = arena0_protocol::ReceiptBody::new(
            arena0_protocol::SessionHeader::new(activation.clone(), termination, self.host_id),
            outcome,
            activation.offer().data().params.as_bytes().to_vec(),
            trace,
        )?;
        self.apply_input_in_transaction(
            execution_id,
            ExecutionInput::ReceiptBody(Box::new(body)),
            None,
            now_ms,
        )
    }

    fn load_public_trace_in_transaction(
        &mut self,
        state: &ExecutionState,
    ) -> Result<Vec<arena0_protocol::TraceEntry>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT step, version, artifact, entry_hash FROM public_commits
             WHERE execution_id = ?1 ORDER BY step",
        )?;
        let mut rows = statement.query(params![state.execution_id().0.to_vec()])?;
        let mut commits = Vec::new();
        while let Some(row) = rows.next()? {
            commits.push((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ));
        }
        drop(rows);
        drop(statement);

        let mut trace = Vec::with_capacity(commits.len());
        for (index, (step, version, encoded, entry_hash)) in commits.into_iter().enumerate() {
            let step = sqlite_i64(step)?;
            let expected_step = u64::try_from(index)
                .map_err(|_| StoreError::Corruption("public step overflow".into()))?;
            if step != expected_step {
                return Err(StoreError::Corruption(
                    "public commit steps are not gapless".into(),
                ));
            }
            let version = sqlite_i64(version)?;
            if version == 0 || version > state.version().get() {
                return Err(StoreError::Corruption(
                    "public commit version is invalid".into(),
                ));
            }
            let payload = open_envelope(
                EnvelopeKind::SharedCommit,
                &encoded,
                arena0_protocol::MAX_COMMIT_PLAN_BYTES,
            )?;
            let commit: arena0_protocol::SharedCommit = decode_borsh(&payload, "public commit")?;
            let entry = commit.entry();
            if entry.step != step
                || entry.entry_hash() != array32(&entry_hash, "public entry hash")?
            {
                return Err(StoreError::Corruption(
                    "public commit index mismatch".into(),
                ));
            }
            let commitment = commit.certificate().commitment();
            if commitment.session_id != state.binding().session_id()
                || commitment.step != entry.step
                || commitment.entry_hash != entry.entry_hash()
                || commitment.pre_state != entry.pre_state
                || commitment.post_state != entry.post_state
            {
                return Err(StoreError::Corruption(
                    "public certificate does not match entry".into(),
                ));
            }
            trace.push(entry.clone());
        }
        let count = u64::try_from(trace.len())
            .map_err(|_| StoreError::Corruption("public commit count overflows u64".into()))?;
        if count != state.public().next_step() {
            return Err(StoreError::Corruption(
                "public cursor does not match public commits".into(),
            ));
        }
        Ok(trace)
    }

    /// Validate the complete public trace, then return the requested bounded
    /// projection. The daemon uses this for event and API reads while the
    /// execution actor retains the only mutable store capability.
    pub(super) fn read_trace(
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
        let trace = self.load_public_trace_in_transaction(&state)?;
        let end = to.min(
            u64::try_from(trace.len())
                .map_err(|_| StoreError::Corruption("public trace length overflows u64".into()))?,
        );
        let start = from.min(end);
        let start = usize::try_from(start)
            .map_err(|_| StoreError::Corruption("public trace start overflows usize".into()))?;
        let end = usize::try_from(end)
            .map_err(|_| StoreError::Corruption("public trace end overflows usize".into()))?;
        Ok(trace[start..end].to_vec())
    }

    /// Validate and summarize one bounded range of durable private records.
    /// Private payloads and replacement local state never leave the store
    /// owner; full-projection validation remains part of execution recovery.
    pub(super) fn read_private_summaries(
        &mut self,
        execution_id: ExecId,
        from: Option<u64>,
        limit: usize,
    ) -> Result<PrivateInspectionPage, StoreError> {
        if limit > MAX_PRIVATE_INSPECTION_RECORDS {
            return Err(StoreError::InvalidConfiguration(
                "private inspection limit exceeds the fixed bound",
            ));
        }
        if limit == 0 {
            return Err(StoreError::InvalidConfiguration(
                "private inspection limit must be non-zero",
            ));
        }
        let state = self
            .load_execution_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
        let total = state.private().next_record();
        let start = from
            .unwrap_or_else(|| total.saturating_sub(limit as u64))
            .min(total);
        let end = start.saturating_add(limit as u64).min(total);
        if start == end {
            return Ok(PrivateInspectionPage::new(start, Vec::new(), total, None));
        }

        let start_sequence = start;
        let end_sequence = end;
        let start = sqlite_u64(start_sequence)?;
        let end = sqlite_u64(end_sequence)?;
        let mut statement = self.connection.prepare(
            "SELECT sequence, version, artifact, record_digest
             FROM private_commits
             WHERE execution_id = ?1 AND sequence >= ?2 AND sequence < ?3
             ORDER BY sequence",
        )?;
        let mut rows = statement.query(params![execution_id.0.to_vec(), start, end])?;
        let mut summaries = Vec::new();
        let mut response_bytes = 0;
        while let Some(row) = rows.next()? {
            let sequence = sqlite_i64(row.get::<_, i64>(0)?)?;
            let version = sqlite_i64(row.get::<_, i64>(1)?)?;
            let encoded = row.get::<_, Vec<u8>>(2)?;
            let digest = row.get::<_, Vec<u8>>(3)?;
            let payload = open_envelope(
                EnvelopeKind::PrivateCommit,
                &encoded,
                arena0_protocol::MAX_PRIVATE_RECORD_BYTES + 1024,
            )?;
            let commit: arena0_protocol::PrivateCommit = decode_borsh(&payload, "private commit")?;
            if commit.record().seq != sequence
                || checksum(&borsh::to_vec(&commit).map_err(|error| {
                    StoreError::Corruption(format!("private commit encode: {error}"))
                })?) != array32(&digest, "private record digest")?
            {
                return Err(StoreError::Corruption(
                    "private commit index mismatch".into(),
                ));
            }
            if version == 0 || version > state.version().get() {
                return Err(StoreError::Corruption(
                    "private commit version is invalid".into(),
                ));
            }
            let summary = PrivateCommitSummary::from_commit(&commit);
            account_response(
                &mut response_bytes,
                256 + summary.effects.len().saturating_mul(64),
            )?;
            summaries.push(summary);
        }
        if summaries.len()
            != usize::try_from(end_sequence - start_sequence).map_err(|_| {
                StoreError::Corruption("private inspection row count overflows usize".into())
            })?
        {
            return Err(StoreError::Corruption(
                "private inspection range is incomplete".into(),
            ));
        }
        let next = (end_sequence < total).then_some(end_sequence);
        Ok(PrivateInspectionPage::new(
            start_sequence,
            summaries,
            total,
            next,
        ))
    }

    pub(super) fn apply_input_in_transaction(
        &mut self,
        execution_id: ExecId,
        input: ExecutionInput,
        inbox: Option<InboxReference>,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        let state = self
            .load_execution_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
        let input_bytes = input_bytes(&input)?;
        let occurrence = input.occurrence(&state)?;
        let occurrence_key = occurrence_key_bytes(occurrence.key())?;
        let occurrence_digest = occurrence.digest();

        if let Some(reference) = inbox {
            let row = self.inbox_row(execution_id, reference.inbox_id)?;
            let Some((stored_source, status, applied_version)) = row else {
                return Err(StoreError::InboxNotAccepted(reference.inbox_id));
            };
            if stored_source != reference.source {
                return Err(StoreError::Corruption(
                    "inbox source changed before application".into(),
                ));
            }
            if status == InboxStatus::Applied {
                return Ok(ApplyOutcome::InboxAlreadyApplied {
                    inbox_id: reference.inbox_id,
                    version: applied_version.ok_or_else(|| {
                        StoreError::Corruption("applied inbox row has no version".into())
                    })?,
                });
            }
            if status == InboxStatus::Consumed {
                return Ok(ApplyOutcome::InboxAlreadyConsumed {
                    inbox_id: reference.inbox_id,
                });
            }
        }

        let existing = self
            .connection
            .query_row(
                "SELECT digest, committed_version FROM occurrences
                 WHERE execution_id = ?1 AND occurrence_key = ?2",
                params![execution_id.0.to_vec(), occurrence_key.clone()],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        if let Some((existing_bytes, existing_version)) = existing {
            let existing =
                OccurrenceDigest::from_bytes(array32(&existing_bytes, "occurrence digest")?);
            if existing == occurrence_digest {
                if let Some(reference) = inbox.filter(|reference| reference.complete_on_apply) {
                    self.mark_inbox_applied(
                        execution_id,
                        reference,
                        ExecutionVersion::new(sqlite_i64(existing_version)?),
                    )?;
                }
                return Ok(ApplyOutcome::AlreadyApplied);
            }
            let conflict = OccurrenceConflict::new(occurrence.key(), existing, occurrence_digest);
            self.connection.execute(
                "INSERT INTO occurrence_conflicts
                 (execution_id, occurrence_key, existing_digest, incoming_digest,
                  incoming_input, observed_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    execution_id.0.to_vec(),
                    occurrence_key,
                    existing.as_bytes().to_vec(),
                    occurrence_digest.as_bytes().to_vec(),
                    envelope(EnvelopeKind::ExecutionInput, &input_bytes)?,
                    sqlite_u64(now_ms)?,
                ],
            )?;
            return Ok(ApplyOutcome::Conflict(conflict));
        }

        let transition = arena0_protocol::execution::transition(&state, input)?;
        let TransitionOutcome::Commit(plan) = transition else {
            return Err(StoreError::Corruption(format!(
                "execution {execution_id} recognizes applied input without its occurrence fact"
            )));
        };
        self.commit_plan_in_transaction(&state, &plan, occurrence, &input_bytes, inbox, now_ms)
    }

    pub(super) fn mark_inbox_applied(
        &mut self,
        execution_id: ExecId,
        reference: InboxReference,
        version: ExecutionVersion,
    ) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "UPDATE inbox SET status = 'applied', applied_version = ?1
             WHERE execution_id = ?2 AND inbox_id = ?3 AND source = ?4
               AND status = 'accepted'",
            params![
                sqlite_u64(version.get())?,
                execution_id.0.to_vec(),
                reference.inbox_id.as_bytes().to_vec(),
                reference.source.0.to_vec(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Corruption(
                "inbox responsibility CAS failed while acknowledging duplicate".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn commit_plan_in_transaction(
        &mut self,
        state: &ExecutionState,
        plan: &CommitPlan,
        occurrence: OccurrenceEvidence,
        input_bytes: &[u8],
        inbox: Option<InboxReference>,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        if plan.execution_id() != state.execution_id()
            || plan.expected_version() != state.version()
            || plan.occurrence() != occurrence
        {
            return Err(StoreError::Corruption(
                "reducer plan does not match loaded execution or occurrence".into(),
            ));
        }
        let expected = plan.expected_version();
        let next = plan.next_version();
        if next
            != expected.next().ok_or_else(|| {
                StoreError::Corruption("plan advances an exhausted execution version".into())
            })?
        {
            return Err(StoreError::Corruption(
                "plan next version is not the expected single-step successor".into(),
            ));
        }

        // CAS the aggregate before writing projections.  The enclosing
        // transaction rolls it back if any projection fails.
        let encoded_state = state_bytes(plan.next_state())?;
        let local = plan.next_state().local_state().as_bytes();
        let changed = self.connection.execute(
            "UPDATE executions SET state = ?1,
                local_state_checksum = ?2, version = ?3, lifecycle = ?4,
                public_step = ?5, private_next_record = ?6, updated_at_ms = ?7
             WHERE execution_id = ?8 AND version = ?9",
            params![
                envelope(EnvelopeKind::ExecutionState, &encoded_state)?,
                checksum(local).to_vec(),
                sqlite_u64(next.get())?,
                lifecycle_tag(plan.next_state().lifecycle()),
                sqlite_u64(plan.next_state().public().next_step())?,
                sqlite_u64(plan.next_state().private().next_record())?,
                sqlite_u64(now_ms)?,
                state.execution_id().0.to_vec(),
                sqlite_u64(expected.get())?,
            ],
        )?;
        if changed != 1 {
            let actual = self
                .load_execution_from_sqlite(state.execution_id())?
                .ok_or(StoreError::ExecutionNotFound(state.execution_id()))?;
            self.validate_projection_rows(&actual)?;
            let actual_version = actual.version();
            self.pending_execution = Some(PendingExecution {
                encoded_bytes: state_bytes(&actual)?.len(),
                state: actual,
            });
            return Ok(ApplyOutcome::VersionMismatch {
                expected,
                actual: actual_version,
            });
        }

        self.insert_occurrence(state.execution_id(), occurrence, input_bytes, next, now_ms)?;
        self.persist_shared_commit(state.execution_id(), next, plan)?;
        self.persist_private_commit(state.execution_id(), next, plan)?;
        self.persist_terminal(state.execution_id(), next, plan, now_ms)?;
        self.persist_timers(state.execution_id(), next, plan)?;
        self.persist_outbox(state.execution_id(), next, plan, now_ms)?;
        if let Some(reference) = inbox.filter(|reference| reference.complete_on_apply) {
            self.mark_inbox_applied(state.execution_id(), reference, next)?;
        }
        self.pending_execution = Some(PendingExecution {
            state: plan.next_state().clone(),
            encoded_bytes: encoded_state.len(),
        });
        let previous_public_step = state.public().next_step();
        let next_public_step = plan.next_state().public().next_step();
        let public_step =
            (next_public_step != previous_public_step).then_some(previous_public_step);
        Ok(ApplyOutcome::Committed(CommittedSummary {
            version: next,
            public_step,
        }))
    }

    pub(super) fn insert_occurrence(
        &mut self,
        execution_id: ExecId,
        occurrence: OccurrenceEvidence,
        input_bytes: &[u8],
        version: ExecutionVersion,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        self.connection.execute(
            "INSERT INTO occurrences
             (execution_id, occurrence_key, digest, input, committed_version, committed_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                execution_id.0.to_vec(),
                occurrence_key_bytes(occurrence.key())?,
                occurrence.digest().as_bytes().to_vec(),
                envelope(EnvelopeKind::ExecutionInput, input_bytes)?,
                sqlite_u64(version.get())?,
                sqlite_u64(now_ms)?,
            ],
        )?;
        Ok(())
    }

    pub(super) fn persist_shared_commit(
        &mut self,
        execution_id: ExecId,
        version: ExecutionVersion,
        plan: &CommitPlan,
    ) -> Result<(), StoreError> {
        let Some(shared) = plan.shared() else {
            return Ok(());
        };
        let entry = shared.entry();
        let step = entry.step;
        let entry_hash = entry.entry_hash();
        let bytes = borsh::to_vec(shared)
            .map_err(|error| StoreError::Corruption(format!("shared commit encode: {error}")))?;
        self.connection.execute(
            "INSERT INTO public_commits
             (execution_id, step, version, artifact, entry_hash)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                execution_id.0.to_vec(),
                sqlite_u64(step)?,
                sqlite_u64(version.get())?,
                envelope(EnvelopeKind::SharedCommit, &bytes)?,
                entry_hash.to_vec(),
            ],
        )?;
        Ok(())
    }

    pub(super) fn persist_private_commit(
        &mut self,
        execution_id: ExecId,
        version: ExecutionVersion,
        plan: &CommitPlan,
    ) -> Result<(), StoreError> {
        let Some(private) = plan.private() else {
            return Ok(());
        };
        let record = private.record();
        let sequence = record.seq;
        let bytes = borsh::to_vec(private)
            .map_err(|error| StoreError::Corruption(format!("private commit encode: {error}")))?;
        self.connection.execute(
            "INSERT INTO private_commits
             (execution_id, sequence, version, artifact, record_digest)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                execution_id.0.to_vec(),
                sqlite_u64(sequence)?,
                sqlite_u64(version.get())?,
                envelope(EnvelopeKind::PrivateCommit, &bytes)?,
                checksum(&bytes).to_vec(),
            ],
        )?;
        Ok(())
    }

    pub(super) fn validate_occurrences(&mut self) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT execution_id, occurrence_key, digest, input, committed_version
             FROM occurrences ORDER BY execution_id, occurrence_key",
        )?;
        let mut rows = statement.query([])?;
        let mut values = Vec::new();
        while let Some(row) = rows.next()? {
            values.push((
                ExecId(array32(&row.get::<_, Vec<u8>>(0)?, "occurrence execution")?),
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, i64>(4)?,
            ));
        }
        drop(rows);
        drop(statement);
        for (execution_id, key, digest, input, version) in values {
            let state = self
                .load_execution(execution_id)?
                .ok_or(StoreError::ExecutionNotFound(execution_id))?;
            let key: OccurrenceKey = decode_borsh(&key, "occurrence key")?;
            if key.execution_id() != execution_id {
                return Err(StoreError::Corruption(
                    "occurrence key is bound to another execution".into(),
                ));
            }
            let input = open_envelope(
                EnvelopeKind::ExecutionInput,
                &input,
                arena0_protocol::MAX_EXECUTION_INPUT_BYTES,
            )?;
            let input = ExecutionInput::decode(&input)?;
            let evidence = input.occurrence(&state)?;
            if evidence.key() != key
                || evidence.digest().as_bytes() != array32(&digest, "occurrence digest")?.as_slice()
                || sqlite_i64(version)? > state.version().get()
            {
                return Err(StoreError::Corruption(
                    "occurrence row does not match its canonical input".into(),
                ));
            }
        }
        Ok(())
    }

    pub(super) fn validate_public_rows(
        &mut self,
        state: &ExecutionState,
    ) -> Result<(), StoreError> {
        let execution_id = state.execution_id();
        let mut statement = self.connection.prepare(
            "SELECT step, version, artifact, entry_hash FROM public_commits
             WHERE execution_id = ?1 ORDER BY step",
        )?;
        let mut rows = statement.query(params![execution_id.0.to_vec()])?;
        let mut commits = Vec::new();
        while let Some(row) = rows.next()? {
            commits.push((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ));
        }
        drop(rows);
        drop(statement);
        for (index, (step, version, encoded, hash)) in commits.iter().enumerate() {
            let step = sqlite_i64(*step)?;
            if step
                != u64::try_from(index)
                    .map_err(|_| StoreError::Corruption("public step overflow".into()))?
            {
                return Err(StoreError::Corruption(
                    "public commit steps are not gapless".into(),
                ));
            }
            let version = sqlite_i64(*version)?;
            if version == 0 || version > state.version().get() {
                return Err(StoreError::Corruption(
                    "public commit version is invalid".into(),
                ));
            }
            let payload = open_envelope(
                EnvelopeKind::SharedCommit,
                encoded,
                arena0_protocol::MAX_COMMIT_PLAN_BYTES,
            )?;
            let commit: arena0_protocol::SharedCommit = decode_borsh(&payload, "public commit")?;
            let entry = commit.entry();
            if entry.step != step || entry.entry_hash() != array32(hash, "public entry hash")? {
                return Err(StoreError::Corruption(
                    "public commit index mismatch".into(),
                ));
            }
            let commitment = commit.certificate().commitment();
            if commitment.session_id != state.binding().session_id()
                || commitment.step != entry.step
                || commitment.entry_hash != entry.entry_hash()
                || commitment.pre_state != entry.pre_state
                || commitment.post_state != entry.post_state
            {
                return Err(StoreError::Corruption(
                    "public certificate does not match entry".into(),
                ));
            }
        }
        let commit_count = u64::try_from(commits.len())
            .map_err(|_| StoreError::Corruption("public commit count overflows u64".into()))?;
        if state.public().next_step() != commit_count {
            return Err(StoreError::Corruption(
                "public cursor does not match public commits".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn validate_private_rows(
        &mut self,
        state: &ExecutionState,
    ) -> Result<(), StoreError> {
        let execution_id = state.execution_id();
        let mut statement = self.connection.prepare(
            "SELECT sequence, version, artifact, record_digest FROM private_commits
             WHERE execution_id = ?1 ORDER BY sequence",
        )?;
        let mut rows = statement.query(params![execution_id.0.to_vec()])?;
        let mut records = Vec::new();
        while let Some(row) = rows.next()? {
            records.push((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ));
        }
        drop(rows);
        drop(statement);
        for (index, (sequence, version, encoded, digest)) in records.iter().enumerate() {
            let sequence = sqlite_i64(*sequence)?;
            if sequence
                != u64::try_from(index)
                    .map_err(|_| StoreError::Corruption("private sequence overflow".into()))?
            {
                return Err(StoreError::Corruption(
                    "private commit sequences are not gapless".into(),
                ));
            }
            let version = sqlite_i64(*version)?;
            if version == 0 || version > state.version().get() {
                return Err(StoreError::Corruption(
                    "private commit version is invalid".into(),
                ));
            }
            let payload = open_envelope(
                EnvelopeKind::PrivateCommit,
                encoded,
                arena0_protocol::MAX_PRIVATE_RECORD_BYTES + 1024,
            )?;
            let commit: arena0_protocol::PrivateCommit = decode_borsh(&payload, "private commit")?;
            if commit.record().seq != sequence
                || checksum(&borsh::to_vec(&commit).map_err(|error| {
                    StoreError::Corruption(format!("private commit encode: {error}"))
                })?) != array32(digest, "private record digest")?
            {
                return Err(StoreError::Corruption(
                    "private commit index mismatch".into(),
                ));
            }
        }
        let record_count = u64::try_from(records.len())
            .map_err(|_| StoreError::Corruption("private commit count overflows u64".into()))?;
        if state.private().next_record() != record_count {
            return Err(StoreError::Corruption(
                "private cursor does not match private commits".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn validate_timer_rows(&mut self, state: &ExecutionState) -> Result<(), StoreError> {
        let expected: std::collections::BTreeSet<_> = state.active_timers().collect();
        let mut statement = self.connection.prepare(
            "SELECT timer_id, deadline_ms, payload, armed_version FROM active_timers
             WHERE execution_id = ?1 ORDER BY timer_id",
        )?;
        let mut rows = statement.query(params![state.execution_id().0.to_vec()])?;
        let mut actual = std::collections::BTreeSet::new();
        while let Some(row) = rows.next()? {
            let timer_id = TimerId::from_bytes(array32(&row.get::<_, Vec<u8>>(0)?, "timer id")?);
            if !expected.contains(&timer_id) {
                return Err(StoreError::Corruption(
                    "timer projection contains an unknown timer".into(),
                ));
            }
            let deadline = sqlite_i64(row.get::<_, i64>(1)?)?;
            let armed_version = sqlite_i64(row.get::<_, i64>(3)?)?;
            if armed_version == 0 || armed_version > state.version().get() {
                return Err(StoreError::Corruption(
                    "timer armed version is invalid".into(),
                ));
            }
            let _ = deadline;
            let _ = open_envelope(
                EnvelopeKind::Timer,
                &row.get::<_, Vec<u8>>(2)?,
                arena0_protocol::MAX_TIMER_PAYLOAD_BYTES,
            )?;
            actual.insert(timer_id);
        }
        if actual != expected {
            return Err(StoreError::Corruption(
                "timer projection does not match state".into(),
            ));
        }
        Ok(())
    }
}
