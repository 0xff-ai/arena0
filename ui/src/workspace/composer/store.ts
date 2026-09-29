import { useSyncExternalStore } from "react";

// The one callout the composer dock is answering. In memory only: a reload
// drops it because the callout may have been answered elsewhere meanwhile.
let calloutKey: string | null = null;
const listeners = new Set<() => void>();

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function set(key: string | null) {
  if (key === calloutKey) return;
  calloutKey = key;
  for (const listener of listeners) listener();
}

const api = { open: (key: string) => set(key), close: () => set(null) };

export function useComposer(): {
  calloutKey: string | null;
  open(key: string): void;
  close(): void;
} {
  const key = useSyncExternalStore(subscribe, () => calloutKey);
  return { calloutKey: key, ...api };
}
