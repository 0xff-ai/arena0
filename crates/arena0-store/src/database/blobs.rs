//! Blob metadata SQL and blob files. Bytes live in files under the store's
//! blob directory or at linked paths; the database records only metadata.
//! Nothing here checks content against a hash except linking (the store
//! naming what it holds); received content is checked by the dispatch that
//! commits it.

use super::*;
use arena0_protocol::BlobHash;
use arena0_protocol::execution::{BlobChange, BlobPartial};
use std::io::Write;
use std::os::unix::fs::{FileExt, OpenOptionsExt};

impl Database {
    pub(crate) fn create_blob_tables(connection: &Connection) -> Result<(), StoreError> {
        connection.execute_batch(
            // `blobs`: every blob this Host can read. `path` is the linked
            // file (`linked = 1`, not owned) or an owned received file under
            // the blob directory (`linked = 0`).
            // `blob_grants`: which blobs an execution may read, from its
            // admission request or from its own commits.
            // `blob_partials`: objects an execution is receiving or received;
            // bytes `[0, written)` of `recv-<exec_hex>-<hash_hex>` are
            // durable. A committed row stays, so the execution never
            // receives that hash again.
            "CREATE TABLE blobs (
                hash BLOB PRIMARY KEY, length INTEGER NOT NULL,
                path TEXT NOT NULL, linked INTEGER NOT NULL
             ) STRICT;
             CREATE TABLE blob_grants (
                execution_id BLOB NOT NULL, hash BLOB NOT NULL REFERENCES blobs(hash),
                PRIMARY KEY (execution_id, hash)
             ) STRICT;
             CREATE TABLE blob_partials (
                execution_id BLOB NOT NULL, hash BLOB NOT NULL,
                length INTEGER NOT NULL, written INTEGER NOT NULL,
                committed INTEGER NOT NULL,
                PRIMARY KEY (execution_id, hash)
             ) STRICT;",
        )?;
        Ok(())
    }

    pub(crate) fn received_path(&self, execution_id: ExecId, hash: BlobHash) -> PathBuf {
        let exec_hex = blake3::Hash::from_bytes(execution_id.0).to_hex();
        let hash_hex = blake3::Hash::from_bytes(hash.0).to_hex();
        self.blob_dir.join(format!("recv-{exec_hex}-{hash_hex}"))
    }

    /// The owner lock excludes actors and imports during this open-time sweep.
    /// Committed duplicate receives have no ownership and can be reclaimed.
    pub(super) fn sweep_blob_files(&self) -> Result<(), StoreError> {
        let mut retained = std::collections::HashSet::new();
        let mut statement = self.connection.prepare("SELECT path FROM blobs")?;
        for path in statement.query_map([], |row| row.get::<_, String>(0))? {
            retained.insert(PathBuf::from(path?));
        }
        let mut statement = self
            .connection
            .prepare("SELECT execution_id, hash FROM blob_partials WHERE committed = 0")?;
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })? {
            let (execution_id, hash) = row?;
            retained.insert(self.received_path(
                ExecId(array32(&execution_id, "blob execution id")?),
                BlobHash(array32(&hash, "blob hash")?),
            ));
        }
        for entry in std::fs::read_dir(&self.blob_dir)? {
            let entry = entry?;
            if entry.file_name().as_encoded_bytes().starts_with(b"recv-")
                && !entry.file_type()?.is_dir()
                && !retained.contains(&entry.path())
            {
                std::fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }

    pub(crate) fn insert_linked_blob(
        &mut self,
        hash: BlobHash,
        length: u64,
        path: &Path,
    ) -> Result<(), StoreError> {
        let path = path.to_str().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "blob path is not UTF-8")
        })?;
        self.connection.execute(
            "INSERT INTO blobs (hash, length, path, linked) VALUES (?1, ?2, ?3, 1)
             ON CONFLICT(hash) DO UPDATE SET length = excluded.length, path = excluded.path
             WHERE blobs.linked = 1",
            params![hash.0.as_slice(), sqlite_u64(length)?, path],
        )?;
        Ok(())
    }

    pub(crate) fn blob_location(
        &self,
        hash: BlobHash,
    ) -> Result<Option<(PathBuf, u64)>, StoreError> {
        self.connection
            .query_row(
                "SELECT path, length FROM blobs WHERE hash = ?1",
                params![hash.0.as_slice()],
                |row| {
                    Ok((
                        PathBuf::from(row.get::<_, String>(0)?),
                        row.get::<_, i64>(1)?,
                    ))
                },
            )
            .optional()?
            .map(|(path, length)| Ok((path, sqlite_i64(length)?)))
            .transpose()
    }

    /// Every blob row, ordered by hash bytes.
    pub(crate) fn list_blobs(&self) -> Result<Vec<BlobRecord>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT hash, length, path, linked FROM blobs ORDER BY hash")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(hash, length, path, linked)| {
                Ok(BlobRecord {
                    hash: BlobHash(array32(&hash, "blob hash")?),
                    length: sqlite_i64(length)?,
                    path: PathBuf::from(path),
                    linked,
                })
            })
            .collect()
    }

    pub(crate) fn blob_granted(
        &self,
        execution_id: ExecId,
        hash: BlobHash,
    ) -> Result<Option<u64>, StoreError> {
        self.connection
            .query_row(
                "SELECT b.length FROM blob_grants g JOIN blobs b ON b.hash = g.hash
             WHERE g.execution_id = ?1 AND g.hash = ?2",
                params![execution_id.0.as_slice(), hash.0.as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .map(sqlite_i64)
            .transpose()
    }

    /// Runs in persist's transaction after its version check. Files are synced
    /// before publishing metadata; rolled-back tails are overwritten on retry.
    pub(super) fn apply_blob_changes(
        &mut self,
        execution_id: ExecId,
        changes: Vec<BlobChange>,
    ) -> Result<(), StoreError> {
        for change in changes {
            match change {
                BlobChange::Append {
                    hash,
                    length,
                    offset,
                    bytes,
                } => {
                    let partial = self.blob_partial(execution_id, hash)?;
                    if partial.map_or(offset != 0, |row| {
                        row.committed || row.length != length || row.written != offset
                    }) {
                        return Err(StoreError::Corruption(
                            "blob append does not continue the partial".into(),
                        ));
                    }
                    let path = self.received_path(execution_id, hash);
                    let written = offset + bytes.len() as u64;
                    if partial.is_none() {
                        let mut file = std::fs::OpenOptions::new()
                            .write(true)
                            .create(true)
                            .truncate(true)
                            .mode(0o600)
                            .open(path)?;
                        file.write_all(&bytes)?;
                        file.sync_data()?;
                        std::fs::File::open(&self.blob_dir)?.sync_all()?;
                        self.connection.execute(
                            "INSERT INTO blob_partials (execution_id, hash, length, written, committed) VALUES (?1, ?2, ?3, ?4, 0)",
                            params![execution_id.0.as_slice(), hash.0.as_slice(), sqlite_u64(length)?, sqlite_u64(written)?],
                        )?;
                    } else {
                        let file = std::fs::OpenOptions::new().write(true).open(path)?;
                        file.write_all_at(&bytes, offset)?;
                        file.sync_data()?;
                        self.connection.execute(
                            "UPDATE blob_partials SET written = ?3 WHERE execution_id = ?1 AND hash = ?2",
                            params![execution_id.0.as_slice(), hash.0.as_slice(), sqlite_u64(written)?],
                        )?;
                    }
                }
                BlobChange::Commit { hash } => {
                    let partial = self.blob_partial(execution_id, hash)?;
                    let Some(partial) =
                        partial.filter(|row| !row.committed && row.written == row.length)
                    else {
                        return Err(StoreError::Corruption(
                            "committing an incomplete blob partial".into(),
                        ));
                    };
                    let path = self.received_path(execution_id, hash);
                    let path = path.to_str().ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidInput,
                            "blob path is not UTF-8",
                        )
                    })?;
                    self.connection.execute(
                        "INSERT INTO blobs (hash, length, path, linked) VALUES (?1, ?2, ?3, 0)
                         ON CONFLICT(hash) DO UPDATE SET length = excluded.length, path = excluded.path, linked = 0 WHERE blobs.linked = 1",
                        params![hash.0.as_slice(), sqlite_u64(partial.length)?, path],
                    )?;
                    self.connection.execute(
                        "INSERT OR IGNORE INTO blob_grants (execution_id, hash) VALUES (?1, ?2)",
                        params![execution_id.0.as_slice(), hash.0.as_slice()],
                    )?;
                    self.connection.execute("UPDATE blob_partials SET committed = 1 WHERE execution_id = ?1 AND hash = ?2", params![execution_id.0.as_slice(), hash.0.as_slice()])?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn blob_partial(
        &self,
        execution_id: ExecId,
        hash: BlobHash,
    ) -> Result<Option<BlobPartial>, StoreError> {
        self.connection.query_row(
            "SELECT length, written, committed FROM blob_partials WHERE execution_id = ?1 AND hash = ?2",
            params![execution_id.0.as_slice(), hash.0.as_slice()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, bool>(2)?)),
        ).optional()?.map(|(length, written, committed)| Ok(BlobPartial {
            length: sqlite_i64(length)?, written: sqlite_i64(written)?, committed,
        })).transpose()
    }
}
