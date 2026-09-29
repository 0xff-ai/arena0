import { useQueries } from "@tanstack/react-query";
import type { Session } from "~/model";
import { ops, type RecordRow, useConnection } from "~/sync";

/** Records per Host the "Local events" view asks for: the latest page the daemon serves. */
const LIMIT = 200;

/**
 * Each local Host's most recent event records, by Host id; nothing is fetched
 * while `enabled` is false. One read per Host, so this cannot be `useRead`
 * (the number of Hosts varies between renders).
 */
export function useRecords(session: Session, enabled: boolean): Map<string, RecordRow[]> {
  const live = useConnection().status === "live";
  const results = useQueries({
    queries: session.executions.map((execution) => {
      const args = { host: execution.host, exec_id: execution.exec_id, from: null, limit: LIMIT };
      return {
        // The step count in the key makes a new step a new read.
        queryKey: ["arena0", "records", args, execution.latest_step],
        queryFn: () => ops.records(args),
        enabled: enabled && live,
      };
    }),
  });
  const byHost = new Map<string, RecordRow[]>();
  session.executions.forEach((execution, i) => {
    const data = results[i]?.data;
    if (data !== undefined) byHost.set(execution.host, data.records);
  });
  return byHost;
}
