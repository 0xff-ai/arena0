import type { SessionState } from "~/model";
import { Dot, type Tone } from "~/ui";

/** Colour of a session state's dot; neutral states are the ones still settling. */
export const STATE_TONE: Record<SessionState, Tone> = {
  active: "ok",
  completed: "done",
  failed: "bad",
  aborted: "bad",
  negotiating: "neutral",
  activating: "neutral",
  ending: "neutral",
};

/** A dot and the state's word; the word carries the meaning, the colour only helps scanning. */
export function StateWord(props: { state: SessionState }) {
  const quiet = STATE_TONE[props.state] === "neutral";
  return (
    <span
      className={`inline-flex items-center gap-1.5 text-sm ${quiet ? "text-subtle" : "text-fg"}`}
    >
      <Dot tone={STATE_TONE[props.state]} />
      {props.state}
    </span>
  );
}
