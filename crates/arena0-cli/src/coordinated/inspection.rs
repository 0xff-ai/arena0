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
        let mut interval = tokio::time::interval(POLL_INTERVAL);
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
    use arena0_client::api::{
        ExecStatus, ExecStatusState, ExecutionInspection, Request, Response, ResponseOk,
        frame::{read_frame, write_frame},
    };
    use arena0_client::protocol::ProgramHash;
    use std::cell::Cell;
    use tokio::io::{AsyncReadExt as _, BufReader};
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn stalled_host_does_not_block_other_hosts_pages_and_cancellation_closes_requests() {
        let directory = tempfile::tempdir().unwrap();
        let socket_a = directory.path().join("a.sock");
        let socket_b = directory.path().join("b.sock");
        let listener_a = UnixListener::bind(&socket_a).unwrap();
        let listener_b = UnixListener::bind(&socket_b).unwrap();
        let host_a = HostName::for_local_index(0);
        let host_b = HostName::for_local_index(1);
        let exec_a = ExecId([1; 32]);
        let exec_b = ExecId([2; 32]);
        let (tui, mut updates, pages) = TuiHandle::test_channel();
        let observer = tokio::spawn(observe(
            tui,
            vec![
                (host_a, DaemonClient::new(socket_a), exec_a),
                (host_b.clone(), DaemonClient::new(socket_b), exec_b),
            ],
        ));
        let milestone = Cell::new("waiting for the first Host request");
        let started = std::time::Instant::now();
        let scenario = tokio::time::timeout(Duration::from_secs(3), async {
            let (stream_a, _) = listener_a.accept().await.unwrap();
            let mut stalled = BufReader::new(stream_a);
            assert!(matches!(
                read_frame::<_, Request>(&mut stalled).await.unwrap(),
                Some(Request::ExecInspect { exec_id, private_from: None, .. }) if exec_id == exec_a
            ));
            milestone.set("first Host stalled; waiting for the second Host request");
            let (stream_b, _) = listener_b.accept().await.unwrap();
            let (read, mut write) = stream_b.into_split();
            let mut read = BufReader::new(read);
            assert!(matches!(
                read_frame::<_, Request>(&mut read).await.unwrap(),
                Some(Request::ExecInspect { exec_id, private_from: None, .. }) if exec_id == exec_b
            ));
            pages.send_replace(Some(PrivatePageRequest { host: host_b.clone(), from: Some(4) }));
            let response = |from| -> Response {
                Ok(ResponseOk::Inspection(ExecutionInspection {
                    status: ExecStatus {
                        exec_id: exec_b,
                        negotiation_id: None,
                        program_id: ProgramHash([3; 32]),
                        state: ExecStatusState::Negotiating { queue_position: None },
                    },
                    activation: None,
                    private_from: from,
                    private: Vec::new(),
                    private_total: 10,
                    private_next: None,
                }))
            };
            write_frame(&mut write, &response(0)).await.unwrap();
            milestone.set("second Host responded; waiting for its requested page");
            let (stream_b, _) = listener_b.accept().await.unwrap();
            let (read, mut write) = stream_b.into_split();
            let mut read = BufReader::new(read);
            assert!(matches!(
                read_frame::<_, Request>(&mut read).await.unwrap(),
                Some(Request::ExecInspect { exec_id, private_from: Some(4), .. }) if exec_id == exec_b
            ));
            write_frame(&mut write, &response(4)).await.unwrap();
            while let Some(update) = updates.recv().await {
                if let RunUpdate::Inspection { host, inspection } = update
                    && host == host_b && inspection.private_from == 4
                {
                    milestone.set("requested page published while first Host remains stalled");
                    return stalled;
                }
            }
            panic!("inspection update channel closed");
        }).await;
        observer.abort();
        assert!(observer.await.unwrap_err().is_cancelled());
        let mut stalled = scenario.unwrap_or_else(|_| {
            panic!(
                "inspection deadline after {:?}; last milestone: {}",
                started.elapsed(),
                milestone.get()
            )
        });
        let mut byte = [0];
        let read = tokio::time::timeout(Duration::from_secs(1), stalled.read(&mut byte))
            .await
            .expect("cancelled observer left a Host request open");
        assert_eq!(read.unwrap(), 0);
    }
}
