import { type Collection, createCollection } from "@tanstack/db";
import type { Daemon } from "./daemon";
import type {
  ActivityRow,
  BlobRow,
  CalloutRow,
  CollectionName,
  ExecutionRow,
  HostRow,
  OfferRow,
  ProgramRow,
  ReceiptRow,
  RowOf,
  RowOp,
  StepRow,
} from "./rows";

export interface Collections {
  hosts: Collection<HostRow, string>;
  executions: Collection<ExecutionRow, string>;
  steps: Collection<StepRow, string>;
  callouts: Collection<CalloutRow, string>;
  programs: Collection<ProgramRow, string>;
  receipts: Collection<ReceiptRow, string>;
  offers: Collection<OfferRow, string>;
  activity: Collection<ActivityRow, string>;
  blobs: Collection<BlobRow, string>;
}

/**
 * Builds one TanStack collection per browser row model. Call once per Daemon
 * connection object: each collection keeps its subscription across reconnects.
 */
export function createCollections(daemon: Daemon): Collections {
  return {
    hosts: replica(daemon, "hosts", (row) => row.id),
    executions: replica(daemon, "executions", (row) => row.key),
    steps: replica(daemon, "steps", (row) => row.key),
    callouts: replica(daemon, "callouts", (row) => row.key),
    programs: replica(daemon, "programs", (row) => row.hash),
    receipts: replica(daemon, "receipts", (row) => row.key),
    offers: replica(daemon, "offers", (row) => row.key),
    activity: replica(daemon, "activity", (row) => row.key),
    blobs: replica(daemon, "blobs", (row) => row.key),
  };
}

function replica<C extends CollectionName>(
  daemon: Daemon,
  name: C,
  getKey: (row: RowOf<C>) => string,
): Collection<RowOf<C>, string> {
  return createCollection<RowOf<C>, string>({
    id: `arena0:${name}`,
    getKey,
    // Workspace navigation must preserve the collection's subscription and
    // readiness state; all documents share these page-lifetime collections.
    gcTime: 0,
    startSync: true,
    sync: {
      rowUpdateMode: "full",
      sync: ({ collection, begin, write, commit, truncate, markReady }) => {
        let ready = false;
        return daemon.onRows((reset, batch) => {
          if (batch.collection !== name) return;
          // `name` selects the batch member, which TypeScript cannot follow through the generic.
          const ops = batch.ops as ReadonlyArray<RowOp<RowOf<C>>>;
          begin();
          // truncate() discards writes already in the transaction, so it goes
          // first; the reset replaces every row, so no key survives it.
          if (reset) truncate();
          // Writes in this batch are not visible through `collection.has` until
          // the commit, so track them for repeated keys.
          const written = new Map<string, boolean>();
          const present = (key: string) => written.get(key) ?? (!reset && collection.has(key));
          for (const op of ops) {
            if (op.op === "upsert") {
              const key = getKey(op.row);
              write({ type: present(key) ? "update" : "insert", value: op.row });
              written.set(key, true);
            } else if (present(op.key)) {
              write({ type: "delete", key: op.key });
              written.set(op.key, false);
            }
          }
          commit();
          if (reset && !ready) {
            ready = true;
            markReady();
          }
        });
      },
    },
  });
}
