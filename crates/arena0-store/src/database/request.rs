use super::*;

impl Database {
    pub(super) fn create_execution_request(
        &mut self,
        execution_id: ExecId,
        program_hash: ProgramHash,
        params_bytes: Option<Vec<u8>>,
        admission: ExecutionAdmission,
        created_at_ms: u64,
    ) -> Result<ExecutionRequestOutcome, StoreError> {
        let params_len = params_bytes.as_ref().map_or(0, Vec::len);
        if params_len > arena0_protocol::MAX_PARAMS_LEN {
            return Err(StoreError::CommandTooLarge {
                required: params_len,
                capacity: arena0_protocol::MAX_PARAMS_LEN,
            });
        }
        validate_request_params(&admission, params_bytes.is_some())?;
        self.begin()?;
        let result = (|| {
            let existing = self.load_execution_request_in_transaction(execution_id)?;
            if let Some(existing) = existing {
                if existing.program_hash == program_hash
                    && existing.params.as_ref().map(JsonBytes::as_bytes) == params_bytes.as_deref()
                    && existing.admission == admission
                {
                    return Ok(ExecutionRequestOutcome::AlreadyExists);
                }
                return Ok(ExecutionRequestOutcome::Conflict);
            }
            self.ensure_program_registered(program_hash)?;
            validate_local_admission(self.host_id, &admission)?;
            let admission_bytes = borsh::to_vec(&admission).map_err(|error| {
                StoreError::InvalidAdmission(format!("admission encoding failed: {error}"))
            })?;
            if admission_bytes.len() > MAX_ADMISSION_BYTES {
                return Err(StoreError::CommandTooLarge {
                    required: admission_bytes.len(),
                    capacity: MAX_ADMISSION_BYTES,
                });
            }
            self.connection.execute(
                "INSERT INTO exec_requests
                 (execution_id, program_hash, params, admission, created_at_ms, failure)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
                params![
                    execution_id.0.to_vec(),
                    program_hash.as_bytes().to_vec(),
                    params_bytes,
                    envelope(EnvelopeKind::ExecutionAdmission, &admission_bytes)?,
                    sqlite_u64(created_at_ms)?,
                ],
            )?;
            Ok(ExecutionRequestOutcome::Created)
        })();
        match result {
            Ok(outcome) => self.commit_result(outcome),
            Err(error) => self.rollback_result(error),
        }
    }

    pub(super) fn load_execution_request(
        &mut self,
        execution_id: ExecId,
    ) -> Result<Option<ExecutionRequest>, StoreError> {
        self.load_execution_request_in_transaction(execution_id)
    }

    pub(super) fn bind_join_target(
        &mut self,
        execution_id: ExecId,
        target: NegotiationTarget,
    ) -> Result<AdmissionBindingOutcome, StoreError> {
        if target.creator == self.host_id {
            return Err(StoreError::InvalidAdmission(
                "a Host cannot join its own negotiation".into(),
            ));
        }
        self.begin()?;
        let result = (|| {
            let request = self
                .load_execution_request_in_transaction(execution_id)?
                .ok_or(StoreError::ExecutionRequestNotFound(execution_id))?;
            if request.failure().is_some() {
                return Err(StoreError::ExecutionLifecycleStarted(execution_id));
            }
            if self.load_activation_in_transaction(execution_id)?.is_some() {
                return Err(StoreError::ExecutionLifecycleStarted(execution_id));
            }
            let admission = match request.admission {
                ExecutionAdmission::Join { target: None } => ExecutionAdmission::Join {
                    target: Some(target),
                },
                ExecutionAdmission::Join {
                    target: Some(existing),
                } if existing == target => return Ok(AdmissionBindingOutcome::AlreadyBound),
                ExecutionAdmission::Join { target: Some(_) } => {
                    return Ok(AdmissionBindingOutcome::Conflict);
                }
                _ => {
                    return Err(StoreError::InvalidAdmission(
                        "execution admission is not an open Join".into(),
                    ));
                }
            };
            let admission_bytes = borsh::to_vec(&admission).map_err(|error| {
                StoreError::InvalidAdmission(format!("admission encoding failed: {error}"))
            })?;
            if admission_bytes.len() > MAX_ADMISSION_BYTES {
                return Err(StoreError::CommandTooLarge {
                    required: admission_bytes.len(),
                    capacity: MAX_ADMISSION_BYTES,
                });
            }
            self.connection.execute(
                "UPDATE exec_requests SET admission = ?1
                 WHERE execution_id = ?2 AND failure IS NULL",
                params![
                    envelope(EnvelopeKind::ExecutionAdmission, &admission_bytes)?,
                    execution_id.0.to_vec(),
                ],
            )?;
            Ok(AdmissionBindingOutcome::Bound)
        })();
        match result {
            Ok(outcome) => self.commit_result(outcome),
            Err(error) => self.rollback_result(error),
        }
    }

    pub(super) fn load_execution_request_in_transaction(
        &mut self,
        execution_id: ExecId,
    ) -> Result<Option<ExecutionRequest>, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT created_order, program_hash, params, admission,
                        created_at_ms, failure
                 FROM exec_requests WHERE execution_id = ?1",
                params![execution_id.0.to_vec()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(created_order, program_hash, params_bytes, admission, created_at_ms, failure)| {
                let admission: ExecutionAdmission = decode_borsh(
                    &open_envelope(
                        EnvelopeKind::ExecutionAdmission,
                        &admission,
                        MAX_ADMISSION_BYTES,
                    )?,
                    "execution admission",
                )?;
                let params = params_bytes
                    .map(|bytes| {
                        JsonBytes::try_new(bytes)
                            .map_err(|e| StoreError::Corruption(format!("request params: {e}")))
                    })
                    .transpose()?;
                validate_request_params(&admission, params.is_some()).map_err(|error| {
                    StoreError::Corruption(format!("invalid request params: {error}"))
                })?;
                Ok(ExecutionRequest {
                    execution_id,
                    program_hash: ProgramHash(array32(&program_hash, "request program hash")?),
                    params,
                    admission,
                    created_order: sqlite_i64(created_order)?,
                    created_at_ms: sqlite_i64(created_at_ms)?,
                    failure,
                })
            },
        )
        .transpose()
    }

    pub(super) fn list_execution_requests(
        &mut self,
        limit: usize,
    ) -> Result<Vec<ExecutionRequest>, StoreError> {
        let limit = i64::try_from(limit)
            .map_err(|_| StoreError::InvalidConfiguration("request limit is too large"))?;
        let mut statement = self.connection.prepare(
            "SELECT execution_id FROM exec_requests
             ORDER BY created_order LIMIT ?1",
        )?;
        let mut rows = statement.query(params![limit])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            ids.push(ExecId(array32(
                &row.get::<_, Vec<u8>>(0)?,
                "request execution id",
            )?));
        }
        drop(rows);
        drop(statement);
        let mut response_bytes = 0;
        ids.into_iter()
            .map(|execution_id| {
                let request = self
                    .load_execution_request_in_transaction(execution_id)?
                    .ok_or_else(|| {
                        StoreError::Corruption("request disappeared while listing".into())
                    })?;
                account_response(
                    &mut response_bytes,
                    request
                        .params
                        .as_ref()
                        .map_or(0, JsonBytes::len)
                        .checked_add(128)
                        .ok_or(StoreError::CommandTooLarge {
                            required: usize::MAX,
                            capacity: MAX_RESPONSE_BYTES,
                        })?,
                )?;
                if let Some(failure) = &request.failure {
                    account_response(&mut response_bytes, failure.len())?;
                }
                Ok(request)
            })
            .collect()
    }

    pub(super) fn record_execution_request_failure(
        &mut self,
        execution_id: ExecId,
        reason: String,
    ) -> Result<ExecutionRequestFailureOutcome, StoreError> {
        self.begin()?;
        let result = (|| {
            let request = self
                .load_execution_request_in_transaction(execution_id)?
                .ok_or(StoreError::ExecutionRequestNotFound(execution_id))?;
            if self.load_activation_in_transaction(execution_id)?.is_some() {
                return Err(StoreError::ExecutionLifecycleStarted(execution_id));
            }
            match request.failure() {
                Some(existing) if existing == reason => {
                    Ok(ExecutionRequestFailureOutcome::AlreadyRecorded)
                }
                Some(_) => Ok(ExecutionRequestFailureOutcome::Conflict),
                None => {
                    self.connection.execute(
                        "UPDATE exec_requests SET failure = ?1 WHERE execution_id = ?2
                         AND failure IS NULL",
                        params![reason, execution_id.0.to_vec()],
                    )?;
                    Ok(ExecutionRequestFailureOutcome::Recorded)
                }
            }
        })();
        match result {
            Ok(outcome) => self.commit_result(outcome),
            Err(error) => self.rollback_result(error),
        }
    }

    pub(super) fn ensure_execution_request_matches(
        &mut self,
        execution_id: ExecId,
        prepared: &PreparedActivation,
    ) -> Result<(), StoreError> {
        let request = self
            .load_execution_request_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionRequestNotFound(execution_id))?;
        if request.failure().is_some()
            || request.program_hash != prepared.offer().data().program_hash
            || request.params.as_ref().is_some_and(|params| {
                params.as_bytes() != prepared.offer().data().params.as_bytes()
            })
            || request.admission.negotiation_id() != Some(prepared.offer().data().negotiation_id)
        {
            return Err(StoreError::Corruption(
                "prepared activation does not match its execution request".into(),
            ));
        }
        ensure_admission_authority(self.host_id, &request.admission, prepared)?;
        Ok(())
    }

    pub(super) fn validate_execution_requests(&mut self) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT execution_id, created_order, program_hash, params,
                    admission, created_at_ms, failure FROM exec_requests
             ORDER BY created_order",
        )?;
        let mut rows = statement.query([])?;
        let mut values = Vec::new();
        while let Some(row) = rows.next()? {
            values.push((
                ExecId(array32(&row.get::<_, Vec<u8>>(0)?, "request execution id")?),
                row.get::<_, i64>(1)?,
                ProgramHash(array32(&row.get::<_, Vec<u8>>(2)?, "request program hash")?),
                row.get::<_, Option<Vec<u8>>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
            ));
        }
        drop(rows);
        drop(statement);
        for (
            execution_id,
            created_order,
            program_hash,
            params_bytes,
            admission_bytes,
            created_at_ms,
            failure,
        ) in values
        {
            if sqlite_i64(created_order)? == 0
                || params_bytes
                    .as_ref()
                    .is_some_and(|params| params.len() > arena0_protocol::MAX_PARAMS_LEN)
                || admission_bytes.len() > MAX_ADMISSION_BYTES + MAX_ENVELOPE_OVERHEAD
                || failure
                    .as_ref()
                    .is_some_and(|value| value.len() > MAX_ERROR_BYTES)
            {
                return Err(StoreError::Corruption(
                    "execution request row exceeds its bounds".into(),
                ));
            }
            let _ = sqlite_i64(created_at_ms)?;
            let admission: ExecutionAdmission = decode_borsh(
                &open_envelope(
                    EnvelopeKind::ExecutionAdmission,
                    &admission_bytes,
                    MAX_ADMISSION_BYTES,
                )?,
                "execution admission",
            )?;
            validate_local_admission(self.host_id, &admission).map_err(|error| {
                StoreError::Corruption(format!("invalid durable execution admission: {error}"))
            })?;
            let params = params_bytes
                .as_ref()
                .map(|bytes| {
                    JsonBytes::try_new(bytes.clone()).map_err(|error| {
                        StoreError::Corruption(format!("invalid durable request params: {error}"))
                    })
                })
                .transpose()?;
            validate_request_params(&admission, params.is_some()).map_err(|error| {
                StoreError::Corruption(format!("invalid durable request params: {error}"))
            })?;
            let activation = self.load_activation_in_transaction(execution_id)?;
            if failure.is_some() && activation.is_some() {
                return Err(StoreError::Corruption(format!(
                    "failed execution request {execution_id} has an activation record"
                )));
            }
            let program = self
                .connection
                .query_row(
                    "SELECT wasm FROM programs WHERE program_hash = ?1",
                    params![program_hash.as_bytes().to_vec()],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()?;
            if program.is_none() {
                // Recovery owns the terminal transition for a missing
                // artifact. Keep the request visible so it can be marked
                // failed before an actor is registered. An execution
                // aggregate, when present, is failed through the authenticated
                // protocol boundary instead.
            } else if let Some(activation) = activation {
                let offer = activation.prepared().offer().data();
                if offer.program_hash != program_hash
                    || params
                        .as_ref()
                        .is_some_and(|params| offer.params.as_bytes() != params.as_bytes())
                    || admission.negotiation_id() != Some(offer.negotiation_id)
                {
                    return Err(StoreError::Corruption(
                        "activation does not match its execution request".into(),
                    ));
                }
                ensure_admission_authority(self.host_id, &admission, activation.prepared())
                    .map_err(|error| {
                        StoreError::Corruption(format!(
                            "activation exceeds durable admission authority: {error}"
                        ))
                    })?;
            }
        }
        Ok(())
    }
}

fn validate_local_admission(
    host_id: PeerId,
    admission: &ExecutionAdmission,
) -> Result<(), StoreError> {
    match admission {
        ExecutionAdmission::Create {
            negotiation_id,
            participant_count,
        } => ExecutionAdmission::create(*negotiation_id, *participant_count)
            .map(|_| ())
            .map_err(|error| StoreError::InvalidAdmission(error.to_string())),
        ExecutionAdmission::Join {
            target: Some(target),
        } if target.creator == host_id => Err(StoreError::InvalidAdmission(
            "a Host cannot join its own negotiation".into(),
        )),
        _ => Ok(()),
    }
}

fn validate_request_params(
    admission: &ExecutionAdmission,
    params_present: bool,
) -> Result<(), StoreError> {
    if matches!(admission, ExecutionAdmission::Create { .. }) && !params_present {
        return Err(StoreError::InvalidAdmission(
            "creator admission requires params".into(),
        ));
    }
    Ok(())
}

fn ensure_admission_authority(
    host_id: PeerId,
    admission: &ExecutionAdmission,
    prepared: &PreparedActivation,
) -> Result<(), StoreError> {
    let offer = prepared.offer().data();
    match admission {
        ExecutionAdmission::Create {
            participant_count,
            negotiation_id: _,
        } => {
            if offer.creator != host_id || offer.target_size != *participant_count {
                return Err(StoreError::InvalidAdmission(
                    "prepared activation changes the creator participant count".into(),
                ));
            }
        }
        ExecutionAdmission::Join {
            target: Some(target),
        } => {
            if offer.creator != target.creator || offer.negotiation_id != target.negotiation_id {
                return Err(StoreError::InvalidAdmission(
                    "prepared activation came from a different creator".into(),
                ));
            }
        }
        ExecutionAdmission::Join { target: None } => {
            return Err(StoreError::InvalidAdmission(
                "prepared activation has no selected Join target".into(),
            ));
        }
    }
    Ok(())
}
