import { useRouter } from "@tanstack/react-router";
import { useState } from "react";
import { fmtRange, isTerminal, useRows, useSessions } from "~/model";
import { type ProgramRow, useCall, useCollections } from "~/sync";
import {
  Button,
  type Column,
  ConfirmDialog,
  DataTable,
  EmptyState,
  HashChip,
  IconButton,
  Icons,
  Toolbar,
  toasts,
} from "~/ui";
import { ImportProgramButton } from "../actions/imports";
import { programHref, useOpenDoc } from "../nav";
import { useSelection } from "../selection";
import { useDocTabs } from "../tabs";

export function ProgramsList() {
  const programs = useRows(useCollections().programs);
  const { sessions } = useSessions();
  const [selection, select] = useSelection();
  const openDoc = useOpenDoc();
  const tabs = useDocTabs();
  const router = useRouter();
  const remove = useCall("program_remove");
  const [removing, setRemoving] = useState<ProgramRow | null>(null);

  const live = new Map<string, number>();
  for (const session of sessions) {
    if (!isTerminal(session))
      live.set(session.programHash, (live.get(session.programHash) ?? 0) + 1);
  }
  const rows = [...programs].sort((a, b) => a.display_name.localeCompare(b.display_name));

  const launch = (program: ProgramRow) => {
    tabs.open({ kind: "program", hash: program.hash }, false);
    router.history.push(programHref(program.hash, true));
  };

  // Each Host holds its own copy, so removal is one call per Host that has it.
  const confirmRemove = async (program: ProgramRow) => {
    const results = await Promise.allSettled(
      program.hosts.map((host) => remove.mutateAsync({ host, program: program.hash })),
    );
    const failed = results.flatMap((result, i) =>
      result.status === "rejected" ? [`${program.hosts[i]}: ${String(result.reason.message)}`] : [],
    );
    if (failed.length === 0) {
      toasts.show({
        tone: "ok",
        title: `Removed ${program.display_name}`,
        body: `From ${program.hosts.join(", ")}`,
      });
    } else {
      toasts.show({
        tone: "bad",
        title: `Could not remove ${program.display_name} everywhere`,
        body: failed.join("; "),
      });
    }
  };

  const columns: Column<ProgramRow>[] = [
    {
      id: "program",
      title: "Program",
      width: "1fr",
      minWidth: 180,
      isRowHeader: true,
      render: (program) => (
        <span className="inline-flex min-w-0 items-baseline gap-1.5">
          <span className="truncate text-fg">{program.display_name}</span>
          <span className="truncate font-mono text-xs text-subtle">{program.name}</span>
        </span>
      ),
    },
    {
      id: "version",
      title: "Version",
      width: 64,
      minWidth: 64,
      render: (program) => <span className="font-mono text-xs">{program.version}</span>,
    },
    {
      id: "participants",
      title: "Participants",
      width: 88,
      minWidth: 88,
      render: (program) => (
        <span className="font-mono text-xs">{fmtRange(program.participants)}</span>
      ),
    },
    {
      id: "hosts",
      title: "Hosts",
      width: 128,
      minWidth: 128,
      render: (program) => <span className="truncate">{program.hosts.join(", ")}</span>,
    },
    {
      id: "live",
      title: "Live",
      width: 48,
      minWidth: 48,
      align: "end",
      render: (program) => (
        <span className="font-mono text-xs tabular">{live.get(program.hash) ?? "—"}</span>
      ),
    },
    {
      id: "hash",
      title: "Hash",
      width: 120,
      minWidth: 120,
      render: (program) => <HashChip hash={program.hash} />,
    },
    {
      id: "actions",
      title: "",
      width: 130,
      minWidth: 130,
      align: "end",
      render: (program) => (
        <span className="inline-flex items-center gap-1">
          <Button
            size="sm"
            variant="ghost"
            icon={Icons.add}
            aria-label={`New ${program.name} session`}
            onPress={() => launch(program)}
          >
            New session
          </Button>
          <IconButton
            icon={Icons.remove}
            label={`Remove ${program.name}`}
            onPress={() => setRemoving(program)}
          />
        </span>
      ),
    },
  ];

  return (
    <div className="flex h-full min-h-0 flex-col">
      <Toolbar aria-label="Programs">
        <span className="text-base text-fg">Programs</span>
        <span className="font-mono text-xs text-subtle tabular">{rows.length}</span>
        <span className="min-w-2 flex-1" />
        <ImportProgramButton />
      </Toolbar>
      <div className="min-h-0 flex-1">
        <DataTable
          label="Programs"
          columns={columns}
          rows={rows}
          getKey={(program) => program.hash}
          selectedKey={selection?.kind === "program" ? selection.hash : null}
          onSelect={(hash) => select({ kind: "program", hash })}
          onAction={(hash) => openDoc({ kind: "program", hash })}
          empty={
            <EmptyState
              icon={Icons.program}
              title="No programs"
              body="Import a .wasm program to start sessions from it."
            />
          }
        />
      </div>
      <ConfirmDialog
        title="Remove program"
        body={
          removing && (
            <>
              Remove <b>{removing.display_name}</b>{" "}
              <span className="font-mono text-subtle">{removing.name}</span> from{" "}
              {removing.hosts.join(", ")}?
            </>
          )
        }
        confirmLabel="Remove"
        danger
        isOpen={removing !== null}
        onOpenChange={(open) => {
          if (!open) setRemoving(null);
        }}
        onConfirm={() => removing && void confirmRemove(removing)}
      />
    </div>
  );
}
