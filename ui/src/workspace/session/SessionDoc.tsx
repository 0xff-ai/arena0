import { useNavigate } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { Link } from "react-aria-components";
import type { UpdateSearch } from "~/app/router";
import { sessionRoute } from "~/app/router";
import {
  fmtOutcome,
  type Session,
  shortHash,
  useFlags,
  useRows,
  useSession,
  useSessions,
  useYourHosts,
} from "~/model";
import { type DaemonError, useCall, useCollections } from "~/sync";
import {
  Button,
  ConfirmDialog,
  EmptyState,
  HashChip,
  Icons,
  JsonView,
  KeyValue,
  MenuButton,
  MenuItem,
  SubTabs,
  toasts,
} from "~/ui";
import { FlagHint } from "../common/FlagHint";
import { participantChip, shortSessionKey } from "../common/SessionLabel";
import { StateWord } from "../common/StateWord";
import { useComposer } from "../composer/store";
import { type SessionSub, sessionHref } from "../nav";
import { useSelection } from "../selection";
import { EvidencePane } from "./EvidencePane";
import { NegotiationPane } from "./NegotiationPane";
import { QueryPane } from "./QueryPane";
import { Steps } from "./Steps";
import { ViewPane } from "./ViewPane";

const SUBS = [
  { id: "steps", label: "Steps", icon: Icons.steps },
  { id: "view", label: "View", icon: Icons.view },
  { id: "negotiation", label: "Negotiation", icon: Icons.negotiation },
  { id: "query", label: "Query", icon: Icons.query },
  { id: "evidence", label: "Evidence", icon: Icons.evidence },
] satisfies { id: SessionSub; label: string; icon: typeof Icons.steps }[];

function FrontMatter(props: { session: Session }) {
  const { session } = props;
  const creator = session.participants.find((p) => p.peerId === session.creator);
  const writer = session.participants.find((p) => p.peerId === session.writer);
  const left = [
    {
      k: "program",
      v: (
        <span className="inline-flex items-center gap-2">
          {session.program?.display_name ?? "unknown program"}
          <HashChip hash={session.programHash} copy />
        </span>
      ),
    },
    {
      k: "session id",
      v:
        session.sessionId === null ? (
          <span className="text-subtle">not activated</span>
        ) : (
          <HashChip hash={session.sessionId} copy />
        ),
    },
    {
      k: "negotiation id",
      v:
        session.negotiationId === null ? (
          <span className="text-subtle">none</span>
        ) : (
          <HashChip hash={session.negotiationId} copy />
        ),
    },
    {
      k: "creator",
      v: creator ? (
        participantChip(session, creator.index)
      ) : (
        <span className="text-subtle">unknown</span>
      ),
    },
    {
      k: "target size",
      v: session.targetSize === null ? "—" : String(session.targetSize),
      mono: true,
    },
    {
      k: "participants",
      v: (
        <div className="flex flex-col gap-1">
          {session.participants.map((participant) => (
            <div key={participant.index} className="flex items-center gap-2">
              {participantChip(session, participant.index)}
              {participant.peerId !== "" && <HashChip hash={participant.peerId} />}
              <span className="text-muted">{participant.host ?? "remote"}</span>
              {participant.userAgent && (
                <span className="truncate text-subtle">{participant.userAgent}</span>
              )}
            </div>
          ))}
        </div>
      ),
    },
  ];
  const right = [
    {
      k: "offer hash",
      v: session.offerHash === null ? "—" : <HashChip hash={session.offerHash} copy />,
    },
    {
      k: "initial state",
      v: session.initialState === null ? "—" : <HashChip hash={session.initialState} />,
    },
    { k: "phase", v: session.phase ?? "—" },
    { k: "writer", v: writer ? participantChip(session, writer.index) : "—" },
    {
      k: "receipts",
      v:
        session.receipts.length === 0
          ? "none"
          : session.receipts
              .map((r) => `${r.kind === "receipt" ? "receipt" : "stop report"} on ${r.host}`)
              .join(", "),
    },
    {
      k: "params",
      v: session.params === null ? "—" : <JsonView value={session.params} collapseDepth={1} />,
    },
  ];
  return (
    <div className="grid gap-x-10 gap-y-1 border-b border-line-soft px-3 py-2 min-[1100px]:grid-cols-2">
      <KeyValue items={left} />
      <KeyValue items={right} />
    </div>
  );
}

function Actions(props: { session: Session }) {
  const { session } = props;
  const composer = useComposer();
  const hosts = useRows(useCollections().hosts).map((host) => host.id);
  const [yourHosts] = useYourHosts(hosts);
  const withdraw = useCall("withdraw");
  const terminate = useCall("terminate");
  const [confirm, setConfirm] = useState<"terminate" | "withdraw" | null>(null);
  const mine = session.callouts.find((callout) => yourHosts.has(callout.host));

  // Each local Host runs its own execution, so ending the session is one call per Host that is still going.
  const live = session.executions.filter((e) => e.terminal === null);
  function stop(op: typeof withdraw | typeof terminate, verb: string) {
    for (const execution of live) {
      op.mutate(
        { host: execution.host, exec_id: execution.exec_id },
        {
          onError: (error: DaemonError) =>
            toasts.show({
              tone: "bad",
              title: `${verb} failed on ${execution.host}`,
              body: error.message,
            }),
        },
      );
    }
  }

  return (
    <div className="flex items-center gap-1.5">
      {mine && (
        <Button variant="primary" icon={Icons.callout} onPress={() => composer.open(mine.key)}>
          Answer
        </Button>
      )}
      <MenuButton label="More" icon={Icons.more}>
        <MenuItem
          icon={Icons.link}
          onAction={() =>
            navigator.clipboard
              .writeText(new URL(sessionHref(session.key), window.location.href).toString())
              .then(() => toasts.show({ tone: "ok", title: "Link copied" }))
          }
        >
          Copy link
        </MenuItem>
        {session.state === "negotiating" && (
          <MenuItem icon={Icons.remove} onAction={() => setConfirm("withdraw")}>
            Withdraw
          </MenuItem>
        )}
        {live.length > 0 && (
          <MenuItem icon={Icons.stop} danger onAction={() => setConfirm("terminate")}>
            Terminate
          </MenuItem>
        )}
      </MenuButton>
      <ConfirmDialog
        title={confirm === "withdraw" ? "Withdraw this negotiation?" : "Terminate this session?"}
        body={
          confirm === "withdraw"
            ? `Withdraws the offer on ${live.map((e) => e.host).join(", ")}.`
            : `Stops the execution on ${live.map((e) => e.host).join(", ")}. The session ends as aborted; this cannot be undone.`
        }
        confirmLabel={confirm === "withdraw" ? "Withdraw" : "Terminate"}
        danger
        isOpen={confirm !== null}
        onOpenChange={(open) => !open && setConfirm(null)}
        onConfirm={() =>
          confirm === "withdraw" ? stop(withdraw, "Withdraw") : stop(terminate, "Terminate")
        }
      />
    </div>
  );
}

export function SessionDoc() {
  const { key } = sessionRoute.useParams();
  const search = sessionRoute.useSearch();
  const navigate = useNavigate();
  const session = useSession(key);
  const { sessions } = useSessions();
  const flags = useFlags(sessions).get(key) ?? [];
  const [, select] = useSelection();
  const sub = search.sub ?? "steps";

  const update: UpdateSearch = (patch) => {
    void navigate({
      to: "/s/$key",
      params: { key },
      search: (previous) => ({ ...previous, ...patch }),
      replace: true,
    });
  };

  // The inspector follows the document: the chosen step, else the session.
  const step = search.step;
  useEffect(() => {
    select(
      step === undefined
        ? { kind: "session", key }
        : { kind: "step", sessionKey: key, step, host: null },
    );
  }, [key, step, select]);

  if (session === null) {
    return (
      <EmptyState
        icon={Icons.session}
        title="This session no longer exists"
        body="It was removed, or its Hosts have not reported it."
        action={
          <Link
            href="/sessions"
            className="text-accent underline outline-none focus-visible:outline-1 focus-visible:outline-accent"
          >
            Back to sessions
          </Link>
        }
      />
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col overflow-auto">
      <header className="flex shrink-0 items-center gap-3 border-b border-line-soft px-3 py-2">
        <h2 className="m-0 min-w-0 truncate text-md font-medium">
          {session.program?.display_name ?? shortHash(session.programHash)}
        </h2>
        <span className="shrink-0 font-mono text-xs text-subtle">
          {shortSessionKey(session.key)}
        </span>
        <StateWord state={session.state} />
        {session.terminal?.outcome != null && (
          <span data-testid="session-result" className="min-w-0 truncate text-sm text-fg">
            {fmtOutcome(session.terminal.outcome)}
          </span>
        )}
        <div className="flex min-w-0 items-center gap-3">
          {flags.map((flag) => (
            <FlagHint key={flag.kind} flag={flag} />
          ))}
        </div>
        <div className="flex-1" />
        <Actions session={session} />
      </header>
      <FrontMatter session={session} />
      <SubTabs
        label="Session sections"
        items={SUBS}
        value={sub}
        onChange={(next) => update({ sub: next })}
      />
      <div className="relative min-h-96 flex-1">
        <div className="absolute inset-0 overflow-auto">
          {sub === "steps" && <Steps session={session} step={search.step} update={update} />}
          {sub === "view" && (
            <ViewPane
              session={session}
              host={search.host}
              step={search.step}
              compare={search.compare}
              update={update}
            />
          )}
          {sub === "negotiation" && <NegotiationPane session={session} />}
          {sub === "query" && <QueryPane session={session} host={search.host} update={update} />}
          {sub === "evidence" && <EvidencePane session={session} />}
        </div>
      </div>
    </div>
  );
}
