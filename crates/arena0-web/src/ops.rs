//! The operations the browser can call, mapped onto daemon requests.
//!
//! Every reply is JSON in the shape documented on the matching [`Op`]
//! variant. Daemon error codes map one to one onto [`ErrorCode`]; transport
//! and shape failures become `gateway` errors whose messages never contain
//! payloads.

use std::path::PathBuf;
use std::sync::Arc;

use arena0_api::{
    ApiError, ApiErrorCode, ColorDepth, EnsembleSpec, EventKind, HostRequest, ReceiptArtifact,
    ReceiptRef, ReceiptTermination, Request, ResponseOk,
};
use arena0_client::proto::{DaemonClient, is_connect_error};
use arena0_protocol::{
    BlobHash, Block, CalloutId, Cell, EffectKind, ExecId, NegotiationId, NegotiationTarget, PeerId,
    ReceiptId, Slot, Tone,
};
use base64::Engine as _;
use serde::Serialize;
use serde_json::{Value, json};

use crate::Launcher;
use crate::model::HostRow;
use crate::protocol::{
    BlobImported, BlockRow, CallResult, CellRow, CreatedReply, EffectKindRow, EffectRow, ErrorCode,
    ErrorRow, FactRow, Op, ProgramImported, RecordEventKind, RecordRow, RecordsReply,
    RosterEntryRow, Termination, ToneRow, VerifyReply, VerifyTarget, ViewReply,
};
use crate::replica::Replica;

const TERMINATE_REASON: &str = "terminated from the web UI";

pub(crate) struct Ops {
    client: DaemonClient,
    launcher: Arc<dyn Launcher>,
    replica: Replica,
}

fn gateway(message: &str) -> ErrorRow {
    ErrorRow {
        code: ErrorCode::Gateway,
        message: message.to_owned(),
    }
}

fn bad_request(message: &str) -> ErrorRow {
    ErrorRow {
        code: ErrorCode::BadRequest,
        message: message.to_owned(),
    }
}

fn api_error(error: ApiError) -> ErrorRow {
    let code = match error.code {
        ApiErrorCode::NotFound => ErrorCode::NotFound,
        ApiErrorCode::BadRequest => ErrorCode::BadRequest,
        ApiErrorCode::CalloutNotPending => ErrorCode::CalloutNotPending,
        ApiErrorCode::InputRejected => ErrorCode::InputRejected,
        ApiErrorCode::Ambiguous => ErrorCode::Ambiguous,
        ApiErrorCode::Schema => ErrorCode::Schema,
        ApiErrorCode::Negotiation => ErrorCode::Negotiation,
        ApiErrorCode::Execution => ErrorCode::Execution,
        ApiErrorCode::Verification => ErrorCode::Verification,
        ApiErrorCode::Storage => ErrorCode::Storage,
        ApiErrorCode::Timeout => ErrorCode::Timeout,
        ApiErrorCode::Internal => ErrorCode::Internal,
    };
    ErrorRow {
        code,
        message: error.message,
    }
}

fn transport_error(error: &anyhow::Error) -> ErrorRow {
    if is_connect_error(error) {
        gateway("daemon unreachable")
    } else {
        gateway("daemon request failed")
    }
}

fn parse<T: std::str::FromStr>(text: &str, what: &str) -> Result<T, ErrorRow> {
    text.parse()
        .map_err(|_| bad_request(&format!("invalid {what}")))
}

fn reply<T: Serialize>(value: &T) -> Result<Value, ErrorRow> {
    serde_json::to_value(value).map_err(|_| gateway("reply could not be encoded"))
}

fn unexpected() -> ErrorRow {
    gateway("daemon replied with an unexpected shape")
}

impl Ops {
    pub(crate) fn new(client: DaemonClient, launcher: Arc<dyn Launcher>, replica: Replica) -> Self {
        Self {
            client,
            launcher,
            replica,
        }
    }

    pub(crate) async fn call(&self, op: Op) -> CallResult {
        match self.run(op).await {
            Ok(value) => CallResult::Ok(value),
            Err(error) => CallResult::Err(error),
        }
    }

    async fn host(&self, host: &str, request: HostRequest) -> Result<ResponseOk, ErrorRow> {
        self.client
            .call_raw(&Request::Host {
                host: host.to_owned(),
                request,
            })
            .await
            .map_err(|error| transport_error(&error))?
            .map_err(api_error)
    }

    async fn daemon(&self, request: Request) -> Result<ResponseOk, ErrorRow> {
        self.client
            .call_raw(&request)
            .await
            .map_err(|error| transport_error(&error))?
            .map_err(api_error)
    }

    #[allow(clippy::too_many_lines)]
    async fn run(&self, op: Op) -> Result<Value, ErrorRow> {
        match op {
            Op::View {
                host,
                exec_id,
                width,
                at_step,
            } => {
                let exec = parse::<ExecId>(&exec_id, "exec id")?;
                let response = self
                    .host(
                        &host,
                        HostRequest::ExecView {
                            exec,
                            width,
                            color: ColorDepth::TrueColor,
                            at_step,
                        },
                    )
                    .await?;
                let ResponseOk::ExecView { step, view } = response else {
                    return Err(unexpected());
                };
                reply(&ViewReply {
                    step,
                    header: view.slots.get(&Slot::Header).cloned(),
                    agents: view.slots.get(&Slot::Agents).cloned(),
                    state: view.slots.get(&Slot::State).cloned(),
                    status_bar: view.slots.get(&Slot::StatusBar).cloned(),
                    blocks: view.blocks.into_iter().map(block_row).collect(),
                })
            }
            Op::Records {
                host,
                exec_id,
                from,
                limit,
            } => {
                let exec_id = parse::<ExecId>(&exec_id, "exec id")?;
                let response = self
                    .host(
                        &host,
                        HostRequest::ExecInspect {
                            exec_id,
                            events_from: from,
                            events_limit: limit,
                        },
                    )
                    .await?;
                let ResponseOk::Inspection(inspection) = response else {
                    return Err(unexpected());
                };
                reply(&RecordsReply {
                    from: inspection.events_from,
                    total: inspection.events_total,
                    next: inspection.events_next,
                    records: inspection
                        .events
                        .iter()
                        .map(|record| RecordRow {
                            position: record.event_position,
                            steps: record.agreed_steps.clone(),
                            event: match record.event {
                                EventKind::SessionStarted => RecordEventKind::SessionStarted,
                                EventKind::MessageReceived => RecordEventKind::MessageReceived,
                                EventKind::InputReceived => RecordEventKind::InputReceived,
                                EventKind::TimerFired => RecordEventKind::TimerFired,
                                EventKind::DirectReceived => RecordEventKind::DirectReceived,
                            },
                            input_bytes: record.input_payload_bytes,
                            effects: record
                                .effects
                                .iter()
                                .map(|effect| EffectRow {
                                    kind: match effect.kind {
                                        EffectKind::SessionEnd => EffectKindRow::SessionEnd,
                                        EffectKind::SessionAbort => EffectKindRow::SessionAbort,
                                        EffectKind::Broadcast => EffectKindRow::Broadcast,
                                        EffectKind::SetTimer => EffectKindRow::SetTimer,
                                        EffectKind::Fail => EffectKindRow::Fail,
                                        EffectKind::SendDirect => EffectKindRow::SendDirect,
                                    },
                                    bytes: effect.payload_bytes,
                                })
                                .collect(),
                        })
                        .collect(),
                })
            }
            Op::Query {
                host,
                exec_id,
                query,
            } => {
                let exec_id = parse::<ExecId>(&exec_id, "exec id")?;
                let response = self
                    .host(
                        &host,
                        HostRequest::ExecQuery {
                            exec_id,
                            query: Some(query),
                        },
                    )
                    .await?;
                let ResponseOk::Query { result } = response else {
                    return Err(unexpected());
                };
                Ok(result)
            }
            Op::Receipt { host, receipt_id } => {
                let id = parse::<ReceiptId>(&receipt_id, "receipt id")?;
                let response = self
                    .host(
                        &host,
                        HostRequest::ReceiptGet {
                            receipt: ReceiptRef::Stored(id),
                        },
                    )
                    .await?;
                let ResponseOk::Receipt(artifact) = response else {
                    return Err(unexpected());
                };
                reply(&*artifact)
            }
            Op::Verify { host, target } => {
                let receipt = match target {
                    VerifyTarget::Stored { receipt_id } => {
                        ReceiptRef::Stored(parse::<ReceiptId>(&receipt_id, "receipt id")?)
                    }
                    VerifyTarget::Inline { artifact } => {
                        ReceiptRef::Inline(Box::new(parse_artifact(artifact)?))
                    }
                };
                let response = self
                    .host(&host, HostRequest::ReceiptVerify { receipt })
                    .await?;
                let ResponseOk::Verified(summary) = response else {
                    return Err(unexpected());
                };
                reply(&VerifyReply {
                    receipt_id: summary.receipt_id.to_string(),
                    program: summary.program_id.to_string(),
                    session_id: summary.session_id.to_string(),
                    ensemble: summary.ensemble.iter().map(ToString::to_string).collect(),
                    steps: summary.steps,
                    termination: match &summary.terminal {
                        ReceiptTermination::Completed => Termination::Completed,
                        ReceiptTermination::Stopped { cause } => Termination::Stopped {
                            cause: format!(
                                "{} at step {}: {}",
                                match cause.kind() {
                                    arena0_protocol::AbortKind::Abort => "aborted",
                                    arena0_protocol::AbortKind::Fail => "failed",
                                },
                                cause.step(),
                                cause.reason()
                            ),
                        },
                    },
                })
            }
            Op::Answer {
                host,
                exec_id,
                pending_id,
                answer,
            } => {
                let exec_id = parse::<ExecId>(&exec_id, "exec id")?;
                let pending_id = parse::<CalloutId>(&pending_id, "pending id")?;
                self.host(
                    &host,
                    HostRequest::ExecSubmit {
                        exec_id,
                        pending_id,
                        answer: Some(answer),
                    },
                )
                .await?;
                Ok(Value::Null)
            }
            Op::Create {
                host,
                program,
                params,
                participants,
                blobs,
            } => {
                let exec_id = ExecId(rand::random());
                let blobs = parse_blobs(&blobs)?;
                self.host(
                    &host,
                    HostRequest::ExecNew {
                        strategy: None,
                        exec_id,
                        program,
                        params,
                        ensemble: EnsembleSpec::Create {
                            participant_count: participants,
                        },
                        blobs,
                    },
                )
                .await?;
                reply(&CreatedReply {
                    exec_id: exec_id.to_string(),
                })
            }
            Op::Join {
                host,
                program,
                target,
                blobs,
            } => {
                let exec_id = ExecId(rand::random());
                let blobs = parse_blobs(&blobs)?;
                let target = target
                    .map(|target| {
                        Ok::<_, ErrorRow>(NegotiationTarget::new(
                            parse::<PeerId>(&target.creator, "creator")?,
                            parse::<NegotiationId>(&target.negotiation_id, "negotiation id")?,
                        ))
                    })
                    .transpose()?;
                self.host(
                    &host,
                    HostRequest::ExecNew {
                        strategy: None,
                        exec_id,
                        program,
                        params: None,
                        ensemble: EnsembleSpec::Join { target },
                        blobs,
                    },
                )
                .await?;
                reply(&CreatedReply {
                    exec_id: exec_id.to_string(),
                })
            }
            Op::Launch(args) => match self.launcher.launch(args).await {
                Ok(launched) => reply(&launched),
                Err(error) => Err(error),
            },
            Op::Withdraw { host, exec_id } => {
                let exec_id = parse::<ExecId>(&exec_id, "exec id")?;
                self.host(&host, HostRequest::ExecWithdraw { exec_id })
                    .await?;
                Ok(Value::Null)
            }
            Op::Terminate { host, exec_id } => {
                let exec_id = parse::<ExecId>(&exec_id, "exec id")?;
                self.host(
                    &host,
                    HostRequest::ExecTerminate {
                        exec_id,
                        reason: TERMINATE_REASON.to_owned(),
                    },
                )
                .await?;
                Ok(Value::Null)
            }
            Op::ProgramImport { hosts, wasm_base64 } => {
                let wasm = base64::engine::general_purpose::STANDARD
                    .decode(wasm_base64.as_bytes())
                    .map_err(|_| bad_request("wasm is not valid base64"))?;
                let hosts = if hosts.is_empty() {
                    let ResponseOk::Hosts(roster) = self.daemon(Request::HostsList).await? else {
                        return Err(unexpected());
                    };
                    roster.into_iter().map(|status| status.host.id).collect()
                } else {
                    hosts
                };
                let imports = hosts
                    .iter()
                    .map(|host| self.host(host, HostRequest::ProgramImport { wasm: wasm.clone() }));
                let results = futures::future::join_all(imports).await;
                let mut hash = None;
                for result in results {
                    let ResponseOk::Program(detail) = result? else {
                        return Err(unexpected());
                    };
                    hash = Some(detail.summary.program_hash.to_string());
                }
                for host in &hosts {
                    self.replica.refresh(host).await;
                }
                let Some(hash) = hash else {
                    return Err(bad_request("no Host to import into"));
                };
                reply(&ProgramImported { hash, hosts })
            }
            Op::ProgramRemove { host, program } => {
                self.host(&host, HostRequest::ProgramRemove { program })
                    .await?;
                self.replica.refresh(&host).await;
                Ok(Value::Null)
            }
            Op::ReceiptImport { host, artifact } => {
                let artifact = parse_artifact(artifact)?;
                let response = self
                    .host(
                        &host,
                        HostRequest::ReceiptImport {
                            receipt: Box::new(artifact),
                        },
                    )
                    .await?;
                let ResponseOk::ReceiptList(entries) = response else {
                    return Err(unexpected());
                };
                let Some(entry) = entries.first() else {
                    return Err(unexpected());
                };
                self.replica.refresh(&host).await;
                Ok(json!({ "receipt_id": entry.receipt_id }))
            }
            Op::BlobImport { host, path } => {
                let response = self
                    .host(
                        &host,
                        HostRequest::BlobImport {
                            path: PathBuf::from(path),
                        },
                    )
                    .await?;
                let ResponseOk::BlobImported { hash, length } = response else {
                    return Err(unexpected());
                };
                self.replica.refresh(&host).await;
                reply(&BlobImported {
                    hash: hash.to_string(),
                    length,
                })
            }
            Op::BlobExport { host, hash, path } => {
                let hash = parse::<BlobHash>(&hash, "blob hash")?;
                let response = self
                    .host(
                        &host,
                        HostRequest::BlobExport {
                            hash,
                            path: PathBuf::from(path),
                        },
                    )
                    .await?;
                let ResponseOk::BlobExported { length } = response else {
                    return Err(unexpected());
                };
                Ok(json!({ "length": length }))
            }
            Op::HostOpen { id, user_agent } => {
                let response = self.daemon(Request::HostsOpen { id, user_agent }).await?;
                let ResponseOk::HostOpened(info) = response else {
                    return Err(unexpected());
                };
                let rows: Vec<HostRow> = self
                    .replica
                    .sync_hosts()
                    .await
                    .map_err(|error| transport_error(&error))?;
                let row = rows
                    .into_iter()
                    .find(|row| row.id == info.id)
                    .ok_or_else(|| gateway("opened Host is not in the roster"))?;
                reply(&row)
            }
            Op::DaemonStop => {
                self.daemon(Request::DaemonStop).await?;
                Ok(Value::Null)
            }
        }
    }
}

fn parse_blobs(blobs: &[String]) -> Result<Vec<BlobHash>, ErrorRow> {
    blobs.iter().map(|hash| parse(hash, "blob hash")).collect()
}

/// A user-supplied artifact is untrusted input; deserialization authenticates
/// it, and a failure says nothing about its content.
fn parse_artifact(value: Value) -> Result<ReceiptArtifact, ErrorRow> {
    serde_json::from_value(value).map_err(|_| bad_request("not a valid receipt or stop report"))
}

/// Mirror a view block. The Host validated it against the view limits
/// before answering, so it is copied as is.
fn block_row(block: Block) -> BlockRow {
    match block {
        Block::Facts { title, items } => BlockRow::Facts {
            title,
            items: items
                .into_iter()
                .map(|fact| FactRow {
                    label: fact.label,
                    value: cell_row(fact.value),
                })
                .collect(),
        },
        Block::Table {
            title,
            columns,
            rows,
        } => BlockRow::Table {
            title,
            columns,
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(cell_row).collect())
                .collect(),
        },
        Block::Board {
            title,
            rows,
            cols,
            cells,
            row_labels,
            col_labels,
        } => BlockRow::Board {
            title,
            rows,
            cols,
            cells: cells.into_iter().map(cell_row).collect(),
            row_labels,
            col_labels,
        },
        Block::Progress { label, value, max } => BlockRow::Progress { label, value, max },
        Block::Roster { title, entries } => BlockRow::Roster {
            title,
            entries: entries
                .into_iter()
                .map(|entry| RosterEntryRow {
                    participant: entry.participant,
                    status: cell_row(entry.status),
                    detail: entry.detail,
                })
                .collect(),
        },
    }
}

fn cell_row(cell: Cell) -> CellRow {
    CellRow {
        text: cell.text,
        tone: match cell.tone {
            Tone::Normal => ToneRow::Normal,
            Tone::Muted => ToneRow::Muted,
            Tone::Good => ToneRow::Good,
            Tone::Warn => ToneRow::Warn,
            Tone::Bad => ToneRow::Bad,
            Tone::Highlight => ToneRow::Highlight,
        },
        participant: cell.participant,
    }
}
