use super::*;
use std::time::Instant;

const PERFORMANCE_TARGET: &str = "arena0::performance";

impl Database {
    pub(crate) fn is_poisoned(&self) -> bool {
        self.transaction_poison.is_some()
    }

    pub(super) fn begin(&mut self) -> Result<(), StoreError> {
        if let Some(reason) = &self.transaction_poison {
            return Err(StoreError::Corruption(format!(
                "store transaction state is poisoned: {reason}"
            )));
        }
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        Ok(())
    }

    /// Run one database operation in a SQLite transaction. An operation or
    /// commit failure rolls it back; a failed rollback poisons the connection
    /// so subsequent store calls fail closed.
    pub(super) fn transaction<T>(
        &mut self,
        operation: impl FnOnce(&mut Self) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.begin()?;
        match operation(self) {
            Ok(value) => self.commit_result(value),
            Err(error) => self.rollback_result(error),
        }
    }

    pub(super) fn commit_result<T>(&mut self, value: T) -> Result<T, StoreError> {
        let started =
            tracing::enabled!(target: PERFORMANCE_TARGET, tracing::Level::DEBUG).then(Instant::now);
        if let Err(error) = self.connection.execute_batch("COMMIT") {
            return match self.connection.execute_batch("ROLLBACK") {
                Ok(()) => {
                    record_transaction(
                        started,
                        "sqlite_transaction_commit",
                        false,
                        "commit_failed_rolled_back",
                    );
                    Err(error.into())
                }
                Err(rollback) => {
                    let reason = format!("commit failed: {error}; rollback failed: {rollback}");
                    self.transaction_poison = Some(reason.clone());
                    record_transaction(
                        started,
                        "sqlite_transaction_commit",
                        false,
                        "commit_failed_poisoned",
                    );
                    Err(StoreError::Corruption(reason))
                }
            };
        }
        record_transaction(started, "sqlite_transaction_commit", true, "committed");
        Ok(value)
    }

    pub(super) fn rollback_result<T>(&mut self, error: StoreError) -> Result<T, StoreError> {
        let started =
            tracing::enabled!(target: PERFORMANCE_TARGET, tracing::Level::DEBUG).then(Instant::now);
        match self.connection.execute_batch("ROLLBACK") {
            Ok(()) => {
                record_transaction(started, "sqlite_transaction_rollback", true, "rolled_back");
                Err(error)
            }
            Err(rollback) => {
                let reason = format!("operation failed: {error}; rollback failed: {rollback}");
                self.transaction_poison = Some(reason.clone());
                record_transaction(
                    started,
                    "sqlite_transaction_rollback",
                    false,
                    "rollback_failed_poisoned",
                );
                Err(StoreError::Corruption(reason))
            }
        }
    }
}

fn record_transaction(
    started: Option<Instant>,
    operation: &'static str,
    success: bool,
    result_class: &'static str,
) {
    let Some(started) = started else {
        return;
    };
    let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    tracing::debug!(
        target: PERFORMANCE_TARGET,
        operation,
        success,
        result_class,
        elapsed_us,
    );
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn performance_transaction_record_contains_only_safe_fields() {
        let output = SharedWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(output.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            record_transaction(
                Some(Instant::now()),
                "sqlite_transaction_commit",
                true,
                "committed",
            );
        });

        let text = output.text();
        let json: serde_json::Value = serde_json::from_str(&text).expect("JSON trace line");
        assert_eq!(json["target"], PERFORMANCE_TARGET);
        let fields = json["fields"].as_object().expect("structured fields");
        let allowed = ["operation", "success", "result_class", "elapsed_us"];
        assert!(fields.keys().all(|field| allowed.contains(&field.as_str())));
        assert_eq!(fields["operation"], "sqlite_transaction_commit");
        assert_eq!(fields["success"], true);
        for forbidden in [
            "params",
            "outcome",
            "context",
            "signature",
            "program",
            "private_state",
            "sql",
            "payload",
        ] {
            assert!(!fields.contains_key(forbidden));
        }
    }

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl SharedWriter {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().expect("writer lock").clone())
                .expect("trace output is UTF-8")
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedWriter {
        type Writer = SharedWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("writer lock").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
