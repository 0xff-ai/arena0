import { GatewayError, type OpReplies, OUTCOME_UNKNOWN } from "./ops";
import type { Hello, JsonValue, OpArgs, OpName, RowBatch, ServerFrame } from "./protocol";

export type ConnectionStatus = "connecting" | "syncing" | "live" | "offline";

export interface ConnectionState {
  status: ConnectionStatus;
  /** Consecutive connects that closed before `ready`; back to 0 on `ready`. */
  attempt: number;
  /** Unix ms of the last status change. */
  since: number;
  /** Why the last socket closed, in words; cleared on `ready`. */
  error: string | null;
}

export interface Gateway {
  /** The same object until the state changes, so it works as a store snapshot. */
  state(): ConnectionState;
  onState(listener: (state: ConnectionState) => void): () => void;
  /** The latest `hello`. Kept while offline; replaced by the next connection's `hello`. */
  hello(): Hello | null;
  /** Never retried: a call is sent once, and a socket that closes first rejects it with `OUTCOME_UNKNOWN`. */
  call<K extends OpName>(op: K, args: OpArgs<K>): Promise<OpReplies[K]>;
  /** Listeners outlive reconnects; the gateway sends fresh `reset` batches on every connection. */
  onRows(listener: (reset: boolean, batch: RowBatch) => void): () => void;
  onReady(listener: () => void): () => void;
  /** Stops reconnecting and closes the current socket. */
  close(): void;
}

const SUBPROTOCOL = "arena0.v1";
const TOKEN_SUBPROTOCOL_PREFIX = "arena0.token.";
const BACKOFF_MIN_MS = 250;
const BACKOFF_MAX_MS = 5000;
/** Application close code: a frame the page cannot read. */
const CLOSE_PROTOCOL_ERROR = 4000;
const TOKEN_KEY = "arena0.token";

interface PendingCall {
  resolve(value: JsonValue): void;
  reject(error: GatewayError): void;
}

/**
 * Owns one WebSocket at a time and reconnects with backoff until `close()`.
 *
 * The gateway and this page ship in one build, so a frame that does not parse
 * is a bug, not a condition to tolerate: the socket is closed with code 4000
 * and the reason is left in `state().error`.
 */
export function connectGateway(options: { url: string; token: string }): Gateway {
  const stateListeners = new Set<(state: ConnectionState) => void>();
  const rowListeners = new Set<(reset: boolean, batch: RowBatch) => void>();
  const readyListeners = new Set<() => void>();
  // Ids grow for the life of the gateway and are never reused, so a reply that
  // outlives its socket can never match a call made on a later one.
  const pending = new Map<number, PendingCall>();
  let nextId = 0;
  let state: ConnectionState = { status: "connecting", attempt: 0, since: Date.now(), error: null };
  let hello: Hello | null = null;
  let socket: WebSocket | null = null;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  let closed = false;

  function transition(status: ConnectionStatus, patch: Partial<ConnectionState> = {}) {
    state = { ...state, ...patch, status, since: Date.now() };
    for (const listener of stateListeners) listener(state);
  }

  function rejectInFlight() {
    const calls = [...pending.values()];
    pending.clear();
    for (const call of calls) call.reject(new GatewayError("gateway", OUTCOME_UNKNOWN));
  }

  function connect() {
    reconnectTimer = null;
    transition("connecting");
    const ws = new WebSocket(options.url, [SUBPROTOCOL, TOKEN_SUBPROTOCOL_PREFIX + options.token]);
    socket = ws;
    let opened = false;
    let sawReady = false;
    let protocolError: string | null = null;

    ws.onopen = () => {
      opened = true;
    };

    ws.onmessage = (event) => {
      // Frames that arrive between our close() and the close event are dropped.
      if (protocolError !== null) return;
      const frame = parseFrame(event.data);
      if (frame === null) {
        protocolError = "the gateway sent a frame this page cannot read";
        ws.close(CLOSE_PROTOCOL_ERROR, protocolError);
        return;
      }
      switch (frame.t) {
        case "hello":
          hello = frame;
          transition("syncing");
          break;
        case "rows":
          for (const listener of rowListeners) listener(frame.reset, frame.batch);
          break;
        case "ready":
          sawReady = true;
          transition("live", { attempt: 0, error: null });
          for (const listener of readyListeners) listener();
          break;
        case "reply": {
          const call = pending.get(frame.id);
          if (call === undefined) return;
          pending.delete(frame.id);
          if ("err" in frame.result) {
            call.reject(new GatewayError(frame.result.err.code, frame.result.err.message));
          } else {
            call.resolve(frame.result.ok);
          }
          break;
        }
      }
    };

    ws.onclose = (event) => {
      if (socket === ws) socket = null;
      rejectInFlight();
      const attempt = sawReady ? 0 : state.attempt + 1;
      transition("offline", { attempt, error: protocolError ?? describeClose(event, opened) });
      if (!closed) reconnectTimer = setTimeout(connect, backoffMs(attempt));
    };
  }

  connect();

  return {
    state: () => state,
    onState(listener) {
      stateListeners.add(listener);
      return () => stateListeners.delete(listener);
    },
    hello: () => hello,
    call<K extends OpName>(op: K, args: OpArgs<K>): Promise<OpReplies[K]> {
      const ws = socket;
      if (ws === null || ws.readyState !== WebSocket.OPEN) {
        return Promise.reject(new GatewayError("gateway", "not connected"));
      }
      const id = ++nextId;
      return new Promise<OpReplies[K]>((resolve, reject) => {
        // The gateway fixes each operation's reply shape, so OpReplies[K] holds.
        pending.set(id, { resolve: (value) => resolve(value as OpReplies[K]), reject });
        // Arg-less operations are `{ op }` on the wire.
        const wireOp = args === null ? { op } : { op, args };
        ws.send(JSON.stringify({ t: "call", id, op: wireOp }));
      });
    },
    onRows(listener) {
      rowListeners.add(listener);
      return () => rowListeners.delete(listener);
    },
    onReady(listener) {
      readyListeners.add(listener);
      return () => readyListeners.delete(listener);
    },
    close() {
      if (closed) return;
      closed = true;
      if (reconnectTimer !== null) clearTimeout(reconnectTimer);
      const ws = socket;
      if (ws === null) return;
      // Detach first: this socket's close event must not schedule a reconnect
      // or overwrite the reason.
      socket = null;
      ws.onopen = ws.onmessage = ws.onclose = null;
      ws.close(1000, "page closed");
      rejectInFlight();
      transition("offline", { error: "closed by this page" });
    },
  };
}

/** 250 ms after the first failed connect or after a live socket drops (`attempt` 0), doubling to 5 s. */
function backoffMs(attempt: number): number {
  return Math.min(BACKOFF_MIN_MS * 2 ** Math.max(attempt - 1, 0), BACKOFF_MAX_MS);
}

function parseFrame(data: unknown): ServerFrame | null {
  if (typeof data !== "string") return null;
  let frame: unknown;
  try {
    frame = JSON.parse(data);
  } catch {
    return null;
  }
  if (typeof frame !== "object" || frame === null || !("t" in frame)) return null;
  switch (frame.t) {
    case "hello":
    case "rows":
    case "ready":
    case "reply":
      // Only the discriminant is checked: same-build peers agree on the rest.
      return frame as ServerFrame;
    default:
      return null;
  }
}

function describeClose(event: CloseEvent, opened: boolean): string {
  if (event.reason !== "") return event.reason;
  // A refused upgrade (wrong token, Host or Origin) and a gateway that is not
  // running look the same from the page: the handshake never completed.
  if (!opened) return "could not connect: the gateway is not running or refused this page's token";
  if (event.code === 1000 || event.code === 1001) return "the gateway closed the connection";
  return `connection lost (code ${event.code})`;
}

/** Read `#token=` from the URL once, keep it in sessionStorage, and strip it from the address bar. */
export function readToken(): string | null {
  const fromHash = new URLSearchParams(location.hash.slice(1)).get("token");
  if (fromHash !== null && fromHash !== "") {
    sessionStorage.setItem(TOKEN_KEY, fromHash);
    // Keep history.state: the router stores its position there.
    history.replaceState(history.state, "", location.pathname + location.search);
  }
  return sessionStorage.getItem(TOKEN_KEY);
}
