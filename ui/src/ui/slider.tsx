import { Slider as RACSlider, SliderThumb, SliderTrack } from "react-aria-components";
import { cx } from "./cx";

/**
 * An integer slider from `min` to `max` with one tick per value while there
 * are at most 100 of them (beyond that ticks turn into a smear).
 */
export function Slider(props: {
  label: string;
  min?: number;
  max: number;
  value: number;
  onChange: (value: number) => void;
  isDisabled?: boolean;
  className?: string;
}) {
  const min = props.min ?? 0;
  const span = Math.max(props.max - min, 1);
  const ticks = props.max - min <= 100 ? props.max - min + 1 : 0;
  return (
    <RACSlider
      aria-label={props.label}
      minValue={min}
      maxValue={Math.max(props.max, min + 1)}
      isDisabled={props.isDisabled}
      value={props.value}
      onChange={props.onChange}
      className={cx("flex items-center gap-2 disabled:opacity-50", props.className)}
    >
      <SliderTrack className="relative h-5 w-full">
        {({ state }) => (
          <>
            <div className="absolute inset-x-0 top-1/2 h-px -translate-y-1/2 bg-line" />
            {Array.from({ length: ticks }, (_, i) => (
              <div
                key={i}
                aria-hidden
                className="absolute top-1/2 h-1.5 w-px -translate-y-1/2 bg-faint"
                style={{ left: `${(i / span) * 100}%` }}
              />
            ))}
            <SliderThumb
              aria-label={props.label}
              className="top-1/2 h-3.5 w-2 rounded-xs bg-accent outline-none focus-visible:outline-1 focus-visible:outline-offset-1 focus-visible:outline-accent"
            >
              <span className="sr-only">{state.getThumbValueLabel(0)}</span>
            </SliderThumb>
          </>
        )}
      </SliderTrack>
    </RACSlider>
  );
}
