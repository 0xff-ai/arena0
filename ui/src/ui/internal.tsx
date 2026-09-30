// Helpers shared by several components. index.ts re-exports only the
// participant colours: the public surface is exactly the contract, the rest of
// this file is the library's own glue.
import type { LucideIcon } from "lucide-react";
import type { Tone } from "./badge";
import { cx } from "./cx";
import type { ParticipantToneName } from "./status";

/** Text colour per tone. Neutral is the quiet default, not a signal. */
export const toneText: Record<Tone, string> = {
  neutral: "text-muted",
  accent: "text-accent",
  ok: "text-ok",
  done: "text-done",
  bad: "text-bad",
  warn: "text-warn",
  info: "text-info",
};

export const toneBg: Record<Tone, string> = {
  neutral: "bg-subtle",
  accent: "bg-accent",
  ok: "bg-ok",
  done: "bg-done",
  bad: "bg-bad",
  warn: "bg-warn",
  info: "bg-info",
};

export const participantBg: Record<ParticipantToneName, string> = {
  p0: "bg-p0",
  p1: "bg-p1",
  p2: "bg-p2",
  p3: "bg-p3",
  p4: "bg-p4",
  pq: "bg-pq",
};

export const participantText: Record<ParticipantToneName, string> = {
  p0: "text-p0",
  p1: "text-p1",
  p2: "text-p2",
  p3: "text-p3",
  p4: "text-p4",
  pq: "text-pq",
};

/** Participant index to its colour: five distinct tones, then one shared "many" tone. */
export function participantToneOf(index: number): ParticipantToneName {
  return (["p0", "p1", "p2", "p3", "p4"] as const)[index] ?? "pq";
}

export function isParticipantTone(tone: Tone | ParticipantToneName): tone is ParticipantToneName {
  return tone === "pq" || /^p[0-4]$/.test(tone);
}

/** Editor-weight glyph: 14px with a thin stroke reads as text, not as a control. */
export function Icon(props: {
  icon: LucideIcon;
  size?: number;
  strokeWidth?: number;
  className?: string;
}) {
  const Glyph = props.icon;
  return (
    <Glyph
      aria-hidden
      size={props.size ?? 14}
      strokeWidth={props.strokeWidth ?? 1.5}
      className={cx("shrink-0", props.className)}
    />
  );
}

/** Evaluated once; the platform does not change during a page's life. */
export const isMac: boolean = /mac|iphone|ipad|ipod/i.test(
  (navigator as Navigator & { userAgentData?: { platform?: string } }).userAgentData?.platform ??
    navigator.platform,
);

/** Overlays fade in and out; RAC waits for the exit transition before unmounting. */
export const fade = "transition-opacity duration-120 entering:opacity-0 exiting:opacity-0";

/**
 * The dimmed layer behind modal overlays, top-anchored at 12vh. It washes the
 * page toward the editor colour: there is no black palette entry, and this reads the
 * same in both themes.
 */
export const scrim = `fixed inset-0 z-40 flex items-start justify-center bg-editor/60 pt-[12vh] ${fade}`;
