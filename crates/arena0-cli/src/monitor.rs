//! Live daemon attachment for the unified execution observatory.
//!
//! This module owns only the effectful monitor shell.  [`crate::tui`] remains
//! the sole terminal loop and state owner: this side fetches bounded snapshots,
//! translates public daemon events, and executes an explicitly selected answer.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use arena0_client::api::{
    ActivityData, ActivityResult, ApiErrorCode, EventData, EventFilter, HostRequest, NextEvent,
    ResponseOk,
};
use arena0_client::proto::DaemonClient;
use arena0_client::protocol::{ColorDepth, ExecId, ProgramHash, TraceEntry, Viewport};
use arena0_home::HostName;
use clap::Args;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinSet;

use crate::tui::{
    MonitorAction, MonitorActivity, MonitorExecutionKey, MonitorHost, MonitorSubmission,
    MonitorUpdate, PRIVATE_INSPECTION_LIMIT, RunUpdate, TuiConfig, TuiDriver, TuiHandle, TuiHost,
    TuiSession,
};

const TRACE_LIMIT: u64 = 256;
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_ACTIVITY_CALLS: usize = 1024;

/// Fetch each immutable program schema once across all observed Hosts.
/// Failed requests remain retryable on the next execution refresh.
#[derive(Default)]
struct MessageSchemas {
    loaded: Mutex<BTreeSet<ProgramHash>>,
}

impl MessageSchemas {
    async fn load(
        &self,
        client: &DaemonClient,
        host: &HostName,
        handle: &TuiHandle,
        program_id: ProgramHash,
    ) -> anyhow::Result<()> {
        let mut loaded = self.loaded.lock().await;
        if loaded.contains(&program_id) {
            return Ok(());
        }
        let response = client
            .call_host(
                host,
                &HostRequest::ProgramGet {
                    program: program_id.to_string(),
                },
            )
            .await;
        let (schema, fetched) = match response {
            Ok(ResponseOk::Program(detail)) => (
                detail
                    .schema
                    .messages
                    .into_iter()
                    .next()
                    .map(|message| message.borsh)
                    .ok_or_else(|| "program has no message schema".to_owned()),
                true,
            ),
            Ok(_) => (Err("unexpected program.get response".to_owned()), false),
            Err(error) => (
                Err(format!("message schema could not be loaded: {error:#}")),
                false,
            ),
        };
        handle
            .update(RunUpdate::Monitor(MonitorUpdate::MessageSchema {
                program_id,
                schema,
            }))
            .await?;
        if fetched {
            loaded.insert(program_id);
        }
        Ok(())
    }
}

/// The effectful state for one monitored Host.  Connection-local sequence
/// tracking deliberately remains inside `run_connection`, so reconnects begin
/// a fresh event-stream sequence while the Host/client/TUI/schema ownership is
/// shared by all refresh operations.
struct HostMonitor {
    host: HostName,
    client: DaemonClient,
    handle: TuiHandle,
    schemas: Arc<MessageSchemas>,
}

/// Arguments for `arena0 monitor`.
#[derive(Debug, Clone, Args)]
pub(crate) struct MonitorArgs {
    /// Restrict the attachment to these daemon Host names.  With no filter,
    /// the roster returned by the selected daemon is observed.
    #[arg(long = "hosts", value_delimiter = ',', value_name = "NAME")]
    pub(crate) hosts: Vec<HostName>,
}

/// Attach the unified observatory to a live daemon roster.
pub(crate) async fn attach(entry_client: DaemonClient, args: MonitorArgs) -> anyhow::Result<()> {
    let roster = discover_hosts(&entry_client, &args.hosts).await?;
    let config = TuiConfig {
        program: "daemon executions".to_owned(),
        hosts: roster
            .iter()
            .map(|host| TuiHost {
                host: host.host.clone(),
                peer_id: host.peer_id,
                driver: TuiDriver::External,
            })
            .collect(),
        message_schema: None,
    };
    let (cancel, _cancelled) = watch::channel(None::<String>);
    let (mut session, mut actions) = TuiSession::start_monitor(config, cancel);
    let handle = session.handle();
    let mut workers = JoinSet::new();
    let schemas = Arc::new(MessageSchemas::default());
    let mut subscriptions = BTreeMap::new();
    let observe = async {
        handle
            .update(RunUpdate::Monitor(MonitorUpdate::Hosts {
                hosts: roster.clone(),
            }))
            .await?;
        for host in roster {
            let client = entry_client.clone();
            let handle = handle.clone();
            let name = host.host.clone();
            let peer = host.peer_id;
            let schemas = Arc::clone(&schemas);
            let task =
                workers.spawn(async move { host_worker(host, client, handle, schemas).await });
            subscriptions.insert(name, (peer, task));
        }
        workers.spawn(activity_worker(entry_client.clone(), handle.clone()));
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + REFRESH_INTERVAL,
            REFRESH_INTERVAL,
        );
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let roster = match discover_hosts(&entry_client, &args.hosts).await {
                        Ok(roster) => roster,
                        Err(error) => {
                            handle.update(RunUpdate::Monitor(MonitorUpdate::Gap {
                                host: None,
                                exec_id: None,
                                summary: format!("Host roster unavailable: {error:#}"),
                            })).await?;
                            continue;
                        }
                    };
                    subscriptions.retain(|name, (peer, task)| {
                        let present = roster.iter().any(|host| &host.host == name && host.peer_id == *peer);
                        if !present {
                            task.abort();
                        }
                        present
                    });
                    for host in &roster {
                        if !subscriptions.contains_key(&host.host) {
                            let client = entry_client.clone();
                            let handle = handle.clone();
                            let observed = host.clone();
                            let schemas = Arc::clone(&schemas);
                            let task = workers.spawn(async move { host_worker(observed, client, handle, schemas).await });
                            subscriptions.insert(host.host.clone(), (host.peer_id, task));
                        }
                    }
                    handle.update(RunUpdate::Monitor(MonitorUpdate::Hosts { hosts: roster })).await?;
                }
                action = actions.recv() => {
                    let Some(action) = action else { return Ok::<(), anyhow::Error>(()); };
                    submit_action(&entry_client, &handle, action).await?;
                }
                joined = workers.join_next() => {
                    let Some(joined) = joined else { return Ok(()); };
                    if joined.as_ref().is_err_and(|error| error.is_cancelled()) {
                        continue;
                    }
                    joined.context("monitor subscription task")??;
                }
            }
        }
    };
    let result = tokio::select! {
        result = observe => {
            let result = result.context("monitor transport");
            let _ = session.close().await;
            result
        }
        result = session.wait() => result.context("monitor TUI"),
    };
    workers.shutdown().await;
    result
}

async fn discover_hosts(
    entry_client: &DaemonClient,
    selected: &[HostName],
) -> anyhow::Result<Vec<MonitorHost>> {
    let infos = entry_client
        .list_hosts()
        .await
        .context("list daemon Hosts")?;
    let mut roster = Vec::with_capacity(infos.len());
    for info in infos {
        let host = info
            .host
            .id
            .parse::<HostName>()
            .with_context(|| format!("daemon returned invalid Host name {:?}", info.host.id))?;
        roster.push(MonitorHost {
            host,
            peer_id: info.host.peer_id,
        });
    }
    if selected.is_empty() {
        return Ok(roster);
    }
    if let Some(missing) = selected
        .iter()
        .find(|name| !roster.iter().any(|host| &host.host == *name))
    {
        bail!("Host '{missing}' is not present in the daemon roster");
    }
    roster.retain(|host| selected.contains(&host.host));
    Ok(roster)
}

async fn host_worker(
    host: MonitorHost,
    client: DaemonClient,
    handle: TuiHandle,
    schemas: Arc<MessageSchemas>,
) -> anyhow::Result<()> {
    let monitor = HostMonitor {
        host: host.host,
        client,
        handle,
        schemas,
    };
    loop {
        match monitor.run_connection().await {
            Ok(()) => return Ok(()),
            Err(error) => {
                let _ = monitor
                    .handle
                    .update(RunUpdate::Monitor(MonitorUpdate::Connection {
                        host: monitor.host.clone(),
                        message: format!("disconnected: {error:#}"),
                        stale: true,
                    }))
                    .await;
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }
}

impl HostMonitor {
    async fn run_connection(&self) -> anyhow::Result<()> {
        let mut subscription = self
            .client
            .subscribe(&self.host, EventFilter::default())
            .await
            .with_context(|| format!("subscribe to Host '{}' events", self.host))?;
        self.refresh_host().await?;
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + REFRESH_INTERVAL,
            REFRESH_INTERVAL,
        );
        let mut last_seq: Option<u64> = None;
        loop {
            tokio::select! {
                event = subscription.next() => {
                    let frame = event?.ok_or_else(|| anyhow!("Host event stream closed"))?;
                    let sequence_gap_after = (frame.seq != 0).then(|| {
                        last_seq.filter(|previous| frame.seq > previous.saturating_add(1))
                    }).flatten();
                    let lagged_skipped = match &frame.data {
                        EventData::Lagged { skipped } => Some(*skipped),
                        _ => None,
                    };
                    let recovery_reason = match (sequence_gap_after, lagged_skipped) {
                        (Some(previous), Some(skipped)) => Some(format!(
                            "event gap after #{previous}; Host event stream lagged by {skipped}"
                        )),
                        (Some(previous), None) => Some(format!("event gap after #{previous}")),
                        (None, Some(skipped)) => {
                            Some(format!("Host event stream lagged by {skipped}"))
                        }
                        (None, None) => None,
                    };
                    let refresh_error = if let Some(reason) = recovery_reason.as_deref() {
                        match self.refresh_host().await {
                            Ok(()) => {
                                self.handle.update(RunUpdate::Monitor(MonitorUpdate::Gap {
                                    host: Some(self.host.clone()),
                                    exec_id: None,
                                    summary: format!("{reason}; snapshot refreshed"),
                                })).await?;
                                None
                            }
                            Err(error) => {
                                self.handle.update(RunUpdate::Monitor(MonitorUpdate::Gap {
                                    host: Some(self.host.clone()),
                                    exec_id: None,
                                    summary: format!("{reason}; snapshot refresh unavailable: {error:#}"),
                                })).await?;
                                Some(error)
                            }
                        }
                    } else {
                        None
                    };
                    if frame.seq != 0 {
                        last_seq = Some(frame.seq);
                    }
                    self.observe_event(frame, recovery_reason.is_none()).await?;
                    if let Some(error) = refresh_error {
                        return Err(error);
                    }
                }
                _ = ticker.tick() => self.refresh_host().await?,
            }
        }
    }

    async fn refresh_host(&self) -> anyhow::Result<()> {
        let statuses = match self
            .client
            .call_host(&self.host, &HostRequest::ExecList)
            .await?
        {
            ResponseOk::ExecList(statuses) => statuses,
            other => bail!(
                "unexpected response to exec.list from '{}': {other:?}",
                self.host
            ),
        };
        for status in statuses {
            self.refresh_execution(status).await?;
        }
        Ok(())
    }

    /// Refresh one execution while retaining whatever parts of its last snapshot
    /// could not be fetched during a transient daemon/storage failure. The TUI
    /// applies stale updates as patches for exactly that reason.
    async fn refresh_execution(
        &self,
        status: arena0_client::api::ExecStatus,
    ) -> anyhow::Result<()> {
        self.schemas
            .load(&self.client, &self.host, &self.handle, status.program_id)
            .await?;
        let has_pending = status.pending_callout().is_some();
        let exec_id = status.exec_id;
        let key = MonitorExecutionKey {
            host: self.host.clone(),
            exec_id,
        };
        let inspection = self.fetch_inspection(exec_id).await;
        // ponytail: keep the one bounded view fetch at its only call site.
        let view = self
            .client
            .exec_view(
                &self.host,
                exec_id,
                Viewport {
                    width: self.handle.view_width(),
                    color: ColorDepth::Mono,
                },
            )
            .await;
        let trace = self.fetch_trace(exec_id, status.step()).await;
        let mut gap = None;
        let inspection = match inspection {
            Ok(value) => Some(value),
            Err(error) => {
                gap = Some(format!("inspection unavailable: {error:#}"));
                None
            }
        };
        let view = match view {
            Ok(value) => value,
            Err(error) => {
                gap.get_or_insert_with(|| format!("view unavailable: {error:#}"));
                None
            }
        };
        let trace = match trace {
            Ok(value) => value,
            Err(error) => {
                gap.get_or_insert_with(|| format!("trace unavailable: {error:#}"));
                Vec::new()
            }
        };
        self.handle
            .update(RunUpdate::Monitor(MonitorUpdate::Execution {
                key,
                status,
                inspection,
                view,
                trace,
                observed_at: now_epoch_seconds(),
                gap,
            }))
            .await?;
        if has_pending {
            let _ = self.fetch_pending_callout(exec_id).await;
        }
        Ok(())
    }

    async fn fetch_pending_callout(&self, exec_id: ExecId) -> anyhow::Result<()> {
        let response = tokio::time::timeout(
            Duration::from_millis(250),
            self.client
                .call_host(&self.host, &HostRequest::ExecNext { exec_id }),
        )
        .await
        .context("bounded pending callout lookup")??;
        let ResponseOk::Next(NextEvent::Callout {
            pending_id,
            callout_index,
            name,
            prompt,
            schema,
            context,
        }) = response
        else {
            return Ok(());
        };
        self.handle
            .update(RunUpdate::Monitor(MonitorUpdate::Callout {
                host: self.host.clone(),
                exec_id,
                pending_id,
                callout_index,
                name,
                prompt,
                context,
                schema: schema.as_value().clone(),
            }))
            .await
    }

    async fn fetch_inspection(
        &self,
        exec_id: ExecId,
    ) -> anyhow::Result<arena0_client::api::ExecutionInspection> {
        match self
            .client
            .call_host(
                &self.host,
                &HostRequest::ExecInspect {
                    exec_id,
                    private_from: None,
                    private_limit: PRIVATE_INSPECTION_LIMIT,
                },
            )
            .await?
        {
            ResponseOk::Inspection(inspection) => Ok(inspection),
            other => bail!("unexpected exec.inspect response: {other:?}"),
        }
    }

    async fn fetch_trace(
        &self,
        exec_id: ExecId,
        step: Option<u64>,
    ) -> anyhow::Result<Vec<TraceEntry>> {
        let end = step.map_or(TRACE_LIMIT, |step| step.saturating_add(1));
        let from = end.saturating_sub(TRACE_LIMIT);
        match self
            .client
            .call_host(
                &self.host,
                &HostRequest::ExecTrace {
                    exec_id,
                    from,
                    to: end,
                },
            )
            .await?
        {
            ResponseOk::Trace(entries) => Ok(entries),
            other => bail!("unexpected exec.trace response: {other:?}"),
        }
    }

    async fn observe_event(
        &self,
        frame: arena0_client::api::EventFrame,
        refresh_execution: bool,
    ) -> anyhow::Result<()> {
        let key = frame.exec_id.map(|exec_id| MonitorExecutionKey {
            host: self.host.clone(),
            exec_id,
        });
        if let Some(key) = &key {
            self.handle
                .update(RunUpdate::Monitor(MonitorUpdate::SystemEvent {
                    key: key.clone(),
                    frame: frame.clone(),
                }))
                .await?;
        }
        if let Some(exec_id) = frame.exec_id {
            let activity = MonitorActivity {
                host: Some(self.host.clone()),
                exec_id: Some(exec_id),
                sequence: Some(frame.seq),
                ts: frame.ts,
                kind: frame.kind().to_owned(),
                summary: crate::tui::event_summary(&frame),
                stale: false,
            };
            self.handle
                .update(RunUpdate::Monitor(MonitorUpdate::Activity { activity }))
                .await?;
        }
        match (&frame.data, key) {
            (
                EventData::SessionCallout {
                    pending_id,
                    callout_index,
                    name,
                    prompt,
                    schema,
                    context,
                },
                Some(key),
            ) => {
                self.handle
                    .update(RunUpdate::Monitor(MonitorUpdate::Callout {
                        host: key.host,
                        exec_id: key.exec_id,
                        pending_id: *pending_id,
                        callout_index: *callout_index,
                        name: name.clone(),
                        prompt: prompt.clone(),
                        context: context.clone(),
                        schema: schema.as_value().clone(),
                    }))
                    .await?;
            }
            (
                EventData::SessionStep {
                    signers,
                    participants,
                    ..
                },
                Some(key),
            ) => {
                self.handle
                    .update(RunUpdate::Monitor(MonitorUpdate::Agreement {
                        key,
                        agreed: *signers,
                        total: *participants,
                    }))
                    .await?;
            }
            _ => {}
        }
        if refresh_execution && let Some(exec_id) = frame.exec_id {
            // The event is a freshness trigger. Refresh only this execution so a
            // busy Host with many executions does not make the selected view lag.
            match self
                .client
                .call_host(&self.host, &HostRequest::ExecStatus { exec_id })
                .await
            {
                Ok(ResponseOk::Status(status)) => {
                    self.refresh_execution(status).await?;
                }
                Ok(other) => {
                    self.mark_execution_gap(
                        exec_id,
                        format!("unexpected exec.status response: {other:?}"),
                    )
                    .await?;
                }
                Err(error) => {
                    self.mark_execution_gap(
                        exec_id,
                        format!("execution refresh unavailable: {error:#}"),
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    async fn mark_execution_gap(&self, exec_id: ExecId, summary: String) -> anyhow::Result<()> {
        self.handle
            .update(RunUpdate::Monitor(MonitorUpdate::Gap {
                host: Some(self.host.clone()),
                exec_id: Some(exec_id),
                summary,
            }))
            .await
    }
}

async fn activity_worker(client: DaemonClient, handle: TuiHandle) -> anyhow::Result<()> {
    let mut calls = BTreeMap::<String, (Option<HostName>, Option<ExecId>, String)>::new();
    loop {
        let mut subscription = match client.subscribe_activity().await {
            Ok(subscription) => subscription,
            Err(error) => {
                report_activity_gap(&handle, format!("activity stream unavailable: {error:#}"))
                    .await?;
                calls.clear();
                tokio::time::sleep(RECONNECT_DELAY).await;
                continue;
            }
        };
        loop {
            let frame = match subscription.next().await {
                Ok(Some(frame)) => frame,
                Ok(None) => {
                    report_activity_gap(&handle, "activity stream closed".to_owned()).await?;
                    calls.clear();
                    break;
                }
                Err(error) => {
                    report_activity_gap(
                        &handle,
                        format!("activity stream disconnected: {error:#}"),
                    )
                    .await?;
                    calls.clear();
                    break;
                }
            };
            match frame.data {
                ActivityData::Started {
                    call_id,
                    tool,
                    host,
                    exec_id,
                } => {
                    let host = host.and_then(|host| host.parse::<HostName>().ok());
                    if calls.len() >= MAX_ACTIVITY_CALLS {
                        calls.clear();
                        report_activity_gap(
                            &handle,
                            format!("in-flight activity limit ({MAX_ACTIVITY_CALLS}) reset"),
                        )
                        .await?;
                    }
                    calls.insert(call_id.clone(), (host.clone(), exec_id, tool.clone()));
                    send_activity(
                        &handle,
                        MonitorActivity {
                            host,
                            exec_id,
                            sequence: Some(frame.seq),
                            ts: frame.ts,
                            kind: "MCP START".to_owned(),
                            summary: format!("{tool}  call {call_id}"),
                            stale: false,
                        },
                    )
                    .await?;
                }
                ActivityData::Finished {
                    call_id,
                    elapsed_ms,
                    result,
                } => {
                    let (host, exec_id, tool) = calls.remove(&call_id).unwrap_or((
                        None,
                        None,
                        "unobserved start".to_owned(),
                    ));
                    send_activity(
                        &handle,
                        MonitorActivity {
                            host,
                            exec_id,
                            sequence: Some(frame.seq),
                            ts: frame.ts,
                            kind: "MCP DONE".to_owned(),
                            summary: format!(
                                "{tool}  {elapsed_ms}ms  {}  call {call_id}",
                                activity_result(&result)
                            ),
                            stale: false,
                        },
                    )
                    .await?;
                }
                ActivityData::Lagged { skipped } => {
                    calls.clear();
                    send_activity(
                        &handle,
                        MonitorActivity {
                            host: None,
                            exec_id: None,
                            sequence: Some(frame.seq),
                            ts: frame.ts,
                            kind: "ACTIVITY GAP".to_owned(),
                            summary: format!("{skipped} activity records skipped"),
                            stale: true,
                        },
                    )
                    .await?;
                    handle
                        .update(RunUpdate::Monitor(MonitorUpdate::Gap {
                            host: None,
                            exec_id: None,
                            summary: format!("activity stream lagged by {skipped} records"),
                        }))
                        .await?;
                }
            }
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

async fn report_activity_gap(handle: &TuiHandle, summary: String) -> anyhow::Result<()> {
    send_activity(
        handle,
        MonitorActivity {
            host: None,
            exec_id: None,
            sequence: None,
            ts: now_epoch_seconds().saturating_mul(1000),
            kind: "ACTIVITY GAP".to_owned(),
            summary: summary.clone(),
            stale: true,
        },
    )
    .await?;
    handle
        .update(RunUpdate::Monitor(MonitorUpdate::Gap {
            host: None,
            exec_id: None,
            summary,
        }))
        .await
}

async fn send_activity(handle: &TuiHandle, activity: MonitorActivity) -> anyhow::Result<()> {
    handle
        .update(RunUpdate::Monitor(MonitorUpdate::Activity { activity }))
        .await
}

async fn submit_action(
    client: &DaemonClient,
    handle: &TuiHandle,
    action: MonitorAction,
) -> anyhow::Result<()> {
    let MonitorAction::Submit {
        host,
        exec_id,
        pending_id,
        answer,
    } = action;
    let result = match client
        .call_host_raw(
            &host,
            &HostRequest::ExecSubmit {
                exec_id,
                pending_id,
                answer: Some(answer),
            },
        )
        .await
    {
        Ok(Ok(ResponseOk::Ack)) => MonitorSubmission::Accepted,
        Ok(Err(error)) if error.code == ApiErrorCode::CalloutNotPending => {
            MonitorSubmission::CalloutNotPending(error.message)
        }
        Ok(Err(error)) => MonitorSubmission::Rejected(error.to_string()),
        Ok(Ok(other)) => {
            MonitorSubmission::Rejected(format!("unexpected exec.submit response: {other:?}"))
        }
        Err(error) => {
            MonitorSubmission::TransportUnknown(format!("answer outcome unknown: {error:#}"))
        }
    };
    handle
        .update(RunUpdate::Monitor(MonitorUpdate::Submission {
            host,
            exec_id,
            pending_id,
            result,
        }))
        .await
}

fn activity_result(result: &ActivityResult) -> &'static str {
    match result {
        ActivityResult::Ok => "ok",
        ActivityResult::ToolError { .. } => "tool error",
        ActivityResult::Interrupted => "interrupted",
    }
}

fn now_epoch_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_client::api::{
        ApiError, ExecOrigin, ExecStatusState, ExecutionInspection, HostInfo, Request, Response,
        frame::{read_frame, write_frame},
    };
    use arena0_client::protocol::PeerId;
    use tokio::io::{AsyncReadExt as _, BufReader};
    use tokio::net::{UnixListener, UnixStream};

    async fn serve_connection(
        stream: UnixStream,
        expected_host: &str,
        statuses: &[arena0_client::api::ExecStatus],
        events: &[arena0_client::api::EventFrame],
        exec_list_count: &std::sync::atomic::AtomicUsize,
        fail_exec_list_at: Option<usize>,
    ) -> anyhow::Result<()> {
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        let request = read_frame::<_, Request>(&mut read)
            .await?
            .ok_or_else(|| anyhow!("monitor test request stream closed"))?;
        let Request::Host { host, request } = request else {
            bail!("monitor test received a daemon-level request")
        };
        assert_eq!(host, expected_host);
        if matches!(&request, HostRequest::EventsSubscribe { .. }) {
            let response: Response = Ok(ResponseOk::Subscribed);
            write_frame(&mut write, &response).await?;
            for event in events {
                write_frame(&mut write, event).await?;
            }
            return Ok(());
        }

        let response: Response = match request {
            HostRequest::ProgramGet { .. } => Ok(ResponseOk::Ack),
            HostRequest::ExecList => {
                let request_number =
                    exec_list_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                if fail_exec_list_at == Some(request_number) {
                    Err(ApiError::new(
                        ApiErrorCode::Internal,
                        "scripted exec.list failure",
                    ))
                } else {
                    Ok(ResponseOk::ExecList(statuses.to_vec()))
                }
            }
            HostRequest::ExecInspect { exec_id, .. } => {
                let status = statuses
                    .iter()
                    .find(|status| status.exec_id == exec_id)
                    .ok_or_else(|| anyhow!("unknown inspection execution {exec_id}"))?;
                Ok(ResponseOk::Inspection(ExecutionInspection {
                    status: status.clone(),
                    activation: None,
                    private_from: 0,
                    private: Vec::new(),
                    private_total: 0,
                    private_next: None,
                }))
            }
            HostRequest::ExecView { .. } => {
                Err(ApiError::new(ApiErrorCode::Execution, "view unavailable"))
            }
            HostRequest::ExecTrace { .. } => Ok(ResponseOk::Trace(Vec::new())),
            other => bail!("unexpected monitor test request: {other:?}"),
        };
        write_frame(&mut write, &response).await?;
        Ok(())
    }

    async fn serve_socket(
        listener: UnixListener,
        expected_host: String,
        statuses: Vec<arena0_client::api::ExecStatus>,
        events: Vec<arena0_client::api::EventFrame>,
        exec_list_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        fail_exec_list_at: Option<usize>,
    ) -> anyhow::Result<()> {
        loop {
            let (stream, _) = listener.accept().await?;
            serve_connection(
                stream,
                &expected_host,
                &statuses,
                &events,
                &exec_list_count,
                fail_exec_list_at,
            )
            .await?;
        }
    }

    async fn serve_scheduling_connection(
        stream: UnixStream,
        expected_host: &str,
        statuses: &[arena0_client::api::ExecStatus],
        event: &arena0_client::api::EventFrame,
        exec_list_count: &std::sync::atomic::AtomicUsize,
        timestamps: &tokio::sync::mpsc::UnboundedSender<(usize, tokio::time::Instant)>,
    ) -> anyhow::Result<()> {
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        let request = read_frame::<_, Request>(&mut read)
            .await?
            .ok_or_else(|| anyhow!("monitor scheduling request stream closed"))?;
        let Request::Host { host, request } = request else {
            bail!("monitor scheduling test received a daemon-level request")
        };
        assert_eq!(host, expected_host);
        if matches!(&request, HostRequest::EventsSubscribe { .. }) {
            let response: Response = Ok(ResponseOk::Subscribed);
            write_frame(&mut write, &response).await?;
            write_frame(&mut write, event).await?;
            let mut discarded = Vec::new();
            read.read_to_end(&mut discarded).await?;
            return Ok(());
        }

        let response: Response = match request {
            HostRequest::ProgramGet { .. } => Ok(ResponseOk::Ack),
            HostRequest::ExecList => {
                let request_number =
                    exec_list_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                timestamps
                    .send((request_number, tokio::time::Instant::now()))
                    .map_err(|_| anyhow!("monitor timestamp receiver closed"))?;
                Ok(ResponseOk::ExecList(statuses.to_vec()))
            }
            HostRequest::ExecStatus { exec_id } => {
                let status = statuses
                    .iter()
                    .find(|status| status.exec_id == exec_id)
                    .ok_or_else(|| anyhow!("unknown scheduling execution {exec_id}"))?;
                Ok(ResponseOk::Status(status.clone()))
            }
            HostRequest::ExecInspect { exec_id, .. } => {
                let status = statuses
                    .iter()
                    .find(|status| status.exec_id == exec_id)
                    .ok_or_else(|| anyhow!("unknown inspection execution {exec_id}"))?;
                Ok(ResponseOk::Inspection(ExecutionInspection {
                    status: status.clone(),
                    activation: None,
                    private_from: 0,
                    private: Vec::new(),
                    private_total: 0,
                    private_next: None,
                }))
            }
            HostRequest::ExecView { .. } => {
                Err(ApiError::new(ApiErrorCode::Execution, "view unavailable"))
            }
            HostRequest::ExecTrace { .. } => Ok(ResponseOk::Trace(Vec::new())),
            other => bail!("unexpected monitor scheduling request: {other:?}"),
        };
        write_frame(&mut write, &response).await?;
        Ok(())
    }

    async fn serve_scheduling_socket(
        listener: UnixListener,
        expected_host: String,
        statuses: Vec<arena0_client::api::ExecStatus>,
        event: arena0_client::api::EventFrame,
        exec_list_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        timestamps: tokio::sync::mpsc::UnboundedSender<(usize, tokio::time::Instant)>,
    ) -> anyhow::Result<()> {
        let mut handlers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let expected_host = expected_host.clone();
                    let statuses = statuses.clone();
                    let event = event.clone();
                    let exec_list_count = exec_list_count.clone();
                    let timestamps = timestamps.clone();
                    handlers.spawn(async move {
                        serve_scheduling_connection(
                            stream,
                            &expected_host,
                            &statuses,
                            &event,
                            &exec_list_count,
                            &timestamps,
                        )
                        .await
                    });
                }
                joined = handlers.join_next(), if !handlers.is_empty() => {
                    let result = joined
                        .ok_or_else(|| anyhow!("monitor scheduling server handler disappeared"))?
                        .context("monitor scheduling server handler join")?;
                    result?;
                }
            }
        }
    }

    fn status(exec_id: ExecId, program_id: ProgramHash) -> arena0_client::api::ExecStatus {
        arena0_client::api::ExecStatus {
            exec_id,
            negotiation_id: None,
            program_id,
            state: ExecStatusState::Negotiating {
                queue_position: None,
            },
        }
    }

    #[tokio::test]
    async fn sequence_gap_refreshes_other_execution_without_duplicate_host_refresh()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir().expect("monitor test directory");
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind monitor test socket");
        let host: HostName = "host-01".parse().expect("test Host name");
        let peer_id = PeerId([7; 32]);
        let host_info = HostInfo {
            id: host.to_string(),
            peer_id,
            user_agent: Some("monitor-test".to_owned()),
        };
        let program_id = ProgramHash([3; 32]);
        let first = status(ExecId([1; 32]), program_id);
        let other = status(ExecId([2; 32]), program_id);
        let events = vec![
            serde_json::from_value(serde_json::json!({
                "host": host_info.clone(),
                "boot_id": "boot-1",
                "seq": 0,
                "ts": 0,
                "kind": "host.started",
                "data": {
                    "version": "test",
                    "transport_key": "07".repeat(32),
                    "abi_version": 1
                }
            }))
            .expect("synthetic HostStarted snapshot"),
            arena0_client::api::EventFrame::new(
                host_info.clone(),
                "boot-1",
                10,
                1,
                EventData::HostStopped {
                    reason: None,
                    uptime_secs: 1,
                },
                None,
                None,
            )
            .expect("first live event"),
            arena0_client::api::EventFrame::new(
                arena0_client::api::HostInfo {
                    id: host.to_string(),
                    peer_id,
                    user_agent: Some("monitor-test".to_owned()),
                },
                "boot-1",
                12,
                2,
                EventData::Created {
                    program_id,
                    negotiation_id: None,
                    queue_position: None,
                    origin: ExecOrigin::Request,
                },
                Some(other.exec_id),
                None,
            )
            .expect("gapped event"),
            arena0_client::api::EventFrame::new(
                arena0_client::api::HostInfo {
                    id: host.to_string(),
                    peer_id,
                    user_agent: Some("monitor-test".to_owned()),
                },
                "boot-1",
                14,
                3,
                EventData::Lagged { skipped: 1 },
                None,
                None,
            )
            .expect("gapped lagged event"),
        ];
        let exec_list_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = tokio::spawn(serve_socket(
            listener,
            host.to_string(),
            vec![first.clone(), other.clone()],
            events,
            exec_list_count.clone(),
            None,
        ));
        let (handle, mut updates, _pages) = TuiHandle::test_channel();
        let monitor = HostMonitor {
            host: host.clone(),
            client: DaemonClient::new(socket),
            handle,
            schemas: Arc::new(MessageSchemas::default()),
        };
        let connection = tokio::spawn(async move { monitor.run_connection().await });
        let connection_result = tokio::time::timeout(Duration::from_secs(3), connection)
            .await
            .expect("gap refresh should complete")
            .expect("monitor task should join");
        assert!(
            connection_result.is_err(),
            "the scripted subscription should end after all frames"
        );

        let mut counts = [0usize; 2];
        let mut gaps = 0usize;
        let mut gap_summaries = Vec::new();
        let mut saw_gapped_event = false;
        while let Some(update) = updates.recv().await {
            match update {
                RunUpdate::Monitor(MonitorUpdate::Execution { key, gap, .. }) => {
                    assert!(gap.is_none(), "successful gap refresh must stay fresh");
                    if key.exec_id == first.exec_id {
                        counts[0] += 1;
                    } else if key.exec_id == other.exec_id {
                        counts[1] += 1;
                    }
                }
                RunUpdate::Monitor(MonitorUpdate::Gap {
                    exec_id, summary, ..
                }) => {
                    assert!(
                        exec_id.is_none(),
                        "Host-wide gap must not stale one execution"
                    );
                    assert!(summary.contains("snapshot refreshed"));
                    gap_summaries.push(summary);
                    gaps += 1;
                }
                RunUpdate::Monitor(MonitorUpdate::SystemEvent { frame, .. }) if frame.seq == 12 => {
                    assert!(matches!(frame.data, EventData::Created { .. }));
                    saw_gapped_event = true;
                }
                RunUpdate::Monitor(MonitorUpdate::Activity { activity })
                    if activity.sequence == Some(12) =>
                {
                    saw_gapped_event = true;
                }
                _ => {}
            }
        }
        server.abort();
        let server_result = server.await.expect_err("scripted server should be aborted");
        assert!(server_result.is_cancelled());
        assert_eq!(counts, [3, 3]);
        assert_eq!(gaps, 2);
        assert!(
            gap_summaries
                .iter()
                .any(|summary| summary.contains("event gap after #10"))
        );
        assert!(gap_summaries.iter().any(|summary| {
            summary.contains("event gap after #12")
                && summary.contains("Host event stream lagged by 1")
        }));
        assert!(
            saw_gapped_event,
            "the received frame after a gap must be observed"
        );
        assert_eq!(
            exec_list_count.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "initial refresh plus two Host-wide gap refreshes, without a duplicate Lagged refresh"
        );
        Ok::<_, anyhow::Error>(())
    }

    #[tokio::test]
    async fn lagged_refresh_failure_is_reported_after_the_refresh_fails() -> anyhow::Result<()> {
        let directory = tempfile::tempdir().expect("monitor test directory");
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind monitor test socket");
        let host: HostName = "host-01".parse().expect("test Host name");
        let peer_id = PeerId([8; 32]);
        let host_info = HostInfo {
            id: host.to_string(),
            peer_id,
            user_agent: Some("monitor-test".to_owned()),
        };
        let events = vec![
            arena0_client::api::EventFrame::new(
                host_info.clone(),
                "boot-1",
                1,
                1,
                EventData::HostStopped {
                    reason: None,
                    uptime_secs: 1,
                },
                None,
                None,
            )
            .expect("first event"),
            arena0_client::api::EventFrame::new(
                host_info,
                "boot-1",
                2,
                2,
                EventData::Lagged { skipped: 1 },
                None,
                None,
            )
            .expect("lagged event"),
        ];
        let exec_list_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = tokio::spawn(serve_socket(
            listener,
            host.to_string(),
            Vec::new(),
            events,
            exec_list_count.clone(),
            Some(2),
        ));
        let (handle, mut updates, _pages) = TuiHandle::test_channel();
        let monitor = HostMonitor {
            host: host.clone(),
            client: DaemonClient::new(socket),
            handle,
            schemas: Arc::new(MessageSchemas::default()),
        };
        let connection = tokio::spawn(async move { monitor.run_connection().await });
        let connection_result = tokio::time::timeout(Duration::from_secs(3), connection)
            .await
            .expect("lagged failure should complete")
            .expect("monitor task should join");
        let error = connection_result.expect_err("lagged refresh failure should reconnect");
        assert!(error.to_string().contains("scripted exec.list failure"));

        let mut summaries = Vec::new();
        while let Some(update) = updates.recv().await {
            if let RunUpdate::Monitor(MonitorUpdate::Gap {
                exec_id, summary, ..
            }) = update
            {
                assert!(exec_id.is_none(), "Lagged is a Host-wide gap");
                summaries.push(summary);
            }
        }
        server.abort();
        let server_result = server.await.expect_err("scripted server should be aborted");
        assert!(server_result.is_cancelled());
        assert_eq!(
            exec_list_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "initial refresh plus one failed Lagged refresh"
        );
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].contains("snapshot refresh unavailable"));
        assert!(!summaries[0].contains("snapshot refreshed"));
        Ok::<_, anyhow::Error>(())
    }

    #[tokio::test(start_paused = true)]
    async fn host_monitor_does_not_repeat_initial_snapshot_before_refresh_interval()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir().expect("monitor scheduling directory");
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind monitor scheduling socket");
        let host: HostName = "host-01".parse().expect("test Host name");
        let peer_id = PeerId([9; 32]);
        let host_info = HostInfo {
            id: host.to_string(),
            peer_id,
            user_agent: Some("monitor-scheduling-test".to_owned()),
        };
        let program_id = ProgramHash([4; 32]);
        let execution = status(ExecId([4; 32]), program_id);
        let event = arena0_client::api::EventFrame::new(
            host_info,
            "boot-1",
            1,
            1,
            EventData::Created {
                program_id,
                negotiation_id: None,
                queue_position: None,
                origin: ExecOrigin::Request,
            },
            Some(execution.exec_id),
            None,
        )
        .expect("scheduling event");
        let exec_list_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (timestamps, mut timestamps_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(serve_scheduling_socket(
            listener,
            host.to_string(),
            vec![execution],
            event,
            exec_list_count.clone(),
            timestamps,
        ));
        let (handle, mut updates, _pages) = TuiHandle::test_channel();
        let monitor = HostMonitor {
            host: host.clone(),
            client: DaemonClient::new(socket),
            handle,
            schemas: Arc::new(MessageSchemas::default()),
        };
        let connection = tokio::spawn(async move { monitor.run_connection().await });
        loop {
            let update = updates
                .recv()
                .await
                .ok_or_else(|| anyhow!("monitor scheduling updates closed"))?;
            if let RunUpdate::Monitor(MonitorUpdate::Activity { activity }) = update
                && activity.sequence == Some(1)
            {
                break;
            }
        }
        let (initial_number, initial_request_at) = timestamps_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("read initial monitor refresh timestamp"))?;
        assert_eq!(initial_number, 1);
        assert_eq!(
            exec_list_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the initial snapshot must not be followed by an immediate periodic refresh"
        );
        let (periodic_number, periodic_request_at) = timestamps_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("read periodic monitor refresh timestamp"))?;
        assert_eq!(periodic_number, 2);
        assert_eq!(
            exec_list_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the refresh occurs once when the interval expires"
        );
        assert!(
            periodic_request_at >= initial_request_at + REFRESH_INTERVAL,
            "monitor refreshed before a full delay after the initial snapshot: {:?}",
            periodic_request_at.saturating_duration_since(initial_request_at)
        );
        connection.abort();
        assert!(
            connection
                .await
                .expect_err("monitor task should be aborted")
                .is_cancelled()
        );
        server.abort();
        assert!(
            server
                .await
                .expect_err("scheduling server should be aborted")
                .is_cancelled()
        );
        Ok(())
    }
}
