//! One-process supervision for a local runtime ensemble.
//!
//! [`Daemon`] is the public process owner. It provisions one durable
//! [`HostConfig`] per participant, wires the corresponding [`HostService`]s to
//! one runtime [`arena0_node::Ensemble`], and owns the shared Unix endpoint
//! and ensemble shutdown.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::Context as _;
use arena0_api::{
    ApiError, DaemonInfo, HostInfo, HostRequest, HostStatus, Request, ResponseOk, frame,
};
use arena0_crypto::NodeKeys;
use arena0_home::{Home, HostName};
use arena0_node::Ensemble;
use arena0_protocol::{PeerId, PeerIdSource};
use arena0_sandbox::{Program, WasmtimeEngine};
use arena0_store::{Store, StoreConfig};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};

use crate::assets::PROGRAMS;
use crate::catalog::ProgramCatalog;
use crate::mcp_auth::{AuthError, DEFAULT_ACCESS_TOKEN_LIFETIME, McpAuth, RawToken};
use crate::paths::{FileLease as HomeLease, Paths};
use crate::server::{Activity, HostService, HostServiceInit, UnixSocket};
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
/// whole daemon ingress; it is separate from the per-Host JWT carried by MCP
/// tool calls.
#[derive(Clone)]
pub struct McpConfig {
    pub(crate) listen: SocketAddr,
    pub(crate) bearer_token: Option<String>,
    pub(crate) access_token_lifetime: Duration,
}

impl McpConfig {
    /// Configure one loopback MCP endpoint with the default one-day JWT life.
    pub fn new(listen: SocketAddr, bearer_token: Option<String>) -> anyhow::Result<Self> {
        Self::with_access_token_lifetime(listen, bearer_token, DEFAULT_ACCESS_TOKEN_LIFETIME)
    }

    /// Configure one loopback MCP endpoint and its JWT lifetime.
    pub fn with_access_token_lifetime(
        listen: SocketAddr,
        bearer_token: Option<String>,
        access_token_lifetime: Duration,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            listen.ip().is_loopback(),
            "Phase 1 MCP must listen on a loopback address"
        );
        if let Some(token) = &bearer_token {
            anyhow::ensure!(!token.is_empty(), "ARENA0_MCP_TOKEN must not be empty");
        }
        anyhow::ensure!(
            access_token_lifetime.as_secs() > 0,
            "MCP access token lifetime must be greater than zero"
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| anyhow::anyhow!("system clock is before Unix epoch"))?
            .as_secs();
        anyhow::ensure!(
            now.checked_add(access_token_lifetime.as_secs()).is_some(),
            "MCP access token lifetime overflows Unix time"
        );
        Ok(Self {
            listen,
            bearer_token,
            access_token_lifetime,
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
            .field("access_token_lifetime", &self.access_token_lifetime)
            .finish()
    }
}

/// Durable state and identity inputs for one local ensemble participant.
///
/// The Host owns its own home, keystore, and SQLite store. The shared runtime
/// topology and daemon endpoint are added by [`Daemon::start`].
#[derive(Debug)]
pub(crate) struct HostConfig {
    /// Operator-visible Host name used in event frames and identity labels.
    name: HostName,
    /// The one signing identity shared by this Host's runtime and service.
    identity: Arc<NodeKeys>,
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
    pub(crate) fn open<N>(name: N, paths: Paths, bootstrap: bool) -> anyhow::Result<Self>
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
            keystore,
            store,
            bootstrap,
        })
    }

    /// Reopen one already-existing Host namespace without minting identity or
    /// creating its directory/database. The expected peer is checked before
    /// reserving the SQLite store or touching the shared runtime.
    pub(crate) fn open_existing(
        name: HostName,
        paths: Paths,
        expected_peer: PeerId,
        bootstrap: bool,
    ) -> anyhow::Result<Self> {
        let state = std::fs::symlink_metadata(&paths.state_dir).with_context(|| {
            format!("inspect existing Host state {}", paths.state_dir.display())
        })?;
        anyhow::ensure!(
            !state.file_type().is_symlink() && state.is_dir(),
            "existing Host state must be a regular directory"
        );
        // Reject a missing database before Store::reserve can create an owner
        // lock or initialize an empty database.
        let database = std::fs::symlink_metadata(&paths.db_path).with_context(|| {
            format!("inspect existing Host database {}", paths.db_path.display())
        })?;
        anyhow::ensure!(
            !database.file_type().is_symlink() && database.is_file(),
            "existing Host database must be a regular file"
        );
        let reservation = Store::reserve(&paths.db_path).context("reserve Host ownership")?;
        // Custody is opened only after this process owns the durable store. A
        // mismatched or missing identity therefore cannot acquire identity
        // custody while another process owns the database.
        let keystore = Arc::new(Keystore::open(paths.keys_dir.clone())?);
        let actual_peer = keystore
            .active_peer_id()
            .ok_or_else(|| anyhow::anyhow!("existing Host has no active identity"))?;
        anyhow::ensure!(
            actual_peer == expected_peer,
            "existing Host identity does not match the authenticated peer"
        );
        let identity = Arc::new(keystore.active_crypto()?);
        anyhow::ensure!(
            identity.peer_id() == expected_peer,
            "existing Host identity changed while opening"
        );
        let store = reservation
            .open(StoreConfig::new(paths.db_path.clone(), expected_peer))
            .with_context(|| format!("open SQLite store at {}", paths.db_path.display()))?;
        Ok(Self {
            name,
            identity,
            keystore,
            store,
            bootstrap,
        })
    }

    /// The persistent peer identity selected for this Host.
    #[must_use]
    pub(crate) fn peer_id(&self) -> PeerId {
        self.identity.peer_id()
    }
}

/// A ready Host and the durable owner retained until its runtime stops.
struct HostSlot {
    service: Arc<HostService>,
    store: Store,
}

async fn shutdown_host_configs(hosts: Vec<HostConfig>) {
    for host in hosts {
        let id = host.name.to_string();
        if let Err(error) = host.store.shutdown().await {
            tracing::error!(host = %id, %error, "SQLite store shutdown failed during startup cleanup");
        }
    }
}

async fn shutdown_startup_owners(
    ensemble: &Ensemble,
    slots: BTreeMap<HostName, HostSlot>,
    configs: Vec<HostConfig>,
) {
    for slot in slots.values() {
        slot.service.stop().await;
    }
    ensemble.stop().await;
    for (id, slot) in slots {
        if let Err(error) = slot.store.shutdown().await {
            tracing::error!(%id, %error, "SQLite store shutdown failed during startup cleanup");
        }
    }
    shutdown_host_configs(configs).await;
}

enum OpenKind {
    Create {
        id: Option<HostName>,
        user_agent: String,
    },
    Existing {
        id: HostName,
        expected_peer: PeerId,
    },
}

struct OpenRequest {
    kind: OpenKind,
    reply: oneshot::Sender<anyhow::Result<HostInfo>>,
}

struct PreparedHost {
    id: HostName,
    slot: HostSlot,
}

enum OpenedHost {
    Existing(HostInfo),
    Prepared(PreparedHost),
}

/// One-process owner of ready Hosts and serialized, supervised provisioning.
pub struct Daemon {
    ensemble: Ensemble,
    hosts: RwLock<BTreeMap<HostName, HostSlot>>,
    _lease: HomeLease,
    socket: Arc<UnixSocket>,
    home: Home,
    bootstrap_new_hosts: bool,
    engine: Arc<WasmtimeEngine>,
    mcp_auth: Arc<McpAuth>,
    opens: mpsc::Sender<OpenRequest>,
    open_requests: Mutex<Option<mpsc::Receiver<OpenRequest>>>,
    stop_requested: Notify,
    activity: Arc<Activity>,
    mcp: McpConfig,
    shutdown: watch::Sender<bool>,
    mcp_endpoint: RwLock<Option<SocketAddr>>,
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
    /// Acquire the Home lease, open the selected Host namespaces, and compose
    /// the shared runtime. Other persisted namespaces are opened on demand.
    pub async fn start(
        names: Vec<HostName>,
        mcp: McpConfig,
        engine: Arc<WasmtimeEngine>,
        home: Home,
        bootstrap_new_hosts: bool,
    ) -> anyhow::Result<Arc<Self>> {
        let startup = Arc::new(StartupTimeline::new(names.len(), PROGRAMS.len()));
        Self::start_with_timeline(names, mcp, engine, home, bootstrap_new_hosts, startup).await
    }

    pub(crate) async fn start_with_timeline(
        names: Vec<HostName>,
        mcp: McpConfig,
        engine: Arc<WasmtimeEngine>,
        home: Home,
        bootstrap_new_hosts: bool,
        startup: Arc<StartupTimeline>,
    ) -> anyhow::Result<Arc<Self>> {
        let (reply, result) = oneshot::channel();
        let (accept, accepted) = oneshot::channel();
        // Construction owns effectful stores across awaits. Keep their owner
        // alive if the caller cancels, and require an acknowledgement before
        // transferring responsibility for the completed daemon to the caller.
        tokio::spawn(async move {
            match Self::construct(names, mcp, engine, home, bootstrap_new_hosts, startup).await {
                Ok(daemon) => {
                    if reply.send(Ok(Arc::clone(&daemon))).is_err() || accepted.await.is_err() {
                        daemon.stop().await;
                    }
                }
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
            }
        });
        let daemon = result.await.context("daemon construction task stopped")??;
        let _ = accept.send(());
        Ok(daemon)
    }

    async fn construct(
        names: Vec<HostName>,
        mcp: McpConfig,
        engine: Arc<WasmtimeEngine>,
        home: Home,
        bootstrap_new_hosts: bool,
        startup: Arc<StartupTimeline>,
    ) -> anyhow::Result<Arc<Self>> {
        validate_host_names(&names)?;
        let lease = HomeLease::acquire_home(&home)?;
        let mcp_auth = Arc::new(
            McpAuth::load_or_create(&home.mcp_signing_key(), mcp.access_token_lifetime)
                .context("load daemon MCP signing key")?,
        );
        startup::progress(StartupStage::HostsProvisioning, &startup);
        let mut hosts = Vec::with_capacity(names.len());
        for name in names {
            let host = name.to_string();
            let paths = match Paths::from_location(&home.host(&name)) {
                Ok(paths) => paths,
                Err(error) => {
                    startup::host_progress(StartupStage::Failed, &host, &startup);
                    shutdown_host_configs(hosts).await;
                    return Err(error);
                }
            };
            let config = match HostConfig::open(name, paths, bootstrap_new_hosts) {
                Ok(config) => config,
                Err(error) => {
                    startup::host_progress(StartupStage::Failed, &host, &startup);
                    shutdown_host_configs(hosts).await;
                    return Err(error);
                }
            };
            startup::host_progress(StartupStage::HostProvisioned, &host, &startup);
            hosts.push(config);
        }
        Self::start_configs(
            hosts,
            mcp,
            engine,
            home,
            bootstrap_new_hosts,
            startup,
            lease,
            mcp_auth,
        )
        .await
    }

    async fn start_configs(
        hosts: Vec<HostConfig>,
        mcp: McpConfig,
        engine: Arc<WasmtimeEngine>,
        home: Home,
        bootstrap_new_hosts: bool,
        startup: Arc<StartupTimeline>,
        lease: HomeLease,
        mcp_auth: Arc<McpAuth>,
    ) -> anyhow::Result<Arc<Self>> {
        if hosts.iter().any(|host| host.bootstrap) {
            startup::progress(StartupStage::ProgramsBootstrapping, &startup);
        }
        for index in 0..hosts.len() {
            let result = bootstrap_programs(&hosts[index], &engine, &startup).await;
            if let Err(error) = result {
                shutdown_host_configs(hosts).await;
                return Err(error);
            }
        }
        let ensemble = match Ensemble::start(
            hosts
                .iter()
                .map(|host| (Arc::clone(&host.identity), host.store.handle().clone()))
                .collect(),
        ) {
            Ok(ensemble) => ensemble,
            Err(error) => {
                shutdown_host_configs(hosts).await;
                return Err(error.into());
            }
        };
        let activity = Arc::new(Activity::new());
        let mut ready = BTreeMap::new();
        let mut remaining = hosts.into_iter();
        while let Some(config) = remaining.next() {
            let peer_id = config.peer_id();
            let runtime = match ensemble.host(&peer_id) {
                Some(runtime) => runtime,
                None => {
                    let error = anyhow::anyhow!("ensemble omitted Host {peer_id}");
                    let mut pending = vec![config];
                    pending.extend(remaining);
                    shutdown_startup_owners(&ensemble, ready, pending).await;
                    return Err(error);
                }
            };
            let transport = match ensemble.transport(&peer_id) {
                Some(transport) => transport,
                None => {
                    let error = anyhow::anyhow!("ensemble omitted transport {peer_id}");
                    let mut pending = vec![config];
                    pending.extend(remaining);
                    shutdown_startup_owners(&ensemble, ready, pending).await;
                    return Err(error);
                }
            };
            let service = match HostService::start_with_runtime(
                HostServiceInit {
                    name: config.name.to_string(),
                    transport,
                    keystore: Arc::clone(&config.keystore),
                    catalog: ProgramCatalog::new(config.store.handle().clone()),
                    store: config.store.handle().clone(),
                    engine: Arc::clone(&engine),
                    startup: Arc::clone(&startup),
                },
                runtime,
            ) {
                Ok(service) => service,
                Err(error) => {
                    let mut pending = vec![config];
                    pending.extend(remaining);
                    shutdown_startup_owners(&ensemble, ready, pending).await;
                    return Err(error);
                }
            };
            startup::host_progress(StartupStage::HostComposed, config.name.as_str(), &startup);
            ready.insert(
                config.name,
                HostSlot {
                    service,
                    store: config.store,
                },
            );
        }
        let (finished, _) = watch::channel(false);
        let (shutdown, _) = watch::channel(false);
        let (opens, open_requests) = mpsc::channel(OPEN_QUEUE_CAPACITY);
        Ok(Arc::new(Self {
            ensemble,
            hosts: RwLock::new(ready),
            _lease: lease,
            socket: Arc::new(UnixSocket::new(home.socket())),
            home,
            bootstrap_new_hosts,
            engine,
            mcp_auth,
            opens,
            open_requests: Mutex::new(Some(open_requests)),
            stop_requested: Notify::new(),
            activity,
            mcp,
            shutdown,
            mcp_endpoint: RwLock::new(None),
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
                kind: OpenKind::Create { id, user_agent },
                reply,
            })
            .await
            .map_err(|_| anyhow::anyhow!("daemon is shutting down"))?;
        result
            .await
            .map_err(|_| anyhow::anyhow!("Host opening supervisor stopped"))?
    }

    /// Reopen an existing Host selected by an authenticated token. This uses
    /// the same supervised queue as ordinary opens, but the worker must find
    /// the namespace and verify its durable identity before publication.
    pub(crate) async fn open_existing(
        &self,
        id: HostName,
        expected_peer: PeerId,
    ) -> anyhow::Result<HostInfo> {
        anyhow::ensure!(
            self.state.load(Ordering::Acquire) == RUNNING,
            "daemon is not accepting Host opens"
        );
        let (reply, result) = oneshot::channel();
        self.opens
            .send(OpenRequest {
                kind: OpenKind::Existing { id, expected_peer },
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
        let mcp_endpoint = match mcp_listener.local_addr().context("read bound MCP endpoint") {
            Ok(endpoint) => endpoint,
            Err(error) => {
                startup::progress(StartupStage::Failed, &self.startup);
                self.cleanup().await;
                return Err(error);
            }
        };
        *self
            .mcp_endpoint
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(mcp_endpoint);
        let mcp_shutdown_rx = self.shutdown.subscribe();
        let mut mcp_task: Option<JoinHandle<anyhow::Result<()>>> = None;
        let mut unix_task: Option<JoinHandle<anyhow::Result<()>>> = None;
        let mut outcome = Ok(());
        // Restore initial metadata and recovery before accepting MCP opens.
        // Otherwise an existing-ID open can race startup's metadata load.
        let mut prepared = Vec::new();
        for (_, service) in self.services() {
            if self.state.load(Ordering::Acquire) != STARTING {
                break;
            }
            match service.prepare().await {
                Ok(()) => prepared.push(service),
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
        }
        if outcome.is_ok() && self.state.load(Ordering::Acquire) == STARTING {
            // Bind the shared endpoint only after every initial Host has
            // recovered. A successful connect must never queue work behind a
            // daemon that has not yet started accepting requests.
            let unix_listener = match self.socket.bind() {
                Ok(listener) => Some(listener),
                Err(error) => {
                    outcome = Err(error);
                    None
                }
            };
            if let Some(unix_listener) = unix_listener {
                let published = {
                    let _hosts = self
                        .hosts
                        .write()
                        .unwrap_or_else(|error| error.into_inner());
                    if self
                        .state
                        .compare_exchange(STARTING, RUNNING, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        // Publication is one daemon-wide commit. If recovery
                        // of any Host failed above, no Host receives HostReady.
                        for service in prepared {
                            service.mark_published();
                        }
                        // The TCP listener was bound before recovery and the
                        // Unix listener above was bound after it. Both are
                        // ready at this publication boundary; the accept
                        // tasks are spawned immediately below.
                        startup::mcp_ready(mcp_endpoint, &self.startup);
                        true
                    } else {
                        false
                    }
                };
                if published {
                    let daemon = Arc::clone(&self);
                    let mcp = self.mcp.clone();
                    mcp_task = Some(tokio::spawn(async move {
                        crate::mcp::serve(daemon, mcp, mcp_listener, mcp_shutdown_rx).await
                    }));
                    let handler_daemon = Arc::clone(&self);
                    let socket = Arc::clone(&self.socket);
                    let unix_shutdown = self.shutdown.subscribe();
                    unix_task = Some(tokio::spawn(async move {
                        socket
                            .listen(unix_listener, unix_shutdown, move |stream| {
                                Arc::clone(&handler_daemon).serve_conn(stream)
                            })
                            .await
                    }));
                    startup::progress(StartupStage::InitializationComplete, &self.startup);
                }
            }
        }
        if self.state.load(Ordering::Acquire) != RUNNING {
            self.socket.remove_owned_path();
        }
        // ponytail: one provisioning job at a time; only parallelize after measured contention.
        let mut opening = JoinSet::new();
        while outcome.is_ok() && self.state.load(Ordering::Acquire) == RUNNING {
            tokio::select! {
                biased;
                result = mcp_task.as_mut().expect("MCP task installed") => {
                    outcome = join_result(result);
                    mcp_task = None;
                    break;
                }
                result = unix_task.as_mut().expect("Unix task installed") => {
                    outcome = join_result(result);
                    unix_task = None;
                    break;
                }
                _ = self.stop_requested.notified() => break,
                completed = opening.join_next(), if !opening.is_empty() => {
                    match completed {
                        Some(Ok((reply, result))) => {
                            self.finish_open(reply, result).await;
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
                        let result = daemon.prepare_open(request.kind).await;
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
                Ok((reply, result)) => self.finish_open(reply, result).await,
                Err(error) if outcome.is_ok() => outcome = Err(error.into()),
                Err(_) => {}
            }
        }
        self.shutdown.send_replace(true);
        let deadline = tokio::time::Instant::now() + SERVE_SHUTDOWN_TIMEOUT;
        if let Some(task) = mcp_task.take() {
            let result = join_server_task(task, deadline, "MCP").await;
            if outcome.is_ok() {
                outcome = result;
            }
        }
        if let Some(task) = unix_task.take() {
            let result = join_server_task(task, deadline, "Unix socket").await;
            if outcome.is_ok() {
                outcome = result;
            }
        }
        self.cleanup().await;
        outcome
    }

    async fn prepare_open(&self, kind: OpenKind) -> anyhow::Result<OpenedHost> {
        anyhow::ensure!(
            self.state.load(Ordering::Acquire) == RUNNING,
            "daemon is shutting down"
        );
        let (id, user_agent, expected_peer) = match kind {
            OpenKind::Create { id, user_agent } => (id, Some(user_agent), None),
            OpenKind::Existing { id, expected_peer } => (Some(id), None, Some(expected_peer)),
        };
        if let Some(ref id) = id
            && let Some(service) = self.service(id.as_str())
        {
            if let Some(expected_peer) = expected_peer {
                anyhow::ensure!(
                    service.peer_id() == expected_peer,
                    "existing Host identity does not match the authenticated peer"
                );
            } else if let Some(user_agent) = user_agent {
                service.set_user_agent(user_agent).await?;
            }
            return Ok(OpenedHost::Existing(service.host_info()));
        }
        if let Some(expected_peer) = expected_peer {
            let id =
                id.ok_or_else(|| anyhow::anyhow!("existing Host reopen requires a Host id"))?;
            anyhow::ensure!(
                self.services().len() < MAX_LOCAL_HOSTS,
                "daemon Host capacity reached ({MAX_LOCAL_HOSTS})"
            );
            let paths = Paths::from_location(&self.home.host(&id))?;
            let bootstrap = self.bootstrap_new_hosts;
            let allocated_id = id.clone();
            let config = tokio::task::spawn_blocking(move || {
                HostConfig::open_existing(id, paths, expected_peer, bootstrap)
            })
            .await
            .context("join existing Host provisioning")?
            .with_context(|| format!("reopen Host '{allocated_id}'"))?;
            return self
                .prepare_existing(config)
                .await
                .map(OpenedHost::Prepared)
                .with_context(|| format!("prepare existing Host '{allocated_id}'"));
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
        let bootstrap = self.bootstrap_new_hosts;
        let allocated_id = id.clone();
        let config = tokio::task::spawn_blocking(move || HostConfig::open(id, paths, bootstrap))
            .await
            .context("join Host provisioning")?
            .with_context(|| format!("open Host '{allocated_id}'"))?;
        self.prepare_new(
            config,
            user_agent.ok_or_else(|| anyhow::anyhow!("new Host requires a user agent"))?,
        )
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
        let service =
            match compose_service(&config, transport, runtime, &self.engine, &self.startup) {
                Ok(service) => service,
                Err(error) => {
                    self.close_provisional_host(peer_id, config.store).await?;
                    return Err(error);
                }
            };
        match service.prepare().await {
            Ok(()) => Ok(PreparedHost {
                id: config.name,
                slot: HostSlot {
                    service,
                    store: config.store,
                },
            }),
            Err(error) => {
                service.stop().await;
                self.close_provisional_host(peer_id, config.store).await?;
                Err(error)
            }
        }
    }

    async fn prepare_existing(&self, config: HostConfig) -> anyhow::Result<PreparedHost> {
        let peer_id = config.peer_id();
        let runtime = async {
            // Existing Hosts retain their durable metadata. Built-in program
            // registration remains idempotent and is allowed only after the
            // expected identity was verified by HostConfig::open_existing.
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
            .expect("existing transport installed");
        let service =
            match compose_service(&config, transport, runtime, &self.engine, &self.startup) {
                Ok(service) => service,
                Err(error) => {
                    self.close_provisional_host(peer_id, config.store).await?;
                    return Err(error);
                }
            };
        match service.prepare().await {
            Ok(()) => Ok(PreparedHost {
                id: config.name,
                slot: HostSlot {
                    service,
                    store: config.store,
                },
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
        let PreparedHost { id, slot } = prepared;
        let info = slot.service.host_info();
        let service = Arc::clone(&slot.service);
        let mut hosts = self
            .hosts
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let published = self.state.load(Ordering::Acquire) == RUNNING;
        if published {
            hosts.insert(id, slot);
            drop(hosts);
            service.mark_published();
            let _ = reply.send(Ok(info));
        } else {
            drop(hosts);
            self.finish_open_rollback(reply, slot).await;
        }
    }

    async fn finish_open_rollback(
        &self,
        reply: oneshot::Sender<anyhow::Result<HostInfo>>,
        slot: HostSlot,
    ) {
        let _ = slot.service.stop().await;
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

    /// Services stop before this releases provisional store ownership.
    async fn close_provisional_host(&self, peer_id: PeerId, store: Store) -> anyhow::Result<()> {
        if let Err(error) = self.ensemble.remove_host(&peer_id).await {
            tracing::error!(%peer_id, %error, "failed to detach provisional Host");
        }
        store.shutdown().await.context("close failed Host store")
    }

    pub(crate) fn services(&self) -> Vec<(String, Arc<HostService>)> {
        self.hosts
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .map(|(id, slot)| (id.to_string(), Arc::clone(&slot.service)))
            .collect()
    }

    pub(crate) fn activity(&self) -> Arc<Activity> {
        Arc::clone(&self.activity)
    }

    pub(crate) fn startup_timeline(&self) -> Arc<StartupTimeline> {
        Arc::clone(&self.startup)
    }

    pub(crate) fn shutdown_receiver(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn mcp_endpoint(&self) -> Option<SocketAddr> {
        *self
            .mcp_endpoint
            .read()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub(crate) fn service(&self, host: &str) -> Option<Arc<HostService>> {
        self.hosts
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .get(host)
            .map(|slot| Arc::clone(&slot.service))
    }

    /// Verify one JWT and resolve its durable Host through the supervised
    /// roster. A valid token for a persisted Host may lazily reopen that Host
    /// after a daemon restart; missing or mismatched identities never fall
    /// through to Host creation.
    pub(crate) async fn authorize_token(
        &self,
        token: RawToken,
    ) -> Result<crate::mcp::AuthorizedHost, AuthError> {
        let claims = self.mcp_auth.verify(token)?;
        let host_name = claims.host_name().clone();
        let peer_id = claims.peer_id();
        if let Some(service) = self.service(host_name.as_str()) {
            if service.peer_id() != peer_id {
                return Err(AuthError::InvalidToken);
            }
            return Ok(crate::mcp::AuthorizedHost::new(
                host_name, peer_id, service, claims,
            ));
        }
        self.open_existing(host_name.clone(), peer_id)
            .await
            .map_err(|_| AuthError::HostUnavailable)?;
        let service = self
            .service(host_name.as_str())
            .ok_or(AuthError::HostUnavailable)?;
        if service.peer_id() != peer_id {
            return Err(AuthError::InvalidToken);
        }
        Ok(crate::mcp::AuthorizedHost::new(
            host_name, peer_id, service, claims,
        ))
    }

    pub(crate) fn issue_token(
        &self,
        host_name: &HostName,
        peer_id: PeerId,
    ) -> anyhow::Result<crate::mcp_auth::IssuedToken> {
        self.mcp_auth.issue(host_name, peer_id)
    }

    pub(crate) async fn host_statuses(&self) -> Result<Vec<HostStatus>, ApiError> {
        let services = self.services();
        let mut statuses = Vec::with_capacity(services.len());
        for (_, service) in services {
            statuses.push(service.host_status().await?);
        }
        Ok(statuses)
    }

    pub(crate) fn daemon_info(&self) -> Result<DaemonInfo, ApiError> {
        let endpoint = self
            .mcp_endpoint
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .unwrap_or(self.mcp.listen);
        Ok(DaemonInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            abi_version: arena0_program::ABI_VERSION,
            uptime_secs: self.startup.elapsed().as_secs(),
            socket: self.home.socket().display().to_string(),
            mcp_endpoint: format!("http://{endpoint}/mcp"),
        })
    }

    async fn serve_conn(self: Arc<Self>, stream: UnixStream) -> anyhow::Result<()> {
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        while let Some(request) = frame::read_frame::<_, Request>(&mut read).await? {
            match request {
                Request::ActivitySubscribe => {
                    // Register before acknowledging so no activity frame can
                    // race between the ack and receiver creation.
                    let rx = self.activity.subscribe();
                    frame::write_frame(
                        &mut write,
                        &Ok::<_, ApiError>(ResponseOk::ActivitySubscribed),
                    )
                    .await?;
                    HostService::stream_activity_unix(&self.activity, rx, &mut read, &mut write)
                        .await?;
                    return Ok(());
                }
                Request::Host { host, request } => {
                    let host = match host.parse::<HostName>() {
                        Ok(host) => host,
                        Err(error) => {
                            frame::write_frame(
                                &mut write,
                                &Err::<ResponseOk, _>(ApiError::new(
                                    arena0_api::ApiErrorCode::BadRequest,
                                    format!("invalid Host name: {error}"),
                                )),
                            )
                            .await?;
                            continue;
                        }
                    };
                    let Some(service) = self.service(host.as_str()) else {
                        frame::write_frame(
                            &mut write,
                            &Err::<ResponseOk, _>(ApiError::new(
                                arena0_api::ApiErrorCode::NotFound,
                                format!("unknown Host '{host}'"),
                            )),
                        )
                        .await?;
                        continue;
                    };
                    if let HostRequest::EventsSubscribe { filter } = request {
                        // Register before acknowledging so events after this
                        // point cannot race between the ack and receiver setup.
                        let rx = service.events.subscribe();
                        let started = service.host_started_frame();
                        frame::write_frame(&mut write, &Ok::<_, ApiError>(ResponseOk::Subscribed))
                            .await?;
                        frame::write_frame(&mut write, &started).await?;
                        service
                            .stream_events_unix(filter, rx, &mut read, &mut write)
                            .await?;
                        return Ok(());
                    }
                    let response = service.dispatch(request).await;
                    frame::write_frame(&mut write, &response).await?;
                }
                Request::DaemonInfo => {
                    let response = self.daemon_info().map(ResponseOk::DaemonInfo);
                    frame::write_frame(&mut write, &response).await?;
                }
                Request::HostsList => {
                    let response = self.host_statuses().await.map(ResponseOk::Hosts);
                    frame::write_frame(&mut write, &response).await?;
                }
                Request::HostsOpen { id, user_agent } => {
                    let response = self
                        .open_host(id, user_agent)
                        .await
                        .map(ResponseOk::HostOpened)
                        .map_err(|error| {
                            ApiError::new(arena0_api::ApiErrorCode::BadRequest, error.to_string())
                        });
                    frame::write_frame(&mut write, &response).await?;
                }
                Request::DaemonStop => {
                    // Write the acknowledgement before waking the supervisor;
                    // the connection task never waits for its own listener.
                    frame::write_frame(&mut write, &Ok::<_, ApiError>(ResponseOk::Ack)).await?;
                    self.stop_requested.notify_one();
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn begin_shutdown(&self) {
        let _hosts = self
            .hosts
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let _ = self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                matches!(state, STARTING | RUNNING).then_some(FINISHING)
            });
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
        let hosts = std::mem::take(
            &mut *self
                .hosts
                .write()
                .unwrap_or_else(|error| error.into_inner()),
        );
        futures::future::join_all(hosts.values().map(|slot| slot.service.stop())).await;
        self.socket.remove_owned_path();
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
) -> anyhow::Result<Arc<HostService>> {
    let store = config.store.handle().clone();
    HostService::start_with_runtime(
        HostServiceInit {
            name: config.name.to_string(),
            transport,
            keystore: Arc::clone(&config.keystore),
            catalog: ProgramCatalog::new(store.clone()),
            store,
            engine: Arc::clone(engine),
            startup: Arc::clone(startup),
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

fn validate_host_names(names: &[HostName]) -> anyhow::Result<()> {
    anyhow::ensure!(
        names.len() <= MAX_LOCAL_HOSTS,
        "daemon Host capacity exceeded ({MAX_LOCAL_HOSTS})"
    );
    let mut seen = BTreeSet::new();
    for host in names {
        anyhow::ensure!(
            seen.insert(host.clone()),
            "ensemble hosts must use distinct names; duplicate {host}"
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

async fn join_server_task(
    mut task: JoinHandle<anyhow::Result<()>>,
    deadline: tokio::time::Instant,
    label: &str,
) -> anyhow::Result<()> {
    match tokio::time::timeout_at(deadline, &mut task).await {
        Ok(result) => join_result(result),
        Err(_) if label == "MCP" => {
            // axum owns the accepted HTTP connection tasks. Aborting its
            // top-level future would detach those tasks from this owner and
            // let them race the Host store shutdown. MCP call_tool observes
            // the daemon watch above, so wait for axum to drain them even if
            // the normal shutdown budget has elapsed.
            tracing::error!("MCP server exceeded shutdown deadline; waiting for graceful drain");
            join_result(task.await)
        }
        Err(_) => {
            tracing::error!(
                server = label,
                "serve child exceeded shutdown deadline; aborting"
            );
            task.abort();
            join_result(task.await)
        }
    }
}

#[cfg(test)]
mod construction_tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_start_releases_home_only_after_store_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let home = Home::from_root(directory.path().to_owned()).unwrap();
        let engine = Arc::new(WasmtimeEngine::new().unwrap());
        let names = vec!["first".parse().unwrap(), "second".parse().unwrap()];
        let mcp = McpConfig::new("127.0.0.1:0".parse().unwrap(), None).unwrap();
        let starting = tokio::spawn(Daemon::start(
            names.clone(),
            mcp.clone(),
            Arc::clone(&engine),
            home.clone(),
            true,
        ));
        // Observe real construction after it opens its stores, while bundled
        // program admission is still running. No synthetic store owner stands
        // in for the operation being cancelled.
        tokio::time::timeout(Duration::from_secs(30), async {
            while !home
                .host(&names[1])
                .state_dir()
                .join("arena0.sqlite")
                .exists()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            !starting.is_finished(),
            "construction must still be in progress"
        );
        starting.abort();
        assert!(starting.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Ok(lease) = HomeLease::acquire_home(&home) {
                    drop(lease);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled construction retained the home lease");
        let restarted = Daemon::start(names, mcp, engine, home, false)
            .await
            .expect("stores must be reusable as soon as the home lease is released");
        restarted.stop().await;
    }
}
