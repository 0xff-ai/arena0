import { createRootRoute, createRoute, createRouter, redirect } from "@tanstack/react-router";
import { lazy } from "react";
import { EmptyWorkspace } from "~/workspace/EmptyWorkspace";
import { docHref, query, type SessionSub } from "~/workspace/nav";
import { firstDoc } from "~/workspace/tabs";
import { Workspace } from "~/workspace/Workspace";

type SessionsSearch = {
  state?: "live" | "flagged" | "all";
  group?: "list" | "host";
  host?: string;
  program?: string;
  q?: string;
  from?: number;
  to?: number;
};
export type SessionSearch = {
  sub?: SessionSub;
  step?: number;
  host?: string;
  compare?: string;
};
/**
 * Changes some of the document's search parameters and keeps the others. A key
 * present with `undefined` clears that parameter. Every sub tab reads and
 * writes its state here, so the address always reproduces what is shown.
 */
export type UpdateSearch = (patch: Partial<SessionSearch>) => void;
type ReceiptsSearch = { host?: string };
type ProgramSearch = { launch?: boolean };

// Search values are validated at the address bar, the trust boundary: an
// unknown or malformed value is dropped, never an error.
function text(value: unknown): string | undefined {
  return typeof value === "string" && value !== "" ? value : undefined;
}

function count(value: unknown): number | undefined {
  const n = typeof value === "string" ? Number(value) : value;
  return typeof n === "number" && Number.isSafeInteger(n) && n >= 0 ? n : undefined;
}

function oneOf<T extends string>(value: unknown, allowed: readonly T[]): T | undefined {
  return allowed.find((candidate) => candidate === value);
}

function validateSessions(search: Record<string, unknown>): SessionsSearch {
  return {
    state: oneOf(search.state, ["live", "flagged", "all"]),
    group: oneOf(search.group, ["list", "host"]),
    host: text(search.host),
    program: text(search.program),
    q: text(search.q),
    from: count(search.from),
    to: count(search.to),
  };
}

function validateSession(search: Record<string, unknown>): SessionSearch {
  return {
    sub: oneOf(search.sub, ["steps", "view", "negotiation", "query", "evidence"]),
    step: count(search.step),
    host: text(search.host),
    compare: text(search.compare),
  };
}

export const rootRoute = createRootRoute({ component: Workspace });

// The root shows the left-most open tab, or an empty workspace once every tab is closed.
const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  beforeLoad: () => {
    const first = firstDoc();
    if (first !== null) throw redirect({ href: docHref(first), replace: true });
  },
  component: EmptyWorkspace,
});

// Route components load on first use; each is a named export of its owner's file.
export const sessionsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/sessions",
  validateSearch: validateSessions,
  component: lazy(() =>
    import("~/workspace/sessions/SessionsList").then((m) => ({ default: m.SessionsList })),
  ),
});

export const offersRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/offers",
  component: lazy(() =>
    import("~/workspace/offers/OffersList").then((m) => ({ default: m.OffersList })),
  ),
});

export const receiptsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/receipts",
  validateSearch: (search: Record<string, unknown>): ReceiptsSearch => ({
    host: text(search.host),
  }),
  component: lazy(() =>
    import("~/workspace/receipts/ReceiptsList").then((m) => ({ default: m.ReceiptsList })),
  ),
});

export const programsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/programs",
  component: lazy(() =>
    import("~/workspace/programs/ProgramsList").then((m) => ({ default: m.ProgramsList })),
  ),
});

export const sessionRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/s/$key",
  validateSearch: validateSession,
  component: lazy(() =>
    import("~/workspace/session/SessionDoc").then((m) => ({ default: m.SessionDoc })),
  ),
});

export const programRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/programs/$hash",
  validateSearch: (search: Record<string, unknown>): ProgramSearch => ({
    launch: search.launch === true || search.launch === "true" ? true : undefined,
  }),
  component: lazy(() =>
    import("~/workspace/program/ProgramDoc").then((m) => ({ default: m.ProgramDoc })),
  ),
});

export const receiptRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/receipts/$host/$id",
  component: lazy(() =>
    import("~/workspace/receipt/ReceiptDoc").then((m) => ({ default: m.ReceiptDoc })),
  ),
});

export const hostRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/hosts/$id",
  component: lazy(() => import("~/workspace/host/HostDoc").then((m) => ({ default: m.HostDoc }))),
});

const routeTree = rootRoute.addChildren([
  indexRoute,
  sessionsRoute,
  offersRoute,
  receiptsRoute,
  programsRoute,
  sessionRoute,
  programRoute,
  receiptRoute,
  hostRoute,
]);

export const router = createRouter({
  routeTree,
  defaultPreload: false,
  // Plain strings, not JSON: a program hash of digits or `1e5` must stay text.
  parseSearch: (search) => Object.fromEntries(new URLSearchParams(search)),
  stringifySearch: query,
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
