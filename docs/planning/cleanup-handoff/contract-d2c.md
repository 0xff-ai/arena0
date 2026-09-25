# D2c The store keeps each fact once

## Contract

Crate: `crates/arena0-store`. No new types, functions, columns, files or helpers.

```sql
-- schema.sql: terminal_proofs loses `publication` and gains a receipt foreign key.
CREATE TABLE terminal_proofs (
    execution_id BLOB NOT NULL CHECK (length(execution_id) = 32),
    version INTEGER NOT NULL CHECK (version >= 0),
    receipt_id BLOB NOT NULL CHECK (length(receipt_id) = 32),
    PRIMARY KEY (execution_id, version),
    FOREIGN KEY (execution_id) REFERENCES executions(execution_id),
    FOREIGN KEY (receipt_id) REFERENCES receipts(receipt_id)
) STRICT;
-- schema.sql: executions loses the `local_state_checksum` column line. Nothing else changes.
```

```rust
// src/lib.rs
const SCHEMA_VERSION: u64 = 6; // was 5. No migration.

// src/codec.rs
// delete: EnvelopeKind::TerminalPublication (= 7). Do not renumber other variants.
// delete: pub(crate) fn checksum(bytes: &[u8]) -> [u8; 32]   (its only callers are removed below)

// src/database.rs
struct ExecutionIndexRow { /* delete field: local_checksum: Vec<u8> */ }

// src/database/execution.rs
// insert (INSERT INTO executions ...): drop the `local_state_checksum` column and its
//   `checksum(...)` param; renumber the remaining ?N placeholders.
// load_execution_from_sqlite: drop `local_state_checksum` from the SELECT, the tuple, and the row.
// decode_state_row: delete the `local_checksum` destructure and the
//   "local state checksum does not match execution state" comparison.
// persist_state (UPDATE executions ...): drop `local_state_checksum = ?2` and its param;
//   renumber placeholders.
// The persist path's event_bytes/effects_bytes stay as they are.

// src/database/receipt.rs
pub(super) fn persist_terminal_publication(
    &mut self, execution_id: ExecId, version: ExecutionVersion,
    receipt: &ReceiptArtifact, now_ms: u64,
) -> Result<(), StoreError>;
// - Keep `self.insert_artifact(receipt, now_ms)?` first.
// - Delete `publication_bytes` and the envelope open/compare.
// - Existing row: SELECT version, receipt_id; if version != `version` or receipt_id differs,
//   return Corruption("terminal publication identity was reused with different evidence").
//   Keep the production-relation check and its message unchanged.
// - INSERT INTO terminal_proofs (execution_id, version, receipt_id) VALUES (?1, ?2, ?3).

pub(super) fn validate_terminal_rows(&mut self, state: &ExecutionState) -> Result<(), StoreError>;
// - Keep the row-count and production-count checks unchanged.
// - SELECT version, receipt_id FROM terminal_proofs (no publication).
// - Load the artifact through the receipt row, once:
//     let row = self.receipt_row_by_id(receipt_id)?
//         .ok_or_else(|| StoreError::Corruption("published receipt row is missing".into()))?;
//     let bytes = open_envelope(EnvelopeKind::Receipt, &row.artifact, arena0_protocol::MAX_RECEIPT_BYTES)?;
//     let receipt = ReceiptArtifact::decode(&bytes)?;
// - Then the existing checks in their current order, unchanged in content: the
//   "terminal proof does not match published execution state" check, trace check,
//   termination match.
// - Final check keeps only `row.session_id` and `row.kind` against the receipt
//   (message "receipt artifact does not match terminal publication"); delete the
//   second decode and the `!= receipt` byte cross-comparison.
```

Tests (`src/tests.rs`):
- `unpublished_execution_rejects_terminal_projection_rows_on_restart`: its INSERT drops the
  `publication` column and value. First insert a `receipts` row is NOT required: the test's
  raw connection has foreign keys off. Assertion unchanged (Corruption).
- `published_receipts_require_terminal_proof_and_production_rows_on_restart`: add a third
  case `(0x36, "receipt", "DELETE FROM receipts WHERE receipt_id IN (SELECT receipt_id FROM terminal_proofs WHERE execution_id = ?1)")`.
  Assertion unchanged (Corruption).
- `receipt_is_published_from_durable_rows_after_restart` already proves reopen loads the
  artifact through the relation; keep it unchanged.
- No other test changes unless a test only exercised the deleted byte comparison or the
  deleted checksum; delete such a test and name it in the report.

## Acceptance
- `cargo nextest run -p arena0-store` passes.
- `rg -n "local_state_checksum|TerminalPublication|publication BLOB|fn checksum" crates/arena0-store` prints nothing.
- `just check` passes.
