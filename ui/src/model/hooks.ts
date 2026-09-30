import { type Collection, useLiveQuery } from "@tanstack/react-db";
import { useSyncExternalStore } from "react";
import { type Collections, type StepRow, useCollections } from "../sync";
import { useNow } from "./clock";
import { divergentStep, type Flag, sessionFlags } from "./flags";
import { useYourHosts } from "./prefs";
import { deriveSessions, groupBy, type Session } from "./session";
import { programStats } from "./stats";

const NO_STEPS: StepRow[] = [];

/**
 * Every session on this machine. `ready` is true once every collection has
 * its first snapshot, and stays true across reconnects: the rows on screen
 * are then stale rather than absent.
 */
export function useSessions(): { sessions: Session[]; ready: boolean } {
  const collections = useCollections();
  const executions = useRows(collections.executions);
  const programs = useRows(collections.programs);
  const callouts = useRows(collections.callouts);
  const receipts = useRows(collections.receipts);
  const hosts = useRows(collections.hosts);
  const steps = useRows(collections.steps);
  const ready = useCollectionsReady(collections);

  const lastStepMs = new Map<string, number>();
  for (const step of steps) {
    // A step row carries `{host, exec_id}`, which is its execution's key.
    const key = `${step.host}/${step.exec_id}`;
    const last = lastStepMs.get(key);
    if (last === undefined || step.certified_ms > last) lastStepMs.set(key, step.certified_ms);
  }
  const sessions = deriveSessions({ executions, programs, callouts, receipts, hosts, lastStepMs });
  return { sessions, ready };
}

export function useSession(key: string): Session | null {
  const { sessions } = useSessions();
  return sessions.find((session) => session.key === key) ?? null;
}

/** All local Hosts' steps of the session, ordered by (step, host). */
export function useSessionSteps(session: Session | null): StepRow[] {
  const steps = useRows(useCollections().steps);
  if (session === null) return NO_STEPS;
  const executions = new Set(session.executions.map((execution) => execution.key));
  return steps
    .filter((step) => executions.has(`${step.host}/${step.exec_id}`))
    .sort((a, b) => a.step - b.step || (a.host < b.host ? -1 : a.host > b.host ? 1 : 0));
}

/** Flags per session key, recomputed every second because waits grow with time. */
export function useFlags(sessions: Session[]): Map<string, Flag[]> {
  const collections = useCollections();
  const steps = useRows(collections.steps);
  const hostRows = useRows(collections.hosts);
  const now = useNow();
  const [yourHosts] = useYourHosts(hostRows.map((host) => host.id));

  const sessionOfExecution = new Map<string, string>();
  const programOf = new Map<string, string>();
  for (const session of sessions) {
    for (const execution of session.executions) {
      sessionOfExecution.set(execution.key, session.key);
      programOf.set(execution.key, execution.program);
    }
  }
  const stepsOfSession = groupBy(steps, (step) =>
    sessionOfExecution.get(`${step.host}/${step.exec_id}`),
  );

  const stats = programStats(steps, programOf);
  const hosts = new Map(hostRows.map((host) => [host.id, host]));
  const flags = new Map<string, Flag[]>();
  for (const session of sessions) {
    flags.set(
      session.key,
      sessionFlags(session, {
        stats,
        yourHosts,
        hosts,
        divergentStep: divergentStep(stepsOfSession.get(session.key) ?? NO_STEPS),
        now,
      }),
    );
  }
  return flags;
}

/**
 * Every row of a collection, live. The workspace reads the plain collections
 * (Hosts, programs, receipts, offers, activity, callouts) through this.
 */
export function useRows<T extends object>(collection: Collection<T, string>): T[] {
  const { data } = useLiveQuery((q) => q.from({ rows: collection }));
  return data;
}

function useCollectionsReady(collections: Collections): boolean {
  return useSyncExternalStore(
    (onChange) => {
      const unsubscribes = Object.values(collections).map((collection) =>
        collection.on("status:change", onChange),
      );
      return () => {
        for (const unsubscribe of unsubscribes) unsubscribe();
      };
    },
    () => Object.values(collections).every((collection) => collection.isReady()),
  );
}

/**
 * Per Host: programs in its catalog, and executions that have not ended.
 * Derived from the replicated rows, which the daemon keeps current; the
 * daemon's own Host counters are a snapshot and are not replicated.
 */
export function useHostCounts(): Map<string, { programs: number; live: number }> {
  const collections = useCollections();
  const programs = useRows(collections.programs);
  const executions = useRows(collections.executions);
  const counts = new Map<string, { programs: number; live: number }>();
  const entry = (host: string) => {
    let found = counts.get(host);
    if (found === undefined) {
      found = { programs: 0, live: 0 };
      counts.set(host, found);
    }
    return found;
  };
  for (const program of programs) for (const host of program.hosts) entry(host).programs += 1;
  for (const execution of executions)
    if (execution.terminal === null) entry(execution.host).live += 1;
  return counts;
}
