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
        let bytes = state_bytes(state)?;
        self.connection.execute(
            "INSERT INTO executions
             (execution_id, host_id, producer, session_id, state,
              local_state_checksum, version, lifecycle, agreed_step, event_position,
              created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
            params![
                state.execution_id().0.to_vec(),
                self.host_id.0.to_vec(),
                state.producer().0.to_vec(),
                state.binding().session_id().0.to_vec(),
                envelope(EnvelopeKind::ExecutionState, &bytes)?,
                checksum(state.local_state().as_bytes()).to_vec(),
                sqlite_u64(state.version().get())?,
                lifecycle_tag(state.lifecycle()),
                sqlite_u64(state.agreed_step())?,
                sqlite_u64(state.event_position())?,
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
        let Some(bytes) = self
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
        self.load_execution(ExecId(array32(&bytes, "execution id")?))
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
                        local_state_checksum, version, lifecycle, agreed_step,
                        event_position
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
            local_checksum,
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
            local_checksum,
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
        if local_checksum != checksum(state.local_state().as_bytes()).as_slice() {
            return Err(StoreError::Corruption(
                "local state checksum does not match execution state".into(),
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
        self.validate_agreed_rows(state)?;
        self.validate_timer_rows(state)?;
        self.validate_terminal_rows(state)?;
        Ok(())
    }

    pub(super) fn activate(
        &mut self,
        execution_id: ExecId,
        expected: ExecutionVersion,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        self.transaction(|store| store.activate_in_transaction(execution_id, expected, now_ms))
    }

    fn activate_in_transaction(
        &mut self,
        execution_id: ExecId,
        expected: ExecutionVersion,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        let state = self
            .load_execution_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
        if state.version() != expected {
            return self.version_mismatch(state, expected);
        }
        let mut next = state.clone();
        next.activate()?;
        self.persist_state_cas(&state, &next, now_ms)?;
        self.cache_pending(next.clone())?;
        Ok(ApplyOutcome::Committed {
            agreed_step: None,
            proposal_staged: false,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn commit_dispatch(
        &mut self,
        execution_id: ExecId,
        expected: ExecutionVersion,
        event: Event<Vec<u8>>,
        shared: SharedStateBytes,
        local: LocalStateBytes,
        effects: Vec<Effect>,
        terminal_outcome: Option<TerminalOutcome>,
        inbox_id: Option<InboxId>,
        timer_id: Option<TimerId>,
        pending_id: Option<PendingId>,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        self.transaction(|store| {
            store.commit_dispatch_in_transaction(
                execution_id,
                expected,
                event,
                shared,
                local,
                effects,
                terminal_outcome,
                inbox_id,
                timer_id,
                pending_id,
                now_ms,
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_dispatch_in_transaction(
        &mut self,
        execution_id: ExecId,
        expected: ExecutionVersion,
        event: Event<Vec<u8>>,
        shared: SharedStateBytes,
        local: LocalStateBytes,
        effects: Vec<Effect>,
        terminal_outcome: Option<TerminalOutcome>,
        inbox_id: Option<InboxId>,
        timer_id: Option<TimerId>,
        pending_id: Option<PendingId>,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        let state = self
            .load_execution_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
        if state.version() != expected {
            return self.version_mismatch(state, expected);
        }
        validate_effect_payloads(&effects)?;
        let post_state = StateHash::of_shared(&shared);
        validate_dispatch_sources(
            self, &state, &event, post_state, &effects, inbox_id, timer_id, pending_id,
        )?;
        let mut next = state.clone();
        let (establishing_frame, closes_pending) = next.apply_dispatch(
            &event,
            shared,
            local,
            &effects,
            terminal_outcome,
            pending_id,
        )?;
        if let Some(outcome) = self.inbox_replay_outcome(&state, inbox_id)? {
            return Ok(outcome);
        }

        let event_position = state.event_position();
        let event_payload = event_bytes(&event)?;
        let effects_payload = effects_bytes(&effects)?;
        let proposal_staged = next.pending_shared().is_some();
        let indexed_effects = (!proposal_staged)
            .then(|| indexed_effects(&effects))
            .transpose()?;

        self.persist_state_cas(&state, &next, now_ms)?;
        self.insert_event_record(
            execution_id,
            event_position,
            &event_payload,
            &effects_payload,
        )?;
        if let Some(indexed_effects) = indexed_effects.as_ref() {
            self.persist_effects(
                execution_id,
                event_position,
                next.version(),
                indexed_effects,
                now_ms,
                &state,
            )?;
        }
        if let Some((index, frame)) = establishing_frame {
            let frame_ordinal = message_frame_ordinal(index)?;
            self.persist_frame_for_remotes(
                execution_id,
                event_position,
                next.version(),
                frame_ordinal,
                &frame,
                &state,
                now_ms,
            )?;
        }
        if let Some(inbox_id) = inbox_id {
            if proposal_staged {
                self.mark_inbox_consumed(execution_id, inbox_id, now_ms)?;
            } else {
                self.mark_inbox_applied(execution_id, inbox_id, next.version())?;
            }
        }
        if closes_pending {
            self.acknowledge_pending_effect(
                execution_id,
                pending_id.expect("pending answer was validated"),
                &event,
            )?;
        }
        if let Some(timer_id) = timer_id {
            self.consume_timer(execution_id, timer_id)?;
        }
        self.cache_pending(next.clone())?;
        Ok(ApplyOutcome::Committed {
            agreed_step: None,
            proposal_staged,
        })
    }

    pub(super) fn commit_step_signature(
        &mut self,
        execution_id: ExecId,
        expected: ExecutionVersion,
        signature: ParticipantStepSignature,
        inbox_id: Option<InboxId>,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        self.transaction(|store| {
            store.commit_step_signature_in_transaction(
                execution_id,
                expected,
                signature,
                inbox_id,
                now_ms,
            )
        })
    }

    fn commit_step_signature_in_transaction(
        &mut self,
        execution_id: ExecId,
        expected: ExecutionVersion,
        signature: ParticipantStepSignature,
        inbox_id: Option<InboxId>,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        let state = self
            .load_execution_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
        if state.version() != expected {
            return self.version_mismatch(state, expected);
        }
        self.validate_step_inbox(&state, &signature, inbox_id)?;
        if let Some(outcome) = self.inbox_replay_outcome(&state, inbox_id)? {
            return Ok(outcome);
        }
        if state.pending_shared().is_some_and(|proposal| {
            proposal
                .signatures()
                .iter()
                .any(|existing| existing == &signature)
        }) {
            if let Some(inbox_id) = inbox_id {
                self.mark_inbox_consumed(execution_id, inbox_id, now_ms)?;
            }
            return Ok(ApplyOutcome::AlreadyApplied);
        }
        let proposal = state
            .pending_shared()
            .ok_or(StoreError::Protocol(ProtocolError::SharedProposalMissing))?;
        let event_position = proposal.event_position();
        let local_frame = inbox_id.is_none().then(|| ExecFrame::StepSignature {
            commitment: proposal.commitment().clone(),
            signature: signature.signature().sig,
        });
        let mut next = state.clone();
        let committed = next.add_step_signature(signature)?;
        if let Some(proposal) = committed.as_ref() {
            self.commit_staged_event(
                execution_id,
                proposal,
                next.pending_shared(),
                next.version(),
                now_ms,
                &state,
            )?;
        }
        let agreed_step = committed.as_ref().map(|proposal| proposal.entry().step);
        self.persist_signature(
            execution_id,
            &state,
            next,
            event_position,
            local_frame,
            inbox_id,
            now_ms,
            agreed_step,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn persist_signature(
        &mut self,
        execution_id: ExecId,
        state: &ExecutionState,
        next: ExecutionState,
        event_position: u64,
        local_frame: Option<ExecFrame>,
        inbox_id: Option<InboxId>,
        now_ms: u64,
        agreed_step: Option<u64>,
    ) -> Result<ApplyOutcome, StoreError> {
        self.persist_state_cas(state, &next, now_ms)?;
        if let Some(frame) = local_frame {
            self.persist_frame_for_remotes(
                execution_id,
                event_position,
                next.version(),
                0,
                &frame,
                state,
                now_ms,
            )?;
        }
        if !matches!(
            next.status().receipt_work(),
            arena0_protocol::ReceiptWork::NotTerminal
        ) {
            self.cancel_terminal_effects(execution_id)?;
        }
        if let Some(inbox_id) = inbox_id {
            self.mark_inbox_consumed(execution_id, inbox_id, now_ms)?;
        }
        let proposal_staged = next.pending_shared().is_some();
        self.cache_pending(next)?;
        Ok(ApplyOutcome::Committed {
            agreed_step,
            proposal_staged,
        })
    }

    pub(super) fn commit_terminal_signature(
        &mut self,
        execution_id: ExecId,
        expected: ExecutionVersion,
        signature: ParticipantTerminalSignature,
        inbox_id: Option<InboxId>,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        self.transaction(|store| {
            store.commit_terminal_signature_in_transaction(
                execution_id,
                expected,
                signature,
                inbox_id,
                now_ms,
            )
        })
    }

    fn commit_terminal_signature_in_transaction(
        &mut self,
        execution_id: ExecId,
        expected: ExecutionVersion,
        signature: ParticipantTerminalSignature,
        inbox_id: Option<InboxId>,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        let state = self
            .load_execution_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
        if state.version() != expected {
            return self.version_mismatch(state, expected);
        }
        self.validate_terminal_inbox(&state, &signature, inbox_id)?;
        if let Some(outcome) = self.inbox_replay_outcome(&state, inbox_id)? {
            return Ok(outcome);
        }
        if let ExecutionStatus::TerminalProof { proof } = state.status()
            && proof.pending_parts().is_some_and(|(_, _, signatures)| {
                signatures.iter().any(|existing| existing == &signature)
            })
        {
            if let Some(inbox_id) = inbox_id {
                self.mark_inbox_consumed(execution_id, inbox_id, now_ms)?;
            }
            return Ok(ApplyOutcome::AlreadyApplied);
        }
        let local_frame = inbox_id
            .is_none()
            .then(|| {
                state.pending_terminal().map(|commitment| ExecFrame::End {
                    commitment: commitment.clone(),
                    signature: signature.signature(),
                })
            })
            .flatten();
        let mut next = state.clone();
        next.add_terminal_signature(signature)?;
        self.persist_signature(
            execution_id,
            &state,
            next,
            state.event_position(),
            local_frame,
            inbox_id,
            now_ms,
            None,
        )
    }

    pub(super) fn stop_execution(
        &mut self,
        execution_id: ExecId,
        expected: ExecutionVersion,
        occurrence: AbortOccurrence,
        inbox_id: Option<InboxId>,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        self.transaction(|store| {
            store.stop_execution_in_transaction(
                execution_id,
                expected,
                occurrence,
                inbox_id,
                now_ms,
            )
        })
    }

    fn stop_execution_in_transaction(
        &mut self,
        execution_id: ExecId,
        expected: ExecutionVersion,
        occurrence: AbortOccurrence,
        inbox_id: Option<InboxId>,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        let state = self
            .load_execution_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
        if state.version() != expected {
            return self.version_mismatch(state, expected);
        }
        self.validate_abort_inbox(&state, &occurrence, inbox_id)?;
        if let Some(outcome) = self.inbox_replay_outcome(&state, inbox_id)? {
            return Ok(outcome);
        }
        if state.status().terminal_cause().is_some_and(|cause| {
            matches!(
                cause,
                arena0_protocol::StopCause::Authenticated(existing) if existing == &occurrence
            )
        }) {
            if let Some(inbox_id) = inbox_id {
                self.mark_inbox_consumed(execution_id, inbox_id, now_ms)?;
            }
            return Ok(ApplyOutcome::AlreadyApplied);
        }
        let mut next = state.clone();
        let pending_proposal = state.pending_shared().cloned();
        next.stop(occurrence)?;
        if let Some(proposal) = pending_proposal.as_ref() {
            self.cancel_proposal_frames(execution_id, proposal)?;
        }
        self.persist_state_cas(&state, &next, now_ms)?;
        if inbox_id.is_none() {
            let frame = ExecFrame::Abort {
                occurrence: match next.status().terminal_cause() {
                    Some(arena0_protocol::StopCause::Authenticated(occurrence)) => {
                        occurrence.clone()
                    }
                    _ => {
                        return Err(StoreError::Corruption(
                            "stopped state lost its authenticated occurrence".into(),
                        ));
                    }
                },
            };
            self.persist_frame_for_remotes(
                execution_id,
                state.event_position(),
                next.version(),
                0,
                &frame,
                &state,
                now_ms,
            )?;
        }
        self.cancel_terminal_effects(execution_id)?;
        if let Some(inbox_id) = inbox_id {
            self.mark_inbox_consumed(execution_id, inbox_id, now_ms)?;
        }
        self.cache_pending(next.clone())?;
        Ok(ApplyOutcome::Committed {
            agreed_step: None,
            proposal_staged: false,
        })
    }

    pub(super) fn interrupt_terminal(
        &mut self,
        execution_id: ExecId,
        expected: ExecutionVersion,
        reason: String,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        self.transaction(|store| {
            store.interrupt_terminal_in_transaction(execution_id, expected, reason, now_ms)
        })
    }

    fn interrupt_terminal_in_transaction(
        &mut self,
        execution_id: ExecId,
        expected: ExecutionVersion,
        reason: String,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        let state = self
            .load_execution_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
        if state.version() != expected {
            return self.version_mismatch(state, expected);
        }

        // The protocol mutator both checks that a proof is in progress and
        // preserves that proof in the resulting Incomplete status. Timer
        // rows are store-owned, so clear every active timer in this same
        // transaction before the state CAS becomes visible.
        let mut next = state.clone();
        next.interrupt_terminal(reason)?;
        self.cancel_active_timers(execution_id)?;
        self.persist_state_cas(&state, &next, now_ms)?;
        self.cancel_terminal_effects(execution_id)?;
        self.cache_pending(next.clone())?;
        Ok(ApplyOutcome::Committed {
            agreed_step: None,
            proposal_staged: false,
        })
    }

    pub(super) fn publish_terminal(
        &mut self,
        execution_id: ExecId,
        expected: ExecutionVersion,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        self.transaction(|store| {
            store.publish_terminal_in_transaction(execution_id, expected, now_ms)
        })
    }

    fn publish_terminal_in_transaction(
        &mut self,
        execution_id: ExecId,
        expected: ExecutionVersion,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        let state = self
            .load_execution_in_transaction(execution_id)?
            .ok_or(StoreError::ExecutionNotFound(execution_id))?;
        if state.version() != expected {
            return self.version_mismatch(state, expected);
        }
        if state.published_receipt_id().is_some() {
            return Ok(ApplyOutcome::AlreadyApplied);
        }
        let artifact = self.assemble_terminal_artifact(&state)?;
        let mut next = state.clone();
        next.publish_receipt(artifact.clone())?;
        self.persist_state_cas(&state, &next, now_ms)?;
        self.persist_terminal_publication(execution_id, next.version(), &artifact, now_ms)?;
        self.cancel_terminal_effects(execution_id)?;
        self.cache_pending(next.clone())?;
        Ok(ApplyOutcome::Committed {
            agreed_step: None,
            proposal_staged: false,
        })
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
            let certificate = state
                .terminal_certificate()
                .ok_or(StoreError::Protocol(ProtocolError::TerminalProofMissing))?;
            arena0_protocol::ReceiptTermination::Completed {
                terminal: arena0_protocol::SessionTerminal {
                    final_step: certificate.commitment().final_step,
                    final_state: certificate.commitment().final_state,
                    outcome_hash: certificate.commitment().outcome_hash,
                    agreement: certificate.agreement().clone(),
                },
            }
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
        let trace = self.load_agreed_trace_in_transaction(&state)?;
        let end = to.min(trace.len() as u64);
        let start = usize::try_from(from.min(end))
            .map_err(|_| StoreError::Corruption("trace start overflows usize".into()))?;
        let end = usize::try_from(end)
            .map_err(|_| StoreError::Corruption("trace end overflows usize".into()))?;
        Ok(trace[start..end].to_vec())
    }

    pub(super) fn read_event_summaries(
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
        let mut participants = state
            .binding()
            .activation()
            .tickets()
            .iter()
            .filter_map(|ticket| match &ticket.data.action {
                arena0_protocol::TicketAction::Active { execution_bls, .. } => {
                    Some((ticket.data.signer, *execution_bls))
                }
                arena0_protocol::TicketAction::Withdrawn => None,
            })
            .collect::<Vec<_>>();
        participants.sort_by_key(|(peer, _)| *peer);
        if participants.len() != state.binding().activation().tickets().len() {
            return Err(StoreError::Corruption(
                "activation contains a non-active participant ticket".into(),
            ));
        }
        let keys = participants.iter().map(|(_, key)| *key).collect::<Vec<_>>();
        let mut trace = Vec::with_capacity(raw.len());
        let mut cursor_state = state.binding().activation().offer().data().initial_state;
        let mut cursor_link = arena0_protocol::CHAIN_START;
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
            if entry.pre_state != cursor_state {
                return Err(StoreError::Corruption(
                    "agreed trace pre-state does not match cursor".into(),
                ));
            }
            let commitment =
                StepCommitment::for_entry(state.binding().session_id(), &entry, cursor_link);
            if commitment.post_state != entry.post_state
                || !entry.agreement.signers.is_full(keys.len())
            {
                return Err(StoreError::Corruption(
                    "agreed trace commitment is inconsistent".into(),
                ));
            }
            entry
                .agreement
                .verify_signatures(step, &commitment.signing_bytes(), &keys)
                .map_err(|error| StoreError::Corruption(format!("agreed signature: {error}")))?;
            cursor_state = entry.post_state;
            cursor_link = commitment.link_hash();
            trace.push(entry);
        }
        if trace.len() as u64 != state.agreed_step()
            || cursor_state != state.agreed_state()
            || cursor_link != state.agreed_link()
        {
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
        successor: Option<&arena0_protocol::SharedProposal>,
        version: ExecutionVersion,
        now_ms: u64,
        state: &ExecutionState,
    ) -> Result<(), StoreError> {
        let release_effects = proposal
            .effects()
            .iter()
            .filter(|(_, effect)| !matches!(effect, Effect::Broadcast { .. }))
            .cloned()
            .collect::<Vec<_>>();
        let deferred_broadcast =
            proposal
                .effects()
                .iter()
                .find_map(|(ordinal, effect)| match effect {
                    Effect::Broadcast { data } => Some((*ordinal, data.clone())),
                    _ => None,
                });
        if successor.is_some() != deferred_broadcast.is_some() {
            return Err(StoreError::Corruption(
                "deferred broadcast and successor proposal disagree".into(),
            ));
        }
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
            state,
        )?;
        if let (Some(successor), Some((ordinal, _))) = (successor, deferred_broadcast) {
            let frame = successor_broadcast_frame(successor)?;
            let frame_ordinal = message_frame_ordinal(ordinal)?;
            self.persist_frame_for_remotes(
                execution_id,
                proposal.event_position(),
                version,
                frame_ordinal,
                &frame,
                state,
                now_ms,
            )?;
        }
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

    fn persist_state_cas(
        &mut self,
        current: &ExecutionState,
        next: &ExecutionState,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let bytes = state_bytes(next)?;
        let changed = self.connection.execute(
            "UPDATE executions SET state = ?1, local_state_checksum = ?2,
                    version = ?3, lifecycle = ?4, agreed_step = ?5,
                    event_position = ?6, updated_at_ms = ?7
             WHERE execution_id = ?8 AND version = ?9",
            params![
                envelope(EnvelopeKind::ExecutionState, &bytes)?,
                checksum(next.local_state().as_bytes()).to_vec(),
                sqlite_u64(next.version().get())?,
                lifecycle_tag(next.lifecycle()),
                sqlite_u64(next.agreed_step())?,
                sqlite_u64(next.event_position())?,
                sqlite_u64(now_ms)?,
                current.execution_id().0.to_vec(),
                sqlite_u64(current.version().get())?,
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::Corruption(
                "execution compare-and-set changed no row".into(),
            ));
        }
        Ok(())
    }

    fn cache_pending(&mut self, state: ExecutionState) -> Result<(), StoreError> {
        self.pending_execution = Some(PendingExecution {
            encoded_bytes: state_bytes(&state)?.len(),
            state,
        });
        Ok(())
    }

    fn version_mismatch(
        &mut self,
        state: ExecutionState,
        expected: ExecutionVersion,
    ) -> Result<ApplyOutcome, StoreError> {
        Ok(ApplyOutcome::VersionMismatch {
            expected,
            actual: state.version(),
        })
    }

    fn inbox_replay_outcome(
        &mut self,
        state: &ExecutionState,
        inbox_id: Option<InboxId>,
    ) -> Result<Option<ApplyOutcome>, StoreError> {
        let Some(inbox_id) = inbox_id else {
            return Ok(None);
        };
        let (_, _, status, version) =
            self.load_inbox_fact(state.execution_id(), inbox_id, state)?;
        match status {
            InboxStatus::Accepted => Ok(None),
            InboxStatus::Applied => Ok(Some(ApplyOutcome::InboxAlreadyApplied {
                inbox_id,
                version: version.ok_or_else(|| {
                    StoreError::Corruption("applied inbox row has no version".into())
                })?,
            })),
            InboxStatus::Consumed => Ok(Some(ApplyOutcome::InboxAlreadyConsumed { inbox_id })),
        }
    }

    fn validate_step_inbox(
        &mut self,
        state: &ExecutionState,
        signature: &ParticipantStepSignature,
        inbox_id: Option<InboxId>,
    ) -> Result<(), StoreError> {
        let Some(inbox_id) = inbox_id else {
            if signature.participant() != self.host_id || state.producer() != self.host_id {
                return Err(StoreError::UnauthenticatedSource(
                    "local step evidence is not authored by this Host".into(),
                ));
            }
            return Ok(());
        };
        let (source, stored, status, _) =
            self.load_inbox_fact(state.execution_id(), inbox_id, state)?;
        let ExecFrame::StepSignature {
            commitment,
            signature: frame_signature,
        } = decode_stored_frame(&stored)?
        else {
            return Err(StoreError::InboxInputMismatch {
                inbox_id,
                part_index: 0,
            });
        };
        let pending_matches = state
            .pending_shared()
            .is_some_and(|proposal| proposal.commitment() == &commitment);
        if source != signature.participant()
            || frame_signature != signature.signature().sig
            || signature.signature().step != commitment.step
            || (status == InboxStatus::Accepted && !pending_matches)
        {
            return Err(StoreError::InboxInputMismatch {
                inbox_id,
                part_index: 0,
            });
        }
        Ok(())
    }

    fn validate_terminal_inbox(
        &mut self,
        state: &ExecutionState,
        signature: &ParticipantTerminalSignature,
        inbox_id: Option<InboxId>,
    ) -> Result<(), StoreError> {
        let Some(inbox_id) = inbox_id else {
            if signature.participant() != self.host_id || state.producer() != self.host_id {
                return Err(StoreError::UnauthenticatedSource(
                    "local terminal evidence is not authored by this Host".into(),
                ));
            }
            return Ok(());
        };
        let (source, stored, status, _) =
            self.load_inbox_fact(state.execution_id(), inbox_id, state)?;
        let ExecFrame::End {
            commitment,
            signature: frame_signature,
        } = decode_stored_frame(&stored)?
        else {
            return Err(StoreError::InboxInputMismatch {
                inbox_id,
                part_index: 0,
            });
        };
        if source != signature.participant()
            || frame_signature != signature.signature()
            || (status == InboxStatus::Accepted && state.pending_terminal() != Some(&commitment))
        {
            return Err(StoreError::InboxInputMismatch {
                inbox_id,
                part_index: 0,
            });
        }
        Ok(())
    }

    fn validate_abort_inbox(
        &mut self,
        state: &ExecutionState,
        occurrence: &AbortOccurrence,
        inbox_id: Option<InboxId>,
    ) -> Result<(), StoreError> {
        let Some(inbox_id) = inbox_id else {
            if occurrence.sender() != self.host_id || state.producer() != self.host_id {
                return Err(StoreError::UnauthenticatedSource(
                    "local abort evidence is not authored by this Host".into(),
                ));
            }
            return Ok(());
        };
        let (source, stored, _status, _) =
            self.load_inbox_fact(state.execution_id(), inbox_id, state)?;
        let ExecFrame::Abort {
            occurrence: stored_occurrence,
        } = decode_stored_frame(&stored)?
        else {
            return Err(StoreError::InboxInputMismatch {
                inbox_id,
                part_index: 0,
            });
        };
        if source != occurrence.sender() || stored_occurrence != *occurrence {
            return Err(StoreError::InboxInputMismatch {
                inbox_id,
                part_index: 0,
            });
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
            validate_effect_payloads(&effects)?;
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

    pub(super) fn validate_agreed_rows(
        &mut self,
        state: &ExecutionState,
    ) -> Result<(), StoreError> {
        let _ = self.load_agreed_trace_in_transaction(state)?;
        Ok(())
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

    fn validate_timer_source(
        &mut self,
        execution_id: ExecId,
        timer_id: TimerId,
        event: &Event<Vec<u8>>,
    ) -> Result<(), StoreError> {
        let payload: Option<Vec<u8>> = self
            .connection
            .query_row(
                "SELECT payload FROM active_timers WHERE execution_id = ?1 AND timer_id = ?2",
                params![execution_id.0.to_vec(), timer_id.as_bytes().to_vec()],
                |row| row.get(0),
            )
            .optional()?;
        let payload =
            payload.ok_or_else(|| StoreError::Corruption("timer source is not active".into()))?;
        let payload = open_envelope(EnvelopeKind::Timer, &payload, MAX_TIMER_RECORD_BYTES)?;
        let timer: TimerPayload = decode_borsh(&payload, "timer payload")?;
        match event {
            Event::TimerFired { timer: expected } if timer == *expected => Ok(()),
            _ => Err(StoreError::Corruption(
                "timer event does not match active timer".into(),
            )),
        }
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

    /// Cancel all active timers while the execution is being frozen. Timer
    /// rows are the scheduling authority, so this deletion must share the
    /// terminal state CAS transaction; otherwise a restart could enqueue work
    /// for an execution whose proof is already incomplete.
    fn cancel_active_timers(&mut self, execution_id: ExecId) -> Result<(), StoreError> {
        self.connection.execute(
            "DELETE FROM active_timers WHERE execution_id = ?1",
            params![execution_id.0.to_vec()],
        )?;
        Ok(())
    }

    /// Close the exact durable request that supplied a continuation answer.
    /// The row is identified by the protocol's `(execution,event,ordinal)`
    /// pending identity, not by the current event position or by whichever
    /// request happens to be first in the outbox. Clearing a live lease here
    /// makes the dispatch transaction the acknowledgement point; a later
    /// explicit delivery acknowledgement is consequently idempotent.
    fn acknowledge_pending_effect(
        &mut self,
        execution_id: ExecId,
        pending: PendingId,
        event: &Event<Vec<u8>>,
    ) -> Result<(), StoreError> {
        let row = self.pending_effect_row(execution_id, pending)?;
        let operation_matches = match (event, &row.effect) {
            (
                Event::InputReceived { callout_index, .. },
                Effect::Callout {
                    callout_index: effect_index,
                    ..
                },
            ) => callout_index == effect_index,
            (Event::Signed { .. }, Effect::Sign { .. }) => true,
            _ => false,
        };
        if !operation_matches {
            return Err(StoreError::Protocol(
                ProtocolError::PendingContinuationMismatch,
            ));
        }
        if row.status == OutboxStatus::Acknowledged {
            // The exact request may already have been acknowledged by the
            // delivery worker. The answer still consumes every retry marker
            // emitted while that same continuation remained pending.
            self.cancel_retry_effects(execution_id)?;
            return Ok(());
        }
        self.acknowledge_effect_rows(execution_id, &[row.outbox_id])?;
        // RetryInput is deliberately not a second continuation. It is a
        // durable request to redeliver the one existing continuation, so the
        // successful answer retires all of its pending/leased retry rows in
        // this same transaction.
        self.cancel_retry_effects(execution_id)?;
        Ok(())
    }
}

fn validate_effect_payloads(effects: &[Effect]) -> Result<(), StoreError> {
    if effects.len() > arena0_protocol::MAX_EFFECTS {
        return Err(StoreError::CommandTooLarge {
            required: effects.len(),
            capacity: arena0_protocol::MAX_EFFECTS,
        });
    }
    let bytes = effects_bytes(effects)?;
    if bytes.len() > arena0_protocol::MAX_RECEIPT_BYTES {
        return Err(StoreError::CommandTooLarge {
            required: bytes.len(),
            capacity: arena0_protocol::MAX_RECEIPT_BYTES,
        });
    }
    Ok(())
}

fn indexed_effects(effects: &[Effect]) -> Result<Vec<(u32, Effect)>, StoreError> {
    effects
        .iter()
        .enumerate()
        .map(|(index, effect)| {
            Ok((
                u32::try_from(index).map_err(|_| StoreError::CommandTooLarge {
                    required: index,
                    capacity: u32::MAX as usize,
                })?,
                effect.clone(),
            ))
        })
        .collect()
}

fn message_frame_ordinal(effect_ordinal: u32) -> Result<u32, StoreError> {
    effect_ordinal
        .checked_add(1)
        .ok_or_else(|| StoreError::Corruption("broadcast frame ordinal exhausted".into()))
}

fn successor_broadcast_frame(
    proposal: &arena0_protocol::SharedProposal,
) -> Result<ExecFrame, StoreError> {
    let Event::MessageReceived {
        message_id,
        position,
        pre_state,
        msg,
        ..
    } = &proposal.entry().event
    else {
        return Err(StoreError::Corruption(
            "deferred broadcast successor is not a message event".into(),
        ));
    };
    Ok(ExecFrame::Message {
        message_id: *message_id,
        seq: *position,
        prestate: *pre_state,
        poststate: proposal.entry().post_state,
        data: msg.clone(),
    })
}

#[allow(clippy::too_many_arguments)]
fn validate_dispatch_sources(
    database: &mut Database,
    state: &ExecutionState,
    event: &Event<Vec<u8>>,
    post_state: StateHash,
    effects: &[Effect],
    inbox_id: Option<InboxId>,
    timer_id: Option<TimerId>,
    pending_id: Option<PendingId>,
) -> Result<(), StoreError> {
    let inbox_applicable = matches!(event, Event::MessageReceived { .. });
    let timer_applicable = matches!(event, Event::TimerFired { .. });
    let pending_applicable = matches!(event, Event::InputReceived { .. } | Event::Signed { .. })
        || effects
            .iter()
            .any(|effect| matches!(effect, Effect::RetryInput { .. }));
    if let Some(inbox_id) = inbox_id
        && !inbox_applicable
    {
        return Err(StoreError::InboxInputMismatch {
            inbox_id,
            part_index: 0,
        });
    }
    if timer_id.is_some() && !timer_applicable {
        return Err(StoreError::Corruption(
            "timer id supplied for a non-timer event".into(),
        ));
    }
    if pending_id.is_some() && !pending_applicable {
        return Err(StoreError::Corruption(
            "pending id supplied for a non-continuation event".into(),
        ));
    }
    let answer = matches!(event, Event::InputReceived { .. } | Event::Signed { .. });
    if (answer || pending_id.is_some())
        && pending_id != state.status().pending().map(|pending| pending.id)
    {
        return Err(StoreError::Corruption(
            "continuation pending id does not match status".into(),
        ));
    }
    if let Some(timer_id) = timer_id {
        database.validate_timer_source(state.execution_id(), timer_id, event)?;
    }
    if let Some(inbox_id) = inbox_id {
        let (source, stored, status, _) =
            database.load_inbox_fact(state.execution_id(), inbox_id, state)?;
        if status == InboxStatus::Accepted {
            let ExecFrame::Message {
                message_id,
                seq,
                prestate,
                poststate: frame_poststate,
                data,
            } = decode_stored_frame(&stored)?
            else {
                return Err(StoreError::InboxNotMessage(inbox_id));
            };
            let Event::MessageReceived {
                message_id: event_id,
                from,
                position,
                pre_state,
                msg,
            } = event
            else {
                unreachable!();
            };
            if source != *from
                || message_id != *event_id
                || seq != *position
                || prestate != *pre_state
                || frame_poststate != post_state
                || data != *msg
            {
                return Err(StoreError::InboxInputMismatch {
                    inbox_id,
                    part_index: 0,
                });
            }
        }
    }
    Ok(())
}

fn dispatch_digest(event: &[u8], effects: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"arena0/store/event/v2");
    hasher.update(event);
    hasher.update(effects);
    *hasher.finalize().as_bytes()
}
