import { useSyncExternalStore } from "react";
import { store } from "./shell";

export type Selection =
  | { kind: "session"; key: string }
  | { kind: "step"; sessionKey: string; step: number; host: string | null }
  | { kind: "program"; hash: string }
  | { kind: "host"; id: string }
  | { kind: "receipt"; host: string; id: string }
  | null;

// What the inspector shows. It lives in memory only: a reload starts with
// nothing selected, and the selected row may not exist any more.
const selection = store<Selection>(null);

export function useSelection(): [Selection, (s: Selection) => void] {
  return [useSyncExternalStore(selection.subscribe, selection.get), selection.set];
}
