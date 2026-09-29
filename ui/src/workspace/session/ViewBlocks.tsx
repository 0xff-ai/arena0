import { type Participant, participantTone, shortHash } from "~/model";
import type { BlockRow, CellRow, ToneRow } from "~/sync";
import { cx, KeyValue, ParticipantChip } from "~/ui";

// Tailwind reads class names from source text, so each mapping is spelled out.
const toneText: Record<ToneRow, string> = {
  normal: "text-fg",
  muted: "text-muted",
  good: "text-ok",
  warn: "text-warn",
  bad: "text-bad",
  highlight: "text-fg",
};

const participantText = {
  p0: "text-p0",
  p1: "text-p1",
  p2: "text-p2",
  p3: "text-p3",
  p4: "text-p4",
  pq: "text-pq",
} as const;

/** A participant's own colour outranks the tone; `highlight` is a background, not a text colour. */
function cellClass(cell: CellRow): string {
  return cx(
    cell.participant === null
      ? toneText[cell.tone]
      : participantText[participantTone(cell.participant)],
    cell.tone === "highlight" && "bg-selected",
  );
}

function Cell(props: { cell: CellRow }) {
  return <span className={cellClass(props.cell)}>{props.cell.text}</span>;
}

function Title(props: { title: string | null }) {
  if (props.title === null) return null;
  return (
    <div className="mb-1 text-xs font-medium tracking-wide text-subtle uppercase">
      {props.title}
    </div>
  );
}

function Table(props: { block: Extract<BlockRow, { kind: "table" }> }) {
  const { block } = props;
  const columns = `repeat(${block.columns.length}, minmax(max-content, 1fr))`;
  return (
    <div role="table" aria-label={block.title ?? "Table"} className="overflow-x-auto text-sm">
      <div
        role="row"
        className="grid h-6 items-center gap-x-3 border-b border-line-soft text-subtle"
        style={{ gridTemplateColumns: columns }}
      >
        {block.columns.map((name) => (
          <span key={name} role="columnheader" className="truncate">
            {name}
          </span>
        ))}
      </div>
      {block.rows.map((row, r) => (
        <div
          // Rows have no identity beyond their position.
          key={r}
          role="row"
          className="grid h-6 items-center gap-x-3 font-mono"
          style={{ gridTemplateColumns: columns }}
        >
          {row.map((cell, c) => (
            <span key={c} role="cell" className={cx("truncate", cellClass(cell))}>
              {cell.text}
            </span>
          ))}
        </div>
      ))}
    </div>
  );
}

function Board(props: { block: Extract<BlockRow, { kind: "board" }> }) {
  const { block } = props;
  return (
    <div
      role="grid"
      aria-label={block.title ?? "Board"}
      className="grid w-96 max-w-full gap-px font-mono text-subtle"
      style={{ gridTemplateColumns: `1.5rem repeat(${block.cols}, minmax(0, 1fr))` }}
    >
      {Array.from({ length: block.rows }, (_, r) => (
        <div key={r} role="row" className="contents">
          <span role="rowheader" className="flex items-center justify-center text-sm">
            {block.row_labels[r]}
          </span>
          {Array.from({ length: block.cols }, (_, c) => {
            const cell = block.cells[r * block.cols + c];
            return (
              <span
                key={c}
                role="gridcell"
                className={cx(
                  "flex aspect-square items-center justify-center border border-line-soft text-glyph",
                  (r + c) % 2 === 1 && "bg-hover",
                  cell && cellClass(cell),
                )}
              >
                {cell?.text}
              </span>
            );
          })}
        </div>
      ))}
      <div role="row" className="contents">
        <span />
        {Array.from({ length: block.cols }, (_, c) => (
          <span
            key={c}
            role="columnheader"
            className="flex h-5 items-center justify-center text-sm"
          >
            {block.col_labels[c]}
          </span>
        ))}
      </div>
    </div>
  );
}

function Progress(props: { block: Extract<BlockRow, { kind: "progress" }> }) {
  const { label, value, max } = props.block;
  const share = max <= 0 ? 0 : Math.min(1, Math.max(0, value / max));
  return (
    <div className="flex flex-col gap-1 text-sm">
      <div className="flex items-baseline gap-2">
        <span className="text-muted">{label}</span>
        <span className="font-mono text-fg tabular">
          {value} / {max}
        </span>
      </div>
      <div
        role="progressbar"
        aria-label={label}
        aria-valuenow={value}
        aria-valuemin={0}
        aria-valuemax={max}
        className="h-1 w-full max-w-72 rounded-xs bg-line-soft"
      >
        <div className="h-full rounded-xs bg-accent" style={{ width: `${share * 100}%` }} />
      </div>
    </div>
  );
}

function Roster(props: {
  block: Extract<BlockRow, { kind: "roster" }>;
  participants: Participant[];
}) {
  return (
    <ul
      aria-label={props.block.title ?? "Roster"}
      className="m-0 flex list-none flex-col gap-1 p-0"
    >
      {props.block.entries.map((entry) => {
        const participant = props.participants.find((p) => p.index === entry.participant);
        return (
          <li key={entry.participant} className="flex items-center gap-2 text-sm">
            <ParticipantChip
              index={entry.participant}
              label={participant?.host ?? shortHash(participant?.peerId ?? "", 6)}
            />
            <Cell cell={entry.status} />
            {entry.detail !== null && <span className="text-subtle">{entry.detail}</span>}
          </li>
        );
      })}
    </ul>
  );
}

/** The typed blocks a program rendered next to its text slots. */
export function ViewBlocks(props: { blocks: BlockRow[]; participants: Participant[] }) {
  return (
    <div className="flex flex-wrap items-start gap-x-8 gap-y-3">
      {props.blocks.map((block, i) => (
        // Blocks are positional: the program orders them.
        <section key={i} className="min-w-0">
          {block.kind === "facts" && (
            <>
              <Title title={block.title} />
              <KeyValue
                items={block.items.map((item) => ({
                  k: item.label,
                  v: <Cell cell={item.value} />,
                }))}
              />
            </>
          )}
          {block.kind === "table" && (
            <>
              <Title title={block.title} />
              <Table block={block} />
            </>
          )}
          {block.kind === "board" && (
            <>
              <Title title={block.title} />
              <Board block={block} />
            </>
          )}
          {block.kind === "progress" && <Progress block={block} />}
          {block.kind === "roster" && (
            <>
              <Title title={block.title} />
              <Roster block={block} participants={props.participants} />
            </>
          )}
        </section>
      ))}
    </div>
  );
}
