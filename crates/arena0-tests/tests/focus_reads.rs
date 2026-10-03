//! Public focus reads on this checkout's real daemon. Every restart owns an
//! isolated home and OS-assigned port. Historical views read certified stored
//! post-states and retain their rendering across restarts and unrelated damage.

mod common;

use std::io::{self, Write};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use arena0_api::{
    ApiErrorCode, AwaitState, EnsembleSpec, HostRequest, NextEvent, RefKind, Resolved, ResponseOk,
};
use arena0_daemon::{Daemon, HttpConfig};
use arena0_home::{Home, HostName};
use arena0_protocol::{ColorDepth, ExecId, NegotiationTarget};
use common::{HostTarget, call, created, ok};
use serde_json::Value;
use tracing_subscriber::prelude::*;

#[derive(Clone, Default)]
struct Trace(Arc<Mutex<Vec<u8>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Trace {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

impl Write for Trace {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Trace {
    fn events(&self, method: &str, operation: &str) -> Vec<Value> {
        String::from_utf8(self.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|event| {
                event["target"] == "arena0::performance"
                    && event["fields"]["operation"]
                        .as_str()
                        .is_some_and(|op| op.starts_with(operation))
                    && event["spans"].as_array().is_some_and(|spans| {
                        spans
                            .iter()
                            .any(|span| span["name"] == "host_request" && span["method"] == method)
                    })
            })
            .collect()
    }
}

// Save even on assertion failure, including the request spans needed to
// distinguish cache misses and background actor work from the measured read.
struct Artifact(Trace);
impl Drop for Artifact {
    fn drop(&mut self) {
        let directory = tempfile::Builder::new()
            .prefix("arena0-focus-reads-")
            .tempdir()
            .unwrap()
            .keep();
        let path = directory.join("trace.jsonl");
        std::fs::write(&path, self.0.0.lock().unwrap().as_slice()).unwrap();
        eprintln!("focus-read trace: {}", path.display());
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    a: HostTarget,
    b: HostTarget,
    service: Option<(Arc<Daemon>, tokio::task::JoinHandle<()>)>,
}

impl Fixture {
    async fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = Home::from_root(directory.path().to_path_buf())
            .unwrap()
            .socket();
        let mut fixture = Self {
            directory,
            a: HostTarget {
                socket: socket.clone(),
                name: HostName::try_from("a").unwrap(),
            },
            b: HostTarget {
                socket,
                name: HostName::try_from("b").unwrap(),
            },
            service: None,
        };
        fixture.boot().await;
        fixture
    }

    async fn boot(&mut self) {
        let home = Home::from_root(self.directory.path().to_path_buf()).unwrap();
        let daemon = Daemon::start(
            vec![self.a.name.clone(), self.b.name.clone()],
            HttpConfig::new(SocketAddr::from(([127, 0, 0, 1], 0)), None).unwrap(),
            arena0_test_engine::shared_test_engine(),
            home,
            true,
        )
        .await
        .unwrap();
        let serving = Arc::clone(&daemon);
        let task = tokio::spawn(async move {
            serving.serve().await.unwrap();
        });
        self.service = Some((daemon, task));
        common::wait_for_socket(&self.a.socket).await;
    }

    async fn stop(&mut self) {
        let (daemon, task) = self.service.take().unwrap();
        daemon.stop().await;
        task.await.unwrap();
        drop(daemon);
    }

    async fn restart(&mut self) {
        self.stop().await;
        self.boot().await;
    }
}

async fn first_legal(target: &HostTarget, exec_id: ExecId) -> arena0_protocol::SessionHash {
    let mut answered = None;
    let mut progress_deadline =
        tokio::time::Instant::now() + arena0_tests::fixtures::LIVE_EXECUTION_TIMEOUT;
    loop {
        // Bound stalled turns while allowing a game spanning more than one
        // hundred certified steps. Each distinct answered callout renews the
        // progress deadline; repeated observations of the same callout do not.
        match ok(tokio::time::timeout_at(
            progress_deadline,
            call(target, &HostRequest::ExecNext { exec_id }),
        )
        .await
        .expect("chess agent makes progress"))
        {
            ResponseOk::Next(NextEvent::Callout {
                pending_id,
                context,
                ..
            }) => {
                if answered == Some(pending_id) {
                    tokio::task::yield_now().await;
                    continue;
                }
                let legal = context["legal_moves"].as_str().unwrap();
                let answer = legal
                    .split(',')
                    .next()
                    .expect("nonterminal board has a legal move")
                    .trim();
                ok(call(
                    target,
                    &HostRequest::ExecSubmit {
                        exec_id,
                        pending_id,
                        answer: Some(serde_json::json!(answer)),
                    },
                )
                .await);
                answered = Some(pending_id);
                progress_deadline =
                    tokio::time::Instant::now() + arena0_tests::fixtures::LIVE_EXECUTION_TIMEOUT;
            }
            ResponseOk::Next(NextEvent::Completed { session_id, .. }) => return session_id,
            other => panic!("chess agent: {other:?}"),
        }
    }
}

async fn view(target: &HostTarget, exec: ExecId, at_step: u64, cache: &str) -> ResponseOk {
    let started = std::time::Instant::now();
    let result = ok(call(
        target,
        &HostRequest::ExecView {
            exec,
            width: 80,
            color: ColorDepth::Mono,
            at_step: Some(at_step),
        },
    )
    .await);
    let elapsed_us = started.elapsed().as_micros() as u64;
    tracing::info!(target: "arena0::performance", operation = "view.latency", step = at_step, cache, elapsed_us);
    eprintln!("view step={at_step} cache={cache} elapsed_us={elapsed_us}");
    result
}

async fn resolve(target: &HostTarget, kind: RefKind, reference: &str) -> Resolved {
    let ResponseOk::Resolved(result) = ok(call(
        target,
        &HostRequest::Resolve {
            kind,
            reference: reference.into(),
        },
    )
    .await) else {
        panic!("expected resolution");
    };
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn focus_reads_preserve_pages_views_and_matching_without_detail_work() {
    let trace = Trace::default();
    tracing_subscriber::registry()
        .with(tracing_subscriber::filter::LevelFilter::DEBUG)
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(false)
                .with_writer(trace.clone()),
        )
        .try_init()
        .unwrap();
    let _artifact = Artifact(trace.clone());
    let mut d = Fixture::start().await;
    let wasm = common::chess_wasm();
    let program = common::import(&d.a, &wasm).await;
    assert_eq!(common::import(&d.b, &wasm).await, program);
    let ResponseOk::HostStatus(info) = ok(call(&d.a, &HostRequest::Info).await) else {
        panic!("host info");
    };
    let a = ExecId([0xa1; 32]);
    let b = ExecId([0xb1; 32]);
    let ResponseOk::ExecCreated {
        negotiation_id: Some(negotiation),
        ..
    } = ok(call(
        &d.a,
        &HostRequest::ExecNew {
            exec_id: a,
            program: program.to_string(),
            params: Some(Value::Null),
            ensemble: EnsembleSpec::Create {
                participant_count: 2,
            },
            blobs: vec![],
        },
    )
    .await)
    else {
        panic!("creator");
    };
    assert_eq!(
        created(
            call(
                &d.b,
                &HostRequest::ExecNew {
                    exec_id: b,
                    program: program.to_string(),
                    params: None,
                    ensemble: EnsembleSpec::Join {
                        target: Some(NegotiationTarget::new(info.host.peer_id, negotiation))
                    },
                    blobs: vec![],
                }
            )
            .await
        ),
        b
    );
    for (host, exec_id) in [(&d.a, a), (&d.b, b)] {
        ok(call(
            host,
            &HostRequest::ExecAwait {
                exec_id,
                until: AwaitState::Active,
            },
        )
        .await);
    }
    let (session, other) = tokio::join!(first_legal(&d.a, a), first_legal(&d.b, b));
    assert_eq!(session, other);
    let ResponseOk::Trace(entries) = ok(call(
        &d.a,
        &HostRequest::ExecTrace {
            exec_id: a,
            from: 0,
            to: u64::MAX,
        },
    )
    .await) else {
        panic!("trace");
    };
    let last = entries.last().unwrap().entry.step;
    assert!(last >= 104, "fixture must exercise step 104, last={last}");

    let mut warmed = Vec::new();
    for step in [0, 15, 16, 17, 50, last] {
        warmed.push((step, view(&d.a, a, step, "warm").await));
    }
    let latest = ok(call(
        &d.a,
        &HostRequest::ExecView {
            exec: a,
            width: 80,
            color: ColorDepth::Mono,
            at_step: None,
        },
    )
    .await);
    assert_eq!(warmed.last().unwrap().1, latest);
    // Each cold request is the first historical view after restarting on the
    // same durable game. The program compilation cache stays process-wide.
    for (step, expected) in &warmed {
        d.restart().await;
        assert_eq!(
            &view(&d.a, a, *step, "cold").await,
            expected,
            "historical step {step}"
        );
    }
    d.restart().await;
    view(&d.a, a, 104, "cold").await;
    view(&d.a, a, 104, "warm").await;
    d.stop().await;
    let database = Home::from_root(d.directory.path().to_path_buf())
        .unwrap()
        .host(&d.a.name)
        .state_dir()
        .join("arena0.sqlite");
    // Copy a different, valid stored envelope while no actor owns the file.
    // Matching the expected hash is the trust boundary, not envelope validity.
    let output = std::process::Command::new("sqlite3").arg(&database).arg(format!(
        "UPDATE step_states SET shared_state = (SELECT s.shared_state FROM step_states s JOIN agreed_steps a USING(execution_id, step) WHERE s.execution_id = X'{id}' AND a.post_state != (SELECT post_state FROM agreed_steps WHERE execution_id = X'{id}' AND step = 17) LIMIT 1) WHERE execution_id = X'{id}' AND step = 17; SELECT changes();", id = hex::encode(a.0)))
        .output().expect("sqlite3 corruption fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "1");
    d.boot().await;
    assert_eq!(
        call(
            &d.a,
            &HostRequest::ExecView {
                exec: a,
                width: 80,
                color: ColorDepth::Mono,
                at_step: Some(17),
            }
        )
        .await
        .unwrap_err()
        .code,
        ApiErrorCode::Storage
    );
    for (step, expected) in &warmed {
        if *step != 17 {
            assert_eq!(&view(&d.a, a, *step, "damaged-neighbor").await, expected);
        }
    }

    for (from, limit) in [(0, 1), (15, 16), (50, 32), (u64::MAX, 1)] {
        let ResponseOk::Inspection(inspection) = ok(call(
            &d.a,
            &HostRequest::ExecInspect {
                exec_id: a,
                events_from: Some(from),
                events_limit: limit,
            },
        )
        .await) else {
            panic!("inspection");
        };
        let ResponseOk::Records(page) = ok(call(
            &d.a,
            &HostRequest::ExecRecords {
                exec_id: a,
                from,
                limit,
            },
        )
        .await) else {
            panic!("records");
        };
        assert_eq!(page.from, inspection.events_from);
        assert_eq!(page.records, inspection.events);
        assert_eq!(page.total, inspection.events_total);
        assert_eq!(page.next, inspection.events_next);
    }
    assert!(trace.events("exec.records", "decode.").is_empty());
    // A positive control proves request attribution did observe detail work.
    assert!(
        !trace
            .events("exec.inspect", "decode.execution_state")
            .is_empty()
    );
    for limit in [0, 257] {
        assert_eq!(
            call(
                &d.a,
                &HostRequest::ExecRecords {
                    exec_id: a,
                    from: 0,
                    limit
                }
            )
            .await
            .unwrap_err()
            .code,
            ApiErrorCode::BadRequest
        );
    }
    assert_eq!(
        call(
            &d.a,
            &HostRequest::ExecRecords {
                exec_id: ExecId([0xff; 32]),
                from: 0,
                limit: 1
            }
        )
        .await
        .unwrap_err()
        .code,
        ApiErrorCode::NotFound
    );

    let ResponseOk::ReceiptList(receipts) = ok(call(&d.a, &HostRequest::ReceiptList).await) else {
        panic!("receipts");
    };
    let receipt = receipts
        .into_iter()
        .find(|r| r.session_id == session)
        .unwrap();
    for (kind, full, expected) in [
        (RefKind::Exec, a.to_string(), Resolved::Exec { exec_id: a }),
        (
            RefKind::Session,
            session.to_string(),
            Resolved::Session {
                session_id: session,
            },
        ),
        (
            RefKind::Receipt,
            receipt.receipt_id.clone(),
            Resolved::Receipt { entry: receipt },
        ),
    ] {
        for length in [1, 3, 4, 64] {
            assert_eq!(resolve(&d.a, kind, &full[..length]).await, expected);
            assert_eq!(
                resolve(&d.a, kind, &format!(" {} ", full[..length].to_uppercase())).await,
                expected
            );
        }
        for invalid in ["", "xyz", "ab?", &"f".repeat(65), &"ff".repeat(32)] {
            assert_eq!(resolve(&d.a, kind, invalid).await, Resolved::None);
        }
    }
    // Seventeen different ids guarantee a shared first nibble. Independent
    // list contents are the oracle for total matches and candidate order.
    for byte in 1..=17u8 {
        created(
            call(
                &d.a,
                &HostRequest::ExecNew {
                    exec_id: ExecId([byte; 32]),
                    program: program.to_string(),
                    params: Some(Value::Null),
                    ensemble: EnsembleSpec::Create {
                        participant_count: 2,
                    },
                    blobs: vec![],
                },
            )
            .await,
        );
    }
    let ResponseOk::ExecList(executions) = ok(call(&d.a, &HostRequest::ExecList).await) else {
        panic!("executions");
    };
    let mut ids = executions
        .iter()
        .map(|entry| entry.exec_id.to_string())
        .filter(|id| id.starts_with('0'))
        .collect::<Vec<_>>();
    ids.sort();
    assert!(ids.len() > 8);
    assert_eq!(
        resolve(&d.a, RefKind::Exec, "0").await,
        Resolved::Ambiguous {
            candidates: ids[..8].to_vec(),
            matches: ids.len() as u64
        }
    );
    assert_eq!(
        resolve(&d.a, RefKind::Exec, &ids[0]).await,
        Resolved::Exec {
            exec_id: ids[0].parse().unwrap()
        }
    );
    // Session and receipt ambiguities need real published artifacts. At most
    // seventeen sessions guarantee collisions in both first-nibble spaces.
    // Use a fresh transport for this independent journey: the oracle restarts
    // above exercise persisted reads, not renegotiation after peer shutdown.
    d.stop().await;
    let mut d = Fixture::start().await;
    let ResponseOk::HostStatus(info) = ok(call(&d.a, &HostRequest::Info).await) else {
        panic!("host info");
    };
    let rps = common::rps_wasm();
    let rps_program = common::import(&d.a, &rps).await;
    assert_eq!(common::import(&d.b, &rps).await, rps_program);
    let mut proved = [false; 2];
    for index in 1..=17u8 {
        let exec_a = ExecId([0x80 + index; 32]);
        let exec_b = ExecId([0xc0 + index; 32]);
        let ResponseOk::ExecCreated {
            negotiation_id: Some(negotiation),
            ..
        } = ok(call(
            &d.a,
            &HostRequest::ExecNew {
                exec_id: exec_a,
                program: rps_program.to_string(),
                params: Some(Value::Null),
                ensemble: EnsembleSpec::Create {
                    participant_count: 2,
                },
                blobs: vec![],
            },
        )
        .await)
        else {
            panic!("RPS creator");
        };
        created(
            call(
                &d.b,
                &HostRequest::ExecNew {
                    exec_id: exec_b,
                    program: rps_program.to_string(),
                    params: None,
                    ensemble: EnsembleSpec::Join {
                        target: Some(NegotiationTarget::new(info.host.peer_id, negotiation)),
                    },
                    blobs: vec![],
                },
            )
            .await,
        );
        let (session_a, session_b) =
            tokio::time::timeout(arena0_tests::fixtures::LIVE_EXECUTION_TIMEOUT, async {
                tokio::join!(common::drive(&d.a, exec_a), common::drive(&d.b, exec_b))
            })
            .await
            .expect("RPS session must complete within the live execution deadline");
        assert_eq!(session_a, session_b);
        let ResponseOk::ReceiptList(entries) = ok(call(&d.a, &HostRequest::ReceiptList).await)
        else {
            panic!("published receipts");
        };
        for (space_index, kind) in [RefKind::Session, RefKind::Receipt].into_iter().enumerate() {
            if proved[space_index] {
                continue;
            }
            let mut ids = entries
                .iter()
                .map(|entry| match kind {
                    RefKind::Session => entry.session_id.to_string(),
                    RefKind::Receipt => entry.receipt_id.clone(),
                    RefKind::Exec => unreachable!(),
                })
                .collect::<Vec<_>>();
            ids.sort();
            ids.dedup();
            for nibble in "0123456789abcdef".chars() {
                let matches = ids
                    .iter()
                    .filter(|id| id.starts_with(nibble))
                    .cloned()
                    .collect::<Vec<_>>();
                if matches.len() < 2 {
                    continue;
                }
                assert_eq!(
                    resolve(&d.a, kind, &nibble.to_string()).await,
                    Resolved::Ambiguous {
                        candidates: matches.iter().take(8).cloned().collect(),
                        matches: matches.len() as u64,
                    }
                );
                proved[space_index] = true;
                break;
            }
        }
        if proved.iter().all(|done| *done) {
            break;
        }
    }
    assert_eq!(proved, [true, true]);
    d.stop().await;
}
