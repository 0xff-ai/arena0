//! Shared local-only harness for daemon API integration tests.
//!
//! One process-level [`Daemon`] owns the Unix socket and all selected Hosts.
//! Every Host operation carries its explicit Host name in the outer wire
//! request, so the tests exercise the same routing boundary as real clients.

#![allow(dead_code, unreachable_pub)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arena0_api::{HostRequest, NextEvent, Request, Response, ResponseOk};
use arena0_daemon::{Daemon, Keystore, McpConfig};
use arena0_home::{Home, HostName};
use arena0_program::ProgramHash;
use arena0_protocol::SessionHash;
use arena0_store::{Store, StoreConfig};
use tokio::io::BufReader;
use tokio::net::UnixStream;

/// The required rock-paper-scissors guest artifact.
pub fn rps_wasm() -> Vec<u8> {
    arena0_tests::wasm::program_wasm("rock_paper_scissors")
}

/// The required cumulative-sum guest artifact.
pub fn cumulative_sum_wasm() -> Vec<u8> {
    arena0_tests::wasm::program_wasm("cumulative_sum")
}

/// One explicit Host target on the shared daemon endpoint.
#[derive(Clone, Debug)]
pub struct HostTarget {
    pub socket: PathBuf,
    pub name: HostName,
}

impl HostTarget {
    pub fn request(&self, request: &HostRequest) -> Request {
        Request::Host {
            host: self.name.to_string(),
            request: request.clone(),
        }
    }
}

/// One framed Host request -> one framed response on a fresh Unix connection.
pub async fn call(target: &HostTarget, req: &HostRequest) -> Response {
    call_request(&target.socket, &target.request(req)).await
}

/// One framed daemon request -> one framed response on a fresh Unix connection.
pub async fn call_daemon(socket: &Path, req: &Request) -> Response {
    call_request(socket, req).await
}

async fn call_request(socket: &Path, req: &Request) -> Response {
    let stream = UnixStream::connect(socket)
        .await
        .unwrap_or_else(|error| panic!("connect to daemon socket {}: {error}", socket.display()));
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    arena0_api::frame::write_frame(&mut write, req)
        .await
        .expect("write request");
    arena0_api::frame::read_frame::<_, Response>(&mut read)
        .await
        .expect("read response")
        .expect("response frame")
}

/// Send one raw JSON frame when testing request-deserialization failures.
pub async fn call_json(
    socket: &Path,
    req: &serde_json::Value,
) -> std::io::Result<Option<Response>> {
    let stream = UnixStream::connect(socket).await?;
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    arena0_api::frame::write_frame(&mut write, req).await?;
    arena0_api::frame::read_frame::<_, Response>(&mut read).await
}

pub fn ok(resp: Response) -> ResponseOk {
    resp.unwrap_or_else(|error| panic!("daemon error: {error}"))
}

/// Drive a rock-paper-scissors execution to completion, answering every callout
/// with the JSON string `"Rock"`.
pub async fn drive(target: &HostTarget, exec_id: arena0_protocol::ExecId) -> SessionHash {
    drive_script(target, exec_id, &[]).await
}

/// Drive one execution to completion with a deterministic answer script. An
/// empty script answers every callout with `"Rock"`.
pub async fn drive_script(
    target: &HostTarget,
    exec_id: arena0_protocol::ExecId,
    script: &[serde_json::Value],
) -> SessionHash {
    let mut cursor = 0;
    loop {
        match ok(call(target, &HostRequest::ExecNext { exec_id }).await) {
            ResponseOk::Next(NextEvent::Callout { pending_id, .. }) => {
                let answer = if script.is_empty() {
                    serde_json::json!("Rock")
                } else {
                    script
                        .get(cursor)
                        .unwrap_or_else(|| panic!("script exhausted at callout {cursor}"))
                        .clone()
                };
                cursor += 1;
                ok(call(
                    target,
                    &HostRequest::ExecSubmit {
                        exec_id,
                        pending_id,
                        answer: Some(answer),
                    },
                )
                .await);
            }
            ResponseOk::Next(NextEvent::Completed { session_id, .. }) => return session_id,
            ResponseOk::Next(NextEvent::Failed { reason }) => {
                panic!("execution failed: {reason}")
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }
}

pub async fn wait_for_socket(socket: &Path) {
    for _ in 0..200 {
        if UnixStream::connect(socket).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("daemon socket never came up: {}", socket.display());
}

async fn wait_for_host(target: &HostTarget) {
    for _ in 0..200 {
        if matches!(
            call(target, &HostRequest::Info).await,
            Ok(ResponseOk::HostStatus(_))
        ) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("Host never became ready: {}", target.name);
}

pub async fn import(target: &HostTarget, wasm: &[u8]) -> ProgramHash {
    match ok(call(
        target,
        &HostRequest::ProgramImport {
            wasm: wasm.to_vec(),
        },
    )
    .await)
    {
        ResponseOk::Program(detail) => detail.summary.program_hash,
        other => panic!("unexpected import response: {other:?}"),
    }
}

pub fn created(resp: Response) -> arena0_protocol::ExecId {
    match ok(resp) {
        ResponseOk::ExecCreated { exec_id, .. } => exec_id,
        other => panic!("unexpected new-exec response: {other:?}"),
    }
}

/// One process-level daemon with two explicit Host targets used by the tests.
pub struct DaemonHarness {
    pub _home: tempfile::TempDir,
    pub _daemon: Arc<Daemon>,
    pub socket: PathBuf,
    pub host_a: HostTarget,
    pub host_b: HostTarget,
    pub peer_a: arena0_protocol::PeerId,
    pub peer_b: arena0_protocol::PeerId,
    pub program_id: ProgramHash,
}

/// Seed one durable Host namespace for a daemon started with bootstrap disabled.
/// The daemon then reopens this identity and store without importing its seven
/// built-in programs; each test imports only the guest artifact it exercises.
async fn seed_host(home: &Home, name: &HostName) {
    let state_dir = home.host(name).state_dir().to_owned();
    let keys_dir = state_dir.join("keys");
    std::fs::create_dir_all(&keys_dir).expect("Host state directories");
    let keystore = Keystore::open(keys_dir).expect("Host keystore");
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

/// Boot one daemon endpoint with two pre-seeded Hosts, import the requested
/// program into both namespaces, and return the shared endpoint plus explicit
/// Host targets.
pub async fn daemon(wasm: &[u8]) -> DaemonHarness {
    let home_dir = tempfile::tempdir().unwrap();
    let home = Home::from_root(home_dir.path().to_path_buf()).unwrap();
    let socket = home.socket();
    let host_a = HostName::try_from("a").unwrap();
    let host_b = HostName::try_from("b").unwrap();
    let mcp = McpConfig::new(SocketAddr::from(([127, 0, 0, 1], 0)), None).unwrap();
    let engine = Arc::new(arena0_sandbox::WasmtimeEngine::new().expect("sandbox engine"));
    seed_host(&home, &host_a).await;
    seed_host(&home, &host_b).await;
    let supervisor = Daemon::start(
        vec![host_a.clone(), host_b.clone()],
        mcp,
        engine,
        home,
        false,
    )
    .await
    .unwrap_or_else(|error| panic!("start daemon supervisor: {error}"));
    tokio::spawn(Arc::clone(&supervisor).serve());
    wait_for_socket(&socket).await;

    let host_a = HostTarget {
        socket: socket.clone(),
        name: host_a,
    };
    let host_b = HostTarget {
        socket: socket.clone(),
        name: host_b,
    };
    wait_for_host(&host_a).await;
    wait_for_host(&host_b).await;
    let program_id = import(&host_a, wasm).await;
    assert_eq!(import(&host_b, wasm).await, program_id);

    let peer_a = match ok(call(&host_a, &HostRequest::Info).await) {
        ResponseOk::HostStatus(status) => status.host.peer_id,
        other => panic!("unexpected host info response: {other:?}"),
    };
    let peer_b = match ok(call(&host_b, &HostRequest::Info).await) {
        ResponseOk::HostStatus(status) => status.host.peer_id,
        other => panic!("unexpected host info response: {other:?}"),
    };

    DaemonHarness {
        _home: home_dir,
        _daemon: supervisor,
        socket,
        host_a,
        host_b,
        peer_a,
        peer_b,
        program_id,
    }
}
