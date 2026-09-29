import { useNavigate } from "@tanstack/react-router";
import type { ReactNode } from "react";
import { Button } from "react-aria-components";
import { fmtAgo, type Session, shortHash, useNow, useRows, useSessions } from "~/model";
import { type HostRow, type ReceiptRow, useCollections } from "~/sync";
import { Badge, IconButton, Icons, Section, Sparkline, Tooltip, Tree, type TreeNode } from "~/ui";
import { AddHostButton, ImportProgramButton, ImportReceiptButton } from "../actions/imports";
import { useOpenDoc } from "../nav";
import { useSelection } from "../selection";

const RECEIPT_LIMIT = 30;
const SPARK_BUCKETS = 12;
const SPARK_BUCKET_MS = 5 * 60_000;
const ALL_RECEIPTS = "all-receipts";

const isLive = (session: Session) =>
  session.state !== "completed" && session.state !== "failed" && session.state !== "aborted";

/** A focusable name for an abbreviated cell; the row itself carries the meaning for a keyboard user. */
function Named(props: { title: string; children: ReactNode }) {
  return (
    <Tooltip title={props.title}>
      <Button className="inline-flex cursor-default items-center rounded-xs outline-none focus-visible:outline-1 focus-visible:outline-offset-1 focus-visible:outline-accent">
        {props.children}
      </Button>
    </Tooltip>
  );
}

export function Explorer() {
  const collections = useCollections();
  const hosts = useRows(collections.hosts);
  const programs = useRows(collections.programs);
  const receipts = useRows(collections.receipts);
  const steps = useRows(collections.steps);
  const executions = useRows(collections.executions);
  const { sessions } = useSessions();
  const now = useNow(30_000);
  const [selection, select] = useSelection();
  const open = useOpenDoc();
  const navigate = useNavigate();

  const liveByProgram = new Map<string, number>();
  for (const session of sessions) {
    if (isLive(session)) {
      liveByProgram.set(session.programHash, (liveByProgram.get(session.programHash) ?? 0) + 1);
    }
  }
  // `HostRow.execs_active` is a snapshot from the last Host event, so live counts come from the executions.
  const liveByHost = new Map<string, number>();
  for (const execution of executions) {
    if (execution.lifecycle === "completed" || execution.lifecycle === "failed") continue;
    if (execution.lifecycle === "aborted") continue;
    liveByHost.set(execution.host, (liveByHost.get(execution.host) ?? 0) + 1);
  }
  const programName = new Map(programs.map((program) => [program.hash, program.display_name]));

  // Steps per 5 minutes over the last hour, oldest bucket first.
  const stepsByHost = new Map<string, number[]>();
  for (const step of steps) {
    const age = Math.floor((now - step.certified_ms) / SPARK_BUCKET_MS);
    if (age < 0 || age >= SPARK_BUCKETS) continue;
    const buckets = stepsByHost.get(step.host) ?? new Array<number>(SPARK_BUCKETS).fill(0);
    buckets[SPARK_BUCKETS - 1 - age] = (buckets[SPARK_BUCKETS - 1 - age] ?? 0) + 1;
    stepsByHost.set(step.host, buckets);
  }

  const hostNodes = hosts.map((host): TreeNode => {
    const spark = stepsByHost.get(host.id) ?? new Array<number>(SPARK_BUCKETS).fill(0);
    return {
      id: `host:${host.id}`,
      icon: Icons.host,
      label: (
        <>
          {host.id}
          {host.user_agent && <span className="ml-1.5 text-subtle">{host.user_agent}</span>}
        </>
      ),
      title: host.id,
      trailing: (
        <HostTrailing host={host} live={liveByHost.get(host.id) ?? 0} spark={spark} now={now} />
      ),
    };
  });

  const programNodes = programs.map((program): TreeNode => {
    const live = liveByProgram.get(program.hash) ?? 0;
    const { min, max } = program.participants;
    return {
      id: `program:${program.hash}`,
      icon: Icons.program,
      label: (
        <>
          {program.display_name}
          <span className="ml-1.5 text-subtle">
            v{program.version} · {min === max ? min : `${min}–${max}`}
          </span>
        </>
      ),
      title: program.display_name,
      trailing: live > 0 ? <span className="text-xs text-ok">{live} live</span> : undefined,
    };
  });

  // Receipt rows carry no time, so "latest" is the end of the replica's arrival order.
  const latest: ReceiptRow[] = receipts.slice(-RECEIPT_LIMIT).reverse();
  const receiptNodes: TreeNode[] = latest.map((receipt) => ({
    id: `receipt:${receipt.key}`,
    icon: receipt.kind === "stop_report" ? Icons.stopReport : Icons.receipt,
    iconTone: receipt.kind === "stop_report" ? "warn" : undefined,
    label: (
      <>
        {programName.get(receipt.program) ?? shortHash(receipt.program)}
        <span className="ml-1.5 font-mono text-xs text-subtle">
          {shortHash(receipt.receipt_id)}
        </span>
        <span className="ml-1.5 text-xs text-subtle">{receipt.host}</span>
      </>
    ),
    title: `${receipt.kind === "stop_report" ? "Stop report" : "Receipt"} ${receipt.receipt_id} on ${receipt.host}`,
  }));
  receiptNodes.push({
    id: ALL_RECEIPTS,
    title: "All receipts",
    icon: Icons.chevronRight,
    label: <span className="text-muted">All receipts ({receipts.length})</span>,
  });

  const selectedId =
    selection?.kind === "host"
      ? `host:${selection.id}`
      : selection?.kind === "program"
        ? `program:${selection.hash}`
        : selection?.kind === "receipt"
          ? `receipt:${selection.host}/${selection.id}`
          : null;

  return (
    <section aria-label="Explorer" className="h-full overflow-auto py-1">
      <Section
        title="Hosts"
        icon={Icons.host}
        count={hosts.length}
        actions={<AddHostButton compact />}
      >
        <Tree
          label="Hosts"
          items={hostNodes}
          selectedId={selectedId}
          onSelect={(id) => select({ kind: "host", id: id.slice("host:".length) })}
          onAction={(id) => open({ kind: "host", id: id.slice("host:".length) })}
        />
      </Section>
      <Section
        title="Programs"
        icon={Icons.program}
        count={programs.length}
        actions={<ImportProgramButton compact />}
      >
        <Tree
          label="Programs"
          items={programNodes}
          selectedId={selectedId}
          onSelect={(id) => select({ kind: "program", hash: id.slice("program:".length) })}
          onAction={(id) => open({ kind: "program", hash: id.slice("program:".length) })}
        />
      </Section>
      <Section
        title="Receipts"
        icon={Icons.receipt}
        count={receipts.length}
        actions={<ImportReceiptButton compact />}
      >
        <Tree
          label="Receipts"
          items={receiptNodes}
          selectedId={selectedId}
          onSelect={(id) => {
            const receipt = latest.find((r) => `receipt:${r.key}` === id);
            if (receipt) select({ kind: "receipt", host: receipt.host, id: receipt.receipt_id });
          }}
          onAction={(id) => {
            if (id === ALL_RECEIPTS) {
              void navigate({ to: "/receipts" });
              return;
            }
            const receipt = latest.find((r) => `receipt:${r.key}` === id);
            if (receipt) open({ kind: "receipt", host: receipt.host, id: receipt.receipt_id });
          }}
        />
      </Section>
    </section>
  );
}

function HostTrailing(props: { host: HostRow; live: number; spark: number[]; now: number }) {
  const { host } = props;
  const navigate = useNavigate();
  const { live } = props;
  return (
    <>
      {host.gaps > 0 && host.last_gap_ms !== null && (
        <Named
          title={`${host.gaps} observation ${host.gaps === 1 ? "gap" : "gaps"}, last ${fmtAgo(host.last_gap_ms, props.now)}`}
        >
          <Icons.warn size={12} className="text-warn" aria-label="observation gaps" />
        </Named>
      )}
      {!host.online && <Badge tone="bad">offline</Badge>}
      <Named title={`${live} live sessions on ${host.id}`}>
        <Badge tone={live > 0 ? "ok" : "neutral"}>{live} live</Badge>
      </Named>
      <span className="text-subtle">
        <Sparkline values={props.spark} width={36} height={12} />
      </span>
      <span className="opacity-0 in-data-focus-visible:opacity-100 in-data-hovered:opacity-100 focus-within:opacity-100">
        <IconButton
          icon={Icons.session}
          label={`Show sessions on ${host.id}`}
          onPress={() => void navigate({ to: "/sessions", search: { host: host.id } })}
        />
      </span>
    </>
  );
}
