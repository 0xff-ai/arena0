use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use arena0_api::{ApiErrorCode, EnsembleSpec, HostRequest, Request, Response, ResponseOk};
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
        arena0_test_engine::shared_test_engine(),
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
async fn id_show_returns_the_host_identity() {
    let (home, daemon, serving) = start(&["a", "b"]).await;
    let socket = home.path().join("arena0.sock");
    let info = call(&socket, &host("a", HostRequest::Info)).await;
    let status = match info {
        Ok(ResponseOk::HostStatus(status)) => status,
        info => panic!("unexpected Host info response: {info:?}"),
    };
    let id = match call(&socket, &host("a", HostRequest::IdShow)).await {
        Ok(ResponseOk::Id(id)) => id,
        response => panic!("unexpected id.show response: {response:?}"),
    };
    assert_eq!(id.peer_id, status.host.peer_id);
    assert_eq!(id.transport_key, status.transport_key);
    assert_ne!(
        id.peer_id,
        match call(&socket, &host("b", HostRequest::IdShow)).await {
            Ok(ResponseOk::Id(id)) => id.peer_id,
            response => panic!("unexpected id.show response: {response:?}"),
        }
    );
    stop(home, daemon, serving).await;
}

#[tokio::test]
async fn shared_unix_api_classifies_program_input_errors() {
    let (home, daemon, serving) = start(&["host-01", "host-02"]).await;
    let socket = home.path().join("arena0.sock");
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
async fn variable_size_program_accepts_supported_creator_count() {
    let (home, daemon, serving) = start(&["host-01", "host-02", "host-03"]).await;
    let socket = home.path().join("arena0.sock");
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
                ensemble: EnsembleSpec::Create {
                    participant_count: 3,
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
                ensemble: EnsembleSpec::Create {
                    participant_count: 3,
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
