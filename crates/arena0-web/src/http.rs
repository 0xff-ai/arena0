//! The loopback HTTP surface: the WebSocket upgrade behind its security
//! checks, and the embedded assets.

use std::sync::Arc;

use arena0_client::proto::DaemonClient;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use subtle::ConstantTimeEq as _;
use tokio::sync::{mpsc, watch};

use crate::Launcher;
use crate::assets;
use crate::ops::Ops;
use crate::protocol::DaemonRow;
use crate::replica::Replica;

pub(crate) const SUBPROTOCOL: &str = "arena0.v1";
const TOKEN_PREFIX: &str = "arena0.token.";
const CSP: &str = "default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; font-src 'self'; script-src 'self'; frame-ancestors 'none'";

/// Everything a request or socket needs, shared behind one `Arc`.
pub(crate) struct Gateway {
    pub(crate) token: String,
    /// Exact `Host` header values the upgrade accepts.
    pub(crate) hosts: Vec<String>,
    /// Exact `Origin` header values the upgrade accepts.
    pub(crate) origins: Vec<String>,
    pub(crate) client: DaemonClient,
    pub(crate) ops: Ops,
    pub(crate) replica: Replica,
    pub(crate) launcher: Arc<dyn Launcher>,
    pub(crate) attached: bool,
    /// Turns true when the server is shutting down.
    pub(crate) shutdown: watch::Receiver<bool>,
    /// Each socket task holds a clone until it ends; `None` once shutdown
    /// began, when no new socket is accepted.
    pub(crate) sockets: Arc<std::sync::Mutex<Option<mpsc::Sender<()>>>>,
}

impl Gateway {
    pub(crate) async fn daemon_row(&self) -> anyhow::Result<DaemonRow> {
        let response = self.client.call(&arena0_api::Request::DaemonInfo).await?;
        let arena0_api::ResponseOk::DaemonInfo(info) = response else {
            anyhow::bail!("unexpected daemon.info response");
        };
        Ok(DaemonRow {
            version: info.version,
            abi_version: info.abi_version,
            uptime_secs: info.uptime_secs,
            socket: info.socket,
            mcp_endpoint: info.mcp_endpoint,
        })
    }

    /// The upgrade is allowed only for a loopback `Host`, a same-origin
    /// `Origin`, and the per-launch token offered as a subprotocol. Nothing
    /// here is logged: a rejected request may carry the token.
    fn authorized(&self, headers: &HeaderMap) -> bool {
        let text = |name: HeaderName| headers.get(name).and_then(|value| value.to_str().ok());
        let host_ok = text(header::HOST).is_some_and(|host| self.hosts.iter().any(|ok| ok == host));
        let origin_ok =
            text(header::ORIGIN).is_some_and(|origin| self.origins.iter().any(|ok| ok == origin));
        let offered: Vec<&str> = headers
            .get_all(header::SEC_WEBSOCKET_PROTOCOL)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .collect();
        let version_ok = offered.contains(&SUBPROTOCOL);
        // Compare every candidate without stopping at the first match.
        let token_ok = offered
            .iter()
            .filter_map(|protocol| protocol.strip_prefix(TOKEN_PREFIX))
            .fold(false, |found, candidate| {
                found | bool::from(candidate.as_bytes().ct_eq(self.token.as_bytes()))
            });
        host_ok & origin_ok & version_ok & token_ok
    }
}

pub(crate) fn router(gateway: Arc<Gateway>) -> Router {
    Router::new()
        .route("/ws", any(upgrade))
        .fallback(get(asset))
        .with_state(gateway)
}

/// Add the headers every response carries. No CORS headers are ever added.
fn secure(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    response
}

async fn upgrade(
    State(gateway): State<Arc<Gateway>>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    if !gateway.authorized(&headers) {
        return secure(StatusCode::FORBIDDEN.into_response());
    }
    secure(match upgrade {
        Ok(upgrade) => upgrade
            .protocols([SUBPROTOCOL])
            .on_upgrade(move |socket| crate::socket::serve(socket, gateway)),
        Err(rejection) => rejection.into_response(),
    })
}

async fn asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let (found, cache) = match assets::find(path) {
        Some(found) if path.starts_with("assets/") => {
            (found, "public, max-age=31536000, immutable")
        }
        Some(found) => (found, "no-cache"),
        // A missing hashed asset must not turn into HTML the browser would
        // try to run as script.
        None if path.starts_with("assets/") => {
            return secure(StatusCode::NOT_FOUND.into_response());
        }
        // Every other path is a client-side route.
        None => (
            assets::find("index.html").expect("index.html is embedded when assets exist"),
            "no-cache",
        ),
    };
    let (bytes, content_type) = found;
    secure(
        (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, cache),
            ],
            Body::from(bytes),
        )
            .into_response(),
    )
}
