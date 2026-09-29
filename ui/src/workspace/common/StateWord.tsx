import type { SessionState } from "~/model";
import { Dot, type Tone } from "~/ui";

const tone: Record<SessionState, Tone> = {
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
  const quiet = tone[props.state] === "neutral";
  return (
    <span
      className={`inline-flex items-center gap-1.5 text-sm ${quiet ? "text-subtle" : "text-fg"}`}
    >
      <Dot tone={tone[props.state]} />
      {props.state}
    </span>
  );
}
