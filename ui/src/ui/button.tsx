import type { LucideIcon } from "lucide-react";
import {
  composeRenderProps,
  Button as RACButton,
  type ButtonProps as RACButtonProps,
  ToggleButton,
  ToggleButtonGroup,
} from "react-aria-components";
import { cx, tv } from "./cx";
import { Icon } from "./internal";
import { Kbd } from "./kbd";
import { Count } from "./status";
import { Tooltip } from "./tooltip";

const button = tv({
  base: "inline-flex shrink-0 cursor-default items-center justify-center gap-1.5 whitespace-nowrap rounded-sm border font-sans outline-none transition-colors duration-120 focus-visible:outline-1 focus-visible:outline-offset-1 focus-visible:outline-accent disabled:opacity-50",
  variants: {
    variant: {
      default:
        "border-line bg-surface text-fg hovered:border-faint hovered:bg-active pressed:bg-editor",
      primary: "border-accent bg-accent text-editor hovered:brightness-110 pressed:brightness-90",
      ghost:
        "border-transparent bg-transparent text-muted hovered:bg-hover hovered:text-fg pressed:bg-active pressed:text-fg",
      danger:
        "border-bad bg-transparent text-bad hovered:bg-bad hovered:text-editor pressed:brightness-90",
    },
    size: {
      sm: "h-6 px-2 text-sm",
      md: "h-7 px-2.5 text-base",
    },
  },
  defaultVariants: { variant: "default", size: "md" },
});

const iconButton = tv({
  base: "inline-flex shrink-0 cursor-default items-center justify-center rounded-sm border border-transparent bg-transparent text-muted outline-none transition-colors duration-120 hovered:bg-hover hovered:text-fg pressed:bg-active pressed:text-fg focus-visible:outline-1 focus-visible:outline-offset-1 focus-visible:outline-accent disabled:opacity-50",
  variants: {
    size: {
      sm: "size-6",
      md: "size-7",
    },
  },
  defaultVariants: { size: "sm" },
});

export interface ButtonProps extends RACButtonProps {
  variant?: "default" | "primary" | "ghost" | "danger";
  size?: "sm" | "md";
  icon?: LucideIcon;
  kbd?: string;
}

export function Button({ variant, size, icon, kbd, ...props }: ButtonProps) {
  return (
    <RACButton
      {...props}
      className={composeRenderProps(props.className, (className) =>
        cx(button({ variant, size }), className),
      )}
    >
      {composeRenderProps(props.children, (children) => (
        <>
          {icon && <Icon icon={icon} size={size === "sm" ? 13 : 14} />}
          {children}
          {kbd && <Kbd keys={kbd} />}
        </>
      ))}
    </RACButton>
  );
}

export interface IconButtonProps extends Omit<RACButtonProps, "children"> {
  icon: LucideIcon;
  label: string;
  kbd?: string;
  size?: "sm" | "md";
}

/** An icon-only button. It always carries a tooltip, so the icon is never the only name. */
export function IconButton({ icon, label, kbd, size, ...props }: IconButtonProps) {
  return (
    <Tooltip title={label} kbd={kbd}>
      <RACButton
        {...props}
        aria-label={label}
        className={composeRenderProps(props.className, (className) =>
          cx(iconButton({ size }), className),
        )}
      >
        <Icon icon={icon} size={size === "md" ? 15 : 14} />
      </RACButton>
    </Tooltip>
  );
}

export interface SegmentedItem<K extends string> {
  id: K;
  label: string;
  icon?: LucideIcon;
  count?: number;
}

const segmentedItem = tv({
  base: "inline-flex cursor-default items-center gap-1 whitespace-nowrap rounded-xs px-2 font-sans text-muted outline-none transition-colors duration-120 hovered:bg-hover hovered:text-fg selected:bg-selected selected:text-fg focus-visible:outline-1 focus-visible:-outline-offset-1 focus-visible:outline-accent",
  variants: {
    size: {
      sm: "h-5 text-sm",
      md: "h-6 text-base",
    },
  },
});

/** A single-choice switch between a few views; one item is always selected. */
export function Segmented<K extends string>(props: {
  label: string;
  items: SegmentedItem<K>[];
  value: K;
  onChange: (id: K) => void;
  size?: "sm" | "md";
}) {
  const size = props.size ?? "sm";
  return (
    <ToggleButtonGroup
      aria-label={props.label}
      selectionMode="single"
      disallowEmptySelection
      selectedKeys={[props.value]}
      onSelectionChange={(keys) => {
        const item = props.items.find((candidate) => keys.has(candidate.id));
        if (item) props.onChange(item.id);
      }}
      className="inline-flex shrink-0 gap-px rounded-sm border border-line-soft bg-hover p-px"
    >
      {props.items.map((item) => (
        <ToggleButton key={item.id} id={item.id} className={segmentedItem({ size })}>
          {item.icon && <Icon icon={item.icon} size={size === "sm" ? 12 : 13} />}
          {item.label}
          {item.count !== undefined && <Count value={item.count} />}
        </ToggleButton>
      ))}
    </ToggleButtonGroup>
  );
}
