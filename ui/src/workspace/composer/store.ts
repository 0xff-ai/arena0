import { useSyncExternalStore } from "react";
import type { JsonLike } from "~/ui";

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

// Unsent answers, per callout key. In memory for the same reason as the open
// callout: after a reload the callout may have been answered elsewhere. Read
// once when a composer opens and written on every edit, so no subscription.
const drafts = new Map<string, JsonLike | undefined>();

export const draftStore = {
  get: (key: string): JsonLike | undefined => drafts.get(key),
  set: (key: string, value: JsonLike | undefined) => void drafts.set(key, value),
  clear: (key: string) => void drafts.delete(key),
};
