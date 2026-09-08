use std::io::Read;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use arena0_client::api::{AwaitState, EnsembleSpec, HostRequest, Request, ResponseOk};
use arena0_client::proto::DaemonClient;
use arena0_home::HostName;
use tempfile::TempDir;
use tokio::net::UnixStream;

const STARTUP_DEADLINE: Duration = Duration::from_secs(30);
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(15);

struct ChildGuard {
    child: Option<std::process::Child>,
}

impl ChildGuard {
    fn new(child: std::process::Child) -> Self {
        Self { child: Some(child) }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().expect("guarded child").id()
    }

    fn as_mut(&mut self) -> &mut std::process::Child {
        self.child.as_mut().expect("guarded child")
    }

    fn take_stderr(&mut self) -> anyhow::Result<std::process::ChildStderr> {
        self.as_mut().stderr.take().context("arena0d stderr pipe")
    }

    async fn wait_for_exit(&mut self) -> anyhow::Result<std::process::ExitStatus> {
        let deadline = Instant::now() + SHUTDOWN_DEADLINE;
        loop {
            if let Some(status) = self.as_mut().try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                let _ = self.as_mut().kill();
                let _ = self.as_mut().wait();
                bail!("arena0d did not exit within {SHUTDOWN_DEADLINE:?}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

async fn wait_for_socket(path: &std::path::Path, deadline: Instant) -> anyhow::Result<()> {
    loop {
        if Instant::now() >= deadline {
            bail!("timed out waiting for {}", path.display());
        }
        if UnixStream::connect(path).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_mcp(address: SocketAddr, deadline: Instant) -> anyhow::Result<()> {
    loop {
        if Instant::now() >= deadline {
            bail!("timed out waiting for MCP at {address}");
        }
        if tokio::net::TcpStream::connect(address).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn preseed_host(home: &TempDir, name: &HostName) -> anyhow::Result<()> {
    let state = home.path().join("hosts").join(name.as_str());
    let keys = state.join("keys");
    std::fs::create_dir_all(&keys).with_context(|| format!("create {} keystore", name.as_str()))?;
    let keystore = arena0_daemon::Keystore::open(keys)
        .with_context(|| format!("open {} keystore", name.as_str()))?;
    keystore
        .new_identity(Some(name.to_string()))
        .map_err(anyhow::Error::new)
        .with_context(|| format!("seed {} identity", name.as_str()))?;
    std::fs::File::create(state.join("arena0.sqlite"))
        .with_context(|| format!("seed {} store", name.as_str()))?;
    Ok(())
}

fn rps_wasm() -> anyhow::Result<Vec<u8>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../programs/target/wasm32-unknown-unknown/release/rock_paper_scissors.wasm");
    std::fs::read(&path).with_context(|| {
        format!(
            "read required rock-paper-scissors guest {}; run `just build-programs`",
            path.display()
        )
    })
}

async fn import_rps(client: &DaemonClient, hosts: &[&HostName], wasm: &[u8]) -> anyhow::Result<()> {
    for host in hosts {
        let response = client
            .call_host(
                host,
                &HostRequest::ProgramImport {
                    wasm: wasm.to_vec(),
                },
            )
            .await?;
        let imported = match response {
            ResponseOk::Program(program) => program,
            other => return Err(anyhow!("unexpected program import response: {other:?}")),
        };
        assert_eq!(imported.summary.name, "rock-paper-scissors");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ctrl_c_stops_real_two_host_server_with_active_execution() -> anyhow::Result<()> {
    let home = TempDir::new().context("create isolated arena0 home")?;
    let host_a: HostName = "host-01".parse().unwrap();
    let host_b: HostName = "host-02".parse().unwrap();
    preseed_host(&home, &host_a)?;
    preseed_host(&home, &host_b)?;
    let socket = home.path().join("arena0.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_arena0d"))
        .env("ARENA0_HOME", home.path())
        .env("RUST_LOG", "arena0_daemon=info,warn")
        .args([
            "--host",
            host_a.as_str(),
            "--host",
            host_b.as_str(),
            "--no-bootstrap",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn arena0d")?;
    let mut child = ChildGuard::new(child);

    let startup_deadline = Instant::now() + STARTUP_DEADLINE;
    wait_for_socket(&socket, startup_deadline).await?;
    let client = DaemonClient::new(&socket);
    let endpoint = match client.call(&Request::DaemonInfo).await? {
        ResponseOk::DaemonInfo(info) => info.mcp_endpoint,
        other => return Err(anyhow!("unexpected daemon info response: {other:?}")),
    };
    let mcp_address: SocketAddr = endpoint
        .strip_prefix("http://")
        .and_then(|endpoint| endpoint.strip_suffix("/mcp"))
        .context("MCP endpoint must be an HTTP /mcp URL")?
        .parse()
        .context("parse bound MCP address")?;
    assert!(mcp_address.ip().is_loopback());
    assert_ne!(mcp_address.port(), 0);
    wait_for_mcp(mcp_address, startup_deadline).await?;

    let hosts = match client.call(&Request::HostsList).await? {
        ResponseOk::Hosts(hosts) => hosts,
        other => return Err(anyhow!("unexpected Hosts list response: {other:?}")),
    };
    let peer_a = hosts
        .iter()
        .find(|status| status.host.id == host_a.as_str())
        .ok_or_else(|| anyhow!("host-01 missing from Hosts list"))?
        .host
        .peer_id;
    let peer_b = hosts
        .iter()
        .find(|status| status.host.id == host_b.as_str())
        .ok_or_else(|| anyhow!("host-02 missing from Hosts list"))?
        .host
        .peer_id;
    let rps = rps_wasm()?;
    import_rps(&client, &[&host_a, &host_b], &rps).await?;
    let program = "rock-paper-scissors".to_owned();
    let created = client
        .call_host(
            &host_a,
            &HostRequest::ExecNew {
                exec_id: arena0_client::protocol::ExecId([line!() as u8; 32]),
                program,
                params: None,
                ensemble: EnsembleSpec::Explicit {
                    peers: vec![peer_b],
                },
            },
        )
        .await?;
    let (exec_a, negotiation_id) = match created {
        ResponseOk::ExecCreated {
            exec_id,
            negotiation_id: Some(negotiation_id),
            ..
        } => (exec_id, negotiation_id),
        other => return Err(anyhow!("unexpected execution response: {other:?}")),
    };
    let created_b = client
        .call_host(
            &host_b,
            &HostRequest::ExecNew {
                exec_id: arena0_client::protocol::ExecId([line!() as u8; 32]),
                program: "rock-paper-scissors".into(),
                params: None,
                ensemble: EnsembleSpec::Join {
                    target: Some(arena0_client::protocol::NegotiationTarget::new(
                        peer_a,
                        negotiation_id,
                    )),
                },
            },
        )
        .await?;
    let exec_b = match created_b {
        ResponseOk::ExecCreated { exec_id, .. } => exec_id,
        other => {
            return Err(anyhow!(
                "unexpected participant execution response: {other:?}"
            ));
        }
    };
    let await_request_a = HostRequest::ExecAwait {
        exec_id: exec_a,
        until: AwaitState::Active,
    };
    let await_request_b = HostRequest::ExecAwait {
        exec_id: exec_b,
        until: AwaitState::Active,
    };
    let await_a = client.call_host(&host_a, &await_request_a);
    let await_b = client.call_host(&host_b, &await_request_b);
    let (active_a, active_b) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(await_a, await_b)
    })
    .await
    .context("wait for active execution")?;
    active_a.context("creator did not become active")?;
    active_b.context("participant did not become active")?;

    let sent_at = Instant::now();
    // SAFETY: `child.id()` identifies the child just spawned above; SIGINT is
    // the same signal delivered by a real terminal Ctrl-C.
    let result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("send SIGINT to arena0d");
    }
    let status = child.wait_for_exit().await?;
    let elapsed = sent_at.elapsed();
    let mut stderr = String::new();
    child
        .take_stderr()?
        .read_to_string(&mut stderr)
        .context("read arena0d stderr")?;
    assert!(
        status.success(),
        "arena0d exited unsuccessfully: {status}\n{stderr}"
    );
    assert!(
        elapsed < SHUTDOWN_DEADLINE,
        "shutdown took {elapsed:?}\n{stderr}"
    );
    assert!(!socket.exists(), "daemon socket survived shutdown");
    assert!(
        !stderr.contains("JoinHandle polled after completion"),
        "shutdown task polled a completed JoinHandle:\n{stderr}"
    );
    assert!(
        !stderr.contains("panicked at"),
        "shutdown spawned a panicking task:\n{stderr}"
    );
    Ok(())
}
