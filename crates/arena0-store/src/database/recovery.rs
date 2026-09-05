use super::*;

impl Database {
    pub(super) fn list_recovery_candidates(
        &mut self,
        cursor: RecoveryCursor,
        limit: usize,
    ) -> Result<RecoveryPage, StoreError> {
        if limit == 0 {
            return Ok(RecoveryPage::new(Vec::new(), None));
        }
        let after = sqlite_u64(cursor.position())?;
        let requested_limit = limit;
        let limit = i64::try_from(limit)
            .map_err(|_| StoreError::InvalidConfiguration("recovery limit is too large"))?;
        let mut statement = self.connection.prepare(
            "SELECT r.execution_id, r.created_order, p.program_hash,
                    a.status, a.session_id, e.execution_id
             FROM exec_requests AS r
             LEFT JOIN activation_records AS a ON a.execution_id = r.execution_id
             LEFT JOIN executions AS e ON e.execution_id = r.execution_id
             LEFT JOIN programs AS p ON p.program_hash = r.program_hash
             WHERE r.created_order > ?1
               AND r.failure IS NULL
               AND (e.execution_id IS NULL OR e.lifecycle IN (0, 1, 2, 3)
                    OR (e.lifecycle IN (5, 7) AND NOT EXISTS (
                        SELECT 1 FROM receipt_productions AS p
                        WHERE p.execution_id = r.execution_id))
                    OR (e.lifecycle <> 6 AND EXISTS (SELECT 1 FROM outbox AS o
                        WHERE o.execution_id = r.execution_id
                          AND o.status <> 'acknowledged')))
             ORDER BY r.created_order ASC
             LIMIT ?2",
        )?;
        let mut rows = statement.query(params![after, limit])?;
        let mut rows_to_load = Vec::new();
        while let Some(row) = rows.next()? {
            let execution_id = ExecId(array32(
                &row.get::<_, Vec<u8>>(0)?,
                "recovery execution id",
            )?);
            let created_order = sqlite_i64(row.get::<_, i64>(1)?)?;
            let program = row
                .get::<_, Option<Vec<u8>>>(2)?
                .map(|bytes| array32(&bytes, "recovery program hash"))
                .transpose()?
                .map(ProgramHash);
            let activation_status = row
                .get::<_, Option<String>>(3)?
                .map(|status| parse_activation_status(&status))
                .transpose()?;
            let session_id = row
                .get::<_, Option<Vec<u8>>>(4)?
                .map(|bytes| array32(&bytes, "recovery activation session"))
                .transpose()?
                .map(SessionHash);
            let execution_present = row.get::<_, Option<Vec<u8>>>(5)?.is_some();
            if activation_status.is_some() != session_id.is_some() {
                return Err(StoreError::Corruption(
                    "recovery activation metadata is incomplete".into(),
                ));
            }
            rows_to_load.push((
                execution_id,
                RecoveryCursor::from_position(created_order),
                program,
                activation_status,
                session_id,
                execution_present,
            ));
        }
        drop(rows);
        drop(statement);

        let mut response_bytes = 0;
        let mut candidates = Vec::with_capacity(rows_to_load.len());
        for (
            execution_id,
            candidate_cursor,
            program,
            activation_status,
            session_id,
            execution_present,
        ) in rows_to_load
        {
            let request = self
                .load_execution_request_in_transaction(execution_id)?
                .ok_or_else(|| {
                    StoreError::Corruption("recovery request disappeared while listing".into())
                })?;
            if request.created_order() != candidate_cursor.position() {
                return Err(StoreError::Corruption(
                    "recovery cursor does not match request creation order".into(),
                ));
            }
            if request.failure().is_some() {
                return Err(StoreError::Corruption(
                    "terminal request appeared in recovery projection".into(),
                ));
            }
            if program.is_some_and(|program| program != request.program_hash()) {
                return Err(StoreError::Corruption(
                    "recovery program join does not match request".into(),
                ));
            }
            // Recovery pages contain only bounded request and row metadata.
            // Activation and execution payloads can each approach their
            // independent 16 MiB storage bound, while this response remains
            // safely below the store's 8 MiB page limit. The daemon loads and
            // validates those aggregates one at a time before registering a
            // live handle.
            account_response(&mut response_bytes, 256)?;
            if let Some(params) = request.params() {
                account_response(&mut response_bytes, params.len())?;
            }
            candidates.push(RecoveryCandidate {
                cursor: candidate_cursor,
                request,
                activation_status,
                session_id,
                execution_present,
                program,
            });
        }

        let next = (candidates.len() == requested_limit)
            .then(|| candidates.last().map(RecoveryCandidate::cursor))
            .flatten();
        Ok(RecoveryPage::new(candidates, next))
    }
}
