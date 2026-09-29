//! The roster of Hosts and the daemon-wide activity feed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arena0_api::{ActivityData, HostStatus};
use arena0_client::proto::DaemonClient;
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::task::{AbortHandle, JoinSet};

use super::host::Worker;
use super::{Input, project};
use crate::model::HostRow;

const ROSTER_POLL: Duration = Duration::from_secs(10);
const RECONNECT: Duration = Duration::from_secs(1);
/// Started tool calls remembered so a `finished` frame can name its tool.
const TOOL_MEMORY: usize = 1024;

pub(super) enum Control {
    /// Re-read the roster now and answer with every Host row.
    Sync(oneshot::Sender<anyhow::Result<Vec<HostRow>>>),
    Refresh(String),
    Stop,
}

struct WorkerHandle {
    refresh: Arc<Notify>,
    abort: AbortHandle,
}

/// Owns the Host workers and the activity feed; dropping it aborts them all.
pub(super) struct Supervisor {
    client: DaemonClient,
    owner: mpsc::Sender<Input>,
    tasks: JoinSet<()>,
    workers: HashMap<String, WorkerHandle>,
}

impl Supervisor {
    pub(super) fn new(client: DaemonClient, owner: mpsc::Sender<Input>) -> Self {
        let mut tasks = JoinSet::new();
        tasks.spawn(activity_feed(client.clone(), owner.clone()));
        Self {
            client,
            owner,
            tasks,
            workers: HashMap::new(),
        }
    }

    /// Read the roster, start workers for new Hosts, stop workers of Hosts
    /// that are gone, and hand the roster to the owner. Returns one receiver
    /// per new worker that resolves when its first load has finished.
    pub(super) async fn sync(
        &mut self,
        reply: Option<oneshot::Sender<anyhow::Result<Vec<HostRow>>>>,
    ) -> anyhow::Result<Vec<oneshot::Receiver<()>>> {
        let hosts = match self.client.list_hosts().await {
            Ok(hosts) => hosts,
            Err(error) => {
                if let Some(reply) = reply {
                    let _ = reply.send(Err(anyhow::anyhow!("{error:#}")));
                }
                return Err(error);
            }
        };
        let listed: Vec<&str> = hosts.iter().map(|status| status.host.id.as_str()).collect();
        self.workers.retain(|id, worker| {
            let keep = listed.contains(&id.as_str());
            if !keep {
                worker.abort.abort();
            }
            keep
        });
        let mut first_loads = Vec::new();
        for status in &hosts {
            let id = &status.host.id;
            if self.workers.contains_key(id) {
                continue;
            }
            let refresh = Arc::new(Notify::new());
            let (loaded, first_load) = oneshot::channel();
            let worker = Worker::new(
                self.client.clone(),
                id.clone(),
                self.owner.clone(),
                refresh.clone(),
                loaded,
            );
            let abort = self.tasks.spawn(worker.run());
            self.workers
                .insert(id.clone(), WorkerHandle { refresh, abort });
            first_loads.push(first_load);
        }
        let hosts: Vec<HostStatus> = hosts;
        self.owner
            .send(Input::Roster { hosts, reply })
            .await
            .map_err(|_| anyhow::anyhow!("replica stopped"))?;
        Ok(first_loads)
    }

    pub(super) async fn run(mut self, mut control: mpsc::Receiver<Control>) {
        let mut poll =
            tokio::time::interval_at(tokio::time::Instant::now() + ROSTER_POLL, ROSTER_POLL);
        loop {
            tokio::select! {
                _ = poll.tick() => {
                    if let Err(error) = self.sync(None).await {
                        tracing::debug!(error = %error, "host roster poll failed");
                    }
                }
                command = control.recv() => match command {
                    None | Some(Control::Stop) => return,
                    Some(Control::Sync(reply)) => {
                        if let Err(error) = self.sync(Some(reply)).await {
                            tracing::debug!(error = %error, "host roster sync failed");
                        }
                    }
                    Some(Control::Refresh(host)) => {
                        if let Some(worker) = self.workers.get(&host) {
                            worker.refresh.notify_one();
                        }
                    }
                },
            }
        }
    }
}

/// Follow the daemon's MCP tool activity, reconnecting every second. Frames
/// carry no arguments or results, only names, timings and result classes.
async fn activity_feed(client: DaemonClient, owner: mpsc::Sender<Input>) {
    let mut tools: HashMap<String, String> = HashMap::new();
    loop {
        if let Ok(mut feed) = client.subscribe_activity().await {
            while let Ok(Some(frame)) = feed.next().await {
                let tool = match &frame.data {
                    ActivityData::Started { call_id, tool, .. } => {
                        if tools.len() >= TOOL_MEMORY {
                            tools.clear();
                        }
                        tools.insert(call_id.clone(), tool.clone());
                        None
                    }
                    ActivityData::Finished { call_id, .. } => tools.remove(call_id),
                    ActivityData::Lagged { .. } => None,
                };
                let row = project::tool_activity(&frame, tool.as_deref());
                if owner.send(Input::Activity(row)).await.is_err() {
                    return;
                }
            }
        }
        tokio::time::sleep(RECONNECT).await;
    }
}
