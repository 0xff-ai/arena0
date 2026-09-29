import { type ReactNode, useState } from "react";
import { Button } from "react-aria-components";
import { Icons } from "../icons";
import { Icon } from "../internal";

export type JsonLike = null | boolean | number | string | JsonLike[] | { [k: string]: JsonLike };

const punct = "text-subtle";

function Primitive(props: { value: null | boolean | number | string }) {
  const { value } = props;
  if (typeof value === "string")
    return (
      <span className="text-syn-str break-all whitespace-pre-wrap">{JSON.stringify(value)}</span>
    );
  if (typeof value === "number") return <span className="text-syn-num">{String(value)}</span>;
  return <span className="text-syn-const">{String(value)}</span>;
}

function Key(props: { name?: string }) {
  if (props.name === undefined) return null;
  return (
    <>
      <span className="text-syn-fn">{JSON.stringify(props.name)}</span>
      <span className={punct}>: </span>
    </>
  );
}

// One line: a 16 px gutter (holding the collapse toggle when there is one), then the text.
function Line(props: { toggle?: ReactNode; children: ReactNode }) {
  return (
    <div className="flex">
      <span className="flex w-4 shrink-0 justify-start">{props.toggle}</span>
      <span className="min-w-0">{props.children}</span>
    </div>
  );
}

function Node(props: {
  name?: string;
  value: JsonLike;
  depth: number;
  collapseDepth: number;
  last: boolean;
}) {
  const { value, depth, collapseDepth } = props;
  const [open, setOpen] = useState(depth <= collapseDepth);
  const comma = props.last ? null : <span className={punct}>,</span>;

  if (value === null || typeof value !== "object") {
    return (
      <Line>
        <Key name={props.name} />
        <Primitive value={value} />
        {comma}
      </Line>
    );
  }

  const isArray = Array.isArray(value);
  const entries: [string, JsonLike][] = isArray
    ? value.map((item, i) => [String(i), item])
    : Object.entries(value);
  const [opening, closing] = isArray ? ["[", "]"] : ["{", "}"];

  if (entries.length === 0) {
    return (
      <Line>
        <Key name={props.name} />
        <span className={punct}>
          {opening}
          {closing}
        </span>
        {comma}
      </Line>
    );
  }

  return (
    <div>
      <Line
        toggle={
          <Button
            aria-label={open ? "Collapse" : "Expand"}
            aria-expanded={open}
            onPress={() => setOpen(!open)}
            className="flex size-4 cursor-default items-center justify-center rounded-xs text-subtle outline-none hovered:text-fg focus-visible:outline-1 focus-visible:-outline-offset-1 focus-visible:outline-accent"
          >
            <Icon icon={open ? Icons.chevronDown : Icons.chevronRight} size={12} />
          </Button>
        }
      >
        <Key name={props.name} />
        <span className={punct}>{opening}</span>
        {!open && (
          <>
            <span className="text-subtle">
              {" "}
              {entries.length}{" "}
              {isArray
                ? entries.length === 1
                  ? "item"
                  : "items"
                : entries.length === 1
                  ? "key"
                  : "keys"}{" "}
            </span>
            <span className={punct}>{closing}</span>
            {comma}
          </>
        )}
      </Line>
      {open && (
        <>
          <div className="pl-4">
            {entries.map(([key, item], i) => (
              <Node
                key={key}
                name={isArray ? undefined : key}
                value={item}
                depth={depth + 1}
                collapseDepth={collapseDepth}
                last={i === entries.length - 1}
              />
            ))}
          </div>
          <Line>
            <span className={punct}>{closing}</span>
            {comma}
          </Line>
        </>
      )}
    </div>
  );
}

/** Pretty-printed JSON with syntax colours. Containers nested deeper than `collapseDepth` (default 2) start collapsed. */
export function JsonView(props: { value: JsonLike; collapseDepth?: number }) {
  return (
    <div className="font-mono text-sm leading-5 text-fg">
      <Node value={props.value} depth={0} collapseDepth={props.collapseDepth ?? 2} last />
    </div>
  );
}
