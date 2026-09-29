const BARS = 8;
const BAR_WIDTH = 2;
const GAP = 1;
const WIDTH = BARS * BAR_WIDTH + (BARS - 1) * GAP;
const HEIGHT = { sm: 10, md: 14 } as const;

/**
 * Eight monochrome bars from the first eight hex digits, bar height (n+1)/16
 * of the box. Equal hashes have equal shapes, so two hashes can be compared
 * at a glance without reading them. Inherits the text colour.
 */
export function Fingerprint(props: { hash: string; size?: "sm" | "md" }) {
  const height = HEIGHT[props.size ?? "md"];
  const bars = Array.from({ length: BARS }, (_, i) => {
    const nibble = Number.parseInt(props.hash.charAt(i), 16) || 0;
    return ((nibble + 1) / 16) * height;
  });
  return (
    <svg
      role="img"
      aria-label={props.hash.slice(0, BARS)}
      width={WIDTH}
      height={height}
      viewBox={`0 0 ${WIDTH} ${height}`}
      shapeRendering="crispEdges"
      className="shrink-0 fill-current"
    >
      {bars.map((barHeight, i) => (
        // Positional and never reordered.
        <rect
          key={i}
          x={i * (BAR_WIDTH + GAP)}
          y={height - barHeight}
          width={BAR_WIDTH}
          height={barHeight}
        />
      ))}
    </svg>
  );
}
