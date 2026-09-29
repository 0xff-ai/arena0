import type {
  BlobImported,
  CreatedReply,
  ErrorCode,
  HostRow,
  JsonValue,
  LaunchReply,
  ProgramImported,
  RecordsReply,
  VerifyReply,
  ViewReply,
} from "./protocol";

/** The reply of each operation. The gateway fixes the shape per operation (see `Op` in protocol.rs). */
export interface OpReplies {
  view: ViewReply;
  records: RecordsReply;
  query: JsonValue;
  receipt: JsonValue;
  verify: VerifyReply;
  answer: null;
  create: CreatedReply;
  join: CreatedReply;
  launch: LaunchReply;
  withdraw: null;
  terminate: null;
  program_import: ProgramImported;
  program_remove: null;
  receipt_import: { receipt_id: string };
  blob_import: BlobImported;
  blob_export: { length: number };
  host_open: HostRow;
  daemon_stop: null;
}

/** A failed call: the daemon's error code and message, or `gateway` for failures of the socket itself. */
export class GatewayError extends Error {
  constructor(
    readonly code: ErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "GatewayError";
  }
}

/**
 * Message of the `gateway` error for a call whose socket closed before its
 * reply: the operation may or may not have run, and the page never retries it.
 */
export const OUTCOME_UNKNOWN = "outcome unknown";
