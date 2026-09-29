export interface StripSpan {
  from: number;
  to: number;
  kind: "negotiating" | "active" | "waiting" | "waiting-you" | "waiting-long" | "ending";
}
export interface StripMark {
  at: number;
  kind: "step" | "end-ok" | "end-bad" | "gap";
}

interface SpanStyle {
  className: string;
  width: number;
  dash?: string;
  round?: boolean;
  /** Waits overlay an active session, so they run on a second line under it. */
  lane: "main" | "wait";
}

const SPAN_STYLE: Record<StripSpan["kind"], SpanStyle> = {
  negotiating: { className: "stroke-subtle", width: 1.25, dash: "2 2.5", lane: "main" },
  active: { className: "stroke-ok", width: 2, lane: "main" },
  waiting: { className: "stroke-warn", width: 1.25, dash: "2 2.5", lane: "wait" },
  "waiting-you": { className: "stroke-accent", width: 1.25, dash: "2 2.5", lane: "wait" },
  "waiting-long": { className: "stroke-warn", width: 2, lane: "wait" },
  ending: { className: "stroke-subtle", width: 1.5, dash: "0.1 3", round: true, lane: "main" },
};

/**
 * One session's life on a shared time axis. Certified steps are ticks above
 * the line so the line itself stays solid. `[from, to]` maps to `[0, width]`;
 * anything outside is clamped to the edge.
 */
export function LifecycleStrip(props: {
  from: number;
  to: number;
  width: number;
  spans: StripSpan[];
  marks: StripMark[];
  height?: number;
}) {
  const { from, to, width } = props;
  const height = props.height ?? 20;
  const y = Math.round(height / 2) + 1;
  const waitY = y + 5;
  const x = (t: number) =>
    to > from ? Math.max(0, Math.min(width, ((t - from) / (to - from)) * width)) : 0;

  const ticks = props.marks
    .filter((mark) => mark.kind === "step")
    .map((mark) => `M${Math.round(x(mark.at)) + 0.5} ${y - 7}V${y - 3.5}`)
    .join("");

  return (
    <svg
      aria-hidden
      width={width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
      className="shrink-0"
    >
      {props.spans.map((span, i) => {
        const style = SPAN_STYLE[span.kind];
        const at = style.lane === "main" ? y : waitY;
        const start = x(span.from);
        const end = Math.max(x(span.to), start + 1);
        return (
          <path
            key={`${span.kind}-${i}`}
            d={`M${start} ${at}H${end}`}
            fill="none"
            strokeWidth={style.width}
            strokeDasharray={style.dash}
            strokeLinecap={style.round ? "round" : "butt"}
            className={style.className}
          />
        );
      })}
      {ticks && <path d={ticks} strokeWidth={1} fill="none" className="stroke-faint" />}
      {props.marks.map((mark, i) => {
        const at = x(mark.at);
        switch (mark.kind) {
          case "step":
            return null;
          case "end-ok":
            return (
              <path
                key={`${mark.kind}-${i}`}
                d={`M${at} ${y - 4.5}l4.5 4.5-4.5 4.5-4.5-4.5z`}
                strokeWidth={1.5}
                className="fill-done stroke-editor"
              />
            );
          case "end-bad":
            return (
              <path
                key={`${mark.kind}-${i}`}
                d={`M${at - 3.5} ${y - 3.5}l7 7M${at + 3.5} ${y - 3.5}l-7 7`}
                strokeWidth={1.75}
                fill="none"
                className="stroke-bad"
              />
            );
          case "gap":
            return (
              <path
                key={`${mark.kind}-${i}`}
                d={`M${at} ${y - 4.5}L${at + 3.5} ${y + 2}H${at - 3.5}z`}
                strokeWidth={1}
                className="fill-warn stroke-editor"
              />
            );
        }
      })}
    </svg>
  );
}
