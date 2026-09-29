//! The replica: the gateway's in-memory copy of daemon facts.
//!
//! Ownership. One owner task holds every row and is the only writer. Each
//! Host has a worker task that reads the daemon strictly in sequence (its
//! event stream, a 2 s status poll, catch-up reads) and sends the results to
//! the owner in the order it produced them, so a stale read can never
//! overwrite a fresher one. Workers hold only read cursors (how many steps
//! they already loaded), never rows. Facts only the owner can hold
//! (negotiation marks, terminal details, program holders, offers) live in the
//! owner.
//!
//! Every change goes through [`Table`], which upserts a row only when it
//! differs from the stored one. Changes are serialized once and broadcast to
//! every socket; a socket that lags asks for a fresh snapshot.

mod host;
mod project;
mod supervisor;
mod table;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use arena0_api::HostStatus;
use arena0_client::proto::DaemonClient;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::model::{
    ActivityRow, BlobRow, CalloutRow, ExecutionRow, HostRow, Lifecycle, NegotiationMark, OfferRow,
    ProgramRow, ReceiptRow, StepRow, TerminalKind, TerminalRow,
};
use crate::protocol::{RowBatch, RowOp, ServerFrame};
use table::Table;

/// Frames of one change kept for lagging sockets: broadcast capacity.
const BROADCAST_CAPACITY: usize = 1024;
/// The latest activity rows kept.
const ACTIVITY_LIMIT: usize = 1000;
/// Offers not seen for this long are deleted.
const OFFER_TTL_MS: u64 = 10 * 60 * 1000;
/// Negotiations that retry for a long time must not grow a row without bound.
const MARK_LIMIT: usize = 256;
const OFFER_SWEEP: Duration = Duration::from_secs(10);

/// What a socket needs to start: the nine reset frames, then live changes.
/// Both come from the same instant, so no change is missed or repeated.
#[derive(Debug)]
pub(crate) struct Subscription {
    /// Serialized `rows {reset: true}` frames in the fixed collection order.
    pub(crate) resets: Vec<String>,
    /// Serialized frames of each subsequent change.
    pub(crate) changes: broadcast::Receiver<Arc<Vec<String>>>,
}

/// Handle to the running replica.
#[derive(Debug, Clone)]
pub(crate) struct Replica {
    owner: mpsc::Sender<Input>,
    control: mpsc::Sender<supervisor::Control>,
}

impl Replica {
    /// Read the daemon's Hosts, start the workers, and wait until each Host's
    /// first load has finished so the first snapshot is complete. Fails when
    /// the daemon is unreachable.
    pub(crate) async fn start(client: DaemonClient) -> anyhow::Result<(Self, ReplicaTasks)> {
        let (owner_tx, owner_rx) = mpsc::channel(1024);
        let (control_tx, control_rx) = mpsc::channel(64);
        let owner = tokio::spawn(Owner::new().run(owner_rx));
        let mut supervisor = supervisor::Supervisor::new(client, owner_tx.clone());
        let first_loads = match supervisor.sync(None).await {
            Ok(first_loads) => first_loads,
            Err(error) => {
                owner.abort();
                return Err(error);
            }
        };
        for loaded in first_loads {
            let _ = loaded.await;
        }
        let supervisor = tokio::spawn(supervisor.run(control_rx));
        Ok((
            Self {
                owner: owner_tx,
                control: control_tx,
            },
            ReplicaTasks { owner, supervisor },
        ))
    }

    pub(crate) async fn subscribe(&self) -> Subscription {
        let (reply, receive) = oneshot::channel();
        self.owner
            .send(Input::Subscribe(reply))
            .await
            .expect("the owner outlives every handle");
        receive.await.expect("the owner answers every subscription")
    }

    /// Re-read one Host's catalogs and executions now. Called after a
    /// successful operation that changes something the event stream does not
    /// announce (programs, receipts, blobs).
    pub(crate) async fn refresh(&self, host: &str) {
        let _ = self
            .control
            .send(supervisor::Control::Refresh(host.to_owned()))
            .await;
    }

    /// Re-read the daemon's Host roster now and return every Host row.
    pub(crate) async fn sync_hosts(&self) -> anyhow::Result<Vec<HostRow>> {
        let (reply, receive) = oneshot::channel();
        self.control
            .send(supervisor::Control::Sync(reply))
            .await
            .map_err(|_| anyhow::anyhow!("replica stopped"))?;
        receive
            .await
            .map_err(|_| anyhow::anyhow!("replica stopped"))?
    }

    pub(crate) async fn stop(&self) {
        let _ = self.control.send(supervisor::Control::Stop).await;
        let _ = self.owner.send(Input::Stop).await;
    }
}

/// The replica's own tasks, joined by [`crate::UiServer::shutdown`].
#[derive(Debug)]
pub(crate) struct ReplicaTasks {
    owner: JoinHandle<()>,
    supervisor: JoinHandle<()>,
}

impl ReplicaTasks {
    pub(crate) async fn join(self) {
        let _ = self.supervisor.await;
        let _ = self.owner.await;
    }
}

/// Messages to the owner.
enum Input {
    Subscribe(oneshot::Sender<Subscription>),
    /// The daemon's current Host roster; answers with every Host row.
    Roster {
        hosts: Vec<HostStatus>,
        reply: Option<oneshot::Sender<anyhow::Result<Vec<HostRow>>>>,
    },
    Host {
        host: String,
        update: Update,
    },
    Activity(ActivityRow),
    Stop,
}

/// One result of a Host worker's reads, in the order it read them.
enum Update {
    Online(bool),
    /// `stream.lagged` was seen.
    Gap {
        at_ms: u64,
    },
    /// The whole Host as just read. After a gap the collections are sent as
    /// resets instead of upserts and deletes.
    Loaded {
        gap: bool,
        slice: Box<Slice>,
    },
    /// An execution row without `terminal` and `negotiation`.
    Exec(Box<ExecutionRow>),
    Steps(Vec<StepRow>),
    CalloutOpened(Box<CalloutRow>),
    CalloutClosed {
        exec_id: String,
        /// `None` closes every callout of the execution.
        pending_id: Option<String>,
    },
    /// Terminal details observed for an execution; `None` fields keep what is
    /// already known.
    Detail {
        exec_id: String,
        reason: Option<String>,
        outcome: Option<Value>,
    },
    Mark {
        exec_id: String,
        mark: NegotiationMark,
    },
    Receipts(Vec<ReceiptRow>),
    Blobs(Vec<BlobRow>),
    /// This Host's whole catalog.
    Programs(Vec<ProgramRow>),
    Offer {
        program: String,
        negotiation_id: String,
        creator: String,
        offer_seq: u64,
        at_ms: u64,
    },
    Activity(ActivityRow),
}

#[derive(Default)]
struct Slice {
    executions: Vec<ExecutionRow>,
    steps: Vec<StepRow>,
    callouts: Vec<CalloutRow>,
    receipts: Vec<ReceiptRow>,
    blobs: Vec<BlobRow>,
    programs: Vec<ProgramRow>,
    reasons: Vec<(String, String)>,
}

/// Facts about one execution that only the owner holds: they come from the
/// event stream and cannot be re-read.
#[derive(Default)]
struct Observed {
    negotiation: Vec<NegotiationMark>,
    reason: Option<String>,
    outcome: Option<Value>,
}

#[derive(Clone, Copy)]
struct HostMeta {
    online: bool,
    gaps: u32,
    last_gap_ms: Option<u64>,
}

/// Row operations gathered while applying one input.
#[derive(Default)]
struct Change {
    hosts: Vec<RowOp<HostRow>>,
    programs: Vec<RowOp<ProgramRow>>,
    executions: Vec<RowOp<ExecutionRow>>,
    steps: Vec<RowOp<StepRow>>,
    callouts: Vec<RowOp<CalloutRow>>,
    receipts: Vec<RowOp<ReceiptRow>>,
    offers: Vec<RowOp<OfferRow>>,
    blobs: Vec<RowOp<BlobRow>>,
    activity: Vec<RowOp<ActivityRow>>,
    /// Collections sent as resets: programs, executions, steps, callouts,
    /// receipts, blobs.
    reset: bool,
}

#[derive(Default)]
struct Owner {
    hosts: Table<HostRow>,
    programs: Table<ProgramRow>,
    executions: Table<ExecutionRow>,
    steps: Table<StepRow>,
    callouts: Table<CalloutRow>,
    receipts: Table<ReceiptRow>,
    offers: Table<OfferRow>,
    blobs: Table<BlobRow>,
    activity: Table<ActivityRow>,
    activity_order: VecDeque<String>,
    observed: HashMap<String, Observed>,
    roster: BTreeMap<String, HostStatus>,
    meta: HashMap<String, HostMeta>,
    program_base: BTreeMap<String, ProgramRow>,
    program_holders: BTreeMap<String, BTreeSet<String>>,
    /// Negotiations some local execution belongs to; their offers are gone.
    local_negotiations: HashSet<String>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

fn frame(reset: bool, batch: RowBatch) -> String {
    serde_json::to_string(&ServerFrame::Rows { reset, batch }).expect("rows serialize")
}

impl Owner {
    fn new() -> Self {
        Self::default()
    }

    async fn run(mut self, mut inputs: mpsc::Receiver<Input>) {
        let (changes, _) = broadcast::channel::<Arc<Vec<String>>>(BROADCAST_CAPACITY);
        let mut sweep = tokio::time::interval(OFFER_SWEEP);
        loop {
            let mut change = Change::default();
            tokio::select! {
                input = inputs.recv() => match input {
                    None | Some(Input::Stop) => return,
                    Some(Input::Subscribe(reply)) => {
                        let _ = reply.send(Subscription {
                            resets: self.frames(Change { reset: true, ..Change::default() }, true),
                            changes: changes.subscribe(),
                        });
                        continue;
                    }
                    Some(Input::Roster { hosts, reply }) => {
                        self.apply_roster(hosts, &mut change);
                        if let Some(reply) = reply {
                            let _ = reply.send(Ok(self.hosts.0.values().cloned().collect()));
                        }
                    }
                    Some(Input::Host { host, update }) => self.apply(&host, update, &mut change),
                    Some(Input::Activity(row)) => self.add_activity(row, &mut change),
                },
                _ = sweep.tick() => self.sweep_offers(&mut change),
            }
            let frames = self.frames(change, false);
            if !frames.is_empty() {
                // No receivers just means no socket is connected.
                let _ = changes.send(Arc::new(frames));
            }
        }
    }

    /// Serialize a change. With `all`, every collection is sent whole and in
    /// the fixed order, whether or not it is empty.
    fn frames(&self, change: Change, all: bool) -> Vec<String> {
        fn batch<T: Clone + PartialEq>(
            whole: bool,
            table: &Table<T>,
            ops: Vec<RowOp<T>>,
            wrap: fn(Vec<RowOp<T>>) -> RowBatch,
            out: &mut Vec<String>,
        ) {
            if whole {
                out.push(frame(true, wrap(table.snapshot())));
            } else if !ops.is_empty() {
                out.push(frame(false, wrap(ops)));
            }
        }
        let reset = change.reset;
        let mut out = Vec::new();
        batch(all, &self.hosts, change.hosts, RowBatch::Hosts, &mut out);
        batch(
            all || reset,
            &self.programs,
            change.programs,
            RowBatch::Programs,
            &mut out,
        );
        batch(
            all || reset,
            &self.executions,
            change.executions,
            RowBatch::Executions,
            &mut out,
        );
        batch(
            all || reset,
            &self.steps,
            change.steps,
            RowBatch::Steps,
            &mut out,
        );
        batch(
            all || reset,
            &self.callouts,
            change.callouts,
            RowBatch::Callouts,
            &mut out,
        );
        batch(
            all || reset,
            &self.receipts,
            change.receipts,
            RowBatch::Receipts,
            &mut out,
        );
        batch(all, &self.offers, change.offers, RowBatch::Offers, &mut out);
        batch(
            all || reset,
            &self.blobs,
            change.blobs,
            RowBatch::Blobs,
            &mut out,
        );
        batch(
            all,
            &self.activity,
            change.activity,
            RowBatch::Activity,
            &mut out,
        );
        out
    }

    fn apply_roster(&mut self, hosts: Vec<HostStatus>, change: &mut Change) {
        let listed: HashSet<String> = hosts.iter().map(|status| status.host.id.clone()).collect();
        for gone in self
            .roster
            .keys()
            .filter(|id| !listed.contains(*id))
            .cloned()
            .collect::<Vec<_>>()
        {
            self.remove_host(&gone, change);
        }
        for status in hosts {
            let id = status.host.id.clone();
            self.roster.insert(id.clone(), status);
            self.meta.entry(id.clone()).or_insert(HostMeta {
                online: true,
                gaps: 0,
                last_gap_ms: None,
            });
            self.upsert_host(&id, change);
        }
    }

    fn upsert_host(&mut self, id: &str, change: &mut Change) {
        let (Some(status), Some(meta)) = (self.roster.get(id), self.meta.get(id)) else {
            return;
        };
        let row = project::host_row(status, meta.online, meta.gaps, meta.last_gap_ms);
        self.hosts.upsert(id, row, &mut change.hosts);
    }

    fn remove_host(&mut self, id: &str, change: &mut Change) {
        let prefix = format!("{id}/");
        self.roster.remove(id);
        self.meta.remove(id);
        self.hosts.delete(id, &mut change.hosts);
        self.executions
            .delete_prefix(&prefix, &mut change.executions);
        self.steps.delete_prefix(&prefix, &mut change.steps);
        self.callouts.delete_prefix(&prefix, &mut change.callouts);
        self.receipts.delete_prefix(&prefix, &mut change.receipts);
        self.blobs.delete_prefix(&prefix, &mut change.blobs);
        self.observed.retain(|key, _| !key.starts_with(&prefix));
        self.set_programs(id, Vec::new(), change);
        for key in self.offers.0.keys().cloned().collect::<Vec<_>>() {
            let Some(row) = self.offers.0.get(&key) else {
                continue;
            };
            if row.seen_by.iter().any(|seen| seen == id) {
                let mut row = row.clone();
                row.seen_by.retain(|seen| seen != id);
                if row.seen_by.is_empty() {
                    self.offers.delete(&key, &mut change.offers);
                } else {
                    self.offers.upsert(&key, row, &mut change.offers);
                }
            }
        }
    }

    fn apply(&mut self, host: &str, update: Update, change: &mut Change) {
        let prefix = format!("{host}/");
        match update {
            Update::Online(online) => {
                if let Some(meta) = self.meta.get_mut(host) {
                    meta.online = online;
                }
                self.upsert_host(host, change);
            }
            Update::Gap { at_ms } => {
                if let Some(meta) = self.meta.get_mut(host) {
                    meta.gaps = meta.gaps.saturating_add(1);
                    meta.last_gap_ms = Some(at_ms);
                }
                self.upsert_host(host, change);
            }
            Update::Loaded { gap, slice } => {
                let slice = *slice;
                for (exec_id, reason) in slice.reasons {
                    self.observed
                        .entry(project::exec_key(host, &exec_id))
                        .or_default()
                        .reason = Some(reason);
                }
                let keep: HashSet<&str> = slice
                    .executions
                    .iter()
                    .map(|row| row.key.as_str())
                    .collect();
                for key in self.executions.keys_with_prefix(&prefix) {
                    if !keep.contains(key.as_str()) {
                        self.executions.delete(&key, &mut change.executions);
                        self.observed.remove(&key);
                    }
                }
                for row in slice.executions {
                    self.upsert_exec(row, change);
                }
                let steps = slice
                    .steps
                    .into_iter()
                    .map(|row| (row.key.clone(), row))
                    .collect();
                self.steps.replace_prefix(&prefix, steps, &mut change.steps);
                let callouts = slice
                    .callouts
                    .into_iter()
                    .map(|row| (row.key.clone(), row))
                    .collect();
                self.callouts
                    .replace_prefix(&prefix, callouts, &mut change.callouts);
                let receipts = slice
                    .receipts
                    .into_iter()
                    .map(|row| (row.key.clone(), row))
                    .collect();
                self.receipts
                    .replace_prefix(&prefix, receipts, &mut change.receipts);
                let blobs = slice
                    .blobs
                    .into_iter()
                    .map(|row| (row.key.clone(), row))
                    .collect();
                self.blobs.replace_prefix(&prefix, blobs, &mut change.blobs);
                self.set_programs(host, slice.programs, change);
                change.reset |= gap;
            }
            Update::Exec(row) => self.upsert_exec(*row, change),
            Update::Steps(rows) => {
                for row in rows {
                    let key = row.key.clone();
                    self.steps.upsert(&key, row, &mut change.steps);
                }
            }
            Update::CalloutOpened(row) => {
                let key = row.key.clone();
                self.callouts.upsert(&key, *row, &mut change.callouts);
            }
            Update::CalloutClosed {
                exec_id,
                pending_id,
            } => {
                let key = project::exec_key(host, &exec_id);
                match pending_id {
                    Some(pending) => {
                        self.callouts
                            .delete(&format!("{key}/{pending}"), &mut change.callouts);
                    }
                    None => self
                        .callouts
                        .delete_prefix(&format!("{key}/"), &mut change.callouts),
                }
            }
            Update::Detail {
                exec_id,
                reason,
                outcome,
            } => {
                let key = project::exec_key(host, &exec_id);
                let observed = self.observed.entry(key.clone()).or_default();
                if reason.is_some() {
                    observed.reason = reason;
                }
                if outcome.is_some() {
                    observed.outcome = outcome;
                }
                if let Some(row) = self.executions.0.get(&key).cloned() {
                    self.upsert_exec(row, change);
                }
            }
            Update::Mark { exec_id, mark } => {
                let key = project::exec_key(host, &exec_id);
                let observed = self.observed.entry(key.clone()).or_default();
                if observed.negotiation.len() == MARK_LIMIT {
                    observed.negotiation.remove(0);
                }
                observed.negotiation.push(mark);
                if let Some(row) = self.executions.0.get(&key).cloned() {
                    self.upsert_exec(row, change);
                }
            }
            Update::Receipts(rows) => {
                let rows = rows.into_iter().map(|row| (row.key.clone(), row)).collect();
                self.receipts
                    .replace_prefix(&prefix, rows, &mut change.receipts);
            }
            Update::Blobs(rows) => {
                let rows = rows.into_iter().map(|row| (row.key.clone(), row)).collect();
                self.blobs.replace_prefix(&prefix, rows, &mut change.blobs);
            }
            Update::Programs(rows) => self.set_programs(host, rows, change),
            Update::Offer {
                program,
                negotiation_id,
                creator,
                offer_seq,
                at_ms,
            } => {
                if self.local_negotiations.contains(&negotiation_id) {
                    return;
                }
                let mut row = self
                    .offers
                    .0
                    .get(&negotiation_id)
                    .cloned()
                    .unwrap_or(OfferRow {
                        negotiation_id: negotiation_id.clone(),
                        program,
                        creator,
                        offer_seq,
                        first_seen_ms: at_ms,
                        last_seen_ms: at_ms,
                        seen_by: Vec::new(),
                    });
                row.offer_seq = row.offer_seq.max(offer_seq);
                row.last_seen_ms = row.last_seen_ms.max(at_ms);
                if !row.seen_by.iter().any(|seen| seen == host) {
                    row.seen_by.push(host.to_owned());
                    row.seen_by.sort();
                }
                self.offers.upsert(&negotiation_id, row, &mut change.offers);
            }
            Update::Activity(row) => self.add_activity(row, change),
        }
    }

    /// Complete an execution row with what only the owner knows, and enforce
    /// that a terminal execution has no open callouts and no live offer.
    fn upsert_exec(&mut self, mut row: ExecutionRow, change: &mut Change) {
        let key = row.key.clone();
        let observed = self.observed.entry(key.clone()).or_default();
        row.negotiation.clone_from(&observed.negotiation);
        row.terminal = match row.lifecycle {
            Lifecycle::Completed => Some(TerminalKind::Completed),
            Lifecycle::Aborted => Some(TerminalKind::Aborted),
            Lifecycle::Failed => Some(TerminalKind::Failed),
            _ => None,
        }
        .map(|kind| TerminalRow {
            kind,
            reason: observed.reason.clone(),
            outcome: observed.outcome.clone(),
        });
        if let Some(negotiation) = &row.negotiation_id {
            self.local_negotiations.insert(negotiation.clone());
            self.offers.delete(negotiation, &mut change.offers);
        }
        if row.terminal.is_some() {
            self.callouts
                .delete_prefix(&format!("{key}/"), &mut change.callouts);
        }
        self.executions.upsert(&key, row, &mut change.executions);
    }

    /// Replace one Host's catalog, and rebuild the merged rows it touches.
    fn set_programs(&mut self, host: &str, rows: Vec<ProgramRow>, change: &mut Change) {
        let mut touched: BTreeSet<String> = BTreeSet::new();
        for (hash, holders) in &mut self.program_holders {
            if holders.remove(host) {
                touched.insert(hash.clone());
            }
        }
        for row in rows {
            touched.insert(row.hash.clone());
            self.program_holders
                .entry(row.hash.clone())
                .or_default()
                .insert(host.to_owned());
            self.program_base.insert(row.hash.clone(), row);
        }
        for hash in touched {
            let holders = self.program_holders.get(&hash).cloned().unwrap_or_default();
            if holders.is_empty() {
                self.program_holders.remove(&hash);
                self.program_base.remove(&hash);
                self.programs.delete(&hash, &mut change.programs);
            } else if let Some(base) = self.program_base.get(&hash) {
                let mut row = base.clone();
                row.hosts = holders.into_iter().collect();
                self.programs.upsert(&hash, row, &mut change.programs);
            }
        }
    }

    fn add_activity(&mut self, row: ActivityRow, change: &mut Change) {
        let key = row.key.clone();
        if !self.activity.0.contains_key(&key) {
            self.activity_order.push_back(key.clone());
        }
        self.activity.upsert(&key, row, &mut change.activity);
        while self.activity_order.len() > ACTIVITY_LIMIT {
            if let Some(oldest) = self.activity_order.pop_front() {
                self.activity.delete(&oldest, &mut change.activity);
            }
        }
    }

    fn sweep_offers(&mut self, change: &mut Change) {
        let now = now_ms();
        let expired: Vec<String> = self
            .offers
            .0
            .iter()
            .filter(|(_, row)| now.saturating_sub(row.last_seen_ms) > OFFER_TTL_MS)
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired {
            self.offers.delete(&key, &mut change.offers);
        }
    }
}
