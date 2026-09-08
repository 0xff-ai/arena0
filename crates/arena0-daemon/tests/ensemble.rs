use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use arena0_api::{ApiErrorCode, EnsembleSpec, HostRequest, Request, Response, ResponseOk};
use arena0_daemon::{Daemon, Keystore, McpConfig};
use arena0_home::{Home, HostName};
use arena0_program::ParticipantCount;
use arena0_store::{Store, StoreConfig};
use tempfile::TempDir;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::time::{sleep, timeout};

const ROCK_PAPER_SCISSORS_WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../programs/target/wasm32-unknown-unknown/release/rock_paper_scissors.wasm"
));
const CUMULATIVE_SUM_WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../programs/target/wasm32-unknown-unknown/release/cumulative_sum.wasm"
));

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

async fn seed_host(home: &Home, name: &str) {
    let name = name.parse::<HostName>().expect("Host name");
    let location = home.host(&name);
    let state_dir = location.state_dir();
    std::fs::create_dir_all(state_dir.join("keys")).expect("Host state directories");
    let keystore = Keystore::open(state_dir.join("keys")).expect("Host keystore");
    let identity = keystore
        .new_identity(Some(name.to_string()))
        .expect("Host identity");
    let store = Store::open(StoreConfig::new(
        state_dir.join("arena0.sqlite"),
        identity.peer_id,
    ))
    .expect("Host store");
    store.shutdown().await.expect("Host store shutdown");
}

async fn start(
    names: &[&str],
) -> (
    TempDir,
    Arc<Daemon>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let home_dir = TempDir::new().expect("temporary daemon home");
    let home = Home::from_root(home_dir.path().to_path_buf()).unwrap();
    for name in names {
        seed_host(&home, name).await;
    }
    let daemon = Daemon::start(
        names.iter().map(|name| name.parse().unwrap()).collect(),
        McpConfig::new("127.0.0.1:0".parse().unwrap(), None).unwrap(),
        Arc::new(arena0_sandbox::WasmtimeEngine::new().expect("sandbox engine")),
        home,
        false,
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

async fn import_program(
    socket: &Path,
    host_name: &str,
    wasm: &[u8],
) -> Box<arena0_api::ProgramDetail> {
    match call(
        socket,
        &host(
            host_name,
            HostRequest::ProgramImport {
                wasm: wasm.to_vec(),
            },
        ),
    )
    .await
    {
        Ok(ResponseOk::Program(program)) => program,
        response => panic!("unexpected program import response: {response:?}"),
    }
}

#[tokio::test]
async fn variable_size_program_accepts_supported_participant_count() {
    let (home, daemon, serving) = start(&["host-01"]).await;
    let socket = home.path().join("arena0.sock");
    let target = "host-01";
    import_program(&socket, target, ROCK_PAPER_SCISSORS_WASM).await;
    let cumulative_hash = import_program(&socket, target, CUMULATIVE_SUM_WASM)
        .await
        .summary
        .program_hash;
    let cumulative_detail = match call(
        &socket,
        &host(
            target,
            HostRequest::ProgramGet {
                program: cumulative_hash.to_string(),
            },
        ),
    )
    .await
    {
        Ok(ResponseOk::Program(program)) => program,
        response => panic!("unexpected cumulative-sum detail response: {response:?}"),
    };
    assert_eq!(
        cumulative_detail.summary.participants,
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
