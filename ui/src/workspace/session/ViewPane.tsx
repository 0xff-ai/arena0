import { type ReactNode, useEffect, useRef, useState } from "react";
import type { UpdateSearch } from "~/app/router";
import type { Session } from "~/model";
import { useRead, type ViewReply } from "~/sync";
import {
  AnsiText,
  EmptyState,
  Icons,
  Segmented,
  Select,
  Slider,
  Toggle,
  Toolbar,
  ToolbarSeparator,
  useElementSize,
} from "~/ui";
import { ViewBlocks } from "./ViewBlocks";

type Mode = "blocks" | "text" | "both";

const MODES = [
  { id: "blocks", label: "Blocks" },
  { id: "text", label: "Text" },
  { id: "both", label: "Both" },
] as const;

/** Text panes stack their slots under this width and sit side by side above it. */
const SIDE_BY_SIDE_PX = 900;
const PAD_PX = 24;
const LINE_PX = 20;
const NO_COMPARISON = "none";

/** The width of one monospace cell at the pane's font, measured from ten glyphs. */
function useCellWidth(): [React.RefObject<HTMLSpanElement | null>, number] {
  const ref = useRef<HTMLSpanElement | null>(null);
  const [width, setWidth] = useState(0);
  useEffect(() => {
    const element = ref.current;
    if (element !== null) setWidth(element.getBoundingClientRect().width / 10);
  }, []);
  return [ref, width];
}

/** Lines of `text` that differ from the same line of `other`, marked behind the text. */
function Slot(props: { text: string; other: string | null | undefined; className?: string }) {
  const lines = props.text.split("\n");
  const otherLines = props.other === undefined ? null : (props.other ?? "").split("\n");
  return (
    <div className="relative min-w-0 overflow-x-auto">
      {otherLines !== null &&
        lines.map((line, i) =>
          line === otherLines[i] ? null : (
            <div
              key={i}
              aria-hidden
              className="absolute inset-x-0 bg-warn/15"
              style={{ top: i * LINE_PX, height: LINE_PX }}
            />
          ),
        )}
      <AnsiText text={props.text} className={`relative text-code ${props.className ?? ""}`} />
    </div>
  );
}

function Pane(props: {
  reply: ViewReply;
  other: ViewReply | null | undefined;
  mode: Mode;
  session: Session;
  sideBySide: boolean;
}) {
  const { reply, other } = props;
  const showBlocks = props.mode !== "text" && reply.blocks.length > 0;
  const showText = props.mode !== "blocks" || reply.blocks.length === 0;
  const slot = (name: "header" | "agents" | "state" | "status_bar", className?: string) => {
    const text = reply[name];
    return text === null ? null : (
      <Slot
        text={text}
        other={other === undefined ? undefined : other?.[name]}
        className={className}
      />
    );
  };
  const body: ReactNode[] = [];
  if (showBlocks) {
    body.push(
      <div key="blocks" className="p-3">
        <ViewBlocks blocks={reply.blocks} session={props.session} />
      </div>,
    );
  }
  if (showText) {
    const header = slot("header", "font-medium");
    const agents = slot("agents");
    const state = slot("state");
    const status = slot("status_bar");
    if (header)
      body.push(
        <div key="header" className="px-3 py-1">
          {header}
        </div>,
      );
    if (agents || state) {
      body.push(
        <div
          key="middle"
          className={
            props.sideBySide
              ? "grid grid-cols-2 divide-x divide-line-soft border-t border-line-soft"
              : "flex flex-col divide-y divide-line-soft border-t border-line-soft"
          }
        >
          {agents && <div className="px-3 py-1">{agents}</div>}
          {state && <div className="px-3 py-1">{state}</div>}
        </div>,
      );
    }
    if (status)
      body.push(
        <div key="status" className="border-t border-line-soft px-3 py-1">
          {status}
        </div>,
      );
  }
  return <div className="flex min-w-0 flex-col divide-y divide-line-soft bg-editor">{body}</div>;
}

export function ViewPane(props: {
  session: Session;
  host: string | undefined;
  step: number | undefined;
  compare: string | undefined;
  update: UpdateSearch;
}) {
  const { session, step } = props;
  const executions = session.executions;
  const first = executions[0];
  const host = props.host ?? first?.host;
  const compare = props.compare !== undefined && props.compare !== host ? props.compare : undefined;
  const [mode, setMode] = useState<Mode>("both");
  const [bodyRef, { width: bodyWidth }] = useElementSize<HTMLDivElement>();
  const [probeRef, cellWidth] = useCellWidth();

  const latest = session.latestStep;
  const panes = compare === undefined ? 1 : 2;
  const paneWidth = bodyWidth / panes;
  const sideBySide = paneWidth >= SIDE_BY_SIDE_PX;
  const measured = cellWidth > 0 && bodyWidth > 0;
  const cells = measured
    ? Math.max(20, Math.floor((paneWidth - 2 * PAD_PX) / cellWidth / (sideBySide ? 2 : 1)))
    : 0;

  function viewArgs(id: string | undefined) {
    const execution = executions.find((e) => e.host === id);
    if (execution === undefined || !measured) return null;
    // The Host's own latest step, not "latest", so the read is keyed by it and refreshes with new steps.
    const at =
      step === undefined ? execution.latest_step : Math.min(step, execution.latest_step ?? step);
    return { host: execution.host, exec_id: execution.exec_id, width: cells, at_step: at };
  }
  const a = useRead("view", viewArgs(host));
  const b = useRead("view", viewArgs(compare));

  if (first === undefined || host === undefined) {
    return <EmptyState icon={Icons.view} title="No execution to show" />;
  }
  const hostItems = executions.map((e) => ({ id: e.host, label: e.host }));
  const compareItems = [
    { id: NO_COMPARISON, label: "No comparison" },
    ...executions.filter((e) => e.host !== host).map((e) => ({ id: e.host, label: e.host })),
  ];
  const current = step === undefined ? (latest ?? 0) : Math.min(step, latest ?? 0);

  const results = [
    { id: host, query: a, other: compare === undefined ? undefined : b.data },
    ...(compare === undefined ? [] : [{ id: compare, query: b, other: a.data }]),
  ];

  return (
    <div className="flex h-full min-h-0 flex-col">
      <Toolbar aria-label="View controls">
        <div className="w-28">
          <Select
            placeholder="Host"
            items={hostItems}
            value={host}
            onChange={(id) => props.update({ host: id })}
          />
        </div>
        <div className="w-36">
          <Select
            placeholder="Compare"
            items={compareItems}
            value={compare ?? NO_COMPARISON}
            onChange={(id) => props.update({ compare: id === NO_COMPARISON ? undefined : id })}
          />
        </div>
        <ToolbarSeparator />
        <Slider
          label="Step"
          max={latest ?? 0}
          isDisabled={latest === null || latest === 0}
          value={current}
          onChange={(value) => props.update({ step: value })}
          className="min-w-24 max-w-64 flex-1"
        />
        <span className="w-14 font-mono text-sm text-muted tabular">
          {latest === null ? "no steps" : `${current} / ${latest}`}
        </span>
        <Toggle
          isSelected={step === undefined}
          onChange={(selected) => props.update({ step: selected ? undefined : (latest ?? 0) })}
        >
          Latest
        </Toggle>
        <div className="flex-1" />
        <Segmented label="View mode" items={[...MODES]} value={mode} onChange={setMode} />
      </Toolbar>
      <div ref={bodyRef} className="relative min-h-0 flex-1 overflow-auto">
        <span
          ref={probeRef}
          aria-hidden
          className="invisible absolute font-mono text-code whitespace-pre"
        >
          0000000000
        </span>
        <div className={panes === 2 ? "grid grid-cols-2 divide-x divide-line" : ""}>
          {results.map(({ id, query, other }) => (
            <div key={id} className="min-w-0">
              {compare !== undefined && (
                <div className="border-b border-line-soft px-3 py-1 font-mono text-sm text-muted">
                  {id}
                </div>
              )}
              {query.error && (
                <div role="alert" className="p-3 text-sm text-bad">
                  {query.error.message}
                </div>
              )}
              {query.data && (
                <Pane
                  reply={query.data}
                  other={other}
                  mode={mode}
                  session={session}
                  sideBySide={sideBySide}
                />
              )}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
