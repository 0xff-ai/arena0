//! Indexed id-prefix lookup for `resolve` (design §3.4).

use super::*;

/// What an id prefix is matched against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdSpace {
    /// `exec_requests.execution_id`.
    Exec,
    /// Session ids of committed activations, execution aggregates and
    /// receipts, deduplicated.
    Session,
    /// `receipts.receipt_id`.
    Receipt,
}

/// Ids matching one prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdMatches {
    /// The first `limit` matching ids in ascending byte order.
    pub ids: Vec<[u8; 32]>,
    /// How many ids match in total.
    pub total: u64,
}

impl ReadDb {
    /// Ids in `space` whose lowercase hex starts with `prefix` (1..=64
    /// lowercase hex characters; the caller validates). Uses a primary-key or
    /// index range scan, never a full-table hex comparison: the prefix maps to
    /// the byte range `[lo, hi)` (an odd final nibble widens to its 16-value
    /// range). Reads index columns only.
    pub(crate) fn resolve_ids(
        &self,
        space: IdSpace,
        prefix: &str,
        limit: usize,
    ) -> Result<IdMatches, StoreError> {
        let mut lo = [0u8; 32];
        for (i, nibble) in prefix.bytes().enumerate() {
            let value = match nibble {
                b'0'..=b'9' => nibble - b'0',
                b'a'..=b'f' => nibble - b'a' + 10,
                _ => unreachable!("caller validates lowercase hex"),
            };
            lo[i / 2] |= value << (if i.is_multiple_of(2) { 4 } else { 0 });
        }
        // Increment the final constrained nibble, carrying towards the most
        // significant byte. An all-f prefix has no finite upper bound.
        let mut hi = lo;
        let mut carry = if prefix.len().is_multiple_of(2) {
            1u16
        } else {
            16u16
        };
        for byte in hi[..prefix.len().div_ceil(2)].iter_mut().rev() {
            let sum = u16::from(*byte) + carry;
            *byte = sum as u8;
            carry = sum >> 8;
        }
        let upper = if carry == 0 {
            Some(hi.as_slice())
        } else {
            None
        };
        // Count and page share one read snapshot, including concurrent writes.
        self.connection.execute_batch("BEGIN")?;
        let result = (|| {
            let limit = i64::try_from(limit)
                .map_err(|_| StoreError::InvalidConfiguration("id limit is too large"))?;
            let [count, page] = space.statements(upper.is_some(), limit);
            let parameters = if let Some(upper) = upper {
                vec![lo.as_slice(), upper]
            } else {
                vec![lo.as_slice()]
            };
            let total: i64 = self.connection.query_row(
                &count,
                rusqlite::params_from_iter(parameters.iter()),
                |row| row.get(0),
            )?;
            let mut statement = self.connection.prepare(&page)?;
            let ids = statement
                .query_map(rusqlite::params_from_iter(parameters.iter()), |row| {
                    row.get::<_, Vec<u8>>(0)
                })?
                .map(|bytes| array32(&bytes?, "resolved id"))
                .collect::<Result<Vec<_>, StoreError>>()?;
            Ok(IdMatches {
                ids,
                total: sqlite_i64(total)?,
            })
        })();
        self.connection.execute_batch("ROLLBACK")?;
        result
    }
}

impl IdSpace {
    /// Count and page the same index union; bounded selects an exclusive upper bound and the caller owns the read transaction.
    pub(crate) fn statements(self, bounded: bool, limit: i64) -> [String; 2] {
        let bound = if bounded {
            ">= ?1 AND {id} < ?2"
        } else {
            ">= ?1"
        };
        let source = match self {
            IdSpace::Exec => format!(
                "SELECT execution_id AS id FROM exec_requests WHERE execution_id {}",
                bound.replace("{id}", "execution_id")
            ),
            IdSpace::Receipt => format!(
                "SELECT receipt_id AS id FROM receipts WHERE receipt_id {}",
                bound.replace("{id}", "receipt_id")
            ),
            IdSpace::Session => {
                let bound = bound.replace("{id}", "session_id");
                format!("SELECT session_id AS id FROM activation_records WHERE session_id {bound} AND status = 'committed'
                    UNION SELECT session_id AS id FROM executions WHERE session_id {bound}
                    UNION SELECT session_id AS id FROM receipts WHERE session_id {bound}")
            }
        };
        [
            format!("SELECT COUNT(*) FROM ({source})"),
            format!("SELECT id FROM ({source}) ORDER BY id LIMIT {limit}"),
        ]
    }
}
