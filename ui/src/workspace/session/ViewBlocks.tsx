import type { Block, Tone, Cell as ViewCell } from "~/api/types.gen";
import type { Session } from "~/model";
import { cx, KeyValue, participantText, participantToneOf } from "~/ui";
import { participantChip } from "../common/SessionLabel";

// Tailwind reads class names from source text, so each mapping is spelled out.
const toneText: Record<Tone, string> = {
  normal: "text-fg",
  muted: "text-muted",
  good: "text-ok",
  warn: "text-warn",
  bad: "text-bad",
  highlight: "text-fg",
};

/** A participant's own colour outranks the tone; `highlight` is a background, not a text colour. */
function cellClass(cell: ViewCell): string {
  return cx(
    cell.participant === undefined
      ? toneText[cell.tone]
      : participantText[participantToneOf(cell.participant)],
    cell.tone === "highlight" && "bg-selected",
  );
}

function Cell(props: { cell: ViewCell }) {
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

function Table(props: { block: Extract<Block, { kind: "table" }> }) {
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

function Board(props: { block: Extract<Block, { kind: "board" }> }) {
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

function Progress(props: { block: Extract<Block, { kind: "progress" }> }) {
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

function Roster(props: { block: Extract<Block, { kind: "roster" }>; session: Session }) {
  return (
    <ul
      aria-label={props.block.title ?? "Roster"}
      className="m-0 flex list-none flex-col gap-1 p-0"
    >
      {props.block.entries.map((entry) => (
        <li key={entry.participant} className="flex items-center gap-2 text-sm">
          {participantChip(props.session, entry.participant)}
          <Cell cell={entry.status} />
          {entry.detail !== null && <span className="text-subtle">{entry.detail}</span>}
        </li>
      ))}
    </ul>
  );
}

/** The typed blocks a program rendered next to its text slots. */
export function ViewBlocks(props: { blocks: Block[]; session: Session }) {
  return (
    <div className="flex flex-wrap items-start gap-x-8 gap-y-3">
      {props.blocks.map((block, i) => (
        // Blocks are positional: the program orders them.
        <section key={i} className="min-w-0">
          {block.kind !== "progress" && <Title title={block.title} />}
          {block.kind === "facts" && (
            <KeyValue
              items={block.items.map((item) => ({ k: item.label, v: <Cell cell={item.value} /> }))}
            />
          )}
          {block.kind === "table" && <Table block={block} />}
          {block.kind === "board" && <Board block={block} />}
          {block.kind === "progress" && <Progress block={block} />}
          {block.kind === "roster" && <Roster block={block} session={props.session} />}
        </section>
      ))}
    </div>
  );
}
