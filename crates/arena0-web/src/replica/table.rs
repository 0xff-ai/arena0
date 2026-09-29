//! One collection's rows, keyed and ordered, with compare-before-upsert.

use std::collections::BTreeMap;

use crate::protocol::RowOp;

/// Rows of one collection. Every mutation reports the resulting row ops, and
/// reports none when nothing changed.
#[derive(Debug)]
pub(super) struct Table<T>(pub(super) BTreeMap<String, T>);

impl<T> Default for Table<T> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<T: Clone + PartialEq> Table<T> {
    pub(super) fn upsert(&mut self, key: &str, row: T, ops: &mut Vec<RowOp<T>>) {
        if self.0.get(key) == Some(&row) {
            return;
        }
        self.0.insert(key.to_owned(), row.clone());
        ops.push(RowOp::Upsert {
            key: key.to_owned(),
            row,
        });
    }

    pub(super) fn delete(&mut self, key: &str, ops: &mut Vec<RowOp<T>>) {
        if self.0.remove(key).is_some() {
            ops.push(RowOp::Delete {
                key: key.to_owned(),
            });
        }
    }

    pub(super) fn keys_with_prefix(&self, prefix: &str) -> Vec<String> {
        self.0
            .range(prefix.to_owned()..)
            .take_while(|(key, _)| key.starts_with(prefix))
            .map(|(key, _)| key.clone())
            .collect()
    }

    pub(super) fn delete_prefix(&mut self, prefix: &str, ops: &mut Vec<RowOp<T>>) {
        for key in self.keys_with_prefix(prefix) {
            self.delete(&key, ops);
        }
    }

    /// Make the rows under `prefix` exactly `rows`.
    pub(super) fn replace_prefix(
        &mut self,
        prefix: &str,
        rows: Vec<(String, T)>,
        ops: &mut Vec<RowOp<T>>,
    ) {
        let keep: std::collections::HashSet<&str> =
            rows.iter().map(|(key, _)| key.as_str()).collect();
        for key in self.keys_with_prefix(prefix) {
            if !keep.contains(key.as_str()) {
                self.delete(&key, ops);
            }
        }
        for (key, row) in rows {
            self.upsert(&key, row, ops);
        }
    }

    pub(super) fn snapshot(&self) -> Vec<RowOp<T>> {
        self.0
            .iter()
            .map(|(key, row)| RowOp::Upsert {
                key: key.clone(),
                row: row.clone(),
            })
            .collect()
    }
}
