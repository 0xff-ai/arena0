import { useSyncExternalStore } from "react";

interface Ticker {
  now: number;
  timer: ReturnType<typeof setInterval> | null;
  listeners: Set<() => void>;
  subscribe(listener: () => void): () => void;
  snapshot(): number;
}

const tickers = new Map<number, Ticker>();

// One interval per period, running only while someone listens. `now` is kept
// between ticks so every reader of a period sees the same value in a render
// pass, and refreshed when the first listener arrives after an idle spell.
function tickerFor(periodMs: number): Ticker {
  let ticker = tickers.get(periodMs);
  if (ticker === undefined) {
    const created: Ticker = {
      now: Date.now(),
      timer: null,
      listeners: new Set(),
      subscribe(listener) {
        created.listeners.add(listener);
        if (created.timer === null) {
          created.now = Date.now();
          created.timer = setInterval(() => {
            created.now = Date.now();
            for (const l of created.listeners) l();
          }, periodMs);
        }
        return () => {
          created.listeners.delete(listener);
          if (created.listeners.size === 0 && created.timer !== null) {
            clearInterval(created.timer);
            created.timer = null;
          }
        };
      },
      snapshot: () => created.now,
    };
    tickers.set(periodMs, created);
    ticker = created;
  }
  return ticker;
}

/** Unix ms, refreshed every `periodMs`. Components with the same period share one interval. */
export function useNow(periodMs = 1000): number {
  const ticker = tickerFor(periodMs);
  return useSyncExternalStore(ticker.subscribe, ticker.snapshot);
}
