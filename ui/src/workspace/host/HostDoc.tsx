import { useRouter } from "@tanstack/react-router";
import { useState } from "react";
import { hostRoute } from "~/app/router";
import { fmtAgo, fmtBytes, shortHash, useHostCounts, useNow, useRows } from "~/model";
import { type BlobRow, useCall, useCollections } from "~/sync";
import {
  Badge,
  Button,
  type Column,
  DataTable,
  Dialog,
  EmptyState,
  FieldError,
  HashChip,
  Icons,
  KeyValue,
  Select,
  TextField,
  toasts,
} from "~/ui";
import { DocPage, DocSection } from "../common/DocLayout";
import { useOpenDoc } from "../nav";

export function HostDoc() {
  const hostCounts = useHostCounts();
  const { id } = hostRoute.useParams();
  const router = useRouter();
  const collections = useCollections();
  const hosts = useRows(collections.hosts);
  const programs = useRows(collections.programs);
  const blobs = useRows(collections.blobs);
  const openDoc = useOpenDoc();
  const now = useNow(10_000);
  const host = hosts.find((row) => row.id === id);

  if (host === undefined) {
    return (
      <EmptyState
        icon={Icons.host}
        title={`No Host named ${id}`}
        action={<Button onPress={() => router.history.push("/sessions")}>Back to sessions</Button>}
      />
    );
  }

  const held = programs.filter((program) => program.hosts.includes(host.id));
  const mine = blobs.filter((blob) => blob.host === host.id);

  return (
    <DocPage
      title={host.id}
      meta={
        host.online ? (
          <Badge tone="ok">online</Badge>
        ) : (
          <Badge tone="bad" icon={Icons.error}>
            offline
          </Badge>
        )
      }
      actions={
        <Button
          size="sm"
          onPress={() => router.history.push(`/sessions?host=${encodeURIComponent(host.id)}`)}
        >
          Show sessions on {host.id}
        </Button>
      }
    >
      <KeyValue
        items={[
          { k: "peer id", v: <HashChip hash={host.peer_id} chars={12} /> },
          { k: "user agent", v: host.user_agent ?? "none", title: host.user_agent ?? undefined },
          { k: "transport key", v: <HashChip hash={host.transport_key} chars={12} /> },
          { k: "programs", v: String(hostCounts.get(host.id)?.programs ?? 0), mono: true },
          { k: "active sessions", v: String(hostCounts.get(host.id)?.live ?? 0), mono: true },
          {
            k: "event gaps",
            v:
              host.gaps === 0
                ? "none"
                : `${host.gaps}${host.last_gap_ms === null ? "" : `, last ${fmtAgo(host.last_gap_ms, now)}`}`,
            mono: host.gaps > 0,
          },
        ]}
      />
      <DocSection title="Programs" note={`${held.length}`}>
        {held.length === 0 ? (
          <span className="text-sm text-subtle">No programs in this Host's catalog.</span>
        ) : (
          <ul className="m-0 flex list-none flex-col p-0">
            {held.map((program) => (
              <li key={program.hash} className="flex h-6.5 items-center gap-2 text-sm">
                <Button
                  variant="ghost"
                  size="sm"
                  className="font-normal"
                  onPress={() => openDoc({ kind: "program", hash: program.hash })}
                >
                  {program.display_name}
                </Button>
                <span className="font-mono text-xs text-subtle">v{program.version}</span>
                <span className="font-mono text-xs text-faint">{shortHash(program.hash)}</span>
              </li>
            ))}
          </ul>
        )}
      </DocSection>
      <Blobs host={host.id} blobs={mine} />
    </DocPage>
  );
}

function Blobs(props: { host: string; blobs: BlobRow[] }) {
  const [importing, setImporting] = useState(false);
  const [exporting, setExporting] = useState(false);
  const columns: Column<BlobRow>[] = [
    {
      id: "hash",
      title: "Hash",
      width: 130,
      isRowHeader: true,
      render: (blob) => <HashChip hash={blob.hash} />,
    },
    {
      id: "length",
      title: "Length",
      width: 90,
      align: "end",
      render: (blob) => <span className="font-mono tabular">{fmtBytes(blob.length)}</span>,
    },
    {
      id: "path",
      title: "Path",
      width: "1fr",
      render: (blob) => (
        <span className="truncate font-mono text-xs text-muted" title={blob.path}>
          {blob.path}
        </span>
      ),
    },
  ];
  return (
    <DocSection title="Blobs" note={`${props.blobs.length}`}>
      <div className="flex gap-2">
        <Button size="sm" icon={Icons.upload} onPress={() => setImporting(true)}>
          Import blob
        </Button>
        <Button
          size="sm"
          icon={Icons.download}
          isDisabled={props.blobs.length === 0}
          onPress={() => setExporting(true)}
        >
          Export blob
        </Button>
      </div>
      {props.blobs.length === 0 ? (
        <EmptyState
          icon={Icons.blob}
          title={`No blobs on ${props.host}`}
          body="A blob is a file this Host links by hash, so a program can refer to it. Import one by its path on this machine."
        />
      ) : (
        <div
          style={{ height: 24 + 26 * Math.min(props.blobs.length, 10) }}
          className="rounded-sm border border-line-soft"
        >
          <DataTable
            label={`Blobs on ${props.host}`}
            columns={columns}
            rows={props.blobs}
            getKey={(blob) => blob.key}
            empty={null}
          />
        </div>
      )}
      <ImportDialog host={props.host} isOpen={importing} onOpenChange={setImporting} />
      <ExportDialog
        host={props.host}
        blobs={props.blobs}
        isOpen={exporting}
        onOpenChange={setExporting}
      />
    </DocSection>
  );
}

function ImportDialog(props: {
  host: string;
  isOpen: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const [path, setPath] = useState("");
  const call = useCall("blob_import");
  const submit = () =>
    call.mutate(
      { host: props.host, path: path.trim() },
      {
        onSuccess: (reply) => {
          toasts.show({
            tone: "ok",
            title: "Blob imported",
            body: `${shortHash(reply.hash)} · ${fmtBytes(reply.length)}`,
          });
          props.onOpenChange(false);
          setPath("");
        },
      },
    );
  return (
    <Dialog
      title={`Import a blob into ${props.host}`}
      isOpen={props.isOpen}
      onOpenChange={props.onOpenChange}
      footer={
        <Button
          variant="primary"
          isDisabled={path.trim() === "" || call.isPending}
          onPress={submit}
        >
          Import
        </Button>
      }
    >
      <div className="flex flex-col gap-2">
        <TextField
          label="File path"
          description="A file on the machine that runs arena0d. The Host links it; it is not copied."
          mono
          autoFocus
          value={path}
          onChange={setPath}
          onKeyDown={(event) => {
            if (event.key === "Enter" && path.trim() !== "") submit();
          }}
        />
        {call.error && <FieldError>{call.error.message}</FieldError>}
      </div>
    </Dialog>
  );
}

function ExportDialog(props: {
  host: string;
  blobs: BlobRow[];
  isOpen: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const [hash, setHash] = useState<string | null>(null);
  const [path, setPath] = useState("");
  const call = useCall("blob_export");
  const chosen = hash ?? props.blobs[0]?.hash ?? null;
  const submit = () => {
    if (chosen === null) return;
    call.mutate(
      { host: props.host, hash: chosen, path: path.trim() },
      {
        onSuccess: (reply) => {
          toasts.show({
            tone: "ok",
            title: "Blob exported",
            body: `${fmtBytes(reply.length)} written to ${path.trim()}`,
          });
          props.onOpenChange(false);
        },
      },
    );
  };
  return (
    <Dialog
      title={`Export a blob from ${props.host}`}
      isOpen={props.isOpen}
      onOpenChange={props.onOpenChange}
      footer={
        <Button
          variant="primary"
          isDisabled={chosen === null || path.trim() === "" || call.isPending}
          onPress={submit}
        >
          Export
        </Button>
      }
    >
      <div className="flex flex-col gap-3">
        <Select
          label="Blob"
          items={props.blobs.map((blob) => ({
            id: blob.hash,
            label: shortHash(blob.hash, 12),
            detail: fmtBytes(blob.length),
          }))}
          value={chosen}
          onChange={setHash}
        />
        <TextField
          label="Destination path"
          description="Written on the machine that runs arena0d."
          mono
          value={path}
          onChange={setPath}
          onKeyDown={(event) => {
            if (event.key === "Enter" && path.trim() !== "") submit();
          }}
        />
        {call.error && <FieldError>{call.error.message}</FieldError>}
      </div>
    </Dialog>
  );
}
