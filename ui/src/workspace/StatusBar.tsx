import { Button } from "react-aria-components";
import { shortHash, useRows, useSeats, useSessions } from "~/model";
import { useCollections, useConnection, useHello } from "~/sync";
import { Dot, Icons } from "~/ui";
import { shortSessionKey } from "./common/SessionLabel";
import { useSelection } from "./selection";
import { useProblems, useSignalsTab } from "./shell";
import { connectionTone } from "./TitleBar";

const quietButton =
  "inline-flex h-full cursor-default items-center gap-1 rounded-xs px-1 outline-none hovered:bg-hover focus-visible:outline-1 focus-visible:-outline-offset-1 focus-visible:outline-accent";

export function StatusBar() {
  const collections = useCollections();
  const hello = useHello();
  const connection = useConnection();
  const hosts = useRows(collections.hosts);
  const callouts = useRows(collections.callouts);
  const [seats] = useSeats(hosts.map((host) => host.id));
  const [, setSignalsTab] = useSignalsTab();
  const problems = useProblems();

  const needsYou = callouts.filter((callout) => seats.has(callout.host)).length;
  const active = problems.all.filter((session) => session.state === "active").length;
  const gaps = hosts.reduce((total, host) => total + host.gaps, 0);
  const goToProblems = () => setSignalsTab("problems");

  return (
    <footer className="flex h-6 items-center gap-3 border-t border-line bg-surface px-2 text-[11.5px] text-muted">
      <Button
        aria-label={`${problems.errors} errors`}
        onPress={goToProblems}
        className={`${quietButton} ${problems.errors > 0 ? "text-bad" : ""}`}
      >
        <Icons.error size={12} strokeWidth={1.5} aria-hidden />
        <span className="font-mono tabular">{problems.errors}</span>
      </Button>
      <Button
        aria-label={`${problems.warnings} warnings`}
        onPress={goToProblems}
        className={`${quietButton} -ml-2 ${problems.warnings > 0 ? "text-warn" : ""}`}
      >
        <Icons.warn size={12} strokeWidth={1.5} aria-hidden />
        <span className="font-mono tabular">{problems.warnings}</span>
      </Button>
      <Button
        onPress={() => setSignalsTab("needs")}
        className={`${quietButton} ${needsYou > 0 ? "text-accent" : ""}`}
      >
        <Icons.you size={12} strokeWidth={1.5} aria-hidden />
        <span className="font-mono tabular">{needsYou}</span> needs you
      </Button>
      <span>
        <span className="font-mono tabular">{problems.all.length}</span> sessions ·{" "}
        <span className="font-mono tabular">{active}</span> active
      </span>

      <span className="min-w-0 flex-1 truncate text-center text-subtle">
        <SelectionSummary />
      </span>

      <span>seats: {seats.size === 0 ? "none" : [...seats].join(", ")}</span>
      <span className={gaps > 0 ? "text-warn" : ""}>
        {gaps > 0 ? `${gaps} ${gaps === 1 ? "gap" : "gaps"}` : "no gaps"}
      </span>
      <span className="inline-flex items-center gap-1.5">
        <Dot tone={connectionTone[connection.status] ?? "neutral"} />
        {connection.status === "offline"
          ? `offline · reconnecting${connection.attempt > 0 ? `, attempt ${connection.attempt}` : ""}`
          : connection.status}
      </span>
      {hello && <span className="font-mono text-subtle">arena0d {hello.daemon.version}</span>}
    </footer>
  );
}

/** One line about what the inspector shows. */
function SelectionSummary() {
  const [selection] = useSelection();
  const { sessions } = useSessions();
  const programs = useRows(useCollections().programs);
  if (selection === null) return null;
  const sessionName = (key: string) => {
    const session = sessions.find((candidate) => candidate.key === key);
    return `${session?.program?.display_name ?? "session"} ${shortSessionKey(key)}`;
  };
  switch (selection.kind) {
    case "session": {
      const session = sessions.find((candidate) => candidate.key === selection.key);
      const step = session?.latestStep;
      return `${sessionName(selection.key)} · ${session?.state ?? "gone"}${
        step === null || step === undefined ? "" : ` · step ${step}`
      }`;
    }
    case "step":
      return `${sessionName(selection.sessionKey)} · step ${selection.step}${
        selection.host ? ` · ${selection.host}` : ""
      }`;
    case "program": {
      const program = programs.find((candidate) => candidate.hash === selection.hash);
      return program
        ? `${program.display_name} v${program.version} · ${shortHash(program.hash)}`
        : shortHash(selection.hash);
    }
    case "host":
      return selection.id;
    case "receipt":
      return `receipt ${shortHash(selection.id)} · ${selection.host}`;
  }
}
