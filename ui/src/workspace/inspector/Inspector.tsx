import { useRouter } from "@tanstack/react-router";
import type { ReactNode } from "react";
import {
  endResult,
  fmtBytes,
  fmtClock,
  fmtDuration,
  type Session,
  shortHash,
  useHostCounts,
  useNow,
  useRows,
  useSession,
  useSessionSteps,
  useYourHosts,
} from "~/model";
import { useCollections } from "~/sync";
import {
  AgreementMeter,
  Badge,
  Button,
  EmptyState,
  Fingerprint,
  HashChip,
  Icons,
  JsonView,
  Kbd,
  KeyValue,
  List,
  ParticipantChip,
  ParticipantDot,
  type Tone,
} from "~/ui";
import { SessionLabel } from "../common/SessionLabel";
import { StateWord } from "../common/StateWord";
import { useComposer } from "../composer/store";
import { VerifyCard } from "../evidence/VerifyCard";
import { programHref, useOpenDoc } from "../nav";
import { type Selection, useSelection } from "../selection";

function Group(props: { title: string; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-1.5 border-b border-line-soft px-3 py-2.5">
      <h3 className="m-0 text-xs font-medium tracking-wide text-subtle uppercase">{props.title}</h3>
      {props.children}
    </section>
  );
}

function chip(session: Session, index: number, label?: string) {
  const p = session.participants[index];
  return (
    <ParticipantChip index={index} label={label ?? p?.host ?? shortHash(p?.peerId ?? "", 6)} />
  );
}

const LIFECYCLE_DOT = {
  negotiating: "idle",
  activating: "idle",
  active: "active",
  completed: "done",
  aborted: "bad",
  failed: "bad",
} as const;

function SessionInspector(props: { sessionKey: string }) {
  const session = useSession(props.sessionKey);
  const steps = useSessionSteps(session);
  const now = useNow();
  const composer = useComposer();
  const openDoc = useOpenDoc();
  const hosts = useRows(useCollections().hosts).map((host) => host.id);
  const [yourHosts] = useYourHosts(hosts);
  if (session === null)
    return <EmptyState icon={Icons.session} title="This session no longer exists" />;

  const latest = steps.filter((row) => row.step === session.latestStep);
  const first = latest[0];
  const diverged = new Set(latest.map((row) => row.post_state)).size > 1;
  const mine = session.callouts.find((callout) => yourHosts.has(callout.host));
  const writer = session.participants.find((p) => p.peerId === session.writer);

  return (
    <>
      <Group title="Session">
        <div className="flex items-center gap-2">
          <SessionLabel session={session} />
          <StateWord state={session.state} />
        </div>
        <KeyValue
          dense
          items={[
            {
              k: "step",
              v: session.latestStep === null ? "—" : String(session.latestStep),
              mono: true,
            },
            { k: "phase", v: session.phase ?? "—" },
          ]}
        />
      </Group>
      <Group title="Waiting on">
        {session.callouts.length > 0 ? (
          session.callouts.map((callout) => {
            const p = session.participants.find((candidate) => candidate.host === callout.host);
            return (
              <div key={callout.key} className="flex items-center gap-2 text-sm">
                {p && chip(session, p.index)}
                <span className="min-w-0 truncate text-fg">{callout.name}</span>
                <span className="font-mono text-xs text-muted tabular">
                  {fmtDuration(now - callout.opened_ms)}
                </span>
              </div>
            );
          })
        ) : writer ? (
          <div className="flex items-center gap-2 text-sm text-muted">
            {chip(session, writer.index)} to write
          </div>
        ) : (
          <span className="text-sm text-subtle">Nothing.</span>
        )}
      </Group>
      <Group title="Participants">
        {session.participants.map((participant) => (
          <div key={participant.index} className="flex items-center gap-2 text-sm">
            {chip(session, participant.index)}
            {participant.execution ? (
              <>
                <ParticipantDot
                  index={participant.index}
                  state={LIFECYCLE_DOT[participant.execution.lifecycle]}
                  you={participant.host !== null && yourHosts.has(participant.host)}
                />
                <span className="text-muted">{participant.execution.lifecycle}</span>
              </>
            ) : (
              <span className="text-subtle">remote</span>
            )}
          </div>
        ))}
      </Group>
      {first && (
        <Group title={`Step ${first.step}`}>
          <div className="flex items-center gap-2 text-sm">
            <AgreementMeter signers={first.signers} participants={first.participants} />
            <span className="text-muted">
              {first.signers.length} of {first.participants} signed
            </span>
            <Badge tone={diverged ? "bad" : "ok"}>{diverged ? "diverged" : "equal"}</Badge>
          </div>
          {latest.map((row) => (
            <div key={row.host} className="flex items-center gap-2 font-mono text-xs text-muted">
              <span className="w-14 shrink-0">{row.host}</span>
              <Fingerprint hash={row.post_state} size="sm" />
              {shortHash(row.post_state, 8)}
            </div>
          ))}
        </Group>
      )}
      <Group title="Evidence">
        {session.receipts.length === 0 ? (
          <span className="text-sm text-subtle">Published when the final step is certified.</span>
        ) : (
          session.receipts.map((receipt) => (
            <div key={receipt.key} className="flex items-center gap-2 text-sm">
              {receipt.kind === "receipt" ? (
                <Icons.receipt size={12} aria-hidden />
              ) : (
                <Icons.stopReport size={12} className="text-warn" aria-hidden />
              )}
              <span>{receipt.kind === "receipt" ? "receipt" : "stop report"}</span>
              <HashChip hash={receipt.receipt_id} />
              <span className="text-subtle">{receipt.host}</span>
            </div>
          ))
        )}
      </Group>
      <Group title="Actions">
        <div className="flex flex-wrap gap-1.5">
          <Button
            size="sm"
            onPress={() => openDoc({ kind: "session", key: session.key }, { pin: true })}
          >
            Open
          </Button>
          {mine && (
            <Button size="sm" variant="primary" onPress={() => composer.open(mine.key)}>
              Answer
            </Button>
          )}
        </div>
      </Group>
    </>
  );
}

function StepInspector(props: { sessionKey: string; step: number; host: string | null }) {
  const session = useSession(props.sessionKey);
  const all = useSessionSteps(session);
  if (session === null)
    return <EmptyState icon={Icons.session} title="This session no longer exists" />;
  const rows = all.filter(
    (row) => row.step === props.step && (props.host === null || row.host === props.host),
  );
  const first = rows[0];
  if (first === undefined)
    return <EmptyState icon={Icons.step} title={`Step ${props.step} is not certified yet`} />;
  const author =
    first.event.kind === "message"
      ? session.participants.find(
          (p) => p.peerId === (first.event.kind === "message" ? first.event.from : ""),
        )
      : undefined;
  return (
    <>
      <Group title={`Step ${first.step}`}>
        <KeyValue
          dense
          items={[
            { k: "session", v: <SessionLabel session={session} /> },
            { k: "author", v: author ? chip(session, author.index) : "—" },
            {
              k: "event",
              v:
                first.event.kind === "session_started"
                  ? `session started · ${first.event.ensemble.length} participants`
                  : `message · ${fmtBytes(first.event.bytes)}`,
            },
            {
              k: "signers",
              v: (
                <span className="inline-flex items-center gap-2">
                  <AgreementMeter signers={first.signers} participants={first.participants} />
                  {first.signers.map((s) => `P${s}`).join(" ")}
                </span>
              ),
            },
          ]}
        />
      </Group>
      {first.event.kind === "message" && (
        <Group title="Event">
          {first.event.decode_error !== null ? (
            <span className="text-sm text-bad">
              decode error: {first.event.decode_error} ({fmtBytes(first.event.bytes)})
            </span>
          ) : first.event.decoded === null ? (
            <span className="text-sm text-subtle">No decoded payload.</span>
          ) : (
            <JsonView value={first.event.decoded} collapseDepth={3} />
          )}
        </Group>
      )}
      {first.terminal && (
        <Group title="Terminal">
          <span className="text-sm">
            {first.terminal.kind === "end"
              ? `end · ${endResult(session, first.terminal.outcome_bytes)}`
              : `${first.terminal.kind} · ${first.terminal.reason}`}
          </span>
        </Group>
      )}
      <Group title="Per Host">
        {rows.map((row) => (
          <div key={row.host} className="flex flex-col gap-0.5 text-xs">
            <div className="font-mono text-sm text-fg">{row.host}</div>
            <div className="flex items-center gap-2 text-muted">
              <span className="w-8 text-subtle">pre</span>
              <HashChip hash={row.pre_state} />
            </div>
            <div className="flex items-center gap-2 text-muted">
              <span className="w-8 text-subtle">post</span>
              <HashChip hash={row.post_state} />
            </div>
            <div className="text-subtle">certified {fmtClock(row.certified_ms)}</div>
          </div>
        ))}
      </Group>
    </>
  );
}

function HostInspector(props: { id: string }) {
  const hostCounts = useHostCounts();
  const collections = useCollections();
  const host = useRows(collections.hosts).find((row) => row.id === props.id);
  const blobs = useRows(collections.blobs).filter((blob) => blob.host === props.id);
  if (host === undefined)
    return <EmptyState icon={Icons.host} title="This Host no longer exists" />;
  return (
    <>
      <Group title="Host">
        <KeyValue
          dense
          items={[
            { k: "id", v: host.id, mono: true },
            { k: "peer", v: <HashChip hash={host.peer_id} copy /> },
            { k: "user agent", v: host.user_agent ?? "—", title: host.user_agent ?? undefined },
            {
              k: "online",
              v: (
                <Badge tone={host.online ? "ok" : "bad"}>
                  {host.online ? "online" : "offline"}
                </Badge>
              ),
            },
            { k: "programs", v: String(hostCounts.get(host.id)?.programs ?? 0), mono: true },
            { k: "active executions", v: String(hostCounts.get(host.id)?.live ?? 0), mono: true },
            {
              k: "gaps",
              v:
                host.gaps === 0
                  ? "none"
                  : `${host.gaps}${host.last_gap_ms === null ? "" : `, last ${fmtClock(host.last_gap_ms)}`}`,
              mono: true,
            },
          ]}
        />
      </Group>
      <Group title={`Blobs · ${blobs.length}`}>
        <div className="h-40">
          <List
            label={`Blobs on ${host.id}`}
            items={blobs}
            getKey={(blob) => blob.key}
            empty={<span className="px-2 text-sm text-subtle">No blobs.</span>}
          >
            {(blob) => (
              <span className="flex min-w-0 items-center gap-2">
                <HashChip hash={blob.hash} />
                <span className="font-mono text-xs text-muted tabular">
                  {fmtBytes(blob.length)}
                </span>
              </span>
            )}
          </List>
        </div>
      </Group>
    </>
  );
}

function ProgramInspector(props: { hash: string }) {
  const program = useRows(useCollections().programs).find((row) => row.hash === props.hash);
  const router = useRouter();
  if (program === undefined)
    return <EmptyState icon={Icons.program} title="This program no longer exists" />;
  const { schema } = program;
  return (
    <>
      <Group title="Program">
        <div className="text-base font-medium">{program.display_name}</div>
        <p className="m-0 text-sm text-muted">{program.description}</p>
        <KeyValue
          dense
          items={[
            { k: "version", v: program.version, mono: true },
            { k: "hash", v: <HashChip hash={program.hash} copy /> },
            {
              k: "participants",
              v:
                program.participants.min === program.participants.max
                  ? String(program.participants.min)
                  : `${program.participants.min}–${program.participants.max}`,
              mono: true,
            },
            { k: "Hosts", v: program.hosts.join(", ") },
          ]}
        />
        <div>
          <Button
            size="sm"
            variant="primary"
            icon={Icons.add}
            onPress={() => router.history.push(programHref(program.hash, true))}
          >
            New session
          </Button>
        </div>
      </Group>
      <Group title={`Callouts · ${schema.callouts.length}`}>
        {schema.callouts.map((callout) => (
          <div key={callout.name} className="text-sm">
            <span className="font-mono text-fg">{callout.name}</span>
            <span className="text-muted"> · {callout.prompt}</span>
          </div>
        ))}
      </Group>
      <Group title={`Queries · ${schema.queries.length}`}>
        {schema.queries.length === 0 && <span className="text-sm text-subtle">None.</span>}
        {schema.queries.map((query) => (
          <div key={query.name} className="text-sm">
            <span className="font-mono text-fg">{query.name}</span>
            <span className="text-muted"> · {query.label}</span>
          </div>
        ))}
      </Group>
      <Group title={`Phases · ${schema.phases.length}`}>
        {schema.phases.length === 0 && <span className="text-sm text-subtle">None.</span>}
        {schema.phases.map((phase) => (
          <div key={phase.name} className="flex flex-wrap items-center gap-1.5 text-sm">
            <span className="font-mono text-fg">{phase.name}</span>
            {phase.is_default && <Badge tone="info">default</Badge>}
            {phase.is_terminal && <Badge tone="done">terminal</Badge>}
            <span className="text-muted">{phase.description}</span>
          </div>
        ))}
      </Group>
    </>
  );
}

function ReceiptInspector(props: { host: string; id: string }) {
  const receipt = useRows(useCollections().receipts).find(
    (row) => row.host === props.host && row.receipt_id === props.id,
  );
  if (receipt === undefined)
    return <EmptyState icon={Icons.receipt} title="This receipt no longer exists" />;
  const tone: Tone = receipt.kind === "receipt" ? "done" : "warn";
  return (
    <>
      <Group title={receipt.kind === "receipt" ? "Receipt" : "Stop report"}>
        <KeyValue
          dense
          items={[
            { k: "id", v: <HashChip hash={receipt.receipt_id} copy /> },
            {
              k: "kind",
              v: (
                <Badge tone={tone}>{receipt.kind === "receipt" ? "receipt" : "stop report"}</Badge>
              ),
            },
            { k: "Host", v: receipt.host, mono: true },
            { k: "program", v: <HashChip hash={receipt.program} /> },
            { k: "session", v: <HashChip hash={receipt.session_id} /> },
            { k: "provenance", v: receipt.provenance },
            { k: "completed", v: receipt.completed ? "yes" : "no" },
          ]}
        />
      </Group>
      <Group title="Verify">
        <VerifyCard
          host={receipt.host}
          target={{ kind: "stored", receipt_id: receipt.receipt_id }}
          expect={{ program: receipt.program, sessionId: receipt.session_id }}
        />
      </Group>
    </>
  );
}

const SHORTCUTS: { keys: string; what: string }[] = [
  { keys: "Mod+K", what: "Find anything" },
  { keys: "Mod+.", what: "Toggle the inspector" },
  { keys: "Mod+W", what: "Close the document" },
  { keys: "G S", what: "Go to Sessions" },
  { keys: "G O", what: "Go to Offers" },
  { keys: "G R", what: "Go to Receipts" },
  { keys: "G P", what: "Go to Programs" },
  { keys: "F8", what: "Next problem" },
  { keys: "Shift+F8", what: "Previous problem" },
  { keys: "Mod+Enter", what: "Submit an answer or a query" },
  { keys: "Esc", what: "Close the composer" },
];

function Shortcuts() {
  return (
    <Group title="Keyboard">
      {SHORTCUTS.map((shortcut) => (
        <div key={shortcut.keys} className="flex items-center gap-2 text-sm">
          <span className="min-w-0 flex-1 text-muted">{shortcut.what}</span>
          <Kbd keys={shortcut.keys} />
        </div>
      ))}
    </Group>
  );
}

function body(selection: Selection) {
  switch (selection?.kind) {
    case "session":
      return <SessionInspector sessionKey={selection.key} />;
    case "step":
      return (
        <StepInspector
          sessionKey={selection.sessionKey}
          step={selection.step}
          host={selection.host}
        />
      );
    case "host":
      return <HostInspector id={selection.id} />;
    case "program":
      return <ProgramInspector hash={selection.hash} />;
    case "receipt":
      return <ReceiptInspector host={selection.host} id={selection.id} />;
    case undefined:
      return <Shortcuts />;
  }
}

/** What the selection is, in detail; the selection itself is set by the lists and documents. */
export function Inspector() {
  const [selection] = useSelection();
  return (
    <div aria-label="Inspector" role="complementary" className="h-full overflow-auto">
      {body(selection)}
    </div>
  );
}
