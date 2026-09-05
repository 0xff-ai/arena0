//! Command-scoped supervision for the local Host ensemble.
//!
//! The child remains `arena0d`: it owns Hosts, stores, transport, sandboxing,
//! and shutdown. This module owns only the process it starts and never stops a
//! daemon that was already serving the requested Host sockets.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use arena0_client::api::Request;
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
    /// Reuse a daemon serving every requested Host, or start and await one.
    pub(crate) async fn connect_or_start(hosts: Vec<HostName>) -> anyhow::Result<Self> {
        if hosts.is_empty() {
            bail!("a local daemon requires at least one Host");
        }
        let clients = clients(&hosts)?;
        let reachable = probe(&clients).await;
        if reachable.iter().all(|ready| *ready) {
            return Ok(Self {
                hosts,
                child: None,
                stderr: None,
            });
        }
        if reachable.iter().any(|ready| *ready) {
            let ready = hosts
                .iter()
                .zip(&reachable)
                .filter_map(|(host, ready)| ready.then_some(host.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "only part of the requested local Ensemble is reachable ({ready}); stop the existing service before changing its Host set"
            );
        }

        let daemon = crate::serve::daemon_executable()?;
        let mut command = Command::new(&daemon);
        for host in &hosts {
            command.arg("--host").arg(host.as_str());
        }
        command
            .arg("--mcp-listen")
            .arg("127.0.0.1:0")
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
        if let Err(error) = local.await_ready(&clients).await {
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

    #[must_use]
    pub(crate) fn is_spawned(&self) -> bool {
        self.child.is_some()
    }

    #[must_use]
    pub(crate) fn hosts(&self) -> &[HostName] {
        &self.hosts
    }

    /// Stop and reap only a daemon started by this value.
    pub(crate) async fn shutdown(mut self) -> anyhow::Result<()> {
        self.shutdown_spawned().await
    }

    async fn await_ready(&mut self, clients: &[DaemonClient]) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + START_TIMEOUT;
        let mut interval = tokio::time::interval(PROBE_INTERVAL);
        loop {
            if probe(clients).await.iter().all(|ready| *ready) {
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
                    clients.len(),
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
        let first = self
            .hosts
            .first()
            .ok_or_else(|| anyhow!("owned local daemon lost its Host set"))?;
        let client = DaemonClient::for_host(first)?;
        let _ = client.call(&Request::DaemonStop).await;
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

fn clients(hosts: &[HostName]) -> anyhow::Result<Vec<DaemonClient>> {
    hosts
        .iter()
        .map(DaemonClient::for_host)
        .collect::<Result<_, _>>()
        .map_err(Into::into)
}

async fn probe(clients: &[DaemonClient]) -> Vec<bool> {
    futures::future::join_all(clients.iter().map(DaemonClient::daemon_up)).await
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
