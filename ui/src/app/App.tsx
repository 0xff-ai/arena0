import { RouterProvider } from "@tanstack/react-router";
import { useState } from "react";
import { useTheme } from "~/model";
import { connectGateway, type Gateway, readToken, SyncProvider, useConnection } from "~/sync";
import { EmptyState, Icons, ToastRegion } from "~/ui";
import { Connecting } from "./Connecting";
import { router } from "./router";

// One socket for the page's whole life. Module scope, not state: StrictMode
// runs initializers twice and would leak a second connection.
let gateway: Gateway | null = null;

function pageGateway(token: string): Gateway {
  gateway ??= connectGateway({
    url: `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws`,
    token,
  });
  return gateway;
}

export function App() {
  // Sets `<html data-theme>` for every state below, including the ones before the workspace.
  useTheme();
  const token = readToken();
  if (token === null) {
    return (
      <div className="flex h-full items-center justify-center bg-editor">
        <EmptyState
          icon={Icons.link}
          title="Open the link printed by `arena0 ui`"
          body={
            <>
              Run{" "}
              <code className="rounded-xs border border-line bg-surface px-1 font-mono text-xs text-fg">
                arena0 ui
              </code>{" "}
              in a terminal and open the address it prints.
            </>
          }
        />
      </div>
    );
  }
  return (
    <SyncProvider gateway={pageGateway(token)}>
      <Shell />
      <ToastRegion />
    </SyncProvider>
  );
}

/** The router once the gateway has been live; a later drop keeps the workspace and shows offline in its bars. */
function Shell() {
  const { status } = useConnection();
  const [wasLive, setWasLive] = useState(false);
  if (status === "live" && !wasLive) setWasLive(true);
  if (!wasLive && status !== "live") return <Connecting />;
  return <RouterProvider router={router} context={{}} />;
}
