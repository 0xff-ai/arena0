import { useRouter } from "@tanstack/react-router";
import { programRoute } from "~/app/router";
import { fmtAgo, shortHash, useNow, useRows, useSessions } from "~/model";
import { useCollections } from "~/sync";
import {
  Badge,
  Button,
  type Column,
  DataTable,
  EmptyState,
  HashChip,
  Icons,
  JsonView,
  KeyValue,
} from "~/ui";
import { SessionLabel } from "../common/SessionLabel";
import { StateWord } from "../common/StateWord";
import { programHref, useOpenDoc } from "../nav";
import { useSelection } from "../selection";
import { DocPage, DocSection } from "./DocLayout";
import { LaunchForm } from "./LaunchForm";

export function ProgramDoc() {
  const { hash } = programRoute.useParams();
  const { launch } = programRoute.useSearch();
  const router = useRouter();
  const programs = useRows(useCollections().programs);
  const program = programs.find((row) => row.hash === hash);

  if (program === undefined) {
    return (
      <EmptyState
        icon={Icons.program}
        title="This program is not in any Host's catalog"
        body={`No Host holds ${shortHash(hash)}. Import it from the Programs list.`}
        action={<Button onPress={() => router.history.push("/programs")}>Back to programs</Button>}
      />
    );
  }

  const { schema } = program;
  const range =
    program.participants.min === program.participants.max
      ? `${program.participants.min}`
      : `${program.participants.min} to ${program.participants.max}`;

  return (
    <DocPage
      title={program.display_name}
      meta={
        <>
          <span className="font-mono text-sm text-subtle">v{program.version}</span>
          <HashChip hash={program.hash} />
          {program.hosts.map((host) => (
            <Badge key={host} icon={Icons.host}>
              {host}
            </Badge>
          ))}
        </>
      }
      actions={
        launch ? (
          <Button size="sm" onPress={() => router.history.push(programHref(hash))}>
            Cancel
          </Button>
        ) : (
          <Button
            variant="primary"
            icon={Icons.add}
            onPress={() => router.history.push(programHref(hash, true))}
          >
            New session
          </Button>
        )
      }
    >
      {launch && (
        <DocSection title="New session">
          <LaunchForm program={program} />
        </DocSection>
      )}
      <p className="m-0 max-w-prose text-base text-muted">{program.description}</p>
      <KeyValue
        items={[
          { k: "name", v: program.name, mono: true },
          { k: "participants", v: range, mono: true },
        ]}
      />
      <DocSection title="Params">
        {typeof schema.params === "object" &&
        schema.params !== null &&
        !Array.isArray(schema.params) &&
        schema.params.type === "null" ? (
          <span className="text-sm text-subtle">This program takes no params.</span>
        ) : (
          <JsonView value={schema.params} collapseDepth={1} />
        )}
      </DocSection>
      <DocSection title="Callouts" note={`${schema.callouts.length}`}>
        {schema.callouts.length === 0 ? (
          <span className="text-sm text-subtle">
            This program never asks a participant for input.
          </span>
        ) : (
          <KeyValue
            items={schema.callouts.map((callout) => ({
              k: callout.name,
              v: callout.prompt,
              title: callout.prompt,
              mono: false,
            }))}
          />
        )}
      </DocSection>
      <DocSection title="Queries" note={`${schema.queries.length}`}>
        {schema.queries.length === 0 ? (
          <span className="text-sm text-subtle">No queries.</span>
        ) : (
          <KeyValue items={schema.queries.map((query) => ({ k: query.name, v: query.label }))} />
        )}
      </DocSection>
      <DocSection title="Phases" note={`${schema.phases.length}`}>
        {schema.phases.length === 0 ? (
          <span className="text-sm text-subtle">This program declares no phases.</span>
        ) : (
          <ul className="m-0 flex list-none flex-col gap-1 p-0">
            {schema.phases.map((phase) => (
              <li key={phase.name} className="flex items-center gap-2 text-sm">
                <span className="font-mono text-fg">{phase.name}</span>
                {phase.is_default && <Badge tone="info">default</Badge>}
                {phase.is_terminal && <Badge tone="done">terminal</Badge>}
                <span className="min-w-0 truncate text-muted">{phase.description}</span>
              </li>
            ))}
          </ul>
        )}
      </DocSection>
      <DocSection title="Outcome">
        <JsonView value={schema.outcome} collapseDepth={1} />
      </DocSection>
      <ProgramSessions hash={program.hash} />
    </DocPage>
  );
}

function ProgramSessions(props: { hash: string }) {
  const { sessions } = useSessions();
  const openDoc = useOpenDoc();
  const [, select] = useSelection();
  const now = useNow(10_000);
  const rows = sessions.filter((session) => session.programHash === props.hash);

  const columns: Column<(typeof rows)[number]>[] = [
    {
      id: "session",
      title: "Session",
      width: "1fr",
      isRowHeader: true,
      render: (s) => <SessionLabel session={s} />,
    },
    { id: "state", title: "State", width: 110, render: (s) => <StateWord state={s.state} /> },
    {
      id: "step",
      title: "Step",
      width: 60,
      align: "end",
      render: (s) => <span className="font-mono tabular">{s.latestStep ?? "–"}</span>,
    },
    {
      id: "age",
      title: "Started",
      width: 110,
      align: "end",
      render: (s) => <span className="text-subtle">{fmtAgo(s.createdMs, now)}</span>,
    },
  ];

  return (
    <DocSection title="Sessions" note={`${rows.length}`}>
      {/* The table scrolls inside its own box, so it needs a height: heading plus up to eight rows. */}
      <div
        style={{ height: 24 + 26 * Math.max(1, Math.min(rows.length, 8)) }}
        className="rounded-sm border border-line-soft"
      >
        <DataTable
          label={`Sessions of this program`}
          columns={columns}
          rows={rows}
          getKey={(s) => s.key}
          onSelect={(key) => select({ kind: "session", key })}
          onAction={(key) => openDoc({ kind: "session", key })}
          empty={
            <div className="px-2 py-2 text-sm text-subtle">No sessions of this program yet.</div>
          }
        />
      </div>
    </DocSection>
  );
}
