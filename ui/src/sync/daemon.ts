import type {
  ActivationInspection,
  ActivityFrame,
  DaemonInfo,
  EventFrame,
  ExecStatus,
  HostRequest,
  HostStatus,
  OpenOffer,
} from "~/api/types.gen";
import {
  activityRow,
  blobRow,
  calloutRow,
  eventActivity,
  executionRow,
  hostRow,
  negotiationMark,
  offerRow,
  programRow,
  receiptRow,
  stepRow,
} from "./project";
import type { CollectionName, NegotiationMark, RowBatch, RowOf } from "./rows";
import { assertReply, DaemonError, hostCall, rpc } from "./rpc";

export type ConnectionStatus = "connecting" | "syncing" | "live" | "offline";
export interface ConnectionState {
  status: ConnectionStatus;
  attempt: number;
  since: number;
  error: string | null;
}

/** The page's one connection to the daemon: /events, /rpc, and the rows built from them. */
export interface Daemon {
  state(): ConnectionState;
  onState(listener: (state: ConnectionState) => void): () => void;
  /** daemon.info from the latest (re)connect; kept while offline. */
  info(): DaemonInfo | null;
  /** Every row batch; reset batches replace a collection's rows. Listeners outlive reconnects. */
  onRows(listener: (reset: boolean, batch: RowBatch) => void): () => void;
  /** Re-read hosts.list and reload this Host, resolving after its replacement rows are emitted. */
  reload(host: string): Promise<void>;
  close(): void;
}

export function connectDaemon(): Daemon {
  let state: ConnectionState = { status: "connecting", attempt: 0, since: Date.now(), error: null };
  let info: DaemonInfo | null = null;
  let closed = false;
  let source: EventSource | null = null;
  let reconnect: ReturnType<typeof setTimeout> | undefined;
  let poll: ReturnType<typeof setTimeout> | undefined;
  const states = new Set<(state: ConnectionState) => void>();
  const listeners = new Set<(reset: boolean, batch: RowBatch) => void>();
  // The latest synced generation's rows, replayed to listeners that subscribe late.
  let snapshot: () => RowBatch[] = () => [];
  // Live-only history belongs to the page, not a transport generation: the
  // daemon cannot reconstruct marks or activity observed before a reconnect.
  const marks = new Map<string, NegotiationMark[]>();
  const boots = new Map<string, string>();
  const hostHistory = new Map<string, RowOf<"hosts">>();
  const activityHistory = new Map<string, RowOf<"activity">>();
  let reloadHost: Daemon["reload"] = async () => {
    throw new DaemonError("transport", "daemon unreachable");
  };
  const changeState = (status: ConnectionStatus, attempt: number, error: string | null) => {
    state = { status, attempt, since: Date.now(), error };
    for (const listener of states) listener(state);
  };

  const connect = () => {
    if (closed) return;
    changeState("connecting", state.attempt, null);
    const stream = new EventSource("/events");
    source = stream;
    // A generation owns all reads, cursors, and queued events. A disconnected
    // generation never publishes again, even if its outstanding HTTP calls finish.
    const current = () => !closed && source === stream;
    const rows: { [C in CollectionName]: Map<string, RowOf<C>> } = {
      hosts: new Map(),
      executions: new Map(),
      steps: new Map(),
      callouts: new Map(),
      programs: new Map(),
      receipts: new Map(),
      offers: new Map(),
      activity: activityHistory,
      blobs: new Map(),
    };
    const statuses = new Map<string, HostStatus>();
    const activations = new Map<string, ActivationInspection | null>();
    const offers = new Map<string, OpenOffer[]>();
    let ready = false;
    let loading = true;
    let chain = Promise.resolve();
    const queued: (() => Promise<void>)[] = [];
    const batches = (): RowBatch[] =>
      (Object.keys(rows) as CollectionName[]).map(
        (collection) =>
          ({
            collection,
            ops: [...rows[collection].values()].map((row) => ({ op: "upsert" as const, row })),
          }) as RowBatch,
      );
    const publish = (batch: RowBatch) => {
      if (!current()) return;
      // The discriminated batch fixes this map's row type. The union is erased
      // only here so every producer still has to supply a correctly typed batch.
      const collection = rows[batch.collection] as Map<string, RowOf<CollectionName>>;
      for (const op of batch.ops) {
        if (op.op === "delete") collection.delete(op.key);
        else {
          const row = op.row;
          const key = "key" in row ? row.key : "id" in row ? row.id : row.hash;
          collection.set(key, row);
        }
      }
      if (batch.collection === "hosts")
        for (const op of batch.ops) {
          if (op.op === "upsert") hostHistory.set(op.row.id, op.row);
        }
      if (ready) for (const listener of listeners) listener(false, batch);
    };
    const read = async (host: string, request: HostRequest) => {
      const reply = await hostCall(host, request);
      if (!current()) throw new DOMException("Connection replaced", "AbortError");
      return reply;
    };
    const fail = (error: unknown) => {
      if (!current()) return;
      if (!(error instanceof DaemonError)) {
        // Shape mismatches are programming errors, never reconnectable transport failures.
        queueMicrotask(() => {
          throw error;
        });
        return;
      }
      stream.close();
      source = null;
      clearTimeout(poll);
      changeState("offline", state.attempt + 1, error.message);
      reconnect = setTimeout(connect, Math.min(250 * 2 ** (state.attempt - 1), 5000));
    };
    const schedule = (work: () => Promise<void>) => {
      // A single ordered queue covers events and polling. Each bulk read below
      // uses batches of eight; no Host can exceed eight reads in flight.
      const pending = chain.then(async () => {
        if (!current()) throw new DaemonError("transport", "daemon unreachable");
        await work();
      });
      chain = pending.catch(fail);
      return pending;
    };
    const mergeOffers = () => {
      const merged = new Map<string, RowOf<"offers">>();
      for (const [host, entries] of offers)
        for (const offer of entries) {
          const row = offerRow(offer, [host], offer.first_seen_ms);
          const previous = merged.get(row.key);
          if (previous) {
            const newest = previous.offer_seq > row.offer_seq ? previous : row;
            merged.set(row.key, {
              ...newest,
              seen_by: [...previous.seen_by, host].sort(),
              first_seen_ms: Math.min(previous.first_seen_ms, row.first_seen_ms),
            });
          } else merged.set(row.key, row);
        }
      publish({
        collection: "offers",
        ops: [
          ...[...rows.offers.keys()]
            .filter((key) => !merged.has(key))
            .map((key) => ({ op: "delete" as const, key })),
          ...[...merged.values()].map((row) => ({ op: "upsert" as const, row })),
        ],
      });
    };
    const loadOffers = async (host: string) => {
      const reply = await read(host, { method: "negotiation.offers" });
      assertReply(reply, "Offers");
      offers.set(host, reply.Offers);
      mergeOffers();
    };
    const loadPrograms = async (host: string) => {
      const reply = await read(host, { method: "program.list" });
      assertReply(reply, "ProgramList");
      const hashes = new Set(reply.ProgramList.map((program) => program.program_hash));
      for (const previous of [...rows.programs.values()])
        if (previous.hosts.includes(host) && !hashes.has(previous.hash)) {
          const hosts = previous.hosts.filter((id) => id !== host);
          publish({
            collection: "programs",
            ops: hosts.length
              ? [{ op: "upsert", row: { ...previous, hosts } }]
              : [{ op: "delete", key: previous.hash }],
          });
        }
      for (let from = 0; from < reply.ProgramList.length; from += 8) {
        await Promise.all(
          reply.ProgramList.slice(from, from + 8).map(async (summary) => {
            const detail = await read(host, {
              method: "program.get",
              params: { program: summary.program_hash },
            });
            assertReply(detail, "Program");
            const hosts = [
              ...new Set([...(rows.programs.get(summary.program_hash)?.hosts ?? []), host]),
            ].sort();
            publish({
              collection: "programs",
              ops: [{ op: "upsert", row: programRow(detail.Program, hosts) }],
            });
          }),
        );
      }
    };
    const loadReceipts = async (host: string) => {
      const reply = await read(host, { method: "receipt.list" });
      assertReply(reply, "ReceiptList");
      publish({
        collection: "receipts",
        ops: reply.ReceiptList.map((entry) => ({ op: "upsert", row: receiptRow(host, entry) })),
      });
    };
    const projectExecution = async (host: string, status: ExecStatus) => {
      const key = `${host}/${status.exec_id}`;
      const row = executionRow(host, status, activations.get(key) ?? null, marks.get(key) ?? []);
      publish({ collection: "executions", ops: [{ op: "upsert", row }] });
      const known = [...rows.steps.values()].filter(
        (step) => step.host === host && step.exec_id === status.exec_id,
      );
      const from = known.reduce((next, step) => Math.max(next, step.step + 1), 0);
      if (row.session_id !== null && row.latest_step !== null && from <= row.latest_step) {
        const reply = await read(host, {
          method: "exec.trace",
          params: { exec_id: status.exec_id, from, to: row.latest_step + 1 },
        });
        assertReply(reply, "Trace");
        publish({
          collection: "steps",
          ops: reply.Trace.map((step) => ({
            op: "upsert",
            row: stepRow(host, status.exec_id, row.session_id!, row.participants!, step),
          })),
        });
      }
      const state = status.state;
      const session =
        state.exec_state === "Failed"
          ? state.session?.session_state === "Started"
            ? state.session.session
            : null
          : "session" in state
            ? state.session
            : null;
      const pending = session?.pending_callout;
      publish({
        collection: "callouts",
        ops: [...rows.callouts.values()]
          .filter(
            (callout) =>
              callout.host === host &&
              callout.exec_id === status.exec_id &&
              callout.pending_id !== pending?.pending_id,
          )
          .map((callout) => ({ op: "delete", key: callout.key })),
      });
      if (pending) {
        const previous = rows.callouts.get(`${key}/${pending.pending_id}`);
        publish({
          collection: "callouts",
          ops: [
            {
              op: "upsert",
              row: calloutRow(
                host,
                status.exec_id,
                row.session_id,
                pending,
                previous?.opened_ms ?? status.updated_at_ms,
              ),
            },
          ],
        });
      }
    };
    const refreshExecution = async (host: string, execId: string) => {
      const reply = await read(host, { method: "exec.status", params: { exec_id: execId } });
      assertReply(reply, "Status");
      const key = `${host}/${execId}`;
      const previous = rows.executions.get(key);
      if (
        !previous ||
        previous.lifecycle === "negotiating" ||
        previous.lifecycle === "activating"
      ) {
        const list = await read(host, { method: "exec.list" });
        assertReply(list, "ExecList");
        for (const entry of list.ExecList)
          activations.set(`${host}/${entry.status.exec_id}`, entry.activation);
      }
      await projectExecution(host, reply.Status);
    };
    const loadHost = async (status: HostStatus, replace: boolean) => {
      const host = status.host.id;
      statuses.set(host, status);
      if (replace) {
        for (const collection of [
          "executions",
          "steps",
          "callouts",
          "receipts",
          "blobs",
        ] as const) {
          publish({
            collection,
            ops: [...rows[collection].values()]
              .filter((row) => row.host === host)
              .map((row) => ({ op: "delete" as const, key: row.key })),
          });
        }
        for (const key of activations.keys())
          if (key.startsWith(`${host}/`)) activations.delete(key);
      }
      const previous = rows.hosts.get(host);
      const history = hostHistory.get(host);
      publish({
        collection: "hosts",
        ops: [
          {
            op: "upsert",
            row: hostRow(
              status,
              previous?.online ?? true,
              history?.gaps ?? 0,
              history?.last_gap_ms ?? null,
            ),
          },
        ],
      });
      await loadPrograms(host);
      const list = await read(host, { method: "exec.list" });
      assertReply(list, "ExecList");
      for (let from = 0; from < list.ExecList.length; from += 8) {
        await Promise.all(
          list.ExecList.slice(from, from + 8).map(async (entry) => {
            activations.set(`${host}/${entry.status.exec_id}`, entry.activation);
            await projectExecution(host, entry.status);
          }),
        );
      }
      await loadReceipts(host);
      const blobs = await read(host, { method: "blob.list" });
      assertReply(blobs, "BlobList");
      publish({
        collection: "blobs",
        ops: blobs.BlobList.map((entry) => ({ op: "upsert", row: blobRow(host, entry) })),
      });
      await loadOffers(host);
    };
    const appendActivity = (row: RowOf<"activity">) => {
      publish({ collection: "activity", ops: [{ op: "upsert", row }] });
      const oldest = [...rows.activity.values()].sort(
        (a, b) => a.at_ms - b.at_ms || a.key.localeCompare(b.key),
      );
      publish({
        collection: "activity",
        ops: oldest
          .slice(0, Math.max(0, oldest.length - 500))
          .map((row) => ({ op: "delete", key: row.key })),
      });
    };
    const hostEvent = async (frame: EventFrame) => {
      const host = frame.host.id;
      appendActivity(eventActivity(host, frame));
      const mark = negotiationMark(frame);
      if (mark && frame.exec_id) {
        const key = `${host}/${frame.exec_id}`;
        const negotiation = [...(marks.get(key) ?? []), mark].slice(-64);
        marks.set(key, negotiation);
        const row = rows.executions.get(key);
        if (row)
          publish({
            collection: "executions",
            ops: [{ op: "upsert", row: { ...row, negotiation } }],
          });
      }
      const previousBoot = boots.get(host);
      boots.set(host, frame.boot_id);
      if (!statuses.has(host)) {
        const roster = await rpc({ method: "hosts.list" });
        assertReply(roster, "Hosts");
        for (const status of roster.Hosts)
          if (!statuses.has(status.host.id)) await loadHost(status, false);
      }
      const status = statuses.get(host)!;
      switch (frame.kind) {
        case "host.started": {
          const row = rows.hosts.get(host)!;
          publish({ collection: "hosts", ops: [{ op: "upsert", row: { ...row, online: true } }] });
          if (previousBoot !== undefined && previousBoot !== frame.boot_id)
            await loadHost(status, true);
          break;
        }
        case "host.stopped":
          publish({
            collection: "hosts",
            ops: [{ op: "upsert", row: { ...rows.hosts.get(host)!, online: false } }],
          });
          break;
        case "exec.created":
          if (!rows.programs.has(frame.data.program_id)) await loadPrograms(host);
          await refreshExecution(host, frame.exec_id!);
          break;
        case "exec.session.callout": {
          // Only status publishes callouts: a queued event can name an already-answered pending_id.
          const row = calloutRow(
            host,
            frame.exec_id!,
            frame.session_id ?? null,
            frame.data,
            frame.ts,
          );
          rows.callouts.set(row.key, row);
          await refreshExecution(host, frame.exec_id!);
          break;
        }
        case "exec.negotiation.prepared":
        case "exec.negotiation.committed":
        case "exec.negotiation.resumed":
        case "exec.session.started":
        case "exec.session.step":
        case "exec.session.end_progress":
        case "exec.session.callout_answered":
          await refreshExecution(host, frame.exec_id!);
          break;
        case "exec.session.ended":
        case "exec.terminated":
          await refreshExecution(host, frame.exec_id!);
          await loadReceipts(host);
          break;
        case "negotiation.offer_seen":
        case "negotiation.offer_closed":
          await loadOffers(host);
          break;
        case "stream.lagged": {
          const row = rows.hosts.get(host)!;
          publish({
            collection: "hosts",
            ops: [{ op: "upsert", row: { ...row, gaps: row.gaps + 1, last_gap_ms: Date.now() } }],
          });
          await loadHost(status, true);
          break;
        }
      }
    };
    const startPoll = () => {
      poll = setTimeout(
        () =>
          schedule(async () => {
            for (const row of [...rows.executions.values()]) {
              if (
                rows.hosts.get(row.host)?.online &&
                (row.terminal === null || row.end.phase !== "ended")
              )
                await refreshExecution(row.host, row.exec_id);
            }
            if (current()) startPoll();
          }),
        2000,
      );
    };
    stream.addEventListener("host", (event) => {
      const frame: EventFrame = JSON.parse((event as MessageEvent<string>).data);
      const work = () => hostEvent(frame);
      if (loading) queued.push(work);
      else schedule(work);
    });
    stream.addEventListener("activity", (event) => {
      const frame: ActivityFrame = JSON.parse((event as MessageEvent<string>).data);
      const work = async () => {
        appendActivity(activityRow(frame));
      };
      if (loading) queued.push(work);
      else schedule(work);
    });
    stream.onopen = () => {
      changeState("syncing", state.attempt, null);
      schedule(async () => {
        const daemonInfo = await rpc({ method: "daemon.info" });
        assertReply(daemonInfo, "DaemonInfo");
        const roster = await rpc({ method: "hosts.list" });
        assertReply(roster, "Hosts");
        for (const status of roster.Hosts) await loadHost(status, false);
        if (!current()) return;
        info = daemonInfo.DaemonInfo;
        snapshot = batches;
        for (const batch of batches()) for (const listener of listeners) listener(true, batch);
        ready = true;
        // New frames remain queued until all frames already received during
        // loading have been applied. Only then can the workspace become live.
        while (queued.length) await queued.shift()!();
        loading = false;
        if (!current()) return;
        changeState("live", 0, null);
        startPoll();
      });
    };
    reloadHost = (host) => {
      if (!current() || state.status !== "live")
        return Promise.reject(new DaemonError("transport", "daemon unreachable"));
      return schedule(async () => {
        const roster = await rpc({ method: "hosts.list" });
        assertReply(roster, "Hosts");
        if (!current()) throw new DaemonError("transport", "daemon unreachable");
        const status = roster.Hosts.find((entry) => entry.host.id === host)!;
        const previous = rows.hosts.get(host);
        publish({
          collection: "hosts",
          ops: [
            {
              op: "upsert",
              row: hostRow(status, true, previous?.gaps ?? 0, previous?.last_gap_ms ?? null),
            },
          ],
        });
        await loadHost(status, true);
      });
    };
    stream.onerror = () => fail(new DaemonError("transport", "daemon unreachable"));
  };
  connect();
  return {
    state: () => state,
    info: () => info,
    reload: (host) => reloadHost(host),
    onState: (listener) => {
      states.add(listener);
      return () => {
        states.delete(listener);
      };
    },
    onRows: (listener) => {
      listeners.add(listener);
      for (const batch of snapshot()) listener(true, batch);
      return () => {
        listeners.delete(listener);
      };
    },
    close: () => {
      closed = true;
      source?.close();
      source = null;
      clearTimeout(reconnect);
      clearTimeout(poll);
      changeState("offline", state.attempt, null);
    },
  };
}
