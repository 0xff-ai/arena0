//! The daemon's loopback HTTP transport: MCP, JSON requests, SSE, and browser files.
use std::collections::BTreeSet;
use std::convert::Infallible;
use std::future::Future as _;
use std::io::IoSlice;
use std::sync::Arc;
use std::task::{Context, Poll};

use arena0_api::{EventFilter, Request, Response, Uploaded};
use arena0_protocol::BlobHash;
use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Request as HttpRequest, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{
    IntoResponse, Response as HttpResponse, Sse,
    sse::{Event, KeepAlive},
};
use axum::routing::{get, post};
use futures::{
    FutureExt as _, Stream, StreamExt as _,
    future::{BoxFuture, Shared},
    stream::{BoxStream, select_all},
};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

use crate::ensemble::{Caller, Daemon, HttpConfig};
use crate::server::activity_stream;
use crate::startup::StartupStage;
use crate::ui_assets::UiDir;

pub(crate) const MAX_UPLOAD_BYTES: usize = 64 * 1024 * 1024;

// The existing shutdown watch also interrupts incomplete HTTP headers and
// blocked writes. Shared owns and removes each connection's wake registration.
type ConnectionShutdown = Shared<BoxFuture<'static, ()>>;

struct ShutdownTcpStream {
    inner: TcpStream,
    shutdown: ConnectionShutdown,
    closed: bool,
}

impl ShutdownTcpStream {
    fn is_closed(&mut self, context: &mut Context<'_>) -> bool {
        if !self.closed {
            self.closed = std::pin::Pin::new(&mut self.shutdown)
                .poll(context)
                .is_ready();
        }
        self.closed
    }
}

impl AsyncRead for ShutdownTcpStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.is_closed(context) {
            return Poll::Ready(Ok(()));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl AsyncWrite for ShutdownTcpStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.is_closed(context) {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        std::pin::Pin::new(&mut self.inner).poll_write(context, buffer)
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        if self.is_closed(context) {
            return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        std::pin::Pin::new(&mut self.inner).poll_write_vectored(context, buffers)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

struct HttpListener {
    inner: TcpListener,
    shutdown: ConnectionShutdown,
}

impl HttpListener {
    fn new(inner: TcpListener, shutdown: ConnectionShutdown) -> Self {
        Self { inner, shutdown }
    }
}

impl axum::serve::Listener for HttpListener {
    type Io = ShutdownTcpStream;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((stream, address)) => {
                    return (
                        ShutdownTcpStream {
                            inner: stream,
                            shutdown: self.shutdown.clone(),
                            closed: false,
                        },
                        address,
                    );
                }
                Err(error) => {
                    tracing::warn!(%error, "MCP accept failed; retrying");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

pub(crate) async fn serve(
    daemon: Arc<Daemon>,
    config: HttpConfig,
    listener: TcpListener,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let startup = daemon.startup_timeline();
    let router = router(daemon, config.bearer_token, config.ui);
    let address = match listener.local_addr() {
        Ok(address) => address,
        Err(error) => {
            startup.progress(StartupStage::Failed);
            return Err(error.into());
        }
    };
    tracing::info!(endpoint = %format_args!("http://{address}"), "arena0d HTTP listening");
    let mut connection_shutdown = shutdown.clone();
    let connection_shutdown = async move {
        if !*connection_shutdown.borrow() {
            let _ = connection_shutdown.changed().await;
        }
    }
    .boxed()
    .shared();
    let listener = HttpListener::new(listener, connection_shutdown);
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            if !*shutdown.borrow() {
                let _ = shutdown.changed().await;
            }
        })
        .await?;
    Ok(())
}

fn router(
    daemon: Arc<Daemon>,
    bearer_token: Option<String>,
    ui: Option<Arc<UiDir>>,
) -> axum::Router {
    let shutdown = daemon.shutdown_receiver();
    // `ARENA0_MCP_TOKEN` guards MCP only. The browser routes are loopback-only
    // and unauthenticated: EventSource cannot send an Authorization header.
    let mcp =
        axum::Router::new().nest_service("/mcp", crate::mcp::mcp_service(Arc::clone(&daemon)));
    let mcp = match bearer_token {
        Some(token) => mcp.layer(middleware::from_fn_with_state(
            Arc::<str>::from(token),
            require_bearer,
        )),
        None => mcp,
    };
    axum::Router::new()
        .route("/rpc", post(rpc))
        .route("/events", get(events))
        .route("/sync", get(sync))
        .route("/sync/{cursor}", get(sync_from))
        .route(
            "/uploads",
            post(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES)),
        )
        .route("/hosts/{host}/blobs/{hash}", get(blob))
        .fallback(get(move |uri: Uri| asset(ui, uri)))
        .with_state(daemon)
        .merge(mcp)
        .layer(middleware::from_fn_with_state(
            shutdown,
            cancel_inflight_request,
        ))
}

async fn rpc(State(daemon): State<Arc<Daemon>>, Json(request): Json<Request>) -> Json<Response> {
    Json(daemon.handle(request, Caller::Http).await)
}

async fn events(
    State(daemon): State<Arc<Daemon>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Subscribe before taking the roster snapshot. Dedupe publication
    // notifications against that snapshot so opens cannot be lost or repeated.
    let mut opened = daemon.subscribe_host_opened();
    let activity = daemon.activity();
    let activity_rx = activity.subscribe();
    let mut streams: futures::stream::SelectAll<BoxStream<'static, Event>> = select_all(vec![
        activity_stream(activity, activity_rx)
            .map(|frame| {
                Event::default()
                    .event("activity")
                    .json_data(frame)
                    .expect("activity serializes")
            })
            .boxed(),
    ]);
    let mut seen = BTreeSet::new();
    let host_daemon = Arc::clone(&daemon);
    let add_hosts = move |streams: &mut futures::stream::SelectAll<BoxStream<'static, Event>>,
                          seen: &mut BTreeSet<String>| {
        for (host, service) in host_daemon.services() {
            if !seen.insert(host) {
                continue;
            }
            let rx = service.events.subscribe();
            let started = service.host_started_frame();
            let stream = futures::stream::once(async move { started })
                .chain(service.event_stream(EventFilter::all(), rx))
                .map(|frame| {
                    Event::default()
                        .event("host")
                        .json_data(frame)
                        .expect("event serializes")
                });
            streams.push(stream.boxed());
        }
    };
    add_hosts(&mut streams, &mut seen);
    let (tx, rx) = mpsc::channel::<Event>(64);
    let mut shutdown = daemon.shutdown_receiver();
    // This one task owns every subscription for this connection. Receiver
    // closure interrupts idle streams and blocked sends on browser disconnect.
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tx.closed() => break,
                _ = shutdown.changed() => break,
                message = opened.recv() => {
                    match message {
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            add_hosts(&mut streams, &mut seen);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
                message = streams.next() => {
                    let Some(message) = message else { break; };
                    tokio::select! {
                        result = tx.send(message) => { if result.is_err() { break; } }
                        _ = shutdown.changed() => break,
                    }
                }
            }
        }
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|message| (Ok::<_, Infallible>(message), rx))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// `GET /sync`: a stream with no cursor (every Host is snapshotted). Each SSE
/// event is named `sync` and carries one `SyncFrame` as JSON.
async fn sync(State(daemon): State<Arc<Daemon>>) -> HttpResponse {
    let stream = crate::sync::sync_frames(daemon, arena0_api::SyncCursor::default()).map(|frame| {
        Ok::<_, Infallible>(
            Event::default()
                .event("sync")
                .json_data(frame)
                .expect("sync frame serializes"),
        )
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// `GET /sync/{cursor}`: resume from `cursor`, the lowercase hex encoding of
/// a `SyncCursor`'s JSON bytes (hex keeps it a single path segment without
/// percent-decoding). A cursor that is not hex or not a valid `SyncCursor` is
/// a client error: 400 with the parse error as text, no stream.
async fn sync_from(State(daemon): State<Arc<Daemon>>, Path(cursor): Path<String>) -> HttpResponse {
    let bytes = match hex::decode(cursor) {
        Ok(bytes) => bytes,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let cursor = match serde_json::from_slice::<arena0_api::SyncCursor>(&bytes) {
        Ok(cursor) => cursor,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let stream = crate::sync::sync_frames(daemon, cursor).map(|frame| {
        Ok::<_, Infallible>(
            Event::default()
                .event("sync")
                .json_data(frame)
                .expect("sync frame serializes"),
        )
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

async fn upload(
    State(daemon): State<Arc<Daemon>>,
    headers: HeaderMap,
    body: Bytes,
) -> HttpResponse {
    let suffix = match headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        Some("application/wasm") => "wasm",
        Some("application/octet-stream") => "bin",
        _ => return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response(),
    };
    let hash = BlobHash(*blake3::hash(&body).as_bytes());
    let directory = daemon.home.uploads_dir();
    let temporary = directory.join(format!("upload-{:016x}", rand::random::<u64>()));
    let result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await?;
        file.write_all(&body).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, directory.join(format!("{hash}.{suffix}"))).await
    }
    .await;
    match result {
        Ok(()) => (
            StatusCode::CREATED,
            Json(Uploaded {
                upload: hash,
                length: body.len() as u64,
            }),
        )
            .into_response(),
        Err(error) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            tracing::warn!(%error, "upload failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn blob(
    State(daemon): State<Arc<Daemon>>,
    Path((host, hash)): Path<(String, String)>,
) -> HttpResponse {
    let Some(service) = daemon.service(&host) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(hash) = hash.parse::<BlobHash>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let location = match service.store.blob_file(hash).await {
        Ok(Some(location)) => location,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => {
            tracing::warn!(%error, "blob lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let (path, length) = location;
    assert!(
        length <= arena0_protocol::MAX_BLOB_BYTES,
        "stored blob exceeds limit"
    );
    let result = async {
        let mut file = tokio::fs::File::open(path).await?;
        let mut bytes = vec![0; usize::try_from(length).expect("blob length fits memory")];
        file.read_exact(&mut bytes).await?;
        Ok::<_, std::io::Error>(bytes)
    }
    .await;
    match result {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{hash}\""),
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "blob read failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn require_bearer(
    State(expected): State<Arc<str>>,
    request: HttpRequest,
    next: Next,
) -> HttpResponse {
    let supplied = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if supplied.is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes())) {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            "unauthorized",
        )
            .into_response()
    }
}

async fn cancel_inflight_request(
    State(mut shutdown): State<watch::Receiver<bool>>,
    request: HttpRequest,
    next: Next,
) -> HttpResponse {
    if *shutdown.borrow() {
        return (StatusCode::SERVICE_UNAVAILABLE, "daemon is shutting down").into_response();
    }
    tokio::select! {
        response = next.run(request) => response,
        changed = shutdown.changed() => {
            let _ = changed;
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "daemon is shutting down",
            )
                .into_response()
        }
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

const CSP: &str = "default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; font-src 'self'; script-src 'self'; frame-ancestors 'none'";

fn secure(mut response: HttpResponse) -> HttpResponse {
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

/// Answer a GET outside the API routes from the web UI, if the daemon has one.
///
/// With a UI: an existing file under the UI directory, `assets/*` cached as
/// immutable and everything else `no-cache`; a missing `assets/*` file or an
/// API route name (`rpc`, `events`, `uploads`, `mcp`) is 404; any other path
/// gets `index.html`, so the UI's client-side routes load. Without a UI: 404
/// with a plain-text body naming `ARENA0_UI_DIR`. Every response goes
/// through `secure`.
async fn asset(ui: Option<Arc<UiDir>>, uri: Uri) -> HttpResponse {
    let Some(ui) = ui else {
        return secure(
            (
                StatusCode::NOT_FOUND,
                [(header::CONTENT_TYPE, "text/plain")],
                "this daemon serves no web UI; start it with ARENA0_UI_DIR set to a built arena0-ui",
            )
                .into_response(),
        );
    };
    let path = uri.path().trim_start_matches('/');
    let (found, cache) = match ui.find(path).await {
        Some(found) if path.starts_with("assets/") => {
            (found, "public, max-age=31536000, immutable")
        }
        Some(found) => (found, "no-cache"),
        None if path.starts_with("assets/")
            || matches!(path, "rpc" | "events" | "uploads" | "mcp") =>
        {
            return secure(StatusCode::NOT_FOUND.into_response());
        }
        None => match ui.find("index.html").await {
            Some(index) => (index, "no-cache"),
            None => {
                return secure(
                    (
                        StatusCode::NOT_FOUND,
                        [(header::CONTENT_TYPE, "text/plain")],
                        "ARENA0_UI_DIR has no index.html",
                    )
                        .into_response(),
                );
            }
        },
    };
    secure(
        (
            [
                (header::CONTENT_TYPE, found.content_type),
                (header::CACHE_CONTROL, cache),
            ],
            Body::from(found.bytes),
        )
            .into_response(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn shutdown_stream_remains_closed_across_reads_and_writes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (stop, mut stopped) = watch::channel(false);
        let shutdown = async move {
            let _ = stopped.changed().await;
        }
        .boxed()
        .shared();
        let mut listener = HttpListener::new(listener, shutdown);
        let (mut stream, _) = axum::serve::Listener::accept(&mut listener).await;
        stop.send_replace(true);
        let mut buffer = [0; 1];
        assert_eq!(stream.read(&mut buffer).await.unwrap(), 0);
        assert_eq!(
            stream.write(b"x").await.unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        assert_eq!(stream.read(&mut buffer).await.unwrap(), 0);
        drop(peer);
    }

    #[test]
    fn bearer_comparison_rejects_prefixes_and_suffixes() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(!constant_time_eq(b"secret", b"secret2"));
        assert!(!constant_time_eq(b"secret", b"Secret"));
    }
}
