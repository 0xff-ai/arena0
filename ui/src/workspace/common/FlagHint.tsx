import { Button } from "react-aria-components";
import type { Flag } from "~/model";
import { Hint, Tooltip } from "~/ui";

/** The flag's chip with its sentence in a tooltip. */
export function FlagHint(props: { flag: Flag }) {
  return (
    <Tooltip title={props.flag.long}>
      {/* A real button rather than `Focusable`, which warns inside virtualized rows. */}
      <Button className="inline-flex min-w-0 max-w-full cursor-default rounded-xs outline-none focus-visible:outline-1 focus-visible:outline-offset-1 focus-visible:outline-accent">
        <Hint tone={props.flag.severity}>{props.flag.short}</Hint>
      </Button>
    </Tooltip>
  );
}
