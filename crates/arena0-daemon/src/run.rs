//! Local ensemble daemon entrypoint.
//!
//! One process supervises a bounded set of participant Hosts. Each Host owns a
//! distinct identity, durable state directory, program registry, receipt store,
//! and Unix socket; [`arena0_node::Ensemble`] supplies their shared virtual
//! local network.

use std::collections::BTreeSet;
use std::{future::Future, sync::Arc};

use arena0_home::{Home, HostName};
use arena0_sandbox::WasmtimeEngine;

use crate::paths::wasmtime_cache_dir;
use crate::startup::{self, StartupStage, StartupTimeline};
use crate::{Daemon, HostConfig, McpConfig, Paths};

/// Open every Host namespace, start their shared local Ensemble, and serve all
/// Host sockets until one service stops, fails, or the process receives
/// Ctrl-C.
pub async fn run(names: Vec<HostName>, bootstrap: bool, mcp: McpConfig) -> anyhow::Result<()> {
    let timeline = Arc::new(StartupTimeline::new(
        names.len(),
        crate::assets::PROGRAMS.len(),
    ));
    if let Err(error) = validate_host_names(&names) {
        startup::progress(StartupStage::Failed, &timeline);
        return Err(error);
    }
    startup::progress(StartupStage::HostsProvisioning, &timeline);
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(error) => {
            startup::progress(StartupStage::Failed, &timeline);
            return Err(error.into());
        }
    };
    let mut hosts = Vec::with_capacity(names.len());
    for name in names {
        let host = name.to_string();
        let paths = match Paths::from_location(&home.host(&name)) {
            Ok(paths) => paths,
            Err(error) => {
                startup::host_progress(StartupStage::Failed, &host, &timeline);
                return Err(error);
            }
        };
        let config = match HostConfig::open(name, paths, bootstrap) {
            Ok(config) => config,
            Err(error) => {
                startup::host_progress(StartupStage::Failed, &host, &timeline);
                return Err(error);
            }
        };
        startup::host_progress(StartupStage::HostProvisioned, &host, &timeline);
        hosts.push(config);
    }
    startup::progress(StartupStage::EngineInitializing, &timeline);
    let cache_dir = match wasmtime_cache_dir(&home) {
        Ok(cache_dir) => cache_dir,
        Err(error) => {
            startup::progress(StartupStage::Failed, &timeline);
            return Err(error);
        }
    };
    let engine = match WasmtimeEngine::new_persistent(&cache_dir) {
        Ok(engine) => Arc::new(engine),
        Err(error) => {
            startup::progress(StartupStage::Failed, &timeline);
            return Err(anyhow::anyhow!("sandbox engine: {error}"));
        }
    };
    startup::progress(StartupStage::EngineReady, &timeline);
    let daemon = match Daemon::start_with_timeline(hosts, mcp, engine, Arc::clone(&timeline)).await
    {
        Ok(daemon) => daemon,
        Err(error) => {
            startup::progress(StartupStage::Failed, &timeline);
            return Err(error);
        }
    };
    startup::progress(StartupStage::InitializationComplete, &timeline);
    serve_until_shutdown(daemon, tokio::signal::ctrl_c()).await
}

fn validate_host_names(names: &[HostName]) -> anyhow::Result<()> {
    let mut seen = BTreeSet::new();
    for name in names {
        anyhow::ensure!(
            seen.insert(name),
            "ensemble hosts must use distinct names; duplicate {name}"
        );
    }
    Ok(())
}

async fn serve_until_shutdown(
    daemon: Arc<Daemon>,
    shutdown: impl Future<Output = std::io::Result<()>>,
) -> anyhow::Result<()> {
    let mut serving = Box::pin(Arc::clone(&daemon).serve());
    tokio::pin!(shutdown);
    tokio::select! {
        result = &mut serving => result,
        signal = &mut shutdown => {
            signal?;
            let (_, result) = tokio::join!(daemon.stop(), serving);
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::net::UnixStream;

    use super::*;

    async fn wait_for_socket(path: &std::path::Path) {
        loop {
            if UnixStream::connect(path).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn shutdown_signal_drives_serving_to_completion() {
        let first = TempDir::new().expect("first Host home");
        let second = TempDir::new().expect("second Host home");
        let first_socket = first.path().join("arena0.sock");
        let second_socket = second.path().join("arena0.sock");
        let hosts = vec![
            HostConfig::open(
                "first",
                Paths::new(first.path().to_path_buf(), first_socket.clone()),
                true,
            )
            .expect("open first Host"),
            HostConfig::open(
                "second",
                Paths::new(second.path().to_path_buf(), second_socket.clone()),
                true,
            )
            .expect("open second Host"),
        ];
        let mcp = McpConfig::new(SocketAddr::from(([127, 0, 0, 1], 0)), None).expect("MCP config");
        let engine = Arc::new(WasmtimeEngine::new().expect("sandbox engine"));
        let daemon = Daemon::start(hosts, mcp, engine)
            .await
            .expect("start daemon");
        let shutdown = async {
            wait_for_socket(&first_socket).await;
            wait_for_socket(&second_socket).await;
            Ok(())
        };

        tokio::time::timeout(
            Duration::from_secs(10),
            serve_until_shutdown(daemon, shutdown),
        )
        .await
        .expect("shutdown should not deadlock")
        .expect("daemon should stop cleanly");

        assert!(!first_socket.exists());
        assert!(!second_socket.exists());
    }
}
