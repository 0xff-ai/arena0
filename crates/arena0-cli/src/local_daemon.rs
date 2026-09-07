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
        Self::connect_or_start_with_mcp(hosts, "127.0.0.1:0".parse().expect("loopback address"))
            .await
    }

    /// Select the MCP listener for a newly started service. A borrowed service
    /// retains its own listener configuration.
    pub(crate) async fn connect_or_start_with_mcp(
        hosts: Vec<HostName>,
        mcp_listen: std::net::SocketAddr,
    ) -> anyhow::Result<Self> {
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
            .arg("--mcp-listen")
            .arg(mcp_listen.to_string())
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
        let mut interval = tokio::time::interval(PROBE_INTERVAL);
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
                _ = interval.tick() => {}
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

    #[test]
    fn local_ensemble_names_are_stable_and_distinct() {
        let names = host_names(4)
            .into_iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, ["host-01", "host-02", "host-03", "host-04"]);
    }
}
