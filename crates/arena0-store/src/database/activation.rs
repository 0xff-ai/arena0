use super::*;

/// Raw SQLite activation columns; decoding validates envelopes and every denormalized index together.
struct RawActivationRow {
    execution_id: ExecId,
    session_bytes: Vec<u8>,
    status: String,
    prepared_bytes: Vec<u8>,
    committed_bytes: Option<Vec<u8>>,
    updated_at_ms: i64,
    facts_bytes: Vec<u8>,
}

/// Own the record's envelope decoding and index checks together, so timed
/// reads and transactional reads reject the same inconsistent evidence.
impl RawActivationRow {
    /// Consume one row only after checking its status, session and facts against authenticated evidence.
    fn decode(self) -> Result<ActivationRecord, StoreError> {
        let payload = DurableEnvelope::open(
            EnvelopeKind::PreparedActivation,
            &self.prepared_bytes,
            MAX_ACTIVATION_BYTES,
        )?;
        let prepared: PreparedActivation = decode_borsh(&payload, "prepared activation")?;
        prepared
            .validate()
            .map_err(|error| StoreError::Corruption(format!("prepared activation: {error}")))?;
        let facts: ActivationFacts = decode_borsh(&self.facts_bytes, "activation facts")?;
        let expected = ActivationFacts::of(&prepared);
        if facts != expected {
            return Err(StoreError::Corruption(
                "activation facts do not match decoded activation".into(),
            ));
        }
        let status = ActivationRecordStatus::parse(&self.status)?;
        let session_id = SessionHash(array32(&self.session_bytes, "activation session")?);
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
        let committed = self
            .committed_bytes
            .map(|bytes| {
                let payload =
                    DurableEnvelope::open(EnvelopeKind::Activation, &bytes, MAX_ACTIVATION_BYTES)?;
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
            execution_id: self.execution_id,
            state,
            updated_at_ms: sqlite_i64(self.updated_at_ms)?,
        })
    }
}

impl Database {
    pub(crate) fn prepare_activation(
        &mut self,
        execution_id: ExecId,
        prepared: PreparedActivation,
        now_ms: u64,
    ) -> Result<PrepareActivationOutcome, StoreError> {
        prepared.validate().map_err(|error| {
            StoreError::Corruption(format!("activation validation failed: {error}"))
        })?;
        self.transaction(|store| {
            store.prepare_activation_in_transaction(execution_id, prepared, now_ms)
        })
    }

    pub(super) fn prepare_activation_in_transaction(
        &mut self,
        execution_id: ExecId,
        prepared: PreparedActivation,
        now_ms: u64,
    ) -> Result<PrepareActivationOutcome, StoreError> {
        let incoming = ActivationRecord::new_prepared(execution_id, prepared.clone(), now_ms);
        let encoded = encode_borsh(&prepared, "prepared activation")?;
        self.ensure_program_registered(prepared.offer().data().program_hash)?;
        let existing = self.load_activation_in_transaction(execution_id)?;
        let Some(existing) = existing else {
            self.ensure_execution_request_matches(execution_id, &prepared)?;
            let facts = ActivationFacts::of(&prepared);
            self.connection.execute(
                "INSERT INTO activation_records
                 (execution_id, session_id, status, prepared_activation, committed_activation,
                  created_at_ms, updated_at_ms, facts)
                 VALUES (?1, ?2, 'prepared', ?3, NULL, ?4, ?4, ?5)",
                params![
                    execution_id.0.to_vec(),
                    prepared.session_hash().0.to_vec(),
                    DurableEnvelope::seal(EnvelopeKind::PreparedActivation, &encoded)?,
                    sqlite_u64(now_ms)?,
                    encode_borsh(&facts, "activation facts")?,
                ],
            )?;
            self.record_change(ChangeKey::Exec(execution_id));
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

    pub(super) fn ensure_program_registered(
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

    pub(crate) fn commit_activation(
        &mut self,
        execution_id: ExecId,
        activation: Activation,
        now_ms: u64,
    ) -> Result<CommitActivationOutcome, StoreError> {
        activation.validate().map_err(|error| {
            StoreError::Corruption(format!("activation validation failed: {error}"))
        })?;
        self.transaction(|store| {
            store.commit_activation_in_transaction(execution_id, activation, now_ms)
        })
    }

    pub(super) fn commit_activation_in_transaction(
        &mut self,
        execution_id: ExecId,
        activation: Activation,
        now_ms: u64,
    ) -> Result<CommitActivationOutcome, StoreError> {
        let incoming = ActivationRecord::new_committed(execution_id, activation.clone(), now_ms);
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
        let encoded = encode_borsh(&activation, "activation")?;
        let facts = ActivationFacts::of(activation.prepared());
        let changed = self.connection.execute(
            "UPDATE activation_records SET status = 'committed', committed_activation = ?2,
                    updated_at_ms = ?1, facts = ?4
             WHERE execution_id = ?3 AND status = 'prepared'",
            params![
                sqlite_u64(now_ms)?,
                DurableEnvelope::seal(EnvelopeKind::Activation, &encoded)?,
                execution_id.0.to_vec(),
                encode_borsh(&facts, "activation facts")?,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Corruption(
                "activation commit compare-and-set changed no row".into(),
            ));
        }
        self.record_change(ChangeKey::Exec(execution_id));
        Ok(CommitActivationOutcome::Committed(Box::new(incoming)))
    }

    pub(crate) fn load_activation(
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
                "SELECT session_id, status, prepared_activation, committed_activation,
                        updated_at_ms, facts
                 FROM activation_records WHERE execution_id = ?1",
                params![execution_id.0.to_vec()],
                |row| {
                    Ok(RawActivationRow {
                        execution_id,
                        session_bytes: row.get(0)?,
                        status: row.get(1)?,
                        prepared_bytes: row.get(2)?,
                        committed_bytes: row.get(3)?,
                        updated_at_ms: row.get(4)?,
                        facts_bytes: row.get(5)?,
                    })
                },
            )
            .optional()?;
        row.map(|row| super::integrity::timed_decode("decode.activation", || row.decode()))
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
                encode_borsh(existing.prepared(), "prepared activation")?,
                encode_borsh(incoming.prepared(), "prepared activation")?,
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
                    encode_borsh(existing_activation, "activation")?,
                    encode_borsh(incoming_activation, "activation")?,
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
                DurableEnvelope::seal(envelope_kind, &existing_bytes)?,
                DurableEnvelope::seal(envelope_kind, &incoming_bytes)?,
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
            // Conflict kinds remain registry facts; archived execution
            // evidence, including its owning activation, is not decoded at open.
            if self.execution_is_archived(execution_id)? {
                continue;
            }
            let record = self
                .load_activation_in_transaction(execution_id)?
                .ok_or_else(|| {
                    StoreError::Corruption("activation conflict has no owning activation".into())
                })?;
            let existing = DurableEnvelope::open(kind, &existing, MAX_ACTIVATION_BYTES)?;
            let incoming = DurableEnvelope::open(kind, &incoming, MAX_ACTIVATION_BYTES)?;
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
