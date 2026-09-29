import type { Session } from "~/model";
import { type ReceiptRow, useCall } from "~/sync";
import { Badge, Button, HashChip, Icons } from "~/ui";
import { downloadJson } from "../common/download";
import { VerifyCard } from "../evidence/VerifyCard";

function ExportButton(props: { receipt: ReceiptRow }) {
  const fetch = useCall("receipt");
  return (
    <Button
      size="sm"
      icon={Icons.download}
      isDisabled={fetch.isPending}
      onPress={() =>
        fetch.mutate(
          { host: props.receipt.host, receipt_id: props.receipt.receipt_id },
          {
            onSuccess: (artifact) => downloadJson(props.receipt.receipt_id, artifact),
          },
        )
      }
    >
      Export
    </Button>
  );
}

function Claims(props: { title: string; items: string[] }) {
  return (
    <div className="min-w-0">
      <div className="mb-1 text-xs font-medium tracking-wide text-subtle uppercase">
        {props.title}
      </div>
      <ul className="m-0 flex list-disc flex-col gap-0.5 pl-4 text-sm text-muted">
        {props.items.map((item) => (
          <li key={item}>{item}</li>
        ))}
      </ul>
    </div>
  );
}

const RECEIPT_PROVES = [
  "Every participant agreed on every step (N-of-N signatures).",
  "The program and params the session ran with.",
  "The outcome the program produced.",
];
const RECEIPT_NOT = [
  "That the program is fair.",
  "The participants' private inputs.",
  "Anything after the end.",
];
const STOP_PROVES = ["The signer's own view of the session up to the stop."];
const STOP_NOT = ["That the other participants agree with it."];

function ReceiptCard(props: { session: Session; receipt: ReceiptRow }) {
  const { session, receipt } = props;
  const stop = receipt.kind === "stop_report";
  return (
    <article className="flex flex-col gap-2 rounded-sm border border-line-soft p-2.5">
      <div className="flex items-center gap-2 text-sm">
        {stop ? (
          <Icons.stopReport size={14} className="text-warn" aria-hidden />
        ) : (
          <Icons.receipt size={14} className="text-subtle" aria-hidden />
        )}
        <span className="text-fg">{stop ? "Stop report" : "Receipt"}</span>
        <HashChip hash={receipt.receipt_id} copy />
        <Badge>{receipt.provenance}</Badge>
        <div className="flex-1" />
        <ExportButton receipt={receipt} />
      </div>
      <VerifyCard
        host={receipt.host}
        target={{ kind: "stored", receipt_id: receipt.receipt_id }}
        expect={{
          program: session.programHash,
          ...(session.sessionId === null ? {} : { sessionId: session.sessionId }),
          participants: session.participants.map((p) => p.peerId),
        }}
      />
      <div className="grid gap-4 sm:grid-cols-2">
        <Claims title="What this proves" items={stop ? STOP_PROVES : RECEIPT_PROVES} />
        <Claims title="What it does not prove" items={stop ? STOP_NOT : RECEIPT_NOT} />
      </div>
    </article>
  );
}

export function EvidencePane(props: { session: Session }) {
  const { session } = props;
  return (
    <div className="flex flex-col gap-4 p-3">
      {session.executions.map((execution) => {
        const receipts = session.receipts.filter((r) => r.host === execution.host);
        return (
          <section
            key={execution.key}
            aria-label={`Evidence on ${execution.host}`}
            className="flex flex-col gap-2"
          >
            <h3 className="m-0 font-mono text-base font-medium">{execution.host}</h3>
            {receipts.length === 0 ? (
              <div className="text-sm text-subtle">No receipt on this Host yet.</div>
            ) : (
              receipts.map((receipt) => (
                <ReceiptCard key={receipt.key} session={session} receipt={receipt} />
              ))
            )}
          </section>
        );
      })}
    </div>
  );
}
