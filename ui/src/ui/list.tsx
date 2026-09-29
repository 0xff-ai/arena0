import type { ReactNode } from "react";
import { GridList, GridListItem, ListLayout, Virtualizer } from "react-aria-components";

/**
 * A virtualized single-select list. It scrolls inside its own box, so the
 * parent must give it a height. Enter or double-click runs `onAction`.
 */
export function List<T>(props: {
  label: string;
  items: T[];
  getKey: (item: T) => string;
  children: (item: T) => ReactNode;
  rowHeight?: number;
  selectedKey?: string | null;
  onSelect?: (key: string) => void;
  onAction?: (key: string) => void;
  empty: ReactNode;
}) {
  return (
    <Virtualizer layout={ListLayout} layoutOptions={{ rowHeight: props.rowHeight ?? 26 }}>
      <GridList
        aria-label={props.label}
        items={props.items.map((item) => ({ id: props.getKey(item), item }))}
        selectionMode={props.onSelect ? "single" : "none"}
        selectionBehavior="replace"
        selectedKeys={props.selectedKey ? [props.selectedKey] : []}
        onSelectionChange={(keys) => {
          if (keys === "all") return;
          const [key] = keys;
          if (typeof key === "string") props.onSelect?.(key);
        }}
        onAction={(key) => props.onAction?.(String(key))}
        renderEmptyState={() => props.empty}
        className="h-full overflow-auto outline-none"
      >
        {({ id, item }) => (
          <GridListItem
            id={id}
            textValue={id}
            className="flex h-full cursor-default items-center px-2 text-sm text-fg outline-none hovered:bg-hover selected:bg-selected focus-visible:outline-1 focus-visible:-outline-offset-1 focus-visible:outline-accent"
          >
            {props.children(item)}
          </GridListItem>
        )}
      </GridList>
    </Virtualizer>
  );
}
