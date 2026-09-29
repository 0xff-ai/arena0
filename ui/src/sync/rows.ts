export type JsonValue =
  | number
  | string
  | boolean
  | Array<JsonValue>
  | { [key in string]: JsonValue }
  | null;

export type HostRow = {
  /**
   * Local namespace such as `host-01`.
   */
  id: string;
  peer_id: string;
  user_agent: string | null;
  transport_key: string;
  /**
   * False after `host.stopped` until the next `host.started`.
   */
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
  /**
   * Program content hash.
   */
  program: string;
  lifecycle: Lifecycle;
  negotiation_id: string | null;
  session_id: string | null;
  queue_position: number | null;
  /**
   * Index of the latest agreed step (steps are numbered from 0, like
   * `StepRow.step`); `None` until step 0 is certified. The daemon's
   * `SessionStatus.step` counts agreed steps, so this is that count minus
   * one.
   */
  latest_step: number | null;
  /**
   * Committed ensemble size.
   */
  participants: number | null;
  /**
   * Committed remote participants; excludes this Host.
   */
  peers: Array<string>;
  pending: PendingRef | null;
  receipt_available: boolean;
  end: EndRow;
  /**
   * Participant expected to author the next step, when the program has one.
   */
  writer: string | null;
  /**
   * Current program phase name, when the program declares phases.
   */
  phase: string | null;
  activation: ActivationRow | null;
  /**
   * Durable local creation time.
   */
  created_ms: number;
  /**
   * Durable local time of the last lifecycle change.
   */
  updated_ms: number;
  terminal: TerminalRow | null;
  /**
   * Negotiation progress this browser observed live. Empty for negotiations
   * that ran while no browser was watching; never reconstructed.
   */
  negotiation: Array<NegotiationMark>;
};

export type Lifecycle =
  | "negotiating"
  | "activating"
  | "active"
  | "completed"
  | "aborted"
  | "failed";

export type PendingRef = { pending_id: string; callout_index: number };

export type EndRow = {
  phase: EndPhase;
  /**
   * Peers that have not confirmed the end.
   */
  unconfirmed: Array<string>;
};

export type EndPhase = "open" | "ending" | "ended";

export type ActivationRow = {
  state: ActivationState;
  offer_hash: string;
  creator: string;
  target_size: number;
  initial_state: string;
  /**
   * All selected participants in participant order (sorted peer ids, as
   * the committed ensemble orders them). Index `i` is participant `P{i}`
   * everywhere in the UI, matching step signers and view cells.
   */
  participants: Array<ActivationParticipantRow>;
  /**
   * Offer params as JSON.
   */
  params: JsonValue;
};

export type ActivationState = "prepared" | "committed";

export type ActivationParticipantRow = { peer_id: string; ticket_hash: string };

export type TerminalRow = {
  kind: TerminalKind;
  /**
   * Terminal reason for aborts and failures.
   */
  reason: string | null;
  /**
   * JSON outcome of a completed session, reported by the daemon.
   */
  outcome: JsonValue | null;
};

export type TerminalKind = "completed" | "aborted" | "failed";

export type NegotiationMark = {
  at_ms: number;
  kind: NegotiationMarkKind;
  /**
   * One line with the event's counts, e.g. `ticket 2/4` or `retried 3 at prepared`.
   */
  detail: string;
};

export type NegotiationMarkKind =
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

export type StepRow = {
  key: string;
  host: string;
  exec_id: string;
  session_id: string;
  step: number;
  /**
   * Local time this Host durably stored the certified step.
   */
  certified_ms: number;
  event: StepEventRow;
  pre_state: string;
  post_state: string;
  /**
   * Canonical participant indexes whose signatures are in the aggregate.
   */
  signers: Array<number>;
  /**
   * Committed ensemble size.
   */
  participants: number;
  terminal: StepTerminalRow | null;
};

export type StepEventRow =
  | { kind: "session_started"; ensemble: Array<string> }
  | {
      kind: "message";
      from: string;
      bytes: number;
      /**
       * The payload decoded with the program's Borsh message schema.
       */
      decoded: JsonValue | null;
      /**
       * Why decoding failed, when it did.
       */
      decode_error: string | null;
    };

export type StepTerminalRow =
  | { kind: "end"; outcome_bytes: number }
  | { kind: "abort"; reason: string }
  | { kind: "fail"; reason: string };

export type CalloutRow = {
  key: string;
  host: string;
  exec_id: string;
  session_id: string | null;
  pending_id: string;
  callout_index: number;
  name: string;
  prompt: string;
  /**
   * JSON Schema (Draft 2020-12) of the answer.
   */
  schema: JsonValue;
  /**
   * Guest-produced context.
   */
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
  participants: ParticipantRange;
  /**
   * Hosts whose catalog holds the program.
   */
  hosts: Array<string>;
  schema: ProgramSchemaRow;
};

export type ParticipantRange = { min: number; max: number };

export type ProgramSchemaRow = {
  params: JsonValue;
  state: JsonValue;
  outcome: JsonValue;
  callouts: Array<CalloutSchemaRow>;
  queries: Array<QuerySchemaRow>;
  phases: Array<PhaseRow>;
};

export type CalloutSchemaRow = {
  name: string;
  prompt: string;
  input: JsonValue;
  output: JsonValue;
};

export type QuerySchemaRow = {
  name: string;
  label: string;
  request: JsonValue;
  response: JsonValue;
};

export type PhaseRow = {
  name: string;
  description: string;
  is_default: boolean;
  is_terminal: boolean;
};

export type ReceiptRow = {
  key: string;
  host: string;
  receipt_id: string;
  session_id: string;
  kind: ReceiptKindRow;
  program: string;
  completed: boolean;
  provenance: ProvenanceRow;
};

export type ReceiptKindRow = "receipt" | "stop_report";

export type ProvenanceRow = "produced" | "imported" | "both";

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
  /**
   * A Host id, or `mcp` for adapter tool calls.
   */
  source: string;
  /**
   * Wire tag such as `exec.session.step` or `tool.finished`.
   */
  kind: string;
  exec_id: string | null;
  session_id: string | null;
  /**
   * One line without payloads, answers, params, or outcomes.
   */
  text: string;
  level: Level;
};

export type Level = "info" | "warn" | "error";

export type BlobRow = { key: string; host: string; hash: string; length: number };

export type RowBatch =
  | { collection: "hosts"; ops: Array<RowOp<HostRow>> }
  | { collection: "executions"; ops: Array<RowOp<ExecutionRow>> }
  | { collection: "steps"; ops: Array<RowOp<StepRow>> }
  | { collection: "callouts"; ops: Array<RowOp<CalloutRow>> }
  | { collection: "programs"; ops: Array<RowOp<ProgramRow>> }
  | { collection: "receipts"; ops: Array<RowOp<ReceiptRow>> }
  | { collection: "offers"; ops: Array<RowOp<OfferRow>> }
  | { collection: "activity"; ops: Array<RowOp<ActivityRow>> }
  | { collection: "blobs"; ops: Array<RowOp<BlobRow>> };

export type RowOp<T> = { op: "upsert"; row: T } | { op: "delete"; key: string };

export type VerifyTarget =
  | { kind: "stored"; receipt_id: string }
  | { kind: "inline"; artifact: JsonValue };

export type JoinTarget = { creator: string; negotiation_id: string };

export type ViewReply = {
  /**
   * Index of the latest agreed step applied to the rendered state; `None`
   * is the initial state before step 0.
   */
  step: number | null;
  /**
   * Slot text: UTF-8 with ANSI SGR sequences only.
   */
  header: string | null;
  agents: string | null;
  state: string | null;
  status_bar: string | null;
  /**
   * Typed blocks the program rendered next to its text slots, validated
   * by the daemon. Empty for programs that render text only.
   */
  blocks: Array<BlockRow>;
};

export type BlockRow =
  | { kind: "facts"; title: string | null; items: Array<FactRow> }
  | { kind: "table"; title: string | null; columns: Array<string>; rows: Array<Array<CellRow>> }
  | {
      kind: "board";
      title: string | null;
      rows: number;
      cols: number;
      cells: Array<CellRow>;
      row_labels: Array<string>;
      col_labels: Array<string>;
    }
  | { kind: "progress"; label: string; value: number; max: number }
  | { kind: "roster"; title: string | null; entries: Array<RosterEntryRow> };

export type FactRow = { label: string; value: CellRow };

export type CellRow = {
  text: string;
  tone: ToneRow;
  /**
   * Participant index in the committed ensemble, for consistent colour.
   */
  participant: number | null;
};

export type RosterEntryRow = { participant: number; status: CellRow; detail: string | null };

export type ToneRow = "normal" | "muted" | "good" | "warn" | "bad" | "highlight";

export type RecordsReply = {
  from: number;
  total: number;
  next: number | null;
  records: Array<RecordRow>;
};

export type RecordRow = {
  position: number;
  /**
   * Agreed steps this event produced.
   */
  steps: Array<number>;
  event: RecordEventKind;
  input_bytes: number | null;
  effects: Array<EffectRow>;
};

export type RecordEventKind =
  | "session_started"
  | "message_received"
  | "input_received"
  | "timer_fired"
  | "direct_received";

export type EffectRow = { kind: EffectKindRow; bytes: number | null };

export type EffectKindRow =
  | "session_end"
  | "session_abort"
  | "broadcast"
  | "set_timer"
  | "fail"
  | "send_direct";

export type VerifyReply = {
  receipt_id: string;
  program: string;
  session_id: string;
  /**
   * Participants in canonical order.
   */
  ensemble: Array<string>;
  steps: number;
  termination: Termination;
};

export type Termination = { kind: "completed" } | { kind: "stopped"; cause: string };

export type CreatedReply = { exec_id: string; negotiation_id: string | null };

export type ProgramImported = { hash: string; hosts: Array<string> };

export type ExecRef = { host: string; exec_id: string };

export type BlobImported = { hash: string; length: number };

export type CollectionName = RowBatch["collection"];
export type RowOf<C extends CollectionName> = Extract<
  RowBatch,
  { collection: C }
>["ops"][number] extends infer O
  ? O extends { op: "upsert"; row: infer R }
    ? R
    : never
  : never;
