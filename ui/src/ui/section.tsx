import type { LucideIcon } from "lucide-react";
import type { ReactNode } from "react";
import { Button, Disclosure, DisclosurePanel, Heading } from "react-aria-components";
import { IconButton } from "./button";
import { Icons } from "./icons";
import { Icon } from "./internal";

/** A collapsible group in a dock: a 22 px label row over its content. */
export function Section(props: {
  title: string;
  icon?: LucideIcon;
  count?: ReactNode;
  actions?: ReactNode;
  /** Adds a focus button that opens the section's full document. */
  onOpen?: () => void;
  defaultExpanded?: boolean;
  children: ReactNode;
}) {
  return (
    <Disclosure defaultExpanded={props.defaultExpanded ?? true} className="group">
      <div className="flex h-5.5 items-center gap-1 pr-1.5 pl-1.5">
        <Heading level={3} className="h-full min-w-0 flex-1">
          <Button
            slot="trigger"
            className="flex h-full w-full min-w-0 cursor-default items-center gap-1 rounded-xs text-xs font-medium tracking-wide text-subtle uppercase outline-none hovered:text-muted focus-visible:outline-1 focus-visible:-outline-offset-1 focus-visible:outline-accent"
          >
            <Icon icon={Icons.chevronDown} size={12} className="hidden group-expanded:block" />
            <Icon icon={Icons.chevronRight} size={12} className="group-expanded:hidden" />
            {props.icon && <Icon icon={props.icon} size={12} />}
            <span className="truncate">{props.title}</span>
          </Button>
        </Heading>
        {props.count !== undefined && (
          <span className="shrink-0 font-mono text-2xs text-subtle tabular">{props.count}</span>
        )}
        {(props.actions || props.onOpen) && (
          <div className="flex shrink-0 items-center">
            {props.actions}
            {props.onOpen && (
              <IconButton icon={Icons.focus} label={`Open ${props.title}`} onPress={props.onOpen} />
            )}
          </div>
        )}
      </div>
      <DisclosurePanel>{props.children}</DisclosurePanel>
    </Disclosure>
  );
}
