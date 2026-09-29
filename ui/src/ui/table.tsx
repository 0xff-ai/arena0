import type { ReactNode } from "react";
import {
  Cell,
  Column as RACColumn,
  Row,
  Table,
  TableBody,
  TableHeader,
  TableLayout,
  Virtualizer,
} from "react-aria-components";
import { cx } from "./cx";

export interface Column<T> {
  id: string;
  title: string;
  width?: number | `${number}fr`;
  minWidth?: number;
  align?: "start" | "end";
  isRowHeader?: boolean;
  render: (row: T) => ReactNode;
}

const HEADING_HEIGHT = 24;

/**
 * A virtualized single-select table. It scrolls inside its own box, so the
 * parent must give it a height. `rowTone: "quiet"` dims a routine row.
 */
export function DataTable<T>(props: {
  label: string;
  columns: Column<T>[];
  rows: T[];
  getKey: (row: T) => string;
  rowHeight?: number;
  selectedKey?: string | null;
  onSelect?: (key: string) => void;
  onAction?: (key: string) => void;
  rowTone?: (row: T) => "quiet" | "normal";
  empty: ReactNode;
}) {
  return (
    <Virtualizer
      layout={TableLayout}
      layoutOptions={{ rowHeight: props.rowHeight ?? 26, headingHeight: HEADING_HEIGHT }}
    >
      <Table
        aria-label={props.label}
        selectionMode={props.onSelect ? "single" : "none"}
        selectionBehavior="replace"
        selectedKeys={props.selectedKey ? [props.selectedKey] : []}
        onSelectionChange={(keys) => {
          if (keys === "all") return;
          const [key] = keys;
          if (typeof key === "string") props.onSelect?.(key);
        }}
        onRowAction={(key) => props.onAction?.(String(key))}
        className="h-full overflow-auto outline-none"
      >
        <TableHeader columns={props.columns}>
          {(column) => (
            <RACColumn
              id={column.id}
              isRowHeader={column.isRowHeader}
              width={column.width}
              minWidth={column.minWidth}
              className={cx(
                "flex items-center border-b border-line-soft bg-editor px-2 text-xs font-normal whitespace-nowrap text-subtle outline-none focus-visible:outline-1 focus-visible:-outline-offset-1 focus-visible:outline-accent",
                column.align === "end" && "justify-end text-end",
              )}
            >
              {column.title}
            </RACColumn>
          )}
        </TableHeader>
        <TableBody
          items={props.rows.map((row) => ({ id: props.getKey(row), row }))}
          renderEmptyState={() => props.empty}
        >
          {({ id, row }) => (
            <Row
              id={id}
              columns={props.columns}
              className={cx(
                // The virtualizer positions the cells absolutely, so the row needs
                // an explicit height to paint its own state.
                "h-full cursor-default text-sm outline-none hovered:bg-hover selected:bg-selected focus-visible:outline-1 focus-visible:-outline-offset-1 focus-visible:outline-accent",
                props.rowTone?.(row) === "quiet" ? "text-muted" : "text-fg",
              )}
            >
              {(column) => (
                <Cell
                  className={cx(
                    "flex min-w-0 items-center overflow-hidden px-2 outline-none focus-visible:outline-1 focus-visible:-outline-offset-1 focus-visible:outline-accent",
                    column.align === "end" && "justify-end text-end",
                  )}
                >
                  {column.render(row)}
                </Cell>
              )}
            </Row>
          )}
        </TableBody>
      </Table>
    </Virtualizer>
  );
}
