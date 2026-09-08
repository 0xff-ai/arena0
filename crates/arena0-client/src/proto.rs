//! Thin unix-socket client: one framed request/response per call and a streaming
//! subscription for `events.subscribe`. Client-side prefix resolution lives in
//! [`crate::resolve`].

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use arena0_api::frame;
pub use arena0_api::{ActivityFrame, EventFrame};
use arena0_api::{
    ApiErrorCode, EventFilter, HostInfo, HostRequest, HostStatus, Request, Response, ResponseOk,
};
use arena0_home::{Home, HomeError, HostName};
use arena0_program::ProgramHash;
use arena0_protocol::{ExecId, View, Viewport};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

/// Whether an error from [`DaemonClient::call_raw`] is a failure to reach the daemon at all
/// (socket missing or refused) rather than a protocol-level failure. Frontends
/// map these to a friendly "start the daemon" message.
#[must_use]
pub fn is_connect_error(e: &anyhow::Error) -> bool {
    // Connection-refused/reset kinds are unambiguous. For the socket-missing
    // case (NotFound) require the "connect to daemon" context, so a NotFound io
    // error from a receipt-file read is not mislabeled as a dead daemon.
    let is_conn_kind = |kind: &std::io::ErrorKind| {
        matches!(
            kind,
            std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::TimedOut
        )
    };
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| is_conn_kind(&io.kind()))
    }) || (e.to_string().contains("connect to daemon")
        && e.chain()
            .any(|c| c.downcast_ref::<std::io::Error>().is_some()))
}

/// A bound daemon-socket client: one framed request/response per call, plus a
/// streaming subscription for `events.subscribe`.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket: PathBuf,
}

impl DaemonClient {
    /// Bind to the daemon socket at `socket`.
    #[must_use]
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// Bind to the one daemon endpoint from the captured process environment.
    pub fn from_env() -> Result<Self, HomeError> {
        Ok(Self::new(Home::from_env()?.socket()))
    }

    /// The socket path this client talks to.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// One framed request, returning the raw [`Response`] (success or `ApiError`). The
    /// caller decides how to render an error, so a card or doctor can degrade instead of
    /// aborting.
    pub async fn call_raw(&self, req: &Request) -> anyhow::Result<Response> {
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connect to daemon at {}", self.socket.display()))?;
        let (read, mut write) = stream.split();
        let mut read = BufReader::new(read);
        frame::write_frame(&mut write, req).await?;
        frame::read_frame::<_, Response>(&mut read)
            .await?
            .ok_or_else(|| anyhow!("daemon closed the connection"))
    }

    /// One framed request, unwrapping to the success payload (mapping `ApiError` to an
    /// anyhow error).
    pub async fn call(&self, req: &Request) -> anyhow::Result<ResponseOk> {
        self.call_raw(req).await?.map_err(|e| anyhow!("{e}"))
    }

    /// Dispatch an operation to an explicit Host through the shared endpoint.
    pub async fn call_host_raw(
        &self,
        host: &HostName,
        request: &HostRequest,
    ) -> anyhow::Result<Response> {
        self.call_raw(&Request::Host {
            host: host.to_string(),
            request: request.clone(),
        })
        .await
    }

    /// Dispatch a Host operation, returning its successful payload.
    pub async fn call_host(
        &self,
        host: &HostName,
        request: &HostRequest,
    ) -> anyhow::Result<ResponseOk> {
        self.call_host_raw(host, request)
            .await?
            .map_err(|error| anyhow!("{error}"))
    }

    /// Fetch a program-authored execution view for a terminal viewport.
    ///
    /// A Host can legitimately have no view while an execution is still
    /// negotiating or otherwise has no active/terminal session. That
    /// execution-level API error is represented as `None`; transport errors,
    /// other API errors, and mismatched success payloads remain failures.
    pub async fn exec_view(
        &self,
        host: &HostName,
        exec: ExecId,
        viewport: Viewport,
    ) -> anyhow::Result<Option<(u64, View)>> {
        match self
            .call_host_raw(
                host,
                &HostRequest::ExecView {
                    exec,
                    width: viewport.width,
                    color: viewport.color,
                },
            )
            .await?
        {
            Ok(ResponseOk::ExecView { step, view }) => Ok(Some((step, view))),
            Ok(other) => bail!("unexpected response to exec.view: {other:?}"),
            Err(error) if error.code == ApiErrorCode::Execution => Ok(None),
            Err(error) => bail!("{error}"),
        }
    }

    /// Open or create one Host through the daemon's existing provisioning owner.
    pub async fn open_host(
        &self,
        id: Option<String>,
        user_agent: String,
    ) -> anyhow::Result<HostInfo> {
        match self.call(&Request::HostsOpen { id, user_agent }).await? {
            ResponseOk::HostOpened(info) => Ok(info),
            other => bail!("unexpected hosts.open response: {other:?}"),
        }
    }

    /// Whether a daemon is reachable and answering on the bound socket (`daemon.info` probe).
    pub async fn daemon_up(&self) -> bool {
        matches!(
            self.call_raw(&Request::DaemonInfo).await,
            Ok(Ok(ResponseOk::DaemonInfo(_)))
        )
    }

    /// Open an `events.subscribe` stream, consuming the `Subscribed` ack.
    pub async fn subscribe(
        &self,
        host: &HostName,
        filter: EventFilter,
    ) -> anyhow::Result<Subscription> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connect to daemon at {}", self.socket.display()))?;
        let (read, write) = stream.into_split();
        let mut read = BufReader::new(read);
        let mut write = write;
        frame::write_frame(
            &mut write,
            &Request::Host {
                host: host.to_string(),
                request: HostRequest::EventsSubscribe { filter },
            },
        )
        .await?;
        match frame::read_frame::<_, Response>(&mut read).await? {
            Some(Ok(ResponseOk::Subscribed)) => {}
            Some(Ok(other)) => bail!("unexpected subscribe response: {other:?}"),
            Some(Err(e)) => bail!("subscribe failed: {e}"),
            None => bail!("daemon closed the connection before acking the subscription"),
        }
        Ok(Subscription {
            read,
            _write: write,
        })
    }

    /// Open the daemon-wide MCP activity stream, consuming its ack.
    pub async fn subscribe_activity(&self) -> anyhow::Result<ActivitySubscription> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connect to daemon at {}", self.socket.display()))?;
        let (read, write) = stream.into_split();
        let mut read = BufReader::new(read);
        let mut write = write;
        frame::write_frame(&mut write, &Request::ActivitySubscribe).await?;
        match frame::read_frame::<_, Response>(&mut read).await? {
            Some(Ok(ResponseOk::ActivitySubscribed)) => {}
            Some(Ok(other)) => bail!("unexpected activity subscribe response: {other:?}"),
            Some(Err(e)) => bail!("activity subscribe failed: {e}"),
            None => bail!("daemon closed the connection before acking the activity subscription"),
        }
        Ok(ActivitySubscription {
            read,
            _write: write,
        })
    }

    /// List every Host supervised by this daemon through the shared daemon socket.
    pub async fn list_hosts(&self) -> anyhow::Result<Vec<HostStatus>> {
        match self.call(&Request::HostsList).await? {
            ResponseOk::Hosts(hosts) => Ok(hosts),
            other => bail!("unexpected hosts list response: {other:?}"),
        }
    }

    /// Best-effort map of `program_id -> handle name`, for rendering program names in
    /// listings that only carry the id. Empty on any error (the caller falls back to the
    /// short id).
    pub async fn program_name_map(
        &self,
        host: &HostName,
    ) -> std::collections::HashMap<ProgramHash, String> {
        let mut map = std::collections::HashMap::new();
        if let Ok(ResponseOk::ProgramList(list)) =
            self.call_host(host, &HostRequest::ProgramList).await
        {
            for s in list {
                map.insert(s.program_hash, s.name);
            }
        }
        map
    }
}

/// A live event subscription: the connection stays open, the write half held so the
/// daemon keeps streaming, until this is dropped.
#[allow(missing_debug_implementations)]
pub struct Subscription {
    read: BufReader<OwnedReadHalf>,
    _write: OwnedWriteHalf,
}

impl Subscription {
    /// The next event frame, or `None` when the daemon closed the stream.
    pub async fn next(&mut self) -> anyhow::Result<Option<EventFrame>> {
        Ok(frame::read_frame::<_, EventFrame>(&mut self.read).await?)
    }
}

/// A live daemon-wide MCP activity subscription.
#[allow(missing_debug_implementations)]
pub struct ActivitySubscription {
    read: BufReader<OwnedReadHalf>,
    _write: OwnedWriteHalf,
}

impl ActivitySubscription {
    /// The next activity frame, or `None` when the daemon closed the stream.
    pub async fn next(&mut self) -> anyhow::Result<Option<ActivityFrame>> {
        Ok(frame::read_frame::<_, ActivityFrame>(&mut self.read).await?)
    }
}
