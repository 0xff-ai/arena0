import { createRef, useSyncExternalStore } from "react";
import type { PanelImperativeHandle } from "react-resizable-panels";
import { type Flag, type Session, useFlags, useSessions } from "~/model";

// Small in-memory stores that the shell's parts share without props: the
// title bar, palette, status bar and global keys are siblings.

function store<T>(initial: T) {
  let value = initial;
  const listeners = new Set<() => void>();
  return {
    get: () => value,
    set(next: T) {
      if (next === value) return;
      value = next;
      for (const listener of listeners) listener();
    },
    subscribe(listener: () => void) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

/** "programs" opens the palette showing only "New <program> session" items. */
export type PaletteMode = "all" | "programs";

const palette = store<PaletteMode | null>(null);

export function openPalette(mode: PaletteMode = "all") {
  palette.set(mode);
}

export function closePalette() {
  palette.set(null);
}

export function usePaletteMode(): PaletteMode | null {
  return useSyncExternalStore(palette.subscribe, palette.get);
}

export type SignalsTab = "needs" | "problems" | "activity";

const signalsTab = store<SignalsTab>("needs");

/** The tab the signals dock shows; the palette and status bar switch it from outside. */
export function useSignalsTab(): [SignalsTab, (tab: SignalsTab) => void] {
  return [useSyncExternalStore(signalsTab.subscribe, signalsTab.get), signalsTab.set];
}

/** The inspector's panel handle; `Workspace` attaches it, keys and the palette toggle it. */
export const inspectorPanel = createRef<PanelImperativeHandle>();

export function toggleInspector() {
  const panel = inspectorPanel.current;
  if (panel === null) return;
  if (panel.isCollapsed()) panel.expand();
  else panel.collapse();
}

/** Sessions with a tier-2 flag, in list order, and the tier-2 flags per severity. */
export function useProblems(): {
  sessions: { session: Session; flags: Flag[] }[];
  errors: number;
  warnings: number;
  flags: Map<string, Flag[]>;
  all: Session[];
} {
  const { sessions } = useSessions();
  const flags = useFlags(sessions);
  const problems: { session: Session; flags: Flag[] }[] = [];
  let errors = 0;
  let warnings = 0;
  for (const session of sessions) {
    const tier2 = (flags.get(session.key) ?? []).filter((flag) => flag.tier === 2);
    if (tier2.length === 0) continue;
    problems.push({ session, flags: tier2 });
    for (const flag of tier2) {
      if (flag.severity === "error") errors += 1;
      else if (flag.severity === "warn") warnings += 1;
    }
  }
  return { sessions: problems, errors, warnings, flags, all: sessions };
}
