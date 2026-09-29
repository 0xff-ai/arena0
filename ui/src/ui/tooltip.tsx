import type { ReactElement, ReactNode } from "react";
import { type Placement, Tooltip as RACTooltip, TooltipTrigger } from "react-aria-components";
import { fade } from "./internal";
import { Kbd } from "./kbd";

/**
 * Names an abbreviated or icon-only control after a 500 ms hover or on focus.
 *
 * `children` must be a React Aria component that is already focusable (Button,
 * ToggleButton, ...) or a plain element wrapped in React Aria's `Focusable`,
 * otherwise the trigger has nothing to attach to.
 */
export function Tooltip(props: {
  title: ReactNode;
  body?: ReactNode;
  kbd?: string;
  placement?: Placement;
  children: ReactElement;
}) {
  return (
    <TooltipTrigger delay={500} closeDelay={0}>
      {props.children}
      <RACTooltip
        placement={props.placement}
        offset={6}
        className={`z-50 max-w-80 rounded-md border border-line bg-elev px-2 py-1 text-sm shadow-overlay ${fade}`}
      >
        <div className="flex items-center gap-2">
          <span className="min-w-0 break-all font-medium text-fg">{props.title}</span>
          {props.kbd && <Kbd keys={props.kbd} />}
        </div>
        {props.body && <div className="mt-0.5 min-w-0 break-words text-muted">{props.body}</div>}
      </RACTooltip>
    </TooltipTrigger>
  );
}
