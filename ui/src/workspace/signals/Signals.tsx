import type { LucideIcon } from "lucide-react";
import { useState } from "react";
import {
  type Flag,
  type FlagSeverity,
  fmtAgo,
  fmtClock,
  fmtDuration,
  type Session,
  useNow,
  useRows,
  useSeats,
} from "~/model";
import { type ActivityRow, type CalloutRow, useCollections } from "~/sync";
import { Dot, EmptyState, Icons, List, ParticipantChip, Segmented, SubTabs, Switch } from "~/ui";
import { SessionLabel } from "../common/SessionLabel";
import { useComposer } from "../composer/store";
import { useOpenDoc } from "../nav";
import { type SignalsTab, useProblems, useSignalsTab } from "../shell";

const severityIcon: Record<FlagSeverity, { icon: LucideIcon; className: string }> = {
  error: { icon: Icons.error, className: "text-bad" },
  warn: { icon: Icons.warn, className: "text-warn" },
  you: { icon: Icons.you, className: "text-accent" },
  info: { icon: Icons.info, className: "text-info" },
};

const levelIcon = {
  error: severityIcon.error,
  warn: severityIcon.warn,
  info: severityIcon.info,
} as const;

/** Which dock tab shows, with counts: needs input and problems are the two things to act on. */
export function Signals() {
  const [tab, setTab] = useSignalsTab();
  const collections = useCollections();
  const problems = useProblems();
  const callouts = useRows(collections.callouts);
  const hosts = useRows(collections.hosts);
  const [seats] = useSeats(hosts.map((host) => host.id));
  const needs = callouts.filter((callout) => seats.has(callout.host));
  const tier2 = problems.sessions.reduce((n, entry) => n + entry.flags.length, 0);

  return (
    <section aria-label="Signals" className="flex h-full min-h-0 flex-col">
      <SubTabs<SignalsTab>
        label="Signals"
        value={tab}
        onChange={setTab}
        items={[
          { id: "needs", label: "Needs input", count: needs.length },
          { id: "problems", label: "Problems", count: tier2 },
          { id: "activity", label: "Activity" },
        ]}
      />
      <div className="min-h-0 flex-1">
        {tab === "needs" && <NeedsInput callouts={needs} seats={[...seats]} />}
        {tab === "problems" && <Problems />}
        {tab === "activity" && <Activity />}
      </div>
    </section>
  );
}

/** Which session and participant an execution belongs to; sessions are keyed by negotiation, executions by Host. */
function useSessionOfExecution() {
  const { all } = useProblems();
  const byExecution = new Map<string, Session>();
  for (const session of all) {
    for (const execution of session.executions) byExecution.set(execution.key, session);
  }
  return byExecution;
}

function NeedsInput(props: { callouts: CalloutRow[]; seats: string[] }) {
  const now = useNow();
  const open = useOpenDoc();
  const composer = useComposer();
  const { flags } = useProblems();
  const sessionOf = useSessionOfExecution();
  const rows = [...props.callouts].sort((a, b) => a.opened_ms - b.opened_ms);

  return (
    <List
      label="Needs input"
      items={rows}
      getKey={(callout) => callout.key}
      rowHeight={38}
      onSelect={() => {}}
      onAction={(key) => {
        const callout = rows.find((row) => row.key === key);
        const session = callout && sessionOf.get(`${callout.host}/${callout.exec_id}`);
        if (!callout || !session) return;
        open({ kind: "session", key: session.key });
        composer.open(callout.key);
      }}
      empty={
        <EmptyState
          icon={Icons.callout}
          title="Nothing needs you"
          body={`Seats: ${props.seats.join(", ") || "none"}`}
        />
      }
    >
      {(callout) => {
        const session = sessionOf.get(`${callout.host}/${callout.exec_id}`);
        const participant = session?.participants.find(
          (p) => p.execution?.key === `${callout.host}/${callout.exec_id}`,
        );
        const waited = now - callout.opened_ms;
        const long = (session ? (flags.get(session.key) ?? []) : []).some(
          (flag) => flag.kind === "long-wait" && flag.tier === 2,
        );
        return (
          <div className="flex min-w-0 flex-1 flex-col gap-0.5">
            <div className="flex min-w-0 items-center gap-1.5">
              <Dot tone="accent" />
              <span className="min-w-0 flex-1 truncate text-fg">
                {session?.program?.display_name ?? "session"}
              </span>
              <span
                className={`shrink-0 font-mono text-xs tabular ${long ? "text-warn" : "text-subtle"}`}
              >
                {fmtDuration(waited)}
              </span>
            </div>
            <div className="flex min-w-0 items-center gap-1.5 pl-3">
              {participant && <ParticipantChip index={participant.index} label={callout.host} />}
              <span className="min-w-0 truncate text-xs text-muted">{callout.name}</span>
            </div>
          </div>
        );
      }}
    </List>
  );
}

function Problems() {
  const { all, flags } = useProblems();
  const [minor, setMinor] = useState(false);
  const now = useNow();
  const open = useOpenDoc();

  const minorCount = [...flags.values()].reduce(
    (n, list) => n + list.filter((flag) => flag.tier === 1).length,
    0,
  );
  const rows: { key: string; session: Session; flag: Flag }[] = [];
  for (const session of all) {
    for (const flag of flags.get(session.key) ?? []) {
      if (flag.tier === 2 || minor)
        rows.push({ key: `${session.key}:${flag.kind}`, session, flag });
    }
  }

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex h-7 shrink-0 items-center border-b border-line-soft px-2">
        <Switch isSelected={minor} onChange={setMinor}>
          <span className="text-sm">Include minor ({minorCount})</span>
        </Switch>
      </div>
      <div className="min-h-0 flex-1">
        <List
          label="Problems"
          items={rows}
          getKey={(row) => row.key}
          onSelect={() => {}}
          onAction={(key) => {
            const row = rows.find((candidate) => candidate.key === key);
            if (row) open({ kind: "session", key: row.session.key });
          }}
          empty={
            <EmptyState icon={Icons.verify} title="No problems" body="Nothing to look at first." />
          }
        >
          {({ session, flag }) => {
            const { icon: SeverityIcon, className } = severityIcon[flag.severity];
            return (
              <div className="flex min-w-0 flex-1 items-center gap-1.5">
                <SeverityIcon size={12} className={`shrink-0 ${className}`} />
                <span className="shrink-0 font-medium">{flag.short}</span>
                <span className="min-w-0 flex-1 truncate">
                  <SessionLabel session={session} />
                </span>
                <span className="shrink-0 text-xs text-subtle">
                  {fmtAgo(session.lastStepMs ?? session.updatedMs, now)}
                </span>
              </div>
            );
          }}
        </List>
      </div>
    </div>
  );
}

function Activity() {
  const rows = useRows(useCollections().activity);
  const sessionOf = useSessionOfExecution();
  const open = useOpenDoc();
  const [filter, setFilter] = useState<"all" | "warnings">("all");
  const shown = rows
    .filter((row) => filter === "all" || row.level !== "info")
    .sort((a, b) => b.at_ms - a.at_ms);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex h-7 shrink-0 items-center border-b border-line-soft px-2">
        <Segmented
          label="Activity filter"
          value={filter}
          onChange={setFilter}
          items={[
            { id: "all", label: "All" },
            { id: "warnings", label: "Warnings" },
          ]}
        />
      </div>
      <div className="min-h-0 flex-1">
        <List
          label="Activity"
          items={shown}
          getKey={(row: ActivityRow) => row.key}
          onSelect={() => {}}
          onAction={(key) => {
            const row = shown.find((candidate) => candidate.key === key);
            const session = row?.exec_id && sessionOf.get(`${row.source}/${row.exec_id}`);
            if (session) open({ kind: "session", key: session.key });
          }}
          empty={
            <EmptyState
              icon={Icons.activity}
              title={filter === "all" ? "No activity yet" : "No warnings"}
              body="Activity lists what the daemon reports while this page is open; the daemon keeps no history of it."
            />
          }
        >
          {(row) => {
            const { icon: LevelIcon, className } = levelIcon[row.level];
            return (
              <div className="flex min-w-0 flex-1 items-center gap-1.5">
                <span className="shrink-0 font-mono text-xs text-subtle tabular">
                  {fmtClock(row.at_ms)}
                </span>
                <span className="shrink-0 font-mono text-xs text-muted">{row.source}</span>
                <span className="min-w-0 flex-1 truncate">{row.text}</span>
                <LevelIcon
                  size={12}
                  role="img"
                  aria-label={row.level}
                  className={`shrink-0 ${className}`}
                />
              </div>
            );
          }}
        </List>
      </div>
    </div>
  );
}
