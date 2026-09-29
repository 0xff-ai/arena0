import { type RefObject, useLayoutEffect, useRef, useState } from "react";

/**
 * An element's content box in whole px, kept current by a `ResizeObserver`.
 * Both sides are 0 until the element mounts. Measured before paint, so a
 * layout derived from the size never flashes at 0.
 */
export function useElementSize<T extends Element>(): [
  RefObject<T | null>,
  { width: number; height: number },
] {
  const ref = useRef<T | null>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });
  useLayoutEffect(() => {
    const element = ref.current;
    if (element === null) return;
    const set = (width: number, height: number) =>
      setSize((last) => {
        const next = { width: Math.floor(width), height: Math.floor(height) };
        return next.width === last.width && next.height === last.height ? last : next;
      });
    const box = element.getBoundingClientRect();
    set(box.width, box.height);
    const observer = new ResizeObserver(([entry]) => {
      if (entry !== undefined) set(entry.contentRect.width, entry.contentRect.height);
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);
  return [ref, size];
}
