//! Live daemon attachment for the unified execution observatory.
//!
//! This module owns only the effectful monitor shell.  [`crate::tui`] remains
//! the sole terminal loop and state owner: this side fetches bounded snapshots,
//! translates public daemon events, and executes an explicitly selected answer.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use arena0_client::api::{
    ActivityData, ActivityResult, ApiErrorCode, ColorDepth, EventData, EventFilter, HostRequest,
    NextEvent, ResponseOk,
};
use arena0_client::program::{BorshSchemaDocument, ProgramHash};
use arena0_client::proto::DaemonClient;
use arena0_client::protocol::{ExecId, TraceEntry};
use arena0_home::HostName;
use clap::Args;
use tokio::sync::watch;
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
            let task = workers.spawn(async move { host_worker(host, client, handle).await });
            subscriptions.insert(name, (peer, task));
        }
        workers.spawn(activity_worker(entry_client.clone(), handle.clone()));
        let mut ticker = tokio::time::interval(REFRESH_INTERVAL);
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
                            let task = workers.spawn(async move { host_worker(observed, client, handle).await });
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
) -> anyhow::Result<()> {
    let mut message_schemas = BTreeMap::<ProgramHash, Option<BorshSchemaDocument>>::new();
    loop {
        match run_host_connection(&host, &client, &handle, &mut message_schemas).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                let _ = handle
                    .update(RunUpdate::Monitor(MonitorUpdate::Connection {
                        host: host.host.clone(),
                        message: format!("disconnected: {error:#}"),
                        stale: true,
                    }))
                    .await;
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }
}

async fn run_host_connection(
    host: &MonitorHost,
    client: &DaemonClient,
    handle: &TuiHandle,
    message_schemas: &mut BTreeMap<ProgramHash, Option<BorshSchemaDocument>>,
) -> anyhow::Result<()> {
    let mut subscription = client
        .subscribe(&host.host, EventFilter::default())
        .await
        .with_context(|| format!("subscribe to Host '{}' events", host.host))?;
    refresh_host(host, client, handle, message_schemas).await?;
    let mut ticker = tokio::time::interval(REFRESH_INTERVAL);
    let mut last_seq: Option<u64> = None;
    loop {
        tokio::select! {
            event = subscription.next() => {
                let frame = event?.ok_or_else(|| anyhow!("Host event stream closed"))?;
                if let Some(previous) = last_seq
                    && frame.seq > previous.saturating_add(1)
                {
                    let _ = handle.update(RunUpdate::Monitor(MonitorUpdate::Gap {
                        host: Some(host.host.clone()),
                        exec_id: frame.exec_id,
                        summary: format!("event gap after #{previous}; snapshot refreshed"),
                    })).await;
                }
                last_seq = Some(frame.seq);
                observe_event(host, client, handle, frame, message_schemas).await?;
            }
            _ = ticker.tick() => refresh_host(host, client, handle, message_schemas).await?,
        }
    }
}

async fn refresh_host(
    host: &MonitorHost,
    client: &DaemonClient,
    handle: &TuiHandle,
    message_schemas: &mut BTreeMap<ProgramHash, Option<BorshSchemaDocument>>,
) -> anyhow::Result<()> {
    let statuses = match client.call_host(&host.host, &HostRequest::ExecList).await? {
        ResponseOk::ExecList(statuses) => statuses,
        other => bail!(
            "unexpected response to exec.list from '{}': {other:?}",
            host.host
        ),
    };
    for status in statuses {
        refresh_execution(host, client, handle, status, message_schemas).await?;
    }
    Ok(())
}

/// Refresh one execution while retaining whatever parts of its last snapshot
/// could not be fetched during a transient daemon/storage failure. The TUI
/// applies stale updates as patches for exactly that reason.
async fn refresh_execution(
    host: &MonitorHost,
    client: &DaemonClient,
    handle: &TuiHandle,
    status: arena0_client::api::ExecStatus,
    message_schemas: &mut BTreeMap<ProgramHash, Option<BorshSchemaDocument>>,
) -> anyhow::Result<()> {
    let has_pending = status.pending_callout().is_some();
    let exec_id = status.exec_id;
    let program_id = status.program_id;
    let key = MonitorExecutionKey {
        host: host.host.clone(),
        exec_id,
    };
    let message_schema =
        lookup_message_schema(client, &host.host, program_id, message_schemas).await;
    let inspection = fetch_inspection(client, &host.host, exec_id).await;
    let view = fetch_view(client, &host.host, exec_id, handle.view_width()).await;
    let trace = fetch_trace(client, &host.host, exec_id, status.step()).await;
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
    let stale = gap.is_some();
    handle
        .update(RunUpdate::Monitor(MonitorUpdate::Execution {
            key,
            status,
            message_schema,
            inspection,
            view,
            trace,
            observed_at: now_epoch_seconds(),
            stale,
            gap,
        }))
        .await?;
    if has_pending {
        let _ = fetch_pending_callout(client, host, exec_id, handle).await;
    }
    Ok(())
}

async fn lookup_message_schema(
    client: &DaemonClient,
    host: &HostName,
    program_id: ProgramHash,
    cache: &mut BTreeMap<ProgramHash, Option<BorshSchemaDocument>>,
) -> Option<BorshSchemaDocument> {
    if let Some(schema) = cache.get(&program_id) {
        return schema.clone();
    }
    let Ok(ResponseOk::Program(program)) = client
        .call_host(
            host,
            &HostRequest::ProgramGet {
                program: program_id.to_string(),
            },
        )
        .await
    else {
        return None;
    };
    let schema = program
        .schema
        .messages
        .first()
        .map(|message| message.borsh.clone());
    cache.insert(program_id, schema.clone());
    schema
}

async fn fetch_pending_callout(
    client: &DaemonClient,
    host: &MonitorHost,
    exec_id: ExecId,
    handle: &TuiHandle,
) -> anyhow::Result<()> {
    let response = tokio::time::timeout(
        Duration::from_millis(250),
        client.call_host(&host.host, &HostRequest::ExecNext { exec_id }),
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
    handle
        .update(RunUpdate::Monitor(MonitorUpdate::Callout {
            host: host.host.clone(),
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
    client: &DaemonClient,
    host: &HostName,
    exec_id: ExecId,
) -> anyhow::Result<arena0_client::api::ExecutionInspection> {
    match client
        .call_host(
            host,
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

async fn fetch_view(
    client: &DaemonClient,
    host: &HostName,
    exec_id: ExecId,
    width: u16,
) -> anyhow::Result<Option<(u64, arena0_client::protocol::View)>> {
    match client
        .call_host_raw(
            host,
            &HostRequest::ExecView {
                exec: exec_id,
                width,
                color: ColorDepth::Mono,
            },
        )
        .await?
    {
        Ok(ResponseOk::ExecView { step, view }) => Ok(Some((step, view))),
        Err(error) if error.code == ApiErrorCode::Execution => Ok(None),
        Err(error) => bail!("{error}"),
        Ok(other) => bail!("unexpected exec.view response: {other:?}"),
    }
}

async fn fetch_trace(
    client: &DaemonClient,
    host: &HostName,
    exec_id: ExecId,
    step: Option<u64>,
) -> anyhow::Result<Vec<TraceEntry>> {
    let end = step.map_or(TRACE_LIMIT, |step| step.saturating_add(1));
    let from = end.saturating_sub(TRACE_LIMIT);
    match client
        .call_host(
            host,
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
    host: &MonitorHost,
    client: &DaemonClient,
    handle: &TuiHandle,
    frame: arena0_client::api::EventFrame,
    message_schemas: &mut BTreeMap<ProgramHash, Option<BorshSchemaDocument>>,
) -> anyhow::Result<()> {
    let key = frame.exec_id.map(|exec_id| MonitorExecutionKey {
        host: host.host.clone(),
        exec_id,
    });
    if let Some(key) = &key {
        handle
            .update(RunUpdate::Monitor(MonitorUpdate::SystemEvent {
                key: key.clone(),
                frame: frame.clone(),
            }))
            .await?;
    }
    if let Some(exec_id) = frame.exec_id {
        let activity = MonitorActivity {
            host: Some(host.host.clone()),
            exec_id: Some(exec_id),
            sequence: Some(frame.seq),
            ts: frame.ts,
            kind: frame.kind().to_owned(),
            summary: event_summary(&frame.data),
            stale: false,
        };
        handle
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
            handle
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
            handle
                .update(RunUpdate::Monitor(MonitorUpdate::Agreement {
                    key,
                    agreed: *signers,
                    total: *participants,
                }))
                .await?;
        }
        _ => {}
    }
    if let EventData::Lagged { skipped } = frame.data {
        let summary = format!("Host event stream lagged by {skipped}; snapshot refreshed");
        handle
            .update(RunUpdate::Monitor(MonitorUpdate::Gap {
                host: Some(host.host.clone()),
                exec_id: None,
                summary,
            }))
            .await?;
        refresh_host(host, client, handle, message_schemas).await?;
    } else if let Some(exec_id) = frame.exec_id {
        // The event is a freshness trigger. Refresh only this execution so a
        // busy Host with many executions does not make the selected view lag.
        match client
            .call_host(&host.host, &HostRequest::ExecStatus { exec_id })
            .await
        {
            Ok(ResponseOk::Status(status)) => {
                refresh_execution(host, client, handle, status, message_schemas).await?;
            }
            Ok(other) => {
                mark_execution_gap(
                    host,
                    handle,
                    exec_id,
                    format!("unexpected exec.status response: {other:?}"),
                )
                .await?;
            }
            Err(error) => {
                mark_execution_gap(
                    host,
                    handle,
                    exec_id,
                    format!("execution refresh unavailable: {error:#}"),
                )
                .await?;
            }
        }
    }
    Ok(())
}

async fn mark_execution_gap(
    host: &MonitorHost,
    handle: &TuiHandle,
    exec_id: ExecId,
    summary: String,
) -> anyhow::Result<()> {
    handle
        .update(RunUpdate::Monitor(MonitorUpdate::Gap {
            host: Some(host.host.clone()),
            exec_id: Some(exec_id),
            summary,
        }))
        .await
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

fn event_summary(data: &EventData) -> String {
    match data {
        EventData::SessionCallout { name, .. } => format!("callout {name}"),
        EventData::SessionCalloutAnswered { pending_id } => {
            format!("callout {pending_id} answered")
        }
        EventData::SessionStep {
            step,
            fuel_used,
            signers,
            participants,
            ..
        } => format!("step {step}  agreement {signers}/{participants}  fuel {fuel_used}"),
        EventData::SessionEnded { .. } => "session ended".to_owned(),
        EventData::Created { .. } => "execution created".to_owned(),
        EventData::Terminated { reason, .. } => reason.clone(),
        _ => data.kind().to_owned(),
    }
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
