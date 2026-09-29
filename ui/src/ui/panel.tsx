import type { ReactNode } from "react";
import { Separator } from "react-resizable-panels";
import { tv } from "./cx";

export { Group as PanelGroup, Panel } from "react-resizable-panels";

/** The title row at the top of a dock. */
export function DockHeader(props: { title: string; count?: ReactNode; actions?: ReactNode }) {
  return (
    <div className="flex h-8 shrink-0 items-center gap-1.5 border-b border-line-soft pr-1.5 pl-3 text-sm">
      <span className="min-w-0 truncate font-medium text-fg">{props.title}</span>
      {props.count !== undefined && (
        <span className="shrink-0 font-mono text-2xs text-subtle tabular">{props.count}</span>
      )}
      <span className="flex-1" />
      {props.actions && <div className="flex shrink-0 items-center">{props.actions}</div>}
    </div>
  );
}

// The element is a 5 px hit area that overlaps its neighbours by 2 px each
// side, so the visible 1 px line takes 1 px of layout.
const handle = tv({
  base: "group relative z-10 flex shrink-0 items-center justify-center outline-none",
  variants: {
    orientation: {
      horizontal: "-mx-0.5 w-[5px] self-stretch",
      vertical: "-my-0.5 h-[5px] w-full flex-col",
    },
  },
});

const line = tv({
  base: "bg-line-soft transition-colors duration-120 group-data-[separator=active]:bg-accent group-data-[separator=focus]:bg-accent group-data-[separator=hover]:bg-accent",
  variants: {
    orientation: {
      horizontal: "h-full w-px",
      vertical: "h-px w-full",
    },
  },
});

/** `orientation` is the enclosing `PanelGroup`'s: a horizontal group is split by vertical lines. */
export function PanelHandle(props: { orientation: "horizontal" | "vertical" }) {
  return (
    <Separator className={handle({ orientation: props.orientation })}>
      <span className={line({ orientation: props.orientation })} />
    </Separator>
  );
}
