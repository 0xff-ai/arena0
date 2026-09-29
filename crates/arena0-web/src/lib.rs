//! The browser workspace for arena0: a loopback HTTP server that serves the
//! embedded web UI and bridges one WebSocket per tab to the daemon's Unix
//! socket.
//!
//! The gateway keeps an in-memory replica of daemon facts (see [`model`]),
//! reconciled from durable reads and kept current from the daemon's event
//! streams, and replicates it to every connected tab (see [`protocol`]). It
//! stores nothing on disk.
//!
//! Security: the listener binds 127.0.0.1 only. The WebSocket upgrade must
//! present the per-launch token as the `arena0.token.<hex>` subprotocol,
//! an exact loopback `Host` header, and a same-origin `Origin`. The gateway
//! sends no CORS headers. Static assets are public build output and need no
//! token.

mod assets;
mod decode;
mod http;
pub mod model;
mod ops;
pub mod protocol;
mod replica;
mod socket;

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, bail};
use arena0_client::proto::DaemonClient;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::protocol::{ErrorRow, LaunchArgs, LaunchReply, StrategyRow};

/// How long the listener may take to drain after every socket has closed.
const SERVER_DRAIN: Duration = Duration::from_secs(5);

/// How to run the gateway.
#[derive(Debug, Clone)]
pub struct UiConfig {
    /// The daemon socket to bridge.
    pub socket: PathBuf,
    /// Loopback port to bind; 0 picks a free port.
    pub port: u16,
    /// One extra origin, such as `http://127.0.0.1:5173`, allowed to open the
    /// WebSocket through a development server's proxy. Its host:port is also
    /// accepted as the `Host` header.
    pub dev_origin: Option<String>,
    /// True when the caller borrowed a daemon it did not start.
    pub attached: bool,
}

/// Starts sessions whose seats need local drivers. The CLI owns driver
/// processes and strategies, so it implements this.
pub trait Launcher: Send + Sync + 'static {
    /// Built-in strategies offered for `builtin` seats.
    fn strategies(&self) -> Vec<StrategyRow>;

    /// Create the session's executions, then keep driving its `builtin` and
    /// `executable` seats in the background until the session ends. Resolves
    /// once every seat's execution exists.
    fn launch(
        &self,
        args: LaunchArgs,
    ) -> Pin<Box<dyn Future<Output = Result<LaunchReply, ErrorRow>> + Send>>;
}

/// A running gateway.
#[derive(Debug)]
pub struct UiServer {
    addr: SocketAddr,
    token: String,
    shutdown: watch::Sender<bool>,
    /// The sender every socket task clones; taken at shutdown so the channel
    /// closes once the last socket ends.
    sockets_tx: Arc<Mutex<Option<mpsc::Sender<()>>>>,
    sockets_rx: mpsc::Receiver<()>,
    server: JoinHandle<()>,
    replica: replica::Replica,
    replica_tasks: replica::ReplicaTasks,
}

impl UiServer {
    /// Bind the listener, start the replica, and serve until
    /// [`UiServer::shutdown`]. Fails when the daemon is unreachable or this
    /// binary was built without UI assets.
    pub async fn start(
        config: UiConfig,
        launcher: std::sync::Arc<dyn Launcher>,
    ) -> anyhow::Result<Self> {
        if !has_assets() {
            bail!("this arena0 was built without the web UI; run `just build-ui` and rebuild");
        }
        let dev = config
            .dev_origin
            .as_deref()
            .map(parse_dev_origin)
            .transpose()?;
        let client = DaemonClient::new(config.socket.clone());
        let (replica, replica_tasks) = replica::Replica::start(client.clone()).await?;
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", config.port))
            .await
            .context("bind the loopback listener")
        {
            Ok(listener) => listener,
            Err(error) => {
                replica.stop().await;
                replica_tasks.join().await;
                return Err(error);
            }
        };
        let addr = listener.local_addr()?;
        let token = hex::encode(rand::random::<[u8; 32]>());

        let mut hosts = vec![
            format!("127.0.0.1:{}", addr.port()),
            format!("localhost:{}", addr.port()),
        ];
        let mut origins = vec![
            format!("http://127.0.0.1:{}", addr.port()),
            format!("http://localhost:{}", addr.port()),
        ];
        if let Some((origin, host)) = dev {
            origins.push(origin);
            hosts.push(host);
        }
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (sockets, sockets_rx) = mpsc::channel(1);
        let sockets_tx = Arc::new(Mutex::new(Some(sockets)));
        let gateway = Arc::new(http::Gateway {
            token: token.clone(),
            hosts,
            origins,
            ops: ops::Ops::new(client.clone(), launcher.clone(), replica.clone()),
            client,
            replica: replica.clone(),
            launcher,
            attached: config.attached,
            shutdown: shutdown_rx.clone(),
            sockets: sockets_tx.clone(),
        });
        let mut stop = shutdown_rx;
        let server = tokio::spawn(async move {
            let serve =
                axum::serve(listener, http::router(gateway)).with_graceful_shutdown(async move {
                    let _ = stop.wait_for(|stopping| *stopping).await;
                });
            if let Err(error) = serve.await {
                tracing::warn!("web server stopped: {error}");
            }
        });
        Ok(Self {
            addr,
            token,
            shutdown,
            sockets_tx,
            sockets_rx,
            server,
            replica,
            replica_tasks,
        })
    }

    /// The page URL, with the token in the fragment so it stays out of
    /// request logs and referrers.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}/#token={}", self.addr, self.token)
    }

    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Close every WebSocket and stop the listener and the replica.
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        self.shutdown.send_replace(true);
        self.sockets_tx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        // No socket ever sends here; the channel closes when the last one ends.
        while self.sockets_rx.recv().await.is_some() {}
        if tokio::time::timeout(SERVER_DRAIN, &mut self.server)
            .await
            .is_err()
        {
            self.server.abort();
        }
        self.replica.stop().await;
        self.replica_tasks.join().await;
        Ok(())
    }
}

/// `http://host:port` → (the origin, its `host:port`).
fn parse_dev_origin(origin: &str) -> anyhow::Result<(String, String)> {
    let origin = origin.trim_end_matches('/');
    let authority = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .filter(|authority| !authority.is_empty() && !authority.contains('/'));
    match authority {
        Some(authority) => Ok((origin.to_owned(), authority.to_owned())),
        None => bail!("the development origin must look like http://127.0.0.1:5173"),
    }
}

/// Whether this binary embeds the built web UI.
#[must_use]
pub fn has_assets() -> bool {
    !assets::ASSETS.is_empty()
}
