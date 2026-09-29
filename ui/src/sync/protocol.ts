import type { Op, RowBatch } from "./protocol.gen";

export * from "./protocol.gen";

export type OpName = Op["op"];
/** `null` for operations that take no arguments (`daemon_stop`). */
export type OpArgs<K extends OpName> = Extract<Op, { op: K }> extends { args: infer A } ? A : null;
export type CollectionName = RowBatch["collection"];
export type RowOf<C extends CollectionName> = Extract<
  RowBatch,
  { collection: C }
>["ops"][number] extends infer O
  ? O extends { op: "upsert"; row: infer R }
    ? R
    : never
  : never;
