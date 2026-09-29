import { type PointerEvent, useState } from "react";
import { useElementSize } from "../size";

export interface TimeBucket {
  t: number;
  up: number;
  down: number;
  bad?: number;
}

const AXIS_HEIGHT = 14;
const TICKS = 5;
const CLICK_SLOP_PX = 3;

function formatClock(t: number, spanMs: number): string {
  return new Intl.DateTimeFormat(undefined, {
    hour: "2-digit",
    minute: "2-digit",
    second: spanMs < 10 * 60_000 ? "2-digit" : undefined,
    hourCycle: "h23",
  }).format(t);
}

/**
 * Mirrored bars around a centre line: `up` above (with its `bad` part in the
 * failure colour) and `down` below, each half scaled to its own maximum since
 * the two series have different units. Drag to select a time range, click to
 * clear it. The chart fills its container's width.
 */
export function TimelineChart(props: {
  buckets: TimeBucket[];
  from: number;
  to: number;
  height: number;
  brush: { from: number; to: number } | null;
  onBrush: (range: { from: number; to: number } | null) => void;
  upLabel: string;
  downLabel: string;
}) {
  const { buckets, from, to, height } = props;
  const [ref, { width }] = useElementSize<SVGSVGElement>();
  // Pixel positions of an in-progress drag, relative to the chart's left edge.
  const [drag, setDrag] = useState<{ anchor: number; current: number } | null>(null);

  const spanMs = to - from;
  const plotHeight = height - AXIS_HEIGHT;
  const mid = Math.round(plotHeight / 2);
  const x = (t: number) => (spanMs > 0 ? ((t - from) / spanMs) * width : 0);
  const timeAt = (px: number) => from + (Math.max(0, Math.min(width, px)) / width) * spanMs;

  const times = buckets.map((bucket) => bucket.t).sort((a, b) => a - b);
  let bucketMs = spanMs / Math.max(1, buckets.length);
  for (let i = 1; i < times.length; i++) {
    const gap = (times[i] ?? 0) - (times[i - 1] ?? 0);
    if (gap > 0) bucketMs = Math.min(bucketMs, gap);
  }
  const barWidth = Math.max(1, x(from + bucketMs) - 1);
  const maxUp = Math.max(1, ...buckets.map((bucket) => bucket.up));
  const maxDown = Math.max(1, ...buckets.map((bucket) => bucket.down));
  const half = mid - 2;

  const shown = drag
    ? {
        from: timeAt(Math.min(drag.anchor, drag.current)),
        to: timeAt(Math.max(drag.anchor, drag.current)),
      }
    : props.brush;

  const localX = (event: PointerEvent<SVGSVGElement>) =>
    event.clientX - event.currentTarget.getBoundingClientRect().left;

  function onPointerDown(event: PointerEvent<SVGSVGElement>) {
    event.currentTarget.setPointerCapture(event.pointerId);
    const px = localX(event);
    setDrag({ anchor: px, current: px });
  }
  function onPointerMove(event: PointerEvent<SVGSVGElement>) {
    if (drag) setDrag({ anchor: drag.anchor, current: localX(event) });
  }
  function onPointerUp(event: PointerEvent<SVGSVGElement>) {
    if (!drag) return;
    const end = localX(event);
    setDrag(null);
    if (Math.abs(end - drag.anchor) < CLICK_SLOP_PX) props.onBrush(null);
    else
      props.onBrush({
        from: timeAt(Math.min(drag.anchor, end)),
        to: timeAt(Math.max(drag.anchor, end)),
      });
  }

  return (
    <svg
      ref={ref}
      role="img"
      aria-label={`${props.upLabel} above the line, ${props.downLabel} below. Drag to select a time range, click to clear.`}
      width="100%"
      height={height}
      className="block cursor-crosshair touch-none select-none"
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={() => setDrag(null)}
    >
      <line
        x1={0}
        x2={width}
        y1={mid + 0.5}
        y2={mid + 0.5}
        strokeWidth={1}
        className="stroke-line-soft"
      />
      {buckets.map((bucket) => {
        const left = x(bucket.t);
        const upHeight = (bucket.up / maxUp) * half;
        const badHeight = (Math.min(bucket.bad ?? 0, bucket.up) / maxUp) * half;
        const downHeight = (bucket.down / maxDown) * half;
        return (
          <g key={bucket.t}>
            <rect
              x={left}
              y={mid - upHeight}
              width={barWidth}
              height={upHeight - badHeight}
              className="fill-muted"
            />
            <rect
              x={left}
              y={mid - badHeight}
              width={barWidth}
              height={badHeight}
              className="fill-bad"
            />
            <rect x={left} y={mid + 1} width={barWidth} height={downHeight} className="fill-warn" />
          </g>
        );
      })}
      {shown && (
        <g>
          <rect
            x={x(shown.from)}
            y={0}
            width={Math.max(1, x(shown.to) - x(shown.from))}
            height={plotHeight}
            className="fill-selected"
          />
          <rect x={x(shown.from)} y={0} width={1} height={plotHeight} className="fill-accent" />
          <rect x={x(shown.to)} y={0} width={1} height={plotHeight} className="fill-accent" />
        </g>
      )}
      <text
        x={4}
        y={10}
        className="fill-subtle stroke-editor text-2xs [paint-order:stroke]"
        strokeWidth={3}
      >
        {props.upLabel} ↑
      </text>
      <text
        x={4}
        y={plotHeight - 4}
        className="fill-subtle stroke-editor text-2xs [paint-order:stroke]"
        strokeWidth={3}
      >
        {props.downLabel} ↓
      </text>
      {Array.from({ length: TICKS }, (_, i) => {
        const fraction = i / (TICKS - 1);
        return (
          <text
            key={fraction}
            x={fraction * width}
            y={height - 3}
            textAnchor={i === 0 ? "start" : i === TICKS - 1 ? "end" : "middle"}
            className="fill-subtle font-mono text-2xs"
          >
            {formatClock(from + fraction * spanMs, spanMs)}
          </text>
        );
      })}
    </svg>
  );
}
