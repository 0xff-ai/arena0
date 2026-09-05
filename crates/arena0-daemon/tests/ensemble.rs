use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use arena0_api::{ApiErrorCode, EnsembleSpec, IdRef, Request, Response, ResponseOk};
use arena0_daemon::{Daemon, HostConfig, McpConfig, Paths};
use arena0_program::ParticipantCount;
use tempfile::TempDir;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::time::{sleep, timeout};

async fn daemon_info(socket: &Path) -> arena0_api::DaemonInfo {
    let response = call(socket, &Request::DaemonInfo)
        .await
        .expect("daemon.info response frame");
    match response {
        ResponseOk::DaemonInfo(info) => info,
        other => panic!("unexpected daemon.info response: {other:?}"),
    }
}

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
    .expect("ensemble daemon sockets should become ready");
}

#[tokio::test]
async fn serves_distinct_hosts_on_distinct_sockets() {
    let dir_a = TempDir::new().expect("temporary node home a");
    let dir_b = TempDir::new().expect("temporary node home b");
    let socket_a = dir_a.path().join("arena0.sock");
    let socket_b = dir_b.path().join("arena0.sock");

    let host_a = HostConfig::open(
        "a",
        Paths::new(dir_a.path().to_path_buf(), socket_a.clone()),
        true,
    )
    .expect("open node a");
    let host_b = HostConfig::open(
        "b",
        Paths::new(dir_b.path().to_path_buf(), socket_b.clone()),
        true,
    )
    .expect("open node b");
    let mcp = McpConfig::new("127.0.0.1:0".parse().unwrap(), None).unwrap();
    let engine = Arc::new(arena0_sandbox::WasmtimeEngine::new().expect("sandbox engine"));
    let supervisor = Daemon::start(vec![host_a, host_b], mcp, engine)
        .await
        .expect("start ensemble");

    let serving = tokio::spawn(Arc::clone(&supervisor).serve());
    wait_for_socket(&socket_a).await;
    wait_for_socket(&socket_b).await;

    let info_a = daemon_info(&socket_a).await;
    let info_b = daemon_info(&socket_b).await;
    assert_ne!(info_a.peer_id, info_b.peer_id);
    assert_eq!(info_a.socket, socket_a.display().to_string());
    assert_eq!(info_b.socket, socket_b.display().to_string());

    supervisor.stop().await;
    serving
        .await
        .expect("ensemble serve task should join")
        .expect("ensemble serve should stop cleanly");
    assert!(!socket_a.exists());
    assert!(!socket_b.exists());
}

#[tokio::test]
async fn active_identity_cannot_be_removed_over_unix_api() {
    let dir_a = TempDir::new().expect("temporary node home a");
    let dir_b = TempDir::new().expect("temporary node home b");
    let socket_a = dir_a.path().join("arena0.sock");
    let socket_b = dir_b.path().join("arena0.sock");

    let host_a = HostConfig::open(
        "a",
        Paths::new(dir_a.path().to_path_buf(), socket_a.clone()),
        true,
    )
    .expect("open node a");
    let active_peer = host_a.peer_id();
    let host_b = HostConfig::open(
        "b",
        Paths::new(dir_b.path().to_path_buf(), socket_b.clone()),
        true,
    )
    .expect("open node b");
    let mcp = McpConfig::new("127.0.0.1:0".parse().unwrap(), None).unwrap();
    let engine = Arc::new(arena0_sandbox::WasmtimeEngine::new().expect("sandbox engine"));
    let supervisor = Daemon::start(vec![host_a, host_b], mcp, engine)
        .await
        .expect("start ensemble");

    let serving = tokio::spawn(Arc::clone(&supervisor).serve());
    wait_for_socket(&socket_a).await;
    wait_for_socket(&socket_b).await;

    let response = call(
        &socket_a,
        &Request::IdRemove {
            id: arena0_api::IdRef::Peer(active_peer),
        },
    )
    .await;
    let error = response.expect_err("active identity removal must be rejected");
    assert!(
        error.message.contains("active Host identity"),
        "unexpected error: {error}"
    );
    let info = daemon_info(&socket_a).await;
    assert_eq!(info.peer_id, active_peer, "Host keeps its active identity");

    supervisor.stop().await;
    serving
        .await
        .expect("ensemble serve task should join")
        .expect("ensemble serve should stop cleanly");
}

#[tokio::test]
async fn unix_api_classifies_identity_and_program_input_errors() {
    let dir_a = TempDir::new().expect("temporary Host home a");
    let dir_b = TempDir::new().expect("temporary Host home b");
    let socket = dir_a.path().join("arena0.sock");
    let host_a = HostConfig::open(
        "host-01",
        Paths::new(dir_a.path().to_path_buf(), socket.clone()),
        true,
    )
    .expect("open Host a");
    let active_peer = host_a.peer_id();
    let host_b = HostConfig::open(
        "host-02",
        Paths::new(dir_b.path().to_path_buf(), dir_b.path().join("arena0.sock")),
        true,
    )
    .expect("open Host b");
    let mcp = McpConfig::new("127.0.0.1:0".parse().unwrap(), None).unwrap();
    let engine = Arc::new(arena0_sandbox::WasmtimeEngine::new().expect("sandbox engine"));
    let supervisor = Daemon::start(vec![host_a, host_b], mcp, engine)
        .await
        .expect("start daemon");

    let serving = tokio::spawn(Arc::clone(&supervisor).serve());
    wait_for_socket(&socket).await;

    let missing = call(
        &socket,
        &Request::IdShow {
            id: IdRef::Label("missing".into()),
        },
    )
    .await
    .expect_err("missing identity should be a typed API error");
    assert_eq!(missing.code, ApiErrorCode::NotFound);

    let invalid_label = call(
        &socket,
        &Request::IdNew {
            label: Some("bad\u{1b}label".into()),
        },
    )
    .await
    .expect_err("invalid identity label should be a typed API error");
    assert_eq!(invalid_label.code, ApiErrorCode::BadRequest);

    for _ in 0..2 {
        let response = call(
            &socket,
            &Request::IdNew {
                label: Some("duplicate".into()),
            },
        )
        .await;
        assert!(matches!(response, Ok(ResponseOk::Id(_))), "{response:?}");
    }
    let ambiguous = call(
        &socket,
        &Request::IdShow {
            id: IdRef::Label("duplicate".into()),
        },
    )
    .await
    .expect_err("ambiguous identity label should be a typed API error");
    assert_eq!(ambiguous.code, ApiErrorCode::Ambiguous);

    let active_removal = call(
        &socket,
        &Request::IdRemove {
            id: IdRef::Peer(active_peer),
        },
    )
    .await
    .expect_err("active identity removal should be a typed API error");
    assert_eq!(active_removal.code, ApiErrorCode::BadRequest);

    let invalid_program = call(
        &socket,
        &Request::ProgramImport {
            wasm: b"Cargo.toml".to_vec(),
        },
    )
    .await
    .expect_err("invalid Wasm should be a typed API error");
    assert_eq!(invalid_program.code, ApiErrorCode::BadRequest);

    let active_seed = dir_a
        .path()
        .join("keys")
        .join(format!("{active_peer}.seed"));
    std::fs::remove_file(active_seed).expect("remove test seed to simulate corruption");
    let storage_failure = call(&socket, &Request::IdList)
        .await
        .expect_err("keystore corruption should remain a storage error");
    assert_eq!(storage_failure.code, ApiErrorCode::Storage);

    supervisor.stop().await;
    serving
        .await
        .expect("daemon serve task should join")
        .expect("daemon serve should stop cleanly");
}

#[tokio::test]
async fn variable_size_program_accepts_supported_explicit_ensemble() {
    let dirs = [
        TempDir::new().expect("temporary host-01 home"),
        TempDir::new().expect("temporary host-02 home"),
        TempDir::new().expect("temporary host-03 home"),
    ];
    let sockets = dirs
        .iter()
        .map(|dir| dir.path().join("arena0.sock"))
        .collect::<Vec<_>>();
    let hosts = ["host-01", "host-02", "host-03"]
        .into_iter()
        .zip(&dirs)
        .zip(&sockets)
        .map(|((name, dir), socket)| {
            HostConfig::open(
                name,
                Paths::new(dir.path().to_path_buf(), socket.clone()),
                true,
            )
            .expect("open Host")
        })
        .collect();
    let mcp = McpConfig::new("127.0.0.1:0".parse().unwrap(), None).unwrap();
    let engine = Arc::new(arena0_sandbox::WasmtimeEngine::new().expect("sandbox engine"));
    let supervisor = Daemon::start(hosts, mcp, engine)
        .await
        .expect("start three-Host daemon");

    let serving = tokio::spawn(Arc::clone(&supervisor).serve());
    for socket in &sockets {
        wait_for_socket(socket).await;
    }

    let mut peers = Vec::with_capacity(sockets.len());
    for socket in &sockets {
        let response = call(socket, &Request::IdList).await;
        match response {
            Ok(ResponseOk::IdList(ids)) => peers.push(ids[0].peer_id),
            response => panic!("unexpected identity response: {response:?}"),
        }
    }
    let programs = call(&sockets[0], &Request::ProgramList)
        .await
        .expect("program.list response");
    let ResponseOk::ProgramList(programs) = programs else {
        panic!("unexpected program.list response: {programs:?}");
    };
    assert_eq!(
        programs.len(),
        7,
        "daemon embeds the complete launch catalog"
    );
    let cumulative = programs
        .iter()
        .find(|program| program.name == "cumulative-sum")
        .expect("bundled cumulative-sum program");
    assert_eq!(
        cumulative.participants,
        ParticipantCount::Range { min: 2, max: 64 },
        "N-party program exposes its supported admission range"
    );

    let fixed_size = call(
        &sockets[0],
        &Request::ExecNew {
            exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
            program: "rock-paper-scissors".into(),
            params: None,
            ensemble: EnsembleSpec::Explicit {
                peers: peers[1..].to_vec(),
            },
        },
    )
    .await
    .expect_err("fixed-size program must reject a three-Host ensemble");
    assert_eq!(fixed_size.code, ApiErrorCode::BadRequest);
    assert!(fixed_size.message.contains("program accepts 2"));

    let response = call(
        &sockets[0],
        &Request::ExecNew {
            exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
            program: "cumulative-sum".into(),
            params: Some(serde_json::json!({ "target_size": 3 })),
            ensemble: EnsembleSpec::Explicit {
                peers: peers[1..].to_vec(),
            },
        },
    )
    .await;
    let negotiation_id = match response {
        Ok(ResponseOk::ExecCreated { negotiation_id, .. }) => negotiation_id,
        response => panic!("supported variable-size admission rejected: {response:?}"),
    };
    let join_response = call(
        &sockets[1],
        &Request::ExecNew {
            exec_id: arena0_protocol::ExecId([line!() as u8; 32]),
            program: "cumulative-sum".into(),
            params: None,
            ensemble: EnsembleSpec::Join {
                creator: peers[0],
                negotiation_id,
            },
        },
    )
    .await;
    assert!(
        matches!(join_response, Ok(ResponseOk::ExecCreated { .. })),
        "joiner must accept a supported variable-size offer: {join_response:?}"
    );

    supervisor.stop().await;
    serving
        .await
        .expect("daemon serve task should join")
        .expect("daemon serve should stop cleanly");
}
