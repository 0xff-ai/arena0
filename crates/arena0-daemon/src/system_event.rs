//! Structured tracing for redacted, process-local Host events.

use arena0_protocol::{ExecutionEvent, NegotiationEvent, SystemEvent, TerminalKind};
use serde_json::Value;
use valuable::{Listable, Mappable, Valuable, Value as ValuableValue, Visit};

const TRACE_TARGET: &str = "arena0::system_event";

pub(crate) fn emit(event: SystemEvent) {
    let severity = severity(&event);
    let enabled = match severity {
        Severity::Trace => tracing::enabled!(target: TRACE_TARGET, tracing::Level::TRACE),
        Severity::Debug => tracing::enabled!(target: TRACE_TARGET, tracing::Level::DEBUG),
        Severity::Info => tracing::enabled!(target: TRACE_TARGET, tracing::Level::INFO),
        Severity::Warn => tracing::enabled!(target: TRACE_TARGET, tracing::Level::WARN),
        Severity::Error => tracing::enabled!(target: TRACE_TARGET, tracing::Level::ERROR),
    };
    if !enabled {
        return;
    }
    let event = serde_json::to_value(event).expect("redacted system event is serializable");
    let event = SerdeValue(&event);
    match severity {
        Severity::Trace => tracing::trace!(
            target: TRACE_TARGET,
            event = tracing::field::valuable(&event),
        ),
        Severity::Debug => tracing::debug!(
            target: TRACE_TARGET,
            event = tracing::field::valuable(&event),
        ),
        Severity::Info => tracing::info!(
            target: TRACE_TARGET,
            event = tracing::field::valuable(&event),
        ),
        Severity::Warn => tracing::warn!(
            target: TRACE_TARGET,
            event = tracing::field::valuable(&event),
        ),
        Severity::Error => tracing::error!(
            target: TRACE_TARGET,
            event = tracing::field::valuable(&event),
        ),
    }
}

/// Borrowed structured view of a Serde JSON value. The tracing subscriber's
/// `valuable` support uses `valuable-serde` to serialize this view without
/// changing the event's nested field shape.
struct SerdeValue<'a>(&'a Value);

impl Valuable for SerdeValue<'_> {
    fn as_value(&self) -> ValuableValue<'_> {
        match self.0 {
            Value::Null => ValuableValue::Unit,
            Value::Bool(value) => ValuableValue::Bool(*value),
            Value::Number(value) => value
                .as_u64()
                .map(ValuableValue::U64)
                .or_else(|| value.as_i64().map(ValuableValue::I64))
                .or_else(|| value.as_f64().map(ValuableValue::F64))
                .unwrap_or(ValuableValue::Unit),
            Value::String(value) => ValuableValue::String(value),
            Value::Array(_) => ValuableValue::Listable(self),
            Value::Object(_) => ValuableValue::Mappable(self),
        }
    }

    fn visit(&self, visit: &mut dyn Visit) {
        match self.0 {
            Value::Array(items) => {
                for item in items {
                    visit.visit_value(Self(item).as_value());
                }
            }
            Value::Object(fields) => {
                for (name, value) in fields {
                    visit.visit_entry(name.as_str().as_value(), Self(value).as_value());
                }
            }
            _ => visit.visit_value(self.as_value()),
        }
    }
}

impl Listable for SerdeValue<'_> {
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0
            .as_array()
            .map_or((0, Some(0)), |items| (items.len(), Some(items.len())))
    }
}

impl Mappable for SerdeValue<'_> {
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0
            .as_object()
            .map_or((0, Some(0)), |fields| (fields.len(), Some(fields.len())))
    }
}

#[derive(Debug, Clone, Copy)]
enum Severity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

fn severity(event: &SystemEvent) -> Severity {
    match event {
        SystemEvent::Negotiation {
            event: NegotiationEvent::Retry { .. },
            ..
        } => Severity::Trace,
        SystemEvent::Negotiation {
            event: NegotiationEvent::TimedOut { .. },
            ..
        } => Severity::Warn,
        SystemEvent::Negotiation {
            event: NegotiationEvent::ActivationCommitted { .. },
            ..
        }
        | SystemEvent::Execution {
            event: ExecutionEvent::Created { .. },
            ..
        }
        | SystemEvent::Execution {
            event: ExecutionEvent::SessionStarted { .. },
            ..
        }
        | SystemEvent::Execution {
            event:
                ExecutionEvent::Terminal {
                    kind: TerminalKind::Completed,
                },
            ..
        } => Severity::Info,
        SystemEvent::Execution {
            event: ExecutionEvent::Terminal { .. },
            ..
        } => Severity::Error,
        SystemEvent::Negotiation { .. }
        | SystemEvent::Execution {
            event:
                ExecutionEvent::StepCommitted { .. }
                | ExecutionEvent::CalloutRequested { .. }
                | ExecutionEvent::CalloutAnswered { .. },
            ..
        } => Severity::Debug,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use arena0_program::ProgramHash;
    use arena0_protocol::{
        EventSource, ExecCreationOrigin, ExecId, ExecutionEvent, NegotiationEvent, NegotiationId,
        PeerId, SystemEvent,
    };
    use tracing_subscriber::fmt::MakeWriter;

    use super::emit;

    #[test]
    fn emits_one_structured_redacted_record() {
        let output = SharedWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::INFO)
            .with_writer(output.clone())
            .finish();
        let event = SystemEvent::Execution {
            source: EventSource::Execution {
                peer_id: PeerId([1; 32]),
                exec_id: ExecId([2; 32]),
                program_id: ProgramHash([3; 32]),
            },
            event: ExecutionEvent::Created {
                origin: ExecCreationOrigin::Request,
            },
        };

        tracing::subscriber::with_default(subscriber, || emit(event));

        let line = output.line();
        let json: serde_json::Value = serde_json::from_str(&line).expect("JSON trace line");
        assert_eq!(
            json["fields"]["event"]["Execution"]["event"]["Created"]["origin"],
            serde_json::json!("Request")
        );
        assert_eq!(
            json["fields"]["event"]["Execution"]["source"]["Execution"]["peer_id"],
            serde_json::json!("01".repeat(32))
        );
        assert!(line.contains("peer_id"));
        assert!(!line.contains("signature"));
        assert!(!line.contains("params"));
    }

    #[test]
    fn human_trace_formats_nested_ids_as_lowercase_hex() {
        let output = SharedWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_target(false)
            .with_writer(output.clone())
            .finish();
        let event = SystemEvent::Negotiation {
            source: EventSource::Negotiation {
                peer_id: PeerId([0xab; 32]),
                exec_id: ExecId([0xcd; 32]),
                program_id: ProgramHash([0xef; 32]),
                negotiation_id: NegotiationId([0x12; 32]),
            },
            event: NegotiationEvent::Started { target_size: 2 },
        };

        tracing::subscriber::with_default(subscriber, || emit(event));

        let line = output.line();
        assert!(line.contains(&"ab".repeat(32)));
        assert!(line.contains(&"cd".repeat(32)));
        assert!(line.contains(&"ef".repeat(32)));
        assert!(line.contains(&"12".repeat(32)));
        assert!(!line.contains("Id(["));
        assert!(!line.contains("Hash(["));
    }

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl SharedWriter {
        fn line(&self) -> String {
            String::from_utf8(self.0.lock().expect("writer lock").clone())
                .expect("trace output is UTF-8")
        }
    }

    impl<'a> MakeWriter<'a> for SharedWriter {
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
