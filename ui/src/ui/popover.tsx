import {
  composeRenderProps,
  Popover as RACPopover,
  type PopoverProps as RACPopoverProps,
} from "react-aria-components";
import { cx } from "./cx";
import { fade } from "./internal";

/** The surface every anchored overlay (menus, selects, pickers) shares. */
export function Popover(props: RACPopoverProps) {
  return (
    <RACPopover
      offset={4}
      {...props}
      className={composeRenderProps(props.className, (className) =>
        cx(
          "z-50 rounded-md border border-line bg-elev text-fg shadow-overlay outline-none",
          fade,
          className,
        ),
      )}
    />
  );
}
