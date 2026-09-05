//! Recording `tracing` subscriber for tests that assert on structured events.
//!
//! Tests assert on structured fields, never message text: the field set is the
//! contract, the message is prose.

use std::fmt::Debug;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;

/// One recorded event: target, level, and structured fields (name -> debug value).
#[derive(Debug, Clone)]
pub struct RecordedEvent {
    pub target: String,
    pub level: Level,
    pub fields: Vec<(String, String)>,
}

impl RecordedEvent {
    /// The debug-formatted value of `name`, if the event carried it.
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A `Layer` that buffers `debug!`/`trace!` events with their structured fields.
#[derive(Clone, Default)]
pub struct Recorder {
    events: Arc<Mutex<Vec<RecordedEvent>>>,
}

impl Recorder {
    /// Install as the default subscriber; the returned guard uninstalls on drop.
    pub fn install(&self) -> tracing::subscriber::DefaultGuard {
        tracing::subscriber::set_default(tracing_subscriber::registry().with(self.clone()))
    }

    pub fn events(&self) -> Vec<RecordedEvent> {
        self.events.lock().expect("recorder poisoned").clone()
    }

    /// Events with the given target (module path, e.g. `arena0_node::execution`).
    pub fn with_target(&self, target: &str) -> Vec<RecordedEvent> {
        self.events()
            .into_iter()
            .filter(|e| e.target == target)
            .collect()
    }

    /// True if any event has a field `name` whose debug value equals `value`.
    pub fn has_field(&self, name: &str, value: &str) -> bool {
        self.events().iter().any(|e| e.field(name) == Some(value))
    }
}

struct FieldCollector {
    fields: Vec<(String, String)>,
}

impl Visit for FieldCollector {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.fields
            .push((field.name().to_string(), format!("{value:?}")));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .push((field.name().to_string(), value.to_string()));
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Recorder {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if !matches!(*event.metadata().level(), Level::DEBUG | Level::TRACE) {
            return;
        }
        let mut collector = FieldCollector { fields: Vec::new() };
        event.record(&mut collector);
        self.events
            .lock()
            .expect("recorder poisoned")
            .push(RecordedEvent {
                target: event.metadata().target().to_string(),
                level: *event.metadata().level(),
                fields: collector.fields,
            });
    }
}
