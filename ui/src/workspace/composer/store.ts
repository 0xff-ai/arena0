import { useSyncExternalStore } from "react";
import type { JsonLike } from "~/ui";
import { store } from "../shell";

// The one callout the composer dock is answering. In memory only: a reload
// drops it because the callout may have been answered elsewhere meanwhile.
const callout = store<string | null>(null);

const api = { open: (key: string) => callout.set(key), close: () => callout.set(null) };

export function useComposer(): {
  calloutKey: string | null;
  open(key: string): void;
  close(): void;
} {
  return { calloutKey: useSyncExternalStore(callout.subscribe, callout.get), ...api };
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
