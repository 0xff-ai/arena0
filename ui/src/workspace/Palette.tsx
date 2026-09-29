import { useRouter } from "@tanstack/react-router";
import { useState } from "react";
import { shortHash, useRows, useTheme } from "~/model";
import { useCall, useCollections } from "~/sync";
import { CommandPalette, ConfirmDialog, Icons, type PaletteItem, toasts } from "~/ui";
import { shortSessionKey } from "./common/SessionLabel";
import { programHref, useOpenDoc } from "./nav";
import {
  closePalette,
  openPalette,
  toggleInspector,
  usePaletteMode,
  useProblems,
  useSignalsTab,
} from "./shell";

const NEXT_THEME = { light: "dark", dark: "system", system: "light" } as const;

export function Palette() {
  const router = useRouter();
  const mode = usePaletteMode();
  const openDoc = useOpenDoc();
  const collections = useCollections();
  const programs = useRows(collections.programs);
  const hosts = useRows(collections.hosts);
  const receipts = useRows(collections.receipts);
  const { all: sessions, flags } = useProblems();
  const [theme, setTheme] = useTheme();
  const [, setSignalsTab] = useSignalsTab();
  const [confirmStop, setConfirmStop] = useState(false);
  const stop = useCall("daemon_stop");

  const go = (path: string) => () => router.history.push(path);
  const goSignals = (tab: "needs" | "problems" | "activity") => () => setSignalsTab(tab);
  const programName = (hash: string) =>
    programs.find((program) => program.hash === hash)?.display_name ?? shortHash(hash);

  // The launch form is the program document with `launch` set.
  const newSession: PaletteItem[] = programs.map((program) => ({
    id: `new:${program.hash}`,
    label: `New ${program.display_name} session`,
    detail: `v${program.version}`,
    icon: Icons.add,
    onAction: () => {
      openDoc({ kind: "program", hash: program.hash }, { pin: true });
      router.history.push(programHref(program.hash, true));
    },
  }));

  const groups =
    mode === "programs"
      ? [{ id: "new", title: "New session", items: newSession }]
      : [
          {
            id: "go",
            title: "Go to",
            items: [
              {
                id: "go:sessions",
                label: "Sessions",
                icon: Icons.session,
                kbd: "g s",
                onAction: go("/sessions"),
              },
              {
                id: "go:offers",
                label: "Offers",
                icon: Icons.offer,
                kbd: "g o",
                onAction: go("/offers"),
              },
              {
                id: "go:receipts",
                label: "Receipts",
                icon: Icons.receipt,
                kbd: "g r",
                onAction: go("/receipts"),
              },
              {
                id: "go:programs",
                label: "Programs",
                icon: Icons.program,
                kbd: "g p",
                onAction: go("/programs"),
              },
              {
                id: "go:needs",
                label: "Needs input",
                icon: Icons.callout,
                onAction: goSignals("needs"),
              },
              {
                id: "go:problems",
                label: "Problems",
                icon: Icons.problem,
                onAction: goSignals("problems"),
              },
              {
                id: "go:activity",
                label: "Activity",
                icon: Icons.activity,
                onAction: goSignals("activity"),
              },
            ],
          },
          {
            id: "sessions",
            title: "Sessions",
            items: sessions.map((session) => {
              const top = flags.get(session.key)?.[0];
              return {
                id: `session:${session.key}`,
                label: `${session.program?.display_name ?? shortHash(session.programHash)} ${shortSessionKey(session.key)}`,
                detail: top ? `${session.state} · ${top.short}` : session.state,
                icon: Icons.session,
                keywords: [session.key, session.sessionId ?? ""],
                onAction: () => openDoc({ kind: "session", key: session.key }),
              };
            }),
          },
          {
            id: "programs",
            title: "Programs",
            items: programs.flatMap((program) => [
              {
                id: `program:${program.hash}`,
                label: program.display_name,
                detail: `v${program.version}`,
                icon: Icons.program,
                keywords: [program.name, program.hash],
                onAction: () => openDoc({ kind: "program", hash: program.hash }),
              },
              ...newSession.filter((item) => item.id === `new:${program.hash}`),
            ]),
          },
          {
            id: "hosts",
            title: "Hosts",
            items: hosts.map((host) => ({
              id: `host:${host.id}`,
              label: host.id,
              detail: host.user_agent ?? undefined,
              icon: Icons.host,
              onAction: () => openDoc({ kind: "host", id: host.id }),
            })),
          },
          {
            id: "receipts",
            title: "Receipts",
            items: receipts.map((receipt) => ({
              id: `receipt:${receipt.key}`,
              label: `${programName(receipt.program)} ${shortHash(receipt.receipt_id)}`,
              detail: `${receipt.kind === "stop_report" ? "stop report" : "receipt"} · ${receipt.host}`,
              icon: receipt.kind === "stop_report" ? Icons.stopReport : Icons.receipt,
              keywords: [receipt.receipt_id],
              onAction: () =>
                openDoc({ kind: "receipt", host: receipt.host, id: receipt.receipt_id }),
            })),
          },
          {
            id: "actions",
            title: "Actions",
            items: [
              {
                id: "act:theme",
                label: "Toggle theme",
                icon: Icons.theme,
                onAction: () => setTheme(NEXT_THEME[theme]),
              },
              {
                id: "act:inspector",
                label: "Toggle inspector",
                icon: Icons.view,
                kbd: "Mod+.",
                onAction: toggleInspector,
              },
              {
                id: "act:stop",
                label: "Stop daemon",
                icon: Icons.stop,
                onAction: () => setConfirmStop(true),
              },
            ],
          },
        ];

  return (
    <>
      <CommandPalette
        isOpen={mode !== null}
        onOpenChange={(open) => (open ? openPalette(mode ?? "all") : closePalette())}
        groups={groups.filter((group) => group.items.length > 0)}
        placeholder={mode === "programs" ? "Choose a program for the new session…" : undefined}
      />
      <ConfirmDialog
        title="Stop daemon"
        body="arena0d stops, and this page loses its connection until it is started again."
        confirmLabel="Stop daemon"
        danger
        isOpen={confirmStop}
        onOpenChange={setConfirmStop}
        onConfirm={() =>
          stop.mutate(null, {
            onError: (error) =>
              toasts.show({ tone: "bad", title: "Could not stop the daemon", body: error.message }),
          })
        }
      />
    </>
  );
}
