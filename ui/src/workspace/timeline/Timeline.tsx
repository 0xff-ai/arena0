import { useNavigate, useSearch } from "@tanstack/react-router";
import { useState } from "react";
import { fmtClock, useNow, useRows, useSessions } from "~/model";
import { useCollections } from "~/sync";
import { Button, Segmented, type TimeBucket, TimelineChart, useElementSize } from "~/ui";

type Range = "15m" | "1h" | "6h" | "24h";

const RANGE_MS: Record<Range, number> = {
  "15m": 15 * 60_000,
  "1h": 60 * 60_000,
  "6h": 6 * 60 * 60_000,
  "24h": 24 * 60 * 60_000,
};
const BUCKETS = 60;

export function Timeline() {
  const collections = useCollections();
  const steps = useRows(collections.steps);
  const executions = useRows(collections.executions);
  const callouts = useRows(collections.callouts);
  const { sessions } = useSessions();
  const now = useNow(5000);
  const [range, setRange] = useState<Range>("1h");
  const [bodyRef, { height }] = useElementSize<HTMLDivElement>();
  const navigate = useNavigate();
  // The brush is the sessions list's own filter, so it is only visible while that list is shown.
  const search = useSearch({ from: "/sessions", shouldThrow: false });

  const from = now - RANGE_MS[range];
  const bucketMs = RANGE_MS[range] / BUCKETS;
  const buckets: TimeBucket[] = Array.from({ length: BUCKETS }, (_, i) => ({
    t: from + i * bucketMs,
    up: 0,
    down: 0,
    bad: 0,
  }));
  // `now` ticks every few seconds, so a row newer than the last tick belongs to the newest bucket.
  const at = (t: number) => Math.min(BUCKETS - 1, Math.floor((t - from) / bucketMs));

  const sessionOfExecution = new Map<string, string>();
  for (const session of sessions) {
    for (const execution of session.executions) sessionOfExecution.set(execution.key, session.key);
  }

  // Every local Host certifies its own copy of a step; the agreement is one step, at its earliest.
  const certified = new Map<string, number>();
  for (const step of steps) {
    const session = sessionOfExecution.get(`${step.host}/${step.exec_id}`);
    if (session === undefined) continue;
    const key = `${session}:${step.step}`;
    const seen = certified.get(key);
    if (seen === undefined || step.certified_ms < seen) certified.set(key, step.certified_ms);
  }
  for (const t of certified.values()) {
    const bucket = buckets[at(t)];
    if (bucket) bucket.up += 1;
  }

  // A session waits from its oldest open callout until now.
  const waitingSince = new Map<string, number>();
  for (const callout of callouts) {
    const session = sessionOfExecution.get(`${callout.host}/${callout.exec_id}`);
    if (session === undefined) continue;
    const since = waitingSince.get(session);
    if (since === undefined || callout.opened_ms < since) {
      waitingSince.set(session, callout.opened_ms);
    }
  }
  for (const since of waitingSince.values()) {
    for (let i = Math.max(0, at(since)); i < BUCKETS; i++) {
      const bucket = buckets[i];
      if (bucket) bucket.down += 1;
    }
  }

  for (const execution of executions) {
    if (execution.lifecycle !== "failed" && execution.lifecycle !== "aborted") continue;
    const bucket = buckets[at(execution.updated_ms)];
    if (bucket) bucket.bad = (bucket.bad ?? 0) + 1;
  }

  const brush =
    search?.from !== undefined && search.to !== undefined
      ? { from: search.from, to: search.to }
      : null;

  return (
    <section aria-label="Timeline" className="flex h-full min-h-0 flex-col">
      <div className="flex h-7 shrink-0 items-center gap-2 border-b border-line-soft px-2">
        <span className="text-xs font-medium tracking-wide text-subtle uppercase">Timeline</span>
        <Segmented<Range>
          label="Timeline range"
          value={range}
          onChange={setRange}
          items={(Object.keys(RANGE_MS) as Range[]).map((id) => ({ id, label: id }))}
        />
        <span className="flex-1" />
        {brush && (
          <>
            <span className="font-mono text-xs text-muted tabular">
              {fmtClock(brush.from)} – {fmtClock(brush.to)}
            </span>
            <Button
              variant="ghost"
              size="sm"
              onPress={() =>
                void navigate({
                  to: "/sessions",
                  search: (prev) => ({ ...prev, from: undefined, to: undefined }),
                })
              }
            >
              Clear
            </Button>
          </>
        )}
      </div>
      <div ref={bodyRef} className="min-h-0 flex-1 overflow-hidden">
        {height > 0 && (
          <TimelineChart
            buckets={buckets}
            from={from}
            to={now}
            height={height}
            brush={brush}
            upLabel="steps certified"
            downLabel="sessions waiting"
            onBrush={(range) => {
              // A click clears the range; with no sessions filter shown there is nothing to clear.
              if (range === null && search === undefined) return;
              void navigate({
                to: "/sessions",
                search: (prev) => ({
                  ...(search === undefined ? {} : prev),
                  from: range === null ? undefined : Math.round(range.from),
                  to: range === null ? undefined : Math.round(range.to),
                }),
              });
            }}
          />
        )}
      </div>
    </section>
  );
}
