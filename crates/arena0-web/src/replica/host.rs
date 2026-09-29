//! One Host's worker: reads the daemon in sequence and tells the owner what
//! it found.
//!
//! The worker is the only reader for its Host, so the results it sends are
//! ordered by the order it read them. It keeps cursors (steps already
//! loaded, whether a callout is open) to read incrementally, never rows.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use arena0_api::{
    ApiError, ApiErrorCode, AwaitState, EventData, EventFilter, EventFrame, ExecEndPhase,
    ExecStatus, HostRequest, NextEvent, ProgramDetail, Request, ResponseOk, SessionTerminal,
};
use arena0_client::proto::{DaemonClient, is_connect_error};
use arena0_program::ProgramHash;
use arena0_protocol::ExecId;
use futures::StreamExt as _;
use tokio::sync::{Notify, mpsc, oneshot};

use super::project::{self, StepContext};
use super::{Input, Slice, Update, now_ms};
use crate::model::{ActivationRow, ActivationState, CalloutRow};

const STATUS_POLL: Duration = Duration::from_secs(2);
const CATCH_UP: Duration = Duration::from_secs(10);
const RECONNECT: Duration = Duration::from_secs(1);
/// Concurrent daemon reads while loading or polling many executions.
const READ_CONCURRENCY: usize = 16;
/// `exec.next` and `exec.await` return at once in the states they are used
/// in. The bound only protects the worker's sequential loop from a callout
/// answered between the status read and the call, where `exec.next` would
/// block for the next callout.
const PROMPT_READ: Duration = Duration::from_secs(5);

/// The owner has stopped; the worker ends.
#[derive(Debug)]
struct OwnerGone;

impl std::fmt::Display for OwnerGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("replica owner stopped")
    }
}

impl std::error::Error for OwnerGone {}

/// Errors that end the session; others skip one read and keep going.
fn fatal(error: &anyhow::Error) -> bool {
    error.downcast_ref::<OwnerGone>().is_some() || is_connect_error(error)
}

/// What the worker has already read for one execution.
#[derive(Clone, Default)]
struct Cursor {
    steps_loaded: u64,
    last_certified_ms: Option<u64>,
    open_callout: Option<String>,
    terminal: bool,
    /// The end handshake is waiting for peers. No event reports its
    /// confirmations or its deadline, so the execution is polled until it
    /// leaves this phase, even after it is terminal.
    ending: bool,
    terminal_read: bool,
    activation: Option<ActivationRow>,
}

enum CalloutChange {
    Keep,
    Opened(Box<CalloutRow>),
    Closed,
}

struct ExecRead {
    exec_id: ExecId,
    row: crate::model::ExecutionRow,
    steps: Vec<crate::model::StepRow>,
    callout: CalloutChange,
    reason: Option<String>,
    cursor: Cursor,
}

type Programs = HashMap<ProgramHash, Arc<ProgramDetail>>;

struct Reader<'a> {
    client: &'a DaemonClient,
    host: &'a str,
    programs: &'a Programs,
}

impl Reader<'_> {
    /// One Host request; an `ApiError` stays in the error chain.
    async fn call(&self, request: HostRequest) -> anyhow::Result<ResponseOk> {
        call(self.client, self.host, request).await
    }

    /// Read one execution and everything derived from it since `cursor`.
    /// `None` when the execution does not exist (any more).
    async fn read_exec(
        &self,
        exec_id: ExecId,
        mut cursor: Cursor,
        inspect: bool,
    ) -> anyhow::Result<Option<ExecRead>> {
        let committed = cursor
            .activation
            .as_ref()
            .is_some_and(|activation| activation.state == ActivationState::Committed);
        let request = if inspect || !committed {
            HostRequest::ExecInspect {
                exec_id,
                events_from: None,
                events_limit: 1,
            }
        } else {
            HostRequest::ExecStatus { exec_id }
        };
        let status: ExecStatus = match self.call(request).await {
            Ok(ResponseOk::Inspection(inspection)) => {
                if let Some(activation) = &inspection.activation {
                    cursor.activation = Some(project::activation_row(activation));
                }
                inspection.status
            }
            Ok(ResponseOk::Status(status)) => status,
            Ok(other) => bail!("unexpected execution read response: {other:?}"),
            Err(error) if is_api(&error, ApiErrorCode::NotFound) => return Ok(None),
            Err(error) => return Err(error),
        };
        cursor.terminal = status.lifecycle().is_terminal();
        cursor.ending = status.end.phase == ExecEndPhase::Ending;
        let exec_text = exec_id.to_string();

        let mut steps = Vec::new();
        if let Some(session) = status.session()
            && session.step > cursor.steps_loaded
        {
            let trace = match self
                .call(HostRequest::ExecTrace {
                    exec_id,
                    from: cursor.steps_loaded,
                    to: session.step,
                })
                .await?
            {
                ResponseOk::Trace(trace) => trace,
                other => bail!("unexpected exec.trace response: {other:?}"),
            };
            let session_text = session.session_id.to_string();
            let schema = self
                .programs
                .get(&status.program_id)
                .and_then(|detail| detail.schema.messages.first())
                .map(|message| &message.borsh);
            let context = StepContext {
                host: self.host,
                exec_id: &exec_text,
                session_id: &session_text,
                participants: u16::try_from(session.participants).unwrap_or(u16::MAX),
                schema,
            };
            for step in &trace {
                steps.push(project::step_row(&context, step));
                cursor.steps_loaded = step.entry.step + 1;
                cursor.last_certified_ms = Some(step.certified_at_ms);
            }
        }

        let callout = match (status.pending_callout(), cursor.open_callout.clone()) {
            (Some(pending), open) if open.as_deref() != Some(&pending.pending_id.to_string()) => {
                match self.open_callout(exec_id, &status, &cursor).await? {
                    Some(row) => {
                        cursor.open_callout = Some(row.pending_id.clone());
                        CalloutChange::Opened(Box::new(row))
                    }
                    None => CalloutChange::Keep,
                }
            }
            (None, Some(_)) => {
                cursor.open_callout = None;
                CalloutChange::Closed
            }
            _ => CalloutChange::Keep,
        };

        let mut reason = None;
        if cursor.terminal && !cursor.terminal_read {
            let awaited = tokio::time::timeout(
                PROMPT_READ,
                self.call(HostRequest::ExecAwait {
                    exec_id,
                    until: AwaitState::Terminal,
                }),
            )
            .await;
            if let Ok(response) = awaited {
                match response? {
                    ResponseOk::Awaited { reason: found, .. } => reason = found,
                    other => bail!("unexpected exec.await response: {other:?}"),
                }
                cursor.terminal_read = true;
            }
        }

        let row = project::execution_row(self.host, &status, cursor.activation.as_ref());
        Ok(Some(ExecRead {
            exec_id,
            row,
            steps,
            callout,
            reason,
            cursor,
        }))
    }

    /// The pending callout's details, from `exec.next`, which returns at once
    /// while a callout is pending.
    async fn open_callout(
        &self,
        exec_id: ExecId,
        status: &ExecStatus,
        cursor: &Cursor,
    ) -> anyhow::Result<Option<CalloutRow>> {
        let Some(session) = status.session() else {
            return Ok(None);
        };
        let next =
            tokio::time::timeout(PROMPT_READ, self.call(HostRequest::ExecNext { exec_id })).await;
        let Ok(next) = next else { return Ok(None) };
        let ResponseOk::Next(NextEvent::Callout {
            pending_id,
            callout_index,
            name,
            prompt,
            schema,
            context,
        }) = next?
        else {
            return Ok(None);
        };
        let exec_text = exec_id.to_string();
        Ok(Some(CalloutRow {
            key: format!("{}/{exec_text}/{pending_id}", self.host),
            host: self.host.to_owned(),
            exec_id: exec_text,
            session_id: Some(session.session_id.to_string()),
            pending_id: pending_id.to_string(),
            callout_index,
            name,
            prompt,
            schema: schema.as_value().clone(),
            context,
            opened_ms: cursor.last_certified_ms.unwrap_or(status.updated_at_ms),
        }))
    }
}

async fn call(
    client: &DaemonClient,
    host: &str,
    request: HostRequest,
) -> anyhow::Result<ResponseOk> {
    let response = client
        .call_raw(&Request::Host {
            host: host.to_owned(),
            request,
        })
        .await?;
    response.map_err(anyhow::Error::new)
}

fn is_api(error: &anyhow::Error, code: ApiErrorCode) -> bool {
    error
        .downcast_ref::<ApiError>()
        .is_some_and(|error| error.code == code)
}

pub(super) struct Worker {
    client: DaemonClient,
    host: String,
    owner: mpsc::Sender<Input>,
    refresh: Arc<Notify>,
    first_load: Option<oneshot::Sender<()>>,
    programs: Programs,
    cursors: HashMap<ExecId, Cursor>,
    boot_id: Option<String>,
}

impl Worker {
    pub(super) fn new(
        client: DaemonClient,
        host: String,
        owner: mpsc::Sender<Input>,
        refresh: Arc<Notify>,
        first_load: oneshot::Sender<()>,
    ) -> Self {
        Self {
            client,
            host,
            owner,
            refresh,
            first_load: Some(first_load),
            programs: HashMap::new(),
            cursors: HashMap::new(),
            boot_id: None,
        }
    }

    async fn send(&self, update: Update) -> anyhow::Result<()> {
        self.owner
            .send(Input::Host {
                host: self.host.clone(),
                update,
            })
            .await
            .map_err(|_| anyhow::Error::new(OwnerGone))
    }

    /// Serve the Host until the owner stops: one session per event stream,
    /// reconnecting every second. The first session's load is not a gap;
    /// every later one is, because events were missed.
    pub(super) async fn run(mut self) {
        let mut gap = false;
        loop {
            let ended = self.session(gap).await;
            if let Some(first) = self.first_load.take() {
                let _ = first.send(());
            }
            if let Err(error) = ended {
                if error.downcast_ref::<OwnerGone>().is_some() {
                    return;
                }
                tracing::debug!(host = %self.host, "host session ended: {error:#}");
            }
            if self.send(Update::Online(false)).await.is_err() {
                return;
            }
            gap = true;
            tokio::time::sleep(RECONNECT).await;
        }
    }

    async fn session(&mut self, gap: bool) -> anyhow::Result<()> {
        // Subscribe before loading so no event between the two is lost; events
        // that the load already reflects are re-applied idempotently.
        let name = self
            .host
            .parse()
            .map_err(|_| anyhow!("invalid Host name"))?;
        let mut events = self.client.subscribe(&name, EventFilter::default()).await?;
        let started = events
            .next()
            .await?
            .context("event stream ended before host.started")?;
        let EventData::HostStarted { .. } = started.data else {
            bail!("event stream did not start with host.started");
        };
        self.boot_id = Some(started.boot_id.clone());
        self.send(Update::Activity(project::event_activity(
            &self.host, &started,
        )))
        .await?;
        self.load(gap).await?;
        self.send(Update::Online(true)).await?;
        if let Some(first) = self.first_load.take() {
            let _ = first.send(());
        }

        let refresh = self.refresh.clone();
        let start = tokio::time::Instant::now();
        let mut poll = tokio::time::interval_at(start + STATUS_POLL, STATUS_POLL);
        let mut catch_up = tokio::time::interval_at(start + CATCH_UP, CATCH_UP);
        loop {
            tokio::select! {
                frame = events.next() => {
                    let frame = frame?.context("event stream ended")?;
                    self.event(frame).await?;
                }
                _ = poll.tick() => self.poll().await?,
                _ = catch_up.tick() => self.catch_up().await?,
                () = refresh.notified() => self.catch_up().await?,
            }
        }
    }

    /// Run one step of session work: an error that does not mean the daemon
    /// or the owner is gone skips the step.
    fn lenient(&self, what: &str, result: anyhow::Result<()>) -> anyhow::Result<()> {
        match result {
            Err(error) if !fatal(&error) => {
                tracing::warn!(host = %self.host, "{what} failed: {error:#}");
                Ok(())
            }
            other => other,
        }
    }

    async fn load(&mut self, gap: bool) -> anyhow::Result<()> {
        let programs = self.load_programs().await?;
        let statuses = match call(&self.client, &self.host, HostRequest::ExecList).await? {
            ResponseOk::ExecList(statuses) => statuses,
            other => bail!("unexpected exec.list response: {other:?}"),
        };
        let reader = Reader {
            client: &self.client,
            host: &self.host,
            programs: &self.programs,
        };
        let ids: Vec<ExecId> = statuses.iter().map(|entry| entry.status.exec_id).collect();
        drop(statuses);
        let reads: Vec<_> = futures::stream::iter(ids)
            .map(|exec_id| reader.read_exec(exec_id, Cursor::default(), true))
            .buffered(READ_CONCURRENCY)
            .collect()
            .await;
        let mut slice = Slice {
            programs,
            ..Slice::default()
        };
        let mut cursors = HashMap::new();
        for read in reads {
            let read = match read {
                Ok(Some(read)) => read,
                Ok(None) => continue,
                Err(error) if !fatal(&error) => {
                    tracing::warn!(host = %self.host, "execution load failed: {error:#}");
                    continue;
                }
                Err(error) => return Err(error),
            };
            slice.steps.extend(read.steps);
            if let CalloutChange::Opened(row) = read.callout {
                slice.callouts.push(*row);
            }
            if let Some(reason) = read.reason {
                slice.reasons.push((read.row.exec_id.clone(), reason));
            }
            cursors.insert(read.exec_id, read.cursor);
            slice.executions.push(read.row);
        }
        slice.receipts = self.read_receipts().await?;
        slice.blobs = self.read_blobs().await?;
        self.cursors = cursors;
        self.send(Update::Loaded {
            gap,
            slice: Box::new(slice),
        })
        .await
    }

    async fn load_programs(&mut self) -> anyhow::Result<Vec<crate::model::ProgramRow>> {
        let summaries = match call(&self.client, &self.host, HostRequest::ProgramList).await? {
            ResponseOk::ProgramList(summaries) => summaries,
            other => bail!("unexpected program.list response: {other:?}"),
        };
        let missing: Vec<ProgramHash> = summaries
            .iter()
            .map(|summary| summary.program_hash)
            .filter(|hash| !self.programs.contains_key(hash))
            .collect();
        let client = &self.client;
        let host = &self.host;
        let fetched: Vec<_> = futures::stream::iter(missing)
            .map(|hash| async move {
                let response = call(
                    client,
                    host,
                    HostRequest::ProgramGet {
                        program: hash.to_string(),
                    },
                )
                .await;
                (hash, response)
            })
            .buffered(READ_CONCURRENCY)
            .collect()
            .await;
        for (hash, response) in fetched {
            match response {
                Ok(ResponseOk::Program(detail)) => {
                    self.programs.insert(hash, Arc::new(*detail));
                }
                Ok(other) => bail!("unexpected program.get response: {other:?}"),
                Err(error) if !fatal(&error) => {
                    tracing::warn!(host = %self.host, "program read failed: {error:#}");
                }
                Err(error) => return Err(error),
            }
        }
        Ok(summaries
            .iter()
            .filter_map(|summary| self.programs.get(&summary.program_hash))
            .map(|detail| project::program_row(detail))
            .collect())
    }

    async fn read_receipts(&self) -> anyhow::Result<Vec<crate::model::ReceiptRow>> {
        match call(&self.client, &self.host, HostRequest::ReceiptList).await? {
            ResponseOk::ReceiptList(entries) => Ok(entries
                .iter()
                .map(|entry| project::receipt_row(&self.host, entry))
                .collect()),
            other => bail!("unexpected receipt.list response: {other:?}"),
        }
    }

    async fn read_blobs(&self) -> anyhow::Result<Vec<crate::model::BlobRow>> {
        match call(&self.client, &self.host, HostRequest::BlobList).await? {
            ResponseOk::BlobList(entries) => Ok(entries
                .iter()
                .map(|entry| project::blob_row(&self.host, entry))
                .collect()),
            other => bail!("unexpected blob.list response: {other:?}"),
        }
    }

    /// Send what a read found: steps first, so an execution row never claims
    /// a step the replica does not hold yet.
    async fn publish(&mut self, read: ExecRead) -> anyhow::Result<()> {
        let exec_text = read.row.exec_id.clone();
        if !read.steps.is_empty() {
            self.send(Update::Steps(read.steps)).await?;
        }
        match read.callout {
            CalloutChange::Keep => {}
            CalloutChange::Opened(row) => self.send(Update::CalloutOpened(row)).await?,
            CalloutChange::Closed => {
                self.send(Update::CalloutClosed {
                    exec_id: exec_text.clone(),
                    pending_id: None,
                })
                .await?;
            }
        }
        if read.reason.is_some() {
            self.send(Update::Detail {
                exec_id: exec_text,
                reason: read.reason,
                outcome: None,
            })
            .await?;
        }
        self.cursors.insert(read.exec_id, read.cursor);
        self.send(Update::Exec(Box::new(read.row))).await
    }

    /// Re-read one execution (with its activation facts when `inspect`).
    async fn refresh_exec(&mut self, exec_id: ExecId, inspect: bool) -> anyhow::Result<()> {
        let cursor = self.cursors.get(&exec_id).cloned().unwrap_or_default();
        let reader = Reader {
            client: &self.client,
            host: &self.host,
            programs: &self.programs,
        };
        let read = reader.read_exec(exec_id, cursor, inspect).await?;
        match read {
            Some(read) => self.publish(read).await,
            None => Ok(()),
        }
    }

    async fn refresh_receipts(&self) -> anyhow::Result<()> {
        let rows = self.read_receipts().await?;
        self.send(Update::Receipts(rows)).await
    }

    /// Every non-terminal or still-ending execution's status, so a missed
    /// event heals within two seconds.
    async fn poll(&mut self) -> anyhow::Result<()> {
        let live: Vec<(ExecId, Cursor)> = self
            .cursors
            .iter()
            .filter(|(_, cursor)| !cursor.terminal || cursor.ending)
            .map(|(id, cursor)| (*id, cursor.clone()))
            .collect();
        let reader = Reader {
            client: &self.client,
            host: &self.host,
            programs: &self.programs,
        };
        let reads: Vec<_> = futures::stream::iter(live)
            .map(|(exec_id, cursor)| {
                let was_terminal = cursor.terminal;
                let reader = &reader;
                async move {
                    let read = reader.read_exec(exec_id, cursor, false).await;
                    (was_terminal, read)
                }
            })
            .buffered(READ_CONCURRENCY)
            .collect()
            .await;
        let mut ended = false;
        for (was_terminal, read) in reads {
            match read {
                Ok(Some(read)) => {
                    ended |= read.cursor.terminal && !was_terminal;
                    self.publish(read).await?;
                }
                Ok(None) => {}
                Err(error) => self.lenient("status poll", Err(error))?,
            }
        }
        if ended {
            let result = self.refresh_receipts().await;
            self.lenient("receipt read", result)?;
        }
        Ok(())
    }

    /// Catalogs and executions that events do not announce.
    async fn catch_up(&mut self) -> anyhow::Result<()> {
        let result = async {
            let programs = self.load_programs().await?;
            self.send(Update::Programs(programs)).await?;
            self.refresh_receipts().await?;
            let blobs = self.read_blobs().await?;
            self.send(Update::Blobs(blobs)).await?;
            let statuses = match call(&self.client, &self.host, HostRequest::ExecList).await? {
                ResponseOk::ExecList(statuses) => statuses,
                other => bail!("unexpected exec.list response: {other:?}"),
            };
            for status in statuses {
                let status = status.status;
                if !self.cursors.contains_key(&status.exec_id) {
                    self.refresh_exec(status.exec_id, true).await?;
                }
            }
            anyhow::Ok(())
        }
        .await;
        self.lenient("catch-up", result)
    }

    async fn event(&mut self, frame: EventFrame) -> anyhow::Result<()> {
        self.send(Update::Activity(project::event_activity(
            &self.host, &frame,
        )))
        .await?;
        let result = self.apply_event(&frame).await;
        self.lenient("event handling", result)
    }

    async fn apply_event(&mut self, frame: &EventFrame) -> anyhow::Result<()> {
        let exec_id = frame.exec_id;
        if let Some(exec_id) = exec_id
            && let Some(mark) = project::negotiation_mark(&frame.data, frame.ts)
        {
            self.send(Update::Mark {
                exec_id: exec_id.to_string(),
                mark,
            })
            .await?;
        }
        match &frame.data {
            EventData::HostStarted { .. } => {
                let changed = self
                    .boot_id
                    .as_deref()
                    .is_some_and(|known| known != frame.boot_id);
                self.boot_id = Some(frame.boot_id.clone());
                self.send(Update::Online(true)).await?;
                if changed {
                    self.load(true).await?;
                }
            }
            EventData::HostStopped { .. } => self.send(Update::Online(false)).await?,
            EventData::OfferSeen {
                program_id,
                negotiation_id,
                creator,
                offer_seq,
            } => {
                self.send(Update::Offer {
                    program: program_id.to_string(),
                    negotiation_id: negotiation_id.to_string(),
                    creator: creator.to_string(),
                    offer_seq: *offer_seq,
                    at_ms: frame.ts,
                })
                .await?;
            }
            EventData::Created { program_id, .. } => {
                if !self.programs.contains_key(program_id) {
                    let programs = self.load_programs().await?;
                    self.send(Update::Programs(programs)).await?;
                }
                if let Some(exec_id) = exec_id {
                    self.refresh_exec(exec_id, true).await?;
                }
            }
            EventData::NegotiationPrepared { .. }
            | EventData::NegotiationCommitted { .. }
            | EventData::NegotiationResumed { .. } => {
                if let Some(exec_id) = exec_id {
                    self.refresh_exec(exec_id, true).await?;
                }
            }
            EventData::NegotiationStarted { .. }
            | EventData::NegotiationOfferAccepted { .. }
            | EventData::NegotiationTicketAccepted { .. }
            | EventData::NegotiationPeers { .. }
            | EventData::NegotiationRetried { .. }
            | EventData::NegotiationRejoined {}
            | EventData::NegotiationTimedOut { .. } => {}
            EventData::SessionStarted { .. } | EventData::SessionStep { .. } => {
                if let Some(exec_id) = exec_id {
                    self.refresh_exec(exec_id, false).await?;
                }
            }
            EventData::SessionCallout {
                pending_id,
                callout_index,
                name,
                prompt,
                schema,
                context,
            } => {
                if let (Some(exec_id), Some(session_id)) = (exec_id, frame.session_id) {
                    let exec_text = exec_id.to_string();
                    let row = CalloutRow {
                        key: format!("{}/{exec_text}/{pending_id}", self.host),
                        host: self.host.clone(),
                        exec_id: exec_text,
                        session_id: Some(session_id.to_string()),
                        pending_id: pending_id.to_string(),
                        callout_index: *callout_index,
                        name: name.clone(),
                        prompt: prompt.clone(),
                        schema: schema.as_value().clone(),
                        context: context.clone(),
                        opened_ms: frame.ts,
                    };
                    self.cursors.entry(exec_id).or_default().open_callout =
                        Some(pending_id.to_string());
                    self.send(Update::CalloutOpened(Box::new(row))).await?;
                }
            }
            EventData::SessionCalloutAnswered { pending_id } => {
                if let Some(exec_id) = exec_id {
                    let cursor = self.cursors.entry(exec_id).or_default();
                    if cursor.open_callout.as_deref() == Some(&pending_id.to_string()) {
                        cursor.open_callout = None;
                    }
                    self.send(Update::CalloutClosed {
                        exec_id: exec_id.to_string(),
                        pending_id: Some(pending_id.to_string()),
                    })
                    .await?;
                    self.refresh_exec(exec_id, false).await?;
                }
            }
            EventData::SessionEnded { terminal } => {
                if let Some(exec_id) = exec_id {
                    let (reason, outcome) = match terminal {
                        SessionTerminal::Completed { outcome } => (None, outcome.clone()),
                        SessionTerminal::Aborted { reason, .. } => (Some(reason.clone()), None),
                    };
                    self.send(Update::Detail {
                        exec_id: exec_id.to_string(),
                        reason,
                        outcome,
                    })
                    .await?;
                    self.refresh_exec(exec_id, false).await?;
                    self.refresh_receipts().await?;
                }
            }
            EventData::SessionEndProgress { .. } => {
                if let Some(exec_id) = exec_id {
                    self.refresh_exec(exec_id, false).await?;
                }
            }
            EventData::Terminated { reason, .. } => {
                if let Some(exec_id) = exec_id {
                    self.send(Update::Detail {
                        exec_id: exec_id.to_string(),
                        reason: Some(reason.clone()),
                        outcome: None,
                    })
                    .await?;
                    self.refresh_exec(exec_id, false).await?;
                    self.refresh_receipts().await?;
                }
            }
            EventData::Lagged { .. } => {
                self.send(Update::Gap { at_ms: now_ms() }).await?;
                self.load(true).await?;
            }
        }
        Ok(())
    }
}
