//! One-process supervision for a local runtime ensemble.
//!
//! [`Daemon`] is the public process owner. It provisions one durable
//! [`HostConfig`] per participant, wires the corresponding [`HostService`]s to
//! one runtime [`arena0_node::Ensemble`], and owns ensemble shutdown.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use arena0_crypto::NodeKeys;
use arena0_home::HostName;
use arena0_node::Ensemble;
use arena0_protocol::{PeerId, PeerIdSource};
use arena0_sandbox::{Program, WasmtimeEngine};
use arena0_store::{Store, StoreConfig};
use tokio::sync::{Mutex as TokioMutex, watch};
use tokio::task::JoinSet;

use crate::assets::PROGRAMS;
use crate::catalog::ProgramCatalog;
use crate::paths::Paths;
use crate::server::{HostService, HostServiceInit};
use crate::startup::{self, StartupStage, StartupTimeline};
use crate::store::Keystore;

const NEW: u8 = 0;
const RUNNING: u8 = 1;
const FINISHING: u8 = 2;
const FINISHED: u8 = 3;
const SERVE_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Configuration for the daemon-owned MCP Streamable HTTP endpoint.
///
/// Phase 1 deliberately binds only to loopback. `bearer_token` protects the
/// whole daemon Ensemble; it never selects or scopes a Host.
#[derive(Clone)]
pub struct McpConfig {
    pub(crate) listen: SocketAddr,
    pub(crate) bearer_token: Option<String>,
}

impl McpConfig {
    /// Configure one loopback MCP endpoint. An optional bearer token applies to
    /// every Host and every MCP session served at that endpoint.
    pub fn new(listen: SocketAddr, bearer_token: Option<String>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            listen.ip().is_loopback(),
            "Phase 1 MCP must listen on a loopback address"
        );
        if let Some(token) = &bearer_token {
            anyhow::ensure!(!token.is_empty(), "ARENA0_MCP_TOKEN must not be empty");
        }
        Ok(Self {
            listen,
            bearer_token,
        })
    }
}

impl std::fmt::Debug for McpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConfig")
            .field("listen", &self.listen)
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// Durable state and identity inputs for one local ensemble participant.
///
/// The Host owns its own home, keystore, SQLite store, and socket. The shared
/// runtime topology is added by [`Daemon::start`].
#[derive(Debug)]
pub struct HostConfig {
    /// Operator-visible Host name used in event frames and identity labels.
    name: HostName,
    /// The one signing identity shared by this Host's runtime and service.
    identity: Arc<NodeKeys>,
    /// Host-local state and socket paths.
    paths: Paths,
    /// Identity custody for this Host.
    keystore: Arc<Keystore>,
    /// The sole durable protocol owner for this Host. It remains in the
    /// [`Daemon`] until all services and the shared [`Ensemble`] have stopped.
    store: Store,
    /// Whether built-in programs should be imported during daemon start.
    bootstrap: bool,
}

impl HostConfig {
    /// Open one Host home, provision its active identity, and import embedded
    /// programs idempotently when `bootstrap` is enabled.
    pub fn open<N>(name: N, paths: Paths, bootstrap: bool) -> anyhow::Result<Self>
    where
        N: TryInto<HostName>,
        N::Error: std::fmt::Display,
    {
        let name = name
            .try_into()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        paths.ensure_dirs().context("create state directories")?;

        let keystore = Arc::new(Keystore::open(paths.keys_dir.clone())?);
        let peer_id = match keystore.active_peer_id() {
            Some(peer_id) => peer_id,
            None if !keystore.list()?.is_empty() => {
                anyhow::bail!(
                    "no active identity in {}; repair or recreate this Host",
                    paths.keys_dir.display()
                );
            }
            None if !bootstrap => {
                anyhow::bail!(
                    "no identity in {}; run `arena0 identity new` or start without --no-bootstrap",
                    paths.keys_dir.display()
                );
            }
            None => {
                let info = keystore
                    .new_identity(Some(name.to_string()))
                    .context("mint the Host identity")?;
                tracing::info!(peer = %info.peer_id, host = %name, "created ensemble identity");
                info.peer_id
            }
        };
        let identity = Arc::new(keystore.active_crypto()?);
        let store = Store::open(StoreConfig::new(paths.db_path.clone(), peer_id))
            .with_context(|| format!("open SQLite store at {}", paths.db_path.display()))?;
        Ok(Self {
            name,
            identity,
            paths,
            keystore,
            store,
            bootstrap,
        })
    }

    /// The persistent peer identity selected for this Host.
    #[must_use]
    pub fn peer_id(&self) -> PeerId {
        self.identity.peer_id()
    }
}

/// One-process supervisor for per-Host services backed by one local runtime
/// ensemble.
pub struct Daemon {
    ensemble: Ensemble,
    services: BTreeMap<String, Arc<HostService>>,
    /// Host SQLite owners. Handles are passed into nodes/services, but these
    /// owners stay here until the complete shutdown order has finished.
    stores: TokioMutex<Option<BTreeMap<String, Store>>>,
    mcp: McpConfig,
    startup: Arc<StartupTimeline>,
    state: AtomicU8,
    finished: watch::Sender<bool>,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon")
            .field("hosts", &self.services.keys())
            .field("mcp", &self.mcp)
            .finish_non_exhaustive()
    }
}

impl Daemon {
    /// Start one existing Host service per configuration over a shared local runtime
    /// ensemble.
    pub async fn start(
        hosts: Vec<HostConfig>,
        mcp: McpConfig,
        engine: Arc<WasmtimeEngine>,
    ) -> anyhow::Result<Arc<Self>> {
        let startup = Arc::new(StartupTimeline::new(hosts.len(), PROGRAMS.len()));
        Self::start_with_timeline(hosts, mcp, engine, startup).await
    }

    pub(crate) async fn start_with_timeline(
        hosts: Vec<HostConfig>,
        mcp: McpConfig,
        engine: Arc<WasmtimeEngine>,
        startup: Arc<StartupTimeline>,
    ) -> anyhow::Result<Arc<Self>> {
        validate_hosts(&hosts)?;

        // Embedded programs are admitted and registered before any Host is
        // exposed through the Ensemble. Registration is idempotent and uses
        // the exact bytes that passed admission, so every selected Host has
        // the program locally before a request can enter negotiation.
        let bootstrap_hosts = hosts.iter().filter(|host| host.bootstrap).count();
        if bootstrap_hosts != 0 {
            startup::progress(StartupStage::ProgramsBootstrapping, &startup);
        }
        for host in &hosts {
            if !host.bootstrap {
                continue;
            }
            for wasm in PROGRAMS {
                let program = Program::try_from((*wasm).to_vec())
                    .with_context(|| format!("parse embedded program for {}", host.name))?;
                let _admitted = engine
                    .admit(&program)
                    .map_err(|error| anyhow::anyhow!("admit embedded program: {error}"))?;
                host.store
                    .handle()
                    .register_program(program.bytes().to_vec(), arena0_node::unix_time_ms())
                    .await
                    .with_context(|| format!("import embedded program for {}", host.name))?;
            }
            startup::host_progress(
                StartupStage::HostProgramsReady,
                host.name.as_str(),
                &startup,
            );
        }

        let identities = hosts
            .iter()
            .map(|host| (Arc::clone(&host.identity), host.store.handle().clone()))
            .collect();
        let ensemble = Ensemble::start(identities)?;

        let mut services = BTreeMap::new();
        let mut stores = BTreeMap::new();
        for host_config in hosts {
            let name = host_config.name.to_string();
            let peer_id = host_config.peer_id();
            let store_handle = host_config.store.handle().clone();
            let host = ensemble
                .host(&peer_id)
                .ok_or_else(|| anyhow::anyhow!("ensemble omitted host {peer_id}"))?;
            let transport = ensemble
                .transport(&peer_id)
                .ok_or_else(|| anyhow::anyhow!("ensemble omitted transport {peer_id}"))?;
            let service = HostService::start_with_runtime(
                HostServiceInit {
                    name: name.clone(),
                    transport,
                    paths: host_config.paths,
                    keystore: host_config.keystore,
                    catalog: ProgramCatalog::new(store_handle.clone()),
                    store: store_handle,
                    engine: Arc::clone(&engine),
                    startup: Arc::clone(&startup),
                },
                host,
            )?;
            services.insert(name.clone(), service);
            startup::host_progress(StartupStage::HostComposed, &name, &startup);
            stores.insert(name, host_config.store);
        }

        let (finished, _receiver) = watch::channel(false);
        Ok(Arc::new(Self {
            ensemble,
            services,
            stores: TokioMutex::new(Some(stores)),
            mcp,
            startup,
            state: AtomicU8::new(NEW),
            finished,
        }))
    }

    /// Serve every host socket until one service exits, then stop and join all
    /// remaining services and the shared runtime topology.
    pub async fn serve(self: Arc<Self>) -> anyhow::Result<()> {
        self.state
            .compare_exchange(NEW, RUNNING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow::anyhow!("ensemble daemon has already been started or stopped"))?;

        let (mcp_shutdown, mcp_shutdown_rx) = watch::channel(false);
        let mut servers = JoinSet::new();
        for service in self.services.values() {
            let service = Arc::clone(service);
            servers.spawn(async move { service.serve().await });
        }
        let daemon = Arc::clone(&self);
        let mcp = self.mcp.clone();
        servers.spawn(async move { crate::mcp::serve(daemon, mcp, mcp_shutdown_rx).await });

        let mut outcome = Ok(());
        if let Some(result) = servers.join_next().await {
            outcome = join_result(result);
        }
        let _ = mcp_shutdown.send(true);
        for service in self.services.values() {
            service.request_shutdown();
        }
        let deadline = tokio::time::Instant::now() + SERVE_SHUTDOWN_TIMEOUT;
        loop {
            match tokio::time::timeout_at(deadline, servers.join_next()).await {
                Ok(Some(result)) => {
                    if outcome.is_ok() {
                        outcome = join_result(result);
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    tracing::error!(
                        timeout_secs = SERVE_SHUTDOWN_TIMEOUT.as_secs(),
                        "serve children did not stop before shutdown deadline; aborting"
                    );
                    servers.abort_all();
                    while servers.join_next().await.is_some() {}
                    break;
                }
            }
        }
        self.cleanup().await;
        outcome
    }

    pub(crate) fn services(&self) -> &BTreeMap<String, Arc<HostService>> {
        &self.services
    }

    pub(crate) fn startup_timeline(&self) -> Arc<StartupTimeline> {
        Arc::clone(&self.startup)
    }

    pub(crate) fn service(&self, host: &str) -> Option<Arc<HostService>> {
        self.services.get(host).cloned()
    }

    pub(crate) fn peer_id(&self, host: &str) -> Option<PeerId> {
        self.services.get(host).map(|service| service.peer_id())
    }

    pub(crate) fn host_name(&self, peer_id: PeerId) -> Option<&str> {
        self.services
            .iter()
            .find_map(|(name, service)| (service.peer_id() == peer_id).then_some(name.as_str()))
    }

    /// Request coordinated shutdown and wait until every per-host service has
    /// completed its own cleanup. Calling this before [`Self::serve`] performs
    /// the same cleanup synchronously.
    pub async fn stop(&self) {
        for service in self.services.values() {
            service.request_shutdown();
        }

        loop {
            match self.state.load(Ordering::Acquire) {
                NEW => {
                    if self
                        .state
                        .compare_exchange(NEW, FINISHING, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        self.cleanup().await;
                        return;
                    }
                }
                RUNNING | FINISHING => {
                    let mut finished = self.finished.subscribe();
                    if *finished.borrow() {
                        return;
                    }
                    let _ = finished.changed().await;
                    return;
                }
                FINISHED => return,
                _ => unreachable!("invalid ensemble daemon state"),
            }
        }
    }

    async fn cleanup(&self) {
        futures::future::join_all(self.services.values().map(|service| service.stop())).await;
        self.ensemble.stop().await;
        // The actor/supervisor and Host accept paths are stopped before the
        // SQLite owners. Taking the owners here makes shutdown ordering
        // explicit and prevents a detached store thread from outliving the
        // daemon.
        if let Some(stores) = self.stores.lock().await.take() {
            for (name, store) in stores {
                if let Err(error) = store.shutdown().await {
                    tracing::error!(%name, %error, "SQLite store shutdown failed");
                }
            }
        }
        self.state.store(FINISHED, Ordering::Release);
        let _ = self.finished.send(true);
    }
}

fn validate_hosts(hosts: &[HostConfig]) -> anyhow::Result<()> {
    let mut names = BTreeSet::new();
    let mut paths = BTreeSet::<PathBuf>::new();
    let mut sockets = BTreeSet::<PathBuf>::new();
    for host in hosts {
        anyhow::ensure!(
            names.insert(host.name.clone()),
            "ensemble hosts must use distinct names; duplicate {}",
            host.name
        );
        anyhow::ensure!(
            paths.insert(host.paths.state_dir.clone()),
            "ensemble hosts must use distinct homes; duplicate {}",
            host.paths.state_dir.display()
        );
        anyhow::ensure!(
            sockets.insert(host.paths.socket.clone()),
            "ensemble hosts must use distinct sockets; duplicate {}",
            host.paths.socket.display()
        );
    }
    Ok(())
}

fn join_result(result: Result<anyhow::Result<()>, tokio::task::JoinError>) -> anyhow::Result<()> {
    match result {
        Ok(result) => result,
        Err(error) => Err(anyhow::anyhow!("daemon service task failed: {error}")),
    }
}
