import { useLayoutEffect, useSyncExternalStore } from "react";

type ThemePref = "light" | "dark" | "system";

const THEME_KEY = "arena0.theme";
const YOUR_HOSTS_KEY = "arena0.your-hosts";
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
  return [theme, (next) => write(THEME_KEY, next)];
}

/**
 * The Hosts whose participants the user answers for; the UI calls them
 * "your participants". Every Host until the user chooses; stored
 * Hosts that no longer exist are dropped.
 */
export function useYourHosts(allHosts: string[]): [ReadonlySet<string>, (hosts: string[]) => void] {
  const stored = parseHosts(useStored(YOUR_HOSTS_KEY));
  const yourHosts =
    stored === null ? new Set(allHosts) : new Set(stored.filter((host) => allHosts.includes(host)));
  return [yourHosts, (hosts) => write(YOUR_HOSTS_KEY, JSON.stringify(hosts))];
}

/** Null when nothing usable is stored: localStorage is outside our control. */
function parseHosts(raw: string | null): string[] | null {
  if (raw === null) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  return Array.isArray(parsed) && parsed.every((host) => typeof host === "string") ? parsed : null;
}
