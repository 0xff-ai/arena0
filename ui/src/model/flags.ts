import type { HostRow, StepRow } from "../sync";
import { fmtDuration } from "./format";
import type { Session } from "./session";
import type { ProgramStats } from "./stats";

export type FlagKind =
  | "needs-you"
  | "long-wait"
  | "idle"
  | "divergence"
  | "stop-report-only"
  | "negotiation-trouble"
  | "unconfirmed-end"
  | "observation-gap";

export type FlagSeverity = "you" | "warn" | "error" | "info";

export interface Flag {
  kind: FlagKind;
  /** 2 is the one to look at first. */
  tier: 1 | 2;
  severity: FlagSeverity;
  /** A chip. */
  short: string;
  /** A sentence for the details. */
  long: string;
}

/** Waits and silences shorter than this are never tier 2, whatever the program's history says. */
const FLOOR_MS = 60_000;

/**
 * Flags for one session, tier 2 first. `stats` is per program hash, `yourHosts`
 * the Hosts whose participants the user answers for, `hosts` keyed by Host id, and
 * `divergentStep` the result of `divergentStep(steps)` over the session's
 * steps.
 */
export function sessionFlags(
  session: Session,
  context: {
    stats: Map<string, ProgramStats>;
    yourHosts: ReadonlySet<string>;
    hosts: Map<string, HostRow>;
    divergentStep: number | null;
    now: number;
  },
): Flag[] {
  const { yourHosts, hosts, now } = context;
  const stats = context.stats.get(session.programHash);
  const p75 = stats?.p75 ?? null;
  // Above this a wait or silence is tier 2.
  const slow = Math.max(stats?.p95 ?? 0, FLOOR_MS);
  const flags: Flag[] = [];

  // Callouts are oldest first, so `find` takes the longest wait.
  const mine = session.callouts.find((c) => yourHosts.has(c.host));
  if (mine !== undefined) {
    const wait = now - mine.opened_ms;
    flags.push({
      kind: "needs-you",
      tier: wait > slow ? 2 : 1,
      severity: "you",
      short: `needs you · ${fmtDuration(wait)}`,
      long: `${mine.host} has waited ${fmtDuration(wait)} for your answer to "${mine.name}".`,
    });
  }

  const theirs = session.callouts.find((c) => !yourHosts.has(c.host));
  if (theirs !== undefined) {
    const wait = now - theirs.opened_ms;
    const tier = wait > slow ? 2 : p75 !== null && wait > p75 ? 1 : null;
    if (tier !== null) {
      const top = stats === undefined || p75 === null ? null : topPercent(stats, wait);
      flags.push({
        kind: "long-wait",
        tier,
        severity: "warn",
        short: `waiting ${fmtDuration(wait)}${top === null ? "" : ` · top ${top}%`}`,
        long:
          `${theirs.host} has waited ${fmtDuration(wait)} on "${theirs.name}"` +
          (top === null ? "." : `; only ${top}% of this program's steps took longer.`),
      });
    }
  }

  if (session.state === "active" && session.callouts.length === 0 && session.lastStepMs !== null) {
    const silence = now - session.lastStepMs;
    if (silence > slow) {
      flags.push({
        kind: "idle",
        tier: 2,
        severity: "warn",
        short: `no step for ${fmtDuration(silence)}`,
        long: `The session is active, has no open callout, and has not stepped for ${fmtDuration(silence)}.`,
      });
    }
  }

  const divergence = divergenceFlag(session, context.divergentStep);
  if (divergence !== null) flags.push(divergence);

  const settled =
    session.state !== "negotiating" && session.state !== "activating" && session.state !== "active";
  if (
    settled &&
    session.receipts.length > 0 &&
    session.receipts.every((r) => r.kind === "stop_report")
  ) {
    flags.push({
      kind: "stop-report-only",
      tier: 2,
      severity: "warn",
      short: "stop report only",
      long: "The session ended with a stop report and no unanimous receipt.",
    });
  }

  const trouble = negotiationFlag(session);
  if (trouble !== null) flags.push(trouble);

  const { phase, unconfirmed } = session.end;
  if (unconfirmed.length > 0 && phase !== "open") {
    const peers = `${unconfirmed.length} peer${unconfirmed.length === 1 ? "" : "s"}`;
    flags.push({
      kind: "unconfirmed-end",
      tier: phase === "ended" ? 2 : 1,
      severity: "warn",
      short: `${peers} unconfirmed`,
      long:
        phase === "ended"
          ? `The session ended locally but ${peers} never confirmed the end.`
          : `The session is ending; ${peers} not yet confirmed.`,
    });
  }

  const gap = session.executions.some((e) => {
    const gapMs = hosts.get(e.host)?.last_gap_ms;
    return gapMs !== null && gapMs !== undefined && gapMs >= session.createdMs;
  });
  if (gap) {
    flags.push({
      kind: "observation-gap",
      tier: 1,
      severity: "info",
      short: "events skipped · reloaded",
      long: "A Host of this session skipped events and reloaded its state from storage; the activity log may be incomplete.",
    });
  }

  return flags.sort((a, b) => b.tier - a.tier);
}

/** The smallest step at which two Hosts of one session stored different states, if any. */
export function divergentStep(steps: StepRow[]): number | null {
  const stateAt = new Map<number, string>();
  let smallest: number | null = null;
  for (const row of steps) {
    const seen = stateAt.get(row.step);
    if (seen === undefined) stateAt.set(row.step, row.post_state);
    else if (seen !== row.post_state && (smallest === null || row.step < smallest)) {
      smallest = row.step;
    }
  }
  return smallest;
}

/** Share of the program's intervals longer than `wait`, in whole percent rounded up, at least 1. */
function topPercent(stats: ProgramStats, wait: number): number {
  const longer = stats.intervals.filter((interval) => interval > wait).length;
  // Multiply first so exact shares (7 of 100) do not round up through float error.
  return Math.max(1, Math.ceil((longer * 100) / stats.samples));
}

function divergenceFlag(session: Session, divergentStep: number | null): Flag | null {
  if (divergentStep !== null) {
    return {
      kind: "divergence",
      tier: 2,
      severity: "error",
      short: `diverged at step ${divergentStep}`,
      long: `Local Hosts stored different states at step ${divergentStep}.`,
    };
  }
  const failed = session.executions.filter((e) => e.lifecycle === "failed");
  if (failed.length === 0) return null;
  if (failed.length < session.executions.length) {
    const hostsFailed = failed.map((e) => e.host).join(", ");
    return {
      kind: "divergence",
      tier: 2,
      severity: "error",
      short: `failed on ${hostsFailed}`,
      long: `${hostsFailed} failed while the other local Hosts of this session did not.`,
    };
  }
  const reason = session.terminal?.reason;
  return {
    kind: "divergence",
    tier: 2,
    severity: "error",
    short: session.latestStep === null ? "session failed" : `failed at step ${session.latestStep}`,
    long: reason ? `The session failed: ${reason}` : "The session failed.",
  };
}

/**
 * Only a timeout is trouble. Retries are the negotiation's normal re-broadcast
 * cadence (a creator retries until its joiners' tickets arrive), and the
 * browser sees them only live, so they are not flagged.
 */
function negotiationFlag(session: Session): Flag | null {
  const timedOut = session.executions
    .flatMap((e) => e.negotiation)
    .find((mark) => mark.kind === "timed_out");
  if (timedOut !== undefined) {
    return {
      kind: "negotiation-trouble",
      tier: 2,
      severity: "warn",
      short: "timed out",
      long: `The negotiation timed out: ${timedOut.detail}.`,
    };
  }
  return null;
}
