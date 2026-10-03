//! Read connections and the summary reads they serve.
//!
//! A `ReadDb` owns one read-only SQLite connection to the same WAL database as
//! the writer (`SQLITE_OPEN_READ_ONLY`, `PRAGMA query_only = ON`). WAL readers
//! see the last committed snapshot and never block, or wait for, the
//! execution actor's write transactions. Every method here reads index
//! columns only (see `crate::summary`): no envelope is opened and no protocol
//! value is decoded, so no method may call into `codec::open_envelope`,
//! `ExecutionState::decode`, `ReceiptArtifact::decode` or activation decoding.

use super::*;
use crate::summary::{ExecSummaryRow, ReceiptSummaryRow};

pub(crate) struct ReadDb {
    pub(super) connection: Connection,
}

const EXEC_SUMMARY_SELECT: &str =
    "SELECT q.execution_id, q.program_hash, q.negotiation_id, q.failure, q.created_at_ms,
       a.status, a.session_id, a.updated_at_ms, a.facts,
       e.session_id, e.lifecycle, e.agreed_step, e.last_step_at_ms, e.end_phase, e.end_unconfirmed,
       e.participants, e.participant_ids, e.callout_id, e.callout_index, e.callout_opened_at_ms,
       e.terminal_reason, e.outcome_json,
       EXISTS (SELECT 1 FROM receipts r JOIN receipt_productions p USING (receipt_id)
               WHERE r.session_id = e.session_id), e.updated_at_ms
FROM exec_requests q LEFT JOIN activation_records a USING (execution_id)
LEFT JOIN executions e USING (execution_id)";

const RECEIPT_SUMMARY_SELECT: &str =
    "SELECT receipt_id, session_id, kind, program_hash, completed, stored_at_ms,
            EXISTS (SELECT 1 FROM receipt_imports i WHERE i.receipt_id = r.receipt_id),
            EXISTS (SELECT 1 FROM receipt_productions p WHERE p.receipt_id = r.receipt_id)
     FROM receipts r";

fn receipt_summary_row(row: &rusqlite::Row<'_>) -> Result<ReceiptSummaryRow, StoreError> {
    let kind = match row.get::<_, String>(2)?.as_str() {
        "receipt" => arena0_protocol::ReceiptKind::Receipt,
        "stop_report" => arena0_protocol::ReceiptKind::StopReport,
        _ => return Err(StoreError::Corruption("unknown receipt kind".into())),
    };
    Ok(ReceiptSummaryRow {
        receipt_id: ReceiptId::from_bytes(array32(&row.get::<_, Vec<u8>>(0)?, "receipt id")?),
        session_id: SessionHash(array32(&row.get::<_, Vec<u8>>(1)?, "receipt session")?),
        kind,
        program_hash: ProgramHash(array32(&row.get::<_, Vec<u8>>(3)?, "program hash")?),
        completed: row.get(4)?,
        provenance: ReceiptProvenance::from_facts(row.get(6)?, row.get(7)?).ok_or_else(|| {
            StoreError::Corruption("receipt artifact has no provenance fact".into())
        })?,
        stored_at_ms: sqlite_i64(row.get(5)?)?,
    })
}

fn exec_summary_row(row: &rusqlite::Row<'_>) -> Result<ExecSummaryRow, StoreError> {
    let activation = row
        .get::<_, Option<String>>(5)?
        .map(|status| {
            Ok::<_, StoreError>(ActivationIndex {
                committed: ActivationRecordStatus::parse(&status)?
                    == ActivationRecordStatus::Committed,
                session_id: SessionHash(array32(&row.get::<_, Vec<u8>>(6)?, "activation session")?),
                updated_at_ms: sqlite_i64(row.get(7)?)?,
                facts: decode_borsh(&row.get::<_, Vec<u8>>(8)?, "activation facts")?,
            })
        })
        .transpose()?;
    let execution = row
        .get::<_, Option<Vec<u8>>>(9)?
        .map(|session| {
            let callout = match (
                row.get::<_, Option<i64>>(17)?,
                row.get::<_, Option<i64>>(18)?,
                row.get::<_, Option<i64>>(19)?,
            ) {
                (None, None, None) => None,
                (Some(id), Some(index), Some(opened)) => Some(CalloutIndex {
                    id: arena0_protocol::CalloutId::new(u64::from_le_bytes(id.to_le_bytes())),
                    callout_index: u32::try_from(index)
                        .map_err(|_| StoreError::Corruption("invalid callout index".into()))?,
                    opened_at_ms: sqlite_i64(opened)?,
                }),
                _ => {
                    return Err(StoreError::Corruption(
                        "callout columns disagree on presence".into(),
                    ));
                }
            };
            Ok::<_, StoreError>(ExecutionIndex {
                session_id: SessionHash(array32(&session, "execution session")?),
                lifecycle: lifecycle_from_tag(row.get(10)?)?,
                agreed_step: sqlite_i64(row.get(11)?)?,
                last_step_at_ms: row.get::<_, Option<i64>>(12)?.map(sqlite_i64).transpose()?,
                end: decode_end(row.get(13)?, &row.get::<_, Vec<u8>>(14)?)?,
                participants: usize::try_from(sqlite_i64(row.get(15)?)?).map_err(|_| {
                    StoreError::Corruption("participant count exceeds usize".into())
                })?,
                participant_ids: decode_borsh(&row.get::<_, Vec<u8>>(16)?, "participant index")?,
                callout,
                terminal_reason: row.get(20)?,
                outcome_json: row.get(21)?,
                receipt_produced: row.get(22)?,
                updated_at_ms: sqlite_i64(row.get(23)?)?,
            })
        })
        .transpose()?;
    let summary = ExecSummaryRow {
        execution_id: ExecId(array32(&row.get::<_, Vec<u8>>(0)?, "execution id")?),
        program_hash: ProgramHash(array32(&row.get::<_, Vec<u8>>(1)?, "program hash")?),
        negotiation_id: row
            .get::<_, Option<Vec<u8>>>(2)?
            .map(|bytes| array32(&bytes, "negotiation id").map(arena0_protocol::NegotiationId))
            .transpose()?,
        request_failure: row.get(3)?,
        created_at_ms: sqlite_i64(row.get(4)?)?,
        activation,
        execution,
    };
    Ok(summary)
}

impl ReadDb {
    /// Open a read-only connection to the database at `path`, which the
    /// writer has already opened and migrated.
    pub(crate) fn open(path: &Path, busy_timeout: Duration) -> Result<Self, StoreError> {
        let connection = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(busy_timeout)?;
        connection.pragma_update(None, "query_only", "ON")?;
        Ok(Self { connection })
    }

    /// Every execution request in `created_order`, at most `limit`, with its
    /// activation and execution index columns (LEFT JOINs).
    pub(crate) fn list_exec_summaries(
        &self,
        limit: usize,
    ) -> Result<Vec<ExecSummaryRow>, StoreError> {
        let limit = i64::try_from(limit)
            .map_err(|_| StoreError::InvalidConfiguration("execution limit is too large"))?;
        let mut statement = self.connection.prepare(&format!(
            "{EXEC_SUMMARY_SELECT} ORDER BY q.created_order LIMIT ?1"
        ))?;
        let mut rows = statement.query(params![limit])?;
        let mut summaries = Vec::new();
        let mut response_bytes = 0;
        while let Some(row) = rows.next()? {
            let summary = exec_summary_row(row)?;
            account_response(
                &mut response_bytes,
                summary.request_failure.as_ref().map_or(0, String::len)
                    + summary.activation.as_ref().map_or(0, |a| {
                        a.facts.params.len() + a.facts.participants.len() * 64
                    })
                    + summary.execution.as_ref().map_or(0, |e| {
                        e.outcome_json.as_ref().map_or(0, Vec::len)
                            + e.terminal_reason.as_ref().map_or(0, String::len)
                            + e.participant_ids.len() * 32
                    }),
            )?;
            summaries.push(summary);
        }
        Ok(summaries)
    }

    /// One execution's summary row; `None` when no request exists.
    pub(crate) fn exec_summary(
        &self,
        execution_id: ExecId,
    ) -> Result<Option<ExecSummaryRow>, StoreError> {
        let mut statement = self
            .connection
            .prepare(&format!("{EXEC_SUMMARY_SELECT} WHERE q.execution_id = ?1"))?;
        let mut rows = statement.query(params![execution_id.0.to_vec()])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        exec_summary_row(row).map(Some)
    }

    /// Executions whose lifecycle column is not terminal.
    pub(crate) fn count_active_executions(&self) -> Result<usize, StoreError> {
        let terminal = [
            ExecLifecycle::Completed,
            ExecLifecycle::Aborted,
            ExecLifecycle::Failed,
        ];
        let count: i64 = self.connection.query_row(
            "SELECT count(*) FROM executions WHERE lifecycle NOT IN (?1, ?2, ?3)",
            params![
                lifecycle_tag(terminal[0]),
                lifecycle_tag(terminal[1]),
                lifecycle_tag(terminal[2])
            ],
            |row| row.get(0),
        )?;
        usize::try_from(count)
            .map_err(|_| StoreError::Corruption("execution count exceeds usize".into()))
    }

    /// Programs with `removed_at_ms IS NULL`.
    pub(crate) fn count_programs(&self) -> Result<usize, StoreError> {
        let count: i64 = self.connection.query_row(
            "SELECT count(*) FROM programs WHERE removed_at_ms IS NULL",
            [],
            |row| row.get(0),
        )?;
        usize::try_from(count)
            .map_err(|_| StoreError::Corruption("program count exceeds usize".into()))
    }

    /// Receipts in `receipt_id` order, at most `limit`, provenance from
    /// `receipt_imports`/`receipt_productions` by `EXISTS`.
    pub(crate) fn list_receipt_summaries(
        &self,
        limit: usize,
    ) -> Result<Vec<ReceiptSummaryRow>, StoreError> {
        let limit = i64::try_from(limit)
            .map_err(|_| StoreError::InvalidConfiguration("receipt limit is too large"))?;
        let mut statement = self.connection.prepare(&format!(
            "{RECEIPT_SUMMARY_SELECT} ORDER BY receipt_id LIMIT ?1"
        ))?;
        let mut rows = statement.query(params![limit])?;
        let mut receipts = Vec::new();
        while let Some(row) = rows.next()? {
            receipts.push(receipt_summary_row(row)?);
        }
        Ok(receipts)
    }

    pub(crate) fn receipt_summary(
        &self,
        id: ReceiptId,
    ) -> Result<Option<ReceiptSummaryRow>, StoreError> {
        let mut statement = self
            .connection
            .prepare(&format!("{RECEIPT_SUMMARY_SELECT} WHERE receipt_id = ?1"))?;
        let mut rows = statement.query(params![id.as_bytes().to_vec()])?;
        rows.next()?.map(receipt_summary_row).transpose()
    }

    pub(crate) fn list_step_times(
        &self,
        exec_id: ExecId,
        from: u64,
    ) -> Result<StepTimesRow, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT step, certified_at_ms, post_state FROM agreed_steps WHERE execution_id = ?1 AND step >= ?2 ORDER BY step")?;
        let mut rows = statement.query(params![exec_id.0.to_vec(), sqlite_u64(from)?])?;
        let mut times = StepTimesRow {
            exec_id,
            from_step: from,
            certified_at_ms: Vec::new(),
            post_state: Vec::new(),
        };
        let mut expected = from;
        let mut response_bytes = 0;
        while let Some(row) = rows.next()? {
            if sqlite_i64(row.get(0)?)? != expected {
                return Err(StoreError::Corruption(
                    "agreed step times are not gapless".into(),
                ));
            }
            account_response(&mut response_bytes, 40)?;
            times.certified_at_ms.push(sqlite_i64(row.get(1)?)?);
            times.post_state.push(arena0_protocol::StateHash(array32(
                &row.get::<_, Vec<u8>>(2)?,
                "step post-state",
            )?));
            expected += 1;
        }
        Ok(times)
    }
}
