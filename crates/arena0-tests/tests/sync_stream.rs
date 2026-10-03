//! Real HTTP/1.1 streams from this checkout, with a fresh home and ephemeral
//! port per scenario. List and trace reads provide the independent row oracle.
mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

use arena0_api::{
    EnsembleSpec, HostCursor, HostRequest, Observation, Request, ResponseOk, RowOp, SyncCursor,
    SyncFrame,
};
use arena0_daemon::{Daemon, HttpConfig};
use arena0_home::{Home, HostName};
use arena0_protocol::{ExecId, NegotiationTarget};
use arena0_tests::fixtures::LIVE_EXECUTION_TIMEOUT;
use common::{HostTarget, call, call_daemon, created, drive, ok};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing_subscriber::{Layer, layer::Context, prelude::*, registry::LookupSpan};

type Rows = BTreeMap<String, Value>;

#[derive(Clone, Default)]
struct Closures {
    endpoints: Arc<Mutex<BTreeSet<String>>>,
    changed: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
struct ClosureFields {
    event: String,
    endpoint: String,
}

impl tracing::field::Visit for ClosureFields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "event" => self.event = value.to_owned(),
            "endpoint" => self.endpoint = value.to_owned(),
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "endpoint" {
            self.endpoint = format!("{value:?}");
        }
    }
}

impl<S: tracing::Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Closures {
    fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
        let mut fields = ClosureFields::default();
        event.record(&mut fields);
        if fields.event == "sync.connection_closed" {
            self.endpoints.lock().unwrap().insert(fields.endpoint);
            self.changed.notify_one();
        }
    }
}

struct Fixture {
    home: tempfile::TempDir,
    service: Option<(Arc<Daemon>, tokio::task::JoinHandle<()>)>,
    a: HostTarget,
    b: HostTarget,
    address: SocketAddr,
    program: String,
}

impl Fixture {
    async fn start(&mut self) {
        let service = Daemon::start(
            vec![self.a.name.clone(), self.b.name.clone()],
            HttpConfig::new(([127, 0, 0, 1], 0).into(), None).unwrap(),
            arena0_test_engine::shared_test_engine(),
            Home::from_root(self.home.path().to_owned()).unwrap(),
            true,
        )
        .await
        .unwrap();
        let serving = Arc::clone(&service);
        let task = tokio::spawn(async move {
            serving.serve().await.unwrap();
        });
        common::wait_for_socket(&self.a.socket).await;
        let ResponseOk::DaemonInfo(info) =
            ok(call_daemon(&self.a.socket, &Request::DaemonInfo).await)
        else {
            panic!("daemon info")
        };
        self.address = info
            .http_url
            .strip_prefix("http://")
            .unwrap()
            .parse()
            .unwrap();
        self.service = Some((service, task));
    }

    async fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let socket = Home::from_root(home.path().to_owned()).unwrap().socket();
        let mut fixture = Self {
            home,
            service: None,
            a: HostTarget {
                socket: socket.clone(),
                name: HostName::try_from("a").unwrap(),
            },
            b: HostTarget {
                socket,
                name: HostName::try_from("b").unwrap(),
            },
            address: ([127, 0, 0, 1], 0).into(),
            program: String::new(),
        };
        fixture.start().await;
        let wasm = common::rps_wasm();
        fixture.program = common::import(&fixture.a, &wasm).await.to_string();
        assert_eq!(
            common::import(&fixture.b, &wasm).await.to_string(),
            fixture.program
        );
        fixture
    }

    async fn stop(&mut self) {
        let (service, task) = self.service.take().unwrap();
        tokio::time::timeout(LIVE_EXECUTION_TIMEOUT, async {
            service.stop().await;
            task.await.unwrap();
        })
        .await
        .expect("shutdown must finish, including disconnected HTTP streams");
        drop(service);
    }

    async fn pair(&self, id: u8, complete: bool) -> (ExecId, ExecId) {
        let completion = if complete {
            Some(common::http::EventStream::open(self.address).await)
        } else {
            None
        };
        let ResponseOk::HostStatus(status) = ok(call(&self.a, &HostRequest::Info).await) else {
            panic!("host info")
        };
        let ResponseOk::ExecCreated {
            exec_id: a,
            negotiation_id: Some(negotiation),
            ..
        } = ok(call(
            &self.a,
            &HostRequest::ExecNew {
                exec_id: ExecId([id; 32]),
                program: self.program.clone(),
                params: Some(Value::Null),
                ensemble: EnsembleSpec::Create {
                    participant_count: 2,
                },
                blobs: vec![],
            },
        )
        .await)
        else {
            panic!("creator")
        };
        let b = created(
            call(
                &self.b,
                &HostRequest::ExecNew {
                    exec_id: ExecId([id + 1; 32]),
                    program: self.program.clone(),
                    params: None,
                    ensemble: EnsembleSpec::Join {
                        target: Some(NegotiationTarget::new(status.host.peer_id, negotiation)),
                    },
                    blobs: vec![],
                },
            )
            .await,
        );
        if complete {
            let (sa, sb) = tokio::join!(drive(&self.a, a), drive(&self.b, b));
            assert_eq!(sa, sb);
            // A program outcome precedes end-handshake settlement. Wait for
            // both durable Ended transitions so subsequent independent list
            // and trace reads observe the same quiescent store state.
            let mut events = completion.unwrap();
            tokio::time::timeout(LIVE_EXECUTION_TIMEOUT, async {
                let mut ended = BTreeSet::new();
                while ended.len() < 2 {
                    let (kind, data) = events.next().await;
                    if kind != "host" {
                        continue;
                    }
                    let frame: arena0_api::EventFrame = serde_json::from_str(&data).unwrap();
                    if matches!(
                        frame.data,
                        arena0_api::EventData::SessionEndProgress {
                            phase: arena0_api::ExecEndPhase::Ended,
                            ..
                        }
                    ) && let Some(exec_id) = frame.exec_id
                        && (exec_id == a || exec_id == b)
                    {
                        ended.insert(exec_id);
                    }
                }
            })
            .await
            .expect("both executions settled their end handshake");
        } else {
            for (host, exec_id) in [(&self.a, a), (&self.b, b)] {
                ok(call(
                    host,
                    &HostRequest::ExecAwait {
                        exec_id,
                        until: arena0_api::AwaitState::Active,
                    },
                )
                .await);
            }
            // A real callout is the barrier that the running execution has a
            // durable step; no elapsed-time assumption is needed.
            let request_a = HostRequest::ExecNext { exec_id: a };
            let request_b = HostRequest::ExecNext { exec_id: b };
            tokio::select! {
                reply = call(&self.a, &request_a) => { ok(reply); }
                reply = call(&self.b, &request_b) => { ok(reply); }
            }
        }
        (a, b)
    }
}

struct Stream {
    read: BufReader<TcpStream>,
    pending: Vec<u8>,
    artifact: std::fs::File,
}

impl Stream {
    async fn open(fixture: &Fixture, cursor: Option<&SyncCursor>) -> Self {
        let path = cursor
            .map(|c| format!("/sync/{}", hex::encode(serde_json::to_vec(c).unwrap())))
            .unwrap_or_else(|| "/sync".into());
        let mut socket = TcpStream::connect(fixture.address).await.unwrap();
        socket
            .write_all(
                format!("GET {path} HTTP/1.1\r\nHost: {}\r\n\r\n", fixture.address).as_bytes(),
            )
            .await
            .unwrap();
        let mut read = BufReader::new(socket);
        let mut line = String::new();
        read.read_line(&mut line).await.unwrap();
        assert!(line.starts_with("HTTP/1.1 200"), "{line}");
        let mut chunked = false;
        let mut sse = false;
        loop {
            line.clear();
            assert_ne!(read.read_line(&mut line).await.unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            chunked |= line
                .to_ascii_lowercase()
                .starts_with("transfer-encoding: chunked");
            sse |= line
                .to_ascii_lowercase()
                .starts_with("content-type: text/event-stream");
        }
        assert!(chunked && sse);
        let artifact = tempfile::Builder::new()
            .prefix("arena0-sync-stream-")
            .suffix(".jsonl")
            .tempfile()
            .unwrap();
        let (artifact, path) = artifact.keep().unwrap();
        eprintln!("sync frames: {}", path.display());
        Self {
            read,
            pending: vec![],
            artifact,
        }
    }

    async fn next(&mut self) -> SyncFrame {
        tokio::time::timeout(LIVE_EXECUTION_TIMEOUT, async {
            loop {
                if let Some(end) = self.pending.windows(2).position(|b| b == b"\n\n") {
                    let message =
                        String::from_utf8(self.pending.drain(..end + 2).collect()).unwrap();
                    let mut event = None;
                    let mut data = Vec::new();
                    for line in message.lines() {
                        if let Some(value) = line.strip_prefix("event:") {
                            event = Some(value.trim());
                        }
                        if let Some(value) = line.strip_prefix("data:") {
                            data.push(value.trim_start());
                        }
                    }
                    if data.is_empty() {
                        continue;
                    }
                    assert_eq!(event, Some("sync"));
                    let data = data.join("\n");
                    use std::io::Write;
                    writeln!(self.artifact, "{data}").unwrap();
                    return serde_json::from_str(&data).unwrap();
                }
                let mut line = String::new();
                assert_ne!(
                    self.read.read_line(&mut line).await.unwrap(),
                    0,
                    "stream closed"
                );
                let length =
                    usize::from_str_radix(line.trim().split(';').next().unwrap(), 16).unwrap();
                assert_ne!(length, 0, "stream ended");
                let start = self.pending.len();
                self.pending.resize(start + length, 0);
                self.read
                    .read_exact(&mut self.pending[start..])
                    .await
                    .unwrap();
                let mut crlf = [0; 2];
                self.read.read_exact(&mut crlf).await.unwrap();
                assert_eq!(crlf, *b"\r\n");
            }
        })
        .await
        .expect("next sync frame")
    }
}

fn insert(rows: &mut Rows, op: &RowOp) {
    let (key, value) = match op {
        RowOp::Exec(row) => (format!("exec:{}", row.exec_id), json!(row)),
        RowOp::Receipt(row) => (format!("receipt:{}", row.receipt_id), json!(row)),
        RowOp::Program(row) => (format!("program:{}", row.summary.program_hash), json!(row)),
        RowOp::Blob(row) => (format!("blob:{}", row.hash), json!(row)),
        RowOp::ProgramRemoved { program_hash } => {
            rows.remove(&format!("program:{program_hash}"));
            return;
        }
        RowOp::Steps(row) => {
            assert_eq!(row.certified_at_ms.len(), row.state_prefix.len());
            for (i, (time, prefix)) in row
                .certified_at_ms
                .iter()
                .zip(&row.state_prefix)
                .enumerate()
            {
                rows.insert(
                    format!("step:{}:{}", row.exec_id, row.from_step + i as u64),
                    json!([time, prefix]),
                );
            }
            return;
        }
    };
    rows.insert(key, value);
}

#[derive(Default)]
struct Applied {
    rows: BTreeMap<String, Rows>,
    cursor: SyncCursor,
    offers: BTreeSet<String>,
}

impl Applied {
    fn apply(&mut self, frame: &SyncFrame) {
        match frame {
            SyncFrame::Host { host, boot_id, .. } => {
                self.cursor.0.entry(host.id.clone()).or_insert(HostCursor {
                    boot_id: boot_id.clone(),
                    seq: 0,
                });
            }
            SyncFrame::Reset { host } => {
                self.rows.insert(host.clone(), Rows::new());
            }
            SyncFrame::Rows { host, seq, ops } => {
                for op in ops {
                    insert(self.rows.entry(host.clone()).or_default(), op);
                }
                self.cursor.0.get_mut(host).unwrap().seq = *seq;
            }
            SyncFrame::Synced { host, seq } => {
                self.cursor.0.get_mut(host).unwrap().seq = *seq;
            }
            SyncFrame::Observed {
                observation: Observation::Offers { host, .. },
            } => {
                self.offers.insert(host.clone());
            }
            SyncFrame::Observed { .. } => {}
        }
    }

    async fn catchup(&mut self, stream: &mut Stream, reset: bool) -> BTreeMap<String, Vec<RowOp>> {
        let mut phases = BTreeMap::from([("a".to_owned(), 0), ("b".to_owned(), 0)]);
        let mut changed = BTreeMap::new();
        while phases.values().any(|phase| *phase != 4) {
            let frame = stream.next().await;
            match &frame {
                SyncFrame::Host { host, online, .. } => {
                    assert!(*online);
                    assert_eq!(phases[&host.id], 0);
                    phases.insert(host.id.clone(), if reset { 1 } else { 2 });
                }
                SyncFrame::Reset { host } => {
                    assert!(reset);
                    assert_eq!(phases[host], 1);
                    phases.insert(host.clone(), 2);
                }
                SyncFrame::Rows { host, ops, .. } => {
                    assert_eq!(phases[host], 2);
                    changed.insert(host.clone(), ops.clone());
                    phases.insert(host.clone(), 3);
                }
                SyncFrame::Synced { host, .. } => {
                    assert!(phases[host] == 3 || (!reset && phases[host] == 2));
                    phases.insert(host.clone(), 4);
                }
                SyncFrame::Observed {
                    observation: Observation::Event(event),
                } => {
                    assert_eq!(phases[&event.host.id], 4, "Host observations follow Synced");
                }
                SyncFrame::Observed { .. } => {}
            }
            self.apply(&frame);
        }
        changed
    }
}

async fn oracle(host: &HostTarget) -> Rows {
    let mut rows = Rows::new();
    let ResponseOk::ExecList(execs) = ok(call(host, &HostRequest::ExecList).await) else {
        panic!("executions")
    };
    for exec in execs {
        if exec.step.is_some_and(|step| step > 0) {
            let ResponseOk::Trace(steps) = ok(call(
                host,
                &HostRequest::ExecTrace {
                    exec_id: exec.exec_id,
                    from: 0,
                    to: u64::MAX,
                },
            )
            .await) else {
                panic!("trace")
            };
            for (i, step) in steps.iter().enumerate() {
                rows.insert(
                    format!("step:{}:{i}", exec.exec_id),
                    json!([
                        step.certified_at_ms,
                        u32::from_be_bytes(step.entry.post_state.0[..4].try_into().unwrap())
                    ]),
                );
            }
        }
        insert(&mut rows, &RowOp::Exec(Box::new(exec)));
    }
    let ResponseOk::ReceiptList(receipts) = ok(call(host, &HostRequest::ReceiptList).await) else {
        panic!("receipts")
    };
    for receipt in receipts {
        insert(&mut rows, &RowOp::Receipt(receipt));
    }
    let ResponseOk::ProgramList(programs) = ok(call(host, &HostRequest::ProgramList).await) else {
        panic!("programs")
    };
    for program in programs {
        let ResponseOk::Program(detail) = ok(call(
            host,
            &HostRequest::ProgramGet {
                program: program.program_hash.to_string(),
            },
        )
        .await) else {
            panic!("program detail")
        };
        assert_eq!(detail.summary, program);
        insert(&mut rows, &RowOp::Program(detail));
    }
    let ResponseOk::BlobList(blobs) = ok(call(host, &HostRequest::BlobList).await) else {
        panic!("blobs")
    };
    for blob in blobs {
        insert(&mut rows, &RowOp::Blob(blob));
    }
    rows
}

async fn expected(fixture: &Fixture) -> BTreeMap<String, Rows> {
    BTreeMap::from([
        ("a".into(), oracle(&fixture.a).await),
        ("b".into(), oracle(&fixture.b).await),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_exactness() {
    let mut fixture = Fixture::new().await;
    fixture.pair(10, true).await;
    fixture.pair(20, false).await;
    let blob = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(blob.path(), b"sync fixture blob").unwrap();
    ok(call(
        &fixture.a,
        &HostRequest::BlobImport {
            source: arena0_api::FileSource::Path(blob.path().to_owned()),
        },
    )
    .await);
    let mut stream = Stream::open(&fixture, None).await;
    let mut applied = Applied::default();
    applied.catchup(&mut stream, true).await;
    assert_eq!(applied.rows, expected(&fixture).await);
    drop(stream);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_exactness() {
    let mut fixture = Fixture::new().await;
    let mut stream = Stream::open(&fixture, None).await;
    let mut applied = Applied::default();
    applied.catchup(&mut stream, true).await;
    fixture.pair(30, true).await;
    let expected = expected(&fixture).await;
    while applied.rows != expected {
        let frame = stream.next().await;
        if let SyncFrame::Rows { host, seq, .. } = &frame {
            assert!(*seq > applied.cursor.0[host].seq);
        }
        assert!(
            !matches!(frame, SyncFrame::Reset { .. }),
            "short session stays inside the change ring"
        );
        applied.apply(&frame);
    }
    drop(stream);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_only_changed_rows() {
    let mut fixture = Fixture::new().await;
    fixture.pair(40, true).await;
    let mut stream = Stream::open(&fixture, None).await;
    let mut applied = Applied::default();
    applied.catchup(&mut stream, true).await;
    let before = applied.rows.clone();
    drop(stream);
    fixture.pair(50, true).await;
    let expected = expected(&fixture).await;
    let mut stream = Stream::open(&fixture, Some(&applied.cursor)).await;
    let changed = applied.catchup(&mut stream, false).await;
    for (host, ops) in changed {
        let mut rows = Rows::new();
        for op in &ops {
            insert(&mut rows, op);
        }
        let keys: BTreeSet<_> = rows.keys().collect();
        let expected_keys: BTreeSet<_> = expected[&host]
            .iter()
            .filter(|(key, value)| before[&host].get(*key) != Some(*value))
            .map(|(key, _)| key)
            .collect();
        assert_eq!(
            keys, expected_keys,
            "first catch-up contains exactly changed row keys"
        );
    }
    assert_eq!(applied.rows, expected);
    drop(stream);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_resets_old_boot_cursor() {
    let mut fixture = Fixture::new().await;
    fixture.pair(60, true).await;
    let mut stream = Stream::open(&fixture, None).await;
    let mut applied = Applied::default();
    applied.catchup(&mut stream, true).await;
    let cursor = applied.cursor.clone();
    drop(stream);
    fixture.stop().await;
    fixture.start().await;
    let mut stream = Stream::open(&fixture, Some(&cursor)).await;
    let mut restarted = Applied::default();
    restarted.catchup(&mut stream, true).await;
    for (host, current) in &restarted.cursor.0 {
        assert_ne!(current.boot_id, cursor.0[host].boot_id);
    }
    assert_eq!(restarted.rows, expected(&fixture).await);
    drop(stream);
    // A prior boot's low sequence must reset too. Otherwise the ahead-of-head
    // fallback could conceal a missing boot-id check in this restart journey.
    let mut low_cursor = cursor;
    for host in low_cursor.0.values_mut() {
        host.seq = 0;
    }
    let mut stream = Stream::open(&fixture, Some(&low_cursor)).await;
    let mut restarted = Applied::default();
    restarted.catchup(&mut stream, true).await;
    assert_eq!(restarted.rows, expected(&fixture).await);
    drop(stream);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cursor_ahead_resets() {
    let mut fixture = Fixture::new().await;
    let mut stream = Stream::open(&fixture, None).await;
    let mut applied = Applied::default();
    applied.catchup(&mut stream, true).await;
    drop(stream);
    for cursor in applied.cursor.0.values_mut() {
        cursor.seq += 1000;
    }
    let mut stream = Stream::open(&fixture, Some(&applied.cursor)).await;
    applied.catchup(&mut stream, true).await;
    assert_eq!(applied.rows, expected(&fixture).await);
    drop(stream);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_cursor_is_http_400() {
    let mut fixture = Fixture::new().await;
    for cursor in ["not-hex", "6e6f742d6a736f6e"] {
        let reply = common::http::http(
            fixture.address,
            "GET",
            &format!("/sync/{cursor}"),
            None,
            &[],
        )
        .await;
        assert_eq!(reply.status, 400);
        assert!(
            reply
                .headers
                .iter()
                .any(|(key, value)| key == "content-type" && value.starts_with("text/plain"))
        );
        assert!(!reply.body.is_empty());
    }
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn observed_after_synced_and_offers_on_connect() {
    let mut fixture = Fixture::new().await;
    let mut stream = Stream::open(&fixture, None).await;
    let mut applied = Applied::default();
    applied.catchup(&mut stream, true).await;
    while applied.offers.len() < 2 {
        if let SyncFrame::Observed {
            observation:
                Observation::Offers {
                    host,
                    offers: entries,
                },
        } = stream.next().await
        {
            assert!(entries.is_empty());
            applied.offers.insert(host);
        }
    }
    let (a, _) = fixture.pair(70, true).await;
    loop {
        if let SyncFrame::Observed {
            observation: Observation::Event(frame),
        } = stream.next().await
            && frame.exec_id == Some(a)
            && frame.data.kind() == "exec.created"
        {
            break;
        }
    }
    drop(stream);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnect_releases_connection_for_shutdown() {
    static CLOSURES: OnceLock<Closures> = OnceLock::new();
    let closures = CLOSURES.get_or_init(|| {
        let closures = Closures::default();
        tracing_subscriber::registry().with(closures.clone()).init();
        closures
    });
    let mut fixture = Fixture::new().await;
    let mut stream = Stream::open(&fixture, None).await;
    let mut applied = Applied::default();
    applied.catchup(&mut stream, true).await;
    // Drain all connection-time sends before disconnecting. An idle watch
    // must end on disconnect without relying on another write or shutdown.
    while applied.offers.len() < 2 {
        applied.apply(&stream.next().await);
    }
    drop(stream);
    let endpoint = format!("http://{}", fixture.address);
    tokio::time::timeout(LIVE_EXECUTION_TIMEOUT, async {
        loop {
            let changed = closures.changed.notified();
            if closures.endpoints.lock().unwrap().contains(&endpoint) {
                break;
            }
            changed.await;
        }
    })
    .await
    .expect("disconnected sync task joins its children before daemon shutdown");
    fixture.stop().await;
}
