//! Command-scoped supervision for the local Host ensemble.
//!
//! The child remains `arena0d`: it owns Hosts, stores, transport, sandboxing,
//! and shutdown. This module owns only the process it starts and never stops a
//! daemon that was already serving the shared endpoint.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use arena0_client::api::{Request, ResponseOk};
use arena0_client::proto::DaemonClient;
use arena0_home::HostName;
use tokio::process::{Child, Command};

use crate::process::StderrCapture;

const START_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_INTERVAL: Duration = Duration::from_millis(25);
const STDERR_TAIL_BYTES: usize = 16 * 1024;

/// Deterministic local Host names for one command-scoped Ensemble.
pub(crate) fn host_names(count: usize) -> Vec<HostName> {
    (0..count).map(HostName::for_local_index).collect()
}

/// A reachable local daemon, optionally owned by the current command.
#[derive(Debug)]
pub(crate) struct LocalDaemon {
    hosts: Vec<HostName>,
    child: Option<Child>,
    stderr: Option<StderrCapture>,
}

impl LocalDaemon {
    /// Reuse the shared daemon and open missing Hosts, or start and await it.
    pub(crate) async fn connect_or_start(hosts: Vec<HostName>) -> anyhow::Result<Self> {
        if hosts.is_empty() {
            bail!("a local daemon requires at least one Host");
        }
        let client = DaemonClient::from_env()?;
        match client.call(&Request::DaemonInfo).await {
            Ok(ResponseOk::DaemonInfo(_)) => {
                let mut local = Self {
                    hosts: Vec::new(),
                    child: None,
                    stderr: None,
                };
                local.ensure_hosts(hosts).await?;
                return Ok(local);
            }
            Ok(other) => bail!("unexpected daemon.info response: {other:?}"),
            Err(error) if arena0_client::proto::is_connect_error(&error) => {}
            Err(error) => return Err(error),
        }

        let daemon = crate::serve::daemon_executable()?;
        let mut command = Command::new(&daemon);
        for host in &hosts {
            command.arg("--host").arg(host.as_str());
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .with_context(|| format!("start local Host service with {}", daemon.display()))?;
        let child_stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("local Host service stderr was not piped"))?;
        let stderr = StderrCapture::start(child_stderr, STDERR_TAIL_BYTES);
        let mut local = Self {
            hosts,
            child: Some(child),
            stderr: Some(stderr),
        };
        if let Err(error) = local.await_ready(&client).await {
            let cleanup = local.shutdown_spawned().await.err();
            let diagnostics = local.stderr_tail();
            let mut message = format!("{error:#}");
            if let Some(cleanup) = cleanup {
                message.push_str(&format!("; cleanup failed: {cleanup:#}"));
            }
            if !diagnostics.is_empty() {
                message.push_str("; daemon stderr tail: ");
                message.push_str(&diagnostics);
            }
            return Err(anyhow!(message));
        }
        Ok(local)
    }

    /// Select participating Hosts, opening missing namespaces through the daemon.
    pub(crate) async fn ensure_hosts(&mut self, hosts: Vec<HostName>) -> anyhow::Result<()> {
        let client = DaemonClient::from_env()?;
        let available = client.list_hosts().await?;
        for host in &hosts {
            if !available.iter().any(|entry| entry.host.id == host.as_str()) {
                client
                    .open_host(
                        Some(host.to_string()),
                        format!("arena0/{}", env!("CARGO_PKG_VERSION")),
                    )
                    .await?;
            }
        }
        self.hosts = hosts;
        Ok(())
    }

    /// Stop and reap only a daemon started by this value.
    pub(crate) async fn shutdown(mut self) -> anyhow::Result<()> {
        self.shutdown_spawned().await
    }

    async fn await_ready(&mut self, client: &DaemonClient) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + START_TIMEOUT;
        loop {
            if client.daemon_up().await {
                return Ok(());
            }
            if let Some(status) = self
                .child
                .as_mut()
                .expect("spawned daemon is present while awaiting readiness")
                .try_wait()
                .context("check local Host service startup")?
            {
                bail!("local Host service exited during startup with {status}");
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "local Host service did not make all {} Hosts ready within {}s",
                    self.hosts.len(),
                    START_TIMEOUT.as_secs()
                );
            }
            tokio::select! {
                _ = tokio::time::sleep(PROBE_INTERVAL) => {}
                signal = tokio::signal::ctrl_c() => {
                    signal.context("listen for cancellation during local Host startup")?;
                    bail!("local Host startup cancelled");
                }
            }
        }
    }

    async fn shutdown_spawned(&mut self) -> anyhow::Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        // Address the owned process directly: a competing startup may have
        // won the shared socket while this child failed to acquire the home.
        // Sending daemon.stop through that socket would stop the other owner.
        if child
            .try_wait()
            .context("check owned daemon before shutdown")?
            .is_none()
            && let Some(id) = child.id()
        {
            let pid = libc::pid_t::try_from(id).context("convert owned daemon process id")?;
            // SAFETY: the unreaped child owns this process ID. SIGINT is
            // handled by arena0d's normal graceful shutdown path.
            let result = unsafe { libc::kill(pid, libc::SIGINT) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error).context("interrupt owned local daemon");
                }
            }
        }
        let status = match tokio::time::timeout(SHUTDOWN_TIMEOUT, child.wait()).await {
            Ok(status) => status.context("reap local Host service")?,
            Err(_) => {
                child
                    .start_kill()
                    .context("kill local Host service after shutdown timeout")?;
                child.wait().await.context("reap local Host service")?
            }
        };
        self.child = None;
        if let Some(stderr) = &mut self.stderr {
            stderr.join(Duration::from_secs(1), "local daemon").await?;
        }
        if status.success() {
            Ok(())
        } else {
            let diagnostics = self.stderr_tail();
            bail!(
                "local Host service did not shut down cleanly ({status}){}",
                if diagnostics.is_empty() {
                    String::new()
                } else {
                    format!("; daemon stderr tail: {diagnostics}")
                }
            )
        }
    }

    fn stderr_tail(&self) -> String {
        self.stderr
            .as_ref()
            .map_or_else(String::new, StderrCapture::tail)
    }
}

impl Drop for LocalDaemon {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_client::api::{
        ApiError, ApiErrorCode, DaemonInfo, Request, Response, ResponseOk,
        frame::{read_frame, write_frame},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::BufReader;
    use tokio::net::UnixListener;
    use tokio::sync::Notify;

    #[test]
    fn local_ensemble_names_are_stable_and_distinct() {
        let names = host_names(4)
            .into_iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, ["host-01", "host-02", "host-03", "host-04"]);
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn await_ready_delays_probe_after_slow_initial_failure() -> anyhow::Result<()> {
        let directory = tempfile::tempdir().expect("local daemon test directory");
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind local daemon test socket");
        let probe_count = Arc::new(AtomicUsize::new(0));
        let first_probe = Arc::new(Notify::new());
        let release_first_probe = Arc::new(Notify::new());
        let (timestamps, mut timestamps_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(serve_probe_socket(
            listener,
            probe_count.clone(),
            first_probe.clone(),
            release_first_probe.clone(),
            timestamps,
        ));
        let child = tokio::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn long-lived readiness child");
        let client = DaemonClient::new(socket);
        let mut local = LocalDaemon {
            hosts: vec![HostName::for_local_index(0)],
            child: Some(child),
            stderr: None,
        };
        let readiness = tokio::spawn(async move {
            let result = local.await_ready(&client).await;
            (result, local)
        });

        first_probe.notified().await;
        assert_eq!(probe_count.load(Ordering::SeqCst), 1);
        tokio::time::advance(PROBE_INTERVAL * 2).await;
        assert_eq!(
            probe_count.load(Ordering::SeqCst),
            1,
            "a slow initial probe must not trigger another probe while it is pending"
        );
        release_first_probe.notify_one();
        let (_, first_response_at) = timestamps_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("read first probe response timestamp"))?;
        let (_, second_probe_at) = timestamps_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("read second probe request timestamp"))?;
        assert_eq!(probe_count.load(Ordering::SeqCst), 2);
        assert!(
            second_probe_at >= first_response_at + PROBE_INTERVAL,
            "readiness retry started before a full delay after the failed probe: {:?}",
            second_probe_at.saturating_duration_since(first_response_at)
        );

        let (result, mut local) = readiness.await.expect("readiness task should join");
        result?;
        if let Some(mut child) = local.child.take() {
            child.kill().await.context("stop readiness test child")?;
            child.wait().await.context("reap readiness test child")?;
        }
        server.abort();
        assert!(
            server
                .await
                .expect_err("probe server should be aborted")
                .is_cancelled()
        );
        Ok(())
    }

    async fn serve_probe_socket(
        listener: UnixListener,
        probe_count: Arc<AtomicUsize>,
        first_probe: Arc<Notify>,
        release_first_probe: Arc<Notify>,
        timestamps: tokio::sync::mpsc::UnboundedSender<(u8, tokio::time::Instant)>,
    ) -> anyhow::Result<()> {
        loop {
            let (stream, _) = listener.accept().await?;
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            let request = read_frame::<_, Request>(&mut read)
                .await?
                .ok_or_else(|| anyhow!("readiness request stream closed"))?;
            assert!(matches!(request, Request::DaemonInfo));
            let number = probe_count.fetch_add(1, Ordering::SeqCst) + 1;
            let response: Response = if number == 1 {
                first_probe.notify_one();
                release_first_probe.notified().await;
                Err(ApiError::new(ApiErrorCode::Internal, "daemon is starting"))
            } else {
                Ok(ResponseOk::DaemonInfo(DaemonInfo {
                    version: "test".to_owned(),
                    abi_version: 1,
                    uptime_secs: 0,
                    socket: "test.sock".to_owned(),
                    mcp_endpoint: "127.0.0.1:0".to_owned(),
                }))
            };
            write_frame(&mut write, &response).await?;
            if number == 1 {
                timestamps
                    .send((1, tokio::time::Instant::now()))
                    .map_err(|_| anyhow!("readiness timestamp receiver closed"))?;
            } else if number == 2 {
                timestamps
                    .send((2, tokio::time::Instant::now()))
                    .map_err(|_| anyhow!("readiness timestamp receiver closed"))?;
            }
        }
    }
}
