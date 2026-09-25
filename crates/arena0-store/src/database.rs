use super::*;
use crate::lock::restrict_database_companions;

mod activation;
mod execution;
mod integrity;
mod receipt;
mod recovery;
mod registry;
mod request;
mod timers;

pub(super) struct Database {
    connection: Connection,
    _lock: OwnerLock,
    host_id: PeerId,
    transaction_poison: Option<String>,
}

struct ExecutionIndexRow {
    execution_id: ExecId,
    state_bytes: Vec<u8>,
    version: i64,
    lifecycle: i64,
    agreed_step: i64,
    event_position: i64,
    producer: PeerId,
    session_id: SessionHash,
}

const DATABASE_VALIDATION_PAGE_SIZE: i64 = 64;

#[derive(Debug, Clone, Copy)]
enum ActivationConflictKind {
    Prepared,
    Committed,
}

impl Database {
    pub(super) fn open(config: &StoreConfig, lock: OwnerLock) -> Result<Self, StoreError> {
        prepare_database_file(&config.path)?;
        let connection = Connection::open(&config.path)?;
        configure_connection(&connection, config.busy_timeout)?;
        initialize_schema(&connection)?;
        let mut database = Self {
            connection,
            _lock: lock,
            host_id: config.host_id,
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
        let stored_user_agent = self
            .connection
            .query_row(
                "SELECT value FROM meta WHERE key = 'user_agent'",
                [],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        if let Some(bytes) = stored_user_agent {
            decode_user_agent(bytes)?;
        }
        Ok(())
    }

    pub(super) fn load_user_agent(&mut self) -> Result<Option<String>, StoreError> {
        self.connection
            .query_row(
                "SELECT value FROM meta WHERE key = 'user_agent'",
                [],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .map(decode_user_agent)
            .transpose()
    }

    pub(super) fn set_user_agent(&mut self, value: String) -> Result<(), StoreError> {
        validate_user_agent(&value)?;
        self.connection.execute(
            "INSERT INTO meta (key, value) VALUES ('user_agent', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![value.into_bytes()],
        )?;
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
            }
        }
        self.validate_execution_requests()?;
        self.validate_activation_conflicts()?;
        self.validate_execution_salts()?;
        self.validate_programs()?;
        self.validate_receipts()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::activation_fixture;

    #[test]
    fn failed_commit_rolls_back_and_failed_rollback_poisons_connection() {
        let fixture = activation_fixture();
        let directory = tempfile::tempdir().expect("tempdir");
        let config = StoreConfig::new(directory.path().join("store.sqlite"), fixture.producer);
        let lock = acquire_process_lock(&config.path).expect("owner lock");
        let mut database = Database::open(&config, lock).expect("database");
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

        assert!(database.commit_result(()).is_err());
        assert!(!database.is_poisoned());
        let count: i64 = database
            .connection
            .query_row("SELECT COUNT(*) FROM execution_salts", [], |row| row.get(0))
            .expect("rolled-back rows");
        assert_eq!(count, 0);

        let rollback: Result<(), StoreError> =
            database.rollback_result(StoreError::Corruption("test failure".into()));
        assert!(matches!(rollback, Err(StoreError::Corruption(_))));
        assert!(database.is_poisoned());
        assert!(database.begin().is_err());
    }
}
