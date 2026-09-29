import {
  QueryClient,
  QueryClientProvider,
  type UseMutationResult,
  type UseQueryResult,
  useMutation,
  useQuery,
} from "@tanstack/react-query";
import { createContext, type JSX, type ReactNode, useContext, useSyncExternalStore } from "react";
import type { DaemonInfo } from "~/api/types.gen";
import { type Collections, createCollections } from "./collections";
import type { ConnectionState, Daemon } from "./daemon";
import { type OpArgs, type OpName, type OpReplies, ops } from "./ops";
import { DaemonError } from "./rpc";

interface Replica {
  collections: Collections;
  queryClient: QueryClient;
}

// One replica per daemon for the daemon's whole life. Collections subscribe
// to the daemon when created, so building them in a render or a state
// initializer would leak a subscription whenever React discards that render
// (StrictMode does it on purpose).
const replicas = new WeakMap<Daemon, Replica>();

function replicaOf(daemon: Daemon): Replica {
  let replica = replicas.get(daemon);
  if (replica === undefined) {
    replica = {
      collections: createCollections(daemon),
      // Reads and calls report failures to the caller; nothing is retried on
      // its own because a call that reached the daemon may have had effects.
      queryClient: new QueryClient({
        defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false } },
      }),
    };
    replicas.set(daemon, replica);
  }
  return replica;
}

const DaemonContext = createContext<Daemon | null>(null);
const CollectionsContext = createContext<Collections | null>(null);

export function SyncProvider(props: { daemon: Daemon; children: ReactNode }): JSX.Element {
  const { collections, queryClient } = replicaOf(props.daemon);
  return (
    <DaemonContext value={props.daemon}>
      <CollectionsContext value={collections}>
        <QueryClientProvider client={queryClient}>{props.children}</QueryClientProvider>
      </CollectionsContext>
    </DaemonContext>
  );
}

export function useDaemon(): Daemon {
  const daemon = useContext(DaemonContext);
  if (daemon === null) throw new Error("useDaemon needs a SyncProvider above it");
  return daemon;
}

export function useCollections(): Collections {
  const collections = useContext(CollectionsContext);
  if (collections === null) throw new Error("useCollections needs a SyncProvider above it");
  return collections;
}

export function useConnection(): ConnectionState {
  const daemon = useDaemon();
  return useSyncExternalStore(daemon.onState, daemon.state);
}

/** The latest daemon info; kept while offline. */
export function useDaemonInfo(): DaemonInfo | null {
  const daemon = useDaemon();
  return useSyncExternalStore(daemon.onState, daemon.info);
}

/** Request/response read, cached by [op, args]; `null` args disables it. Never retries. */
export function useRead<K extends "view" | "records" | "query" | "receipt" | "verify">(
  op: K,
  args: OpArgs<K> | null,
  options?: { staleTime?: number },
): UseQueryResult<OpReplies[K], DaemonError> {
  // Reads wait for a live connection instead of failing with "not connected";
  // a stale read refetches when the connection comes back.
  const live = useConnection().status === "live";
  return useQuery<OpReplies[K], DaemonError>({
    queryKey: ["arena0", op, args],
    enabled: args !== null && live,
    queryFn: () => {
      if (args === null) throw new Error("disabled read executed");
      return ops[op](args);
    },
    staleTime: options?.staleTime ?? 0,
  });
}

/** One mutation per call; never retries. */
export function useCall<K extends OpName>(
  op: K,
): UseMutationResult<OpReplies[K], DaemonError, OpArgs<K>> {
  const daemon = useDaemon();
  return useMutation<OpReplies[K], DaemonError, OpArgs<K>>({
    mutationFn: async (args) => {
      const reply = await ops[op](args);
      // The operation discriminates the generic arguments/reply, but TypeScript
      // cannot narrow a type parameter by comparing its value.
      let hosts: string[] = [];
      switch (op) {
        case "program_import":
          hosts = (reply as OpReplies["program_import"]).hosts;
          break;
        case "host_open":
          hosts = [(reply as OpReplies["host_open"]).id];
          break;
        case "program_remove":
        case "receipt_import":
        case "blob_import":
          hosts = [(args as OpArgs<"program_remove" | "receipt_import" | "blob_import">).host];
          break;
      }
      // The write already succeeded. A failed reload is a connection failure the
      // engine owns: it goes offline, and its reconnect reloads every Host.
      for (const host of hosts)
        await daemon.reload(host).catch((error: unknown) => {
          if (!(error instanceof DaemonError)) throw error;
        });
      return reply;
    },
    retry: false,
  });
}
