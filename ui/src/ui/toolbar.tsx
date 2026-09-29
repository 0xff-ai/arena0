import {
  composeRenderProps,
  Toolbar as RACToolbar,
  type ToolbarProps as RACToolbarProps,
  Separator,
} from "react-aria-components";
import { cx } from "./cx";

/** A row of controls with roving arrow-key focus. */
export function Toolbar(props: RACToolbarProps) {
  return (
    <RACToolbar
      {...props}
      className={composeRenderProps(props.className, (className) =>
        cx("flex h-8.5 shrink-0 items-center gap-2 border-b border-line-soft px-2.5", className),
      )}
    />
  );
}

export function ToolbarSeparator() {
  return <Separator orientation="vertical" className="mx-0.5 h-4 w-px shrink-0 bg-line" />;
}
