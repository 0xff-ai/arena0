import type {
  ActivationInspectionState,
  ActivationParticipant,
  Block,
  CalloutSchema,
  EffectKind,
  EventKind,
  ExecEndStatus,
  JsonValue,
  PhaseSchema,
  QuerySchema,
  ReceiptKind,
  ReceiptProvenance,
} from "~/api/types.gen";

export type HostRow = {
  /** Local namespace such as `host-01`. */
  id: string;
  peer_id: string;
  user_agent: string | null;
  transport_key: string;
  /** False after `host.stopped` until the next `host.started`. */
  online: boolean;
  /**
   * `stream.lagged` frames seen for this Host since the browser connected.
   * Each one made the browser reload the Host from durable reads.
   */
  gaps: number;
  last_gap_ms: number | null;
};

export type ExecutionRow = {
  key: string;
  host: string;
  exec_id: string;
  /** Program content hash. */
  program: string;
  lifecycle: Lifecycle;
  negotiation_id: string | null;
  session_id: string | null;
  queue_position: number | null;
  /**
   * Index of the latest agreed step (steps are numbered from 0, like
   * `StepRow.step`); null until step 0 is certified. The daemon's
   * `SessionStatus.step` counts agreed steps, so this is that count minus
   * one.
   */
  latest_step: number | null;
  /** Committed ensemble size. */
  participants: number | null;
  /** Committed remote participants; excludes this Host. */
  peers: string[];
  pending: { pending_id: string; callout_index: number } | null;
  receipt_available: boolean;
  end: ExecEndStatus;
  /** Participant expected to author the next step, when the program has one. */
  writer: string | null;
  /** Current program phase name, when the program declares phases. */
  phase: string | null;
  activation: ActivationRow | null;
  /** Durable local creation time. */
  created_ms: number;
  /** Durable local time of the last lifecycle change. */
  updated_ms: number;
  terminal: TerminalRow | null;
  /**
   * Negotiation progress this browser observed live. Empty for negotiations
   * that ran while no browser was watching; never reconstructed.
   */
  negotiation: NegotiationMark[];
};

export type Lifecycle =
  | "negotiating"
  | "activating"
  | "active"
  | "completed"
  | "aborted"
  | "failed";

export type ActivationRow = {
  state: ActivationInspectionState;
  offer_hash: string;
  creator: string;
  target_size: number;
  initial_state: string;
  /**
   * All selected participants in participant order (sorted peer ids, as
   * the committed ensemble orders them). Index `i` is participant `P{i}`
   * everywhere in the UI, matching step signers and view cells.
   */
  participants: ActivationParticipant[];
  /** Offer params as JSON. */
  params: JsonValue;
};

export type TerminalRow = {
  kind: "completed" | "aborted" | "failed";
  /** Terminal reason for aborts and failures. */
  reason: string | null;
  /** JSON outcome of a completed session, reported by the daemon. */
  outcome: JsonValue | null;
};

export type NegotiationMark = {
  at_ms: number;
  kind:
    | "started"
    | "offer_accepted"
    | "ticket_accepted"
    | "peers"
    | "prepared"
    | "resumed"
    | "committed"
    | "retried"
    | "rejoined"
    | "timed_out";
  /** One line with the event's counts, e.g. `ticket 2/4` or `retried 3 at prepared`. */
  detail: string;
};

export type StepRow = {
  key: string;
  host: string;
  exec_id: string;
  session_id: string;
  step: number;
  /** Local time this Host durably stored the certified step. */
  certified_ms: number;
  event:
    | { kind: "session_started"; ensemble: string[] }
    | {
        kind: "message";
        from: string;
        bytes: number;
        /** The payload decoded with the program's Borsh message schema. */
        decoded: JsonValue | null;
        /** Why decoding failed, when it did. */
        decode_error: string | null;
      };
  pre_state: string;
  post_state: string;
  /** Canonical participant indexes whose signatures are in the aggregate. */
  signers: number[];
  /** Committed ensemble size. */
  participants: number;
  terminal:
    | { kind: "end"; outcome_bytes: number }
    | { kind: "abort"; reason: string }
    | { kind: "fail"; reason: string }
    | null;
};

export type CalloutRow = {
  key: string;
  host: string;
  exec_id: string;
  session_id: string | null;
  pending_id: string;
  callout_index: number;
  name: string;
  prompt: string;
  /** JSON Schema (Draft 2020-12) of the answer. */
  schema: JsonValue;
  /** Guest-produced context. */
  context: JsonValue;
  /**
   * When the callout became pending: the event time when observed live,
   * otherwise the execution status's durable update time.
   */
  opened_ms: number;
};

export type ProgramRow = {
  hash: string;
  name: string;
  display_name: string;
  version: string;
  description: string;
  participants: { min: number; max: number };
  /** Hosts whose catalog holds the program. */
  hosts: string[];
  schema: {
    params: JsonValue;
    state: JsonValue;
    outcome: JsonValue;
    callouts: CalloutSchema[];
    queries: QuerySchema[];
    phases: PhaseSchema[];
  };
};

export type ReceiptRow = {
  key: string;
  host: string;
  receipt_id: string;
  session_id: string;
  kind: ReceiptKind;
  program: string;
  completed: boolean;
  provenance: ReceiptProvenance;
};

export interface OfferRow {
  /** `${program}/${creator}/${negotiation_id}`. */
  key: string;
  program: string;
  negotiation_id: string;
  creator: string;
  offer_seq: number;
  target_size: number;
  /** Offer params; every joiner signs these terms. */
  params: JsonValue;
  deadline_ms: number;
  /** Earliest `first_seen_ms` among the Hosts that list it. */
  first_seen_ms: number;
  /** Local Hosts whose `negotiation.offers` list it, sorted. */
  seen_by: string[];
}

export type ActivityRow = {
  key: string;
  at_ms: number;
  /** A Host id, or `mcp` for adapter tool calls. */
  source: string;
  /** Wire tag such as `exec.session.step` or `tool.finished`. */
  kind: string;
  exec_id: string | null;
  session_id: string | null;
  /** One line without payloads, answers, params, or outcomes. */
  text: string;
  level: "info" | "warn" | "error";
};

export type BlobRow = { key: string; host: string; hash: string; length: number };

export type RowBatch =
  | { collection: "hosts"; ops: RowOp<HostRow>[] }
  | { collection: "executions"; ops: RowOp<ExecutionRow>[] }
  | { collection: "steps"; ops: RowOp<StepRow>[] }
  | { collection: "callouts"; ops: RowOp<CalloutRow>[] }
  | { collection: "programs"; ops: RowOp<ProgramRow>[] }
  | { collection: "receipts"; ops: RowOp<ReceiptRow>[] }
  | { collection: "offers"; ops: RowOp<OfferRow>[] }
  | { collection: "activity"; ops: RowOp<ActivityRow>[] }
  | { collection: "blobs"; ops: RowOp<BlobRow>[] };

export type RowOp<T> = { op: "upsert"; row: T } | { op: "delete"; key: string };

export type VerifyTarget =
  | { kind: "stored"; receipt_id: string }
  | { kind: "inline"; artifact: JsonValue };

export type ViewReply = {
  /**
   * Index of the latest agreed step applied to the rendered state; null is
   * the initial state before step 0.
   */
  step: number | null;
  /** Slot text: UTF-8 with ANSI SGR sequences only. */
  header: string | null;
  agents: string | null;
  state: string | null;
  status_bar: string | null;
  /**
   * Typed blocks the program rendered next to its text slots, validated
   * by the daemon. Empty for programs that render text only.
   */
  blocks: Block[];
};

export type RecordsReply = {
  from: number;
  total: number;
  next: number | null;
  records: RecordRow[];
};

export type RecordRow = {
  position: number;
  /** Agreed steps this event produced. */
  steps: number[];
  event: EventKind;
  input_bytes: number | null;
  effects: { kind: EffectKind; bytes: number | null }[];
};

export type VerifyReply = {
  receipt_id: string;
  program: string;
  session_id: string;
  /** Participants in canonical order. */
  ensemble: string[];
  steps: number;
  termination: { kind: "completed" } | { kind: "stopped"; cause: string };
};

export type CreatedReply = { exec_id: string; negotiation_id: string | null };

export type ExecRef = { host: string; exec_id: string };

export type CollectionName = RowBatch["collection"];
export type RowOf<C extends CollectionName> = Extract<
  RowBatch,
  { collection: C }
>["ops"][number] extends infer O
  ? O extends { op: "upsert"; row: infer R }
    ? R
    : never
  : never;
