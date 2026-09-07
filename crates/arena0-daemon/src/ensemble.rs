//! One-process supervision for a local runtime ensemble.
//!
//! [`Daemon`] is the public process owner. It provisions one durable
//! [`HostConfig`] per participant, wires the corresponding [`HostService`]s to
//! one runtime [`arena0_node::Ensemble`], and owns ensemble shutdown.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Context as _;
use arena0_api::{ApiError, DaemonInfo, HostInfo};
use arena0_crypto::NodeKeys;
use arena0_home::{Home, HostName};
use arena0_node::Ensemble;
use arena0_protocol::{PeerId, PeerIdSource};
use arena0_sandbox::{Program, WasmtimeEngine};
use arena0_store::{Store, StoreConfig};
use tokio::net::UnixListener;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinSet;

use crate::assets::PROGRAMS;
use crate::catalog::ProgramCatalog;
use crate::paths::Paths;
use crate::server::{Activity, HostService, HostServiceInit};
use crate::startup::{self, StartupStage, StartupTimeline};
use crate::store::Keystore;

const NEW: u8 = 0;
const STARTING: u8 = 1;
const RUNNING: u8 = 2;
const FINISHING: u8 = 3;
const FINISHED: u8 = 4;
// ponytail: preserve the existing resource ceiling independently of execution membership.
const MAX_LOCAL_HOSTS: usize = 64;
const OPEN_QUEUE_CAPACITY: usize = 16;
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

        let reservation = Store::reserve(&paths.db_path).context("reserve Host ownership")?;
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
        let store = reservation
            .open(StoreConfig::new(paths.db_path.clone(), peer_id))
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

/// A ready Host and the durable owner retained until its runtime stops.
struct HostSlot {
    service: Arc<HostService>,
    store: Store,
}

struct OpenRequest {
    id: Option<HostName>,
    user_agent: String,
    reply: oneshot::Sender<anyhow::Result<HostInfo>>,
}

struct PreparedHost {
    id: HostName,
    slot: HostSlot,
    listener: UnixListener,
}

enum OpenedHost {
    Existing(HostInfo),
    Prepared(PreparedHost),
}

/// Authoritative owner of the daemon's live Host roster.
///
/// Services only retain a weak reference to this owner for `hosts.list`, so a
/// service cannot keep the daemon alive and the listing always follows the
/// same map used by dynamic provisioning and shutdown.
#[derive(Default)]
pub(crate) struct HostDirectory {
    hosts: RwLock<BTreeMap<HostName, HostSlot>>,
}

impl HostDirectory {
    fn services(&self) -> Vec<(String, Arc<HostService>)> {
        self.hosts
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .map(|(id, slot)| (id.to_string(), Arc::clone(&slot.service)))
            .collect()
    }

    fn service(&self, host: &str) -> Option<Arc<HostService>> {
        self.hosts
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(host)
            .map(|slot| Arc::clone(&slot.service))
    }

    pub(crate) async fn infos(&self) -> Result<Vec<DaemonInfo>, ApiError> {
        let services = self.services();
        let mut infos = Vec::with_capacity(services.len());
        for (_, service) in services {
            infos.push(service.daemon_info().await?);
        }
        Ok(infos)
    }

    fn take(&self) -> BTreeMap<HostName, HostSlot> {
        std::mem::take(
            &mut *self
                .hosts
                .write()
                .unwrap_or_else(|error| error.into_inner()),
        )
    }

    fn insert_if_running(
        &self,
        state: &AtomicU8,
        id: HostName,
        slot: HostSlot,
    ) -> Result<(), HostSlot> {
        let mut hosts = self
            .hosts
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if state.load(Ordering::Acquire) == RUNNING {
            hosts.insert(id, slot);
            Ok(())
        } else {
            Err(slot)
        }
    }

    fn begin_shutdown(&self, state: &AtomicU8) {
        let _hosts = self
            .hosts
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let _ = state.fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
            matches!(state, STARTING | RUNNING).then_some(FINISHING)
        });
    }
}

/// One-process owner of ready Hosts and serialized, supervised provisioning.
pub struct Daemon {
    ensemble: Ensemble,
    hosts: Arc<HostDirectory>,
    home: Home,
    bootstrap_new_hosts: bool,
    engine: Arc<WasmtimeEngine>,
    opens: mpsc::Sender<OpenRequest>,
    open_requests: Mutex<Option<mpsc::Receiver<OpenRequest>>>,
    stop_requested: Notify,
    activity: Arc<Activity>,
    mcp: McpConfig,
    startup: Arc<StartupTimeline>,
    state: AtomicU8,
    finished: watch::Sender<bool>,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon")
            .field(
                "hosts",
                &self.services().iter().map(|(id, _)| id).collect::<Vec<_>>(),
            )
            .field("mcp", &self.mcp)
            .finish_non_exhaustive()
    }
}

impl Daemon {
    /// Compose the selected startup Hosts. Other persisted namespaces are opened
    /// on demand through MCP, so separate daemons can share one home layout.
    pub async fn start(
        hosts: Vec<HostConfig>,
        mcp: McpConfig,
        engine: Arc<WasmtimeEngine>,
        home: Home,
        bootstrap_new_hosts: bool,
    ) -> anyhow::Result<Arc<Self>> {
        let startup = Arc::new(StartupTimeline::new(hosts.len(), PROGRAMS.len()));
        Self::start_with_timeline(hosts, mcp, engine, home, bootstrap_new_hosts, startup).await
    }

    pub(crate) async fn start_with_timeline(
        hosts: Vec<HostConfig>,
        mcp: McpConfig,
        engine: Arc<WasmtimeEngine>,
        home: Home,
        bootstrap_new_hosts: bool,
        startup: Arc<StartupTimeline>,
    ) -> anyhow::Result<Arc<Self>> {
        validate_hosts(&hosts)?;
        if hosts.iter().any(|host| host.bootstrap) {
            startup::progress(StartupStage::ProgramsBootstrapping, &startup);
        }
        for host in &hosts {
            bootstrap_programs(host, &engine, &startup).await?;
        }
        let ensemble = Ensemble::start(
            hosts
                .iter()
                .map(|host| (Arc::clone(&host.identity), host.store.handle().clone()))
                .collect(),
        )?;
        let activity = Arc::new(Activity::new());
        let directory = Arc::new(HostDirectory::default());
        let mut ready = BTreeMap::new();
        for config in hosts {
            let peer_id = config.peer_id();
            let runtime = ensemble
                .host(&peer_id)
                .ok_or_else(|| anyhow::anyhow!("ensemble omitted Host {peer_id}"))?;
            let transport = ensemble
                .transport(&peer_id)
                .ok_or_else(|| anyhow::anyhow!("ensemble omitted transport {peer_id}"))?;
            let service = HostService::start_with_runtime(
                HostServiceInit {
                    name: config.name.to_string(),
                    transport,
                    paths: config.paths.clone(),
                    keystore: Arc::clone(&config.keystore),
                    catalog: ProgramCatalog::new(config.store.handle().clone()),
                    store: config.store.handle().clone(),
                    engine: Arc::clone(&engine),
                    startup: Arc::clone(&startup),
                    activity: Arc::clone(&activity),
                    host_directory: Arc::downgrade(&directory),
                },
                runtime,
            )?;
            startup::host_progress(StartupStage::HostComposed, config.name.as_str(), &startup);
            ready.insert(
                config.name,
                HostSlot {
                    service,
                    store: config.store,
                },
            );
        }
        *directory
            .hosts
            .write()
            .unwrap_or_else(|error| error.into_inner()) = ready;
        let (finished, _) = watch::channel(false);
        let (opens, open_requests) = mpsc::channel(OPEN_QUEUE_CAPACITY);
        Ok(Arc::new(Self {
            ensemble,
            hosts: directory,
            home,
            bootstrap_new_hosts,
            engine,
            opens,
            open_requests: Mutex::new(Some(open_requests)),
            stop_requested: Notify::new(),
            activity,
            mcp,
            startup,
            state: AtomicU8::new(NEW),
            finished,
        }))
    }

    /// Open or create a Host. Once queued, work belongs to the supervisor even
    /// if the requesting MCP connection disconnects.
    pub(crate) async fn open_host(
        &self,
        id: Option<String>,
        user_agent: String,
    ) -> anyhow::Result<HostInfo> {
        arena0_store::validate_user_agent(&user_agent)?;
        let id = id.map(|id| id.parse::<HostName>()).transpose()?;
        anyhow::ensure!(
            self.state.load(Ordering::Acquire) == RUNNING,
            "daemon is not accepting Host opens"
        );
        let (reply, result) = oneshot::channel();
        self.opens
            .send(OpenRequest {
                id,
                user_agent,
                reply,
            })
            .await
            .map_err(|_| anyhow::anyhow!("daemon is shutting down"))?;
        result
            .await
            .map_err(|_| anyhow::anyhow!("Host opening supervisor stopped"))?
    }

    /// Serve startup and dynamically opened Hosts under one shutdown owner.
    ///
    /// Keep this future running through shutdown. Call [`Self::stop`] to stop
    /// the daemon; cancelling this future cannot finish resource cleanup.
    pub async fn serve(self: Arc<Self>) -> anyhow::Result<()> {
        self.state
            .compare_exchange(NEW, STARTING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow::anyhow!("ensemble daemon has already been started or stopped"))?;
        let mut opens = self
            .open_requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .expect("first serve owns open queue");
        let mcp_listener = match crate::mcp::bind(&self.mcp).await {
            Ok(listener) => listener,
            Err(error) => {
                startup::progress(StartupStage::Failed, &self.startup);
                self.cleanup().await;
                return Err(error);
            }
        };
        let (mcp_shutdown, mcp_shutdown_rx) = watch::channel(false);
        let mut servers = JoinSet::new();
        let mut outcome = Ok(());
        // Restore initial metadata and recovery before accepting MCP opens.
        // Otherwise an existing-ID open can race startup's metadata load.
        for (_, service) in self.services() {
            if self.state.load(Ordering::Acquire) != STARTING {
                break;
            }
            match service.prepare().await {
                Ok(listener) => {
                    servers.spawn(service.serve_prepared(listener));
                }
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
        }
        if outcome.is_ok()
            && self
                .state
                .compare_exchange(STARTING, RUNNING, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            let daemon = Arc::clone(&self);
            let mcp = self.mcp.clone();
            servers.spawn(async move {
                crate::mcp::serve(daemon, mcp, mcp_listener, mcp_shutdown_rx).await
            });
        }
        // ponytail: one provisioning job at a time; only parallelize after measured contention.
        let mut opening = JoinSet::new();
        while outcome.is_ok() && self.state.load(Ordering::Acquire) == RUNNING {
            tokio::select! {
                biased;
                result = servers.join_next() => {
                    if let Some(result) = result { outcome = join_result(result); }
                    break;
                }
                _ = self.stop_requested.notified() => break,
                completed = opening.join_next(), if !opening.is_empty() => {
                    match completed {
                        Some(Ok((reply, result))) => {
                            self.finish_open(reply, result, &mut servers).await;
                        }
                        Some(Err(error)) => {
                            outcome = Err(anyhow::anyhow!("Host provisioning task failed: {error}"));
                            break;
                        }
                        None => {}
                    }
                }
                Some(request) = opens.recv(), if opening.is_empty() => {
                    let daemon = Arc::clone(&self);
                    opening.spawn(async move {
                        let result = daemon.prepare_open(request.id, request.user_agent).await;
                        (request.reply, result)
                    });
                }
            }
        }
        self.begin_shutdown();
        opens.close();
        while let Ok(request) = opens.try_recv() {
            let _ = request
                .reply
                .send(Err(anyhow::anyhow!("daemon is shutting down")));
        }
        // An effectful open must reach installation or rollback; aborting it
        // would detach runtime or store resources from their owner.
        while let Some(completed) = opening.join_next().await {
            match completed {
                Ok((reply, result)) => self.finish_open(reply, result, &mut servers).await,
                Err(error) if outcome.is_ok() => outcome = Err(error.into()),
                Err(_) => {}
            }
        }
        let _ = mcp_shutdown.send(true);
        for (_, service) in self.services() {
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
                    tracing::error!("serve children exceeded shutdown deadline; aborting");
                    servers.abort_all();
                    while servers.join_next().await.is_some() {}
                    break;
                }
            }
        }
        self.cleanup().await;
        outcome
    }

    async fn prepare_open(
        &self,
        id: Option<HostName>,
        user_agent: String,
    ) -> anyhow::Result<OpenedHost> {
        anyhow::ensure!(
            self.state.load(Ordering::Acquire) == RUNNING,
            "daemon is shutting down"
        );
        if let Some(ref id) = id
            && let Some(service) = self.service(id.as_str())
        {
            service.set_user_agent(user_agent).await?;
            return Ok(OpenedHost::Existing(service.host_info()));
        }
        anyhow::ensure!(
            self.services().len() < MAX_LOCAL_HOSTS,
            "daemon Host capacity reached ({MAX_LOCAL_HOSTS})"
        );
        let id = match id {
            Some(id) => id,
            None => self.generate_host_id()?,
        };
        let paths = Paths::from_location(&self.home.host(&id))?;
        anyhow::ensure!(
            self.services()
                .iter()
                .all(|(_, service)| service.socket_path() != paths.socket),
            "Host socket is already assigned to another Host"
        );
        let bootstrap = self.bootstrap_new_hosts;
        let allocated_id = id.clone();
        let config = tokio::task::spawn_blocking(move || HostConfig::open(id, paths, bootstrap))
            .await
            .context("join Host provisioning")?
            .with_context(|| format!("open Host '{allocated_id}'"))?;
        self.prepare_new(config, user_agent)
            .await
            .map(OpenedHost::Prepared)
            .with_context(|| format!("prepare Host '{allocated_id}'"))
    }

    fn generate_host_id(&self) -> anyhow::Result<HostName> {
        for _ in 0..8 {
            let id: HostName = hex::encode(rand::random::<[u8; 16]>()).parse()?;
            if self.service(id.as_str()).is_some() {
                continue;
            }
            match std::fs::symlink_metadata(self.home.host(&id).state_dir()) {
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(id),
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::bail!("could not allocate a unique Host id")
    }

    async fn prepare_new(
        &self,
        config: HostConfig,
        user_agent: String,
    ) -> anyhow::Result<PreparedHost> {
        let peer_id = config.peer_id();
        let runtime = async {
            config.store.handle().set_user_agent(user_agent).await?;
            bootstrap_programs(&config, &self.engine, &self.startup).await?;
            self.ensemble
                .add_host(Arc::clone(&config.identity), config.store.handle().clone())
                .map_err(anyhow::Error::from)
        }
        .await;
        let runtime = match runtime {
            Ok(runtime) => runtime,
            Err(error) => {
                config
                    .store
                    .shutdown()
                    .await
                    .context("close failed Host store")?;
                return Err(error);
            }
        };
        let transport = self
            .ensemble
            .transport(&peer_id)
            .expect("new transport installed");
        let service = match compose_service(
            &config,
            transport,
            runtime,
            &self.engine,
            &self.startup,
            &self.activity,
            &self.hosts,
        ) {
            Ok(service) => service,
            Err(error) => {
                self.close_provisional_host(peer_id, config.store).await?;
                return Err(error);
            }
        };
        match service.prepare().await {
            Ok(listener) => Ok(PreparedHost {
                id: config.name,
                slot: HostSlot {
                    service,
                    store: config.store,
                },
                listener,
            }),
            Err(error) => {
                service.stop().await;
                self.close_provisional_host(peer_id, config.store).await?;
                Err(error)
            }
        }
    }

    async fn finish_open(
        &self,
        reply: oneshot::Sender<anyhow::Result<HostInfo>>,
        result: anyhow::Result<OpenedHost>,
        servers: &mut JoinSet<anyhow::Result<()>>,
    ) {
        let prepared = match result {
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
            Ok(OpenedHost::Existing(info)) => {
                let result = if self.state.load(Ordering::Acquire) == RUNNING {
                    Ok(info)
                } else {
                    Err(anyhow::anyhow!("daemon is shutting down"))
                };
                let _ = reply.send(result);
                return;
            }
            Ok(OpenedHost::Prepared(prepared)) => prepared,
        };
        // Publication and begin_shutdown share the directory lock: an open
        // either commits before shutdown or follows the rollback path.
        let PreparedHost { id, slot, listener } = prepared;
        let info = slot.service.host_info();
        let service = Arc::clone(&slot.service);
        match self.hosts.insert_if_running(&self.state, id, slot) {
            Ok(()) => {
                servers.spawn(service.serve_prepared(listener));
                let _ = reply.send(Ok(info));
            }
            Err(slot) => {
                drop(listener);
                slot.service.remove_prepared_socket();
                slot.service.stop().await;
                let cleanup = self
                    .close_provisional_host(slot.service.peer_id(), slot.store)
                    .await;
                let error = match cleanup {
                    Ok(()) => anyhow::anyhow!("daemon is shutting down"),
                    Err(error) => {
                        anyhow::anyhow!("daemon is shutting down; Host cleanup failed: {error:#}")
                    }
                };
                let _ = reply.send(Err(error));
            }
        }
    }

    /// Services relinquish their socket and stop before this releases store ownership.
    async fn close_provisional_host(&self, peer_id: PeerId, store: Store) -> anyhow::Result<()> {
        if let Err(error) = self.ensemble.remove_host(&peer_id).await {
            tracing::error!(%peer_id, %error, "failed to detach provisional Host");
        }
        store.shutdown().await.context("close failed Host store")
    }

    pub(crate) fn services(&self) -> Vec<(String, Arc<HostService>)> {
        self.hosts.services()
    }

    pub(crate) fn activity(&self) -> Arc<Activity> {
        Arc::clone(&self.activity)
    }

    pub(crate) fn startup_timeline(&self) -> Arc<StartupTimeline> {
        Arc::clone(&self.startup)
    }

    pub(crate) fn service(&self, host: &str) -> Option<Arc<HostService>> {
        self.hosts.service(host)
    }

    pub(crate) fn host_name(&self, peer_id: PeerId) -> Option<String> {
        self.services()
            .into_iter()
            .find_map(|(id, service)| (service.peer_id() == peer_id).then_some(id))
    }

    fn begin_shutdown(&self) {
        self.hosts.begin_shutdown(&self.state);
    }

    /// Request coordinated shutdown, including pending dynamic opens.
    pub async fn stop(&self) {
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
                STARTING | RUNNING | FINISHING => {
                    self.begin_shutdown();
                    self.stop_requested.notify_one();
                    let mut finished = self.finished.subscribe();
                    if !*finished.borrow() {
                        let _ = finished.changed().await;
                    }
                    return;
                }
                FINISHED => return,
                _ => unreachable!("invalid daemon state"),
            }
        }
    }

    async fn cleanup(&self) {
        let hosts = self.hosts.take();
        futures::future::join_all(hosts.values().map(|slot| slot.service.stop())).await;
        // Also clear paths for listeners aborted at the shutdown deadline.
        // Old service handles must lose unlink authority before another daemon
        // can acquire the store and bind a replacement socket.
        for slot in hosts.values() {
            slot.service.remove_prepared_socket();
        }
        self.ensemble.stop().await;
        for (id, slot) in hosts {
            if let Err(error) = slot.store.shutdown().await {
                tracing::error!(%id, %error, "SQLite store shutdown failed");
            }
        }
        self.state.store(FINISHED, Ordering::Release);
        self.finished.send_replace(true);
    }
}

fn compose_service(
    config: &HostConfig,
    transport: Arc<arena0_transport::local::LocalTransport>,
    runtime: Arc<arena0_node::Host>,
    engine: &Arc<WasmtimeEngine>,
    startup: &Arc<StartupTimeline>,
    activity: &Arc<Activity>,
    directory: &Arc<HostDirectory>,
) -> anyhow::Result<Arc<HostService>> {
    let store = config.store.handle().clone();
    HostService::start_with_runtime(
        HostServiceInit {
            name: config.name.to_string(),
            transport,
            paths: config.paths.clone(),
            keystore: Arc::clone(&config.keystore),
            catalog: ProgramCatalog::new(store.clone()),
            store,
            engine: Arc::clone(engine),
            startup: Arc::clone(startup),
            activity: Arc::clone(activity),
            host_directory: Arc::downgrade(directory),
        },
        runtime,
    )
}

async fn bootstrap_programs(
    config: &HostConfig,
    engine: &Arc<WasmtimeEngine>,
    startup: &StartupTimeline,
) -> anyhow::Result<()> {
    if !config.bootstrap {
        return Ok(());
    }
    for wasm in PROGRAMS {
        let engine = Arc::clone(engine);
        let program = tokio::task::spawn_blocking(move || {
            let program = Program::try_from((*wasm).to_vec()).context("parse embedded program")?;
            let _admitted = engine
                .admit(&program)
                .map_err(|error| anyhow::anyhow!("admit embedded program: {error}"))?;
            Ok::<_, anyhow::Error>(program)
        })
        .await
        .context("join embedded program admission")??;
        config
            .store
            .handle()
            .register_program(program.bytes().to_vec(), arena0_node::unix_time_ms())
            .await?;
    }
    startup::host_progress(
        StartupStage::HostProgramsReady,
        config.name.as_str(),
        startup,
    );
    Ok(())
}

fn validate_hosts(hosts: &[HostConfig]) -> anyhow::Result<()> {
    anyhow::ensure!(
        hosts.len() <= MAX_LOCAL_HOSTS,
        "daemon Host capacity exceeded ({MAX_LOCAL_HOSTS})"
    );
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
