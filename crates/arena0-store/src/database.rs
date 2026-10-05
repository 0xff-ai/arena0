use super::*;
use crate::lock::{restrict_database_companions, sync_parent_directory};

mod activation;
mod blobs;
mod execution;
mod integrity;
mod receipt;
mod recovery;
mod registry;
mod request;
mod resolve;
mod step_state;
mod summary;
mod timers;

pub use resolve::{IdMatches, IdSpace};
pub(crate) use summary::ReadDb;

pub(super) struct Database {
    connection: Connection,
    _lock: OwnerLock,
    host_id: PeerId,
    transaction_poison: Option<String>,
    // The writer holds Inner.db before locking this log; readers of the log
    // never take the writer lock. Publishing after COMMIT preserves its order.
    pub(super) changes: Arc<StdMutex<changes::ChangeLog>>,
    pending: Vec<ChangeKey>,
    /// Owned received files, created at open with mode 0o700 on Unix.
    blob_dir: PathBuf,
}

impl Database {
    /// Record that the open transaction wrote the summary row `key`. Every
    /// write path that changes a row a summary read returns calls this inside
    /// its transaction; `commit_result` publishes the recorded keys to the
    /// store's change log after `COMMIT` succeeds, and a rollback discards
    /// them. Duplicate keys within one transaction publish once.
    pub(super) fn record_change(&mut self, key: crate::ChangeKey) {
        if !self.pending.contains(&key) {
            self.pending.push(key);
        }
    }

    /// Park the actual writer with a transaction open, solely to prove read
    /// connections can serve committed snapshots independently of its mutex.
    #[cfg(test)]
    pub(crate) fn hold_write_transaction(
        &mut self,
        ready: tokio::sync::oneshot::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    ) -> Result<(), StoreError> {
        self.begin()?;
        ready.send(()).unwrap();
        release.recv().unwrap();
        self.commit_result(())
    }

    /// The store's owned-blob directory.
    pub(crate) fn blob_dir(&self) -> &Path {
        &self.blob_dir
    }
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
        // Use the full database filename, including its extension. The owner
        // lock and the sweep must cover exactly the same store's files.
        let mut blob_dir = config.path.as_os_str().to_os_string();
        blob_dir.push(".blobs");
        let mut database = Self {
            connection,
            _lock: lock,
            host_id: config.host_id,
            transaction_poison: None,
            changes: Arc::new(StdMutex::new(changes::ChangeLog::new())),
            pending: Vec::new(),
            blob_dir: PathBuf::from(blob_dir),
        };
        database.bind_metadata()?;
        database.validate_database()?;
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.recursive(true).create(&database.blob_dir)?;
        // Appends sync files and blob_dir, but its own directory entry belongs
        // to the parent. Persist that entry before any transition can use it.
        sync_parent_directory(&database.blob_dir)?;
        // Linked paths are canonical. Use the same spelling for owned paths
        // so the sweep also preserves links within a relatively opened store.
        database.blob_dir = std::fs::canonicalize(&database.blob_dir)?;
        database.sweep_blob_files()?;
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

    /// An aggregate's terminal lifecycle owns archive status when it exists.
    /// Without an aggregate, a recorded request failure makes it archived.
    /// Startup uses columns only to decide which execution evidence to decode.
    fn execution_is_archived(&self, execution_id: ExecId) -> Result<bool, StoreError> {
        self.connection.query_row(
            "SELECT CASE WHEN EXISTS (SELECT 1 FROM executions WHERE execution_id = ?1)
             THEN EXISTS (SELECT 1 FROM executions WHERE execution_id = ?1 AND lifecycle IN (?2, ?3, ?4))
             ELSE EXISTS (SELECT 1 FROM exec_requests WHERE execution_id = ?1 AND failure IS NOT NULL) END",
            params![execution_id.0.to_vec(), lifecycle_tag(ExecLifecycle::Completed), lifecycle_tag(ExecLifecycle::Aborted), lifecycle_tag(ExecLifecycle::Failed)],
            |row| row.get(0),
        ).map_err(StoreError::Sqlite)
    }

    /// Validate unfinished execution evidence and the other store registries at open.
    /// Terminal aggregates and failed requests without an aggregate are archived;
    /// their request and activation evidence is validated when explicitly read.
    fn validate_database(&mut self) -> Result<(), StoreError> {
        let terminal = [
            ExecLifecycle::Completed,
            ExecLifecycle::Aborted,
            ExecLifecycle::Failed,
        ];
        let mut after = None;
        loop {
            let after_bytes = after.map(|execution_id: ExecId| execution_id.0.to_vec());
            let execution_ids = {
                let mut statement = self.connection.prepare(
                    "SELECT execution_id FROM executions
                     WHERE (?1 IS NULL OR execution_id > ?1)
                     AND lifecycle NOT IN (?3, ?4, ?5)
                     ORDER BY execution_id LIMIT ?2",
                )?;
                let mut rows = statement.query(params![
                    after_bytes,
                    DATABASE_VALIDATION_PAGE_SIZE,
                    lifecycle_tag(terminal[0]),
                    lifecycle_tag(terminal[1]),
                    lifecycle_tag(terminal[2])
                ])?;
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
        database.record_change(ChangeKey::Exec(ExecId([0xff; 32])));

        assert!(database.commit_result(()).is_err());
        assert_eq!(*database.changes.lock().unwrap().subscribe().borrow(), 0);
        assert!(!database.is_poisoned());
        let count: i64 = database
            .connection
            .query_row("SELECT COUNT(*) FROM execution_salts", [], |row| row.get(0))
            .expect("rolled-back rows");
        assert_eq!(count, 0);

        let first = ChangeKey::Exec(ExecId([1; 32]));
        let second = ChangeKey::Exec(ExecId([2; 32]));
        database
            .transaction(|db| {
                db.record_change(first);
                db.record_change(first);
                Ok(())
            })
            .unwrap();
        let failed: Result<(), StoreError> = database.transaction(|db| {
            db.record_change(ChangeKey::Exec(ExecId([3; 32])));
            db.connection.execute(
                "INSERT INTO execution_salts (execution_id, salt, created_at_ms) VALUES (?1, ?2, 1)",
                params![vec![0xff_u8; 32], vec![1_u8]],
            )?;
            Ok(())
        });
        assert!(failed.is_err());
        assert_eq!(*database.changes.lock().unwrap().subscribe().borrow(), 1);
        database
            .transaction(|db| {
                db.record_change(second);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            database.changes.lock().unwrap().since(0),
            Catchup::Keys {
                head: 2,
                keys: vec![first, second]
            }
        );

        let rollback: Result<(), StoreError> =
            database.rollback_result(StoreError::Corruption("test failure".into()));
        assert!(matches!(rollback, Err(StoreError::Corruption(_))));
        assert!(database.is_poisoned());
        assert!(database.begin().is_err());
    }
}
