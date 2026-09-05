use super::*;

struct RawReceiptRow {
    receipt_id: Vec<u8>,
    session_id: Vec<u8>,
    kind: String,
    artifact: Vec<u8>,
}

fn artifact_kind(receipt: &ReceiptArtifact) -> &'static str {
    match receipt.kind() {
        arena0_protocol::ReceiptKind::Receipt => "receipt",
        arena0_protocol::ReceiptKind::StopReport => "stop_report",
    }
}

impl Database {
    fn receipt_row_by_id(
        &self,
        receipt_id: ReceiptId,
    ) -> Result<Option<RawReceiptRow>, StoreError> {
        self.connection
            .query_row(
                "SELECT receipt_id, session_id, kind, artifact FROM receipts WHERE receipt_id = ?1",
                params![receipt_id.as_bytes().to_vec()],
                |row| {
                    Ok(RawReceiptRow {
                        receipt_id: row.get(0)?,
                        session_id: row.get(1)?,
                        kind: row.get(2)?,
                        artifact: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sqlite)
    }

    /// Resolve this Host's own publication, including a unilateral stop report.
    /// Imported evidence for the same session cannot replace that local fact.
    pub(super) fn load_receipt(
        &mut self,
        session_id: SessionHash,
    ) -> Result<Option<StoredReceipt>, StoreError> {
        let id = self.connection.query_row(
            "SELECT r.receipt_id FROM receipts r JOIN receipt_productions p USING (receipt_id) WHERE r.session_id = ?1",
            params![session_id.0.to_vec()], |row| row.get::<_, Vec<u8>>(0),
        ).optional()?;
        match id {
            Some(id) => self.load_receipt_by_id(ReceiptId::from_bytes(array32(&id, "receipt id")?)),
            None => Ok(None),
        }
    }

    pub(super) fn load_receipt_by_id(
        &mut self,
        receipt_id: ReceiptId,
    ) -> Result<Option<StoredReceipt>, StoreError> {
        let Some(row) = self.receipt_row_by_id(receipt_id)? else {
            return Ok(None);
        };
        let stored = self.decode_stored_receipt(row)?;
        if stored.receipt_id != receipt_id {
            return Err(StoreError::Corruption(
                "receipt id lookup returned a different artifact".into(),
            ));
        }
        Ok(Some(stored))
    }

    fn decode_stored_receipt(&mut self, row: RawReceiptRow) -> Result<StoredReceipt, StoreError> {
        let payload = open_envelope(
            EnvelopeKind::Receipt,
            &row.artifact,
            arena0_protocol::MAX_RECEIPT_BYTES,
        )?;
        let receipt = ReceiptArtifact::decode(&payload)
            .map_err(|error| StoreError::Corruption(format!("receipt decode: {error}")))?;
        let receipt_id = ReceiptId::from_bytes(array32(&row.receipt_id, "receipt id")?);
        let session_id = SessionHash(array32(&row.session_id, "receipt session")?);
        if receipt.receipt_id() != receipt_id
            || receipt.body().header().session_hash() != session_id
            || artifact_kind(&receipt) != row.kind
        {
            return Err(StoreError::Corruption(
                "receipt indexes do not match its artifact".into(),
            ));
        }
        let imported = self.receipt_import_exists(receipt_id)?;
        let produced_execution = self.receipt_production(receipt_id)?;
        if let Some(execution_id) = produced_execution {
            let state = self
                .load_execution(execution_id)?
                .ok_or(StoreError::ExecutionNotFound(execution_id))?;
            if state.binding().session_id() != session_id
                || state.producer() != self.host_id
                || state.published_receipt_id() != Some(receipt_id)
            {
                return Err(StoreError::Corruption(
                    "produced receipt is not bound to its Host execution".into(),
                ));
            }
        }
        Ok(StoredReceipt {
            receipt_id,
            provenance: ReceiptProvenance::from_facts(imported, produced_execution.is_some())?,
            receipt,
        })
    }

    fn receipt_import_exists(&self, receipt_id: ReceiptId) -> Result<bool, StoreError> {
        Ok(self
            .connection
            .query_row(
                "SELECT 1 FROM receipt_imports WHERE receipt_id = ?1",
                params![receipt_id.as_bytes().to_vec()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .is_some())
    }

    fn receipt_production(&self, receipt_id: ReceiptId) -> Result<Option<ExecId>, StoreError> {
        self.connection
            .query_row(
                "SELECT execution_id FROM receipt_productions WHERE receipt_id = ?1",
                params![receipt_id.as_bytes().to_vec()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .map(|bytes| Ok(ExecId(array32(&bytes, "receipt production execution")?)))
            .transpose()
    }

    pub(super) fn list_receipts(&mut self, limit: usize) -> Result<Vec<StoredReceipt>, StoreError> {
        let limit = i64::try_from(limit)
            .map_err(|_| StoreError::InvalidConfiguration("receipt limit is too large"))?;
        let mut statement = self
            .connection
            .prepare("SELECT receipt_id FROM receipts ORDER BY receipt_id LIMIT ?1")?;
        let ids = statement
            .query_map(params![limit], |row| row.get::<_, Vec<u8>>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut response_bytes = 0;
        ids.into_iter()
            .map(|id| {
                let receipt = self
                    .load_receipt_by_id(ReceiptId::from_bytes(array32(&id, "receipt id")?))?
                    .ok_or_else(|| {
                        StoreError::Corruption("receipt disappeared while listing".into())
                    })?;
                account_response(&mut response_bytes, receipt.receipt.encode()?.len())?;
                Ok(receipt)
            })
            .collect()
    }

    pub(super) fn import_receipt(
        &mut self,
        receipt: ReceiptArtifact,
        now_ms: u64,
    ) -> Result<ReceiptImportOutcome, StoreError> {
        self.begin()?;
        let result = self.import_receipt_in_transaction(receipt, now_ms);
        match result {
            Ok(outcome) => self.commit_result(outcome),
            Err(error) => self.rollback_result(error),
        }
    }

    fn import_receipt_in_transaction(
        &mut self,
        receipt: ReceiptArtifact,
        now_ms: u64,
    ) -> Result<ReceiptImportOutcome, StoreError> {
        let receipt_id = receipt.receipt_id();
        let stored = self.load_receipt_by_id(receipt_id)?;
        let existed = stored.is_some();
        if let Some(stored) = stored
            && stored.receipt != receipt
        {
            return Err(StoreError::Corruption(
                "receipt identity is already bound to different content".into(),
            ));
        }
        self.insert_artifact(&receipt, now_ms)?;
        if self.receipt_import_exists(receipt_id)? {
            return Ok(ReceiptImportOutcome::AlreadyImported);
        }
        self.connection.execute(
            "INSERT INTO receipt_imports (receipt_id, imported_at_ms) VALUES (?1, ?2)",
            params![receipt_id.as_bytes().to_vec(), sqlite_u64(now_ms)?],
        )?;
        Ok(if existed {
            ReceiptImportOutcome::AlreadyProduced
        } else {
            ReceiptImportOutcome::Imported
        })
    }

    /// Store immutable evidence once. Reports may coexist for a session; two
    /// different unanimously certified receipts for one session are a conflict.
    fn insert_artifact(
        &mut self,
        receipt: &ReceiptArtifact,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let bytes = receipt.encode()?;
        if ReceiptArtifact::decode(&bytes)? != *receipt {
            return Err(StoreError::Corruption(
                "receipt codec changed the artifact content".into(),
            ));
        }
        let receipt_id = receipt.receipt_id();
        let session_id = receipt.body().header().session_hash();
        let kind = artifact_kind(receipt);
        if let Some(row) = self.receipt_row_by_id(receipt_id)? {
            let payload = open_envelope(
                EnvelopeKind::Receipt,
                &row.artifact,
                arena0_protocol::MAX_RECEIPT_BYTES,
            )?;
            if payload != bytes || row.session_id != session_id.0.to_vec() || row.kind != kind {
                return Err(StoreError::Corruption(
                    "receipt identity is already bound to different content".into(),
                ));
            }
            return Ok(());
        }
        if kind == "receipt" {
            let occupied = self
                .connection
                .query_row(
                    "SELECT receipt_id FROM receipts WHERE session_id = ?1 AND kind = 'receipt'",
                    params![session_id.0.to_vec()],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()?;
            if occupied.is_some() {
                return Err(StoreError::Corruption(
                    "session already has a different canonical receipt".into(),
                ));
            }
        }
        self.connection.execute("INSERT INTO receipts (receipt_id, session_id, kind, artifact, stored_at_ms) VALUES (?1, ?2, ?3, ?4, ?5)", params![receipt_id.as_bytes().to_vec(), session_id.0.to_vec(), kind, envelope(EnvelopeKind::Receipt, &bytes)?, sqlite_u64(now_ms)?])?;
        Ok(())
    }

    pub(super) fn persist_terminal(
        &mut self,
        execution_id: ExecId,
        version: ExecutionVersion,
        plan: &CommitPlan,
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let Some(terminal) = plan.terminal() else {
            return Ok(());
        };
        let receipt = terminal.receipt();
        let publication_bytes = borsh::to_vec(terminal).map_err(|error| {
            StoreError::Corruption(format!("terminal publication encode: {error}"))
        })?;
        self.insert_artifact(receipt, now_ms)?;
        self.connection.execute(
            "INSERT INTO receipt_productions (receipt_id, execution_id) VALUES (?1, ?2)",
            params![
                receipt.receipt_id().as_bytes().to_vec(),
                execution_id.0.to_vec()
            ],
        )?;
        self.connection.execute("INSERT INTO terminal_proofs (execution_id, version, receipt_id, publication) VALUES (?1, ?2, ?3, ?4)", params![execution_id.0.to_vec(), sqlite_u64(version.get())?, receipt.receipt_id().as_bytes().to_vec(), envelope(EnvelopeKind::TerminalPublication, &publication_bytes)?])?;
        Ok(())
    }

    pub(super) fn validate_terminal_rows(
        &mut self,
        state: &ExecutionState,
    ) -> Result<(), StoreError> {
        let execution_id = state.execution_id();
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM terminal_proofs WHERE execution_id = ?1",
            params![execution_id.0.to_vec()],
            |row| row.get(0),
        )?;
        let production_count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM receipt_productions WHERE execution_id = ?1",
            params![execution_id.0.to_vec()],
            |row| row.get(0),
        )?;
        let expected = i64::from(state.published_receipt_id().is_some());
        if count != expected || production_count != expected {
            return Err(StoreError::Corruption(
                "terminal publication rows do not match execution status".into(),
            ));
        }
        let Some(receipt_id) = state.published_receipt_id() else {
            return Ok(());
        };
        let (version, row_id, publication): (i64, Vec<u8>, Vec<u8>) = self.connection.query_row(
            "SELECT version, receipt_id, publication FROM terminal_proofs WHERE execution_id = ?1",
            params![execution_id.0.to_vec()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let payload = open_envelope(
            EnvelopeKind::TerminalPublication,
            &publication,
            arena0_protocol::MAX_RECEIPT_BYTES,
        )?;
        let terminal: arena0_protocol::TerminalPublication =
            decode_borsh(&payload, "terminal publication")?;
        let receipt = terminal.receipt();
        if sqlite_i64(version)? != state.version().get()
            || row_id != receipt_id.as_bytes().to_vec()
            || receipt.receipt_id() != receipt_id
            || receipt.body().header().activation != *state.binding().activation()
            || state.producer() != self.host_id
            || self.receipt_production(receipt_id)? != Some(execution_id)
        {
            return Err(StoreError::Corruption(
                "terminal proof does not match published execution state".into(),
            ));
        }
        if receipt.body().trace() != self.load_public_trace_in_transaction(state)? {
            return Err(StoreError::Corruption(
                "receipt trace does not match durable public commits".into(),
            ));
        }
        match receipt.body().termination() {
            arena0_protocol::ReceiptTermination::Completed { terminal } => {
                let certificate = state.terminal_certificate().ok_or_else(|| {
                    StoreError::Corruption("completed receipt has no execution certificate".into())
                })?;
                let outcome = state.terminal_outcome().ok_or_else(|| {
                    StoreError::Corruption("completed receipt has no execution outcome".into())
                })?;
                if terminal.agreement != *certificate.agreement()
                    || receipt.body().outcome() != outcome.borsh()
                {
                    return Err(StoreError::Corruption(
                        "receipt completion does not match execution evidence".into(),
                    ));
                }
            }
            arena0_protocol::ReceiptTermination::Stopped { cause } => {
                if state.status().terminal_cause() != Some(cause) {
                    return Err(StoreError::Corruption(
                        "stop artifact does not match execution cause".into(),
                    ));
                }
            }
        }
        let row = self
            .receipt_row_by_id(receipt_id)?
            .ok_or_else(|| StoreError::Corruption("published receipt row is missing".into()))?;
        let bytes = open_envelope(
            EnvelopeKind::Receipt,
            &row.artifact,
            arena0_protocol::MAX_RECEIPT_BYTES,
        )?;
        if ReceiptArtifact::decode(&bytes)? != *receipt
            || row.session_id != state.binding().session_id().0.to_vec()
            || row.kind != artifact_kind(receipt)
        {
            return Err(StoreError::Corruption(
                "receipt artifact does not match terminal publication".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn validate_receipts(&mut self) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT receipt_id, session_id, kind, artifact FROM receipts ORDER BY receipt_id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(RawReceiptRow {
                    receipt_id: row.get(0)?,
                    session_id: row.get(1)?,
                    kind: row.get(2)?,
                    artifact: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for row in rows {
            self.decode_stored_receipt(row)?;
        }
        Ok(())
    }
}
