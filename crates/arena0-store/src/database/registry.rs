use super::*;
use arena0_crypto::CryptoError;
use zeroize::Zeroizing;

impl Database {
    pub(super) fn load_or_create_execution_salt(
        &mut self,
        execution_id: ExecId,
        now_ms: u64,
    ) -> Result<ExecutionSalt, StoreError> {
        self.begin()?;
        let result = (|| {
            let request_exists: bool = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM exec_requests WHERE execution_id = ?1)",
                params![execution_id.0.to_vec()],
                |row| row.get(0),
            )?;
            if !request_exists {
                return Err(StoreError::ExecutionRequestNotFound(execution_id));
            }
            let existing = self
                .connection
                .query_row(
                    "SELECT salt FROM execution_salts WHERE execution_id = ?1",
                    params![execution_id.0.to_vec()],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()?;
            if let Some(encoded) = existing {
                let encoded = Zeroizing::new(encoded);
                return decode_execution_salt(&encoded);
            }
            let salt = loop {
                match ExecutionSalt::try_from_bytes(rand::random()) {
                    Ok(salt) => break salt,
                    Err(CryptoError::ZeroExecutionSalt) => continue,
                    Err(error) => {
                        return Err(StoreError::Corruption(format!(
                            "generated execution salt is invalid: {error}"
                        )));
                    }
                }
            };
            let encoded = envelope(EnvelopeKind::ExecutionSalt, salt.as_bytes())?;
            self.connection.execute(
                "INSERT INTO execution_salts (execution_id, salt, created_at_ms)
                 VALUES (?1, ?2, ?3)",
                params![execution_id.0.to_vec(), encoded, sqlite_u64(now_ms)?,],
            )?;
            Ok(salt)
        })();
        match result {
            Ok(salt) => self.commit_result(salt),
            Err(error) => self.rollback_result(error),
        }
    }

    pub(super) fn register_program(
        &mut self,
        hash: ProgramHash,
        wasm: Vec<u8>,
        now_ms: u64,
    ) -> Result<ProgramStoreOutcome, StoreError> {
        let max = max_program_bytes()?;
        if wasm.is_empty() || wasm.len() > max || ProgramHash::of(&wasm) != hash {
            return Err(StoreError::Corruption(
                "program bytes do not match their content address or bound".into(),
            ));
        }
        let existing = self
            .connection
            .query_row(
                "SELECT wasm, removed_at_ms FROM programs WHERE program_hash = ?1",
                params![hash.as_bytes().to_vec()],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .optional()?;
        if let Some((stored, removed_at_ms)) = existing {
            let payload = open_envelope(EnvelopeKind::Program, &stored, max)?;
            if ProgramHash::of(&payload) != hash {
                return Err(StoreError::Corruption(
                    "program hash does not match its envelope".into(),
                ));
            }
            if payload != wasm {
                return Ok(ProgramStoreOutcome::Conflict);
            }
            if removed_at_ms.is_some() {
                self.connection.execute(
                    "UPDATE programs SET removed_at_ms = NULL WHERE program_hash = ?1",
                    params![hash.as_bytes().to_vec()],
                )?;
                return Ok(ProgramStoreOutcome::Reactivated);
            } else {
                return Ok(ProgramStoreOutcome::AlreadyStored);
            }
        }
        self.connection.execute(
            "INSERT INTO programs (program_hash, wasm, imported_at_ms, removed_at_ms)
             VALUES (?1, ?2, ?3, NULL)",
            params![
                hash.as_bytes().to_vec(),
                envelope(EnvelopeKind::Program, &wasm)?,
                sqlite_u64(now_ms)?,
            ],
        )?;
        Ok(ProgramStoreOutcome::Stored)
    }

    pub(super) fn load_program(
        &mut self,
        hash: ProgramHash,
    ) -> Result<Option<StoredProgram>, StoreError> {
        let Some(encoded) = self
            .connection
            .query_row(
                "SELECT wasm FROM programs WHERE program_hash = ?1",
                params![hash.as_bytes().to_vec()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let wasm = open_envelope(EnvelopeKind::Program, &encoded, max_program_bytes()?)?;
        if ProgramHash::of(&wasm) != hash {
            return Err(StoreError::Corruption(
                "program registry indexes do not match Wasm content".into(),
            ));
        }
        Ok(Some(StoredProgram { hash, wasm }))
    }

    pub(super) fn list_programs(&mut self, limit: usize) -> Result<Vec<ProgramHash>, StoreError> {
        let limit = i64::try_from(limit)
            .map_err(|_| StoreError::InvalidConfiguration("program limit is too large"))?;
        let mut statement = self.connection.prepare(
            "SELECT program_hash FROM programs WHERE removed_at_ms IS NULL
             ORDER BY program_hash LIMIT ?1",
        )?;
        let mut rows = statement.query(params![limit])?;
        let mut hashes = Vec::new();
        let mut response_bytes = 0;
        while let Some(row) = rows.next()? {
            account_response(&mut response_bytes, 40)?;
            hashes.push(ProgramHash(array32(
                &row.get::<_, Vec<u8>>(0)?,
                "program hash",
            )?));
        }
        Ok(hashes)
    }

    pub(super) fn remove_program(
        &mut self,
        hash: ProgramHash,
        now_ms: u64,
    ) -> Result<ProgramRemoveOutcome, StoreError> {
        let existing = self
            .connection
            .query_row(
                "SELECT removed_at_ms FROM programs WHERE program_hash = ?1",
                params![hash.as_bytes().to_vec()],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?;
        let Some(removed_at_ms) = existing else {
            return Err(StoreError::Corruption(
                "cannot unregister an unknown program hash".into(),
            ));
        };
        if removed_at_ms.is_some() {
            return Ok(ProgramRemoveOutcome::AlreadyRemoved);
        }
        self.connection.execute(
            "UPDATE programs SET removed_at_ms = ?1 WHERE program_hash = ?2 AND removed_at_ms IS NULL",
            params![sqlite_u64(now_ms)?, hash.as_bytes().to_vec()],
        )?;
        Ok(ProgramRemoveOutcome::Removed)
    }

    pub(super) fn validate_execution_salts(&mut self) -> Result<(), StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT execution_id, salt FROM execution_salts ORDER BY execution_id")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let _execution_id = ExecId(array32(&row.get::<_, Vec<u8>>(0)?, "salt execution")?);
            decode_execution_salt(&row.get::<_, Vec<u8>>(1)?)?;
        }
        Ok(())
    }

    pub(super) fn validate_programs(&mut self) -> Result<(), StoreError> {
        let max = max_program_bytes()?;
        let mut statement = self
            .connection
            .prepare("SELECT program_hash, wasm FROM programs ORDER BY program_hash")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let hash = ProgramHash(array32(&row.get::<_, Vec<u8>>(0)?, "program hash")?);
            let wasm = open_envelope(EnvelopeKind::Program, &row.get::<_, Vec<u8>>(1)?, max)?;
            if wasm.is_empty() || wasm.len() > max || ProgramHash::of(&wasm) != hash {
                return Err(StoreError::Corruption(
                    "program registry row failed content validation".into(),
                ));
            }
        }
        Ok(())
    }
}
