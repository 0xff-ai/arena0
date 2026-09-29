import type { Cell, HostRequest, ReceiptArtifact } from "~/api/types.gen";
import { hostRow } from "./project";
import type {
  BlobImported,
  CreatedReply,
  HostRow,
  JoinTarget,
  JsonValue,
  ProgramImported,
  RecordsReply,
  VerifyReply,
  VerifyTarget,
  ViewReply,
} from "./rows";
import { assertReply, DaemonError, hostCall, rpc, upload } from "./rpc";

export interface LaunchArgs {
  program: string;
  params: JsonValue | null;
  seats: Seat[];
}
export interface Seat {
  host: string;
  driver: "you" | "external";
}
export interface LaunchReply {
  execs: { host: string; exec_id: string }[];
}
export type OpName = keyof OpReplies;
export type OpArgs<K extends OpName> = {
  view: { host: string; exec_id: string; width: number; at_step: number | null };
  records: { host: string; exec_id: string; from: number | null; limit: number };
  query: { host: string; exec_id: string; query: JsonValue };
  receipt: { host: string; receipt_id: string };
  verify: { host: string; target: VerifyTarget };
  answer: { host: string; exec_id: string; pending_id: string; answer: JsonValue };
  create: {
    host: string;
    program: string;
    params: JsonValue | null;
    participants: number;
    blobs: string[];
  };
  join: { host: string; program: string; target: JoinTarget | null; blobs: string[] };
  launch: LaunchArgs;
  withdraw: { host: string; exec_id: string };
  terminate: { host: string; exec_id: string };
  program_import: { hosts: string[]; file: File };
  program_remove: { host: string; program: string };
  receipt_import: { host: string; artifact: JsonValue };
  blob_import: { host: string; file: File };
  host_open: { id: string | null; user_agent: string };
}[K];

/** The row-facing reply of each operation. */
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
  host_open: HostRow;
}

/** exec.new with a fresh random exec_id. */
async function execNew(
  host: string,
  params: Omit<Extract<HostRequest, { method: "exec.new" }>["params"], "exec_id">,
): Promise<CreatedReply> {
  const exec_id = Array.from(crypto.getRandomValues(new Uint8Array(32)), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
  const reply = await hostCall(host, { method: "exec.new", params: { exec_id, ...params } });
  assertReply(reply, "ExecCreated");
  return reply.ExecCreated;
}

/** Each operation sends requests once; failures are returned to its caller. */
export const ops: { [K in OpName]: (args: OpArgs<K>) => Promise<OpReplies[K]> } = {
  async view({ host, exec_id, width, at_step }) {
    const reply = await hostCall(host, {
      method: "exec.view",
      params: { exec: exec_id, width, at_step, color: "TrueColor" },
    });
    assertReply(reply, "ExecView");
    const { step, view } = reply.ExecView;
    // Wire cells omit absent participant indexes; rows have an explicit null.
    const cell = (value: Cell) => ({ ...value, participant: value.participant ?? null });
    return {
      step,
      header: view.slots.Header ?? null,
      agents: view.slots.Agents ?? null,
      state: view.slots.State ?? null,
      status_bar: view.slots.StatusBar ?? null,
      blocks: (view.blocks ?? []).map((block) => {
        switch (block.kind) {
          case "facts":
            return {
              ...block,
              items: block.items.map((item) => ({ ...item, value: cell(item.value) })),
            };
          case "table":
            return { ...block, rows: block.rows.map((row) => row.map(cell)) };
          case "board":
            return { ...block, cells: block.cells.map(cell) };
          case "roster":
            return {
              ...block,
              entries: block.entries.map((entry) => ({ ...entry, status: cell(entry.status) })),
            };
          case "progress":
            return block;
        }
      }),
    };
  },
  async records({ host, exec_id, from, limit }) {
    const reply = await hostCall(host, {
      method: "exec.inspect",
      params: { exec_id, events_from: from, events_limit: limit },
    });
    assertReply(reply, "Inspection");
    const inspection = reply.Inspection;
    return {
      from: inspection.events_from,
      total: inspection.events_total,
      next: inspection.events_next,
      records: inspection.events.map((record) => ({
        position: record.event_position,
        steps: record.agreed_steps,
        event: record.event,
        input_bytes: record.input_payload_bytes,
        effects: record.effects.map((effect) => ({
          kind: effect.kind,
          bytes: effect.payload_bytes,
        })),
      })),
    };
  },
  async query({ host, exec_id, query }) {
    const reply = await hostCall(host, { method: "exec.query", params: { exec_id, query } });
    assertReply(reply, "Query");
    return reply.Query.result;
  },
  async receipt({ host, receipt_id }) {
    const reply = await hostCall(host, {
      method: "receipt.get",
      params: { receipt: { Stored: receipt_id } },
    });
    assertReply(reply, "Receipt");
    return reply.Receipt;
  },
  async verify({ host, target }) {
    // The daemon validates uploaded artifact JSON at the trust boundary.
    const receipt =
      target.kind === "stored"
        ? { Stored: target.receipt_id }
        : { Inline: target.artifact as ReceiptArtifact };
    const reply = await hostCall(host, { method: "receipt.verify", params: { receipt } });
    assertReply(reply, "Verified");
    const summary = reply.Verified;
    let termination: VerifyReply["termination"] = { kind: "completed" };
    if (summary.terminal !== "Completed") {
      const cause = summary.terminal.Stopped.cause;
      const value = "Authenticated" in cause ? cause.Authenticated : cause.Shared;
      const step =
        "Authenticated" in cause
          ? cause.Authenticated.coordinate.next_step
          : cause.Shared.commitment.step;
      termination = {
        kind: "stopped",
        cause: `${value.kind === 0 ? "aborted" : "failed"} at step ${step}: ${value.reason}`,
      };
    }
    return {
      receipt_id: summary.receipt_id,
      program: summary.program_id,
      session_id: summary.session_id,
      ensemble: summary.ensemble,
      steps: summary.steps,
      termination,
    };
  },
  async answer({ host, ...params }) {
    const reply = await hostCall(host, { method: "exec.submit", params });
    if (reply !== "Ack") throw new Error("unexpected reply to exec.submit");
    return null;
  },
  async create({ host, program, params, participants, blobs }) {
    return execNew(host, {
      program,
      params,
      blobs,
      ensemble: { Create: { participant_count: participants } },
    });
  },
  async join({ host, program, target, blobs }) {
    return execNew(host, { program, params: null, blobs, ensemble: { Join: { target } } });
  },
  async launch({ program, params, seats }) {
    const execs: LaunchReply["execs"] = [];
    try {
      const host = seats[0]!.host;
      const info = await hostCall(host, { method: "host.info" });
      assertReply(info, "HostStatus");
      const created = await ops.create({
        host,
        program,
        params,
        participants: seats.length,
        blobs: [],
      });
      execs.push({ host, exec_id: created.exec_id });
      // A Create always selects its own negotiation.
      const target = {
        creator: info.HostStatus.host.peer_id,
        negotiation_id: created.negotiation_id!,
      };
      for (const seat of seats.slice(1)) {
        const joined = await ops.join({ host: seat.host, program, target, blobs: [] });
        execs.push({ host: seat.host, exec_id: joined.exec_id });
      }
      return { execs };
    } catch (error) {
      // Cleanup cannot replace the original failure, even if a Host has gone away.
      await Promise.allSettled(execs.map((exec) => ops.withdraw(exec)));
      throw error;
    }
  },
  async withdraw({ host, exec_id }) {
    const reply = await hostCall(host, { method: "exec.withdraw", params: { exec_id } });
    if (reply !== "Ack") throw new Error("unexpected reply to exec.withdraw");
    return null;
  },
  async terminate({ host, exec_id }) {
    const reply = await hostCall(host, {
      method: "exec.terminate",
      params: { exec_id, reason: "terminated from the web UI" },
    });
    if (reply !== "Ack") throw new Error("unexpected reply to exec.terminate");
    return null;
  },
  async program_import({ hosts, file }) {
    const uploaded = await upload(file, "application/wasm");
    if (hosts.length === 0) {
      const reply = await rpc({ method: "hosts.list" });
      assertReply(reply, "Hosts");
      hosts = reply.Hosts.map((status) => status.host.id);
    }
    if (hosts.length === 0) throw new DaemonError("BadRequest", "no Host to import into");
    const replies = await Promise.all(
      hosts.map((host) =>
        hostCall(host, {
          method: "program.import",
          params: { source: { upload: uploaded.upload } },
        }),
      ),
    );
    let hash = "";
    for (const reply of replies) {
      assertReply(reply, "Program");
      hash = reply.Program.summary.program_hash;
    }
    return { hash, hosts };
  },
  async program_remove({ host, program }) {
    const reply = await hostCall(host, { method: "program.remove", params: { program } });
    if (reply !== "Ack") throw new Error("unexpected reply to program.remove");
    return null;
  },
  async receipt_import({ host, artifact }) {
    const reply = await hostCall(host, {
      method: "receipt.import",
      params: { receipt: artifact as ReceiptArtifact },
    });
    if (typeof reply !== "object" || !("ReceiptList" in reply) || reply.ReceiptList.length === 0)
      throw new Error("unexpected reply to receipt.import");
    return { receipt_id: reply.ReceiptList[0]!.receipt_id };
  },
  async blob_import({ host, file }) {
    const uploaded = await upload(file, "application/octet-stream");
    const reply = await hostCall(host, {
      method: "blob.import",
      params: { source: { upload: uploaded.upload } },
    });
    assertReply(reply, "BlobImported");
    return reply.BlobImported;
  },
  async host_open(params) {
    const reply = await rpc({ method: "hosts.open", params });
    assertReply(reply, "HostOpened");
    const status = await hostCall(reply.HostOpened.id, { method: "host.info" });
    assertReply(status, "HostStatus");
    return hostRow(status.HostStatus, true, 0, null);
  },
};
