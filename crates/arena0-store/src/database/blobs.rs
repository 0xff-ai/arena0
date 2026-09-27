//! Blob SQL stays with the dispatch transaction owner. Writes are verified by
//! the sandbox before staging; publication checks coverage, not cryptography.

use super::*;
use arena0_protocol::execution::{BlobChange, BlobResource};
use arena0_protocol::{BlobHandle, BlobHash, MAX_BLOB_BYTES};

impl Database {
    pub(crate) fn create_blob_tables(connection: &Connection) -> Result<(), StoreError> {
        connection.execute_batch(
            "CREATE TABLE blob_content (
                hash BLOB PRIMARY KEY, length INTEGER NOT NULL, bytes BLOB NOT NULL
             ) STRICT;
             CREATE TABLE blob_resources (
                execution_id BLOB NOT NULL, event_position INTEGER NOT NULL,
                call_index INTEGER NOT NULL, hash BLOB NOT NULL, length INTEGER NOT NULL,
                output INTEGER NOT NULL, committed INTEGER NOT NULL,
                PRIMARY KEY (execution_id, event_position, call_index)
             ) STRICT;
             CREATE TABLE blob_writes (
                execution_id BLOB NOT NULL, event_position INTEGER NOT NULL,
                call_index INTEGER NOT NULL, offset INTEGER NOT NULL, bytes BLOB NOT NULL,
                PRIMARY KEY (execution_id, event_position, call_index, offset)
             ) STRICT;",
        )?;
        Ok(())
    }

    pub(crate) fn import_blob(&mut self, bytes: Vec<u8>) -> Result<(BlobHash, u64), StoreError> {
        let length = bytes.len() as u64;
        if length > MAX_BLOB_BYTES {
            return Err(StoreError::BlobTooLarge { length });
        }
        let hash = BlobHash(*blake3::hash(&bytes).as_bytes());
        self.connection.execute(
            "INSERT OR IGNORE INTO blob_content (hash, length, bytes) VALUES (?1, ?2, ?3)",
            params![&hash.0[..], sqlite_u64(length)?, bytes],
        )?;
        Ok((hash, length))
    }

    pub(crate) fn read_blob(&self, hash: BlobHash) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self
            .connection
            .query_row(
                "SELECT bytes FROM blob_content WHERE hash = ?1",
                params![&hash.0[..]],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub(crate) fn blob_contains(&self, hash: BlobHash, length: u64) -> Result<bool, StoreError> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM blob_content WHERE hash = ?1 AND length = ?2)",
            params![&hash.0[..], sqlite_u64(length)?],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn blob_resource(
        &self,
        execution_id: ExecId,
        handle: BlobHandle,
    ) -> Result<Option<BlobResource>, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT hash, length, output, committed FROM blob_resources
             WHERE execution_id = ?1 AND event_position = ?2 AND call_index = ?3",
                params![
                    &execution_id.0[..],
                    sqlite_u64(handle.event_position)?,
                    handle.call_index
                ],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, bool>(2)?,
                        row.get::<_, bool>(3)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(hash, length, output, committed)| {
            Ok(BlobResource {
                hash: BlobHash(array32(&hash, "blob hash")?),
                length: sqlite_i64(length)?,
                output,
                committed,
            })
        })
        .transpose()
    }

    pub(crate) fn blob_written(
        &self,
        execution_id: ExecId,
        handle: BlobHandle,
    ) -> Result<Vec<std::ops::Range<u64>>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT offset, length(bytes) FROM blob_writes
             WHERE execution_id = ?1 AND event_position = ?2 AND call_index = ?3
             ORDER BY offset",
        )?;
        let rows = statement.query_map(
            params![
                &execution_id.0[..],
                sqlite_u64(handle.event_position)?,
                handle.call_index
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        rows.map(|row| {
            let (offset, length) = row?;
            let start = sqlite_i64(offset)?;
            let end = start
                .checked_add(sqlite_i64(length)?)
                .ok_or_else(|| StoreError::Corruption("blob range overflow".into()))?;
            Ok(start..end)
        })
        .collect()
    }

    /// Called only inside persist's transaction, after its version check.
    /// Call order matters: a dispatch can create, write, and publish an output.
    pub(super) fn apply_blob_changes(
        &mut self,
        execution_id: ExecId,
        changes: Vec<BlobChange>,
    ) -> Result<(), StoreError> {
        for change in changes {
            match change {
                BlobChange::Create {
                    handle,
                    hash,
                    length,
                }
                | BlobChange::Resolve {
                    handle,
                    hash,
                    length,
                } => {
                    let output = matches!(change, BlobChange::Create { .. });
                    self.connection.execute(
                        "INSERT INTO blob_resources
                         (execution_id, event_position, call_index, hash, length, output, committed)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            &execution_id.0[..],
                            sqlite_u64(handle.event_position)?,
                            handle.call_index,
                            &hash.0[..],
                            sqlite_u64(length)?,
                            output,
                            !output
                        ],
                    )?;
                }
                BlobChange::Write {
                    handle,
                    offset,
                    bytes,
                } => {
                    self.connection.execute(
                        "INSERT INTO blob_writes
                         (execution_id, event_position, call_index, offset, bytes)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            &execution_id.0[..],
                            sqlite_u64(handle.event_position)?,
                            handle.call_index,
                            sqlite_u64(offset)?,
                            bytes
                        ],
                    )?;
                }
                BlobChange::Commit { handle } => {
                    let resource = self.blob_resource(execution_id, handle)?.ok_or_else(|| {
                        StoreError::Corruption("committing a missing blob resource".into())
                    })?;
                    let mut content = Vec::new();
                    {
                        let mut statement = self.connection.prepare(
                            "SELECT offset, bytes FROM blob_writes
                             WHERE execution_id = ?1 AND event_position = ?2 AND call_index = ?3
                             ORDER BY offset",
                        )?;
                        let mut rows = statement.query(params![
                            &execution_id.0[..],
                            sqlite_u64(handle.event_position)?,
                            handle.call_index
                        ])?;
                        while let Some(row) = rows.next()? {
                            let offset = sqlite_i64(row.get(0)?)?;
                            let bytes: Vec<u8> = row.get(1)?;
                            if offset != content.len() as u64
                                || bytes.is_empty()
                                || bytes.len() as u64 > resource.length.saturating_sub(offset)
                            {
                                return Err(StoreError::Corruption(
                                    "blob writes do not tile the object".into(),
                                ));
                            }
                            content.extend_from_slice(&bytes);
                        }
                    }
                    if content.len() as u64 != resource.length {
                        return Err(StoreError::Corruption(
                            "blob writes do not tile the object".into(),
                        ));
                    }
                    // Verified ranges already establish the bound hash. Rehashing
                    // here would duplicate the sandbox's proof work.
                    self.connection.execute(
                        "INSERT OR IGNORE INTO blob_content (hash, length, bytes) VALUES (?1, ?2, ?3)",
                        params![&resource.hash.0[..], sqlite_u64(resource.length)?, content],
                    )?;
                    self.connection.execute(
                        "DELETE FROM blob_writes WHERE execution_id = ?1 AND event_position = ?2 AND call_index = ?3",
                        params![&execution_id.0[..], sqlite_u64(handle.event_position)?, handle.call_index],
                    )?;
                    self.connection.execute(
                        "UPDATE blob_resources SET committed = 1
                         WHERE execution_id = ?1 AND event_position = ?2 AND call_index = ?3",
                        params![
                            &execution_id.0[..],
                            sqlite_u64(handle.event_position)?,
                            handle.call_index
                        ],
                    )?;
                }
            }
        }
        Ok(())
    }
}
