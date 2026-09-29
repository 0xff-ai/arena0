import { useNavigate } from "@tanstack/react-router";
import { useRef, useState } from "react";
import { sessionsRoute } from "~/app/router";
import {
  fmtClock,
  fmtDuration,
  type Session,
  type SessionState,
  useFlags,
  useNow,
  useRows,
  useSeats,
  useSessions,
} from "~/model";
import { type StepRow, useCollections } from "~/sync";
import {
  Badge,
  Button,
  type Column,
  DataTable,
  Dot,
  EmptyState,
  IconButton,
  Icons,
  LifecycleStrip,
  ParticipantDot,
  SearchField,
  Section,
  Segmented,
  type Tone,
  Toolbar,
} from "~/ui";
import { FlagHint } from "../common/FlagHint";
import { SessionLabel } from "../common/SessionLabel";
import { useOpenDoc } from "../nav";
import { useSelection } from "../selection";
import { sessionStrip } from "./strip";
import { useWidth } from "./useWidth";

const DAY_MS = 24 * 3600 * 1000;
const ROW_HEIGHT = 26;
const HEADING_HEIGHT = 24;

const STATE_TONE: Record<SessionState, Tone> = {
  active: "ok",
  completed: "done",
  failed: "bad",
  aborted: "bad",
  negotiating: "neutral",
  activating: "neutral",
  ending: "neutral",
};

const isTerminal = (session: Session) =>
  session.state === "completed" || session.state === "failed" || session.state === "aborted";

/** Which optional columns fit in a list this wide; the lifecycle strip keeps the rest. */
function fits(width: number) {
  return {
    evidence: width >= 1100,
    age: width >= 980,
    participants: width >= 860,
    hint: width >= 760,
  };
}

function matches(session: Session, q: string, hostOf: (peer: string) => string | null): boolean {
  const needle = q.toLowerCase();
  const key = session.key.slice(session.key.lastIndexOf("/") + 1).replace(/^e:/, "");
  return (
    (session.program?.display_name ?? "").toLowerCase().includes(needle) ||
    (session.program?.name ?? "").toLowerCase().includes(needle) ||
    session.executions.some((execution) => execution.host.toLowerCase().includes(needle)) ||
    session.key.toLowerCase().startsWith(needle) ||
    key.toLowerCase().startsWith(needle) ||
    (session.sessionId ?? "").toLowerCase().startsWith(needle) ||
    session.participants.some(
      (participant) =>
        participant.peerId.toLowerCase().startsWith(needle) ||
        (hostOf(participant.peerId) ?? "").toLowerCase().includes(needle),
    )
  );
}

export function SessionsList() {
  const search = sessionsRoute.useSearch();
  const navigate = useNavigate({ from: sessionsRoute.fullPath });
  const collections = useCollections();
  const { sessions } = useSessions();
  const flags = useFlags(sessions);
  const hosts = useRows(collections.hosts);
  const steps = useRows(collections.steps);
  const [seats] = useSeats(hosts.map((host) => host.id));
  const now = useNow();
  const [selection, select] = useSelection();
  const openDoc = useOpenDoc();

  const box = useRef<HTMLDivElement>(null);
  const width = useWidth(box);
  const [text, setText] = useState(search.q ?? "");

  const state = search.state ?? "live";
  const group = search.group ?? "list";
  const set = (patch: Partial<typeof search>) =>
    void navigate({ search: (prev) => ({ ...prev, ...patch }) });

  const isLive = (session: Session) =>
    !isTerminal(session) || (flags.get(session.key) ?? []).some((flag) => flag.tier === 2);
  const isFlagged = (session: Session) => (flags.get(session.key) ?? []).length > 0;
  const hostOfPeer = (peer: string) => hosts.find((host) => host.peer_id === peer)?.id ?? null;

  // The state filter is the only one with counts, so they ignore the others.
  const scoped = sessions.filter(
    (session) =>
      (search.host === undefined || session.executions.some((e) => e.host === search.host)) &&
      (search.program === undefined ||
        session.programHash === search.program ||
        session.program?.name === search.program) &&
      (search.from === undefined ||
        search.to === undefined ||
        (session.createdMs <= search.to &&
          (!isTerminal(session) || session.updatedMs >= search.from))) &&
      (search.q === undefined || matches(session, search.q, hostOfPeer)),
  );
  const counts = { live: 0, flagged: 0, all: scoped.length };
  for (const session of scoped) {
    if (isLive(session)) counts.live++;
    if (isFlagged(session)) counts.flagged++;
  }
  const rows = scoped.filter(
    (session) => state === "all" || (state === "live" ? isLive(session) : isFlagged(session)),
  );

  const axisFrom =
    search.from !== undefined && search.to !== undefined
      ? search.from
      : Math.max(now - DAY_MS, Math.min(now, ...rows.map((row) => row.createdMs)));
  const axisTo = search.from !== undefined && search.to !== undefined ? search.to : now;

  const stepsOf = stepsByExecution(steps);
  const optional = fits(width);
  const fixed =
    28 +
    220 +
    56 +
    (optional.participants ? 96 : 0) +
    (optional.age ? 72 : 0) +
    (optional.evidence ? 96 : 0) +
    (optional.hint ? 220 : 0);
  // The cell has 8 px of padding on each side.
  const stripWidth = Math.max(200, width - fixed) - 16;

  const columns: Column<Session>[] = [
    {
      id: "state",
      title: "",
      width: 28,
      minWidth: 28,
      render: (session) => (
        <span role="img" aria-label={session.state} className="inline-flex">
          <Dot tone={STATE_TONE[session.state]} pulse={session.state === "active"} />
        </span>
      ),
    },
    {
      id: "session",
      title: "Session",
      width: 220,
      minWidth: 220,
      isRowHeader: true,
      render: (session) => <SessionLabel session={session} />,
    },
    ...(optional.participants
      ? [
          {
            id: "participants",
            title: "Participants",
            width: 96,
            minWidth: 96,
            render: (session: Session) => <Participants session={session} seats={seats} />,
          },
        ]
      : []),
    {
      id: "lifecycle",
      title: "Lifecycle",
      width: "1fr",
      minWidth: 200,
      render: (session) => {
        const own = session.executions.flatMap((execution) => stepsOf.get(execution.key) ?? []);
        const { spans, marks } = sessionStrip(
          session,
          own,
          flags.get(session.key) ?? [],
          seats,
          now,
        );
        return (
          <LifecycleStrip
            from={axisFrom}
            to={axisTo}
            width={stripWidth}
            spans={spans}
            marks={marks}
          />
        );
      },
    },
    {
      id: "step",
      title: "Step",
      width: 56,
      minWidth: 56,
      align: "end",
      render: (session) => (
        <span className="font-mono text-xs text-muted tabular">{session.latestStep ?? "—"}</span>
      ),
    },
    ...(optional.age
      ? [
          {
            id: "age",
            title: "Age",
            width: 72,
            minWidth: 72,
            align: "end" as const,
            render: (session: Session) => (
              <span className="font-mono text-xs text-subtle tabular">
                {fmtDuration(now - session.createdMs)}
              </span>
            ),
          },
        ]
      : []),
    ...(optional.evidence
      ? [
          {
            id: "evidence",
            title: "Evidence",
            width: 96,
            minWidth: 96,
            render: (session: Session) => <Evidence session={session} />,
          },
        ]
      : []),
    ...(optional.hint
      ? [
          {
            id: "hint",
            title: "Hint",
            width: 220,
            minWidth: 220,
            render: (session: Session) => {
              const top = flags.get(session.key)?.[0];
              return top === undefined ? null : <FlagHint flag={top} />;
            },
          },
        ]
      : []),
  ];

  const selectedKey = selection?.kind === "session" ? selection.key : null;
  const table = (label: string, list: Session[]) => (
    <DataTable
      label={label}
      columns={columns}
      rows={list}
      getKey={(session) => session.key}
      rowHeight={ROW_HEIGHT}
      selectedKey={selectedKey}
      onSelect={(key) => select({ kind: "session", key })}
      onAction={(key) => openDoc({ kind: "session", key })}
      rowTone={(session) =>
        isTerminal(session) && (flags.get(session.key) ?? []).length === 0 ? "quiet" : "normal"
      }
      empty={null}
    />
  );

  const clear = () => {
    setText("");
    void navigate({ search: { state: "all" } });
  };
  const empty = (
    <EmptyState
      icon={Icons.filter}
      title="No sessions match these filters"
      action={<Button onPress={clear}>Show all sessions</Button>}
    />
  );

  const programName =
    search.program === undefined
      ? null
      : (sessions.find(
          (s) => s.programHash === search.program || s.program?.name === search.program,
        )?.program?.display_name ?? search.program);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <Toolbar aria-label="Sessions" className="h-auto min-h-8.5 flex-wrap py-1">
        <Segmented
          label="State"
          value={state}
          onChange={(value) => set({ state: value === "live" ? undefined : value })}
          items={[
            { id: "live", label: "Live", count: counts.live },
            { id: "flagged", label: "Flagged", count: counts.flagged },
            { id: "all", label: "All", count: counts.all },
          ]}
        />
        <Segmented
          label="Grouping"
          value={group}
          onChange={(value) => set({ group: value === "list" ? undefined : value })}
          items={[
            { id: "list", label: "List" },
            { id: "host", label: "By host" },
          ]}
        />
        {search.host !== undefined && (
          <FilterChip name="host" value={search.host} onRemove={() => set({ host: undefined })} />
        )}
        {programName !== null && (
          <FilterChip
            name="program"
            value={programName}
            onRemove={() => set({ program: undefined })}
          />
        )}
        {search.from !== undefined && search.to !== undefined && (
          <FilterChip
            name="time"
            value={`${fmtClock(search.from).slice(0, 5)}–${fmtClock(search.to).slice(0, 5)}`}
            onRemove={() => set({ from: undefined, to: undefined })}
          />
        )}
        <span className="min-w-2 flex-1" />
        <SearchField
          label="Find"
          placeholder="Find"
          kbd="/"
          value={text}
          onChange={(value) => {
            setText(value);
            set({ q: value === "" ? undefined : value });
          }}
          className="w-52"
        />
        <span className="shrink-0 font-mono text-xs text-subtle tabular">
          {rows.length} {rows.length === 1 ? "session" : "sessions"}
        </span>
      </Toolbar>
      <div ref={box} className="min-h-0 flex-1">
        {rows.length === 0 ? (
          empty
        ) : group === "list" ? (
          table("Sessions", rows)
        ) : (
          <div className="h-full overflow-auto">
            {hosts.map((host) => {
              const list = rows.filter((row) => row.executions.some((e) => e.host === host.id));
              if (list.length === 0) return null;
              return (
                <Section key={host.id} title={host.id} icon={Icons.host} count={list.length}>
                  <div style={{ height: HEADING_HEIGHT + list.length * ROW_HEIGHT }}>
                    {table(`Sessions on ${host.id}`, list)}
                  </div>
                </Section>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}

/** Steps grouped by their execution's key, which a step row carries as `{host, exec_id}`. */
function stepsByExecution(steps: StepRow[]): Map<string, StepRow[]> {
  const groups = new Map<string, StepRow[]>();
  for (const step of steps) {
    const key = `${step.host}/${step.exec_id}`;
    const group = groups.get(key);
    if (group === undefined) groups.set(key, [step]);
    else group.push(step);
  }
  return groups;
}

function Participants(props: { session: Session; seats: ReadonlySet<string> }) {
  const { session, seats } = props;
  const shown = session.participants.slice(0, 5);
  return (
    <span className="inline-flex items-center gap-2">
      {shown.map((participant) => {
        const lifecycle = participant.execution?.lifecycle;
        const state =
          lifecycle === "completed"
            ? "done"
            : lifecycle === "failed" || lifecycle === "aborted"
              ? "bad"
              : lifecycle === "active" || (lifecycle === undefined && session.state === "active")
                ? "active"
                : lifecycle === undefined && isTerminal(session)
                  ? session.state === "completed"
                    ? "done"
                    : "bad"
                  : "idle";
        return (
          <ParticipantDot
            key={participant.index}
            index={participant.index}
            state={state}
            you={participant.host !== null && seats.has(participant.host)}
          />
        );
      })}
      {session.participants.length > shown.length && (
        <span className="font-mono text-2xs text-subtle">
          +{session.participants.length - shown.length}
        </span>
      )}
    </span>
  );
}

function Evidence(props: { session: Session }) {
  const { receipts } = props.session;
  if (receipts.length === 0) return <span className="text-faint">—</span>;
  const stopped = receipts.every((receipt) => receipt.kind === "stop_report");
  return stopped ? (
    <Badge tone="warn" icon={Icons.stopReport}>
      stop report
    </Badge>
  ) : (
    <Badge tone="done" icon={Icons.receipt}>
      receipt
    </Badge>
  );
}

function FilterChip(props: { name: string; value: string; onRemove: () => void }) {
  return (
    <span className="inline-flex h-5 items-center gap-1 rounded-xs border border-line bg-surface pr-0.5 pl-1.5 text-xs">
      <span className="text-subtle">{props.name}</span>
      <span className="font-mono text-fg">{props.value}</span>
      <IconButton
        icon={Icons.close}
        label={`Remove ${props.name} filter`}
        onPress={props.onRemove}
        className="size-4"
      />
    </span>
  );
}
