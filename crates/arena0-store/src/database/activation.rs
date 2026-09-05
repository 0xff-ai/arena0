use super::*;

impl Database {
    pub(super) fn prepare_activation(
        &mut self,
        execution_id: ExecId,
        prepared: PreparedActivation,
        now_ms: u64,
    ) -> Result<PrepareActivationOutcome, StoreError> {
        prepared.validate().map_err(|error| {
            StoreError::Corruption(format!("activation validation failed: {error}"))
        })?;
        self.begin()?;
        let result = self.prepare_activation_in_transaction(execution_id, prepared, now_ms);
        match result {
            Ok(outcome) => self.commit_result(outcome),
            Err(error) => self.rollback_result(error),
        }
    }

    pub(super) fn prepare_activation_in_transaction(
        &mut self,
        execution_id: ExecId,
        prepared: PreparedActivation,
        now_ms: u64,
    ) -> Result<PrepareActivationOutcome, StoreError> {
        let incoming = prepared_activation_record(execution_id, prepared.clone());
        let encoded = prepared_activation_bytes(&prepared)?;
        self.ensure_program_registered(prepared.offer().data().program_hash)?;
        let existing = self.load_activation_in_transaction(execution_id)?;
        let Some(existing) = existing else {
            self.ensure_execution_request_matches(execution_id, &prepared)?;
            self.connection.execute(
                "INSERT INTO activation_records
                 (execution_id, session_id, status, prepared_activation, committed_activation,
                  created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, 'prepared', ?3, NULL, ?4, ?4)",
                params![
                    execution_id.0.to_vec(),
                    prepared.session_hash().0.to_vec(),
                    envelope(EnvelopeKind::PreparedActivation, &encoded)?,
                    sqlite_u64(now_ms)?,
                ],
            )?;
            return Ok(PrepareActivationOutcome::Prepared(Box::new(incoming)));
        };
        if existing.prepared() == &prepared {
            return Ok(match existing.status() {
                ActivationRecordStatus::Prepared => {
                    PrepareActivationOutcome::AlreadyPrepared(Box::new(existing))
                }
                ActivationRecordStatus::Committed => {
                    PrepareActivationOutcome::AlreadyCommitted(Box::new(existing))
                }
            });
        }
        self.record_activation_conflict(
            &existing,
            &incoming,
            ActivationConflictKind::Prepared,
            now_ms,
        )?;
        Ok(PrepareActivationOutcome::Conflict {
            existing: Box::new(existing),
            incoming: Box::new(incoming),
        })
    }

    pub(crate) fn ensure_program_registered(
        &mut self,
        hash: ProgramHash,
    ) -> Result<(), StoreError> {
        let registered: Option<Option<i64>> = self
            .connection
            .query_row(
                "SELECT removed_at_ms FROM programs WHERE program_hash = ?1",
                params![hash.as_bytes().to_vec()],
                |row| row.get(0),
            )
            .optional()?;
        if !matches!(registered, Some(None)) {
            return Err(StoreError::Corruption(
                "activation references a program that is not currently registered".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn commit_activation(
        &mut self,
        execution_id: ExecId,
        activation: Activation,
        now_ms: u64,
    ) -> Result<CommitActivationOutcome, StoreError> {
        activation.validate().map_err(|error| {
            StoreError::Corruption(format!("activation validation failed: {error}"))
        })?;
        self.begin()?;
        let result = self.commit_activation_in_transaction(execution_id, activation, now_ms);
        match result {
            Ok(outcome) => self.commit_result(outcome),
            Err(error) => self.rollback_result(error),
        }
    }

    pub(super) fn commit_activation_in_transaction(
        &mut self,
        execution_id: ExecId,
        activation: Activation,
        now_ms: u64,
    ) -> Result<CommitActivationOutcome, StoreError> {
        let incoming = committed_activation_record(execution_id, activation.clone())?;
        let Some(existing) = self.load_activation_in_transaction(execution_id)? else {
            return Err(StoreError::ActivationNotFound(execution_id));
        };
        if !existing.prepared().matches(&activation) {
            self.record_activation_conflict(
                &existing,
                &incoming,
                ActivationConflictKind::Prepared,
                now_ms,
            )?;
            return Ok(CommitActivationOutcome::Conflict {
                existing: Box::new(existing),
                incoming: Box::new(incoming),
            });
        }
        if existing.status() == ActivationRecordStatus::Committed {
            if existing.activation() == incoming.activation() {
                return Ok(CommitActivationOutcome::AlreadyCommitted(Box::new(
                    existing,
                )));
            }
            self.record_activation_conflict(
                &existing,
                &incoming,
                ActivationConflictKind::Committed,
                now_ms,
            )?;
            return Ok(CommitActivationOutcome::Conflict {
                existing: Box::new(existing),
                incoming: Box::new(incoming),
            });
        }
        let encoded = activation_bytes(&activation)?;
        let changed = self.connection.execute(
            "UPDATE activation_records SET status = 'committed', committed_activation = ?2,
                    updated_at_ms = ?1
             WHERE execution_id = ?3 AND status = 'prepared'",
            params![
                sqlite_u64(now_ms)?,
                envelope(EnvelopeKind::Activation, &encoded)?,
                execution_id.0.to_vec(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Corruption(
                "activation commit compare-and-set changed no row".into(),
            ));
        }
        Ok(CommitActivationOutcome::Committed(Box::new(incoming)))
    }

    pub(super) fn load_activation(
        &mut self,
        execution_id: ExecId,
    ) -> Result<Option<ActivationRecord>, StoreError> {
        self.load_activation_in_transaction(execution_id)
    }

    pub(super) fn load_activation_in_transaction(
        &mut self,
        execution_id: ExecId,
    ) -> Result<Option<ActivationRecord>, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT session_id, status, prepared_activation, committed_activation
                 FROM activation_records WHERE execution_id = ?1",
                params![execution_id.0.to_vec()],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(session_bytes, status, prepared_bytes, committed_bytes)| {
            let payload = open_envelope(
                EnvelopeKind::PreparedActivation,
                &prepared_bytes,
                MAX_FRAME_BYTES,
            )?;
            let prepared: PreparedActivation = decode_borsh(&payload, "prepared activation")?;
            prepared
                .validate()
                .map_err(|error| StoreError::Corruption(format!("prepared activation: {error}")))?;
            let status = parse_activation_status(&status)?;
            let session_id = SessionHash(array32(&session_bytes, "activation session")?);
            if prepared.session_hash() == SessionHash([0; 32]) {
                return Err(StoreError::Corruption(
                    "activation has zero session identity".into(),
                ));
            }
            if prepared.session_hash() != session_id {
                return Err(StoreError::Corruption(
                    "activation session index does not match prepared evidence".into(),
                ));
            }
            let committed = committed_bytes
                .map(|bytes| {
                    let payload = open_envelope(EnvelopeKind::Activation, &bytes, MAX_FRAME_BYTES)?;
                    let activation: Activation = decode_borsh(&payload, "activation")?;
                    activation
                        .validate()
                        .map_err(|error| StoreError::Corruption(format!("activation: {error}")))?;
                    if !prepared.matches(&activation) {
                        return Err(StoreError::Corruption(
                            "committed activation does not match prepared evidence".into(),
                        ));
                    }
                    Ok(activation)
                })
                .transpose()?;
            if matches!(status, ActivationRecordStatus::Prepared) != committed.is_none() {
                return Err(StoreError::Corruption(
                    "activation status and committed evidence disagree".into(),
                ));
            }
            let state = match (status, committed) {
                (ActivationRecordStatus::Prepared, None) => {
                    ActivationRecordState::Prepared { evidence: prepared }
                }
                (ActivationRecordStatus::Committed, Some(activation)) => {
                    ActivationRecordState::Committed {
                        evidence: prepared,
                        activation: Box::new(activation),
                    }
                }
                _ => {
                    return Err(StoreError::Corruption(
                        "activation status and committed evidence disagree".into(),
                    ));
                }
            };
            Ok(ActivationRecord {
                execution_id,
                state,
            })
        })
        .transpose()
    }

    pub(super) fn record_activation_conflict(
        &mut self,
        existing: &ActivationRecord,
        incoming: &ActivationRecord,
        kind: ActivationConflictKind,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let (envelope_kind, existing_bytes, incoming_bytes) = match kind {
            ActivationConflictKind::Prepared => (
                EnvelopeKind::PreparedActivation,
                prepared_activation_bytes(existing.prepared())?,
                prepared_activation_bytes(incoming.prepared())?,
            ),
            ActivationConflictKind::Committed => {
                let existing_activation = existing.activation().ok_or_else(|| {
                    StoreError::Corruption(
                        "committed activation conflict has no existing certificate".into(),
                    )
                })?;
                let incoming_activation = incoming.activation().ok_or_else(|| {
                    StoreError::Corruption(
                        "committed activation conflict has no incoming certificate".into(),
                    )
                })?;
                (
                    EnvelopeKind::Activation,
                    activation_bytes(existing_activation)?,
                    activation_bytes(incoming_activation)?,
                )
            }
        };
        self.connection.execute(
            "INSERT INTO activation_conflicts
             (execution_id, conflict_kind, existing_activation, incoming_activation, observed_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                existing.execution_id.0.to_vec(),
                match kind {
                    ActivationConflictKind::Prepared => "prepared",
                    ActivationConflictKind::Committed => "committed",
                },
                envelope(envelope_kind, &existing_bytes)?,
                envelope(envelope_kind, &incoming_bytes)?,
                sqlite_u64(now_ms)?,
            ],
        )?;
        Ok(())
    }

    pub(super) fn validate_activation_conflicts(&mut self) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT execution_id, conflict_kind, existing_activation,
                    incoming_activation FROM activation_conflicts",
        )?;
        let mut rows = statement.query([])?;
        let mut values = Vec::new();
        while let Some(row) = rows.next()? {
            values.push((
                ExecId(array32(
                    &row.get::<_, Vec<u8>>(0)?,
                    "activation conflict execution",
                )?),
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ));
        }
        drop(rows);
        drop(statement);
        for (execution_id, kind, existing, incoming) in values {
            let kind = match kind.as_str() {
                "prepared" => EnvelopeKind::PreparedActivation,
                "committed" => EnvelopeKind::Activation,
                other => {
                    return Err(StoreError::Corruption(format!(
                        "unknown activation conflict kind {other}"
                    )));
                }
            };
            let record = self
                .load_activation_in_transaction(execution_id)?
                .ok_or_else(|| {
                    StoreError::Corruption("activation conflict has no owning activation".into())
                })?;
            let existing = open_envelope(kind, &existing, MAX_FRAME_BYTES)?;
            let incoming = open_envelope(kind, &incoming, MAX_FRAME_BYTES)?;
            match kind {
                EnvelopeKind::PreparedActivation => {
                    let existing: PreparedActivation =
                        decode_borsh(&existing, "prepared activation conflict")?;
                    let incoming: PreparedActivation =
                        decode_borsh(&incoming, "prepared activation conflict")?;
                    existing.validate().map_err(|error| {
                        StoreError::Corruption(format!("prepared conflict evidence: {error}"))
                    })?;
                    incoming.validate().map_err(|error| {
                        StoreError::Corruption(format!("prepared conflict evidence: {error}"))
                    })?;
                    if record.prepared() != &existing {
                        return Err(StoreError::Corruption(
                            "prepared conflict existing evidence does not match activation".into(),
                        ));
                    }
                }
                EnvelopeKind::Activation => {
                    let existing: Activation = decode_borsh(&existing, "activation conflict")?;
                    let incoming: Activation = decode_borsh(&incoming, "activation conflict")?;
                    existing.validate().map_err(|error| {
                        StoreError::Corruption(format!("committed conflict evidence: {error}"))
                    })?;
                    incoming.validate().map_err(|error| {
                        StoreError::Corruption(format!("committed conflict evidence: {error}"))
                    })?;
                    if record.activation() != Some(&existing) {
                        return Err(StoreError::Corruption(
                            "committed conflict existing evidence does not match activation".into(),
                        ));
                    }
                }
                _ => unreachable!("conflict kind maps only to activation envelopes"),
            }
        }
        Ok(())
    }
}
