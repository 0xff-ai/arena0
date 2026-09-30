import type { ExecEndPhase, ExecEndStatus, JsonValue } from "~/api/types.gen";
import type {
  CalloutRow,
  ExecutionRow,
  HostRow,
  Lifecycle,
  ProgramRow,
  ReceiptRow,
  TerminalRow,
} from "../sync";

export type SessionState =
  | "negotiating"
  | "activating"
  | "active"
  | "ending"
  | "completed"
  | "aborted"
  | "failed";

export interface Participant {
  index: number;
  peerId: string;
  /** The local Host with this peer id; null for a remote participant. */
  host: string | null;
  userAgent: string | null;
  /** That Host's execution in this session. */
  execution: ExecutionRow | null;
}

/** One agreement between participants, seen through every local Host that takes part. */
export interface Session {
  key: string;
  sessionId: string | null;
  negotiationId: string | null;
  programHash: string;
  program: ProgramRow | null;
  /** By participant index when the activation is known, else by Host id. */
  executions: ExecutionRow[];
  participants: Participant[];
  state: SessionState;
  /** Index of the latest agreed step across local Hosts; null before step 0. */
  latestStep: number | null;
  createdMs: number;
  updatedMs: number;
  lastStepMs: number | null;
  /** Open callouts, oldest first. */
  callouts: CalloutRow[];
  receipts: ReceiptRow[];
  /** Offer terms, from the first execution that has an activation. */
  offerHash: string | null;
  params: JsonValue | null;
  initialState: string | null;
  creator: string | null;
  targetSize: number | null;
  /** From the execution furthest along. */
  writer: string | null;
  phase: string | null;
  terminal: TerminalRow | null;
  end: ExecEndStatus;
}

/** The session has ended, however it ended; `ending` is still in progress. */
export function isTerminal(session: Session): boolean {
  return session.state === "completed" || session.state === "failed" || session.state === "aborted";
}

/**
 * `negotiation_id` when the execution has one, else its own key. Every Host's
 * execution of one negotiation shares it from creation, so the key does not
 * change when the session activates.
 */
export function sessionKey(execution: ExecutionRow): string {
  return execution.negotiation_id ?? `e:${execution.key}`;
}

const STATE_ORDER: Record<SessionState, number> = {
  active: 0,
  activating: 1,
  negotiating: 2,
  ending: 3,
  failed: 4,
  aborted: 5,
  completed: 6,
};

/**
 * Groups executions into sessions. Sessions that need an answer come first,
 * then by state, then most recently updated. `lastStepMs` maps an execution
 * key to the `certified_ms` of its latest step.
 */
export function deriveSessions(input: {
  executions: ExecutionRow[];
  programs: ProgramRow[];
  callouts: CalloutRow[];
  receipts: ReceiptRow[];
  hosts: HostRow[];
  lastStepMs: Map<string, number>;
}): Session[] {
  const hostsById = new Map(input.hosts.map((host) => [host.id, host]));
  const hostsByPeer = new Map(input.hosts.map((host) => [host.peer_id, host]));
  const programs = new Map(input.programs.map((program) => [program.hash, program]));

  const executionsBySession = groupBy(input.executions, sessionKey);
  const sessionOfExecution = new Map<string, string>();
  for (const [key, executions] of executionsBySession) {
    for (const execution of executions) sessionOfExecution.set(execution.key, key);
  }
  // A callout row carries `{host, exec_id}`, which is its execution's key.
  const calloutsBySession = groupBy(input.callouts, (callout) =>
    sessionOfExecution.get(`${callout.host}/${callout.exec_id}`),
  );
  const receiptsBySessionId = groupBy(input.receipts, (receipt) => receipt.session_id);

  const sessions: Session[] = [];
  for (const [key, group] of executionsBySession) {
    const byHost = [...group].sort((a, b) => compareText(a.host, b.host));
    const activation =
      byHost.find((execution) => execution.activation !== null)?.activation ?? null;

    // Index in the activation; Hosts outside it (unknown until it exists) sort after, by Host id.
    const ranked = byHost.map((execution) => {
      const peerId = hostsById.get(execution.host)?.peer_id;
      const index = activation?.participants.findIndex((p) => p.peer_id === peerId) ?? -1;
      return { execution, rank: index >= 0 ? index : Number.POSITIVE_INFINITY };
    });
    ranked.sort((a, b) => (a.rank === b.rank ? 0 : a.rank < b.rank ? -1 : 1));
    const executions = ranked.map((r) => r.execution);

    const participants: Participant[] =
      activation === null
        ? executions.map((execution, index) => {
            const host = hostsById.get(execution.host);
            return {
              index,
              peerId: host?.peer_id ?? "",
              host: execution.host,
              userAgent: host?.user_agent ?? null,
              execution,
            };
          })
        : activation.participants.map((participant, index) => {
            const host = hostsByPeer.get(participant.peer_id);
            return {
              index,
              peerId: participant.peer_id,
              host: host?.id ?? null,
              userAgent: host?.user_agent ?? null,
              execution: executions.find((execution) => execution.host === host?.id) ?? null,
            };
          });

    const sessionId = executions.find((e) => e.session_id !== null)?.session_id ?? null;
    // Every execution of a session runs one program, so any of them names it.
    const furthest = executions.reduce((best, e) =>
      (e.latest_step ?? -1) > (best.latest_step ?? -1) ? e : best,
    );
    const state = aggregateState(executions);

    sessions.push({
      key,
      sessionId,
      negotiationId: executions.find((e) => e.negotiation_id !== null)?.negotiation_id ?? null,
      programHash: furthest.program,
      program: programs.get(furthest.program) ?? null,
      executions,
      participants,
      state,
      latestStep: maxOf(executions.map((e) => e.latest_step)),
      createdMs: Math.min(...executions.map((e) => e.created_ms)),
      updatedMs: Math.max(...executions.map((e) => e.updated_ms)),
      lastStepMs: maxOf(executions.map((e) => input.lastStepMs.get(e.key) ?? null)),
      callouts: [...(calloutsBySession.get(key) ?? [])].sort(
        (a, b) => a.opened_ms - b.opened_ms || compareText(a.key, b.key),
      ),
      receipts: [...(sessionId === null ? [] : (receiptsBySessionId.get(sessionId) ?? []))].sort(
        (a, b) => compareText(a.key, b.key),
      ),
      offerHash: activation?.offer_hash ?? null,
      params: activation?.params ?? null,
      initialState: activation?.initial_state ?? null,
      creator: activation?.creator ?? null,
      targetSize: activation?.target_size ?? null,
      writer: furthest.writer,
      phase: furthest.phase,
      terminal: sessionTerminal(executions),
      end: sessionEnd(executions),
    });
  }
  return sessions.sort(compareSessions);
}

function aggregateState(executions: ExecutionRow[]): SessionState {
  const any = (lifecycle: Lifecycle) => executions.some((e) => e.lifecycle === lifecycle);
  if (any("negotiating")) return "negotiating";
  if (any("activating")) return "activating";
  if (any("active")) return "active";
  // Everything is terminal.
  if (executions.some((e) => e.end.phase === "ending")) return "ending";
  if (any("failed")) return "failed";
  if (any("aborted")) return "aborted";
  return "completed";
}

function sessionEnd(executions: ExecutionRow[]): ExecEndStatus {
  const phases = executions.map((e) => e.end.phase);
  const phase: ExecEndPhase = phases.includes("ending")
    ? "ending"
    : phases.every((p) => p === "ended")
      ? "ended"
      : "open";
  return { phase, unconfirmed: [...new Set(executions.flatMap((e) => e.end.unconfirmed))] };
}

/**
 * The terminal record that explains the session's outcome: failure first,
 * then abort, then completion, preferring a record that carries a reason or
 * outcome. Null until every execution is terminal.
 */
function sessionTerminal(executions: ExecutionRow[]): TerminalRow | null {
  if (executions.some((e) => e.terminal === null)) return null;
  const terminals = executions.flatMap((e) => (e.terminal === null ? [] : [e.terminal]));
  for (const kind of ["failed", "aborted", "completed"] as const) {
    const ofKind = terminals.filter((t) => t.kind === kind);
    if (ofKind.length > 0) {
      return ofKind.find((t) => t.reason !== null || t.outcome !== null) ?? ofKind[0] ?? null;
    }
  }
  return null;
}

function compareSessions(a: Session, b: Session): number {
  const needsA = a.callouts.length > 0;
  const needsB = b.callouts.length > 0;
  if (needsA !== needsB) return needsA ? -1 : 1;
  return (
    STATE_ORDER[a.state] - STATE_ORDER[b.state] ||
    b.updatedMs - a.updatedMs ||
    compareText(a.key, b.key)
  );
}

function compareText(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

function maxOf(values: Array<number | null>): number | null {
  let max: number | null = null;
  for (const value of values) if (value !== null && (max === null || value > max)) max = value;
  return max;
}

/** Items by key, in input order; items whose key is undefined are left out. */
export function groupBy<T>(items: T[], key: (item: T) => string | undefined): Map<string, T[]> {
  const groups = new Map<string, T[]>();
  for (const item of items) {
    const k = key(item);
    if (k === undefined) continue;
    const group = groups.get(k);
    if (group === undefined) groups.set(k, [item]);
    else group.push(item);
  }
  return groups;
}
