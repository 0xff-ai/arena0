import { useSyncExternalStore } from "react";

/** The lists the workspace shows as documents, in Explorer order. */
export const LISTS = ["offers", "sessions", "programs", "receipts"] as const;
export type ListName = (typeof LISTS)[number];

export type DocRef =
  | { kind: "list"; list: ListName }
  | { kind: "session"; key: string }
  | { kind: "program"; hash: string }
  | { kind: "host"; id: string }
  | { kind: "receipt"; host: string; id: string };

interface DocTab {
  ref: DocRef;
  preview: boolean;
}

const KEY = "arena0.tabs";

export function docId(ref: DocRef): string {
  switch (ref.kind) {
    case "list":
      return `list:${ref.list}`;
    case "session":
      return `session:${ref.key}`;
    case "program":
      return `program:${ref.hash}`;
    case "host":
      return `host:${ref.id}`;
    case "receipt":
      return `receipt:${ref.host}/${ref.id}`;
  }
}

const listeners = new Set<() => void>();

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  window.addEventListener("storage", listener);
  return () => {
    listeners.delete(listener);
    window.removeEventListener("storage", listener);
  };
}

function write(docs: DocTab[]) {
  localStorage.setItem(KEY, JSON.stringify(docs));
  for (const listener of listeners) listener();
}

function isString(value: unknown): value is string {
  return typeof value === "string";
}

/** localStorage is outside our control: anything that is not a well-formed tab is dropped. */
function parseRef(value: unknown): DocRef | null {
  if (typeof value !== "object" || value === null) return null;
  const r = value as Record<string, unknown>;
  if (r.kind === "list" && LISTS.includes(r.list as ListName)) {
    return { kind: "list", list: r.list as ListName };
  }
  if (r.kind === "session" && isString(r.key)) return { kind: "session", key: r.key };
  if (r.kind === "program" && isString(r.hash)) return { kind: "program", hash: r.hash };
  if (r.kind === "host" && isString(r.id)) return { kind: "host", id: r.id };
  if (r.kind === "receipt" && isString(r.host) && isString(r.id)) {
    return { kind: "receipt", host: r.host, id: r.id };
  }
  return null;
}

// A browser that never stored tabs starts on the sessions list.
const FIRST_RUN: DocTab[] = [{ ref: { kind: "list", list: "sessions" }, preview: false }];

function parse(raw: string | null): DocTab[] {
  if (raw === null) return FIRST_RUN;
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return [];
  }
  if (!Array.isArray(parsed)) return [];
  const docs: DocTab[] = [];
  for (const entry of parsed) {
    if (typeof entry !== "object" || entry === null) continue;
    const ref = parseRef((entry as Record<string, unknown>).ref);
    if (ref === null) continue;
    docs.push({ ref, preview: (entry as Record<string, unknown>).preview === true });
  }
  return docs;
}

// `getSnapshot` must return the same object until the store changes, so the
// parse is cached per raw string.
let cachedRaw: string | null | undefined;
let cachedDocs: DocTab[] = [];

function snapshot(): DocTab[] {
  const raw = localStorage.getItem(KEY);
  if (raw !== cachedRaw) {
    cachedRaw = raw;
    cachedDocs = parse(raw);
  }
  return cachedDocs;
}

function indexOf(docs: DocTab[], ref: DocRef): number {
  const id = docId(ref);
  return docs.findIndex((doc) => docId(doc.ref) === id);
}

/** The left-most open document, where the workspace root goes; null when every tab is closed. */
export function firstDoc(): DocRef | null {
  return snapshot()[0]?.ref ?? null;
}

/**
 * Open documents, persisted in localStorage. Lists are documents too: every
 * tab can be closed. At most one is a preview: opening
 * another document unpinned replaces it in place, and pinning (or opening
 * pinned) keeps it. The active tab is not stored; it follows the route.
 */
export function useDocTabs(): {
  docs: { ref: DocRef; preview: boolean }[];
  open(ref: DocRef, pin: boolean): void;
  close(ref: DocRef): void;
} {
  const docs = useSyncExternalStore(subscribe, snapshot);
  return { docs, open, close };
}

function open(ref: DocRef, pin: boolean) {
  const docs = snapshot();
  const at = indexOf(docs, ref);
  if (at >= 0) {
    if (pin && docs[at]?.preview)
      write(docs.map((doc, i) => (i === at ? { ...doc, preview: false } : doc)));
    return;
  }
  if (pin) {
    write([...docs, { ref, preview: false }]);
    return;
  }
  const replaced = docs.findIndex((doc) => doc.preview);
  if (replaced < 0) write([...docs, { ref, preview: true }]);
  else write(docs.map((doc, i) => (i === replaced ? { ref, preview: true } : doc)));
}

function close(ref: DocRef) {
  const docs = snapshot();
  const at = indexOf(docs, ref);
  if (at >= 0) write(docs.filter((_, i) => i !== at));
}
