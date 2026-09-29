import { useLayoutEffect, useSyncExternalStore } from "react";

export type ThemePref = "light" | "dark" | "system";

const THEME_KEY = "arena0.theme";
const SEATS_KEY = "arena0.seats";
const DARK_QUERY = "(prefers-color-scheme: dark)";

const listeners = new Set<() => void>();

// `storage` fires only in other tabs, so this tab's writes notify by hand.
function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  window.addEventListener("storage", listener);
  return () => {
    listeners.delete(listener);
    window.removeEventListener("storage", listener);
  };
}

function write(key: string, value: string) {
  localStorage.setItem(key, value);
  for (const listener of listeners) listener();
}

/** The raw stored string: a primitive, so it is a stable store snapshot. */
function useStored(key: string): string | null {
  return useSyncExternalStore(subscribe, () => localStorage.getItem(key));
}

function setTheme(theme: ThemePref) {
  write(THEME_KEY, theme);
}

function setSeats(hosts: string[]) {
  write(SEATS_KEY, JSON.stringify(hosts));
}

/** Also keeps `<html data-theme>` in step; `system` follows the OS setting live. */
export function useTheme(): [ThemePref, (theme: ThemePref) => void] {
  const stored = useStored(THEME_KEY);
  const theme: ThemePref = stored === "light" || stored === "dark" ? stored : "system";
  useLayoutEffect(() => {
    const media = matchMedia(DARK_QUERY);
    const apply = () => {
      document.documentElement.dataset.theme =
        theme === "system" ? (media.matches ? "dark" : "light") : theme;
    };
    apply();
    if (theme !== "system") return;
    media.addEventListener("change", apply);
    return () => media.removeEventListener("change", apply);
  }, [theme]);
  return [theme, setTheme];
}

/**
 * The Hosts the user answers for. Every Host until the user chooses; stored
 * Hosts that no longer exist are dropped.
 */
export function useSeats(allHosts: string[]): [ReadonlySet<string>, (hosts: string[]) => void] {
  const stored = parseSeats(useStored(SEATS_KEY));
  const seats =
    stored === null ? new Set(allHosts) : new Set(stored.filter((host) => allHosts.includes(host)));
  return [seats, setSeats];
}

/** Null when nothing usable is stored: localStorage is outside our control. */
function parseSeats(raw: string | null): string[] | null {
  if (raw === null) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  return Array.isArray(parsed) && parsed.every((host) => typeof host === "string") ? parsed : null;
}
