import { createRootRoute, createRoute, createRouter, redirect } from "@tanstack/react-router";
import { lazy } from "react";
import { Workspace } from "~/workspace/Workspace";

export type SessionsSearch = {
  state?: "live" | "flagged" | "all";
  group?: "list" | "host";
  host?: string;
  program?: string;
  q?: string;
  from?: number;
  to?: number;
};
export type SessionSearch = {
  sub?: "steps" | "view" | "negotiation" | "query" | "evidence";
  step?: number;
  host?: string;
  compare?: string;
};
export type ReceiptsSearch = { host?: string };
export type ProgramSearch = { launch?: boolean };

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

const rootRoute = createRootRoute({ component: Workspace });

export { rootRoute };

const indexRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/",
  beforeLoad: () => {
    throw redirect({ to: "/sessions", replace: true });
  },
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
  stringifySearch: (search) => {
    const params = new URLSearchParams();
    for (const [name, value] of Object.entries(search)) {
      if (value !== undefined && value !== null) params.set(name, String(value));
    }
    const encoded = params.toString();
    return encoded === "" ? "" : `?${encoded}`;
  },
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
