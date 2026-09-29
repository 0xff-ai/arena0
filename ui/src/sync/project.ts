import type {
  ActivationInspection,
  ActivityFrame,
  AgreedStep,
  BlobEntry,
  EventFrame,
  ExecStatus,
  HostStatus,
  OpenOffer,
  PendingCalloutStatus,
  ProgramDetail,
  ReceiptListEntry,
} from "~/api/types.gen";
import type {
  ActivationRow,
  ActivityRow,
  BlobRow,
  CalloutRow,
  ExecutionRow,
  HostRow,
  NegotiationMark,
  OfferRow,
  ProgramRow,
  ReceiptRow,
  StepRow,
} from "./rows";

export function hostRow(
  status: HostStatus,
  online: boolean,
  gaps: number,
  lastGapMs: number | null,
): HostRow {
  return {
    ...status.host,
    transport_key: status.transport_key,
    online,
    gaps,
    last_gap_ms: lastGapMs,
  };
}

export function executionRow(
  host: string,
  status: ExecStatus,
  activation: ActivationInspection | null,
  negotiation: NegotiationMark[],
): ExecutionRow {
  const state = status.state;
  const session =
    state.exec_state === "Failed"
      ? state.session?.session_state === "Started"
        ? state.session.session
        : null
      : "session" in state
        ? state.session
        : null;
  const sessionId =
    session?.session_id ??
    (state.exec_state === "Activating"
      ? state.session_id
      : state.exec_state === "Failed" && state.session?.session_state === "Activated"
        ? state.session.session_id
        : null);
  return {
    key: `${host}/${status.exec_id}`,
    host,
    exec_id: status.exec_id,
    program: status.program_id,
    lifecycle: (
      {
        Negotiating: "negotiating",
        Activating: "activating",
        Active: "active",
        Completed: "completed",
        Aborted: "aborted",
        Failed: "failed",
      } as const
    )[state.exec_state],
    negotiation_id: status.negotiation_id,
    session_id: sessionId,
    queue_position: state.exec_state === "Negotiating" ? state.queue_position : null,
    latest_step: session && session.step > 0 ? session.step - 1 : null,
    participants: session?.participants ?? null,
    peers: session?.peers ?? [],
    pending: session?.pending_callout
      ? {
          pending_id: session.pending_callout.pending_id,
          callout_index: session.pending_callout.callout_index,
        }
      : null,
    receipt_available: session?.receipt_available ?? false,
    end: { ...status.end },
    writer: session?.writer ?? null,
    phase: session?.phase ?? null,
    activation: activation ? activationRow(activation) : null,
    created_ms: status.created_at_ms,
    updated_ms: status.updated_at_ms,
    terminal:
      state.exec_state === "Completed"
        ? { kind: "completed", reason: null, outcome: state.outcome }
        : state.exec_state === "Aborted"
          ? { kind: "aborted", reason: state.reason, outcome: null }
          : state.exec_state === "Failed"
            ? { kind: "failed", reason: state.reason, outcome: null }
            : null,
    negotiation,
  };
}

export function activationRow(activation: ActivationInspection): ActivationRow {
  return {
    state: activation.state,
    offer_hash: activation.offer_hash,
    creator: activation.creator,
    target_size: activation.target_size,
    initial_state: activation.initial_state,
    // Ticket order can differ from the committed ensemble's sorted peer order.
    // UI participant indexes must match signer bits and program view cells.
    participants: activation.participants
      .map((p) => ({ ...p }))
      .sort((a, b) => a.peer_id.localeCompare(b.peer_id)),
    params: activation.params,
  };
}

export function stepRow(
  host: string,
  execId: string,
  sessionId: string,
  participants: number,
  step: AgreedStep,
): StepRow {
  const entry = step.entry;
  const terminal = entry.terminal;
  return {
    key: `${host}/${execId}/${entry.step}`,
    host,
    exec_id: execId,
    session_id: sessionId,
    step: entry.step,
    certified_ms: step.certified_at_ms,
    event:
      "SessionStarted" in entry.event
        ? { kind: "session_started", ensemble: entry.event.SessionStarted.ensemble }
        : {
            kind: "message",
            from: entry.event.Message.from,
            bytes: entry.event.Message.data.length,
            decoded: step.message && "Json" in step.message ? step.message.Json : null,
            decode_error:
              step.message && "Undecodable" in step.message ? step.message.Undecodable.error : null,
          },
    pre_state: entry.pre_state,
    post_state: entry.post_state,
    signers: Array.from({ length: participants }, (_, index) => index).filter(
      (index) => ((entry.agreement.signers[Math.floor(index / 8)] ?? 0) & (1 << (index % 8))) !== 0,
    ),
    participants,
    terminal:
      terminal === null
        ? null
        : "End" in terminal
          ? { kind: "end", outcome_bytes: terminal.End.outcome.length }
          : "Abort" in terminal
            ? { kind: "abort", reason: terminal.Abort.reason }
            : { kind: "fail", reason: terminal.Fail.reason },
  };
}

export function calloutRow(
  host: string,
  execId: string,
  sessionId: string | null,
  pending: PendingCalloutStatus,
  openedMs: number,
): CalloutRow {
  return {
    key: `${host}/${execId}/${pending.pending_id}`,
    host,
    exec_id: execId,
    session_id: sessionId,
    ...pending,
    opened_ms: openedMs,
  };
}

export function programRow(detail: ProgramDetail, hosts: string[]): ProgramRow {
  const { summary, schema } = detail;
  const range = summary.participants;
  return {
    hash: summary.program_hash,
    name: summary.name,
    display_name: summary.display_name,
    version: summary.version,
    description: summary.description,
    participants:
      range.kind === "exact"
        ? { min: range.count, max: range.count }
        : { min: range.min, max: range.max },
    hosts,
    schema: {
      params: schema.params,
      state: schema.state.schema,
      outcome: schema.outcome,
      callouts: schema.callouts,
      queries: schema.queries,
      phases: schema.phases,
    },
  };
}

export function receiptRow(host: string, entry: ReceiptListEntry): ReceiptRow {
  return {
    key: `${host}/${entry.receipt_id}`,
    host,
    receipt_id: entry.receipt_id,
    session_id: entry.session_id,
    kind: entry.kind,
    program: entry.program_id,
    completed: entry.completed,
    provenance: entry.provenance,
  };
}

export function blobRow(host: string, entry: BlobEntry): BlobRow {
  return { key: `${host}/${entry.hash}`, host, hash: entry.hash, length: entry.length };
}

export function offerRow(offer: OpenOffer, seenBy: string[], firstSeenMs: number): OfferRow {
  return {
    key: `${offer.program_id}/${offer.creator}/${offer.negotiation_id}`,
    program: offer.program_id,
    negotiation_id: offer.negotiation_id,
    creator: offer.creator,
    offer_seq: offer.offer_seq,
    target_size: offer.target_size,
    params: offer.params,
    deadline_ms: offer.deadline_unix_ms,
    first_seen_ms: firstSeenMs,
    seen_by: [...seenBy].sort(),
  };
}

/** null for events that are not negotiation marks; at_ms is frame.ts. */
export function negotiationMark(frame: EventFrame): NegotiationMark | null {
  const at_ms = frame.ts;
  switch (frame.kind) {
    case "exec.negotiation.started":
      return { at_ms, kind: "started", detail: `target ${frame.data.target_size}` };
    case "exec.negotiation.offer_accepted":
      return { at_ms, kind: "offer_accepted", detail: `offer ${frame.data.offer_seq}` };
    case "exec.negotiation.ticket_accepted":
      return {
        at_ms,
        kind: "ticket_accepted",
        detail: `ticket ${frame.data.ticket_count}/${frame.data.target_size}`,
      };
    case "exec.negotiation.peers":
      return { at_ms, kind: "peers", detail: `${frame.data.peers.length} peers` };
    case "exec.negotiation.prepared":
      return { at_ms, kind: "prepared", detail: `${frame.data.participants} participants` };
    case "exec.negotiation.resumed":
      return { at_ms, kind: "resumed", detail: `${frame.data.participants} participants` };
    case "exec.negotiation.committed":
      return { at_ms, kind: "committed", detail: `${frame.data.participants} participants` };
    case "exec.negotiation.retried":
      return {
        at_ms,
        kind: "retried",
        detail: `retried ${frame.data.attempt} at ${frame.data.stage}`,
      };
    case "exec.negotiation.rejoined":
      return { at_ms, kind: "rejoined", detail: "rejoined" };
    case "exec.negotiation.timed_out":
      return {
        at_ms,
        kind: "timed_out",
        detail: `timed out at ${frame.data.stage} · tickets ${frame.data.ticket_count}/${frame.data.target_size} · signatures ${frame.data.sig_count}/${frame.data.target_size}`,
      };
    default:
      return null;
  }
}

/** Activity text contains tags and counts, never payloads, answers, params, or outcomes. */
export function eventActivity(host: string, frame: EventFrame): ActivityRow {
  let text: string;
  let level: ActivityRow["level"] = "info";
  switch (frame.kind) {
    case "host.started":
      text = "host started";
      break;
    case "host.stopped":
      text = "host stopped";
      break;
    case "negotiation.offer_seen":
      text = `offer seen from ${frame.data.creator.slice(0, 4)}…`;
      break;
    case "negotiation.offer_closed":
      text = `offer closed · ${frame.data.reason}`;
      break;
    case "exec.created":
      text = "execution created";
      break;
    case "exec.terminated":
      text = frame.data.failed_class
        ? `execution terminated (${frame.data.failed_class.replaceAll("_", " ")})`
        : "execution terminated";
      if (frame.data.failed_class) level = "error";
      break;
    case "exec.negotiation.started":
      text = `negotiation started · target ${frame.data.target_size}`;
      break;
    case "exec.negotiation.offer_accepted":
      text = "offer accepted";
      break;
    case "exec.negotiation.ticket_accepted":
      text = `ticket ${frame.data.ticket_count}/${frame.data.target_size} accepted`;
      break;
    case "exec.negotiation.peers":
      text = `${frame.data.peers.length} peers`;
      break;
    case "exec.negotiation.prepared":
      text = `prepared · ${frame.data.participants} participants`;
      break;
    case "exec.negotiation.resumed":
      text = `resumed · ${frame.data.participants} participants`;
      break;
    case "exec.negotiation.committed":
      text = `committed · ${frame.data.participants} participants`;
      break;
    case "exec.negotiation.retried":
      text = `retried ${frame.data.attempt} at ${frame.data.stage}`;
      level = "warn";
      break;
    case "exec.negotiation.rejoined":
      text = "rejoined";
      break;
    case "exec.negotiation.timed_out":
      text = `timed out at ${frame.data.stage}`;
      level = "warn";
      break;
    case "exec.session.started":
      text = `session started · ${frame.data.ensemble.length} participants`;
      break;
    case "exec.session.callout":
      text = `callout ${frame.data.name} opened`;
      break;
    case "exec.session.callout_answered":
      text = "callout answered";
      break;
    case "exec.session.step":
      text = `step ${frame.data.step} · ${frame.data.signers}/${frame.data.participants} signed`;
      break;
    case "exec.session.ended":
      text =
        "completed" in frame.data.terminal
          ? "session completed"
          : `session aborted at step ${frame.data.terminal.aborted.step}`;
      if ("aborted" in frame.data.terminal) level = "warn";
      break;
    case "exec.session.end_progress":
      text = `end handshake ${frame.data.phase[0]!.toUpperCase()}${frame.data.phase.slice(1)}`;
      break;
    case "stream.lagged":
      text = `${frame.data.skipped} events skipped · reloading`;
      level = "warn";
      break;
  }
  return {
    key: `${host}/${frame.boot_id}/${frame.seq}`,
    at_ms: frame.ts,
    source: host,
    kind: frame.kind,
    exec_id: frame.exec_id ?? null,
    session_id: frame.session_id ?? null,
    text,
    level,
  };
}

export function activityRow(frame: ActivityFrame): ActivityRow {
  let text: string;
  let level: ActivityRow["level"] = "info";
  switch (frame.kind) {
    case "started":
      text = `tool ${frame.data.tool} started`;
      break;
    case "finished": {
      const result = frame.data.result;
      const outcome =
        result.kind === "ok"
          ? "ok"
          : result.kind === "interrupted"
            ? "interrupted"
            : result.data.code
              ? `error ${result.data.code}`
              : "error";
      if (result.kind === "tool_error") level = "error";
      text = `tool call ${outcome} · ${frame.data.elapsed_ms} ms`;
      break;
    }
    case "lagged":
      text = `${frame.data.skipped} events skipped · reloading`;
      level = "warn";
      break;
  }
  return {
    key: `mcp/${frame.boot_id}/${frame.seq}`,
    at_ms: frame.ts,
    source: "mcp",
    kind: `tool.${frame.kind}`,
    exec_id: frame.kind === "started" ? (frame.data.exec_id ?? null) : null,
    session_id: null,
    text,
    level,
  };
}
