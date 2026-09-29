/** A line through `values`, scaled to its own min and max. Inherits the text colour. */
export function Sparkline(props: { values: number[]; width: number; height: number }) {
  const { values, width, height } = props;
  const pad = 1;
  const min = Math.min(...values);
  const span = Math.max(...values) - min || 1;
  const step = values.length > 1 ? (width - 2 * pad) / (values.length - 1) : 0;
  const points = values.map((value, i) => {
    const x = pad + i * step;
    const y = height - pad - ((value - min) / span) * (height - 2 * pad);
    return `${x.toFixed(1)},${y.toFixed(1)}`;
  });
  return (
    <svg
      aria-hidden
      width={width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
      className="shrink-0 overflow-visible"
    >
      <polyline
        points={points.join(" ")}
        fill="none"
        strokeWidth={1}
        strokeLinejoin="round"
        className="stroke-current"
      />
    </svg>
  );
}
