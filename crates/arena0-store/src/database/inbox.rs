use super::*;
use arena0_protocol::PublicEvent;

struct RawInboxRow {
    source: Vec<u8>,
    digest: Vec<u8>,
    frame: Vec<u8>,
    status: String,
    applied_version: Option<i64>,
    consumed_at_ms: Option<i64>,
}

struct ValidatedInboxFrame {
    source: PeerId,
    stored: StoredFrame,
    frame: ExecFrame,
}

impl Database {
    pub(super) fn inbox_row(
        &mut self,
        execution_id: ExecId,
        inbox_id: InboxId,
    ) -> Result<Option<(PeerId, InboxStatus, Option<ExecutionVersion>)>, StoreError> {
        let row: Option<(Vec<u8>, String, Option<i64>)> = self.connection.query_row(
            "SELECT source, status, applied_version FROM inbox WHERE execution_id = ?1 AND inbox_id = ?2",
            params![execution_id.0.to_vec(), inbox_id.as_bytes().to_vec()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        match row {
            Some((source, status, version)) => Ok(Some((
                peer_id_from_blob(&source, "inbox source")?,
                parse_inbox_status(&status)?,
                version
                    .map(sqlite_i64)
                    .transpose()?
                    .map(ExecutionVersion::new),
            ))),
            None => Ok(None),
        }
    }

    pub(super) fn accept_inbound(
        &mut self,
        execution_id: ExecId,
        frame: AuthenticatedFrame,
        now_ms: u64,
    ) -> Result<InboxAcceptOutcome, StoreError> {
        self.begin()?;
        let result = self.accept_inbound_in_transaction(execution_id, frame, now_ms);
        match result {
            Ok(value) => self.commit_result(value),
            Err(error) => self.rollback_result(error),
        }
    }

    fn accept_inbound_in_transaction(
        &mut self,
        execution_id: ExecId,
        frame: AuthenticatedFrame,
        now_ms: u64,
    ) -> Result<InboxAcceptOutcome, StoreError> {
        let state = self
            .load_execution_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
        let stored = canonical_frame(&frame, &state)?;
        let frame_bytes = borsh::to_vec(&stored)
            .map_err(|error| StoreError::Corruption(format!("inbox frame encode: {error}")))?;
        let identity_digest = inbox_identity_digest(frame.source, &stored)?;
        let inbox_id = InboxId::from_bytes(identity_digest);
        let digest = identity_digest;
        let existing = self.connection.query_row(
            "SELECT source, digest, frame, status FROM inbox WHERE execution_id = ?1 AND inbox_id = ?2",
            params![execution_id.0.to_vec(), inbox_id.as_bytes().to_vec()],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, Vec<u8>>(2)?, row.get::<_, String>(3)?)),
        ).optional()?;
        if let Some((source, stored_digest, encoded, status)) = existing {
            if source == frame.source.0.to_vec()
                && stored_digest == digest
                && open_envelope(EnvelopeKind::InboundFrame, &encoded, MAX_FRAME_BYTES)?
                    == frame_bytes
            {
                return Ok(match parse_inbox_status(&status)? {
                    InboxStatus::Accepted => InboxAcceptOutcome::AlreadyAccepted,
                    InboxStatus::Applied => InboxAcceptOutcome::AlreadyApplied,
                    InboxStatus::Consumed => InboxAcceptOutcome::AlreadyConsumed,
                });
            }
            self.connection.execute(
                "INSERT INTO inbox_conflicts (execution_id, inbox_id, existing_source, incoming_source, existing_digest, incoming_digest, incoming_frame, observed_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![execution_id.0.to_vec(), inbox_id.as_bytes().to_vec(), source, frame.source.0.to_vec(), stored_digest, digest.to_vec(), envelope(EnvelopeKind::InboundFrame, &frame_bytes)?, sqlite_u64(now_ms)?],
            )?;
            return Ok(InboxAcceptOutcome::Conflict);
        }
        self.connection.execute(
            "INSERT INTO inbox (execution_id, inbox_id, source, digest, frame, status, accepted_at_ms, applied_version, consumed_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, 'accepted', ?6, NULL, NULL)",
            params![execution_id.0.to_vec(), inbox_id.as_bytes().to_vec(), frame.source.0.to_vec(), digest.to_vec(), envelope(EnvelopeKind::InboundFrame, &frame_bytes)?, sqlite_u64(now_ms)?],
        )?;
        Ok(InboxAcceptOutcome::Accepted)
    }

    pub(super) fn list_pending_inbox(
        &mut self,
        execution_id: ExecId,
        limit: usize,
    ) -> Result<Vec<PendingInboxItem>, StoreError> {
        let state = self
            .load_execution(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
        let limit = i64::try_from(limit)
            .map_err(|_| StoreError::InvalidConfiguration("inbox limit is too large"))?;
        let mut statement = self.connection.prepare("SELECT inbox_id, source, digest, frame FROM inbox WHERE execution_id = ?1 AND status = 'accepted' ORDER BY inbox_id LIMIT ?2")?;
        let mut rows = statement.query(params![execution_id.0.to_vec(), limit])?;
        let mut values = Vec::new();
        while let Some(row) = rows.next()? {
            values.push((
                InboxId::from_bytes(array32(&row.get::<_, Vec<u8>>(0)?, "inbox id")?),
                peer_id_from_blob(&row.get::<_, Vec<u8>>(1)?, "inbox source")?,
                array32(&row.get::<_, Vec<u8>>(2)?, "inbox digest")?,
                row.get::<_, Vec<u8>>(3)?,
            ));
        }
        drop(rows);
        drop(statement);
        let mut response_bytes = 0;
        values
            .into_iter()
            .map(|(inbox_id, source, digest, encoded)| {
                let validated = validate_inbox_frame(
                    &state,
                    inbox_id,
                    source,
                    &digest,
                    &encoded,
                    "pending inbox row failed identity validation",
                )?;
                let canonical_bytes = borsh::to_vec(&validated.stored).map_err(|error| {
                    StoreError::Corruption(format!("inbox frame encode: {error}"))
                })?;
                account_response(&mut response_bytes, canonical_bytes.len())?;
                Ok(PendingInboxItem {
                    execution_id,
                    inbox_id,
                    source: validated.source,
                    frame: validated.frame,
                })
            })
            .collect()
    }

    pub(super) fn apply_inbound(
        &mut self,
        execution_id: ExecId,
        inbox_id: InboxId,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        self.begin()?;
        let result = (|| {
            let state = self
                .load_execution_in_transaction(execution_id)?
                .ok_or(StoreError::ExecutionNotFound(execution_id))?;
            let (source, stored, status, applied_version) =
                self.load_inbox_fact(execution_id, inbox_id, &state)?;
            match status {
                InboxStatus::Applied => Ok(ApplyOutcome::InboxAlreadyApplied {
                    inbox_id,
                    version: applied_version.ok_or_else(|| {
                        StoreError::Corruption("applied inbox row has no version".into())
                    })?,
                }),
                InboxStatus::Consumed => Ok(ApplyOutcome::InboxAlreadyConsumed { inbox_id }),
                InboxStatus::Accepted => {
                    let input = match decode_stored_frame(&stored)? {
                        ExecFrame::StepSignature {
                            commitment,
                            signature,
                        } => {
                            ensure_step_commitment(&state, &commitment, inbox_id)?;
                            ExecutionInput::StepSignature(ParticipantStepSignature::new(
                                source,
                                commitment.step,
                                signature,
                            ))
                        }
                        ExecFrame::End {
                            commitment,
                            signature,
                        } => {
                            ensure_terminal_commitment(&state, &commitment, inbox_id)?;
                            ExecutionInput::TerminalSignature(ParticipantTerminalSignature::new(
                                source, signature,
                            ))
                        }
                        ExecFrame::Abort { occurrence } => ExecutionInput::Abort(occurrence),
                        ExecFrame::Message { .. } => {
                            return Err(StoreError::InboxMessageNeedsResolution(inbox_id));
                        }
                    };
                    self.apply_input_in_transaction(
                        execution_id,
                        input,
                        Some(InboxReference {
                            inbox_id,
                            source,
                            complete_on_apply: true,
                        }),
                        now_ms,
                    )
                }
            }
        })();
        match result {
            Ok(value) => self.commit_result(value),
            Err(error) => self.rollback_result(error),
        }
    }

    pub(super) fn apply_inbound_message(
        &mut self,
        execution_id: ExecId,
        inbox_id: InboxId,
        delta: SharedDelta,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        self.begin()?;
        let result = (|| {
            let state = self
                .load_execution_in_transaction(execution_id)?
                .ok_or(StoreError::ExecutionNotFound(execution_id))?;
            let (source, stored, status, applied_version) =
                self.load_inbox_fact(execution_id, inbox_id, &state)?;
            let ExecFrame::Message {
                message_id,
                seq,
                prestate,
                data,
                witness,
            } = decode_stored_frame(&stored)?
            else {
                return Err(StoreError::InboxNotMessage(inbox_id));
            };
            match status {
                InboxStatus::Applied => Ok(ApplyOutcome::InboxAlreadyApplied {
                    inbox_id,
                    version: applied_version.ok_or_else(|| {
                        StoreError::Corruption("applied inbox row has no version".into())
                    })?,
                }),
                InboxStatus::Consumed => Ok(ApplyOutcome::InboxAlreadyConsumed { inbox_id }),
                InboxStatus::Accepted => {
                    let entry = delta.entry();
                    let binds = matches!(&entry.event, PublicEvent::MessageReceived { message_id: candidate_id, from, position, pre_state, msg } if *candidate_id == message_id && *from == source && *position == seq && *pre_state == prestate && msg == &data)
                        && entry.witness == Some(witness);
                    if !binds {
                        return Err(StoreError::InboxInputMismatch {
                            inbox_id,
                            part_index: 0,
                        });
                    }
                    self.apply_input_in_transaction(
                        execution_id,
                        ExecutionInput::ProposeShared(delta),
                        Some(InboxReference {
                            inbox_id,
                            source,
                            complete_on_apply: true,
                        }),
                        now_ms,
                    )
                }
            }
        })();
        match result {
            Ok(value) => self.commit_result(value),
            Err(error) => self.rollback_result(error),
        }
    }

    pub(super) fn reject_inbound(
        &mut self,
        execution_id: ExecId,
        inbox_id: InboxId,
        now_ms: u64,
    ) -> Result<InboxRejectOutcome, StoreError> {
        self.begin()?;
        let result = (|| {
            let state = self
                .load_execution_in_transaction(execution_id)?
                .ok_or(StoreError::ExecutionNotFound(execution_id))?;
            let (_source, _stored, status, _version) =
                self.load_inbox_fact(execution_id, inbox_id, &state)?;
            match status {
                InboxStatus::Applied => Ok(InboxRejectOutcome::AlreadyApplied),
                InboxStatus::Consumed => Ok(InboxRejectOutcome::AlreadyRejected),
                InboxStatus::Accepted => {
                    let changed = self.connection.execute("UPDATE inbox SET status = 'consumed', consumed_at_ms = ?1 WHERE execution_id = ?2 AND inbox_id = ?3 AND status = 'accepted'", params![sqlite_u64(now_ms)?, execution_id.0.to_vec(), inbox_id.as_bytes().to_vec()])?;
                    if changed != 1 {
                        return Err(StoreError::Corruption(
                            "inbound rejection compare-and-set failed".into(),
                        ));
                    }
                    Ok(InboxRejectOutcome::Rejected)
                }
            }
        })();
        match result {
            Ok(value) => self.commit_result(value),
            Err(error) => self.rollback_result(error),
        }
    }

    fn raw_inbox_row(
        &self,
        execution_id: ExecId,
        inbox_id: InboxId,
    ) -> Result<Option<RawInboxRow>, StoreError> {
        self.connection
            .query_row(
                "SELECT source, digest, frame, status, applied_version, consumed_at_ms
                 FROM inbox WHERE execution_id = ?1 AND inbox_id = ?2",
                params![execution_id.0.to_vec(), inbox_id.as_bytes().to_vec()],
                |row| {
                    Ok(RawInboxRow {
                        source: row.get(0)?,
                        digest: row.get(1)?,
                        frame: row.get(2)?,
                        status: row.get(3)?,
                        applied_version: row.get(4)?,
                        consumed_at_ms: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sqlite)
    }

    fn load_inbox_fact(
        &mut self,
        execution_id: ExecId,
        inbox_id: InboxId,
        state: &ExecutionState,
    ) -> Result<(PeerId, StoredFrame, InboxStatus, Option<ExecutionVersion>), StoreError> {
        let row = self
            .raw_inbox_row(execution_id, inbox_id)?
            .ok_or(StoreError::InboxNotAccepted(inbox_id))?;
        let source = peer_id_from_blob(&row.source, "inbox source")?;
        let validated = validate_inbox_frame(
            state,
            inbox_id,
            source,
            &row.digest,
            &row.frame,
            "inbox frame identity validation failed",
        )?;
        Ok((
            validated.source,
            validated.stored,
            parse_inbox_status(&row.status)?,
            row.applied_version
                .map(sqlite_i64)
                .transpose()?
                .map(ExecutionVersion::new),
        ))
    }

    pub(super) fn validate_inbox_rows(&mut self) -> Result<(), StoreError> {
        let mut after: Option<(ExecId, InboxId)> = None;
        loop {
            let keys = {
                let mut statement = if after.is_some() {
                    self.connection.prepare(
                        "SELECT execution_id, inbox_id FROM inbox
                         WHERE (execution_id, inbox_id) > (?1, ?2)
                         ORDER BY execution_id, inbox_id LIMIT ?3",
                    )?
                } else {
                    self.connection.prepare(
                        "SELECT execution_id, inbox_id FROM inbox
                         ORDER BY execution_id, inbox_id LIMIT ?1",
                    )?
                };
                let mut rows = match after {
                    Some((execution_id, inbox_id)) => statement.query(params![
                        execution_id.0.to_vec(),
                        inbox_id.as_bytes().to_vec(),
                        DATABASE_VALIDATION_PAGE_SIZE,
                    ])?,
                    None => statement.query(params![DATABASE_VALIDATION_PAGE_SIZE])?,
                };
                let mut keys = Vec::new();
                while let Some(row) = rows.next()? {
                    keys.push((
                        ExecId(array32(&row.get::<_, Vec<u8>>(0)?, "inbox execution")?),
                        InboxId::from_bytes(array32(&row.get::<_, Vec<u8>>(1)?, "inbox id")?),
                    ));
                }
                keys
            };
            let Some(last) = keys.last().copied() else {
                break;
            };
            let full_page = keys.len() == DATABASE_VALIDATION_PAGE_SIZE as usize;
            for (execution_id, inbox_id) in keys {
                let state = self
                    .load_execution(execution_id)?
                    .ok_or(StoreError::ExecutionNotFound(execution_id))?;
                let row = self.raw_inbox_row(execution_id, inbox_id)?.ok_or_else(|| {
                    StoreError::Corruption("inbox row disappeared while validating".into())
                })?;
                let source = peer_id_from_blob(&row.source, "inbox source")?;
                let digest = array32(&row.digest, "inbox digest")?;
                validate_inbox_frame(
                    &state,
                    inbox_id,
                    source,
                    &digest,
                    &row.frame,
                    "inbox frame identity mismatch",
                )?;
                match parse_inbox_status(&row.status)? {
                    InboxStatus::Accepted
                        if row.applied_version.is_some() || row.consumed_at_ms.is_some() =>
                    {
                        return Err(StoreError::Corruption(
                            "accepted inbox row has completion indexes".into(),
                        ));
                    }
                    InboxStatus::Applied
                        if row.applied_version.is_none() || row.consumed_at_ms.is_some() =>
                    {
                        return Err(StoreError::Corruption(
                            "applied inbox row has invalid indexes".into(),
                        ));
                    }
                    InboxStatus::Consumed
                        if row.consumed_at_ms.is_none() || row.applied_version.is_some() =>
                    {
                        return Err(StoreError::Corruption(
                            "consumed inbox row has invalid indexes".into(),
                        ));
                    }
                    _ => {}
                }
                if let Some(version) = row.applied_version
                    && sqlite_i64(version)? > state.version().get()
                {
                    return Err(StoreError::Corruption(
                        "inbox applied version is ahead of state".into(),
                    ));
                }
            }
            if !full_page {
                break;
            }
            after = Some(last);
        }
        Ok(())
    }
}

fn validate_inbox_frame(
    state: &ExecutionState,
    inbox_id: InboxId,
    source: PeerId,
    digest: &[u8],
    encoded: &[u8],
    identity_error: &'static str,
) -> Result<ValidatedInboxFrame, StoreError> {
    let stored: StoredFrame = decode_borsh(
        &open_envelope(EnvelopeKind::InboundFrame, encoded, MAX_FRAME_BYTES)?,
        "inbox frame",
    )?;
    let frame = decode_stored_frame(&stored)?;
    let authenticated = AuthenticatedFrame { source, frame };
    let canonical = canonical_frame(&authenticated, state)?;
    if canonical != stored {
        return Err(StoreError::Corruption(identity_error.into()));
    }
    let identity_digest = inbox_identity_digest(source, &stored)?;
    if identity_digest != *inbox_id.as_bytes() || identity_digest.as_slice() != digest {
        return Err(StoreError::Corruption(identity_error.into()));
    }
    Ok(ValidatedInboxFrame {
        source,
        stored,
        frame: authenticated.frame,
    })
}

fn ensure_step_commitment(
    state: &ExecutionState,
    commitment: &StepCommitment,
    inbox_id: InboxId,
) -> Result<(), StoreError> {
    if commitment.session_id != state.binding().session_id()
        || state
            .pending_shared()
            .is_none_or(|proposal| proposal.commitment() != commitment)
    {
        return Err(StoreError::InboxInputMismatch {
            inbox_id,
            part_index: 0,
        });
    }
    Ok(())
}

fn ensure_terminal_commitment(
    state: &ExecutionState,
    commitment: &TerminalCommitment,
    inbox_id: InboxId,
) -> Result<(), StoreError> {
    if state.pending_terminal() != Some(commitment) {
        return Err(StoreError::InboxInputMismatch {
            inbox_id,
            part_index: 0,
        });
    }
    Ok(())
}
