import {
  QueryClient,
  QueryClientProvider,
  skipToken,
  type UseMutationResult,
  type UseQueryResult,
  useMutation,
  useQuery,
} from "@tanstack/react-query";
import { createContext, type JSX, type ReactNode, useContext, useSyncExternalStore } from "react";
import { type Collections, createCollections } from "./collections";
import type { ConnectionState, Gateway } from "./gateway";
import type { GatewayError, OpReplies } from "./ops";
import type { Hello, OpArgs, OpName } from "./protocol";

interface Replica {
  collections: Collections;
  queryClient: QueryClient;
}

// One replica per gateway for the gateway's whole life. Collections subscribe
// to the gateway when created, so building them in a render or a state
// initializer would leak a subscription whenever React discards that render
// (StrictMode does it on purpose).
const replicas = new WeakMap<Gateway, Replica>();

function replicaOf(gateway: Gateway): Replica {
  let replica = replicas.get(gateway);
  if (replica === undefined) {
    replica = {
      collections: createCollections(gateway),
      // Reads and calls report failures to the caller; nothing is retried on
      // its own because a call that reached the daemon may have had effects.
      queryClient: new QueryClient({
        defaultOptions: { queries: { retry: false, refetchOnWindowFocus: false } },
      }),
    };
    replicas.set(gateway, replica);
  }
  return replica;
}

const GatewayContext = createContext<Gateway | null>(null);
const CollectionsContext = createContext<Collections | null>(null);

export function SyncProvider(props: { gateway: Gateway; children: ReactNode }): JSX.Element {
  const { collections, queryClient } = replicaOf(props.gateway);
  return (
    <GatewayContext value={props.gateway}>
      <CollectionsContext value={collections}>
        <QueryClientProvider client={queryClient}>{props.children}</QueryClientProvider>
      </CollectionsContext>
    </GatewayContext>
  );
}

export function useGateway(): Gateway {
  const gateway = useContext(GatewayContext);
  if (gateway === null) throw new Error("useGateway needs a SyncProvider above it");
  return gateway;
}

export function useCollections(): Collections {
  const collections = useContext(CollectionsContext);
  if (collections === null) throw new Error("useCollections needs a SyncProvider above it");
  return collections;
}

export function useConnection(): ConnectionState {
  const gateway = useGateway();
  return useSyncExternalStore(gateway.onState, gateway.state);
}

/** The gateway's `hello`; it changes together with the connection state. */
export function useHello(): Hello | null {
  const gateway = useGateway();
  return useSyncExternalStore(gateway.onState, gateway.hello);
}

/** Request/response read, cached by [op, args]; `null` args disables it. Never retries. */
export function useRead<K extends "view" | "records" | "query" | "receipt" | "verify">(
  op: K,
  args: OpArgs<K> | null,
  options?: { staleTime?: number },
): UseQueryResult<OpReplies[K], GatewayError> {
  const gateway = useGateway();
  // Reads wait for a live connection instead of failing with "not connected";
  // a stale read refetches when the connection comes back.
  const live = useConnection().status === "live";
  return useQuery<OpReplies[K], GatewayError>({
    queryKey: ["arena0", op, args],
    // skipToken disables the query and narrows `args` to non-null for the call.
    queryFn: args === null || !live ? skipToken : () => gateway.call(op, args),
    staleTime: options?.staleTime ?? 0,
  });
}

/** One mutation per call; never retries. */
export function useCall<K extends OpName>(
  op: K,
): UseMutationResult<OpReplies[K], GatewayError, OpArgs<K>> {
  const gateway = useGateway();
  return useMutation<OpReplies[K], GatewayError, OpArgs<K>>({
    mutationFn: (args) => gateway.call(op, args),
    retry: false,
  });
}
