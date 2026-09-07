use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use arena0_api::{ApiErrorCode, EnsembleSpec, HostRequest, IdRef, Request, Response, ResponseOk};
use arena0_daemon::{Daemon, McpConfig};
use arena0_program::ParticipantCount;
use tempfile::TempDir;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::time::{sleep, timeout};

async fn call(socket: &Path, request: &Request) -> Response {
    let stream = UnixStream::connect(socket)
        .await
        .expect("ensemble daemon socket should accept connections");
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    arena0_api::frame::write_frame(&mut write, request)
        .await
        .expect("write daemon request");
    arena0_api::frame::read_frame::<_, Response>(&mut read)
        .await
        .expect("read daemon response")
        .expect("daemon response frame")
}

fn host(name: &str, request: HostRequest) -> Request {
    Request::Host {
        host: name.to_owned(),
        request,
    }
}

async fn wait_for_socket(socket: &Path) {
    timeout(Duration::from_secs(5), async {
        loop {
            if UnixStream::connect(socket).await.is_ok() {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("ensemble daemon socket should become ready");
}

async fn start(
    names: &[&str],
) -> (
    TempDir,
    Arc<Daemon>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let home_dir = TempDir::new().expect("temporary daemon home");
    let home = arena0_home::Home::from_root(home_dir.path().to_path_buf()).unwrap();
    let daemon = Daemon::start(
        names.iter().map(|name| name.parse().unwrap()).collect(),
        McpConfig::new("127.0.0.1:0".parse().unwrap(), None).unwrap(),
        Arc::new(arena0_sandbox::WasmtimeEngine::new().expect("sandbox engine")),
        home,
        true,
    )
    .await
    .expect("start ensemble");
    let serving = tokio::spawn(Arc::clone(&daemon).serve());
    wait_for_socket(&home_dir.path().join("arena0.sock")).await;
    (home_dir, daemon, serving)
}

async fn stop(
    home: TempDir,
    daemon: Arc<Daemon>,
    serving: tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let socket = home.path().join("arena0.sock");
    daemon.stop().await;
    serving
        .await
        .expect("ensemble serve task should join")
        .expect("ensemble serve should stop cleanly");
    assert!(!socket.exists());
}

#[tokio::test]
async fn routes_two_hosts_through_one_socket() {
    let (home, daemon, serving) = start(&["a", "b"]).await;
    let socket = home.path().join("arena0.sock");

    let response = call(&socket, &Request::HostsList).await;
    let hosts = match response {
        Ok(ResponseOk::Hosts(hosts)) => hosts,
        response => panic!("unexpected hosts.list response: {response:?}"),
    };
    assert_eq!(hosts.len(), 2);
    assert_eq!(hosts[0].host.id, "a");
    assert_eq!(hosts[1].host.id, "b");
    assert_ne!(hosts[0].host.peer_id, hosts[1].host.peer_id);

    for name in ["a", "b"] {
        let response = call(&socket, &host(name, HostRequest::Info)).await;
        assert!(
            matches!(response, Ok(ResponseOk::HostStatus(_))),
            "{response:?}"
        );
    }
    let unknown = call(&socket, &host("missing", HostRequest::Info)).await;
    assert_eq!(
        unknown.expect_err("unknown Host must be rejected").code,
        ApiErrorCode::NotFound
    );

    stop(home, daemon, serving).await;
}

#[tokio::test]
async fn active_identity_cannot_be_removed_over_shared_unix_api() {
    let (home, daemon, serving) = start(&["a", "b"]).await;
    let socket = home.path().join("arena0.sock");
    let info = call(&socket, &host("a", HostRequest::Info)).await;
    let status = match info {
        Ok(ResponseOk::HostStatus(status)) => status,
        info => panic!("unexpected Host info response: {info:?}"),
    };
    let response = call(
        &socket,
        &host(
            "a",
            HostRequest::IdRemove {
                id: IdRef::Peer(status.host.peer_id),
            },
        ),
    )
    .await;
    let error = response.expect_err("active identity removal must be rejected");
    assert_eq!(error.code, ApiErrorCode::BadRequest);
    assert!(error.message.contains("active Host identity"));
    stop(home, daemon, serving).await;
}

#[tokio::test]
async fn shared_unix_api_classifies_identity_and_program_input_errors() {
    let (home, daemon, serving) = start(&["host-01", "host-02"]).await;
    let socket = home.path().join("arena0.sock");
    let missing = call(
        &socket,
        &host(
            "host-01",
            HostRequest::IdShow {
                id: IdRef::Label("missing".into()),
            },
        ),
    )
    .await
    .expect_err("missing identity should be typed");
    assert_eq!(missing.code, ApiErrorCode::NotFound);

    let invalid_label = call(
        &socket,
        &host(
            "host-01",
            HostRequest::IdNew {
                label: Some("bad\u{1b}label".into()),
            },
        ),
    )
    .await
    .expect_err("invalid identity label should be typed");
    assert_eq!(invalid_label.code, ApiErrorCode::BadRequest);

    let invalid_program = call(
        &socket,
        &host(
            "host-01",
            HostRequest::ProgramImport {
                wasm: b"Cargo.toml".to_vec(),
            },
        ),
    )
    .await
    .expect_err("invalid Wasm should be typed");
    assert_eq!(invalid_program.code, ApiErrorCode::BadRequest);

    stop(home, daemon, serving).await;
}

#[tokio::test]
async fn variable_size_program_accepts_supported_explicit_ensemble() {
    let (home, daemon, serving) = start(&["host-01", "host-02", "host-03"]).await;
    let socket = home.path().join("arena0.sock");
    let mut peers = Vec::new();
    for name in ["host-01", "host-02", "host-03"] {
        let response = call(&socket, &host(name, HostRequest::IdList)).await;
        let ids = match response {
            Ok(ResponseOk::IdList(ids)) => ids,
            response => panic!("unexpected identity response: {response:?}"),
        };
        peers.push(ids[0].peer_id);
    }
    let response = call(&socket, &host("host-01", HostRequest::ProgramList)).await;
    let programs = match response {
        Ok(ResponseOk::ProgramList(programs)) => programs,
        response => panic!("unexpected program response: {response:?}"),
    };
    assert_eq!(programs.len(), 7);
    let cumulative = programs
        .iter()
        .find(|program| program.name == "cumulative-sum")
        .expect("bundled cumulative-sum program");
    assert_eq!(
        cumulative.participants,
        ParticipantCount::Range { min: 2, max: 64 }
    );

    let fixed_size = call(
        &socket,
        &host(
            "host-01",
            HostRequest::ExecNew {
                exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
                program: "rock-paper-scissors".into(),
                params: None,
                ensemble: EnsembleSpec::Explicit {
                    peers: peers[1..].to_vec(),
                },
            },
        ),
    )
    .await
    .expect_err("fixed-size program must reject a three-Host ensemble");
    assert_eq!(fixed_size.code, ApiErrorCode::BadRequest);

    let response = call(
        &socket,
        &host(
            "host-01",
            HostRequest::ExecNew {
                exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
                program: "cumulative-sum".into(),
                params: Some(serde_json::json!({ "target_size": 3 })),
                ensemble: EnsembleSpec::Explicit {
                    peers: peers[1..].to_vec(),
                },
            },
        ),
    )
    .await;
    assert!(
        matches!(response, Ok(ResponseOk::ExecCreated { .. })),
        "{response:?}"
    );

    stop(home, daemon, serving).await;
}
