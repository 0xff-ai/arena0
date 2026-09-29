import { useRouter } from "@tanstack/react-router";
import { receiptRoute } from "~/app/router";
import { shortHash, useRows } from "~/model";
import { useCollections, useRead } from "~/sync";
import { Badge, Button, EmptyState, FieldError, HashChip, Icons, JsonView, KeyValue } from "~/ui";
import { VerifyCard } from "../evidence/VerifyCard";
import { DocPage, DocSection } from "../program/DocLayout";

/** Saves `artifact` as `<name>.json` through the browser's download. */
function download(name: string, artifact: unknown) {
  const url = URL.createObjectURL(
    new Blob([JSON.stringify(artifact, null, 2)], { type: "application/json" }),
  );
  const link = document.createElement("a");
  link.href = url;
  link.download = `${name}.json`;
  link.click();
  URL.revokeObjectURL(url);
}

export function ReceiptDoc() {
  const { host, id } = receiptRoute.useParams();
  const router = useRouter();
  const collections = useCollections();
  const receipts = useRows(collections.receipts);
  const programs = useRows(collections.programs);
  const artifact = useRead(
    "receipt",
    { host, receipt_id: id },
    { staleTime: Number.POSITIVE_INFINITY },
  );
  const receipt = receipts.find((row) => row.host === host && row.receipt_id === id);

  if (receipt === undefined) {
    return (
      <EmptyState
        icon={Icons.receipt}
        title={`No receipt ${shortHash(id)} on ${host}`}
        action={<Button onPress={() => router.history.push("/receipts")}>Back to receipts</Button>}
      />
    );
  }

  const program = programs.find((row) => row.hash === receipt.program);
  const stop = receipt.kind === "stop_report";

  return (
    <DocPage
      title={stop ? "Stop report" : "Receipt"}
      meta={
        <>
          <HashChip hash={receipt.receipt_id} chars={12} />
          <Badge tone={stop ? "warn" : "done"} icon={stop ? Icons.stopReport : Icons.receipt}>
            {stop ? "stop report" : "receipt"}
          </Badge>
        </>
      }
      actions={
        <Button
          icon={Icons.download}
          isDisabled={artifact.data === undefined}
          onPress={() => artifact.data !== undefined && download(receipt.receipt_id, artifact.data)}
        >
          Export
        </Button>
      }
    >
      <KeyValue
        items={[
          {
            k: "program",
            v: (
              <span className="inline-flex items-center gap-2">
                {program?.display_name ?? "unknown program"}
                <HashChip hash={receipt.program} />
              </span>
            ),
            mono: false,
          },
          { k: "session", v: <HashChip hash={receipt.session_id} /> },
          { k: "held by", v: receipt.host, mono: true },
          { k: "provenance", v: receipt.provenance },
          {
            k: "session ended",
            v: receipt.completed ? "completed" : "stopped before the end",
          },
        ]}
      />
      <DocSection title="Verification">
        <VerifyCard
          host={receipt.host}
          target={{ kind: "stored", receipt_id: receipt.receipt_id }}
          expect={{ program: receipt.program, sessionId: receipt.session_id }}
        />
      </DocSection>
      <DocSection title="Artifact">
        {artifact.error ? (
          <FieldError>{artifact.error.message}</FieldError>
        ) : artifact.data === undefined ? (
          <span className="text-sm text-subtle">Loading…</span>
        ) : (
          <JsonView value={artifact.data} collapseDepth={2} />
        )}
      </DocSection>
    </DocPage>
  );
}
