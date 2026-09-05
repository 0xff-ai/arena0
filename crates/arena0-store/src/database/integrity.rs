use super::*;
use std::time::Instant;

const PERFORMANCE_TARGET: &str = "arena0::performance";

impl Database {
    pub(super) fn is_poisoned(&self) -> bool {
        self.transaction_poison.is_some()
    }

    pub(super) fn begin(&mut self) -> Result<(), StoreError> {
        if let Some(reason) = &self.transaction_poison {
            return Err(StoreError::Corruption(format!(
                "store transaction state is poisoned: {reason}"
            )));
        }
        if self.pending_execution.is_some() {
            return Err(StoreError::Corruption(
                "execution update remained pending outside its transaction".into(),
            ));
        }
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        Ok(())
    }

    pub(super) fn commit_result<T>(&mut self, value: T) -> Result<T, StoreError> {
        let started =
            tracing::enabled!(target: PERFORMANCE_TARGET, tracing::Level::DEBUG).then(Instant::now);
        let correlation = self.pending_execution.as_ref().map(|pending| {
            (
                pending.state.execution_id(),
                pending.state.version().get(),
                pending.state.public().next_step(),
                pending.encoded_bytes,
            )
        });
        if let Err(error) = self.connection.execute_batch("COMMIT") {
            self.pending_execution = None;
            return match self.connection.execute_batch("ROLLBACK") {
                Ok(()) => {
                    record_transaction(
                        started,
                        "sqlite_transaction_commit",
                        correlation,
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
                        correlation,
                        false,
                        "commit_failed_poisoned",
                    );
                    Err(StoreError::Corruption(reason))
                }
            };
        }
        if let Some(pending) = self.pending_execution.take() {
            self.executions.insert(pending.state, pending.encoded_bytes);
        }
        record_transaction(
            started,
            "sqlite_transaction_commit",
            correlation,
            true,
            "committed",
        );
        Ok(value)
    }

    pub(super) fn rollback_result<T>(&mut self, error: StoreError) -> Result<T, StoreError> {
        let started =
            tracing::enabled!(target: PERFORMANCE_TARGET, tracing::Level::DEBUG).then(Instant::now);
        let correlation = self.pending_execution.as_ref().map(|pending| {
            (
                pending.state.execution_id(),
                pending.state.version().get(),
                pending.state.public().next_step(),
                pending.encoded_bytes,
            )
        });
        self.pending_execution = None;
        match self.connection.execute_batch("ROLLBACK") {
            Ok(()) => {
                record_transaction(
                    started,
                    "sqlite_transaction_rollback",
                    correlation,
                    true,
                    "rolled_back",
                );
                Err(error)
            }
            Err(rollback) => {
                let reason = format!("operation failed: {error}; rollback failed: {rollback}");
                self.transaction_poison = Some(reason.clone());
                record_transaction(
                    started,
                    "sqlite_transaction_rollback",
                    correlation,
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
    correlation: Option<(ExecId, u64, u64, usize)>,
    success: bool,
    result_class: &'static str,
) {
    let Some(started) = started else {
        return;
    };
    let (exec_id, version, public_step, encoded_size) = correlation
        .map(|(exec_id, version, public_step, encoded_size)| {
            (
                Some(exec_id),
                Some(version),
                Some(public_step),
                Some(encoded_size),
            )
        })
        .unwrap_or((None, None, None, None));
    let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    tracing::debug!(
        target: PERFORMANCE_TARGET,
        operation,
        ?exec_id,
        ?version,
        ?public_step,
        ?encoded_size,
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
                Some((ExecId([1; 32]), 7, 3, 4096)),
                true,
                "committed",
            );
        });

        let text = output.text();
        let json: serde_json::Value = serde_json::from_str(&text).expect("JSON trace line");
        assert_eq!(json["target"], PERFORMANCE_TARGET);
        let fields = json["fields"].as_object().expect("structured fields");
        let allowed = [
            "operation",
            "exec_id",
            "version",
            "public_step",
            "encoded_size",
            "success",
            "result_class",
            "elapsed_us",
        ];
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
