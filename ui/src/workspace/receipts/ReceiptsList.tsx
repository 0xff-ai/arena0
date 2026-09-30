import { useNavigate } from "@tanstack/react-router";
import { receiptsRoute } from "~/app/router";
import { programName, shortHash, useRows, useSessions } from "~/model";
import { type ReceiptRow, useCollections } from "~/sync";
import { Badge, type Column, DataTable, EmptyState, HashChip, Icons, Select, Toolbar } from "~/ui";
import { ImportReceiptButton } from "../actions/imports";
import { useOpenDoc } from "../nav";
import { useSelection } from "../selection";

const ALL = "*";

export function ReceiptsList() {
  const search = receiptsRoute.useSearch();
  const navigate = useNavigate({ from: receiptsRoute.fullPath });
  const collections = useCollections();
  const receipts = useRows(collections.receipts);
  const programs = useRows(collections.programs);
  const hosts = useRows(collections.hosts);
  const { sessions } = useSessions();
  const [selection, select] = useSelection();
  const openDoc = useOpenDoc();

  // Newest sessions first, then by Host; a receipt has no time of its own.
  const updated = new Map(
    sessions.flatMap((session) =>
      session.sessionId === null ? [] : [[session.sessionId, session.updatedMs] as const],
    ),
  );
  const rows = receipts
    .filter((receipt) => search.host === undefined || receipt.host === search.host)
    .sort(
      (a, b) =>
        (updated.get(b.session_id) ?? 0) - (updated.get(a.session_id) ?? 0) ||
        (a.host < b.host ? -1 : a.host > b.host ? 1 : 0),
    );
  const byKey = new Map(rows.map((row) => [row.key, row]));

  const columns: Column<ReceiptRow>[] = [
    {
      id: "kind",
      title: "",
      width: 28,
      minWidth: 28,
      render: (receipt) => (
        <span
          role="img"
          aria-label={receipt.kind === "receipt" ? "receipt" : "stop report"}
          className={receipt.kind === "receipt" ? "text-muted" : "text-warn"}
        >
          {receipt.kind === "receipt" ? (
            <Icons.receipt size={13} />
          ) : (
            <Icons.stopReport size={13} />
          )}
        </span>
      ),
    },
    {
      id: "id",
      title: "Receipt",
      width: 150,
      minWidth: 150,
      isRowHeader: true,
      render: (receipt) => <HashChip hash={receipt.receipt_id} />,
    },
    {
      id: "program",
      title: "Program",
      width: "1fr",
      minWidth: 140,
      render: (receipt) => (
        <span className="truncate">{programName(programs, receipt.program)}</span>
      ),
    },
    {
      id: "session",
      title: "Session",
      width: 96,
      minWidth: 96,
      render: (receipt) => (
        <span className="font-mono text-xs text-subtle">{shortHash(receipt.session_id)}</span>
      ),
    },
    { id: "host", title: "Host", width: 90, minWidth: 90, render: (receipt) => receipt.host },
    {
      id: "provenance",
      title: "Provenance",
      width: 100,
      minWidth: 100,
      render: (receipt) => (
        <Badge tone={receipt.provenance === "imported" ? "info" : "neutral"}>
          {receipt.provenance}
        </Badge>
      ),
    },
    {
      id: "completed",
      title: "Completed",
      width: 112,
      minWidth: 112,
      render: (receipt) =>
        receipt.completed ? (
          <Badge tone="done" icon={Icons.check}>
            completed
          </Badge>
        ) : (
          <Badge tone="warn">stopped</Badge>
        ),
    },
  ];

  return (
    <div className="flex h-full min-h-0 flex-col">
      <Toolbar aria-label="Receipts">
        <span className="text-base text-fg">Receipts</span>
        <span className="font-mono text-xs text-subtle tabular">{rows.length}</span>
        <span className="min-w-2 flex-1" />
        <div className="w-36">
          <Select
            placeholder="Filter by Host"
            value={search.host ?? ALL}
            onChange={(id) =>
              void navigate({ search: (prev) => ({ ...prev, host: id === ALL ? undefined : id }) })
            }
            items={[
              { id: ALL, label: "All Hosts" },
              ...hosts.map((host) => ({ id: host.id, label: host.id })),
            ]}
          />
        </div>
        <ImportReceiptButton />
      </Toolbar>
      <div className="min-h-0 flex-1">
        <DataTable
          label="Receipts"
          columns={columns}
          rows={rows}
          getKey={(receipt) => receipt.key}
          selectedKey={selection?.kind === "receipt" ? receiptKey(selection, receipts) : null}
          onSelect={(key) => {
            const receipt = byKey.get(key);
            if (receipt) select({ kind: "receipt", host: receipt.host, id: receipt.receipt_id });
          }}
          onAction={(key) => {
            const receipt = byKey.get(key);
            if (receipt) openDoc({ kind: "receipt", host: receipt.host, id: receipt.receipt_id });
          }}
          empty={
            <EmptyState
              icon={Icons.receipt}
              title="No receipts"
              body="A receipt is kept on each Host when a session completes. Import one to verify it here."
            />
          }
        />
      </div>
    </div>
  );
}

function receiptKey(
  selection: { host: string; id: string },
  receipts: ReceiptRow[],
): string | null {
  return (
    receipts.find(
      (receipt) => receipt.host === selection.host && receipt.receipt_id === selection.id,
    )?.key ?? null
  );
}
