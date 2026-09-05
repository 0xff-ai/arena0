use super::*;
use crate::lock::restrict_database_companions;

mod activation;
mod execution;
mod inbox;
mod integrity;
mod outbox;
mod receipt;
mod recovery;
mod registry;
mod request;

pub(super) fn owner_loop(
    config: StoreConfig,
    lock: OwnerLock,
    mut receiver: mpsc::Receiver<QueuedCommand>,
    ready: std_mpsc::SyncSender<Result<RecoveryReport, StoreError>>,
) {
    let mut database = match Database::open(&config, lock) {
        Ok(database) => database,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let now_ms = match unix_time_ms() {
        Ok(now_ms) => now_ms,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let recovery = match database.recover_all_expired_leases(now_ms) {
        Ok(recovery) => recovery,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    if ready.send(Ok(recovery)).is_err() {
        return;
    }

    while let Some(queued) = receiver.blocking_recv() {
        let QueuedCommand { command, _budget } = queued;
        let shutdown = matches!(command, Command::Shutdown { .. });
        dispatch_command(&mut database, command);
        // Dropping `queued` here releases its byte reservation only after the
        // typed command has been removed from the owner queue.
        drop(_budget);
        if shutdown {
            break;
        }
        if database.is_poisoned() {
            break;
        }
    }
}

fn dispatch_command(database: &mut Database, command: Command) {
    match command {
        Command::CreateExecutionRequest {
            execution_id,
            program_hash,
            params,
            admission,
            created_at_ms,
            reply,
        } => {
            let _ = reply.send(database.create_execution_request(
                execution_id,
                program_hash,
                params.map(JsonBytes::into_bytes),
                admission,
                created_at_ms,
            ));
        }
        Command::LoadExecutionRequest {
            execution_id,
            reply,
        } => {
            let _ = reply.send(database.load_execution_request(execution_id));
        }
        Command::ListExecutionRequests { limit, reply } => {
            let _ = reply.send(database.list_execution_requests(limit));
        }
        Command::ListRecoveryCandidates {
            cursor,
            limit,
            reply,
        } => {
            let _ = reply.send(database.list_recovery_candidates(cursor, limit));
        }
        Command::RecordExecutionRequestFailure {
            execution_id,
            reason,
            reply,
        } => {
            let _ = reply.send(database.record_execution_request_failure(execution_id, reason));
        }
        Command::LoadOrCreateExecutionSalt {
            execution_id,
            now_ms,
            reply,
        } => {
            let _ = reply.send(database.load_or_create_execution_salt(execution_id, now_ms));
        }
        Command::RegisterProgram {
            hash,
            wasm,
            now_ms,
            reply,
        } => {
            let _ = reply.send(database.register_program(hash, wasm, now_ms));
        }
        Command::LoadProgram { hash, reply } => {
            let _ = reply.send(database.load_program(hash));
        }
        Command::ListPrograms { limit, reply } => {
            let _ = reply.send(database.list_programs(limit));
        }
        Command::RemoveProgram {
            hash,
            now_ms,
            reply,
        } => {
            let _ = reply.send(database.remove_program(hash, now_ms));
        }
        Command::PrepareActivation {
            execution_id,
            prepared,
            now_ms,
            reply,
        } => {
            let _ = reply.send(database.prepare_activation(execution_id, prepared, now_ms));
        }
        Command::CommitActivation {
            execution_id,
            activation,
            now_ms,
            reply,
        } => {
            let _ = reply.send(database.commit_activation(execution_id, activation, now_ms));
        }
        Command::LoadActivation {
            execution_id,
            reply,
        } => {
            let _ = reply.send(database.load_activation(execution_id));
        }
        Command::CreateExecution {
            execution_id,
            activation,
            producer,
            shared_state,
            local_state,
            now_ms,
            reply,
        } => {
            let _ = reply.send(database.create_execution(
                execution_id,
                activation,
                producer,
                shared_state,
                local_state,
                now_ms,
            ));
        }
        Command::LoadExecution {
            execution_id,
            reply,
        } => {
            let _ = reply.send(database.load_execution(execution_id));
        }
        Command::LoadExecutionBySession { session_id, reply } => {
            let _ = reply.send(database.load_execution_by_session(session_id));
        }
        Command::ListActivations { limit, reply } => {
            let _ = reply.send(database.list_activations(limit));
        }
        Command::ListExecutions { limit, reply } => {
            let _ = reply.send(database.list_executions(limit));
        }
        Command::ApplyInput {
            execution_id,
            input,
            inbox,
            now_ms,
            reply,
        } => {
            let _ = reply.send(database.apply_input(execution_id, *input, inbox, now_ms));
        }
        Command::AcceptInbound {
            execution_id,
            frame,
            now_ms,
            reply,
        } => {
            let _ = reply.send(database.accept_inbound(execution_id, *frame, now_ms));
        }
        Command::ListPendingInbox {
            execution_id,
            limit,
            reply,
        } => {
            let _ = reply.send(database.list_pending_inbox(execution_id, limit));
        }
        Command::ListPendingRequests {
            execution_id,
            reply,
        } => {
            let _ = reply.send(database.list_pending_requests(execution_id));
        }
        Command::ReadTrace {
            execution_id,
            from,
            to,
            reply,
        } => {
            let _ = reply.send(database.read_trace(execution_id, from, to));
        }
        Command::ReadPrivateSummaries {
            execution_id,
            from,
            limit,
            reply,
        } => {
            let _ = reply.send(database.read_private_summaries(execution_id, from, limit));
        }
        Command::ApplyInbound {
            execution_id,
            inbox_id,
            now_ms,
            reply,
        } => {
            let _ = reply.send(database.apply_inbound(execution_id, inbox_id, now_ms));
        }
        Command::ApplyInboundMessage {
            execution_id,
            inbox_id,
            delta,
            now_ms,
            reply,
        } => {
            let _ =
                reply.send(database.apply_inbound_message(execution_id, inbox_id, *delta, now_ms));
        }
        Command::RejectInbound {
            execution_id,
            inbox_id,
            now_ms,
            reply,
        } => {
            let _ = reply.send(database.reject_inbound(execution_id, inbox_id, now_ms));
        }
        Command::LeaseOutbox {
            execution_id,
            now_ms,
            reply,
        } => {
            let _ = reply.send(database.lease_next_outbox(execution_id, now_ms));
        }
        Command::AcknowledgeOutbox {
            execution_id,
            outbox_id,
            lease_id,
            reply,
        } => {
            let _ = reply.send(database.acknowledge_outbox(execution_id, outbox_id, lease_id));
        }
        Command::RetryOutbox {
            execution_id,
            outbox_id,
            lease_id,
            now_ms,
            reason,
            reply,
        } => {
            let _ = reply.send(database.retry_outbox(
                execution_id,
                outbox_id,
                lease_id,
                now_ms,
                reason,
            ));
        }
        Command::RecoverExpiredLeases {
            execution_id,
            now_ms,
            reply,
        } => {
            let _ = reply.send(database.recover_expired_leases(execution_id, now_ms));
        }
        Command::DueTimers {
            execution_id,
            now_ms,
            limit,
            reply,
        } => {
            let _ = reply.send(database.due_timers(execution_id, now_ms, limit));
        }
        Command::AssembleReceipt {
            execution_id,
            now_ms,
            reply,
        } => {
            let _ = reply.send(database.assemble_receipt(execution_id, now_ms));
        }
        Command::ImportReceipt {
            receipt,
            now_ms,
            reply,
        } => {
            let _ = reply.send(database.import_receipt(*receipt, now_ms));
        }
        Command::LoadReceipt { session_id, reply } => {
            let _ = reply.send(database.load_receipt(session_id));
        }
        Command::LoadReceiptById { receipt_id, reply } => {
            let _ = reply.send(database.load_receipt_by_id(receipt_id));
        }
        Command::ListReceipts { limit, reply } => {
            let _ = reply.send(database.list_receipts(limit));
        }
        Command::Shutdown { reply } => {
            let _ = reply.send(Ok(()));
        }
    }
}

struct Database {
    connection: Connection,
    _lock: OwnerLock,
    host_id: PeerId,
    lease_duration_ms: u64,
    retry_delay_ms: u64,
    executions: ExecutionWorkingSet,
    pending_execution: Option<PendingExecution>,
    transaction_poison: Option<String>,
}

struct ExecutionIndexRow {
    execution_id: ExecId,
    state_bytes: Vec<u8>,
    local_checksum: Vec<u8>,
    version: i64,
    lifecycle: i64,
    public_step: i64,
    private_next_record: i64,
    producer: PeerId,
    session_id: SessionHash,
}

const DATABASE_VALIDATION_PAGE_SIZE: i64 = 64;

struct WorkingExecution {
    state: ExecutionState,
    encoded_bytes: usize,
}

struct PendingExecution {
    state: ExecutionState,
    encoded_bytes: usize,
}

struct ExecutionWorkingSet {
    entries: HashMap<ExecId, WorkingExecution>,
    lru: VecDeque<ExecId>,
    encoded_bytes: usize,
}

impl ExecutionWorkingSet {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            lru: VecDeque::new(),
            encoded_bytes: 0,
        }
    }

    fn get(&mut self, execution_id: ExecId) -> Option<ExecutionState> {
        let state = self.entries.get(&execution_id)?.state.clone();
        self.touch(execution_id);
        Some(state)
    }

    fn insert(&mut self, state: ExecutionState, encoded_bytes: usize) {
        let execution_id = state.execution_id();
        if let Some(previous) = self.entries.remove(&execution_id) {
            self.encoded_bytes = self.encoded_bytes.saturating_sub(previous.encoded_bytes);
            self.lru.retain(|candidate| *candidate != execution_id);
        }
        while self.encoded_bytes.saturating_add(encoded_bytes) > EXECUTION_WORKING_SET_BYTES {
            let Some(evicted) = self.lru.pop_front() else {
                break;
            };
            if let Some(previous) = self.entries.remove(&evicted) {
                self.encoded_bytes = self.encoded_bytes.saturating_sub(previous.encoded_bytes);
            }
        }
        if encoded_bytes <= EXECUTION_WORKING_SET_BYTES {
            self.encoded_bytes = self.encoded_bytes.saturating_add(encoded_bytes);
            self.entries.insert(
                execution_id,
                WorkingExecution {
                    state,
                    encoded_bytes,
                },
            );
            self.lru.push_back(execution_id);
        }
    }

    fn touch(&mut self, execution_id: ExecId) {
        self.lru.retain(|candidate| *candidate != execution_id);
        self.lru.push_back(execution_id);
    }
}

#[derive(Debug, Clone, Copy)]
enum ActivationConflictKind {
    Prepared,
    Committed,
}

impl Database {
    fn open(config: &StoreConfig, lock: OwnerLock) -> Result<Self, StoreError> {
        prepare_database_file(&config.path)?;
        let connection = Connection::open(&config.path)?;
        configure_connection(&connection, config.busy_timeout)?;
        initialize_schema(&connection)?;
        let mut database = Self {
            connection,
            _lock: lock,
            host_id: config.host_id,
            lease_duration_ms: config.lease_duration_ms,
            retry_delay_ms: config.retry_delay_ms,
            executions: ExecutionWorkingSet::new(),
            pending_execution: None,
            transaction_poison: None,
        };
        database.bind_metadata()?;
        database.validate_database()?;
        restrict_database_companions(&config.path)?;
        Ok(database)
    }

    fn bind_metadata(&mut self) -> Result<(), StoreError> {
        let stored_host = self
            .connection
            .query_row("SELECT value FROM meta WHERE key = 'host_id'", [], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .optional()?;
        match stored_host {
            Some(bytes) => {
                let stored = peer_id_from_blob(&bytes, "host_id")?;
                if stored != self.host_id {
                    return Err(StoreError::IdentityMismatch {
                        database: stored,
                        requested: self.host_id,
                    });
                }
            }
            None => {
                self.connection.execute(
                    "INSERT INTO meta (key, value) VALUES ('host_id', ?1)",
                    params![self.host_id.0.to_vec()],
                )?;
            }
        }
        Ok(())
    }

    fn validate_database(&mut self) -> Result<(), StoreError> {
        let mut after = None;
        loop {
            let after_bytes = after.map(|execution_id: ExecId| execution_id.0.to_vec());
            let execution_ids = {
                let mut statement = self.connection.prepare(
                    "SELECT execution_id FROM executions
                     WHERE (?1 IS NULL OR execution_id > ?1)
                     ORDER BY execution_id LIMIT ?2",
                )?;
                let mut rows =
                    statement.query(params![after_bytes, DATABASE_VALIDATION_PAGE_SIZE])?;
                let mut execution_ids = Vec::new();
                while let Some(row) = rows.next()? {
                    execution_ids
                        .push(ExecId(array32(&row.get::<_, Vec<u8>>(0)?, "execution id")?));
                }
                execution_ids
            };
            let Some(last) = execution_ids.last().copied() else {
                break;
            };
            after = Some(last);
            for execution_id in execution_ids {
                let state = self
                    .load_execution_from_sqlite(execution_id)?
                    .ok_or_else(|| {
                        StoreError::Corruption("execution disappeared during validation".into())
                    })?;
                self.validate_projection_rows(&state)?;
                let encoded_bytes = state_bytes(&state)?.len();
                self.executions.insert(state, encoded_bytes);
            }
        }
        self.validate_execution_requests()?;
        self.validate_activation_conflicts()?;
        self.validate_execution_salts()?;
        self.validate_programs()?;
        self.validate_occurrences()?;
        self.validate_receipts()?;
        self.validate_outbox_rows()?;
        self.validate_inbox_rows()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::activation_fixture;

    fn execution_state(byte: u8) -> ExecutionState {
        let fixture = activation_fixture();
        ExecutionState::new(
            ExecId([byte; 32]),
            fixture.activation,
            fixture.producer,
            SharedStateBytes::try_new(vec![0]).expect("shared state"),
            LocalStateBytes::try_new(Vec::new()).expect("local state"),
        )
        .expect("execution state")
    }

    #[test]
    fn execution_working_set_is_byte_bounded_and_lru() {
        let first = execution_state(1);
        let second = execution_state(2);
        let third = execution_state(3);
        let first_id = first.execution_id();
        let second_id = second.execution_id();
        let third_id = third.execution_id();
        let mut working_set = ExecutionWorkingSet::new();

        working_set.insert(first, 20 * 1024 * 1024);
        working_set.insert(second, 20 * 1024 * 1024);
        assert!(working_set.get(first_id).is_some());
        working_set.insert(third, 30 * 1024 * 1024);

        assert!(working_set.entries.contains_key(&first_id));
        assert!(!working_set.entries.contains_key(&second_id));
        assert!(working_set.entries.contains_key(&third_id));
        assert_eq!(working_set.encoded_bytes, 50 * 1024 * 1024);

        working_set.insert(execution_state(4), EXECUTION_WORKING_SET_BYTES + 1);
        assert!(working_set.entries.is_empty());
        assert!(working_set.lru.is_empty());
        assert_eq!(working_set.encoded_bytes, 0);
    }

    #[test]
    fn failed_commit_keeps_cached_state_and_failed_rollback_poisons_owner() {
        let fixture = activation_fixture();
        let directory = tempfile::tempdir().expect("tempdir");
        let config = StoreConfig::new(directory.path().join("store.sqlite"), fixture.producer);
        let lock = acquire_process_lock(&config.path).expect("owner lock");
        let mut database = Database::open(&config, lock).expect("database");
        let state = execution_state(8);
        let state_size = state_bytes(&state).expect("state encoding").len();
        database.executions.insert(state.clone(), state_size);
        let next = match arena0_protocol::execution::transition(&state, ExecutionInput::Activate)
            .expect("activation transition")
        {
            TransitionOutcome::Commit(plan) => plan.next_state().clone(),
            TransitionOutcome::AlreadyApplied => panic!("fresh execution must activate"),
        };

        database.begin().expect("transaction");
        database
            .connection
            .execute_batch("PRAGMA defer_foreign_keys = ON")
            .expect("defer foreign keys");
        database
            .connection
            .execute(
                "INSERT INTO execution_salts (execution_id, salt, created_at_ms)
                 VALUES (?1, ?2, 1)",
                params![vec![0xff_u8; 32], vec![1_u8]],
            )
            .expect("deferred foreign-key violation");
        database.pending_execution = Some(PendingExecution {
            encoded_bytes: state_bytes(&next).expect("next state encoding").len(),
            state: next,
        });

        assert!(database.commit_result(()).is_err());
        assert!(!database.is_poisoned());
        assert_eq!(
            database
                .executions
                .get(state.execution_id())
                .expect("cached state")
                .version(),
            state.version()
        );
        assert!(database.pending_execution.is_none());

        let rollback: Result<(), StoreError> =
            database.rollback_result(StoreError::Corruption("test failure".into()));
        assert!(matches!(rollback, Err(StoreError::Corruption(_))));
        assert!(database.is_poisoned());
        assert!(database.begin().is_err());
    }
}
