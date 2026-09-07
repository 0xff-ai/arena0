//! One daemon-owned MCP surface for the complete local Ensemble.
//!
//! Tool calls are scoped by one daemon-issued JWT. The token resolves one
//! durable Host before a tool handler runs; wire references contain only
//! Host-local execution/program/session ids and public peer identities.

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
    ActivityData, ActivityResult, ApiError, ApiErrorCode, ColorDepth, EnsembleSpec,
    ExecStatusState, HostRequest, NextEvent, PendingId, ProgramSummary, ReceiptRef, ResponseOk,
    VerifiedResult,
};
use arena0_program::ParticipantCount;
use arena0_protocol::{ExecId, NegotiationTarget, PeerId, SessionHash};
#[cfg(test)]
use arena0_sandbox::WasmtimeEngine;
use axum::extract::{Request as HttpRequest, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response as HttpResponse};
use rmcp::handler::server::common::Extension;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{CallToolRequestParams, CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::{Deserialize, de::Deserializer};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::ensemble::{Daemon, McpConfig};
use crate::mcp_auth::{AuthError, VerifiedClaims};
use crate::server::HostService;
use crate::startup::{self, StartupStage};

/// The only Host selector available to authenticated MCP tools. It is kept
/// private to this crate and inserted into RMCP request extensions after the
/// central token check; no wire representation can construct it.
#[derive(Clone)]
pub(crate) struct AuthorizedHost {
    pub(crate) host_name: arena0_home::HostName,
    pub(crate) peer_id: PeerId,
    pub(crate) service: Arc<HostService>,
    pub(crate) claims: VerifiedClaims,
}

impl AuthorizedHost {
    pub(crate) fn new(
        host_name: arena0_home::HostName,
        peer_id: PeerId,
        service: Arc<HostService>,
        claims: VerifiedClaims,
    ) -> Self {
        Self {
            host_name,
            peer_id,
            service,
            claims,
        }
    }
}

#[derive(
    Clone, Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(deny_unknown_fields)]
struct ProgramRef {
    /// Exact program content id returned by `list_programs`.
    #[schemars(length(min = 1))]
    program_id: String,
}

#[derive(
    Clone, Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(deny_unknown_fields)]
struct ExecRef {
    /// Host-local execution id (64 lowercase hex characters).
    exec_id: String,
}

#[derive(
    Clone, Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema, PartialEq, Eq,
)]
#[serde(deny_unknown_fields)]
struct SessionRef {
    /// Cross-party session id (64 lowercase hex characters).
    session_id: String,
}

/// A bearer token carried by the MCP JSON boundary. Its serialized form is
/// the JWT string, while Debug is deliberately redacted so DTOs cannot leak
/// credentials through diagnostics.
#[derive(Clone, Eq, PartialEq, serde::Serialize, schemars::JsonSchema)]
#[serde(transparent)]
struct McpToken(String);

impl McpToken {
    fn into_raw(self) -> crate::mcp_auth::RawToken {
        crate::mcp_auth::RawToken::new(self.0)
    }

    fn from_raw(token: crate::mcp_auth::RawToken) -> Self {
        Self(token.as_str().to_owned())
    }
}

impl<'de> serde::Deserialize<'de> for McpToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self)
    }
}

impl std::fmt::Debug for McpToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted JWT>")
    }
}

fn deserialize_optional_token<'de, D>(deserializer: D) -> Result<Option<McpToken>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::String(value) => Ok(Some(McpToken(value))),
        Value::Null => Err(serde::de::Error::custom("token must be a string")),
        _ => Err(serde::de::Error::custom("token must be a string")),
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct HelloArg {
    /// Caller-reported harness and version, for example claude-code/1.0.
    #[schemars(length(min = 1, max = 256))]
    user_agent: Option<String>,
    /// A previously issued token renews the same Host.
    #[serde(default, deserialize_with = "deserialize_optional_token")]
    token: Option<McpToken>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct HelloOutput {
    token: McpToken,
    peer_id: String,
    expires_at: u64,
    renew_after: u64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ProgramArg {
    #[serde(rename = "token")]
    #[schemars(rename = "token")]
    _token: McpToken,
    program: ProgramRef,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct TokenArg {
    #[serde(rename = "token")]
    #[schemars(rename = "token")]
    _token: McpToken,
}

/// Agent-facing admission choice. The authenticated Host is the owner; other
/// Participants are selected by their public peer identities.
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
enum McpEnsemble {
    /// Publish an offer and collect Participants. Fixed-size programs infer the count.
    Create {
        #[schemars(range(min = 2, max = 64))]
        participant_count: Option<u16>,
    },
    /// Create an offer for these exact other local peers.
    Explicit {
        #[schemars(length(min = 1))]
        peers: Vec<PeerId>,
    },
    /// Listen for a suitable offer, or join the specified negotiation.
    Join {
        target: Option<McpNegotiationTarget>,
    },
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct McpNegotiationTarget {
    creator: PeerId,
    #[schemars(length(min = 1))]
    negotiation_id: String,
}

impl McpEnsemble {
    async fn into_spec(
        self,
        server: &Arena0Mcp,
        owner: &AuthorizedHost,
        program_id: &str,
    ) -> Result<EnsembleSpec, CallToolResult> {
        let owner_peer = owner.peer_id;
        match self {
            Self::Create { participant_count } => {
                let participant_count = match participant_count {
                    Some(count) => count,
                    None => match server
                        .request(
                            owner,
                            HostRequest::ProgramGet {
                                program: program_id.to_owned(),
                            },
                        )
                        .await?
                    {
                        ResponseOk::Program(detail) => match detail.summary.participants {
                            ParticipantCount::Exact { count } => u16::from(count),
                            ParticipantCount::Range { .. } => {
                                return Err(err(
                                    "participant_count is required for a variable-size program",
                                ));
                            }
                        },
                        other => return Err(unexpected(&other)),
                    },
                };
                Ok(EnsembleSpec::Create { participant_count })
            }
            Self::Explicit { peers } => {
                if peers.is_empty() {
                    return Err(err("explicit ensemble needs at least one other peer"));
                }
                let mut seen = BTreeSet::new();
                for peer in &peers {
                    if *peer == owner_peer {
                        return Err(err("explicit ensemble must not include its owner peer"));
                    }
                    if !seen.insert(*peer) {
                        return Err(err("explicit ensemble contains a peer more than once"));
                    }
                }
                Ok(EnsembleSpec::Explicit { peers })
            }
            Self::Join { target: None } => Ok(EnsembleSpec::Join { target: None }),
            Self::Join {
                target:
                    Some(McpNegotiationTarget {
                        creator,
                        negotiation_id,
                    }),
            } => {
                if creator == owner_peer {
                    return Err(err("join creator must be a different peer"));
                }
                Ok(EnsembleSpec::Join {
                    target: Some(NegotiationTarget::new(
                        creator,
                        parse(&negotiation_id, "negotiation id")?,
                    )),
                })
            }
        }
    }
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StartExecutionArg {
    #[serde(rename = "token")]
    #[schemars(rename = "token")]
    _token: McpToken,
    program: ProgramRef,
    /// Program params as JSON, validated against the program's params schema.
    params: Option<Value>,
    ensemble: McpEnsemble,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExecArg {
    #[serde(rename = "token")]
    #[schemars(rename = "token")]
    _token: McpToken,
    execution: ExecRef,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct AwaitExecutionArg {
    #[serde(rename = "token")]
    #[schemars(rename = "token")]
    _token: McpToken,
    execution: ExecRef,
    /// Wait up to this many milliseconds. Omit or use zero for an immediate check.
    #[serde(default)]
    #[schemars(range(max = 20_000))]
    wait_ms: u64,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct AnswerCalloutArg {
    #[serde(rename = "token")]
    #[schemars(rename = "token")]
    _token: McpToken,
    execution: ExecRef,
    /// Pending id returned by `await_execution_event`.
    pending_id: PendingId,
    /// JSON answer in the shape of the callout's inline schema.
    answer: Option<Value>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct QueryExecutionArg {
    #[serde(rename = "token")]
    #[schemars(rename = "token")]
    _token: McpToken,
    execution: ExecRef,
    /// JSON query validated against the program's query request schema.
    query: Option<Value>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct StopExecutionArg {
    #[serde(rename = "token")]
    #[schemars(rename = "token")]
    _token: McpToken,
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
    #[serde(rename = "token")]
    #[schemars(rename = "token")]
    _token: McpToken,
    session: SessionRef,
    /// `light` checks portable proof evidence; `full` also replays the exact Wasm.
    #[serde(default)]
    mode: VerificationMode,
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
    negotiation_id: Option<String>,
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
struct ExecutionListOutput {
    executions: Vec<ExecutionStatusOutput>,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct ExecutionViewOutput {
    step: u64,
    view: Value,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct QueryExecutionOutput {
    result: Value,
}

#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
struct ParticipantOutput {
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
        authorized: &AuthorizedHost,
        request: HostRequest,
    ) -> Result<ResponseOk, CallToolResult> {
        authorized
            .service
            .dispatch(request)
            .await
            .map_err(api_error)
    }

    fn participant(&self, peer_id: PeerId) -> ParticipantOutput {
        ParticipantOutput {
            peer_id: peer_id.to_string(),
        }
    }

    fn issue_hello(
        &self,
        authorized: &AuthorizedHost,
    ) -> Result<Json<HelloOutput>, CallToolResult> {
        let issued = self
            .daemon
            .issue_token(authorized.claims.host_name(), authorized.claims.peer_id())
            .map_err(|_| err("could not issue MCP token"))?;
        Ok(Json(HelloOutput {
            token: McpToken::from_raw(issued.token),
            peer_id: issued.peer_id.to_string(),
            expires_at: issued.expires_at,
            renew_after: issued.renew_after,
        }))
    }
}

#[tool_router]
impl Arena0Mcp {
    #[tool(
        description = "Create a fresh authenticated Host with user_agent, or renew the same Host by supplying its token.",
        output_schema = output_schema::<HelloOutput>(),
        annotations(title = "Open Host", read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = false)
    )]
    async fn hello(
        &self,
        Parameters(arg): Parameters<HelloArg>,
    ) -> Result<Json<HelloOutput>, CallToolResult> {
        if let Some(token) = arg.token {
            if arg.user_agent.is_some() {
                return Err(err("hello accepts either token or user_agent, not both"));
            }
            let authorized = self
                .daemon
                .authorize_token(token.into_raw())
                .await
                .map_err(auth_error)?;
            return self.issue_hello(&authorized);
        }
        let user_agent = arg
            .user_agent
            .ok_or_else(|| err("hello requires user_agent or token"))?;
        let info = self
            .daemon
            .open_host(None, user_agent)
            .await
            .map_err(|_| err("Host unavailable"))?;
        let issued = self
            .daemon
            .issue_token(
                &info.id.parse().map_err(|_| err("Host unavailable"))?,
                info.peer_id,
            )
            .map_err(|_| err("could not issue MCP token"))?;
        Ok(Json(HelloOutput {
            token: McpToken::from_raw(issued.token),
            peer_id: issued.peer_id.to_string(),
            expires_at: issued.expires_at,
            renew_after: issued.renew_after,
        }))
    }

    #[tool(
        description = "List programs installed on one Host. Inspect a selected program before starting an execution.",
        output_schema = output_schema::<ProgramListOutput>(),
        annotations(title = "List programs", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn list_programs(
        &self,
        Extension(authorized): Extension<AuthorizedHost>,
        Parameters(_): Parameters<TokenArg>,
    ) -> Result<Json<ProgramListOutput>, CallToolResult> {
        match self.request(&authorized, HostRequest::ProgramList).await? {
            ResponseOk::ProgramList(programs) => Ok(Json(ProgramListOutput {
                programs: programs.iter().map(summary_output).collect(),
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
        Extension(authorized): Extension<AuthorizedHost>,
        Parameters(ProgramArg { program, .. }): Parameters<ProgramArg>,
    ) -> Result<Json<ProgramOutput>, CallToolResult> {
        match self
            .request(
                &authorized,
                HostRequest::ProgramGet {
                    program: program.program_id,
                },
            )
            .await?
        {
            ResponseOk::Program(detail) => Ok(Json(ProgramOutput {
                summary: summary_output(&detail.summary),
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
        Extension(authorized): Extension<AuthorizedHost>,
        Parameters(arg): Parameters<StartExecutionArg>,
    ) -> Result<Json<StartExecutionOutput>, CallToolResult> {
        let ensemble = arg
            .ensemble
            .into_spec(self, &authorized, &arg.program.program_id)
            .await?;
        let exec_id = ExecId(rand::random());
        let created = self
            .request(
                &authorized,
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
                        .request(&authorized, HostRequest::ExecCancelCreation { exec_id })
                        .await;
                    return match cleanup {
                        Ok(ResponseOk::Ack) => {
                            Err(err("execution id mismatch; requested id withdrawn"))
                        }
                        Ok(_) | Err(_) => Err(err("execution id mismatch; cleanup failed")),
                    };
                }
                Ok(Json(StartExecutionOutput {
                    execution: exec_ref(returned_exec_id),
                    negotiation_id: negotiation_id.map(|id| id.to_string()),
                    session: session_id.map(session_ref),
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
        Extension(authorized): Extension<AuthorizedHost>,
        Parameters(ExecArg { execution, .. }): Parameters<ExecArg>,
    ) -> Result<Json<ExecutionStatusOutput>, CallToolResult> {
        let exec_id = parse(&execution.exec_id, "execution id")?;
        match self
            .request(&authorized, HostRequest::ExecStatus { exec_id })
            .await?
        {
            ResponseOk::Status(status) => Ok(Json(status_output(status)?)),
            other => Err(unexpected(&other)),
        }
    }

    #[tool(
        description = "List this Host's executions to recover retained execution references after reconnecting.",
        output_schema = output_schema::<ExecutionListOutput>(),
        annotations(title = "List executions", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn list_executions(
        &self,
        Extension(authorized): Extension<AuthorizedHost>,
        Parameters(_): Parameters<TokenArg>,
    ) -> Result<Json<ExecutionListOutput>, CallToolResult> {
        match self.request(&authorized, HostRequest::ExecList).await? {
            ResponseOk::ExecList(statuses) => Ok(Json(ExecutionListOutput {
                executions: statuses
                    .into_iter()
                    .map(status_output)
                    .collect::<Result<_, _>>()?,
            })),
            other => Err(unexpected(&other)),
        }
    }

    #[tool(
        description = "Read the program-authored execution view, including its current board and move history when the program provides them.",
        output_schema = output_schema::<ExecutionViewOutput>(),
        annotations(title = "View execution", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn view_execution(
        &self,
        Extension(authorized): Extension<AuthorizedHost>,
        Parameters(ExecArg { execution, .. }): Parameters<ExecArg>,
    ) -> Result<Json<ExecutionViewOutput>, CallToolResult> {
        let exec = parse(&execution.exec_id, "execution id")?;
        match self
            .request(
                &authorized,
                HostRequest::ExecView {
                    exec,
                    width: 80,
                    color: ColorDepth::Mono,
                },
            )
            .await?
        {
            ResponseOk::ExecView { step, view } => Ok(Json(ExecutionViewOutput {
                step,
                view: serialized_value(&view)?,
            })),
            other => Err(unexpected(&other)),
        }
    }

    #[tool(
        description = "Return a callout, completion, failure, or waiting. Set wait_ms to 20000 to wait while another Participant acts; zero or omitted checks immediately. Repeating the call does not consume a pending result.",
        output_schema = output_schema::<ExecutionEventOutput>(),
        annotations(title = "Await execution event", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn await_execution_event(
        &self,
        Extension(authorized): Extension<AuthorizedHost>,
        Parameters(AwaitExecutionArg {
            execution, wait_ms, ..
        }): Parameters<AwaitExecutionArg>,
    ) -> Result<Json<ExecutionEventOutput>, CallToolResult> {
        if wait_ms > 20_000 {
            return Err(err("wait_ms must be at most 20000"));
        }
        let exec_id = parse(&execution.exec_id, "execution id")?;
        let service = &authorized.service;
        let next = if wait_ms == 0 {
            service
                .next_ready(exec_id)
                .await
                .map_err(|_| err("execution unavailable"))?
        } else {
            match tokio::time::timeout(
                std::time::Duration::from_millis(wait_ms),
                self.request(&authorized, HostRequest::ExecNext { exec_id }),
            )
            .await
            {
                Ok(Ok(ResponseOk::Next(event))) => Some(event),
                Ok(Ok(other)) => return Err(unexpected(&other)),
                Ok(Err(error)) => return Err(error),
                Err(_) => None,
            }
        };
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
                session: session_ref(session_id),
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
        Extension(authorized): Extension<AuthorizedHost>,
        Parameters(arg): Parameters<AnswerCalloutArg>,
    ) -> Result<Json<AckOutput>, CallToolResult> {
        let exec_id = parse(&arg.execution.exec_id, "execution id")?;
        match self
            .request(
                &authorized,
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
        Extension(authorized): Extension<AuthorizedHost>,
        Parameters(arg): Parameters<QueryExecutionArg>,
    ) -> Result<Json<QueryExecutionOutput>, CallToolResult> {
        let exec_id = parse(&arg.execution.exec_id, "execution id")?;
        match self
            .request(
                &authorized,
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
        Extension(authorized): Extension<AuthorizedHost>,
        Parameters(arg): Parameters<StopExecutionArg>,
    ) -> Result<Json<AckOutput>, CallToolResult> {
        let exec_id = parse(&arg.execution.exec_id, "execution id")?;
        let service = &authorized.service;
        let reason = arg
            .reason
            .unwrap_or_else(|| "agent stopped execution".to_string());
        for _ in 0..4 {
            let status = match service.dispatch(HostRequest::ExecStatus { exec_id }).await {
                Ok(ResponseOk::Status(status)) => status,
                Ok(other) => return Err(unexpected(&other)),
                Err(error) => {
                    return Err(err(format!("execution unavailable: {error}")));
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
                            return Err(err(format!("execution action failed: {action_error}")));
                        }
                    }
                }
            }
        }
        Err(err("execution lifecycle did not settle while stopping"))
    }

    #[tool(
        description = "Verify the receipt produced by one Host and return recovered proof evidence. Full mode also replays the exact registered Wasm.",
        output_schema = output_schema::<VerifySessionOutput>(),
        annotations(title = "Verify session", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn verify_session(
        &self,
        Extension(authorized): Extension<AuthorizedHost>,
        Parameters(arg): Parameters<VerifySessionArg>,
    ) -> Result<Json<VerifySessionOutput>, CallToolResult> {
        let session_id: SessionHash = parse(&arg.session.session_id, "session id")?;
        match self
            .request(
                &authorized,
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
                    session: session_ref(session_id),
                    program: ProgramRef {
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
        let authorization = if tool == "hello" || tool == "unknown" {
            Ok(None)
        } else {
            let token = request
                .arguments
                .as_ref()
                .and_then(|arguments| arguments.get("token"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            match token {
                Some(token) => self
                    .daemon
                    .authorize_token(crate::mcp_auth::RawToken::new(token))
                    .await
                    .map(Some)
                    .map_err(auth_error),
                None => Err(token_required()),
            }
        };
        let authorized = authorization.as_ref().ok().and_then(Option::as_ref);
        let (host, exec_id) = activity_scope(
            authorized,
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
        let mut shutdown = self.daemon.shutdown_receiver();
        let result = if let Err(error) = authorization {
            Ok(error)
        } else if *shutdown.borrow() {
            Err(rmcp::ErrorData::internal_error(
                "daemon is shutting down",
                None,
            ))
        } else {
            let mut context = context;
            if let Ok(Some(authorized)) = authorization.as_ref() {
                context.extensions.insert(authorized.clone());
            }
            let call = self
                .tool_router
                .call(ToolCallContext::new(self, request, context));
            tokio::select! {
                result = call => result,
                changed = shutdown.changed() => {
                    if changed.is_ok() && *shutdown.borrow() {
                        Err(rmcp::ErrorData::internal_error("daemon is shutting down", None))
                    } else {
                        Err(rmcp::ErrorData::internal_error("daemon shutdown state was unavailable", None))
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
            "When asked to play, drive the interaction to completion. Call hello with user_agent once and retain its token; renew it with hello(token) when needed. \
             Every other tool requires that token as a top-level argument. Inspect the program, then start_execution with ensemble mode create to publish an Offer or join to wait for one. \
             Keep calling await_execution_event with wait_ms:20000 and answer each callout without asking approval for routine moves. \
             Waiting means continue. Use view_execution to read the program state. \
             Verify completed sessions. Respect requests to stop. \
             Use list_executions after an uncertain creation response; do not duplicate games. \
             When driving multiple Participants, interleave calls with wait_ms:0. \
             Program params, answers, queries, and outcomes are JSON; never hand-encode bytes or sign anything."
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

fn exec_ref(exec_id: ExecId) -> ExecRef {
    ExecRef {
        exec_id: exec_id.to_string(),
    }
}

fn session_ref(session_id: SessionHash) -> SessionRef {
    SessionRef {
        session_id: session_id.to_string(),
    }
}

fn status_output(status: arena0_api::ExecStatus) -> Result<ExecutionStatusOutput, CallToolResult> {
    Ok(ExecutionStatusOutput {
        execution: exec_ref(status.exec_id),
        negotiation_id: status.negotiation_id.map(|id| id.to_string()),
        program: ProgramRef {
            program_id: status.program_id.to_string(),
        },
        state: serialized_value(&status.state)?,
    })
}

fn summary_output(summary: &ProgramSummary) -> ProgramSummaryOutput {
    ProgramSummaryOutput {
        program: ProgramRef {
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
    authorized: Option<&AuthorizedHost>,
    tool: &str,
    arguments: Option<&rmcp::model::JsonObject>,
) -> (Option<String>, Option<ExecId>) {
    let host = authorized.map(|host| host.host_name.to_string());
    let Some(arguments) = arguments else {
        return (host, None);
    };
    let exec_id = match tool {
        "get_execution_status"
        | "view_execution"
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

fn token_required() -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({
        "code": "TokenRequired",
        "message": "MCP token required",
    }))
}

fn auth_error(error: AuthError) -> CallToolResult {
    let code = match error {
        AuthError::InvalidToken => "InvalidToken",
        AuthError::TokenExpired => "TokenExpired",
        AuthError::HostUnavailable => "HostUnavailable",
    };
    CallToolResult::structured_error(serde_json::json!({
        "code": code,
        "message": error.to_string(),
    }))
}

fn api_error(error: ApiError) -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({
        "code": error.code,
        "message": error.message,
    }))
}

fn unexpected(_other: &ResponseOk) -> CallToolResult {
    err("unexpected daemon response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use arena0_api::{ActivityFrame, Request, Response};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
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
        let result = call_mcp_tool_result(client, name, arguments).await;
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

    async fn call_mcp_tool_result(
        client: &rmcp::Peer<RoleClient>,
        name: &'static str,
        arguments: Value,
    ) -> CallToolResult {
        let allowed_wait = if name == "await_execution_event" {
            arguments
                .get("wait_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        } else {
            0
        };
        let arguments = serde_json::from_value(arguments).expect("MCP arguments must be an object");
        let result = timeout(
            Duration::from_secs(10) + Duration::from_millis(allowed_wait),
            client.call_tool(CallToolRequestParams::new(name).with_arguments(arguments)),
        )
        .await
        .unwrap_or_else(|_| panic!("MCP tool '{name}' timed out"))
        .unwrap_or_else(|error| panic!("MCP tool '{name}' request failed: {error}"));
        result
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
        let output = summary_output(&summary);
        let encoded = serde_json::to_value(output).expect("encode MCP program summary");
        assert_eq!(
            encoded["participants"],
            serde_json::json!({"kind": "range", "min": 2, "max": 64})
        );
    }

    #[tokio::test]
    async fn tool_catalog_is_exact_stable_and_closed_world() {
        let test = test_daemon().await;
        let server = Arena0Mcp::new(test.daemon);
        let expected = [
            ("answer_callout", false, true, false),
            ("await_execution_event", true, false, true),
            ("get_execution_status", true, false, true),
            ("hello", false, false, false),
            ("inspect_program", true, false, true),
            ("list_executions", true, false, true),
            ("list_programs", true, false, true),
            ("query_execution", true, false, true),
            ("start_execution", false, false, false),
            ("stop_execution", false, true, true),
            ("verify_session", true, false, true),
            ("view_execution", true, false, true),
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
            let schema = serde_json::to_value(&*tool.input_schema).expect("input schema");
            if tool.name != "hello" {
                assert!(
                    schema["required"]
                        .as_array()
                        .is_some_and(|required| { required.iter().any(|value| value == "token") })
                );
                assert!(schema["properties"].get("host").is_none());
            }
        }
        assert_eq!(tools.len(), 12);
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
    async fn hello_http_dispatch_requires_a_user_agent() {
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
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"hello","arguments":{}}}"#,
        )
        .await;
        assert!(omitted.starts_with("HTTP/1.1 200"), "{omitted}");
        assert_eq!(
            serde_json::from_str::<Value>(response_body(&omitted)).expect("JSON result")["result"]
                ["isError"],
            true
        );

        let rejected = post_mcp(
            address,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"hello","arguments":{"token":"bad","user_agent":"x"}}}"#,
        )
        .await;
        assert!(rejected.starts_with("HTTP/1.1 200"), "{rejected}");
        let rejected: Value = serde_json::from_str(response_body(&rejected)).expect("JSON result");
        assert_eq!(rejected["result"]["isError"], true);
        let error_text = rejected["result"]["content"][0]["text"]
            .as_str()
            .expect("tool error text");
        assert!(error_text.contains("either token or user_agent"));

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
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"hello","arguments":{"user_agent":"activity/1"}}}"#,
            ),
            post_mcp(
                address,
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"hello","arguments":{"user_agent":"activity/2"}}}"#,
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
                    assert_eq!(tool, "hello");
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

    #[test]
    fn activity_scope_without_authorization_ignores_wire_references() {
        let public_exec = ExecId([0x11; 32]);
        let arguments = serde_json::json!({
            "token": "redacted",
            "execution": {"host": {"id": "alice"}, "exec_id": public_exec},
            "params": {"host": {"id": "bob"}, "exec_id": "private"},
        });
        assert_eq!(
            activity_scope(None, "get_execution_status", arguments.as_object(),),
            (None, Some(public_exec))
        );
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
    async fn two_mcp_clients_are_scoped_by_hello_tokens() {
        let test = test_daemon().await;
        let supervisor = tokio::spawn(Arc::clone(&test.daemon).serve());
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind MCP test listener");
        let address = listener.local_addr().expect("MCP listener address");
        let daemon = Arc::clone(&test.daemon);
        let serving =
            tokio::spawn(async move { axum::serve(listener, router(daemon, None)).await });

        let client_a = ClientInfo::default()
            .serve(StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(format!("http://{address}/mcp")),
            ))
            .await
            .expect("initialize first MCP client");
        let client_b = ClientInfo::default()
            .serve(StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(format!("http://{address}/mcp")),
            ))
            .await
            .expect("initialize second MCP client");

        let hello_a = call_mcp_tool(
            &client_a,
            "hello",
            serde_json::json!({"user_agent": "client-a/1"}),
        )
        .await;
        let hello_b = call_mcp_tool(
            &client_b,
            "hello",
            serde_json::json!({"user_agent": "client-b/1"}),
        )
        .await;
        let token_a = hello_a["token"].as_str().expect("first JWT").to_owned();
        let token_b = hello_b["token"].as_str().expect("second JWT").to_owned();
        assert_ne!(hello_a["peer_id"], hello_b["peer_id"]);
        assert_ne!(token_a, token_b);

        let programs_a = call_mcp_tool(
            &client_a,
            "list_programs",
            serde_json::json!({"token": token_a}),
        )
        .await;
        let programs_b = call_mcp_tool(
            &client_b,
            "list_programs",
            serde_json::json!({"token": token_b}),
        )
        .await;
        let program_a = programs_a["programs"]
            .as_array()
            .and_then(|programs| programs.first())
            .expect("first client program")
            .clone();
        let program_b = programs_b["programs"]
            .as_array()
            .and_then(|programs| programs.first())
            .expect("second client program")
            .clone();
        assert_eq!(
            program_a["program"]["program_id"],
            program_b["program"]["program_id"]
        );

        let bob_execution = call_mcp_tool(
            &client_b,
            "start_execution",
            serde_json::json!({
                "token": token_b,
                "program": program_b["program"],
                "params": null,
                "ensemble": {"mode": "join"},
            }),
        )
        .await;
        let foreign = call_mcp_tool_result(
            &client_a,
            "get_execution_status",
            serde_json::json!({
                "token": token_a,
                "execution": bob_execution["execution"],
            }),
        )
        .await;
        assert_eq!(foreign.is_error, Some(true));
        let foreign_text = foreign
            .structured_content
            .as_ref()
            .and_then(|value| value.get("message"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(foreign_text.contains("execution"), "{foreign_text}");

        call_mcp_tool(
            &client_b,
            "stop_execution",
            serde_json::json!({
                "token": token_b,
                "execution": bob_execution["execution"],
            }),
        )
        .await;

        let renewed =
            call_mcp_tool(&client_a, "hello", serde_json::json!({"token": token_a})).await;
        assert_eq!(renewed["peer_id"], hello_a["peer_id"]);

        let before_invalid = test.daemon.services().len();
        let invalid = call_mcp_tool_result(
            &client_a,
            "hello",
            serde_json::json!({"token": null, "user_agent": "must-not-provision"}),
        )
        .await;
        assert_eq!(invalid.is_error, Some(true));
        assert_eq!(test.daemon.services().len(), before_invalid);

        client_a.cancel().await.expect("close first MCP client");
        client_b.cancel().await.expect("close second MCP client");
        serving.abort();
        let _ = serving.await;
        test.daemon.stop().await;
        supervisor
            .await
            .expect("supervisor task")
            .expect("clean shutdown");
    }

    #[tokio::test]
    async fn hello_reopens_only_the_original_identity_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let home = arena0_home::Home::from_root(directory.path().to_owned()).unwrap();
        let engine = Arc::new(WasmtimeEngine::new().unwrap());
        let mut token = String::new();
        let mut wrong_identity_token = String::new();
        let mut original_peer = Value::Null;
        let mut identity_index = std::path::PathBuf::new();

        for phase in 0..3 {
            let daemon = Daemon::start(
                vec![],
                McpConfig::new("127.0.0.1:0".parse().unwrap(), None).unwrap(),
                Arc::clone(&engine),
                home.clone(),
                true,
            )
            .await
            .unwrap();
            let supervisor = tokio::spawn(Arc::clone(&daemon).serve());
            let address = wait_for_mcp_address(&daemon).await;
            let client = ClientInfo::default()
                .serve(StreamableHttpClientTransport::from_config(
                    StreamableHttpClientTransportConfig::with_uri(format!("http://{address}/mcp")),
                ))
                .await
                .unwrap();
            assert!(daemon.services().is_empty());

            match phase {
                0 => {
                    let hello = call_mcp_tool(
                        &client,
                        "hello",
                        serde_json::json!({"user_agent": "restart-client/1"}),
                    )
                    .await;
                    token = hello["token"].as_str().unwrap().to_owned();
                    original_peer = hello["peer_id"].clone();
                    let info = daemon.services()[0].1.host_info();
                    let name = info.id.parse().unwrap();
                    identity_index = home.host(&name).state_dir().join("keys/index.json");
                    wrong_identity_token = daemon
                        .issue_token(&name, PeerId([9; 32]))
                        .unwrap()
                        .token
                        .as_str()
                        .to_owned();
                }
                1 => {
                    let rejected = call_mcp_tool_result(
                        &client,
                        "hello",
                        serde_json::json!({"token": wrong_identity_token}),
                    )
                    .await;
                    assert_eq!(rejected.is_error, Some(true));
                    assert!(daemon.services().is_empty());
                    let (first, second) = tokio::join!(
                        call_mcp_tool(&client, "hello", serde_json::json!({"token": token})),
                        call_mcp_tool(&client, "hello", serde_json::json!({"token": token})),
                    );
                    assert_eq!(first["peer_id"], original_peer);
                    assert_eq!(second["peer_id"], original_peer);
                    assert_eq!(daemon.services().len(), 1);
                    assert_eq!(
                        daemon.services()[0].1.host_info().user_agent.as_deref(),
                        Some("restart-client/1")
                    );
                    // Earlier tokens remain usable after renewal.
                    call_mcp_tool(
                        &client,
                        "list_executions",
                        serde_json::json!({"token": token}),
                    )
                    .await;
                    call_mcp_tool(
                        &client,
                        "list_executions",
                        serde_json::json!({"token": first["token"]}),
                    )
                    .await;
                }
                2 => {
                    let rejected =
                        call_mcp_tool_result(&client, "hello", serde_json::json!({"token": token}))
                            .await;
                    assert_eq!(rejected.is_error, Some(true));
                    assert!(daemon.services().is_empty());
                    assert!(!identity_index.exists(), "missing identity was recreated");
                }
                _ => unreachable!(),
            }
            client.cancel().await.unwrap();
            daemon.stop().await;
            supervisor.await.unwrap().unwrap();
            drop(daemon);
            if phase == 1 {
                std::fs::remove_file(&identity_index).unwrap();
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_mcp_clients_complete_and_verify_one_session() {
        run_token_admission_scenario(false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn open_join_waits_for_a_later_creator() {
        run_token_admission_scenario(true).await;
    }

    async fn run_token_admission_scenario(join_first: bool) {
        timeout(Duration::from_secs(80), async {
            let test = test_daemon().await;
            let supervisor = tokio::spawn(Arc::clone(&test.daemon).serve());
            let address = wait_for_mcp_address(&test.daemon).await;
            let mut clients = Vec::new();
            let mut tokens = Vec::new();
            let mut programs = Vec::new();
            for user_agent in ["creator/1", "joiner/1"] {
                let client = ClientInfo::default().serve(StreamableHttpClientTransport::from_config(
                    StreamableHttpClientTransportConfig::with_uri(format!("http://{address}/mcp")),
                )).await.unwrap();
                let hello = call_mcp_tool(&client, "hello", serde_json::json!({"user_agent": user_agent})).await;
                let token = hello["token"].clone();
                let catalog = call_mcp_tool(&client, "list_programs", serde_json::json!({"token": token})).await;
                programs.push(catalog["programs"].as_array().unwrap().iter()
                    .find(|program| program["name"] == "rock-paper-scissors").unwrap()["program"].clone());
                tokens.push(token);
                clients.push(client);
            }
            assert_eq!(programs[0], programs[1]);
            let start_join = || call_mcp_tool(&clients[1], "start_execution", serde_json::json!({
                "token": tokens[1], "program": programs[1], "params": null, "ensemble": {"mode": "join"}
            }));
            let cancelled = start_join().await;
            assert!(cancelled["negotiation_id"].is_null());
            timeout(Duration::from_secs(2), call_mcp_tool(&clients[1], "stop_execution", serde_json::json!({
                "token": tokens[1], "execution": cancelled["execution"]
            }))).await.expect("unbound Join cancels promptly");
            let early_join = if join_first {
                let joined = start_join().await;
                assert!(joined["negotiation_id"].is_null());
                let waiting = call_mcp_tool(&clients[1], "await_execution_event", serde_json::json!({
                    "token": tokens[1], "execution": joined["execution"], "wait_ms": 30
                })).await;
                assert_eq!(waiting, serde_json::json!({"event": "waiting"}));
                Some(joined)
            } else { None };
            let created = call_mcp_tool(&clients[0], "start_execution", serde_json::json!({
                "token": tokens[0], "program": programs[0], "params": null, "ensemble": {"mode": "create"}
            })).await;
            assert_eq!(created["state"], "Negotiating");
            assert!(created["session"].is_null());
            assert!(created["negotiation_id"].is_string());
            if !join_first {
                // Preserve the existing regression check across the old 30s timeout.
                let started = Instant::now();
                for _ in 0..2 {
                    let waiting = call_mcp_tool(&clients[0], "await_execution_event", serde_json::json!({
                        "token": tokens[0], "execution": created["execution"], "wait_ms": 20000
                    })).await;
                    assert_eq!(waiting, serde_json::json!({"event": "waiting"}));
                }
                assert!(started.elapsed() >= Duration::from_secs(40));
            }
            let recovered = call_mcp_tool(&clients[0], "list_executions", serde_json::json!({"token": tokens[0]})).await;
            assert_eq!(recovered["executions"][0]["execution"], created["execution"]);
            let joined = if let Some(joined) = early_join { joined } else { start_join().await };
            let executions = [created["execution"].clone(), joined["execution"].clone()];
            let mut activated = false;
            for _ in 0..250 {
                let mut statuses = Vec::new();
                for index in 0..2 {
                    statuses.push(call_mcp_tool(&clients[index], "get_execution_status", serde_json::json!({
                        "token": tokens[index], "execution": executions[index]
                    })).await);
                }
                if statuses.iter().all(|status| status["state"]["exec_state"] == "Active") {
                    assert_eq!(
                        statuses[0]["state"]["session"]["session_id"],
                        statuses[1]["state"]["session"]["session_id"]
                    );
                    activated = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(activated, "MCP-driven executions did not activate");
            let mut sessions = [Value::Null, Value::Null];
            for _ in 0..100 {
                for index in 0..2 {
                    if !sessions[index].is_null() { continue; }
                    let event = call_mcp_tool(&clients[index], "await_execution_event", serde_json::json!({
                        "token": tokens[index], "execution": executions[index], "wait_ms": 100
                    })).await;
                    match event["event"].as_str() {
                        Some("waiting") => {},
                        Some("callout") => {
                            call_mcp_tool(&clients[index], "answer_callout", serde_json::json!({
                                "token": tokens[index], "execution": executions[index],
                                "pending_id": event["pending_id"], "answer": "Rock"
                            })).await;
                        }
                        Some("completed") => sessions[index] = event["session"].clone(),
                        _ => panic!("unexpected execution event"),
                    }
                }
                if sessions.iter().all(|session| !session.is_null()) { break; }
            }
            assert!(sessions.iter().all(|session| !session.is_null()), "MCP executions did not complete");
            assert_eq!(sessions[0], sessions[1]);
            for index in 0..2 {
                assert!(executions[index].get("host").is_none());
                assert!(sessions[index].get("host").is_none());
                let view = call_mcp_tool(&clients[index], "view_execution", serde_json::json!({
                    "token": tokens[index], "execution": executions[index]
                })).await;
                assert!(view["view"]["slots"].is_object());
                let verified = call_mcp_tool(&clients[index], "verify_session", serde_json::json!({
                    "token": tokens[index], "session": sessions[index], "mode": "full"
                })).await;
                assert_eq!(verified["session"], sessions[index]);
                let participants = verified["participants"].as_array().unwrap();
                assert_eq!(participants.len(), 2);
                assert!(participants.iter().all(|participant| participant.get("host").is_none()));
                assert!(verified["steps"].as_u64().is_some_and(|steps| steps > 0));
                assert!(verified["terminal"].get("Completed").is_some());
            }
            for client in clients { client.cancel().await.unwrap(); }
            test.daemon.stop().await;
            supervisor.await.unwrap().unwrap();
        }).await.expect("two-client MCP scenario exceeded 80 seconds");
    }

    #[tokio::test]
    async fn references_and_admission_are_structured() {
        let test = test_daemon().await;
        let server = Arena0Mcp::new(test.daemon);
        let start = tool_input_schema(&server, "start_execution");
        assert!(start["properties"]["program"]["$ref"].is_string());
        assert!(start["$defs"]["ProgramRef"]["properties"]["program_id"].is_object());
        assert!(
            start["$defs"]["ProgramRef"]["properties"]
                .get("host")
                .is_none()
        );
        let ensemble = &start["$defs"]["McpEnsemble"];
        let variants = ensemble["oneOf"].as_array().expect("ensemble variants");
        assert_eq!(variants.len(), 3);
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
