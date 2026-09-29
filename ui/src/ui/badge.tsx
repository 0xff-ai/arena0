import type { LucideIcon } from "lucide-react";
import type { ReactNode } from "react";
import { tv } from "./cx";
import { Icon } from "./internal";

export type Tone = "neutral" | "accent" | "ok" | "done" | "bad" | "warn" | "info";

// Every class is spelled out: Tailwind only generates utilities it can read.
const badge = tv({
  base: "inline-flex h-4 shrink-0 items-center gap-1 whitespace-nowrap rounded-xs border px-1.5 text-xs leading-none",
  variants: {
    tone: {
      neutral: "",
      accent: "",
      ok: "",
      done: "",
      bad: "",
      warn: "",
      info: "",
    },
    variant: {
      soft: "border-transparent",
      outline: "bg-transparent",
    },
  },
  compoundVariants: [
    { variant: "soft", tone: "neutral", class: "bg-hover text-muted" },
    { variant: "soft", tone: "accent", class: "bg-accent/12 text-accent" },
    { variant: "soft", tone: "ok", class: "bg-ok/12 text-ok" },
    { variant: "soft", tone: "done", class: "bg-done/12 text-done" },
    { variant: "soft", tone: "bad", class: "bg-bad/12 text-bad" },
    { variant: "soft", tone: "warn", class: "bg-warn/12 text-warn" },
    { variant: "soft", tone: "info", class: "bg-info/12 text-info" },
    { variant: "outline", tone: "neutral", class: "border-line text-muted" },
    { variant: "outline", tone: "accent", class: "border-accent/60 text-accent" },
    { variant: "outline", tone: "ok", class: "border-ok/60 text-ok" },
    { variant: "outline", tone: "done", class: "border-done/60 text-done" },
    { variant: "outline", tone: "bad", class: "border-bad/60 text-bad" },
    { variant: "outline", tone: "warn", class: "border-warn/60 text-warn" },
    { variant: "outline", tone: "info", class: "border-info/60 text-info" },
  ],
  defaultVariants: { tone: "neutral", variant: "soft" },
});

/** A nearly square label for a state or category. Carry meaning in the words and icon, not the colour. */
export function Badge(props: {
  tone?: Tone;
  variant?: "soft" | "outline";
  icon?: LucideIcon;
  children: ReactNode;
}) {
  return (
    <span className={badge({ tone: props.tone, variant: props.variant })}>
      {props.icon && <Icon icon={props.icon} size={11} />}
      {props.children}
    </span>
  );
}
