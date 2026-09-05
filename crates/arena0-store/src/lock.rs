use super::*;

use fs2::FileExt;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

pub(crate) struct OwnerLock {
    _file: File,
    path: PathBuf,
}

impl std::fmt::Debug for OwnerLock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OwnerLock")
            .field("path", &self.path)
            .finish()
    }
}

pub(crate) fn acquire_process_lock(path: &Path) -> Result<OwnerLock, StoreError> {
    let lock_path = owner_lock_path(path);
    let mut options = OpenOptions::new();
    options.create_new(true).read(true).write(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let file = match options.open(&lock_path) {
        Ok(file) => {
            file.sync_all()?;
            sync_parent_directory(&lock_path)?;
            file
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let mut existing = OpenOptions::new();
            existing.read(true).write(true);
            #[cfg(unix)]
            existing.custom_flags(libc::O_NOFOLLOW);
            existing.open(&lock_path)?
        }
        Err(error) => return Err(StoreError::Io(error)),
    };
    restrict_regular_file(&lock_path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::WouldBlock | ErrorKind::AlreadyExists
            ) =>
        {
            return Err(StoreError::AlreadyOwned {
                path: path.to_path_buf(),
            });
        }
        Err(error) => return Err(StoreError::Io(error)),
    }
    Ok(OwnerLock {
        _file: file,
        path: lock_path,
    })
}

pub(crate) fn prepare_database_file(path: &Path) -> Result<(), StoreError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let mut options = OpenOptions::new();
            options.create_new(true).read(true).write(true);
            #[cfg(unix)]
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            let file = options.open(path)?;
            file.sync_all()?;
            sync_parent_directory(path)?;
            return Ok(());
        }
        Err(error) => return Err(StoreError::Io(error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StoreError::InvalidDatabase(
            "store database path must be a regular non-symlink file".into(),
        ));
    }
    restrict_regular_file(path)
}

fn owner_lock_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".arena0-owner");
    PathBuf::from(value)
}

fn sync_parent_directory(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

pub(crate) fn restrict_database_companions(path: &Path) -> Result<(), StoreError> {
    for suffix in ["-wal", "-shm"] {
        let mut value = path.as_os_str().to_os_string();
        value.push(suffix);
        let companion = PathBuf::from(value);
        if let Ok(metadata) = std::fs::symlink_metadata(&companion) {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(StoreError::InvalidDatabase(
                    "SQLite companion path must be a regular non-symlink file".into(),
                ));
            }
            restrict_regular_file(&companion)?;
        }
    }
    Ok(())
}

fn restrict_regular_file(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        let mut permissions = std::fs::metadata(path)?.permissions();
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o600);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

pub(crate) fn configure_connection(
    connection: &Connection,
    busy_timeout: Duration,
) -> Result<(), StoreError> {
    connection.busy_timeout(busy_timeout)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    let foreign_keys: i64 =
        connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    if foreign_keys != 1 {
        return Err(StoreError::InvalidDatabase(
            "foreign_keys pragma is not enabled".into(),
        ));
    }
    let journal_mode: String =
        connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::InvalidDatabase(format!(
            "SQLite journal mode is {journal_mode}, expected WAL"
        )));
    }
    let synchronous: i64 = connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
    // SQLite exposes FULL as 2 for file-backed databases.
    if synchronous != 2 {
        return Err(StoreError::InvalidDatabase(format!(
            "SQLite synchronous mode is {synchronous}, expected FULL"
        )));
    }
    Ok(())
}

pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), StoreError> {
    let current = sqlite_i64(
        connection.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?,
    )?;
    if current == SCHEMA_VERSION {
        return Ok(());
    }
    if current != 0 {
        return Err(StoreError::UnsupportedSchema(current));
    }

    let objects = sqlite_i64(connection.query_row(
        "SELECT COUNT(*) FROM sqlite_schema",
        [],
        |row| row.get::<_, i64>(0),
    )?)?;
    if objects != 0 {
        return Err(StoreError::UnsupportedSchema(0));
    }

    connection.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        connection.execute_batch(include_str!("../schema.sql"))?;
        connection.pragma_update(None, "user_version", sqlite_u64(SCHEMA_VERSION)?)?;
        Ok::<(), StoreError>(())
    })();
    match result {
        Ok(()) => connection.execute_batch("COMMIT")?,
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            return Err(error);
        }
    }

    let recorded = sqlite_i64(
        connection.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?,
    )?;
    if recorded != SCHEMA_VERSION {
        return Err(StoreError::InvalidDatabase(
            "schema initialization did not record the current version".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initializes_an_empty_database_at_the_exact_schema_version() {
        let connection = Connection::open_in_memory().expect("database");

        initialize_schema(&connection).expect("initialize schema");
        initialize_schema(&connection).expect("reopen current schema");

        let version = sqlite_i64(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .expect("schema version"),
        )
        .expect("schema version fits u64");
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn rejects_a_used_unversioned_database() {
        let connection = Connection::open_in_memory().expect("database");
        connection
            .execute_batch(
                "CREATE TABLE discarded (
                    id INTEGER PRIMARY KEY AUTOINCREMENT
                );
                DROP TABLE discarded;",
            )
            .expect("leave sqlite_sequence behind");

        assert!(matches!(
            initialize_schema(&connection),
            Err(StoreError::UnsupportedSchema(0))
        ));
    }

    #[test]
    fn rejects_an_unknown_schema_version() {
        let connection = Connection::open_in_memory().expect("database");
        connection
            .pragma_update(
                None,
                "user_version",
                sqlite_u64(SCHEMA_VERSION + 1).expect("schema version fits i64"),
            )
            .expect("future schema version");

        assert!(matches!(
            initialize_schema(&connection),
            Err(StoreError::UnsupportedSchema(version)) if version == SCHEMA_VERSION + 1
        ));
    }
}
