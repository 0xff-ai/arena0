//! Local ensemble daemon entrypoint.
//!
//! One process supervises a bounded set of participant Hosts. Each Host owns a
//! distinct identity, durable state directory, program registry, and receipt
//! store; the daemon owns one shared Unix socket and [`arena0_node::Ensemble`]
//! supplies their shared virtual local network.

use std::{future::Future, sync::Arc};

use arena0_home::{Home, HostName};
use arena0_sandbox::WasmtimeEngine;

use crate::paths::wasmtime_cache_dir;
use crate::startup::{StartupStage, StartupTimeline};
use crate::{Daemon, McpConfig};

/// Open every Host namespace, start their shared local Ensemble, and serve the
/// shared daemon socket until one service stops, fails, or the process receives
/// Ctrl-C.
pub async fn run(names: Vec<HostName>, bootstrap: bool, mcp: McpConfig) -> anyhow::Result<()> {
    let timeline = Arc::new(StartupTimeline::new(
        names.len(),
        crate::assets::PROGRAMS.len(),
    ));
    if let Err(error) = crate::ensemble::validate_host_names(&names) {
        timeline.progress(StartupStage::Failed);
        return Err(error);
    }
    timeline.progress(StartupStage::HostsProvisioning);
    let home = match Home::from_env() {
        Ok(home) => home,
        Err(error) => {
            timeline.progress(StartupStage::Failed);
            return Err(error.into());
        }
    };
    timeline.progress(StartupStage::EngineInitializing);
    let cache_dir = match wasmtime_cache_dir(&home) {
        Ok(cache_dir) => cache_dir,
        Err(error) => {
            timeline.progress(StartupStage::Failed);
            return Err(error);
        }
    };
    let engine = match WasmtimeEngine::new_persistent(&cache_dir) {
        Ok(engine) => Arc::new(engine),
        Err(error) => {
            timeline.progress(StartupStage::Failed);
            return Err(anyhow::anyhow!("sandbox engine: {error}"));
        }
    };
    timeline.progress(StartupStage::EngineReady);
    let daemon = match Daemon::start_with_timeline(
        names,
        mcp,
        engine,
        home,
        bootstrap,
        Arc::clone(&timeline),
    )
    .await
    {
        Ok(daemon) => daemon,
        Err(error) => {
            timeline.progress(StartupStage::Failed);
            return Err(error);
        }
    };
    serve_until_shutdown(daemon, tokio::signal::ctrl_c()).await
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
