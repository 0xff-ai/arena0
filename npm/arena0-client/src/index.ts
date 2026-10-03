// The arena0 daemon's HTTP API for TypeScript: the generated request,
// response and event types, and a small client for /rpc, /uploads, blob
// downloads and the /events stream.
import type {
  ActivityFrame,
  ApiErrorCode,
  EventFrame,
  HostRequest,
  Request,
  Response,
  ResponseOk,
  Uploaded,
} from "./types.gen.js";

export type * from "./types.gen.js";

/** A failed call: the daemon's error code, or `transport` when no reply arrived. */
export class DaemonError extends Error {
  constructor(
    readonly code: ApiErrorCode | "transport",
    message: string,
  ) {
    super(message);
    this.name = "DaemonError";
  }
}

/** Message of a transport error whose request may or may not have run; never retried. */
export const OUTCOME_UNKNOWN = "outcome unknown";

/** A variant name a `ResponseOk` can carry, e.g. `"DaemonInfo"`. */
export type Variant = ResponseOk extends infer R ? (R extends object ? keyof R : never) : never;

/** Narrows a reply to the variant its request produces; any other reply is a client/daemon mismatch and throws. */
export function assertReply<K extends Variant>(
  reply: ResponseOk,
  variant: K,
): asserts reply is Extract<ResponseOk, Record<K, unknown>> {
  if (typeof reply !== "object" || !(variant in reply))
    throw new Error(`expected a ${variant} reply`);
}

/** Handlers for the `/events` stream: `host` events carry an `EventFrame`, `activity` events an `ActivityFrame`, each as JSON data. */
export interface EventHandlers {
  host?(frame: EventFrame): void;
  activity?(frame: ActivityFrame): void;
}

/** One daemon's HTTP API. */
export interface Client {
  /** The daemon's origin without a trailing slash, e.g. `http://127.0.0.1:43127`; `""` addresses the page's own origin. */
  readonly baseUrl: string;
  /** POST /rpc once. Resolves the daemon's Ok, rejects `DaemonError` with its Err, or with `transport` and `OUTCOME_UNKNOWN` when no reply arrived. Never retries. */
  rpc(request: Request): Promise<ResponseOk>;
  /** Call one Host in the daemon's local namespace (`host.call`). */
  hostCall(host: string, request: HostRequest): Promise<ResponseOk>;
  /** POST /uploads. 201 resolves `Uploaded`; any other status rejects `DaemonError("transport", "<status> <statusText>")`, a network failure `"0 Network Error"`. */
  upload(bytes: Blob, contentType: "application/wasm" | "application/octet-stream"): Promise<Uploaded>;
  /** The download URL of a blob the Host already stores. */
  blobUrl(host: string, hash: string): string;
  /** Open `/events` with the global `EventSource` and pass each parsed frame to its handler. The caller owns the returned source: its `onopen`, `onerror` and `close()`. */
  events(handlers: EventHandlers): EventSource;
}

/** A client for the daemon at `baseUrl`. The default `""` suits pages the daemon, or a proxy in front of it, serves. */
export function createClient(baseUrl = ""): Client {
  baseUrl = baseUrl.replace(/\/+$/, "");
  const rpc = async (request: Request): Promise<ResponseOk> => {
    const reply: Response | null = await fetch(`${baseUrl}/rpc`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(request),
    })
      .then((response) => (response.ok ? response.json() : null))
      .catch(() => null);
    if (reply === null) throw new DaemonError("transport", OUTCOME_UNKNOWN);
    if ("Err" in reply) throw new DaemonError(reply.Err.code, reply.Err.message);
    return reply.Ok;
  };
  return {
    baseUrl,
    rpc,
    async hostCall(host, request) {
      return rpc({ method: "host.call", params: { host, request } });
    },
    async upload(bytes, contentType) {
      let response: globalThis.Response;
      try {
        response = await fetch(`${baseUrl}/uploads`, {
          method: "POST",
          headers: { "Content-Type": contentType },
          body: bytes,
        });
      } catch {
        // A network failure has no HTTP status. Use the browser's status-zero convention.
        throw new DaemonError("transport", "0 Network Error");
      }
      if (response.status !== 201)
        throw new DaemonError("transport", `${response.status} ${response.statusText}`);
      return response.json();
    },
    blobUrl(host, hash) {
      return `${baseUrl}/hosts/${encodeURIComponent(host)}/blobs/${hash}`;
    },
    events(handlers) {
      const source = new EventSource(`${baseUrl}/events`);
      source.addEventListener("host", (event) => {
        const frame: EventFrame = JSON.parse((event as MessageEvent<string>).data);
        handlers.host?.(frame);
      });
      source.addEventListener("activity", (event) => {
        const frame: ActivityFrame = JSON.parse((event as MessageEvent<string>).data);
        handlers.activity?.(frame);
      });
      return source;
    },
  };
}
