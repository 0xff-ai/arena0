import { useSyncExternalStore } from "react";

export type Selection =
  | { kind: "session"; key: string }
  | { kind: "step"; sessionKey: string; step: number; host: string | null }
  | { kind: "program"; hash: string }
  | { kind: "host"; id: string }
  | { kind: "receipt"; host: string; id: string }
  | null;

// What the inspector shows. It lives in memory only: a reload starts with
// nothing selected, and the selected row may not exist any more.
let current: Selection = null;
const listeners = new Set<() => void>();

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

function set(selection: Selection) {
  current = selection;
  for (const listener of listeners) listener();
}

export function useSelection(): [Selection, (s: Selection) => void] {
  return [useSyncExternalStore(subscribe, () => current), set];
}
