import { Button } from "react-aria-components";
import { cx } from "../cx";
import { participantBg, participantText, participantToneOf } from "../internal";
import { Tooltip } from "../tooltip";

/** Colour swatch, `P{index}` and the label. Beyond the fifth participant the swatch is one shared grey and the index carries the identity. */
export function ParticipantChip(props: {
  index: number;
  label: string;
  you?: boolean;
  title?: string;
}) {
  const className = cx(
    "inline-flex h-4.5 items-center gap-1.5 rounded-xs border px-1.5 font-mono text-xs whitespace-nowrap outline-none focus-visible:outline-1 focus-visible:outline-offset-1 focus-visible:outline-accent",
    props.you ? "border-accent" : "border-line",
  );
  const content = (
    <>
      <span
        className={cx("size-2 shrink-0 rounded-xs", participantBg[participantToneOf(props.index)])}
      />
      <span className="text-muted">P{props.index}</span>
      <span className="min-w-0 truncate text-fg">{props.label}</span>
      {props.you && <span className="text-accent">you</span>}
    </>
  );
  if (!props.title) return <span className={className}>{content}</span>;
  // A real button rather than `Focusable`, which warns inside virtualized rows.
  return (
    <Tooltip title={props.title}>
      <Button className={cx(className, "cursor-default")}>{content}</Button>
    </Tooltip>
  );
}

const stateWord = { active: "active", done: "done", bad: "failed", idle: "idle" } as const;

/**
 * A participant's colour as an 8 px dot. Active is solid, done is faded, idle
 * is hollow; a failed participant gets a `bad` ring and a seat you hold an
 * accent ring.
 */
export function ParticipantDot(props: {
  index: number;
  state?: "active" | "done" | "bad" | "idle";
  you?: boolean;
}) {
  const state = props.state ?? "active";
  const tone = participantToneOf(props.index);
  return (
    <span
      role="img"
      aria-label={`P${props.index} ${stateWord[state]}${props.you ? ", you" : ""}`}
      className={cx(
        "inline-block size-2 shrink-0 rounded-full",
        state === "idle" ? "border border-current bg-transparent" : participantBg[tone],
        state === "idle" && participantText[tone],
        state === "done" && "opacity-50",
        state === "bad" && "ring-1 ring-bad ring-offset-1 ring-offset-editor",
        props.you && "outline-1 outline-offset-2 outline-accent",
      )}
    />
  );
}
