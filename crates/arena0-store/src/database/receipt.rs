use super::*;

struct RawReceiptRow {
    receipt_id: Vec<u8>,
    proof_id: Vec<u8>,
    session_id: Vec<u8>,
    producer: Vec<u8>,
    artifact: Vec<u8>,
}

impl Database {
    fn receipt_row_by_key(&self, key: ReceiptKey) -> Result<Option<RawReceiptRow>, StoreError> {
        self.connection
            .query_row(
                "SELECT receipt_id, proof_id, session_id, producer, artifact
                 FROM receipts WHERE session_id = ?1 AND producer = ?2",
                params![key.session_id().0.to_vec(), key.producer().0.to_vec()],
                |row| {
                    Ok(RawReceiptRow {
                        receipt_id: row.get(0)?,
                        proof_id: row.get(1)?,
                        session_id: row.get(2)?,
                        producer: row.get(3)?,
                        artifact: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sqlite)
    }

    fn receipt_row_by_id(
        &self,
        receipt_id: ReceiptId,
    ) -> Result<Option<RawReceiptRow>, StoreError> {
        self.connection
            .query_row(
                "SELECT receipt_id, proof_id, session_id, producer, artifact
                 FROM receipts WHERE receipt_id = ?1",
                params![receipt_id.as_bytes().to_vec()],
                |row| {
                    Ok(RawReceiptRow {
                        receipt_id: row.get(0)?,
                        proof_id: row.get(1)?,
                        session_id: row.get(2)?,
                        producer: row.get(3)?,
                        artifact: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sqlite)
    }

    pub(super) fn load_receipt(
        &mut self,
        key: ReceiptKey,
    ) -> Result<Option<StoredReceipt>, StoreError> {
        let Some(row) = self.receipt_row_by_key(key)? else {
            return Ok(None);
        };
        self.decode_stored_receipt(row, key).map(Some)
    }

    pub(super) fn load_receipt_by_id(
        &mut self,
        receipt_id: ReceiptId,
    ) -> Result<Option<StoredReceipt>, StoreError> {
        let Some(row) = self.receipt_row_by_id(receipt_id)? else {
            return Ok(None);
        };
        let key = ReceiptKey::new(
            SessionHash(array32(&row.session_id, "receipt session")?),
            peer_id_from_blob(&row.producer, "receipt producer")?,
        );
        let stored = self.decode_stored_receipt(row, key)?;
        if stored.receipt_id != receipt_id {
            return Err(StoreError::Corruption(
                "receipt id lookup returned a different artifact".into(),
            ));
        }
        Ok(Some(stored))
    }

    fn decode_stored_receipt(
        &mut self,
        row: RawReceiptRow,
        key: ReceiptKey,
    ) -> Result<StoredReceipt, StoreError> {
        let payload = open_envelope(
            EnvelopeKind::Receipt,
            &row.artifact,
            arena0_protocol::MAX_RECEIPT_BYTES,
        )?;
        let receipt = Receipt::decode(&payload)
            .map_err(|error| StoreError::Corruption(format!("receipt decode: {error}")))?;
        let stored_receipt_id = ReceiptId::from_bytes(array32(&row.receipt_id, "receipt id")?);
        let stored_proof = ProofId::from_bytes(array32(&row.proof_id, "receipt proof")?);
        let stored_session = SessionHash(array32(&row.session_id, "receipt session")?);
        let stored_producer = peer_id_from_blob(&row.producer, "receipt producer")?;
        if receipt.proof_id() != stored_proof
            || receipt.receipt_id() != stored_receipt_id
            || receipt.producer() != stored_producer
            || receipt.body().header().session_hash() != stored_session
            || receipt.key() != key
        {
            return Err(StoreError::Corruption(
                "receipt indexes do not match its artifact".into(),
            ));
        }

        let imported = self.receipt_import_exists(stored_receipt_id)?;
        let produced_execution = self.receipt_production(stored_receipt_id)?;
        if let Some(execution_id) = produced_execution {
            let state = self
                .load_execution(execution_id)?
                .ok_or(StoreError::ExecutionNotFound(execution_id))?;
            if state.binding().session_id() != stored_session
                || state.producer() != stored_producer
                || stored_producer != self.host_id
            {
                return Err(StoreError::Corruption(
                    "produced receipt is not bound to its Host execution".into(),
                ));
            }
        }

        Ok(StoredReceipt {
            key,
            proof_id: stored_proof,
            receipt_id: stored_receipt_id,
            provenance: ReceiptProvenance::from_facts(imported, produced_execution.is_some())?,
            receipt,
        })
    }

    fn receipt_import_exists(&self, receipt_id: ReceiptId) -> Result<bool, StoreError> {
        let exists = self
            .connection
            .query_row(
                "SELECT 1 FROM receipt_imports WHERE receipt_id = ?1",
                params![receipt_id.as_bytes().to_vec()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        Ok(exists.is_some())
    }

    fn receipt_production(&self, receipt_id: ReceiptId) -> Result<Option<ExecId>, StoreError> {
        let execution = self
            .connection
            .query_row(
                "SELECT execution_id FROM receipt_productions WHERE receipt_id = ?1",
                params![receipt_id.as_bytes().to_vec()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        execution
            .map(|bytes| Ok(ExecId(array32(&bytes, "receipt production execution")?)))
            .transpose()
    }

    pub(super) fn list_receipts(&mut self, limit: usize) -> Result<Vec<StoredReceipt>, StoreError> {
        let limit = i64::try_from(limit)
            .map_err(|_| StoreError::InvalidConfiguration("receipt limit is too large"))?;
        let mut statement = self.connection.prepare(
            "SELECT session_id, producer FROM receipts
             ORDER BY session_id, producer LIMIT ?1",
        )?;
        let mut rows = statement.query(params![limit])?;
        let mut keys = Vec::new();
        while let Some(row) = rows.next()? {
            keys.push(ReceiptKey::new(
                SessionHash(array32(&row.get::<_, Vec<u8>>(0)?, "receipt session")?),
                peer_id_from_blob(&row.get::<_, Vec<u8>>(1)?, "receipt producer")?,
            ));
        }
        drop(rows);
        drop(statement);
        let mut response_bytes = 0;
        keys.into_iter()
            .map(|key| {
                let receipt = self.load_receipt(key)?.ok_or_else(|| {
                    StoreError::Corruption("receipt disappeared while listing".into())
                })?;
                account_response(&mut response_bytes, receipt.receipt.encode()?.len())?;
                Ok(receipt)
            })
            .collect()
    }

    pub(super) fn import_receipt(
        &mut self,
        receipt: Receipt,
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
        receipt: Receipt,
        now_ms: u64,
    ) -> Result<ReceiptImportOutcome, StoreError> {
        let receipt_bytes = receipt
            .encode()
            .map_err(|error| StoreError::Corruption(format!("receipt encode: {error}")))?;
        // Re-decode at the storage boundary so the owner never persists a
        // typed value whose intrinsic IDs or receipt body invariants were not
        // checked by the protocol codec.
        let decoded = Receipt::decode(&receipt_bytes)
            .map_err(|error| StoreError::Corruption(format!("receipt decode: {error}")))?;
        if decoded != receipt {
            return Err(StoreError::Corruption(
                "receipt codec changed the artifact content".into(),
            ));
        }
        let key = receipt.key();
        let receipt_id = receipt.receipt_id();
        let proof_id = receipt.proof_id();
        let artifact = envelope(EnvelopeKind::Receipt, &receipt_bytes)?;

        if let Some(row) = self.receipt_row_by_id(receipt_id)? {
            let stored_payload = open_envelope(
                EnvelopeKind::Receipt,
                &row.artifact,
                arena0_protocol::MAX_RECEIPT_BYTES,
            )?;
            let stored = self.decode_stored_receipt(row, key)?;
            if stored_payload != receipt_bytes || stored.receipt != receipt {
                return Err(StoreError::Corruption(
                    "receipt identity is already bound to different content".into(),
                ));
            }
            let already_imported = stored.provenance.is_imported();
            if !already_imported {
                // Importing an artifact that was produced locally adds the
                // independent import fact. The production relation remains
                // untouched, so the derived provenance becomes `Both`.
                self.connection.execute(
                    "INSERT INTO receipt_imports (receipt_id, imported_at_ms)
                     VALUES (?1, ?2)",
                    params![receipt_id.as_bytes().to_vec(), sqlite_u64(now_ms)?],
                )?;
            }
            return Ok(if already_imported {
                ReceiptImportOutcome::AlreadyImported
            } else {
                ReceiptImportOutcome::AlreadyProduced
            });
        }

        if let Some(row) = self.receipt_row_by_key(key)? {
            // Validate the occupied row before reporting a key conflict. A
            // malformed pre-existing artifact is corruption, not a normal
            // import conflict.
            let _ = self.decode_stored_receipt(row, key)?;
            return Err(StoreError::Corruption(
                "receipt key is already bound to different content".into(),
            ));
        }

        self.connection.execute(
            "INSERT INTO receipts
             (receipt_id, proof_id, session_id, producer, artifact, stored_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                receipt_id.as_bytes().to_vec(),
                proof_id.as_bytes().to_vec(),
                key.session_id().0.to_vec(),
                key.producer().0.to_vec(),
                artifact,
                sqlite_u64(now_ms)?,
            ],
        )?;
        self.connection.execute(
            "INSERT INTO receipt_imports (receipt_id, imported_at_ms)
             VALUES (?1, ?2)",
            params![receipt_id.as_bytes().to_vec(), sqlite_u64(now_ms)?],
        )?;
        Ok(ReceiptImportOutcome::Imported)
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
        let receipt_bytes = receipt.encode()?;
        let proof_id = receipt.proof_id();
        let receipt_id = receipt.receipt_id();
        if receipt.producer() != self.host_id {
            return Err(StoreError::IdentityMismatch {
                database: self.host_id,
                requested: receipt.producer(),
            });
        }
        let publication_bytes = borsh::to_vec(terminal).map_err(|error| {
            StoreError::Corruption(format!("terminal publication encode: {error}"))
        })?;
        let body = receipt.body();
        let session_id = body.header().session_hash();
        self.insert_or_check_receipt(execution_id, session_id, receipt, &receipt_bytes, now_ms)?;
        self.connection.execute(
            "INSERT INTO terminal_proofs
             (execution_id, version, proof_id, receipt_id, publication)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                execution_id.0.to_vec(),
                sqlite_u64(version.get())?,
                proof_id.as_bytes().to_vec(),
                receipt_id.as_bytes().to_vec(),
                envelope(EnvelopeKind::TerminalPublication, &publication_bytes)?,
            ],
        )?;
        Ok(())
    }

    pub(super) fn insert_or_check_receipt(
        &mut self,
        execution_id: ExecId,
        session_id: SessionHash,
        receipt: &Receipt,
        receipt_bytes: &[u8],
        now_ms: u64,
    ) -> Result<(), StoreError> {
        let receipt_id = receipt.receipt_id();
        let proof_id = receipt.proof_id();
        let key = ReceiptKey::new(session_id, receipt.producer());
        let receipt_payload = Receipt::decode(receipt_bytes)
            .map_err(|error| StoreError::Corruption(format!("receipt decode: {error}")))?;
        if receipt_payload != *receipt
            || receipt_payload.receipt_id() != receipt_id
            || receipt_payload.proof_id() != proof_id
        {
            return Err(StoreError::Corruption(
                "receipt artifact failed intrinsic content validation".into(),
            ));
        }

        if let Some(row) = self.receipt_row_by_id(receipt_id)? {
            let stored_payload = open_envelope(
                EnvelopeKind::Receipt,
                &row.artifact,
                arena0_protocol::MAX_RECEIPT_BYTES,
            )?;
            let stored_receipt = Receipt::decode(&stored_payload).map_err(|error| {
                StoreError::Corruption(format!("stored receipt decode: {error}"))
            })?;
            let same = stored_payload == receipt_bytes
                && stored_receipt == *receipt
                && row.receipt_id == receipt_id.as_bytes().to_vec()
                && row.proof_id == proof_id.as_bytes().to_vec()
                && row.session_id == session_id.0.to_vec()
                && row.producer == receipt.producer().0.to_vec();
            if !same {
                return Err(StoreError::Corruption(
                    "receipt identity is already bound to different content".into(),
                ));
            }
            if let Some(stored_execution) = self.receipt_production(receipt_id)? {
                if stored_execution != execution_id {
                    return Err(StoreError::Corruption(
                        "receipt is already bound to a different execution".into(),
                    ));
                }
                return Ok(());
            }
            // Local publication adds a production fact. If an import fact is
            // already present, it remains untouched and the derived
            // provenance becomes `Both`.
            self.connection.execute(
                "INSERT INTO receipt_productions (receipt_id, execution_id)
                 VALUES (?1, ?2)",
                params![receipt_id.as_bytes().to_vec(), execution_id.0.to_vec()],
            )?;
            return Ok(());
        } else if self.receipt_row_by_key(key)?.is_some() {
            // The producer may seal only one receipt for a session. Check this
            // identity before the insert so a different receipt does not
            // escape as an untyped SQLite UNIQUE violation.
            return Err(StoreError::Corruption(
                "receipt key is already bound to another receipt".into(),
            ));
        } else {
            self.connection.execute(
                "INSERT INTO receipts
                 (receipt_id, proof_id, session_id, producer, artifact, stored_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    receipt_id.as_bytes().to_vec(),
                    proof_id.as_bytes().to_vec(),
                    session_id.0.to_vec(),
                    receipt.producer().0.to_vec(),
                    envelope(EnvelopeKind::Receipt, receipt_bytes)?,
                    sqlite_u64(now_ms)?,
                ],
            )?;
        }
        self.connection.execute(
            "INSERT INTO receipt_productions (receipt_id, execution_id)
             VALUES (?1, ?2)",
            params![receipt_id.as_bytes().to_vec(), execution_id.0.to_vec(),],
        )?;
        Ok(())
    }

    pub(super) fn validate_terminal_rows(
        &mut self,
        state: &ExecutionState,
    ) -> Result<(), StoreError> {
        let execution_id = state.execution_id();
        let terminal_count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM terminal_proofs WHERE execution_id = ?1",
            params![execution_id.0.to_vec()],
            |row| row.get(0),
        )?;
        let receipt_count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM receipts
             INNER JOIN receipt_productions
                ON receipt_productions.receipt_id = receipts.receipt_id
             WHERE receipt_productions.execution_id = ?1",
            params![execution_id.0.to_vec()],
            |row| row.get(0),
        )?;
        let published = state.published_terminal_ids().is_some();
        let expected = i64::from(published);
        if terminal_count != expected {
            return Err(StoreError::Corruption(format!(
                "execution {execution_id} has {terminal_count} terminal proof rows, expected {expected}"
            )));
        }
        if receipt_count != expected {
            return Err(StoreError::Corruption(format!(
                "execution {execution_id} has {receipt_count} receipt rows, expected {expected}"
            )));
        }

        let mut statement = self.connection.prepare(
            "SELECT version, proof_id, receipt_id, publication FROM terminal_proofs
             WHERE execution_id = ?1 ORDER BY version",
        )?;
        let mut rows = statement.query(params![state.execution_id().0.to_vec()])?;
        let mut terminal = None;
        while let Some(row) = rows.next()? {
            let version = sqlite_i64(row.get::<_, i64>(0)?)?;
            if version == 0 || version != state.version().get() {
                return Err(StoreError::Corruption(
                    "terminal proof version does not match execution state".into(),
                ));
            }
            let proof_id =
                ProofId::from_bytes(array32(&row.get::<_, Vec<u8>>(1)?, "terminal proof id")?);
            let receipt_id =
                ReceiptId::from_bytes(array32(&row.get::<_, Vec<u8>>(2)?, "terminal receipt id")?);
            let payload = open_envelope(
                EnvelopeKind::TerminalPublication,
                &row.get::<_, Vec<u8>>(3)?,
                arena0_protocol::MAX_RECEIPT_BYTES,
            )?;
            let publication: arena0_protocol::TerminalPublication =
                decode_borsh(&payload, "terminal publication")?;
            let receipt = publication.receipt();
            if receipt.proof_id() != proof_id || receipt.receipt_id() != receipt_id {
                return Err(StoreError::Corruption(
                    "terminal proof indexes do not match publication".into(),
                ));
            }
            if receipt.body().header().session_hash() != state.binding().session_id()
                || receipt.producer() != self.host_id
            {
                return Err(StoreError::Corruption(
                    "terminal receipt is not execution-bound".into(),
                ));
            }
            terminal = Some((version, proof_id, receipt_id, receipt.clone()));
        }

        drop(rows);
        drop(statement);

        let Some((state_proof_id, state_receipt_id, state_producer)) =
            state.published_terminal_ids()
        else {
            debug_assert!(terminal.is_none());
            return Ok(());
        };
        let Some((version, proof_id, receipt_id, terminal_receipt)) = terminal else {
            return Err(StoreError::Corruption(
                "published execution has no terminal proof row".into(),
            ));
        };
        if version != state.version().get()
            || proof_id != state_proof_id
            || receipt_id != state_receipt_id
            || terminal_receipt.producer() != state_producer
        {
            return Err(StoreError::Corruption(
                "terminal proof does not match published execution state".into(),
            ));
        }

        let receipt_row = self
            .connection
            .query_row(
                "SELECT receipts.receipt_id, receipts.proof_id, receipt_productions.execution_id,
                        receipts.session_id, receipts.producer, receipts.artifact
                 FROM receipts
                 INNER JOIN receipt_productions
                    ON receipt_productions.receipt_id = receipts.receipt_id
                 WHERE receipt_productions.execution_id = ?1",
                params![execution_id.0.to_vec()],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, Vec<u8>>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::Corruption("published execution receipt row is missing".into())
            })?;
        let row_receipt_id = ReceiptId::from_bytes(array32(&receipt_row.0, "receipt id")?);
        let row_proof_id = ProofId::from_bytes(array32(&receipt_row.1, "receipt proof id")?);
        let row_execution_id = ExecId(array32(&receipt_row.2, "receipt execution")?);
        let row_session_id = SessionHash(array32(&receipt_row.3, "receipt session")?);
        let row_producer = peer_id_from_blob(&receipt_row.4, "receipt producer")?;
        if row_execution_id != execution_id
            || row_receipt_id != state_receipt_id
            || row_proof_id != state_proof_id
            || row_session_id != state.binding().session_id()
            || row_producer != state_producer
        {
            return Err(StoreError::Corruption(
                "receipt row does not match published execution state".into(),
            ));
        }
        let receipt_payload = open_envelope(
            EnvelopeKind::Receipt,
            &receipt_row.5,
            arena0_protocol::MAX_RECEIPT_BYTES,
        )?;
        let indexed_receipt = Receipt::decode(&receipt_payload)
            .map_err(|error| StoreError::Corruption(format!("receipt decode: {error}")))?;
        if indexed_receipt.receipt_id() != state_receipt_id
            || indexed_receipt.proof_id() != state_proof_id
            || indexed_receipt.producer() != state_producer
            || indexed_receipt.body().header().session_hash() != state.binding().session_id()
            || indexed_receipt != terminal_receipt
        {
            return Err(StoreError::Corruption(
                "receipt artifact does not match published execution state".into(),
            ));
        }

        Ok(())
    }

    pub(super) fn validate_receipts(&mut self) -> Result<(), StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT receipt_id, proof_id, session_id, producer, artifact
             FROM receipts ORDER BY receipt_id",
        )?;
        let mut rows = statement.query([])?;
        let mut values = Vec::new();
        while let Some(row) = rows.next()? {
            values.push((
                array32(&row.get::<_, Vec<u8>>(0)?, "receipt id")?,
                array32(&row.get::<_, Vec<u8>>(1)?, "receipt proof id")?,
                SessionHash(array32(&row.get::<_, Vec<u8>>(2)?, "receipt session")?),
                peer_id_from_blob(&row.get::<_, Vec<u8>>(3)?, "receipt producer")?,
                row.get::<_, Vec<u8>>(4)?,
            ));
        }
        drop(rows);
        drop(statement);
        for (receipt_id, proof_id, session_id, producer, artifact) in values {
            let payload = open_envelope(
                EnvelopeKind::Receipt,
                &artifact,
                arena0_protocol::MAX_RECEIPT_BYTES,
            )?;
            let receipt = Receipt::decode(&payload)
                .map_err(|error| StoreError::Corruption(format!("receipt decode: {error}")))?;
            if receipt.receipt_id() != ReceiptId::from_bytes(receipt_id)
                || receipt.proof_id() != ProofId::from_bytes(proof_id)
                || receipt.producer() != producer
                || receipt.body().header().session_hash() != session_id
            {
                return Err(StoreError::Corruption("receipt index mismatch".into()));
            }
            let imported = self.receipt_import_exists(ReceiptId::from_bytes(receipt_id))?;
            let produced_execution = self.receipt_production(ReceiptId::from_bytes(receipt_id))?;
            let _provenance =
                ReceiptProvenance::from_facts(imported, produced_execution.is_some())?;
            if let Some(execution_id) = produced_execution {
                let state = self
                    .load_execution(execution_id)?
                    .ok_or(StoreError::ExecutionNotFound(execution_id))?;
                if state.binding().session_id() != session_id
                    || state.producer() != producer
                    || producer != self.host_id
                {
                    return Err(StoreError::Corruption(
                        "produced receipt is not bound to its Host execution".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}
