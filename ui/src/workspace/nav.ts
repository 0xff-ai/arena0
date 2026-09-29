import { useRouter, useRouterState } from "@tanstack/react-router";
import { type DocRef, docId, useDocTabs } from "./tabs";

export type SessionSub = "steps" | "view" | "negotiation" | "query" | "evidence";

// Search values are plain strings in the address bar (see `router.tsx`), so
// hashes made only of digits are never turned into numbers.
function query(params: Record<string, string | number | undefined>): string {
  const search = new URLSearchParams();
  for (const [name, value] of Object.entries(params)) {
    if (value !== undefined) search.set(name, String(value));
  }
  const text = search.toString();
  return text === "" ? "" : `?${text}`;
}

export function sessionHref(
  key: string,
  search?: { sub?: SessionSub; step?: number; host?: string },
): string {
  return `/s/${encodeURIComponent(key)}${query({ ...search })}`;
}

export function programHref(hash: string, launch?: boolean): string {
  return `/programs/${encodeURIComponent(hash)}${query({ launch: launch ? "true" : undefined })}`;
}

export function hostHref(id: string): string {
  return `/hosts/${encodeURIComponent(id)}`;
}

export function receiptHref(host: string, id: string): string {
  return `/receipts/${encodeURIComponent(host)}/${encodeURIComponent(id)}`;
}

export function docHref(ref: DocRef): string {
  switch (ref.kind) {
    case "session":
      return sessionHref(ref.key);
    case "program":
      return programHref(ref.hash);
    case "host":
      return hostHref(ref.id);
    case "receipt":
      return receiptHref(ref.host, ref.id);
  }
}

/** Navigates to the document and makes sure it has a tab (a preview unless `pin`). */
export function useOpenDoc(): (doc: DocRef, options?: { pin?: boolean }) => void {
  const router = useRouter();
  const tabs = useDocTabs();
  return (doc, options) => {
    tabs.open(doc, options?.pin ?? false);
    router.history.push(docHref(doc));
  };
}

/** The document the current route shows, or null on a list route. */
export function useActiveDoc(): DocRef | null {
  // Route params arrive decoded, so a session key containing "/" stays whole.
  const leaf = useRouterState({ select: (state) => state.matches.at(-1) });
  if (leaf === undefined) return null;
  const params = leaf.params as Record<string, string | undefined>;
  switch (leaf.routeId) {
    case "/s/$key":
      return params.key === undefined ? null : { kind: "session", key: params.key };
    case "/programs/$hash":
      return params.hash === undefined ? null : { kind: "program", hash: params.hash };
    case "/hosts/$id":
      return params.id === undefined ? null : { kind: "host", id: params.id };
    case "/receipts/$host/$id":
      return params.host === undefined || params.id === undefined
        ? null
        : { kind: "receipt", host: params.host, id: params.id };
    default:
      return null;
  }
}

/** Closes a tab. Closing the active one goes to the tab on its left, else to the sessions list. */
export function useCloseDoc(): (doc: DocRef) => void {
  const router = useRouter();
  const tabs = useDocTabs();
  const active = useActiveDoc();
  return (doc) => {
    const at = tabs.docs.findIndex((d) => docId(d.ref) === docId(doc));
    tabs.close(doc);
    if (active === null || docId(active) !== docId(doc)) return;
    const left = tabs.docs[at - 1];
    router.history.push(left ? docHref(left.ref) : "/sessions");
  };
}
