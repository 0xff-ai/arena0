import { Outlet, useRouter } from "@tanstack/react-router";
import { Suspense } from "react";
import { RouterProvider } from "react-aria-components";
import { useDefaultLayout } from "react-resizable-panels";
import { Panel, PanelGroup, PanelHandle } from "~/ui";
import { ComposerDock } from "./composer/ComposerDock";
import { useComposer } from "./composer/store";
import { Explorer } from "./explorer/Explorer";
import { Inspector } from "./inspector/Inspector";
import { useGlobalKeys } from "./keys";
import { Palette } from "./Palette";
import { StatusBar } from "./StatusBar";
import { inspectorPanel } from "./shell";
import { Signals } from "./signals/Signals";
import { TabRow } from "./TabRow";
import { TitleBar } from "./TitleBar";
import { Timeline } from "./timeline/Timeline";

export function Workspace() {
  const router = useRouter();
  useGlobalKeys();
  const composing = useComposer().calloutKey !== null;

  const main = useDefaultLayout({ id: "arena0.layout.main" });
  const left = useDefaultLayout({ id: "arena0.layout.left" });
  // The composer panel comes and goes; a layout is saved per set of panels.
  const centre = useDefaultLayout({
    id: "arena0.layout.centre",
    panelIds: composing ? ["document", "composer", "timeline"] : ["document", "timeline"],
  });

  return (
    // RAC links inside the workspace navigate through the router instead of reloading.
    <RouterProvider
      navigate={(to) => router.history.push(to as string)}
      useHref={(to) => router.history.createHref(to as string)}
    >
      <div className="grid h-full grid-rows-[36px_minmax(0,1fr)_24px]">
        <TitleBar />
        <PanelGroup
          orientation="horizontal"
          id="main"
          defaultLayout={main.defaultLayout}
          onLayoutChanged={main.onLayoutChanged}
        >
          <Panel
            id="left-dock"
            defaultSize="20%"
            minSize="14%"
            maxSize="32%"
            className="bg-surface"
          >
            <PanelGroup
              orientation="vertical"
              id="left"
              defaultLayout={left.defaultLayout}
              onLayoutChanged={left.onLayoutChanged}
            >
              <Panel id="explorer" defaultSize="58%" minSize="15%">
                <Explorer />
              </Panel>
              <PanelHandle orientation="vertical" />
              <Panel id="signals" defaultSize="42%" minSize="15%">
                <Signals />
              </Panel>
            </PanelGroup>
          </Panel>
          <PanelHandle orientation="horizontal" />
          <Panel id="centre" minSize="30%">
            <PanelGroup
              orientation="vertical"
              id="centre"
              defaultLayout={centre.defaultLayout}
              onLayoutChanged={centre.onLayoutChanged}
            >
              <Panel id="document" minSize="25%">
                <div className="flex h-full min-h-0 flex-col bg-editor">
                  <TabRow />
                  <main data-region="document" className="min-h-0 flex-1 overflow-auto">
                    {/* Route components load lazily; only the document waits for them. */}
                    <Suspense fallback={null}>
                      <Outlet />
                    </Suspense>
                  </main>
                </div>
              </Panel>
              {composing && (
                <>
                  <PanelHandle orientation="vertical" />
                  <Panel id="composer" defaultSize="32%" minSize="15%" className="bg-surface">
                    <ComposerDock />
                  </Panel>
                </>
              )}
              <PanelHandle orientation="vertical" />
              <Panel id="timeline" defaultSize="18%" minSize="10%" className="bg-surface">
                <Timeline />
              </Panel>
            </PanelGroup>
          </Panel>
          <PanelHandle orientation="horizontal" />
          <Panel
            id="inspector"
            panelRef={inspectorPanel}
            defaultSize="22%"
            minSize="16%"
            collapsible
            collapsedSize={0}
            className="bg-surface"
          >
            <Inspector />
          </Panel>
        </PanelGroup>
        <StatusBar />
      </div>
      <Palette />
    </RouterProvider>
  );
}
