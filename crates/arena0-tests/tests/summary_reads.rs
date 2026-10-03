//! Summary reads use durable index facts; detail reads validate archived
//! executions. Each scenario boots this checkout's daemon on its own
//! temporary home and OS-assigned HTTP port.

mod common;

use std::io::{self, Write};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

use arena0_api::{
    ApiErrorCode, AwaitState, EnsembleSpec, EventData, EventFilter, EventFrame, ExecEndPhase,
    ExecLifecycle, ExecStatusState, HostRequest, NextEvent, Request, Response, ResponseOk,
};
use arena0_daemon::{Daemon, HttpConfig};
use arena0_home::{Home, HostName};
use arena0_protocol::{ExecId, NegotiationTarget};
use arena0_tests::fixtures::LIVE_EXECUTION_TIMEOUT;
use common::{HostTarget, call, call_daemon, created, drive, ok, rps_wasm};
use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::{
    UnixStream,
    unix::{OwnedReadHalf, OwnedWriteHalf},
};
use tracing_subscriber::{Layer, layer::Context, prelude::*, registry::LookupSpan};

// A process-wide subscriber observes store threads as well as Tokio tasks.
// Serialize scenarios sharing the subscriber and artifact buffer. Decode
// attribution follows request spans, so background actors are excluded.
static SCENARIO: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static TRACE: OnceLock<Trace> = OnceLock::new();

struct TestDaemon {
    _home: tempfile::TempDir,
    socket: std::path::PathBuf,
    host_a: HostTarget,
    host_b: HostTarget,
    peer_a: arena0_protocol::PeerId,
    program_id: arena0_program::ProgramHash,
    // The fixture owns both references that keep the home lock alive. Stop
    // must join the serve task and drop the service before reopening it.
    service: Option<(Arc<Daemon>, tokio::task::JoinHandle<()>)>,
}

impl TestDaemon {
    async fn stop(&mut self) {
        let (service, serving) = self.service.take().expect("running fixture daemon");
        service.stop().await;
        serving.await.expect("fixture serve task");
        drop(service);
    }
}

async fn start_service(
    home: Home,
    hosts: Vec<HostName>,
) -> (Arc<Daemon>, tokio::task::JoinHandle<()>) {
    let service = Daemon::start(
        hosts,
        HttpConfig::new(SocketAddr::from(([127, 0, 0, 1], 0)), None).unwrap(),
        arena0_test_engine::shared_test_engine(),
        home,
        true,
    )
    .await
    .expect("start fixture daemon");
    let serving = Arc::clone(&service);
    let task = tokio::spawn(async move {
        serving.serve().await.expect("serve fixture daemon");
    });
    (service, task)
}

async fn daemon(wasm: &[u8]) -> TestDaemon {
    let directory = tempfile::tempdir().unwrap();
    let home = Home::from_root(directory.path().to_path_buf()).unwrap();
    let socket = home.socket();
    let host_a = HostTarget {
        socket: socket.clone(),
        name: HostName::try_from("a").unwrap(),
    };
    let host_b = HostTarget {
        socket: socket.clone(),
        name: HostName::try_from("b").unwrap(),
    };
    let service = start_service(home, vec![host_a.name.clone(), host_b.name.clone()]).await;
    common::wait_for_socket(&socket).await;
    let program_id = common::import(&host_a, wasm).await;
    assert_eq!(common::import(&host_b, wasm).await, program_id);
    let ResponseOk::HostStatus(status) = ok(call(&host_a, &HostRequest::Info).await) else {
        panic!("expected host info");
    };
    TestDaemon {
        _home: directory,
        socket,
        host_a,
        host_b,
        peer_a: status.host.peer_id,
        program_id,
        service: Some(service),
    }
}

#[derive(Clone, Default)]
struct Trace {
    bytes: Arc<Mutex<Vec<u8>>>,
    request_decodes: Arc<Mutex<Vec<(String, String)>>>,
}

#[derive(Default)]
struct RequestMethod(String);

#[derive(Default)]
struct Operation(String);

impl tracing::field::Visit for RequestMethod {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "method" {
            self.0 = value.to_owned();
        }
    }

    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

impl tracing::field::Visit for Operation {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "operation" {
            self.0 = value.to_owned();
        }
    }

    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

impl<S> Layer<S> for Trace
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        if matches!(attrs.metadata().name(), "host_request" | "daemon_request") {
            let mut method = RequestMethod::default();
            attrs.record(&mut method);
            ctx.span(id).unwrap().extensions_mut().insert(method);
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        if event.metadata().target() != "arena0::performance" {
            return;
        }
        let mut operation = Operation::default();
        event.record(&mut operation);
        if !operation.0.starts_with("decode.") {
            return;
        }
        if let Some(scope) = ctx.event_scope(event) {
            for span in scope.from_root() {
                if let Some(method) = span.extensions().get::<RequestMethod>() {
                    self.request_decodes
                        .lock()
                        .unwrap()
                        .push((method.0.clone(), operation.0.clone()));
                }
            }
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Trace {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

impl Write for Trace {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Trace {
    fn clear(&self) {
        self.request_decodes.lock().unwrap().clear();
    }

    fn decodes(&self, method: &str) -> Vec<String> {
        self.request_decodes
            .lock()
            .unwrap()
            .iter()
            .filter(|(request_method, _)| request_method == method)
            .map(|(_, operation)| operation.clone())
            .collect()
    }

    fn save(&self, name: &str) {
        let directory = tempfile::Builder::new()
            .prefix("arena0-summary-reads-")
            .tempdir()
            .unwrap()
            .keep();
        let path = directory.join(format!("{name}.jsonl"));
        std::fs::write(&path, self.bytes.lock().unwrap().as_slice()).unwrap();
        eprintln!("summary-read trace: {}", path.display());
    }
}

fn trace() -> &'static Trace {
    TRACE.get_or_init(|| {
        let trace = Trace::default();
        tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::DEBUG)
            .with(trace.clone())
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_ansi(false)
                    .with_writer(trace.clone()),
            )
            .try_init()
            .expect("install summary-read trace layer");
        trace
    })
}

async fn new_creator(d: &TestDaemon, id: u8) -> (ExecId, arena0_protocol::NegotiationId) {
    let ResponseOk::ExecCreated {
        exec_id,
        negotiation_id: Some(negotiation_id),
        ..
    } = ok(call(
        &d.host_a,
        &HostRequest::ExecNew {
            exec_id: ExecId([id; 32]),
            program: d.program_id.to_string(),
            params: Some(Value::Null),
            ensemble: EnsembleSpec::Create {
                participant_count: 2,
            },
            blobs: vec![],
        },
    )
    .await)
    else {
        panic!("expected creator");
    };
    (exec_id, negotiation_id)
}

async fn join(d: &TestDaemon, negotiation_id: arena0_protocol::NegotiationId, id: u8) -> ExecId {
    created(
        call(
            &d.host_b,
            &HostRequest::ExecNew {
                exec_id: ExecId([id; 32]),
                program: d.program_id.to_string(),
                params: None,
                ensemble: EnsembleSpec::Join {
                    target: Some(NegotiationTarget::new(d.peer_a, negotiation_id)),
                },
                blobs: vec![],
            },
        )
        .await,
    )
}

async fn active_pair(d: &TestDaemon, id: u8) -> (ExecId, ExecId) {
    let (a, negotiation) = new_creator(d, id).await;
    let b = join(d, negotiation, id + 1).await;
    for (host, exec_id) in [(&d.host_a, a), (&d.host_b, b)] {
        ok(call(
            host,
            &HostRequest::ExecAwait {
                exec_id,
                until: AwaitState::Active,
            },
        )
        .await);
    }
    let request_a = HostRequest::ExecNext { exec_id: a };
    let request_b = HostRequest::ExecNext { exec_id: b };
    tokio::select! {
        response = call(&d.host_a, &request_a) => assert!(matches!(ok(response), ResponseOk::Next(NextEvent::Callout { .. }))),
        response = call(&d.host_b, &request_b) => assert!(matches!(ok(response), ResponseOk::Next(NextEvent::Callout { .. }))),
    }
    (a, b)
}

async fn summaries(host: &HostTarget) -> Vec<arena0_api::ExecSummary> {
    let ResponseOk::ExecList(entries) = ok(call(host, &HostRequest::ExecList).await) else {
        panic!("expected list");
    };
    entries
}

async fn subscribe(host: &HostTarget) -> (BufReader<OwnedReadHalf>, OwnedWriteHalf) {
    let stream = UnixStream::connect(&host.socket).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    arena0_api::frame::write_frame(
        &mut write,
        &host.request(&HostRequest::EventsSubscribe {
            filter: EventFilter {
                include: vec![],
                exclude: vec![],
            },
        }),
    )
    .await
    .unwrap();
    let ack: Response = arena0_api::frame::read_frame(&mut read)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(ok(ack), ResponseOk::Subscribed));
    (read, write)
}

async fn ended(events: &mut BufReader<OwnedReadHalf>, exec_id: ExecId) {
    // Terminal receipt publication precedes the end handshake. Wait for its
    // durable final transition before comparing multiple independent reads.
    tokio::time::timeout(LIVE_EXECUTION_TIMEOUT, async {
        loop {
            let event: EventFrame = arena0_api::frame::read_frame(events)
                .await
                .unwrap()
                .unwrap();
            if event.exec_id == Some(exec_id)
                && matches!(
                    event.data,
                    EventData::SessionEndProgress {
                        phase: ExecEndPhase::Ended,
                        ..
                    }
                )
            {
                break;
            }
        }
    })
    .await
    .expect("end handshake reached durable Ended phase");
}

async fn assert_projection(host: &HostTarget) {
    for entry in summaries(host).await {
        let ResponseOk::Status(status) = ok(call(
            host,
            &HostRequest::ExecStatus {
                exec_id: entry.exec_id,
            },
        )
        .await) else {
            panic!("expected status");
        };
        let ResponseOk::Inspection(inspection) = ok(call(
            host,
            &HostRequest::ExecInspect {
                exec_id: entry.exec_id,
                events_from: None,
                events_limit: 16,
            },
        )
        .await) else {
            panic!("expected inspect");
        };
        assert_eq!(entry.exec_id, status.exec_id);
        assert_eq!(entry.negotiation_id, status.negotiation_id);
        assert_eq!(entry.program_id, status.program_id);
        assert_eq!(entry.lifecycle, status.lifecycle());
        assert_eq!(entry.session_id, status.session_id());
        assert_eq!(entry.created_at_ms, status.created_at_ms);
        assert_eq!(entry.updated_at_ms, status.updated_at_ms);
        assert_eq!(entry.end, status.end);
        assert_eq!(entry.activation, inspection.activation);
        let session = status.session();
        assert_eq!(entry.step, session.map(|session| session.step));
        assert_eq!(
            entry.participants,
            session.map(|session| session.participants)
        );
        assert_eq!(
            entry.peers,
            session
                .map(|session| session.peers.clone())
                .unwrap_or_default()
        );
        assert_eq!(
            entry.receipt_available,
            session.is_some_and(|session| session.receipt_available)
        );
        assert_eq!(entry.turn, session.and_then(|session| session.turn));
        assert_eq!(
            entry.phase,
            session.and_then(|session| session.phase.clone())
        );
        match (&entry.pending_callout, status.pending_callout()) {
            (None, None) => {}
            (Some(summary), Some(detail)) => {
                assert_eq!(summary.pending_id, detail.pending_id);
                assert_eq!(summary.callout_index, detail.callout_index);
                assert_eq!(summary.name, detail.name);
                assert!(summary.opened_at_ms >= entry.created_at_ms);
                assert!(summary.opened_at_ms <= entry.updated_at_ms);
            }
            other => panic!("callout projection disagrees: {other:?}"),
        }
        let reason = match &status.state {
            ExecStatusState::Aborted { reason, .. } => Some(reason.clone()),
            ExecStatusState::Failed { reason, .. } => reason.clone(),
            _ => None,
        };
        assert_eq!(entry.reason, reason);
        let outcome = match &status.state {
            ExecStatusState::Completed { outcome, .. } => outcome.clone(),
            _ => None,
        };
        assert_eq!(entry.outcome, outcome);
        // Step timestamps come from certified trace records, rather than a
        // second copy of the store's projection algorithm.
        if let Some(step) = entry.step {
            let ResponseOk::Trace(steps) = ok(call(
                host,
                &HostRequest::ExecTrace {
                    exec_id: entry.exec_id,
                    from: 0,
                    to: u64::MAX,
                },
            )
            .await) else {
                panic!("expected trace");
            };
            assert_eq!(
                entry.last_step_at_ms,
                steps
                    .iter()
                    .find(|record| Some(record.entry.step) == step.checked_sub(1))
                    .map(|record| record.certified_at_ms)
            );
        } else {
            assert_eq!(entry.last_step_at_ms, None);
        }
    }
}

async fn reopen(d: &mut TestDaemon) {
    if d.service.is_some() {
        d.stop().await;
    }
    let home = Home::from_root(d._home.path().to_path_buf()).unwrap();
    d.service = Some(start_service(home, vec![d.host_a.name.clone(), d.host_b.name.clone()]).await);
    common::wait_for_socket(&d.socket).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_matches_detail_through_negotiating_active_completed_aborted_and_failed_request() {
    let _scenario = SCENARIO.lock().await;
    let trace = trace();
    trace.clear();
    let mut d = daemon(&rps_wasm()).await;
    let (mut events_a, _write_a) = subscribe(&d.host_a).await;
    let (mut events_b, _write_b) = subscribe(&d.host_b).await;
    let (a, negotiation) = new_creator(&d, 0xa1).await;
    assert_projection(&d.host_a).await;
    assert_eq!(
        summaries(&d.host_a).await[0].lifecycle,
        ExecLifecycle::Negotiating
    );
    let b = join(&d, negotiation, 0xb1).await;
    for (host, exec_id) in [(&d.host_a, a), (&d.host_b, b)] {
        ok(call(
            host,
            &HostRequest::ExecAwait {
                exec_id,
                until: AwaitState::Active,
            },
        )
        .await);
    }
    let request_a = HostRequest::ExecNext { exec_id: a };
    let request_b = HostRequest::ExecNext { exec_id: b };
    tokio::select! {
        response = call(&d.host_a, &request_a) => assert!(matches!(ok(response), ResponseOk::Next(NextEvent::Callout { .. }))),
        response = call(&d.host_b, &request_b) => assert!(matches!(ok(response), ResponseOk::Next(NextEvent::Callout { .. }))),
    }
    assert_projection(&d.host_a).await;
    assert_projection(&d.host_b).await;
    let active = [summaries(&d.host_a).await, summaries(&d.host_b).await];
    assert!(
        active
            .iter()
            .flatten()
            .all(|entry| entry.lifecycle == ExecLifecycle::Active)
    );
    assert!(
        active
            .iter()
            .flatten()
            .any(|entry| entry.pending_callout.is_some())
    );
    tokio::join!(drive(&d.host_a, a), drive(&d.host_b, b));
    tokio::join!(ended(&mut events_a, a), ended(&mut events_b, b));
    assert_projection(&d.host_a).await;
    assert_projection(&d.host_b).await;
    assert_eq!(
        summaries(&d.host_a)
            .await
            .iter()
            .find(|entry| entry.exec_id == a)
            .unwrap()
            .lifecycle,
        ExecLifecycle::Completed
    );
    let (abort, _) = active_pair(&d, 0xc1).await;
    ok(call(
        &d.host_a,
        &HostRequest::ExecTerminate {
            exec_id: abort,
            reason: "summary abort".into(),
        },
    )
    .await);
    ok(call(
        &d.host_a,
        &HostRequest::ExecAwait {
            exec_id: abort,
            until: AwaitState::Terminal,
        },
    )
    .await);
    ended(&mut events_a, abort).await;
    assert_projection(&d.host_a).await;
    assert_eq!(
        summaries(&d.host_a)
            .await
            .iter()
            .find(|entry| entry.exec_id == abort)
            .unwrap()
            .lifecycle,
        ExecLifecycle::Aborted
    );
    let (failed, _) = new_creator(&d, 0xd1).await;
    ok(call(&d.host_a, &HostRequest::ExecWithdraw { exec_id: failed }).await);
    assert_projection(&d.host_a).await;
    assert_eq!(
        summaries(&d.host_a)
            .await
            .iter()
            .find(|entry| entry.exec_id == failed)
            .unwrap()
            .lifecycle,
        ExecLifecycle::Failed
    );
    d.stop().await;
    trace.save("projection");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_list_decodes_only_live_turns_and_warm_lists_and_counters_decode_nothing() {
    let _scenario = SCENARIO.lock().await;
    let trace = trace();
    trace.clear();
    let mut d = daemon(&rps_wasm()).await;
    let (mut events_a, _write_a) = subscribe(&d.host_a).await;
    let (mut events_b, _write_b) = subscribe(&d.host_b).await;
    let (completed_a, completed_b) = active_pair(&d, 0xc2).await;
    tokio::join!(drive(&d.host_a, completed_a), drive(&d.host_b, completed_b));
    tokio::join!(
        ended(&mut events_a, completed_a),
        ended(&mut events_b, completed_b)
    );
    let (a, _) = active_pair(&d, 0xa2).await;
    trace.clear();
    reopen(&mut d).await;
    trace.clear();
    let entries = summaries(&d.host_a).await;
    assert!(
        entries
            .iter()
            .any(|entry| entry.lifecycle == ExecLifecycle::Completed),
        "exercise a terminal row in the cold list"
    );
    let expected = entries
        .iter()
        .filter(|entry| !entry.lifecycle.is_terminal() && entry.step.is_some())
        .count();
    assert!(expected > 0, "exercise cold turn projection");
    assert_eq!(
        trace.decodes("exec.list"),
        vec!["decode.execution_state"; expected]
    );
    trace.clear();
    summaries(&d.host_a).await;
    assert!(
        trace.decodes("exec.list").is_empty(),
        "warm list: {:?}",
        trace.decodes("exec.list")
    );
    trace.clear();
    let ResponseOk::ReceiptList(receipts) = ok(call(&d.host_a, &HostRequest::ReceiptList).await)
    else {
        panic!("expected receipt list");
    };
    assert!(!receipts.is_empty(), "exercise existing receipt artifacts");
    assert!(
        trace.decodes("receipt.list").is_empty(),
        "receipt list: {:?}",
        trace.decodes("receipt.list")
    );
    trace.clear();
    ok(call_daemon(&d.socket, &Request::HostsList).await);
    assert!(
        trace.decodes("hosts.list").is_empty(),
        "Host counters: {:?}",
        trace.decodes("hosts.list")
    );
    trace.clear();
    ok(call(&d.host_a, &HostRequest::ExecStatus { exec_id: a }).await);
    assert!(
        !trace.decodes("exec.status").is_empty(),
        "detail must decode and validate"
    );
    d.stop().await;
    trace.save("decode-counts");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn open_callout_time_survives_restart_of_the_same_home() {
    let _scenario = SCENARIO.lock().await;
    let trace = trace();
    trace.clear();
    let mut d = daemon(&rps_wasm()).await;
    active_pair(&d, 0xa3).await;
    let before = [summaries(&d.host_a).await, summaries(&d.host_b).await]
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            entry
                .pending_callout
                .map(|callout| (entry.exec_id, callout))
        })
        .collect::<Vec<_>>();
    assert!(!before.is_empty(), "exercise an open callout");
    trace.clear();
    reopen(&mut d).await;
    let after = [summaries(&d.host_a).await, summaries(&d.host_b).await]
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            entry
                .pending_callout
                .map(|callout| (entry.exec_id, callout))
        })
        .collect::<Vec<_>>();
    assert_eq!(before, after);
    d.stop().await;
    trace.save("callout-restart");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn archived_corrupt_state_is_validated_on_detail_read_only() {
    let _scenario = SCENARIO.lock().await;
    let trace = trace();
    trace.clear();
    let mut d = daemon(&rps_wasm()).await;
    let (mut events_a, _write_a) = subscribe(&d.host_a).await;
    let (mut events_b, _write_b) = subscribe(&d.host_b).await;
    let (bad, b) = active_pair(&d, 0xa4).await;
    tokio::join!(drive(&d.host_a, bad), drive(&d.host_b, b));
    tokio::join!(ended(&mut events_a, bad), ended(&mut events_b, b));
    let (good, _) = new_creator(&d, 0xc4).await;
    ok(call(&d.host_a, &HostRequest::ExecWithdraw { exec_id: good }).await);
    let before = summaries(&d.host_a).await;
    assert_eq!(
        before
            .iter()
            .find(|entry| entry.exec_id == bad)
            .unwrap()
            .lifecycle,
        ExecLifecycle::Completed
    );
    d.stop().await;
    let home = Home::from_root(d._home.path().to_path_buf()).unwrap();
    // Deliberate disk corruption while the daemon is stopped is the failure
    // boundary being tested; it cannot race an actor transaction.
    let directory = home.host(&d.host_a.name);
    let database = std::fs::read_dir(directory.state_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension().is_some_and(|extension| {
                extension == "sqlite3" || extension == "sqlite" || extension == "db"
            })
        })
        .expect("Host SQLite database");
    let changed = rusqlite::Connection::open(&database)
        .expect("open the Host database")
        .execute(
            "UPDATE executions SET state = X'00' WHERE execution_id = ?1",
            [&bad.0[..]],
        )
        .expect("corrupt the archived state");
    assert_eq!(changed, 1);
    trace.clear();
    reopen(&mut d).await;
    let error = call(&d.host_a, &HostRequest::ExecStatus { exec_id: bad })
        .await
        .unwrap_err();
    assert_eq!(error.code, ApiErrorCode::Storage);
    let after = summaries(&d.host_a).await;
    assert_eq!(
        before, after,
        "every indexed execution survives archived-state corruption"
    );
    assert!(after.iter().any(|entry| entry.exec_id == good));
    assert!(after.iter().any(|entry| entry.exec_id == bad));
    d.stop().await;
    trace.save("archived-corruption");
}
