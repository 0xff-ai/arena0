import type { ReactNode } from "react";
import { cx } from "./cx";

/**
 * Label and value pairs in two aligned columns. A `title` carries the full
 * value of a truncated one as a native tooltip: it keeps the list semantics
 * of `dl` and adds no tab stops to a dense inspector.
 */
export function KeyValue(props: {
  items: { k: string; v: ReactNode; mono?: boolean; title?: string }[];
  dense?: boolean;
}) {
  return (
    <dl
      className={cx(
        "grid grid-cols-[max-content_minmax(0,1fr)] items-baseline gap-x-3",
        props.dense ? "gap-y-0.5 text-xs" : "gap-y-1 text-sm",
      )}
    >
      {props.items.map((item) => (
        <div key={item.k} className="contents">
          <dt className="text-subtle">{item.k}</dt>
          <dd
            title={item.title}
            className={cx("m-0 min-w-0 truncate text-fg", item.mono && "font-mono tabular")}
          >
            {item.v}
          </dd>
        </div>
      ))}
    </dl>
  );
}
