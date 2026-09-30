import type { Flag, Session } from "~/model";
import type { StepRow } from "~/sync";
import type { StripMark, StripSpan } from "~/ui";

/**
 * One session's lifecycle strip. `steps` are the session's rows from every
 * local Host: each step number counts once, at the earliest time any Host
 * certified it. Activation is step 0's time, so a session whose step 0 has
 * not been certified is still negotiating however its state reads.
 *
 * Gap marks are not drawn: a `Flag` says that a Host skipped events but not
 * when, and the strip has no other source for the time.
 */
export function sessionStrip(
  session: Session,
  steps: StepRow[],
  flags: Flag[],
  yourHosts: ReadonlySet<string>,
  now: number,
): { spans: StripSpan[]; marks: StripMark[] } {
  const spans: StripSpan[] = [];
  const marks: StripMark[] = [];

  const stepTimes = new Map<number, number>();
  for (const row of steps) {
    const seen = stepTimes.get(row.step);
    if (seen === undefined || row.certified_ms < seen) stepTimes.set(row.step, row.certified_ms);
  }
  for (const at of stepTimes.values()) marks.push({ at, kind: "step" });

  const settled =
    session.state === "completed" || session.state === "failed" || session.state === "aborted";
  const activation = stepTimes.get(0) ?? null;
  const ended = settled || session.state === "ending";
  // A settled session's life stops at its last lifecycle change; one still
  // ending stopped at its last step and has been closing since.
  const lastStep = Math.max(activation ?? session.createdMs, ...stepTimes.values());
  const activeEnd = settled
    ? Math.max(session.updatedMs, lastStep)
    : session.state === "ending"
      ? lastStep
      : now;

  if (activation === null) {
    spans.push({
      from: session.createdMs,
      to: ended ? session.updatedMs : now,
      kind: "negotiating",
    });
  } else {
    if (activation > session.createdMs) {
      spans.push({ from: session.createdMs, to: activation, kind: "negotiating" });
    }
    spans.push({ from: activation, to: activeEnd, kind: "active" });
  }
  if (session.state === "ending") spans.push({ from: activeEnd, to: now, kind: "ending" });

  if (settled) {
    marks.push({
      at: activation === null ? session.updatedMs : activeEnd,
      kind: session.state === "completed" ? "end-ok" : "end-bad",
    });
  }

  // The flag names the oldest callout of Hosts whose participants are not yours, so only that one is "long".
  const longWait = flags.some((flag) => flag.kind === "long-wait" && flag.tier === 2);
  const oldestTheirs = session.callouts.find((callout) => !yourHosts.has(callout.host));
  for (const callout of session.callouts) {
    const kind = yourHosts.has(callout.host)
      ? "waiting-you"
      : longWait && callout === oldestTheirs
        ? "waiting-long"
        : "waiting";
    spans.push({ from: callout.opened_ms, to: now, kind });
  }
  return { spans, marks };
}
