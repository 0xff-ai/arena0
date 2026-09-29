import { type ReactNode, useEffect, useRef, useState } from "react";
import { GridList, GridListItem, ListLayout, Virtualizer } from "react-aria-components";
import {
  fmtBytes,
  fmtClock,
  fmtDuration,
  type Participant,
  type Session,
  shortHash,
  useNow,
  useRows,
  useSeats,
  useSessionSteps,
} from "~/model";
import { type RecordRow, type StepRow, useCollections } from "~/sync";
import {
  AgreementMeter,
  Badge,
  Button,
  EmptyState,
  Fingerprint,
  Icons,
  InlineJson,
  JsonView,
  ParticipantChip,
  Switch,
  Toolbar,
} from "~/ui";
import { useComposer } from "../composer/store";
import { useRecords } from "./records";
import type { UpdateSearch } from "./update";

const ROW_PX = 26;

type Item =
  | { id: string; kind: "step"; step: number; rows: StepRow[] }
  | { id: string; kind: "record"; host: string; record: RecordRow }
  | { id: "tail"; kind: "tail" };

const RECORD_EVENT = {
  session_started: "start",
  message_received: "message",
  input_received: "input",
  timer_fired: "timer",
  direct_received: "direct",
} as const;

function recordText(host: string, record: RecordRow): string {
  const input = record.input_bytes === null ? "" : ` ${fmtBytes(record.input_bytes)}`;
  const effects = record.effects
    .map(
      (effect) =>
        `${effect.kind.replace("_", " ")}${effect.bytes === null ? "" : ` ${fmtBytes(effect.bytes)}`}`,
    )
    .join(", ");
  return `▸ ${host} · ${RECORD_EVENT[record.event]}${input}${effects === "" ? "" : ` → ${effects}`}`;
}

/** Steps grouped by number, then each Host's records under the last step they produced. */
function buildItems(steps: StepRow[], records: Map<string, RecordRow[]>): Item[] {
  const byStep = new Map<number, StepRow[]>();
  for (const row of steps) byStep.set(row.step, [...(byStep.get(row.step) ?? []), row]);

  // A record that produced no step follows the step the Host had reached; before any, it leads.
  const under = new Map<number, Item[]>();
  for (const [host, list] of [...records].sort(([a], [b]) => (a < b ? -1 : 1))) {
    let anchor = -1;
    for (const record of [...list].sort((a, b) => a.position - b.position)) {
      anchor = record.steps.at(-1) ?? anchor;
      const items = under.get(anchor) ?? [];
      items.push({ id: `r:${host}:${record.position}`, kind: "record", host, record });
      under.set(anchor, items);
    }
  }

  const items: Item[] = [...(under.get(-1) ?? [])];
  for (const [step, rows] of [...byStep].sort(([a], [b]) => a - b)) {
    items.push({ id: `s:${step}`, kind: "step", step, rows });
    items.push(...(under.get(step) ?? []));
  }
  items.push({ id: "tail", kind: "tail" });
  return items;
}

function authorOf(session: Session, event: StepRow["event"]): Participant | null {
  if (event.kind !== "message") return null;
  return session.participants.find((p) => p.peerId === event.from) ?? null;
}

function StepEvent(props: { row: StepRow; expanded: boolean }) {
  const { event } = props.row;
  if (event.kind === "session_started") {
    return (
      <span className="text-muted">session started · {event.ensemble.length} participants</span>
    );
  }
  if (event.decode_error !== null) {
    return (
      <span className="text-bad">
        decode error: {event.decode_error} ({fmtBytes(event.bytes)})
      </span>
    );
  }
  if (event.decoded === null)
    return <span className="text-muted">message · {fmtBytes(event.bytes)}</span>;
  return props.expanded ? (
    <span className="text-muted">message</span>
  ) : (
    <InlineJson value={event.decoded} />
  );
}

/** All local Hosts stored the same post-state, differ, or some have not stored the step yet. */
function Alignment(props: { rows: StepRow[]; hosts: number }) {
  const states = new Set(props.rows.map((row) => row.post_state));
  if (props.rows.length < props.hosts) {
    return (
      <span title="not stored on every local Host yet" className="w-4 text-center text-subtle">
        –
      </span>
    );
  }
  if (states.size > 1) {
    return (
      <span
        role="img"
        aria-label="Hosts differ"
        title="Hosts stored different states"
        className="w-4 text-center text-bad"
      >
        ≠
      </span>
    );
  }
  return (
    <span
      role="img"
      aria-label="Hosts agree"
      title="Every local Host stored the same state"
      className="flex w-4 justify-center text-ok"
    >
      <Icons.check size={12} aria-hidden />
    </span>
  );
}

function StepLine(props: {
  session: Session;
  item: Extract<Item, { kind: "step" }>;
  previous: number | null;
  expanded: boolean;
  onToggle: () => void;
}) {
  const { session, item } = props;
  const first = item.rows[0];
  if (first === undefined) return null;
  const author = authorOf(session, first.event);
  const certified = Math.min(...item.rows.map((row) => row.certified_ms));
  const terminal = first.terminal;
  const expandable = first.event.kind === "message" && first.event.decoded !== null;
  return (
    <div className="flex min-w-0 flex-col">
      <div className="flex h-6.5 min-w-0 items-center gap-2">
        <span className="w-8 shrink-0 text-right font-mono text-sm text-faint tabular">
          {item.step}
        </span>
        <span className="w-9 shrink-0">
          {author && (
            <ParticipantChip
              index={author.index}
              label=""
              title={`${author.host ?? "remote"} · ${shortHash(author.peerId)}`}
            />
          )}
        </span>
        <span className="flex min-w-0 flex-1 items-center gap-2 text-sm">
          <Button
            variant="ghost"
            size="sm"
            isDisabled={!expandable}
            onPress={props.onToggle}
            aria-expanded={expandable ? props.expanded : undefined}
            className="h-5 min-w-0 shrink justify-start px-1 font-normal disabled:opacity-100"
          >
            <span className="min-w-0 truncate">
              <StepEvent row={first} expanded={props.expanded} />
            </span>
          </Button>
          {terminal && (
            // Outside the truncated event, so a long payload never hides how the session ended.
            <span className="shrink-0">
              <Badge tone={terminal.kind === "end" ? "done" : "bad"}>
                {terminal.kind === "end"
                  ? `end · outcome ${fmtBytes(terminal.outcome_bytes)}`
                  : `${terminal.kind} · ${terminal.reason}`}
              </Badge>
            </span>
          )}
        </span>
        <span className="flex w-24 shrink-0 items-center gap-1.5 font-mono text-xs text-muted">
          <Fingerprint hash={first.post_state} size="sm" />
          {shortHash(first.post_state, 6)}
        </span>
        <span className="flex w-16 shrink-0 justify-end">
          <AgreementMeter signers={first.signers} participants={first.participants} />
        </span>
        <Alignment rows={item.rows} hosts={session.executions.length} />
        <span className="w-24 shrink-0 text-right font-mono text-xs text-subtle tabular">
          {fmtClock(certified)}
          {props.previous !== null && ` +${fmtDuration(certified - props.previous)}`}
        </span>
      </div>
      {props.expanded && first.event.kind === "message" && first.event.decoded !== null && (
        <div className="pb-1.5 pl-12">
          <JsonView value={first.event.decoded} collapseDepth={2} />
        </div>
      )}
    </div>
  );
}

function Tail(props: { session: Session; hasTerminalStep: boolean; nextStep: number }) {
  const { session } = props;
  const now = useNow();
  const composer = useComposer();
  const hosts = useRows(useCollections().hosts).map((host) => host.id);
  const [seats] = useSeats(hosts);
  const indexOf = (peerOrHost: { host?: string; peer?: string }) =>
    session.participants.find((p) =>
      peerOrHost.host !== undefined ? p.host === peerOrHost.host : p.peerId === peerOrHost.peer,
    )?.index;

  const line = (glyph: string, children: ReactNode, tone = "text-muted") => (
    <div className="flex h-6.5 items-center gap-2 text-sm">
      <span className="w-8 shrink-0 text-right font-mono text-faint tabular">{glyph}</span>
      <span className="w-9 shrink-0" />
      <span className={`min-w-0 flex-1 truncate ${tone}`}>{children}</span>
    </div>
  );

  if (session.terminal !== null) {
    if (props.hasTerminalStep) return null;
    const { kind, reason } = session.terminal;
    return line(
      "■",
      `${kind}${reason ? ` · ${reason}` : ""}`,
      kind === "completed" ? "text-muted" : "text-bad",
    );
  }
  const callout = session.callouts[0];
  if (callout !== undefined) {
    const index = indexOf({ host: callout.host });
    return line(
      String(props.nextStep),
      <span className="inline-flex items-center gap-2">
        <span>
          waiting on {index === undefined ? "" : `P${index} · `}
          {callout.host} · {callout.name} · {fmtDuration(now - callout.opened_ms)}
        </span>
        {seats.has(callout.host) && (
          <Button size="sm" variant="primary" onPress={() => composer.open(callout.key)}>
            Answer
          </Button>
        )}
      </span>,
    );
  }
  if (session.state === "active" && session.writer !== null) {
    const index = indexOf({ peer: session.writer });
    return line(
      String(props.nextStep),
      `${index === undefined ? shortHash(session.writer) : `P${index}`} to write`,
    );
  }
  if (session.latestStep === null) {
    return line(
      "",
      `${session.state}: the first step is certified after every participant commits`,
    );
  }
  return null;
}

export function Steps(props: { session: Session; step: number | undefined; update: UpdateSearch }) {
  const { session } = props;
  const steps = useSessionSteps(session);
  const [showLocal, setShowLocal] = useState(false);
  const records = useRecords(session, showLocal);
  const [expanded, setExpanded] = useState<ReadonlySet<number>>(new Set());
  const items = buildItems(steps, showLocal ? records : new Map());
  const scroller = useRef<HTMLDivElement | null>(null);

  const stepIndex = items.findIndex((item) => item.kind === "step" && item.step === props.step);
  // The list is virtualized, so a row far away is not in the document. Jump near it by the
  // nominal row height, then let the browser settle on the row once it is mounted.
  useEffect(() => {
    const element = scroller.current;
    if (element === null || props.step === undefined || stepIndex < 0) return;
    element.scrollTop = Math.max(0, stepIndex * ROW_PX - element.clientHeight / 2);
    const frame = requestAnimationFrame(() => {
      element.querySelector(`[data-key="s:${props.step}"]`)?.scrollIntoView({ block: "nearest" });
    });
    return () => cancelAnimationFrame(frame);
  }, [props.step, stepIndex]);

  const previousMs = new Map<number, number>();
  let last: number | null = null;
  for (const item of items) {
    if (item.kind !== "step") continue;
    if (last !== null) previousMs.set(item.step, last);
    last = Math.min(...item.rows.map((row) => row.certified_ms));
  }
  const hasTerminalStep = steps.some((row) => row.terminal !== null);
  const nextStep = (session.latestStep ?? -1) + 1;

  return (
    <div className="flex h-full min-h-0 flex-col">
      <Toolbar aria-label="Step options">
        <Switch isSelected={showLocal} onChange={setShowLocal}>
          Local events
        </Switch>
        <span className="text-xs text-subtle">
          Each Host's own event log, folded under the step it produced.
        </span>
      </Toolbar>
      <div className="min-h-0 flex-1">
        <Virtualizer layout={ListLayout} layoutOptions={{ estimatedRowHeight: ROW_PX }}>
          <GridList
            ref={scroller}
            aria-label="Steps"
            items={items}
            selectionMode="single"
            selectionBehavior="replace"
            disallowEmptySelection={false}
            selectedKeys={props.step === undefined ? [] : [`s:${props.step}`]}
            onSelectionChange={(keys) => {
              if (keys === "all") return;
              const [key] = keys;
              if (typeof key === "string" && key.startsWith("s:")) {
                props.update({ step: Number(key.slice(2)) });
              }
            }}
            renderEmptyState={() => (
              <EmptyState icon={Icons.steps} title="No certified steps yet" />
            )}
            className="h-full overflow-auto outline-none"
          >
            {(item) => (
              <GridListItem
                id={item.id}
                textValue={item.kind === "step" ? `Step ${item.step}` : item.id}
                aria-label={
                  item.kind === "step"
                    ? `Step ${item.step}`
                    : item.kind === "tail"
                      ? "End of steps"
                      : "Local events"
                }
                className="block cursor-default px-2 text-sm text-fg outline-none hovered:bg-hover selected:bg-selected focus-visible:outline-1 focus-visible:-outline-offset-1 focus-visible:outline-accent"
              >
                {item.kind === "step" && (
                  <StepLine
                    session={session}
                    item={item}
                    previous={previousMs.get(item.step) ?? null}
                    expanded={expanded.has(item.step)}
                    onToggle={() => {
                      props.update({ step: item.step });
                      setExpanded((current) => {
                        const next = new Set(current);
                        if (!next.delete(item.step)) next.add(item.step);
                        return next;
                      });
                    }}
                  />
                )}
                {item.kind === "record" && (
                  <div className="flex h-6.5 items-center gap-2 pl-12 font-mono text-xs text-subtle">
                    <span className="min-w-0 truncate">{recordText(item.host, item.record)}</span>
                  </div>
                )}
                {item.kind === "tail" && (
                  <Tail session={session} hasTerminalStep={hasTerminalStep} nextStep={nextStep} />
                )}
              </GridListItem>
            )}
          </GridList>
        </Virtualizer>
      </div>
    </div>
  );
}
