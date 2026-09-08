//! Bounded per-Host inspection polling and interactive page selection.

use std::collections::BTreeMap;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt as _};
use tokio::sync::watch;

use super::{DaemonClient, ExecId, HostName, TuiHandle, refresh_tui_inspection};

const POLL_INTERVAL: Duration = Duration::from_millis(300);

struct Poller {
    host: HostName,
    client: DaemonClient,
    exec_id: ExecId,
    page: watch::Receiver<Option<u64>>,
}

impl Poller {
    async fn run(mut self, tui: TuiHandle) {
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + POLL_INTERVAL, POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                changed = self.page.changed() => {
                    if changed.is_err() { return; }
                }
                _ = interval.tick() => {}
            }
            let from = *self.page.borrow_and_update();
            if refresh_tui_inspection(&tui, &self.host, &self.client, self.exec_id, from)
                .await
                .is_err()
            {
                tracing::warn!(host = %self.host, "TUI inspection refresh failed");
            }
        }
    }
}

pub(super) async fn observe(
    mut tui: TuiHandle,
    sources: Vec<(HostName, DaemonClient, ExecId)>,
) -> anyhow::Result<()> {
    let mut pages = BTreeMap::new();
    let mut pollers = FuturesUnordered::new();
    for (host, client, exec_id) in sources {
        let (sender, page) = watch::channel(None);
        pages.insert(host.clone(), sender);
        pollers.push(
            Poller {
                host,
                client,
                exec_id,
                page,
            }
            .run(tui.clone()),
        );
    }
    loop {
        tokio::select! {
            request = tui.changed_private_page() => {
                let Some(request) = request else { return Ok(()); };
                if let Some(page) = pages.get(&request.host) {
                    page.send_replace(request.from);
                }
            }
            _ = pollers.next() => anyhow::bail!("TUI inspection poller stopped unexpectedly"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{PrivatePageRequest, RunUpdate};
    use anyhow::{anyhow, bail};
    use arena0_client::api::{
        ExecStatus, ExecStatusState, ExecutionInspection, HostRequest, Request, Response,
        ResponseOk,
        frame::{read_frame, write_frame},
    };
    use arena0_client::protocol::ProgramHash;
    use std::cell::Cell;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncRead, AsyncReadExt as _, BufReader};
    use tokio::net::UnixListener;

    async fn read_routed_request<R>(read: &mut R) -> (String, HostRequest)
    where
        R: AsyncRead + Unpin,
    {
        let envelope = read_frame::<_, Request>(read)
            .await
            .expect("read routed Host request")
            .expect("Host request frame");
        let Request::Host { host, request } = envelope else {
            panic!("request was not routed through host.call");
        };
        (host, request)
    }

    #[tokio::test]
    async fn stalled_host_does_not_block_other_hosts_pages_and_cancellation_closes_requests() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let host_a = HostName::for_local_index(0);
        let host_b = HostName::for_local_index(1);
        let exec_a = ExecId([1; 32]);
        let exec_b = ExecId([2; 32]);
        let client = DaemonClient::new(socket);
        let (tui, mut updates, pages) = TuiHandle::test_channel();
        let observer = tokio::spawn(observe(
            tui,
            vec![
                (host_a.clone(), client.clone(), exec_a),
                (host_b.clone(), client, exec_b),
            ],
        ));
        let milestone = Cell::new("waiting for the first Host request");
        let started = std::time::Instant::now();
        let scenario = tokio::time::timeout(Duration::from_secs(3), async {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, stalled_write) = stream.into_split();
            let mut stalled = BufReader::new(read);
            let (stalled_wire_host, stalled_request) = read_routed_request(&mut stalled).await;
            let (stalled_host, stalled_exec) = if stalled_wire_host == host_a.to_string() {
                (host_a.clone(), exec_a)
            } else if stalled_wire_host == host_b.to_string() {
                (host_b.clone(), exec_b)
            } else {
                panic!("unexpected stalled Host target {stalled_wire_host}");
            };
            assert!(matches!(
                stalled_request,
                HostRequest::ExecInspect { exec_id, private_from: None, .. }
                    if exec_id == stalled_exec
            ));
            milestone.set("first Host stalled; waiting for the second Host request");
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut responsive_write) = stream.into_split();
            let mut responsive = BufReader::new(read);
            let (responsive_wire_host, responsive_request) =
                read_routed_request(&mut responsive).await;
            let (responsive_host, responsive_exec) = if responsive_wire_host == host_a.to_string() {
                (host_a.clone(), exec_a)
            } else if responsive_wire_host == host_b.to_string() {
                (host_b.clone(), exec_b)
            } else {
                panic!("unexpected responsive Host target {responsive_wire_host}");
            };
            assert_ne!(responsive_host, stalled_host);
            assert!(matches!(
                responsive_request,
                HostRequest::ExecInspect { exec_id, private_from: None, .. }
                    if exec_id == responsive_exec
            ));
            pages.send_replace(Some(PrivatePageRequest {
                host: responsive_host.clone(),
                from: Some(4),
            }));
            let response = |exec_id, from| -> Response {
                Ok(ResponseOk::Inspection(ExecutionInspection {
                    status: ExecStatus {
                        exec_id,
                        negotiation_id: None,
                        program_id: ProgramHash([3; 32]),
                        state: ExecStatusState::Negotiating {
                            queue_position: None,
                        },
                    },
                    activation: None,
                    private_from: from,
                    private: Vec::new(),
                    private_total: 10,
                    private_next: None,
                }))
            };
            write_frame(&mut responsive_write, &response(responsive_exec, 0))
                .await
                .unwrap();
            milestone.set("second Host responded; waiting for its requested page");
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut responsive_write) = stream.into_split();
            let mut responsive = BufReader::new(read);
            let (wire_host, request) = read_routed_request(&mut responsive).await;
            assert_eq!(wire_host, responsive_host.to_string());
            assert!(matches!(
                request,
                HostRequest::ExecInspect { exec_id, private_from: Some(4), .. }
                    if exec_id == responsive_exec
            ));
            write_frame(&mut responsive_write, &response(responsive_exec, 4))
                .await
                .unwrap();
            while let Some(update) = updates.recv().await {
                if let RunUpdate::Inspection { host, inspection } = update
                    && host == responsive_host
                    && inspection.private_from == 4
                {
                    milestone.set("requested page published while first Host remains stalled");
                    break;
                }
            }
            milestone.set("requested page published; waiting for a second pending request");
            let (stream, _) = listener.accept().await.unwrap();
            let (read, pending_write) = stream.into_split();
            let mut pending = BufReader::new(read);
            let (wire_host, request) = read_routed_request(&mut pending).await;
            assert_eq!(wire_host, responsive_host.to_string());
            assert!(matches!(
                request,
                HostRequest::ExecInspect { exec_id, private_from: Some(4), .. }
                    if exec_id == responsive_exec
            ));
            milestone.set("one stalled and one pending Host request");
            ((stalled, stalled_write), (pending, pending_write))
        })
        .await;
        observer.abort();
        assert!(observer.await.unwrap_err().is_cancelled());
        let ((mut stalled, _stalled_write), (mut pending, _pending_write)) = scenario
            .unwrap_or_else(|_| {
                panic!(
                    "inspection deadline after {:?}; last milestone: {}",
                    started.elapsed(),
                    milestone.get()
                )
            });
        let mut stalled_byte = [0];
        let read = tokio::time::timeout(Duration::from_secs(1), stalled.read(&mut stalled_byte))
            .await
            .expect("cancelled observer left the stalled Host request open");
        assert_eq!(read.unwrap(), 0);
        let mut pending_byte = [0];
        let read = tokio::time::timeout(Duration::from_secs(1), pending.read(&mut pending_byte))
            .await
            .expect("cancelled observer left the pending Host request open");
        assert_eq!(read.unwrap(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn inspection_poller_waits_before_its_first_periodic_request() -> anyhow::Result<()> {
        let directory = tempfile::tempdir().expect("inspection scheduling directory");
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind inspection scheduling socket");
        let host = HostName::for_local_index(0);
        let exec_id = ExecId([7; 32]);
        let request_count = Arc::new(AtomicUsize::new(0));
        let (timestamps, mut timestamps_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(serve_scheduling_socket(
            listener,
            host.to_string(),
            exec_id,
            request_count.clone(),
            timestamps,
        ));
        let client = DaemonClient::new(socket);
        let (tui, mut updates, _pages) = TuiHandle::test_channel();
        refresh_tui_inspection(&tui, &host, &client, exec_id, None).await?;
        assert_eq!(request_count.load(Ordering::SeqCst), 1);
        let (initial_number, initial_request_at) = timestamps_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("read initial inspection request timestamp"))?;
        assert_eq!(initial_number, 1);
        assert!(matches!(
            updates.recv().await,
            Some(RunUpdate::Inspection { host: update_host, inspection })
                if update_host == host && inspection.private_from == 0
        ));
        let observer = tokio::spawn(observe(tui, vec![(host.clone(), client, exec_id)]));
        tokio::task::yield_now().await;
        assert_eq!(
            request_count.load(Ordering::SeqCst),
            1,
            "the poller must not issue an immediate periodic inspection"
        );
        let (periodic_number, periodic_request_at) = timestamps_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("read periodic inspection request timestamp"))?;
        assert_eq!(periodic_number, 2);
        assert_eq!(request_count.load(Ordering::SeqCst), 2);
        assert!(
            periodic_request_at >= initial_request_at + POLL_INTERVAL,
            "inspection poll started before a full delay after the caller's initial fetch: {:?}",
            periodic_request_at.saturating_duration_since(initial_request_at)
        );

        observer.abort();
        assert!(
            observer
                .await
                .expect_err("inspection observer should be aborted")
                .is_cancelled()
        );
        server.abort();
        assert!(
            server
                .await
                .expect_err("inspection scheduling server should be aborted")
                .is_cancelled()
        );
        Ok(())
    }

    async fn serve_scheduling_socket(
        listener: UnixListener,
        expected_host: String,
        expected_exec_id: ExecId,
        request_count: Arc<AtomicUsize>,
        timestamps: tokio::sync::mpsc::UnboundedSender<(usize, tokio::time::Instant)>,
    ) -> anyhow::Result<()> {
        loop {
            let (stream, _) = listener.accept().await?;
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            let (wire_host, request) = read_routed_request(&mut read).await;
            assert_eq!(wire_host, expected_host);
            let HostRequest::ExecInspect {
                exec_id,
                private_from,
                ..
            } = request
            else {
                bail!("inspection scheduling test received a non-inspection request")
            };
            assert_eq!(exec_id, expected_exec_id);
            let request_number = request_count.fetch_add(1, Ordering::SeqCst) + 1;
            timestamps
                .send((request_number, tokio::time::Instant::now()))
                .map_err(|_| anyhow!("inspection timestamp receiver closed"))?;
            let response: Response = Ok(ResponseOk::Inspection(ExecutionInspection {
                status: ExecStatus {
                    exec_id,
                    negotiation_id: None,
                    program_id: ProgramHash([7; 32]),
                    state: ExecStatusState::Negotiating {
                        queue_position: None,
                    },
                },
                activation: None,
                private_from: private_from.unwrap_or(0),
                private: Vec::new(),
                private_total: 0,
                private_next: None,
            }));
            write_frame(&mut write, &response).await?;
        }
    }
}
