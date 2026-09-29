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

pub mod model;
pub mod protocol;

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;

use crate::protocol::{ErrorRow, LaunchArgs, LaunchReply, StrategyRow};

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
}

impl UiServer {
    /// Bind the listener, start the replica, and serve until
    /// [`UiServer::shutdown`]. Fails when the daemon is unreachable or this
    /// binary was built without UI assets.
    pub async fn start(
        config: UiConfig,
        launcher: std::sync::Arc<dyn Launcher>,
    ) -> anyhow::Result<Self> {
        let _ = (config, launcher);
        todo!()
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
    pub async fn shutdown(self) -> anyhow::Result<()> {
        todo!()
    }
}

/// Whether this binary embeds the built web UI.
#[must_use]
pub fn has_assets() -> bool {
    todo!()
}
