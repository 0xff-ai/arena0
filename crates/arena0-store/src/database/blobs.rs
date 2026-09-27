//! Blob metadata SQL and blob files. Bytes live in files under the store's
//! blob directory or at linked paths; the database records only metadata.
//! Nothing here checks content against a hash except linking (the store
//! naming what it holds); received content is checked by the dispatch that
//! commits it.

use super::*;
use arena0_protocol::BlobHash;
use arena0_protocol::execution::{BlobChange, BlobPartial};

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

    /// Called only inside persist's transaction, after its version check, in
    /// call order. Contract in U1.
    pub(super) fn apply_blob_changes(
        &mut self,
        execution_id: ExecId,
        changes: Vec<BlobChange>,
    ) -> Result<(), StoreError> {
        for change in changes {
            let _ = (execution_id, change);
            todo!("U1: apply_blob_changes")
        }
        Ok(())
    }

    #[expect(dead_code, reason = "S0 stub; U1 implements")]
    pub(crate) fn blob_partial(
        &self,
        execution_id: ExecId,
        hash: BlobHash,
    ) -> Result<Option<BlobPartial>, StoreError> {
        let _ = (execution_id, hash);
        todo!("U1: blob_partial")
    }
}
