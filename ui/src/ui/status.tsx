import type { LucideIcon } from "lucide-react";
import type { ReactNode } from "react";
import type { Tone } from "./badge";
import { cx, tv } from "./cx";
import { Icons } from "./icons";
import { Icon, isParticipantTone, participantBg, toneBg } from "./internal";

export type ParticipantToneName = "p0" | "p1" | "p2" | "p3" | "p4" | "pq";

const count = tv({
  base: "inline-flex min-w-4 shrink-0 items-center justify-center font-mono text-2xs leading-4 tabular",
  variants: {
    tone: {
      neutral: "text-subtle",
      accent: "rounded-xs bg-accent px-1 text-editor",
      ok: "rounded-xs bg-ok px-1 text-editor",
      done: "rounded-xs bg-done px-1 text-editor",
      bad: "rounded-xs bg-bad px-1 text-editor",
      warn: "rounded-xs bg-warn px-1 text-editor",
      info: "rounded-xs bg-info px-1 text-editor",
    },
  },
  defaultVariants: { tone: "neutral" },
});

/** A number beside a label. Neutral is plain text; any other tone is a filled square for "look here". */
export function Count(props: { value: number; tone?: Tone }) {
  return <span className={count({ tone: props.tone })}>{props.value}</span>;
}

const ringClass: Record<Tone, string> = {
  neutral: "outline-1 outline-offset-1 outline-subtle",
  accent: "outline-1 outline-offset-1 outline-accent",
  ok: "outline-1 outline-offset-1 outline-ok",
  done: "outline-1 outline-offset-1 outline-done",
  bad: "outline-1 outline-offset-1 outline-bad",
  warn: "outline-1 outline-offset-1 outline-warn",
  info: "outline-1 outline-offset-1 outline-info",
};

/** A status or participant dot. `ring` outlines it in a second tone; `pulse` marks live activity. */
export function Dot(props: {
  tone: Tone | ParticipantToneName;
  ring?: Tone;
  pulse?: boolean;
  size?: 6 | 8;
}) {
  const bg = isParticipantTone(props.tone) ? participantBg[props.tone] : toneBg[props.tone];
  return (
    <span
      aria-hidden
      className={cx(
        "inline-block shrink-0 rounded-full",
        props.size === 8 ? "size-2" : "size-1.5",
        bg,
        props.ring && ringClass[props.ring],
        props.pulse && "animate-pulse motion-reduce:animate-none",
      )}
    />
  );
}

const hint = tv({
  base: "inline-flex min-w-0 max-w-full items-center gap-1 text-xs",
  variants: {
    tone: {
      you: "text-accent",
      warn: "text-warn",
      error: "text-bad",
      info: "text-info",
      quiet: "text-subtle",
    },
  },
});

const hintIcon: Record<"you" | "warn" | "error" | "info" | "quiet", LucideIcon | null> = {
  you: Icons.you,
  warn: Icons.warn,
  error: Icons.error,
  info: Icons.info,
  quiet: null,
};

/** An icon and words in the tone's colour: emphasis without a box. */
export function Hint(props: {
  tone: "you" | "warn" | "error" | "info" | "quiet";
  icon?: LucideIcon;
  children: ReactNode;
}) {
  const glyph = props.icon ?? hintIcon[props.tone];
  return (
    <span className={hint({ tone: props.tone })}>
      {glyph && <Icon icon={glyph} size={12} />}
      <span className="min-w-0 truncate">{props.children}</span>
    </span>
  );
}
