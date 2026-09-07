//! One daemon-owned MCP surface for the complete local Ensemble.
//!
//! Tool calls carry explicit Host references, so one Streamable HTTP endpoint
//! and one bearer token can safely drive every supervised participant.

use futures::{
    FutureExt as _,
    future::{BoxFuture, Shared},
};
use std::collections::BTreeSet;
use std::future::Future as _;
use std::io::IoSlice;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use arena0_api::{
    ActivityData, ActivityResult, ApiError, ApiErrorCode, EnsembleSpec, ExecStatusState,
    HostRequest, NextEvent, PendingId, ProgramSummary, ReceiptRef, ResponseOk, VerifiedResult,
};
use arena0_program::ParticipantCount;
use arena0_protocol::{ExecId, PeerId, SessionHash};
#[cfg(test)]
use arena0_sandbox::WasmtimeEngine;
use axum::extract::{Request as HttpRequest, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response as HttpResponse};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{CallToolRequestParams, CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::ensemble::{Daemon, McpConfig};
use crate::server::HostService;
use crate::startup::{self, StartupStage};

#[derive(
    Clone, Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(deny_unknown_fields)]
struct HostRef {
    /// Daemon-local Host id, as returned by `open_host` or `list_hosts`.
    #[schemars(length(min = 1))]
    id: String,
}

#[derive(
    Clone, Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(deny_unknown_fields)]
struct ProgramRef {
    host: HostRef,
    /// Exact program content id returned by `list_programs`.
    #[schemars(length(min = 1))]
    program_id: String,
}

#[derive(
    Clone, Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(deny_unknown_fields)]
struct ExecRef {
    host: HostRef,
    /// Host-local execution id (64 lowercase hex characters).
    exec_id: String,
}

#[derive(
    Clone, Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(deny_unknown_fields)]
struct SessionRef {
    host: HostRef,
    /// Cross-party session id (64 lowercase hex characters).
    session_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct OpenHostArg {
    /// Omit to create a new Host; retain the returned id for reopening and retries.
    id: Option<String>,
    /// Caller-reported harness and version, for example claude-code/1.0.
    #[schemars(length(min = 1, max = 256))]
    user_agent: String,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct OpenHostOutput {
    host: HostRef,
    peer_id: String,
    user_agent: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct HostArg {
    host: HostRef,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProgramArg {
    program: ProgramRef,
}

/// Agent-facing admission choice. Host names are resolved to the exact peer
/// identities owned by this daemon before the protocol request is dispatched.
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
enum McpEnsemble {
    /// Create an offer for these exact other local Hosts.
    Explicit {
        #[schemars(length(min = 1))]
        hosts: Vec<HostRef>,
    },
    /// Join one exact negotiation created by another local Host.
    Join {
        creator: HostRef,
        #[schemars(length(min = 1))]
        negotiation_id: String,
    },
}

impl McpEnsemble {
    fn into_spec(self, daemon: &Daemon, owner: &HostRef) -> Result<EnsembleSpec, CallToolResult> {
        let owner_peer = peer_id(daemon, owner)?;
        match self {
            Self::Explicit { hosts } => {
                if hosts.is_empty() {
                    return Err(err("explicit ensemble needs at least one other Host"));
                }
                let mut seen = BTreeSet::new();
                let mut peers = Vec::with_capacity(hosts.len());
                for host in hosts {
                    let peer = peer_id(daemon, &host)?;
                    if peer == owner_peer {
                        return Err(err(format!(
                            "explicit ensemble must not include its owner Host '{}'",
                            owner.id
                        )));
                    }
                    if !seen.insert(peer) {
                        return Err(err(format!(
                            "explicit ensemble contains Host '{}' more than once",
                            host.id
                        )));
                    }
                    peers.push(peer);
                }
                Ok(EnsembleSpec::Explicit { peers })
            }
            Self::Join {
                creator,
                negotiation_id,
            } => {
                let creator_peer = peer_id(daemon, &creator)?;
                if creator_peer == owner_peer {
                    return Err(err("join creator must be a different Host"));
                }
                Ok(EnsembleSpec::Join {
                    creator: creator_peer,
                    negotiation_id: parse(&negotiation_id, "negotiation id")?,
                })
            }
        }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StartExecutionArg {
    program: ProgramRef,
    /// Program params as JSON, validated against the program's params schema.
    params: Option<Value>,
    ensemble: McpEnsemble,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExecArg {
    execution: ExecRef,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct AnswerCalloutArg {
    execution: ExecRef,
    /// Pending id returned by `await_execution_event`.
    pending_id: PendingId,
    /// JSON answer in the shape of the callout's inline schema.
    answer: Option<Value>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct QueryExecutionArg {
    execution: ExecRef,
    /// JSON query validated against the program's query request schema.
    query: Option<Value>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StopExecutionArg {
    execution: ExecRef,
    /// Human-readable forfeit or resignation reason after activation.
    reason: Option<String>,
}

#[derive(
    Debug, Default, Clone, Copy, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
enum VerificationMode {
    #[default]
    Light,
    Full,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct VerifySessionArg {
    session: SessionRef,
    /// `light` checks portable proof evidence; `full` also replays the exact Wasm.
    #[serde(default)]
    mode: VerificationMode,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct HostOutput {
    host: HostRef,
    peer_id: String,
    user_agent: Option<String>,
    programs: usize,
    executions_active: usize,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct HostListOutput {
    hosts: Vec<HostOutput>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct ProgramSummaryOutput {
    program: ProgramRef,
    name: String,
    display_name: String,
    version: String,
    description: String,
    participants: ParticipantCount,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct ProgramListOutput {
    programs: Vec<ProgramSummaryOutput>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct ProgramOutput {
    summary: ProgramSummaryOutput,
    schema: Value,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct StartExecutionOutput {
    execution: ExecRef,
    negotiation_id: String,
    session: Option<SessionRef>,
    state: Value,
    queue_position: Option<usize>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct AckOutput {
    ok: bool,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
enum ExecutionEvent {
    Waiting,
    Callout {
        pending_id: PendingId,
        callout_index: u32,
        name: String,
        prompt: String,
        schema: Value,
        context: Value,
    },
    Completed {
        session: SessionRef,
        outcome: Option<Value>,
    },
    Failed {
        reason: String,
    },
}

/// MCP output schemas require an object root. Flattening preserves the useful
/// tagged-event shape while satisfying that contract.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct ExecutionEventOutput {
    #[serde(flatten)]
    event: ExecutionEvent,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct ExecutionStatusOutput {
    execution: ExecRef,
    negotiation_id: Option<String>,
    program: ProgramRef,
    state: Value,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct QueryExecutionOutput {
    result: Value,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct ParticipantOutput {
    host: Option<HostRef>,
    peer_id: String,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct VerifySessionOutput {
    mode: Value,
    session: SessionRef,
    program: ProgramRef,
    participants: Vec<ParticipantOutput>,
    steps: u64,
    terminal: Value,
}

fn output_schema<T: schemars::JsonSchema + std::any::Any>() -> Arc<rmcp::model::JsonObject> {
    rmcp::handler::server::tool::schema_for_output::<T>()
        .unwrap_or_else(|error| panic!("invalid MCP output schema: {error}"))
}

#[derive(Clone, Debug)]
struct Arena0Mcp {
    #[allow(dead_code)]
    tool_router: ToolRouter<Arena0Mcp>,
    daemon: Arc<Daemon>,
}

struct ActivityGuard {
    activity: Arc<crate::server::Activity>,
    call_id: String,
    started: Instant,
    finished: bool,
}

impl ActivityGuard {
    fn new(activity: Arc<crate::server::Activity>, call_id: String, started: Instant) -> Self {
        Self {
            activity,
            call_id,
            started,
            finished: false,
        }
    }

    fn finish(&mut self, result: ActivityResult) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.activity.emit(ActivityData::Finished {
            call_id: self.call_id.clone(),
            elapsed_ms: self
                .started
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            result,
        });
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.finish(ActivityResult::Interrupted);
        }
    }
}

impl Arena0Mcp {
    fn new(daemon: Arc<Daemon>) -> Self {
        Self {
            tool_router: Self::tool_router(),
            daemon,
        }
    }

    async fn request(
        &self,
        host: &HostRef,
        request: HostRequest,
    ) -> Result<ResponseOk, CallToolResult> {
        let service = service(&self.daemon, host)?;
        service
            .dispatch(request)
            .await
            .map_err(|error| api_error(&host.id, error))
    }

    fn participant(&self, peer_id: PeerId) -> ParticipantOutput {
        let host = self.daemon.host_name(peer_id).map(|name| HostRef {
            id: name.to_owned(),
        });
        ParticipantOutput {
            host,
            peer_id: peer_id.to_string(),
        }
    }
}

#[tool_router]
impl Arena0Mcp {
    #[tool(
        description = "Open or create your Host. Supply its local id to reopen it, or omit id to generate a new Host. Retain the returned reference. user_agent records your harness/version for the monitor; it is not authentication. A repeated call without id creates another Host.",
        output_schema = output_schema::<OpenHostOutput>(),
        annotations(title = "Open Host", read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = false)
    )]
    async fn open_host(
        &self,
        Parameters(arg): Parameters<OpenHostArg>,
    ) -> Result<Json<OpenHostOutput>, CallToolResult> {
        let info = self
            .daemon
            .open_host(arg.id, arg.user_agent)
            .await
            .map_err(|error| err(format!("{error:#}")))?;
        Ok(Json(OpenHostOutput {
            host: HostRef { id: info.id },
            peer_id: info.peer_id.to_string(),
            user_agent: info.user_agent,
        }))
    }

    #[tool(
        description = "List every Host available through this daemon. Use the returned Host names in all other tools.",
        output_schema = output_schema::<HostListOutput>(),
        annotations(title = "List Hosts", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn list_hosts(
        &self,
        Parameters(_): Parameters<EmptyArgs>,
    ) -> Result<Json<HostListOutput>, CallToolResult> {
        let services = self.daemon.services();
        let mut hosts = Vec::with_capacity(services.len());
        for (name, service) in services {
            let info = service
                .host_status()
                .await
                .map_err(|error| err(format!("Host '{name}': {error}")))?;
            hosts.push(HostOutput {
                host: HostRef { id: name },
                peer_id: info.host.peer_id.to_string(),
                user_agent: info.host.user_agent,
                programs: info.programs,
                executions_active: info.execs_active,
            });
        }
        Ok(Json(HostListOutput { hosts }))
    }

    #[tool(
        description = "List programs installed on one Host. Inspect a selected program before starting an execution.",
        output_schema = output_schema::<ProgramListOutput>(),
        annotations(title = "List programs", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn list_programs(
        &self,
        Parameters(HostArg { host }): Parameters<HostArg>,
    ) -> Result<Json<ProgramListOutput>, CallToolResult> {
        match self.request(&host, HostRequest::ProgramList).await? {
            ResponseOk::ProgramList(programs) => Ok(Json(ProgramListOutput {
                programs: programs
                    .iter()
                    .map(|summary| summary_output(&host, summary))
                    .collect(),
            })),
            other => Err(unexpected(&other)),
        }
    }

    #[tool(
        description = "Read one program's summary and public JSON Schema. Use it to construct params, queries, and callout answers.",
        output_schema = output_schema::<ProgramOutput>(),
        annotations(title = "Inspect program", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inspect_program(
        &self,
        Parameters(ProgramArg { program }): Parameters<ProgramArg>,
    ) -> Result<Json<ProgramOutput>, CallToolResult> {
        match self
            .request(
                &program.host,
                HostRequest::ProgramGet {
                    program: program.program_id,
                },
            )
            .await?
        {
            ResponseOk::Program(detail) => Ok(Json(ProgramOutput {
                summary: summary_output(&program.host, &detail.summary),
                schema: serialized_value(&detail.schema)?,
            })),
            other => Err(unexpected(&other)),
        }
    }

    #[tool(
        description = "Create or join an execution on one Host. Negotiation continues in the background and the returned execution reference owns every later call.",
        output_schema = output_schema::<StartExecutionOutput>(),
        annotations(title = "Start execution", read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = false)
    )]
    async fn start_execution(
        &self,
        Parameters(arg): Parameters<StartExecutionArg>,
    ) -> Result<Json<StartExecutionOutput>, CallToolResult> {
        let ensemble = arg.ensemble.into_spec(&self.daemon, &arg.program.host)?;
        let exec_id = ExecId(rand::random());
        let created = self
            .request(
                &arg.program.host,
                HostRequest::ExecNew {
                    exec_id,
                    program: arg.program.program_id,
                    params: arg.params,
                    ensemble,
                },
            )
            .await?;
        match created {
            ResponseOk::ExecCreated {
                exec_id: returned_exec_id,
                negotiation_id,
                session_id,
                exec_state,
                queue_position,
            } => {
                if returned_exec_id != exec_id {
                    let cleanup = self
                        .request(
                            &arg.program.host,
                            HostRequest::ExecCancelCreation { exec_id },
                        )
                        .await;
                    return match cleanup {
                        Ok(ResponseOk::Ack) => Err(err(format!(
                            "Host '{}' returned execution id {returned_exec_id}, requested {exec_id}; the requested id was withdrawn",
                            arg.program.host.id
                        ))),
                        Ok(other) => Err(err(format!(
                            "Host '{}' returned execution id {returned_exec_id}, requested {exec_id}; cleanup returned {other:?}",
                            arg.program.host.id
                        ))),
                        Err(error) => Err(err(format!(
                            "Host '{}' returned execution id {returned_exec_id}, requested {exec_id}; cleanup failed: {error:?}",
                            arg.program.host.id
                        ))),
                    };
                }
                Ok(Json(StartExecutionOutput {
                    execution: exec_ref(&arg.program.host, returned_exec_id),
                    negotiation_id: negotiation_id.to_string(),
                    session: session_id.map(|id| session_ref(&arg.program.host, id)),
                    state: serialized_value(&exec_state)?,
                    queue_position,
                }))
            }
            other => Err(unexpected(&other)),
        }
    }

    #[tool(
        description = "Read an execution's durable lifecycle snapshot without waiting for or consuming a callout.",
        output_schema = output_schema::<ExecutionStatusOutput>(),
        annotations(title = "Get execution status", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn get_execution_status(
        &self,
        Parameters(ExecArg { execution }): Parameters<ExecArg>,
    ) -> Result<Json<ExecutionStatusOutput>, CallToolResult> {
        let exec_id = parse(&execution.exec_id, "execution id")?;
        match self
            .request(&execution.host, HostRequest::ExecStatus { exec_id })
            .await?
        {
            ResponseOk::Status(status) => Ok(Json(status_output(&execution.host, status)?)),
            other => Err(unexpected(&other)),
        }
    }

    #[tool(
        description = "Return the next durable agent decision point: a callout to answer, a completed session, or failure. Returns waiting immediately when no event is ready, so one MCP client can advance every Host. Repeating the call does not consume a pending result.",
        output_schema = output_schema::<ExecutionEventOutput>(),
        annotations(title = "Await execution event", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn await_execution_event(
        &self,
        Parameters(ExecArg { execution }): Parameters<ExecArg>,
    ) -> Result<Json<ExecutionEventOutput>, CallToolResult> {
        let exec_id = parse(&execution.exec_id, "execution id")?;
        let service = service(&self.daemon, &execution.host)?;
        let next = service
            .next_ready(exec_id)
            .await
            .map_err(|error| err(format!("Host '{}': {error}", execution.host.id)))?;
        let event = match next {
            None => ExecutionEvent::Waiting,
            Some(NextEvent::Callout {
                pending_id,
                callout_index,
                name,
                prompt,
                schema,
                context,
            }) => ExecutionEvent::Callout {
                pending_id,
                callout_index,
                name,
                prompt,
                schema: serialized_value(&schema)?,
                context,
            },
            Some(NextEvent::Completed {
                session_id,
                outcome,
            }) => ExecutionEvent::Completed {
                session: session_ref(&execution.host, session_id),
                outcome,
            },
            Some(NextEvent::Failed { reason }) => ExecutionEvent::Failed { reason },
        };
        Ok(Json(ExecutionEventOutput { event }))
    }

    #[tool(
        description = "Answer the pending callout returned by await_execution_event. The answer must match that event's inline JSON Schema.",
        output_schema = output_schema::<AckOutput>(),
        annotations(title = "Answer callout", read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn answer_callout(
        &self,
        Parameters(arg): Parameters<AnswerCalloutArg>,
    ) -> Result<Json<AckOutput>, CallToolResult> {
        let exec_id = parse(&arg.execution.exec_id, "execution id")?;
        match self
            .request(
                &arg.execution.host,
                HostRequest::ExecSubmit {
                    exec_id,
                    pending_id: arg.pending_id,
                    answer: arg.answer,
                },
            )
            .await?
        {
            ResponseOk::Ack => Ok(Json(AckOutput { ok: true })),
            other => Err(unexpected(&other)),
        }
    }

    #[tool(
        description = "Run a guest-defined read-only JSON query against an active execution. This does not advance program state.",
        output_schema = output_schema::<QueryExecutionOutput>(),
        annotations(title = "Query execution", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn query_execution(
        &self,
        Parameters(arg): Parameters<QueryExecutionArg>,
    ) -> Result<Json<QueryExecutionOutput>, CallToolResult> {
        let exec_id = parse(&arg.execution.exec_id, "execution id")?;
        match self
            .request(
                &arg.execution.host,
                HostRequest::ExecQuery {
                    exec_id,
                    query: arg.query,
                },
            )
            .await?
        {
            ResponseOk::Query { result } => Ok(Json(QueryExecutionOutput { result })),
            other => Err(unexpected(&other)),
        }
    }

    #[tool(
        description = "Stop this Host's participation. It withdraws during negotiation, terminates after activation, and is safe to repeat after success.",
        output_schema = output_schema::<AckOutput>(),
        annotations(title = "Stop execution", read_only_hint = false, destructive_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn stop_execution(
        &self,
        Parameters(arg): Parameters<StopExecutionArg>,
    ) -> Result<Json<AckOutput>, CallToolResult> {
        let exec_id = parse(&arg.execution.exec_id, "execution id")?;
        let service = service(&self.daemon, &arg.execution.host)?;
        let reason = arg
            .reason
            .unwrap_or_else(|| "agent stopped execution".to_string());
        for _ in 0..4 {
            let status = match service.dispatch(HostRequest::ExecStatus { exec_id }).await {
                Ok(ResponseOk::Status(status)) => status,
                Ok(other) => return Err(unexpected(&other)),
                Err(error) => {
                    return Err(err(format!("Host '{}': {error}", arg.execution.host.id)));
                }
            };
            let lifecycle = status.lifecycle();
            let request = match status.state {
                ExecStatusState::Negotiating { .. } => HostRequest::ExecWithdraw { exec_id },
                ExecStatusState::Activating { .. } | ExecStatusState::Active { .. } => {
                    HostRequest::ExecTerminate {
                        exec_id,
                        reason: reason.clone(),
                    }
                }
                ExecStatusState::Completed { .. }
                | ExecStatusState::Aborted { .. }
                | ExecStatusState::Failed { .. } => return Ok(Json(AckOutput { ok: true })),
            };
            match service.dispatch(request).await {
                Ok(ResponseOk::Ack) => return Ok(Json(AckOutput { ok: true })),
                Ok(other) => return Err(unexpected(&other)),
                Err(action_error) => {
                    let refreshed = service.dispatch(HostRequest::ExecStatus { exec_id }).await;
                    match refreshed {
                        Ok(ResponseOk::Status(status)) if status.lifecycle().is_terminal() => {
                            return Ok(Json(AckOutput { ok: true }));
                        }
                        Ok(ResponseOk::Status(status)) if status.lifecycle() != lifecycle => {
                            continue;
                        }
                        _ => {
                            return Err(err(format!(
                                "Host '{}': {action_error}",
                                arg.execution.host.id
                            )));
                        }
                    }
                }
            }
        }
        Err(err(format!(
            "Host '{}': execution lifecycle did not settle while stopping",
            arg.execution.host.id
        )))
    }

    #[tool(
        description = "Verify the receipt produced by one Host and return recovered proof evidence. Full mode also replays the exact registered Wasm.",
        output_schema = output_schema::<VerifySessionOutput>(),
        annotations(title = "Verify session", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn verify_session(
        &self,
        Parameters(arg): Parameters<VerifySessionArg>,
    ) -> Result<Json<VerifySessionOutput>, CallToolResult> {
        let session_id: SessionHash = parse(&arg.session.session_id, "session id")?;
        match self
            .request(
                &arg.session.host,
                HostRequest::ReceiptVerify {
                    receipt: ReceiptRef::Produced(session_id),
                    full: matches!(arg.mode, VerificationMode::Full),
                },
            )
            .await?
        {
            ResponseOk::Verified {
                receipt_id: _,
                program_id,
                session_id,
                ensemble,
                steps,
                result,
            } => {
                let (mode, terminal) = match result {
                    VerifiedResult::Light { terminal } => ("light", serialized_value(&terminal)?),
                    VerifiedResult::Full { terminal } => ("full", serialized_value(&terminal)?),
                };
                Ok(Json(VerifySessionOutput {
                    mode: serialized_value(&mode)?,
                    session: session_ref(&arg.session.host, session_id),
                    program: ProgramRef {
                        host: arg.session.host,
                        program_id: program_id.to_string(),
                    },
                    participants: ensemble
                        .into_iter()
                        .map(|peer| self.participant(peer))
                        .collect(),
                    steps,
                    terminal,
                }))
            }
            other => Err(unexpected(&other)),
        }
    }
}

#[tool_handler]
impl ServerHandler for Arena0Mcp {
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let activity = self.daemon.activity();
        let call_id = activity.next_call_id();
        let tool = if self.tool_router.get(request.name.as_ref()).is_some() {
            request.name.to_string()
        } else {
            "unknown".to_owned()
        };
        let (host, exec_id) = activity_scope(
            &self.daemon,
            request.name.as_ref(),
            request.arguments.as_ref(),
        );
        let started = Instant::now();
        activity.emit(ActivityData::Started {
            call_id: call_id.clone(),
            tool,
            host,
            exec_id,
        });
        let mut activity_guard = ActivityGuard::new(activity, call_id, started);
        let call = self
            .tool_router
            .call(ToolCallContext::new(self, request, context));
        let mut shutdown = self.daemon.shutdown_receiver();
        let result = if *shutdown.borrow() {
            Err(rmcp::ErrorData::internal_error(
                "daemon is shutting down",
                None,
            ))
        } else {
            tokio::select! {
                result = call => result,
                changed = shutdown.changed() => {
                    if changed.is_ok() && *shutdown.borrow() {
                        Err(rmcp::ErrorData::internal_error(
                            "daemon is shutting down",
                            None,
                        ))
                    } else {
                        Err(rmcp::ErrorData::internal_error(
                            "daemon shutdown state was unavailable",
                            None,
                        ))
                    }
                }
            }
        };
        let activity_result = match &result {
            Ok(result) if result.is_error != Some(true) => ActivityResult::Ok,
            Ok(result) => ActivityResult::ToolError {
                code: result
                    .structured_content
                    .as_ref()
                    .and_then(activity_error_code),
            },
            Err(_) => ActivityResult::ToolError { code: None },
        };
        activity_guard.finish(activity_result);
        result
    }

    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.instructions = Some(
            "arena0d exposes one local Ensemble. Open your Host with open_host and retain its id across reconnects. Carry the \
             returned Host reference explicitly; there is no implicit current Host. \
             Choose a program with list_programs and inspect_program, start or join one execution, \
             then interleave each Host's await_execution_event calls and answer callouts immediately \
             until terminal; a waiting event means advance another Host. Verify a \
             completed SessionRef with verify_session. Program params, answers, queries, and \
             outcomes are JSON; never hand-encode bytes or sign anything."
                .into(),
        );
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }
}

pub(crate) async fn bind(config: &McpConfig) -> anyhow::Result<TcpListener> {
    Ok(TcpListener::bind(config.listen).await?)
}

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

struct McpListener {
    inner: TcpListener,
    shutdown: ConnectionShutdown,
}

impl McpListener {
    fn new(inner: TcpListener, shutdown: ConnectionShutdown) -> Self {
        Self { inner, shutdown }
    }
}

impl axum::serve::Listener for McpListener {
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
    config: McpConfig,
    listener: TcpListener,
    mut shutdown: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let startup = daemon.startup_timeline();
    let router = router(daemon, config.bearer_token);
    let address = match listener.local_addr() {
        Ok(address) => address,
        Err(error) => {
            startup::progress(StartupStage::Failed, &startup);
            return Err(error.into());
        }
    };
    tracing::info!(endpoint = %format_args!("http://{address}/mcp"), "arena0d MCP listening");
    let mut connection_shutdown = shutdown.clone();
    let connection_shutdown = async move {
        if !*connection_shutdown.borrow() {
            let _ = connection_shutdown.changed().await;
        }
    }
    .boxed()
    .shared();
    let listener = McpListener::new(listener, connection_shutdown);
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            if !*shutdown.borrow() {
                let _ = shutdown.changed().await;
            }
        })
        .await?;
    Ok(())
}

fn router(daemon: Arc<Daemon>, bearer_token: Option<String>) -> axum::Router {
    let shutdown = daemon.shutdown_receiver();
    let service: StreamableHttpService<Arena0Mcp, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(Arena0Mcp::new(Arc::clone(&daemon))),
        Default::default(),
        // Every tool is self-contained, so MCP session state adds nothing.
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true),
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let router = match bearer_token {
        Some(token) => router.layer(middleware::from_fn_with_state(
            Arc::<str>::from(token),
            require_bearer,
        )),
        None => router,
    };
    router.layer(middleware::from_fn_with_state(
        shutdown,
        cancel_inflight_request,
    ))
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

fn service(daemon: &Daemon, host: &HostRef) -> Result<Arc<HostService>, CallToolResult> {
    daemon.service(&host.id).ok_or_else(|| {
        let available = daemon
            .services()
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        err(format!(
            "unknown Host '{}'; available Hosts: {}",
            host.id,
            available.join(", ")
        ))
    })
}

fn peer_id(daemon: &Daemon, host: &HostRef) -> Result<PeerId, CallToolResult> {
    service(daemon, host).map(|service| service.peer_id())
}

fn exec_ref(host: &HostRef, exec_id: ExecId) -> ExecRef {
    ExecRef {
        host: host.clone(),
        exec_id: exec_id.to_string(),
    }
}

fn session_ref(host: &HostRef, session_id: SessionHash) -> SessionRef {
    SessionRef {
        host: host.clone(),
        session_id: session_id.to_string(),
    }
}

fn status_output(
    host: &HostRef,
    status: arena0_api::ExecStatus,
) -> Result<ExecutionStatusOutput, CallToolResult> {
    Ok(ExecutionStatusOutput {
        execution: exec_ref(host, status.exec_id),
        negotiation_id: status.negotiation_id.map(|id| id.to_string()),
        program: ProgramRef {
            host: host.clone(),
            program_id: status.program_id.to_string(),
        },
        state: serialized_value(&status.state)?,
    })
}

fn summary_output(host: &HostRef, summary: &ProgramSummary) -> ProgramSummaryOutput {
    ProgramSummaryOutput {
        program: ProgramRef {
            host: host.clone(),
            program_id: summary.program_hash.to_string(),
        },
        name: summary.name.clone(),
        display_name: summary.display_name.clone(),
        version: summary.version.clone(),
        description: summary.description.clone(),
        participants: summary.participants,
    }
}

fn serialized_value<T: serde::Serialize>(value: &T) -> Result<Value, CallToolResult> {
    serde_json::to_value(value).map_err(|error| err(format!("encode structured value: {error}")))
}

fn activity_scope(
    daemon: &Daemon,
    tool: &str,
    arguments: Option<&rmcp::model::JsonObject>,
) -> (Option<String>, Option<ExecId>) {
    let Some(arguments) = arguments else {
        return (None, None);
    };
    let host = match tool {
        "list_programs" => scoped_string(arguments, &["host", "id"]),
        "inspect_program" | "start_execution" => {
            scoped_string(arguments, &["program", "host", "id"])
        }
        "get_execution_status"
        | "await_execution_event"
        | "answer_callout"
        | "query_execution"
        | "stop_execution" => scoped_string(arguments, &["execution", "host", "id"]),
        "verify_session" => scoped_string(arguments, &["session", "host", "id"]),
        _ => None,
    };
    let host = host.filter(|name| daemon.service(name).is_some());
    let exec_id = match tool {
        "get_execution_status"
        | "await_execution_event"
        | "answer_callout"
        | "query_execution"
        | "stop_execution" => {
            scoped_string(arguments, &["execution", "exec_id"]).and_then(|value| value.parse().ok())
        }
        _ => None,
    };
    (host, exec_id)
}

fn scoped_string(arguments: &rmcp::model::JsonObject, path: &[&str]) -> Option<String> {
    let (first, rest) = path.split_first()?;
    let mut value = arguments.get(*first)?;
    for field in rest {
        value = value.get(*field)?;
    }
    value.as_str().map(str::to_owned)
}

fn activity_error_code(value: &Value) -> Option<ApiErrorCode> {
    value
        .get("code")
        .and_then(|code| serde_json::from_value(code.clone()).ok())
}

fn parse<T: std::str::FromStr>(value: &str, what: &str) -> Result<T, CallToolResult> {
    value
        .parse::<T>()
        .map_err(|_| err(format!("invalid {what}: {value}")))
}

fn err(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message.into())])
}

fn api_error(host: &str, error: ApiError) -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({
        "code": error.code,
        "message": format!("Host '{host}': {}", error.message),
    }))
}

fn unexpected(other: &ResponseOk) -> CallToolResult {
    err(format!("unexpected daemon response: {other:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_api::{ActivityFrame, Request, Response};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::Path;
    use std::time::Duration;

    use rmcp::model::{CallToolRequestParams, ClientInfo};
    use rmcp::transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    };
    use rmcp::{RoleClient, ServiceExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream, UnixStream};
    use tokio::time::timeout;

    struct TestDaemon {
        _homes: Vec<tempfile::TempDir>,
        daemon: Arc<Daemon>,
    }

    async fn test_daemon() -> TestDaemon {
        let home = tempfile::tempdir().expect("temporary daemon home");
        let mcp = McpConfig::new(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0), None)
            .expect("test MCP config");
        let dynamic_home = arena0_home::Home::from_root(home.path().to_path_buf()).unwrap();
        TestDaemon {
            _homes: vec![home],
            daemon: Daemon::start(
                vec!["alice".parse().unwrap(), "bob".parse().unwrap()],
                mcp,
                Arc::new(WasmtimeEngine::new().expect("sandbox engine")),
                dynamic_home,
                true,
            )
            .await
            .expect("start daemon"),
        }
    }

    fn tool_input_schema(server: &Arena0Mcp, name: &str) -> Value {
        let tool = server
            .tool_router
            .list_all()
            .into_iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("missing tool {name}"));
        serde_json::to_value(&*tool.input_schema).expect("serialize input schema")
    }

    async fn call_mcp_tool(
        client: &rmcp::Peer<RoleClient>,
        name: &'static str,
        arguments: Value,
    ) -> Value {
        let arguments = serde_json::from_value(arguments).expect("MCP arguments must be an object");
        let result = timeout(
            Duration::from_secs(10),
            client.call_tool(CallToolRequestParams::new(name).with_arguments(arguments)),
        )
        .await
        .unwrap_or_else(|_| panic!("MCP tool '{name}' timed out"))
        .unwrap_or_else(|error| panic!("MCP tool '{name}' request failed: {error}"));
        assert_ne!(
            result.is_error,
            Some(true),
            "MCP tool '{name}' returned an error: {:?}",
            result.content
        );
        result
            .structured_content
            .unwrap_or_else(|| panic!("MCP tool '{name}' returned no structured content"))
    }

    async fn call_unix(socket: &Path, request: &Request) -> Response {
        let stream = UnixStream::connect(socket)
            .await
            .expect("connect Host socket");
        let (mut read, mut write) = stream.into_split();
        arena0_api::frame::write_frame(&mut write, request)
            .await
            .expect("write Host request");
        arena0_api::frame::read_frame::<_, Response>(&mut read)
            .await
            .expect("read Host response")
            .expect("Host response frame")
    }

    async fn wait_for_mcp_activation(
        client: &rmcp::Peer<RoleClient>,
        alice_exec: &Value,
        bob_exec: &Value,
    ) {
        for _ in 0..250 {
            let alice = call_mcp_tool(
                client,
                "get_execution_status",
                serde_json::json!({"execution": alice_exec}),
            )
            .await;
            let bob = call_mcp_tool(
                client,
                "get_execution_status",
                serde_json::json!({"execution": bob_exec}),
            )
            .await;
            if alice["state"]["exec_state"] == "Active" && bob["state"]["exec_state"] == "Active" {
                assert_eq!(
                    alice["state"]["session"]["session_id"], bob["state"]["session"]["session_id"],
                    "both Hosts activated the same SessionHash"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("MCP-driven executions did not activate");
    }

    async fn wait_for_mcp_address(daemon: &Daemon) -> SocketAddr {
        timeout(Duration::from_secs(10), async {
            loop {
                if let Some(address) = daemon.mcp_endpoint()
                    && address.port() != 0
                {
                    return address;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("MCP endpoint did not bind")
    }

    async fn poll_mcp_host(client: &rmcp::Peer<RoleClient>, execution: &Value) -> Option<Value> {
        let event = call_mcp_tool(
            client,
            "await_execution_event",
            serde_json::json!({"execution": execution}),
        )
        .await;
        match event["event"].as_str() {
            Some("waiting") => None,
            Some("callout") => {
                call_mcp_tool(
                    client,
                    "answer_callout",
                    serde_json::json!({
                        "execution": execution,
                        "pending_id": event["pending_id"],
                        "answer": "Rock"
                    }),
                )
                .await;
                None
            }
            Some("completed") => Some(event["session"].clone()),
            other => panic!("unexpected MCP execution event: {other:?} ({event})"),
        }
    }

    #[test]
    fn program_summary_output_preserves_participants() {
        let summary = ProgramSummary {
            program_hash: arena0_program::ProgramHash([1; 32]),
            name: "cumulative-sum".into(),
            display_name: "Cumulative sum".into(),
            version: "1.0.0".into(),
            description: "N-party".into(),
            participants: ParticipantCount::Range { min: 2, max: 64 },
        };
        let output = summary_output(
            &HostRef {
                id: "host-01".into(),
            },
            &summary,
        );
        let encoded = serde_json::to_value(output).expect("encode MCP program summary");
        assert_eq!(
            encoded["participants"],
            serde_json::json!({"kind": "range", "min": 2, "max": 64})
        );
    }

    #[tokio::test]
    async fn list_hosts_exposes_the_whole_ensemble_in_stable_order() {
        let test = test_daemon().await;
        let server = Arena0Mcp::new(Arc::clone(&test.daemon));
        let Json(output) = server
            .list_hosts(Parameters(EmptyArgs {}))
            .await
            .expect("list Hosts");
        assert_eq!(
            output
                .hosts
                .iter()
                .map(|host| host.host.id.as_str())
                .collect::<Vec<_>>(),
            ["alice", "bob"]
        );
        assert_ne!(output.hosts[0].peer_id, output.hosts[1].peer_id);
    }

    #[tokio::test]
    async fn tool_catalog_is_exact_stable_and_closed_world() {
        let test = test_daemon().await;
        let server = Arena0Mcp::new(test.daemon);
        let expected = [
            ("answer_callout", false, true, false),
            ("await_execution_event", true, false, true),
            ("get_execution_status", true, false, true),
            ("inspect_program", true, false, true),
            ("list_hosts", true, false, true),
            ("list_programs", true, false, true),
            ("open_host", false, false, false),
            ("query_execution", true, false, true),
            ("start_execution", false, false, false),
            ("stop_execution", false, true, true),
            ("verify_session", true, false, true),
        ];
        let mut tools = server.tool_router.list_all();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>(),
            expected.iter().map(|(name, ..)| *name).collect::<Vec<_>>()
        );
        for (tool, (_, read_only, destructive, idempotent)) in tools.iter().zip(expected) {
            assert!(tool.output_schema.is_some(), "{} output schema", tool.name);
            let annotations = tool.annotations.as_ref().expect("explicit annotations");
            assert_eq!(annotations.read_only_hint, Some(read_only), "{}", tool.name);
            assert_eq!(
                annotations.destructive_hint,
                Some(destructive),
                "{}",
                tool.name
            );
            assert_eq!(
                annotations.idempotent_hint,
                Some(idempotent),
                "{}",
                tool.name
            );
            assert_eq!(annotations.open_world_hint, Some(false), "{}", tool.name);
        }
        let list_hosts = tool_input_schema(&server, "list_hosts");
        assert_eq!(list_hosts["type"], "object");
        assert_eq!(list_hosts["additionalProperties"], false);
    }

    async fn post_mcp(address: SocketAddr, body: &str) -> String {
        let mut stream = TcpStream::connect(address)
            .await
            .expect("MCP test server should accept TCP connections");
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: {address}\r\nAccept: application/json, text/event-stream\r\nContent-Type: application/json\r\nMCP-Protocol-Version: 2025-03-26\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write MCP request");
        let mut response = Vec::new();
        timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
            .await
            .expect("MCP response should complete")
            .expect("read MCP response");
        String::from_utf8(response).expect("MCP response should be UTF-8")
    }

    fn response_body(response: &str) -> &str {
        response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .expect("HTTP response should contain a body")
    }

    #[tokio::test]
    async fn list_hosts_http_dispatch_rejects_unknown_arguments_but_accepts_empty() {
        let test = test_daemon().await;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind MCP test listener");
        let address = listener.local_addr().expect("MCP listener address");
        let daemon = Arc::clone(&test.daemon);
        let serving =
            tokio::spawn(async move { axum::serve(listener, router(daemon, None)).await });

        let omitted = post_mcp(
            address,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_hosts"}}"#,
        )
        .await;
        assert!(omitted.starts_with("HTTP/1.1 200"), "{omitted}");
        assert_eq!(
            serde_json::from_str::<Value>(response_body(&omitted)).expect("JSON result")["result"]
                ["isError"],
            false
        );

        let empty = post_mcp(
            address,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_hosts","arguments":{}}}"#,
        )
        .await;
        assert!(empty.starts_with("HTTP/1.1 200"), "{empty}");
        assert_eq!(
            serde_json::from_str::<Value>(response_body(&empty)).expect("JSON result")["result"]["isError"],
            false
        );

        let rejected = post_mcp(
            address,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_hosts","arguments":{"extra":true}}}"#,
        )
        .await;
        assert!(rejected.starts_with("HTTP/1.1 200"), "{rejected}");
        let rejected: Value = serde_json::from_str(response_body(&rejected)).expect("JSON result");
        assert_eq!(rejected["result"]["isError"], true);
        let error_text = rejected["result"]["content"][0]["text"]
            .as_str()
            .expect("tool error text");
        assert!(error_text.contains("failed to deserialize parameters"));
        assert!(error_text.contains("unknown field `extra`"));

        serving.abort();
        serving
            .await
            .expect_err("aborted test server should report cancellation");
        test.daemon.stop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn activity_stream_correlation_survives_duplicate_mcp_request_ids() {
        let test = test_daemon().await;
        let socket = test._homes[0].path().join("arena0.sock");
        let daemon_task = tokio::spawn(Arc::clone(&test.daemon).serve());
        for _ in 0..100 {
            if UnixStream::connect(&socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let mut activity = subscribe_activity(&socket).await;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind MCP test listener");
        let address = listener.local_addr().expect("MCP listener address");
        let daemon = Arc::clone(&test.daemon);
        let serving =
            tokio::spawn(async move { axum::serve(listener, router(daemon, None)).await });

        let (first, second) = tokio::join!(
            post_mcp(
                address,
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_hosts","arguments":{}}}"#,
            ),
            post_mcp(
                address,
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_hosts","arguments":{}}}"#,
            ),
        );
        assert!(first.starts_with("HTTP/1.1 200"), "{first}");
        assert!(second.starts_with("HTTP/1.1 200"), "{second}");

        let started_one = next_activity(&mut activity).await;
        let finished_one = next_activity(&mut activity).await;
        let started_two = next_activity(&mut activity).await;
        let finished_two = next_activity(&mut activity).await;
        let mut starts = Vec::new();
        let mut finishes = Vec::new();
        for frame in [started_one, finished_one, started_two, finished_two] {
            match frame.data {
                ActivityData::Started {
                    call_id,
                    tool,
                    host,
                    exec_id,
                } => {
                    assert_eq!(tool, "list_hosts");
                    assert_eq!(host, None);
                    assert_eq!(exec_id, None);
                    starts.push(call_id);
                }
                ActivityData::Finished {
                    call_id,
                    result: ActivityResult::Ok,
                    ..
                } => finishes.push(call_id),
                other => panic!("unexpected activity frame: {other:?}"),
            }
        }
        assert_eq!(starts.len(), 2);
        assert_eq!(finishes.len(), 2);
        assert_ne!(starts[0], starts[1], "daemon activity ids are unique");
        assert_eq!(
            starts.into_iter().collect::<BTreeSet<_>>(),
            finishes.into_iter().collect::<BTreeSet<_>>(),
            "each finished frame closes one started call"
        );

        serving.abort();
        let _ = serving.await;
        test.daemon.stop().await;
        daemon_task
            .await
            .expect("daemon task joined")
            .expect("daemon served");
    }

    #[tokio::test]
    async fn activity_scope_uses_only_public_reference_paths() {
        let test = test_daemon().await;
        let public_exec = ExecId([0x11; 32]);
        let private_exec = ExecId([0x22; 32]).to_string();
        let cases = [
            (
                "list_hosts",
                serde_json::json!({
                    "host": {"id": "alice"},
                    "exec_id": public_exec.to_string(),
                }),
                None,
                None,
            ),
            (
                "list_programs",
                serde_json::json!({
                    "host": {"id": "alice"},
                    "params": {"host": {"id": "bob"}, "exec_id": private_exec},
                }),
                Some("alice"),
                None,
            ),
            (
                "inspect_program",
                serde_json::json!({
                    "program": {"host": {"id": "alice"}},
                    "params": {"host": {"id": "bob"}, "exec_id": private_exec},
                }),
                Some("alice"),
                None,
            ),
            (
                "start_execution",
                serde_json::json!({
                    "program": {"host": {"id": "alice"}},
                    "params": {"host": {"id": "bob"}, "exec_id": private_exec},
                    "ensemble": {"mode": "explicit", "hosts": [{"id": "bob"}]},
                }),
                Some("alice"),
                None,
            ),
            (
                "get_execution_status",
                serde_json::json!({
                    "execution": {"host": {"id": "alice"}, "exec_id": public_exec},
                    "params": {"host": {"id": "bob"}, "exec_id": private_exec},
                }),
                Some("alice"),
                Some(public_exec),
            ),
            (
                "await_execution_event",
                serde_json::json!({
                    "execution": {"host": {"id": "alice"}, "exec_id": public_exec},
                    "params": {"host": {"id": "bob"}, "exec_id": private_exec},
                }),
                Some("alice"),
                Some(public_exec),
            ),
            (
                "answer_callout",
                serde_json::json!({
                    "execution": {"host": {"id": "alice"}, "exec_id": public_exec},
                    "answer": {"host": {"id": "bob"}, "exec_id": private_exec},
                }),
                Some("alice"),
                Some(public_exec),
            ),
            (
                "query_execution",
                serde_json::json!({
                    "execution": {"host": {"id": "alice"}, "exec_id": public_exec},
                    "query": {"host": {"id": "bob"}, "exec_id": private_exec},
                }),
                Some("alice"),
                Some(public_exec),
            ),
            (
                "stop_execution",
                serde_json::json!({
                    "execution": {"host": {"id": "alice"}, "exec_id": public_exec},
                    "reason": "stop",
                    "params": {"host": {"id": "bob"}, "exec_id": private_exec},
                }),
                Some("alice"),
                Some(public_exec),
            ),
            (
                "verify_session",
                serde_json::json!({
                    "session": {"host": {"id": "alice"}, "session_id": "aa"},
                    "params": {"host": {"id": "bob"}, "exec_id": private_exec},
                }),
                Some("alice"),
                None,
            ),
            (
                "unknown_tool",
                serde_json::json!({
                    "host": {"id": "alice"},
                    "exec_id": public_exec.to_string(),
                }),
                None,
                None,
            ),
        ];

        for (tool, value, expected_host, expected_exec) in cases {
            let arguments = serde_json::from_value(value).expect("activity arguments object");
            assert_eq!(
                activity_scope(&test.daemon, tool, Some(&arguments)),
                (expected_host.map(str::to_owned), expected_exec),
                "activity scope for {tool}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn occupied_mcp_port_fails_before_unix_services_start() {
        let home = tempfile::tempdir().expect("temporary daemon home");
        let socket = home.path().join("arena0.sock");
        let occupied = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind occupied MCP port");
        let address = occupied.local_addr().expect("occupied MCP address");
        let mcp = McpConfig::new(address, None).expect("MCP config");
        let dynamic_home =
            arena0_home::Home::from_root(home.path().to_path_buf()).expect("dynamic Host home");
        let daemon = Daemon::start(
            vec![
                "occupied-port-a".parse().unwrap(),
                "occupied-port-b".parse().unwrap(),
            ],
            mcp,
            Arc::new(WasmtimeEngine::new().expect("sandbox engine")),
            dynamic_home,
            true,
        )
        .await
        .expect("start daemon");

        let result = timeout(Duration::from_secs(10), daemon.serve())
            .await
            .expect("serve should fail promptly");
        assert!(result.is_err(), "occupied MCP port must fail startup");
        assert!(
            !socket.exists(),
            "Unix service must not start after bind failure"
        );
        drop(occupied);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_drains_a_partial_mcp_body() {
        let test = test_daemon().await;
        let serving = tokio::spawn(Arc::clone(&test.daemon).serve());
        let address = wait_for_mcp_address(&test.daemon).await;
        let mut client = TcpStream::connect(address)
            .await
            .expect("connect MCP endpoint");
        client
            .write_all(
                format!(
                    "POST /mcp HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: 10000\r\nConnection: keep-alive\r\n\r\n{{}}"
                )
                .as_bytes(),
            )
            .await
            .expect("write partial MCP body");
        tokio::time::sleep(Duration::from_millis(100)).await;

        timeout(Duration::from_secs(5), test.daemon.stop())
            .await
            .expect("partial MCP body blocked daemon shutdown");
        timeout(Duration::from_secs(5), serving)
            .await
            .expect("serve task did not join after partial MCP body")
            .expect("serve task panicked")
            .expect("serve task failed");
    }

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
        let mut listener = McpListener::new(listener, shutdown);
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_drains_a_partial_mcp_header() {
        let test = test_daemon().await;
        let serving = tokio::spawn(Arc::clone(&test.daemon).serve());
        let address = wait_for_mcp_address(&test.daemon).await;
        let mut client = TcpStream::connect(address)
            .await
            .expect("connect MCP endpoint");
        client
            .write_all(
                format!("POST /mcp HTTP/1.1\r\nHost: {address}\r\nContent-Length: 10000\r\n")
                    .as_bytes(),
            )
            .await
            .expect("write partial MCP header");
        tokio::time::sleep(Duration::from_millis(100)).await;

        timeout(Duration::from_secs(5), test.daemon.stop())
            .await
            .expect("partial MCP header blocked daemon shutdown");
        timeout(Duration::from_secs(5), serving)
            .await
            .expect("serve task did not join after partial MCP header")
            .expect("serve task panicked")
            .expect("serve task failed");
    }

    struct ActivityTestSubscription {
        read: BufReader<tokio::net::unix::OwnedReadHalf>,
        _write: tokio::net::unix::OwnedWriteHalf,
    }

    async fn subscribe_activity(socket: &std::path::Path) -> ActivityTestSubscription {
        let stream = UnixStream::connect(socket)
            .await
            .expect("connect activity socket");
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        arena0_api::frame::write_frame(&mut write, &Request::ActivitySubscribe)
            .await
            .expect("write activity subscribe");
        assert!(matches!(
            arena0_api::frame::read_frame::<_, Response>(&mut read)
                .await
                .expect("read activity ack")
                .expect("activity ack frame"),
            Ok(ResponseOk::ActivitySubscribed)
        ));
        ActivityTestSubscription {
            read,
            _write: write,
        }
    }

    async fn next_activity(subscription: &mut ActivityTestSubscription) -> ActivityFrame {
        timeout(
            Duration::from_secs(5),
            arena0_api::frame::read_frame::<_, ActivityFrame>(&mut subscription.read),
        )
        .await
        .expect("activity frame timeout")
        .expect("read activity frame")
        .expect("activity stream closed")
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn one_mcp_client_drives_two_hosts_to_one_session() {
        timeout(Duration::from_secs(30), async {
            let test = test_daemon().await;
            let supervisor = tokio::spawn(Arc::clone(&test.daemon).serve());
            while tokio::net::UnixStream::connect(test._homes[0].path().join("arena0.sock"))
                .await
                .is_err()
            {
                tokio::task::yield_now().await;
            }
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind MCP test listener");
            let address = listener.local_addr().expect("MCP listener address");
            let daemon = Arc::clone(&test.daemon);
            let serving =
                tokio::spawn(async move { axum::serve(listener, router(daemon, None)).await });

            let transport = StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(format!("http://{address}/mcp")),
            );
            let client = ClientInfo::default()
                .serve(transport)
                .await
                .expect("initialize one MCP client");

            let opened_a = call_mcp_tool(
                &client,
                "open_host",
                serde_json::json!({
                    "user_agent": "claude-code/test"
                }),
            )
            .await;
            let opened_b = call_mcp_tool(
                &client,
                "open_host",
                serde_json::json!({
                    "id": "new-bob", "user_agent": "codex/test"
                }),
            )
            .await;
            let alice = opened_a["host"].clone();
            let bob = opened_b["host"].clone();
            assert_ne!(opened_a["peer_id"], opened_b["peer_id"]);
            let reopened = call_mcp_tool(
                &client,
                "open_host",
                serde_json::json!({
                    "id": bob["id"], "user_agent": "codex/new"
                }),
            )
            .await;
            assert_eq!(reopened["peer_id"], opened_b["peer_id"]);
            let hosts = call_mcp_tool(&client, "list_hosts", serde_json::json!({})).await;
            let hosts = hosts["hosts"].as_array().expect("Host list");
            assert_eq!(hosts.len(), 4);
            let row = hosts
                .iter()
                .find(|row| row["host"] == bob)
                .expect("new Host listed");
            assert_eq!(row["user_agent"], "codex/new");
            let service = test.daemon.service("new-bob").expect("ready Host");
            let ResponseOk::HostStatus(info) = service.dispatch(HostRequest::Info).await.unwrap()
            else {
                panic!("expected Host info");
            };
            assert_eq!(row["peer_id"], info.host.peer_id.to_string());
            assert_eq!(info.host.user_agent.as_deref(), Some("codex/new"));

            let daemon_socket = test._homes[0].path().join("arena0.sock");
            match call_unix(&daemon_socket, &Request::HostsList).await {
                Ok(ResponseOk::Hosts(hosts)) => {
                    assert_eq!(hosts.len(), 4, "Unix HostsList includes dynamic Hosts");
                    let bob_info = hosts
                        .iter()
                        .find(|host| host.host.id == "new-bob")
                        .expect("dynamically opened Host is visible");
                    assert_eq!(bob_info.host.user_agent.as_deref(), Some("codex/new"));
                    assert_eq!(
                        bob_info.host.peer_id.to_string(),
                        opened_b["peer_id"].as_str().unwrap()
                    );
                    assert!(
                        hosts
                            .iter()
                            .any(|host| host.host.id == alice["id"].as_str().unwrap())
                    );
                }
                other => panic!("unexpected Unix HostsList response: {other:?}"),
            }

            let alice_programs =
                call_mcp_tool(&client, "list_programs", serde_json::json!({"host": alice})).await;
            let bob_programs =
                call_mcp_tool(&client, "list_programs", serde_json::json!({"host": bob})).await;
            let rps = |programs: &Value| {
                programs["programs"]
                    .as_array()
                    .expect("program list")
                    .iter()
                    .find(|program| program["name"] == "rock-paper-scissors")
                    .expect("rock-paper-scissors program")["program"]
                    .clone()
            };
            let alice_program = rps(&alice_programs);
            let bob_program = rps(&bob_programs);
            assert_eq!(
                alice_program["program_id"], bob_program["program_id"],
                "both Hosts admitted the same Wasm"
            );

            let created = call_mcp_tool(
                &client,
                "start_execution",
                serde_json::json!({
                    "program": alice_program,
                    "params": null,
                    "ensemble": {"mode": "explicit", "hosts": [bob.clone()]}
                }),
            )
            .await;
            assert_eq!(created["state"], "Negotiating");
            assert!(created["session"].is_null());
            let negotiation_id = created["negotiation_id"].clone();
            let alice_exec = created["execution"].clone();
            assert_eq!(alice_exec["host"], alice);
            let waiting = call_mcp_tool(
                &client,
                "await_execution_event",
                serde_json::json!({"execution": alice_exec}),
            )
            .await;
            assert_eq!(waiting, serde_json::json!({"event": "waiting"}));
            let joined = call_mcp_tool(
                &client,
                "start_execution",
                serde_json::json!({
                    "program": bob_program,
                    "params": null,
                    "ensemble": {
                        "mode": "join",
                        "creator": alice,
                        "negotiation_id": negotiation_id
                    }
                }),
            )
            .await;
            let bob_exec = joined["execution"].clone();
            assert_eq!(bob_exec["host"], bob);

            wait_for_mcp_activation(&client, &alice_exec, &bob_exec).await;

            let mut alice_session = None;
            let mut bob_session = None;
            for _ in 0..100 {
                if alice_session.is_none() {
                    alice_session = poll_mcp_host(&client, &alice_exec).await;
                }
                if bob_session.is_none() {
                    bob_session = poll_mcp_host(&client, &bob_exec).await;
                }
                if alice_session.is_some() && bob_session.is_some() {
                    break;
                }
            }
            let alice_session = alice_session.expect("Alice MCP execution did not complete");
            let bob_session = bob_session.expect("Bob MCP execution did not complete");
            assert_eq!(
                alice_session["session_id"], bob_session["session_id"],
                "both Hosts completed the same SessionHash"
            );
            assert_eq!(alice_session["host"], alice);
            assert_eq!(bob_session["host"], bob);

            let alice_verified = call_mcp_tool(
                &client,
                "verify_session",
                serde_json::json!({"session": alice_session, "mode": "full"}),
            )
            .await;
            let bob_verified = call_mcp_tool(
                &client,
                "verify_session",
                serde_json::json!({"session": bob_session, "mode": "full"}),
            )
            .await;
            for verified in [&alice_verified, &bob_verified] {
                assert_eq!(verified["participants"].as_array().map(Vec::len), Some(2));
                assert!(verified["steps"].as_u64().is_some_and(|steps| steps > 0));
                assert!(verified["terminal"].get("Completed").is_some());
            }
            assert_eq!(
                alice_verified["session"]["session_id"],
                bob_verified["session"]["session_id"]
            );
            assert_eq!(alice_verified["session"]["host"], alice);
            assert_eq!(bob_verified["session"]["host"], bob);

            client.cancel().await.expect("close the one MCP client");
            serving.abort();
            serving
                .await
                .expect_err("aborted MCP server should report cancellation");
            test.daemon.stop().await;
            supervisor
                .await
                .expect("supervisor task")
                .expect("clean shutdown");
        })
        .await
        .expect("one-client MCP scenario exceeded 30 seconds");
    }

    #[tokio::test]
    async fn references_and_admission_are_structured() {
        let test = test_daemon().await;
        let server = Arena0Mcp::new(test.daemon);
        let start = tool_input_schema(&server, "start_execution");
        assert!(start["properties"]["program"]["$ref"].is_string());
        assert!(start["$defs"]["ProgramRef"]["properties"]["host"]["$ref"].is_string());
        let ensemble = &start["$defs"]["McpEnsemble"];
        let variants = ensemble["oneOf"].as_array().expect("ensemble variants");
        assert_eq!(variants.len(), 2);
        assert!(
            variants
                .iter()
                .all(|variant| variant["additionalProperties"] == false)
        );

        let status = tool_input_schema(&server, "get_execution_status");
        assert!(status["properties"]["execution"]["$ref"].is_string());
        let verify = tool_input_schema(&server, "verify_session");
        assert!(verify["properties"]["session"]["$ref"].is_string());
        let answer = tool_input_schema(&server, "answer_callout");
        assert_eq!(answer["properties"]["pending_id"]["type"], "string");
        assert_eq!(answer["properties"]["pending_id"]["pattern"], "^[0-9]+$");
    }

    #[test]
    fn bearer_comparison_rejects_prefixes_and_suffixes() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(!constant_time_eq(b"secret", b"secret2"));
        assert!(!constant_time_eq(b"secret", b"Secret"));
    }

    #[test]
    fn non_loopback_mcp_is_rejected() {
        let result = McpConfig::new("0.0.0.0:7330".parse().expect("socket address"), None);
        assert!(result.is_err());
    }
}
