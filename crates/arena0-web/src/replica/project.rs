//! Projections from daemon facts to replicated rows. Pure functions: nothing
//! here reads the daemon or holds state.

use arena0_api::{
    ActivationInspection, ActivationInspectionState, ActivityData, ActivityFrame, ActivityResult,
    AgreedStep, BlobEntry, EventData, EventFrame, ExecEndPhase, ExecStatus, ExecStatusState,
    HostStatus, NegotiationStage, ProgramDetail, ReceiptListEntry, SessionTerminal,
};
use arena0_program::BorshSchemaDocument;
use arena0_protocol::{StepEvent, StepTerminal};

use crate::decode::decode_message;
use crate::model::{
    ActivationParticipantRow, ActivationRow, ActivationState, ActivityRow, BlobRow,
    CalloutSchemaRow, EndPhase, EndRow, ExecutionRow, HostRow, Level, Lifecycle, NegotiationMark,
    NegotiationMarkKind, ParticipantRange, PendingRef, PhaseRow, ProgramRow, ProgramSchemaRow,
    ProvenanceRow, QuerySchemaRow, ReceiptKindRow, ReceiptRow, StepEventRow, StepRow,
    StepTerminalRow,
};

pub(super) fn exec_key(host: &str, exec_id: &str) -> String {
    format!("{host}/{exec_id}")
}

pub(super) fn host_row(
    status: &HostStatus,
    online: bool,
    gaps: u32,
    last_gap_ms: Option<u64>,
) -> HostRow {
    HostRow {
        id: status.host.id.clone(),
        peer_id: status.host.peer_id.to_string(),
        user_agent: status.host.user_agent.clone(),
        transport_key: hex::encode(status.transport_key.0),
        online,
        gaps,
        last_gap_ms,
    }
}

/// The execution row without the facts only the owner can hold
/// (`terminal`, `negotiation`).
pub(super) fn execution_row(
    host: &str,
    status: &ExecStatus,
    activation: Option<&ActivationRow>,
) -> ExecutionRow {
    let exec_id = status.exec_id.to_string();
    let session = status.session();
    let lifecycle = match status.state {
        ExecStatusState::Negotiating { .. } => Lifecycle::Negotiating,
        ExecStatusState::Activating { .. } => Lifecycle::Activating,
        ExecStatusState::Active { .. } => Lifecycle::Active,
        ExecStatusState::Completed { .. } => Lifecycle::Completed,
        ExecStatusState::Aborted { .. } => Lifecycle::Aborted,
        ExecStatusState::Failed { .. } => Lifecycle::Failed,
    };
    ExecutionRow {
        key: exec_key(host, &exec_id),
        host: host.to_owned(),
        exec_id,
        program: status.program_id.to_string(),
        lifecycle,
        negotiation_id: status.negotiation_id.map(|id| id.to_string()),
        session_id: status.session_id().map(|id| id.to_string()),
        queue_position: status
            .queue_position()
            .map(|position| u32::try_from(position).unwrap_or(u32::MAX)),
        latest_step: session.and_then(|session| session.step.checked_sub(1)),
        participants: session
            .map(|session| u32::try_from(session.participants).unwrap_or(u32::MAX)),
        peers: session
            .map(|session| session.peers.iter().map(ToString::to_string).collect())
            .unwrap_or_default(),
        pending: status.pending_callout().map(|pending| PendingRef {
            pending_id: pending.pending_id.to_string(),
            callout_index: pending.callout_index,
        }),
        receipt_available: session.is_some_and(|session| session.receipt_available),
        end: EndRow {
            phase: match status.end.phase {
                ExecEndPhase::Open => EndPhase::Open,
                ExecEndPhase::Ending => EndPhase::Ending,
                ExecEndPhase::Ended => EndPhase::Ended,
            },
            unconfirmed: status
                .end
                .unconfirmed
                .iter()
                .map(ToString::to_string)
                .collect(),
        },
        writer: session.and_then(|session| session.writer.map(|peer| peer.to_string())),
        phase: session.and_then(|session| session.phase.clone()),
        activation: activation.cloned(),
        created_ms: status.created_at_ms,
        updated_ms: status.updated_at_ms,
        terminal: None,
        negotiation: Vec::new(),
    }
}

pub(super) fn activation_row(activation: &ActivationInspection) -> ActivationRow {
    ActivationRow {
        state: match activation.state {
            ActivationInspectionState::Prepared => ActivationState::Prepared,
            ActivationInspectionState::Committed => ActivationState::Committed,
        },
        offer_hash: activation.offer_hash.to_string(),
        creator: activation.creator.to_string(),
        target_size: activation.target_size,
        initial_state: activation.initial_state.to_string(),
        participants: {
            // The daemon lists tickets in activation order; participant
            // indexes follow the committed ensemble, which sorts peer ids
            // (`Ensemble::from_peers`). Reorder so index i is participant i.
            let mut participants: Vec<_> = activation.participants.iter().collect();
            participants.sort_by_key(|participant| participant.peer_id);
            participants
                .into_iter()
                .map(|participant| ActivationParticipantRow {
                    peer_id: participant.peer_id.to_string(),
                    ticket_hash: participant.ticket_hash.to_string(),
                })
                .collect()
        },
        params: activation.params.clone(),
    }
}

pub(super) struct StepContext<'a> {
    pub host: &'a str,
    pub exec_id: &'a str,
    pub session_id: &'a str,
    pub participants: u16,
    pub schema: Option<&'a BorshSchemaDocument>,
}

pub(super) fn step_row(context: &StepContext<'_>, step: &AgreedStep) -> StepRow {
    let entry = &step.entry;
    let event = match &entry.event {
        StepEvent::SessionStarted { ensemble } => StepEventRow::SessionStarted {
            ensemble: ensemble.peers().iter().map(ToString::to_string).collect(),
        },
        StepEvent::Message { from, data } => {
            let (decoded, decode_error) = decode_message(context.schema, data);
            StepEventRow::Message {
                from: from.to_string(),
                bytes: u32::try_from(data.len()).unwrap_or(u32::MAX),
                decoded,
                decode_error,
            }
        }
    };
    StepRow {
        key: format!("{}/{}/{}", context.host, context.exec_id, entry.step),
        host: context.host.to_owned(),
        exec_id: context.exec_id.to_owned(),
        session_id: context.session_id.to_owned(),
        step: entry.step,
        certified_ms: step.certified_at_ms,
        event,
        pre_state: entry.pre_state.to_string(),
        post_state: entry.post_state.to_string(),
        signers: (0..context.participants)
            .filter(|index| entry.agreement.signers.contains(usize::from(*index)))
            .collect(),
        participants: context.participants,
        terminal: entry.terminal.as_ref().map(|terminal| match terminal {
            StepTerminal::End { outcome } => StepTerminalRow::End {
                outcome_bytes: u32::try_from(outcome.len()).unwrap_or(u32::MAX),
            },
            StepTerminal::Abort { reason } => StepTerminalRow::Abort {
                reason: reason.clone(),
            },
            StepTerminal::Fail { reason } => StepTerminalRow::Fail {
                reason: reason.clone(),
            },
        }),
    }
}

/// One program's row without its holders, which the owner merges.
pub(super) fn program_row(detail: &ProgramDetail) -> ProgramRow {
    let summary = &detail.summary;
    let (min, max) = summary.participants.bounds();
    let schema = &detail.schema;
    ProgramRow {
        hash: summary.program_hash.to_string(),
        name: summary.name.clone(),
        display_name: summary.display_name.clone(),
        version: summary.version.clone(),
        description: summary.description.clone(),
        participants: ParticipantRange {
            min: u16::from(min),
            max: u16::from(max),
        },
        hosts: Vec::new(),
        schema: ProgramSchemaRow {
            params: schema.params.as_value().clone(),
            state: schema.state.schema.as_value().clone(),
            outcome: schema.outcome.as_value().clone(),
            callouts: schema
                .callouts
                .iter()
                .map(|callout| CalloutSchemaRow {
                    name: callout.name.clone(),
                    prompt: callout.prompt.clone(),
                    input: callout.input.as_value().clone(),
                    output: callout.output.as_value().clone(),
                })
                .collect(),
            queries: schema
                .queries
                .iter()
                .map(|query| QuerySchemaRow {
                    name: query.name.clone(),
                    label: query.label.clone(),
                    request: query.request.as_value().clone(),
                    response: query.response.as_value().clone(),
                })
                .collect(),
            phases: schema
                .phases
                .iter()
                .map(|phase| PhaseRow {
                    name: phase.name.clone(),
                    description: phase.description.clone(),
                    is_default: phase.is_default,
                    is_terminal: phase.is_terminal,
                })
                .collect(),
        },
    }
}

pub(super) fn receipt_row(host: &str, entry: &ReceiptListEntry) -> ReceiptRow {
    use arena0_api::ReceiptProvenance;
    use arena0_protocol::ReceiptKind;
    ReceiptRow {
        key: format!("{host}/{}", entry.receipt_id),
        host: host.to_owned(),
        receipt_id: entry.receipt_id.clone(),
        session_id: entry.session_id.to_string(),
        kind: match entry.kind {
            ReceiptKind::Receipt => ReceiptKindRow::Receipt,
            ReceiptKind::StopReport => ReceiptKindRow::StopReport,
        },
        program: entry.program_id.to_string(),
        completed: entry.completed,
        provenance: match entry.provenance {
            ReceiptProvenance::Produced => ProvenanceRow::Produced,
            ReceiptProvenance::Imported => ProvenanceRow::Imported,
            ReceiptProvenance::Both => ProvenanceRow::Both,
        },
    }
}

pub(super) fn blob_row(host: &str, entry: &BlobEntry) -> BlobRow {
    let hash = entry.hash.to_string();
    BlobRow {
        key: format!("{host}/{hash}"),
        host: host.to_owned(),
        hash,
        length: entry.length,
        path: entry.path.display().to_string(),
    }
}

fn stage_name(stage: NegotiationStage) -> &'static str {
    match stage {
        NegotiationStage::Gossiping => "gossiping",
        NegotiationStage::Prepared => "prepared",
    }
}

/// The negotiation mark an event contributes, if it is a negotiation event.
pub(super) fn negotiation_mark(data: &EventData, at_ms: u64) -> Option<NegotiationMark> {
    use NegotiationMarkKind as Kind;
    let (kind, detail) = match data {
        EventData::NegotiationStarted { target_size } => {
            (Kind::Started, format!("target {target_size}"))
        }
        EventData::NegotiationOfferAccepted { offer_seq, .. } => {
            (Kind::OfferAccepted, format!("offer {offer_seq}"))
        }
        EventData::NegotiationTicketAccepted {
            ticket_count,
            target_size,
            ..
        } => (
            Kind::TicketAccepted,
            format!("ticket {ticket_count}/{target_size}"),
        ),
        EventData::NegotiationPeers { peers, .. } => {
            (Kind::Peers, format!("{} peers", peers.len()))
        }
        EventData::NegotiationPrepared { participants } => {
            (Kind::Prepared, format!("{participants} participants"))
        }
        EventData::NegotiationResumed { participants } => {
            (Kind::Resumed, format!("{participants} participants"))
        }
        EventData::NegotiationCommitted { participants } => {
            (Kind::Committed, format!("{participants} participants"))
        }
        EventData::NegotiationRetried { attempt, stage, .. } => (
            Kind::Retried,
            format!("retried {attempt} at {}", stage_name(*stage)),
        ),
        EventData::NegotiationRejoined {} => (Kind::Rejoined, "rejoined".to_owned()),
        EventData::NegotiationTimedOut {
            stage,
            ticket_count,
            sig_count,
            target_size,
        } => (
            Kind::TimedOut,
            format!(
                "timed out at {} · tickets {ticket_count}/{target_size} · signatures {sig_count}/{target_size}",
                stage_name(*stage)
            ),
        ),
        _ => return None,
    };
    Some(NegotiationMark {
        at_ms,
        kind,
        detail,
    })
}

/// One activity row for a Host event. The text is built from the tag and
/// counts only: never payloads, answers, params, or outcomes.
pub(super) fn event_activity(host: &str, frame: &EventFrame) -> ActivityRow {
    let (text, level) = match &frame.data {
        EventData::HostStarted { .. } => ("host started".to_owned(), Level::Info),
        EventData::HostStopped { .. } => ("host stopped".to_owned(), Level::Info),
        EventData::OfferSeen { creator, .. } => (
            format!("offer seen from {}…", &creator.to_string()[..4]),
            Level::Info,
        ),
        EventData::Created { .. } => ("execution created".to_owned(), Level::Info),
        EventData::Terminated { failed_class, .. } => match failed_class {
            Some(class) => (
                format!("execution terminated ({})", failure_name(*class)),
                Level::Error,
            ),
            None => ("execution terminated".to_owned(), Level::Info),
        },
        EventData::NegotiationStarted { target_size } => (
            format!("negotiation started · target {target_size}"),
            Level::Info,
        ),
        EventData::NegotiationOfferAccepted { .. } => ("offer accepted".to_owned(), Level::Info),
        EventData::NegotiationTicketAccepted {
            ticket_count,
            target_size,
            ..
        } => (
            format!("ticket {ticket_count}/{target_size} accepted"),
            Level::Info,
        ),
        EventData::NegotiationPeers { peers, .. } => {
            (format!("{} peers", peers.len()), Level::Info)
        }
        EventData::NegotiationPrepared { participants } => (
            format!("prepared · {participants} participants"),
            Level::Info,
        ),
        EventData::NegotiationResumed { participants } => (
            format!("resumed · {participants} participants"),
            Level::Info,
        ),
        EventData::NegotiationCommitted { participants } => (
            format!("committed · {participants} participants"),
            Level::Info,
        ),
        EventData::NegotiationRetried { attempt, stage, .. } => (
            format!("retried {attempt} at {}", stage_name(*stage)),
            Level::Warn,
        ),
        EventData::NegotiationRejoined {} => ("rejoined".to_owned(), Level::Info),
        EventData::NegotiationTimedOut { stage, .. } => {
            (format!("timed out at {}", stage_name(*stage)), Level::Warn)
        }
        EventData::SessionStarted { ensemble } => (
            format!("session started · {} participants", ensemble.len()),
            Level::Info,
        ),
        EventData::SessionCallout { name, .. } => (format!("callout {name} opened"), Level::Info),
        EventData::SessionCalloutAnswered { .. } => ("callout answered".to_owned(), Level::Info),
        EventData::SessionStep {
            step,
            signers,
            participants,
            ..
        } => (
            format!("step {step} · {signers}/{participants} signed"),
            Level::Info,
        ),
        EventData::SessionEnded { terminal } => match terminal {
            SessionTerminal::Completed { .. } => ("session completed".to_owned(), Level::Info),
            SessionTerminal::Aborted { step, .. } => {
                (format!("session aborted at step {step}"), Level::Warn)
            }
        },
        EventData::Lagged { skipped } => {
            (format!("{skipped} events skipped · reloading"), Level::Warn)
        }
    };
    ActivityRow {
        key: format!("{host}/{}/{}", frame.boot_id, frame.seq),
        at_ms: frame.ts,
        source: host.to_owned(),
        kind: frame.kind().to_owned(),
        exec_id: frame.exec_id.map(|id| id.to_string()),
        session_id: frame.session_id.map(|id| id.to_string()),
        text,
        level,
    }
}

fn failure_name(class: arena0_api::ExecutionFailureKind) -> &'static str {
    use arena0_api::ExecutionFailureKind as Kind;
    match class {
        Kind::Negotiation => "negotiation",
        Kind::HostStopped => "host stopped",
        Kind::ProgramAborted => "program aborted",
        Kind::Runtime => "runtime",
        Kind::InvalidGuestOutput => "invalid guest output",
    }
}

/// One activity row for an MCP tool frame. `tool` is the tool name the
/// matching `started` frame announced, when this gateway saw it.
pub(super) fn tool_activity(frame: &ActivityFrame, tool: Option<&str>) -> ActivityRow {
    let name = tool.unwrap_or("call");
    let (kind, text, level, exec_id) = match &frame.data {
        ActivityData::Started { tool, exec_id, .. } => (
            "tool.started",
            format!("tool {tool} started"),
            Level::Info,
            exec_id.map(|id| id.to_string()),
        ),
        ActivityData::Finished {
            elapsed_ms, result, ..
        } => {
            let (outcome, level) = match result {
                ActivityResult::Ok => ("ok".to_owned(), Level::Info),
                ActivityResult::ToolError { code } => (
                    code.map_or_else(|| "error".to_owned(), |code| format!("error {code:?}")),
                    Level::Error,
                ),
                ActivityResult::Interrupted => ("interrupted".to_owned(), Level::Info),
            };
            (
                "tool.finished",
                format!("tool {name} {outcome} · {elapsed_ms} ms"),
                level,
                None,
            )
        }
        ActivityData::Lagged { skipped } => (
            "tool.lagged",
            format!("{skipped} events skipped · reloading"),
            Level::Warn,
            None,
        ),
    };
    ActivityRow {
        key: format!("mcp/{}/{}", frame.boot_id, frame.seq),
        at_ms: frame.ts,
        source: "mcp".to_owned(),
        kind: kind.to_owned(),
        exec_id,
        session_id: None,
        text,
        level,
    }
}
