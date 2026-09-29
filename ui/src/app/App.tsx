import { RouterProvider } from "@tanstack/react-router";
import { useState } from "react";
import { useTheme } from "~/model";
import { connectDaemon, type Daemon, SyncProvider, useConnection } from "~/sync";
import { ToastRegion } from "~/ui";
import { Connecting } from "./Connecting";
import { router } from "./router";

// Module scope gives StrictMode one connection for the page's lifetime.
let daemon: Daemon | null = null;

export function App() {
  // Sets `<html data-theme>` for every state below, including the ones before the workspace.
  useTheme();
  daemon ??= connectDaemon();
  return (
    <SyncProvider daemon={daemon}>
      <Shell />
      <ToastRegion />
    </SyncProvider>
  );
}

/** The router once the daemon has been live; a later drop keeps the workspace and shows offline in its bars. */
function Shell() {
  const { status } = useConnection();
  const [wasLive, setWasLive] = useState(false);
  if (status === "live" && !wasLive) setWasLive(true);
  if (!wasLive && status !== "live") return <Connecting />;
  return <RouterProvider router={router} context={{}} />;
}
