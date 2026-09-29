import { type Collection, createCollection } from "@tanstack/db";
import type { Gateway } from "./gateway";
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
} from "./protocol";

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
 * Builds one replica collection per gateway collection. Call once per gateway:
 * each collection subscribes to `gateway.onRows` for as long as the gateway
 * lives.
 */
export function createCollections(gateway: Gateway): Collections {
  return {
    hosts: replica(gateway, "hosts", (row) => row.id),
    executions: replica(gateway, "executions", (row) => row.key),
    steps: replica(gateway, "steps", (row) => row.key),
    callouts: replica(gateway, "callouts", (row) => row.key),
    programs: replica(gateway, "programs", (row) => row.hash),
    receipts: replica(gateway, "receipts", (row) => row.key),
    offers: replica(gateway, "offers", (row) => row.negotiation_id),
    activity: replica(gateway, "activity", (row) => row.key),
    blobs: replica(gateway, "blobs", (row) => row.key),
  };
}

function replica<C extends CollectionName>(
  gateway: Gateway,
  name: C,
  getKey: (row: RowOf<C>) => string,
): Collection<RowOf<C>, string> {
  return createCollection<RowOf<C>, string>({
    id: `arena0:${name}`,
    getKey,
    // The collection is the only copy of the replica: the gateway sends its
    // rows once per connection, so a collection that was garbage collected
    // and restarted would come back empty.
    gcTime: 0,
    startSync: true,
    sync: {
      rowUpdateMode: "full",
      sync: ({ collection, begin, write, commit, truncate, markReady }) => {
        let ready = false;
        return gateway.onRows((reset, batch) => {
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
