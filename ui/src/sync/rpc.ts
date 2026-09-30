import type {
  ApiErrorCode,
  HostRequest,
  Request,
  Response,
  ResponseOk,
  Uploaded,
} from "~/api/types.gen";

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

/** POST /rpc once. Resolves Ok, rejects DaemonError with the daemon's Err. */
export async function rpc(request: Request): Promise<ResponseOk> {
  const reply: Response | null = await fetch("/rpc", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(request),
  })
    .then((response) => (response.ok ? response.json() : null))
    .catch(() => null);
  if (reply === null) throw new DaemonError("transport", OUTCOME_UNKNOWN);
  if ("Err" in reply) throw new DaemonError(reply.Err.code, reply.Err.message);
  return reply.Ok;
}

type Variant = ResponseOk extends infer R ? (R extends object ? keyof R : never) : never;

/** Narrows a reply to the variant its request produces; any other reply is a UI/daemon mismatch. */
export function assertReply<K extends Variant>(
  reply: ResponseOk,
  variant: K,
): asserts reply is Extract<ResponseOk, Record<K, unknown>> {
  if (typeof reply !== "object" || !(variant in reply))
    throw new Error(`expected a ${variant} reply`);
}

/** Call one Host in the daemon's local namespace. */
export async function hostCall(host: string, request: HostRequest): Promise<ResponseOk> {
  return rpc({ method: "host.call", params: { host, request } });
}

/** POST /uploads with the given Content-Type; 201 → Uploaded. */
export async function upload(
  bytes: Blob,
  contentType: "application/wasm" | "application/octet-stream",
): Promise<Uploaded> {
  let response: globalThis.Response;
  try {
    response = await fetch("/uploads", {
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
}

/** The daemon's download route for an already stored blob. */
export function blobUrl(host: string, hash: string): string {
  return `/hosts/${encodeURIComponent(host)}/blobs/${hash}`;
}
