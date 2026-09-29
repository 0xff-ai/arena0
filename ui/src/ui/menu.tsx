import type { LucideIcon } from "lucide-react";
import type { ReactNode } from "react";
import {
  Header,
  Menu,
  MenuTrigger,
  type Placement,
  MenuItem as RACMenuItem,
  MenuSection as RACMenuSection,
  Separator,
} from "react-aria-components";
import { Button, type ButtonProps } from "./button";
import { cx } from "./cx";
import { Icons } from "./icons";
import { Icon } from "./internal";
import { Kbd } from "./kbd";
import { Popover } from "./popover";

/** A button that opens a menu of `MenuItem`s, `MenuSection`s and `MenuSeparator`s. */
export function MenuButton(props: {
  label: string;
  icon?: LucideIcon;
  variant?: ButtonProps["variant"];
  children: ReactNode;
  placement?: Placement;
}) {
  return (
    <MenuTrigger>
      <Button variant={props.variant} icon={props.icon}>
        {props.label}
        <Icon icon={Icons.chevronDown} size={12} className="text-subtle" />
      </Button>
      <Popover placement={props.placement}>
        <Menu className="max-h-[inherit] min-w-44 overflow-auto p-1 outline-none">
          {props.children}
        </Menu>
      </Popover>
    </MenuTrigger>
  );
}

export function MenuItem(props: {
  id?: string;
  icon?: LucideIcon;
  kbd?: string;
  danger?: boolean;
  onAction?: () => void;
  children: ReactNode;
}) {
  return (
    <RACMenuItem
      id={props.id}
      onAction={props.onAction}
      textValue={typeof props.children === "string" ? props.children : undefined}
      className={cx(
        "flex h-6 cursor-default items-center gap-2 rounded-sm px-2 text-base outline-none focused:bg-selected disabled:opacity-50",
        props.danger ? "text-bad" : "text-fg",
      )}
    >
      {props.icon && <Icon icon={props.icon} className={props.danger ? "" : "text-subtle"} />}
      <span className="min-w-0 flex-1 truncate">{props.children}</span>
      {props.kbd && <Kbd keys={props.kbd} />}
    </RACMenuItem>
  );
}

export function MenuSection(props: { title?: string; children: ReactNode }) {
  return (
    <RACMenuSection
      aria-label={props.title ?? "Actions"}
      className="not-first:mt-1 not-first:border-t not-first:border-line-soft not-first:pt-1"
    >
      {props.title && (
        <Header className="px-2 pt-1 pb-0.5 text-xs uppercase tracking-wide text-subtle">
          {props.title}
        </Header>
      )}
      {props.children}
    </RACMenuSection>
  );
}

export function MenuSeparator() {
  return <Separator className="mx-1 my-1 h-px bg-line-soft" />;
}
