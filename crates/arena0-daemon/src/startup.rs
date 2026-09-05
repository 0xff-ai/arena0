//! Redacted, bounded progress records for daemon startup.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

const TRACE_TARGET: &str = "arena0::startup";

/// One process-start monotonic clock shared by every startup boundary.
#[derive(Debug)]
pub(crate) struct StartupTimeline {
    started: Instant,
    host_count: usize,
    program_count: usize,
}

impl StartupTimeline {
    pub(crate) fn new(host_count: usize, program_count: usize) -> Self {
        Self {
            started: Instant::now(),
            host_count,
            program_count,
        }
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

/// One bounded startup milestone. These values are operational observations;
/// they never contain program bytes, request values, or durable state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartupStage {
    HostsProvisioning,
    HostProvisioned,
    EngineInitializing,
    EngineReady,
    ProgramsBootstrapping,
    HostProgramsReady,
    HostComposed,
    HostStarting,
    HostReady,
    McpReady,
    InitializationComplete,
    Failed,
}

impl StartupStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::HostsProvisioning => "hosts_provisioning",
            Self::HostProvisioned => "host_provisioned",
            Self::EngineInitializing => "engine_initializing",
            Self::EngineReady => "engine_ready",
            Self::ProgramsBootstrapping => "programs_bootstrapping",
            Self::HostProgramsReady => "host_programs_ready",
            Self::HostComposed => "host_composed",
            Self::HostStarting => "host_starting",
            Self::HostReady => "host_ready",
            Self::McpReady => "mcp_ready",
            Self::InitializationComplete => "initialization_complete",
            Self::Failed => "failed",
        }
    }
}

/// Emit one bounded aggregate startup milestone. All fields are aggregate
/// counts or monotonic elapsed time; Host-owned identity details stay on the
/// per-Host projection below.
pub(crate) fn progress(stage: StartupStage, timeline: &StartupTimeline) {
    tracing::info!(
        target: TRACE_TARGET,
        operation = "startup",
        stage = stage.as_str(),
        host_count = timeline.host_count,
        program_count = timeline.program_count,
        elapsed_ms = elapsed_ms(timeline.elapsed()),
        "arena0d startup progress"
    );
}

/// Emit one per-Host startup milestone without exposing Host-owned identity
/// material or durable state.
pub(crate) fn host_progress(stage: StartupStage, host: &str, timeline: &StartupTimeline) {
    tracing::info!(
        target: TRACE_TARGET,
        operation = "startup",
        stage = stage.as_str(),
        host,
        elapsed_ms = elapsed_ms(timeline.elapsed()),
        "arena0d Host startup progress"
    );
}

/// Emit the point at which the MCP listener has bound its loopback endpoint.
pub(crate) fn mcp_ready(address: SocketAddr, timeline: &StartupTimeline) {
    tracing::info!(
        target: TRACE_TARGET,
        operation = "startup",
        stage = StartupStage::McpReady.as_str(),
        endpoint = %format_args!("http://{address}/mcp"),
        elapsed_ms = elapsed_ms(timeline.elapsed()),
        "arena0d MCP ready"
    );
}

fn elapsed_ms(elapsed: Duration) -> u64 {
    elapsed.as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::fmt::MakeWriter;

    use super::{StartupStage, StartupTimeline, host_progress, mcp_ready, progress};

    #[test]
    fn progress_fields_are_structured_and_redacted() {
        let output = SharedWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::INFO)
            .with_writer(output.clone())
            .finish();
        let timeline = StartupTimeline::new(5, 2);

        tracing::subscriber::with_default(subscriber, || {
            progress(StartupStage::EngineReady, &timeline);
            host_progress(StartupStage::HostReady, "host-01", &timeline);
            progress(StartupStage::Failed, &timeline);
            mcp_ready("127.0.0.1:7330".parse().expect("MCP address"), &timeline);
        });

        let lines = output.lines();
        assert_eq!(lines.len(), 4);
        let engine: serde_json::Value = serde_json::from_str(&lines[0]).expect("JSON trace line");
        assert_eq!(engine["target"], "arena0::startup");
        assert_eq!(engine["fields"]["operation"], "startup");
        assert_eq!(engine["fields"]["stage"], "engine_ready");
        assert_eq!(engine["fields"]["host_count"], 5);
        assert_eq!(engine["fields"]["program_count"], 2);
        assert!(engine["fields"]["elapsed_ms"].is_u64());
        assert_eq!(
            field_keys(&engine),
            BTreeSet::from([
                "elapsed_ms",
                "host_count",
                "message",
                "operation",
                "program_count",
                "stage",
            ])
        );

        let host: serde_json::Value = serde_json::from_str(&lines[1]).expect("JSON trace line");
        assert_eq!(host["fields"]["stage"], "host_ready");
        assert_eq!(host["fields"]["host"], "host-01");
        assert_eq!(
            field_keys(&host),
            BTreeSet::from(["elapsed_ms", "host", "message", "operation", "stage"])
        );
        let failed: serde_json::Value = serde_json::from_str(&lines[2]).expect("JSON trace line");
        assert_eq!(failed["fields"]["stage"], "failed");
        assert_eq!(field_keys(&failed), field_keys(&engine));
        let mcp: serde_json::Value = serde_json::from_str(&lines[3]).expect("JSON trace line");
        assert_eq!(mcp["fields"]["stage"], "mcp_ready");
        assert_eq!(mcp["fields"]["endpoint"], "http://127.0.0.1:7330/mcp");
        assert_eq!(
            field_keys(&mcp),
            BTreeSet::from(["elapsed_ms", "endpoint", "message", "operation", "stage"])
        );
        let elapsed = lines
            .iter()
            .map(|line| {
                let value: serde_json::Value = serde_json::from_str(line).expect("JSON trace line");
                value["fields"]["elapsed_ms"]
                    .as_u64()
                    .expect("elapsed time is an integer")
            })
            .collect::<Vec<_>>();
        assert!(elapsed.windows(2).all(|pair| pair[0] <= pair[1]));
        for line in lines {
            assert!(!line.contains("program_bytes"));
            assert!(!line.contains("params"));
            assert!(!line.contains("signature"));
            assert!(!line.contains("sql"));
            assert!(!line.contains("token"));
        }
    }

    fn field_keys(value: &serde_json::Value) -> BTreeSet<&str> {
        value["fields"]
            .as_object()
            .expect("structured fields")
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl SharedWriter {
        fn lines(&self) -> Vec<String> {
            String::from_utf8(self.0.lock().expect("writer lock").clone())
                .expect("trace output is UTF-8")
                .lines()
                .map(str::to_owned)
                .collect()
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
