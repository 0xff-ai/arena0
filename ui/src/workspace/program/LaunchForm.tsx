import { useEffect, useRef, useState } from "react";
import { sessionKey, useRows, useSeats } from "~/model";
import { comparePeerIds, type ExecRef, type ProgramRow, useCall, useCollections } from "~/sync";
import {
  Button,
  FieldError,
  type JsonLike,
  NumberField,
  SchemaForm,
  Segmented,
  Select,
  validate,
} from "~/ui";
import { AddHostButton } from "../actions/imports";
import { useComposer } from "../composer/store";
import { useOpenDoc } from "../nav";

type DriverKind = "you" | "external";

interface SeatDraft {
  host: string | null;
  kind: DriverKind;
}

const DRIVERS: { id: DriverKind; label: string }[] = [
  { id: "you", label: "You" },
  { id: "external", label: "External" },
];

const isNullSchema = (schema: JsonLike): boolean =>
  typeof schema === "object" && schema !== null && !Array.isArray(schema) && schema.type === "null";

/**
 * Starts a session: params, participant count and one seat per participant.
 * A seat's driver is whether its Host is in your Seats: each row starts from
 * the current Seats, and Launch writes the choices back, so the Answer button,
 * the composer and Needs input never offer an External seat's callouts.
 * After `launch` replies it waits for the new executions to show up in the
 * replica, then opens the session (pinned) and, when a seat of yours has a
 * callout, the composer. Nothing outlives this component, so the navigation
 * happens while it is still mounted.
 */
export function LaunchForm(props: { program: ProgramRow }) {
  const { program } = props;
  const collections = useCollections();
  const hosts = useRows(collections.hosts);
  const executions = useRows(collections.executions);
  const callouts = useRows(collections.callouts);
  const openDoc = useOpenDoc();
  const composer = useComposer();
  const launch = useCall("launch");
  const [yourSeats, setYourSeats] = useSeats(hosts.map((host) => host.id));

  // A seat needs a Host that holds the program.
  const capable = hosts.filter((host) => program.hosts.includes(host.id));

  const noParams = isNullSchema(program.schema.params);
  const [params, setParams] = useState<JsonLike | undefined>(undefined);
  const [count, setCount] = useState(program.participants.min);
  const [seatDrafts, setSeatDrafts] = useState<Record<number, Partial<SeatDraft>>>({});
  const [attempted, setAttempted] = useState(false);
  const [started, setStarted] = useState<ExecRef[] | null>(null);

  const seatAt = (index: number): SeatDraft => {
    const host = seatDrafts[index]?.host ?? capable[index]?.id ?? null;
    const kind = host !== null && yourSeats.has(host) ? "you" : "external";
    return { host, kind, ...seatDrafts[index] };
  };
  const seats = Array.from({ length: Number.isFinite(count) ? Math.max(0, count) : 0 }, (_, i) =>
    seatAt(i),
  );
  const edit = (index: number, change: Partial<SeatDraft>) =>
    setSeatDrafts((all) => ({ ...all, [index]: { ...all[index], ...change } }));

  const paramIssues = noParams || !attempted ? [] : validate(program.schema.params, params);
  const problems: string[] = [];
  if (
    !Number.isInteger(count) ||
    count < program.participants.min ||
    count > program.participants.max
  ) {
    problems.push(
      `Participants must be between ${program.participants.min} and ${program.participants.max}.`,
    );
  }
  if (capable.length < program.participants.min) {
    problems.push(
      `This program needs ${program.participants.min} Hosts and ${capable.length} hold it.`,
    );
  }
  const chosen = seats.flatMap((seat) => (seat.host === null ? [] : [seat.host]));
  if (seats.some((seat) => seat.host === null) || new Set(chosen).size !== chosen.length) {
    problems.push("Every seat needs its own Host.");
  }
  const invalid = problems.length > 0 || paramIssues.length > 0;

  function start() {
    setAttempted(true);
    const body = noParams ? null : params;
    if (
      problems.length > 0 ||
      body === undefined ||
      (!noParams && validate(program.schema.params, body).length > 0)
    ) {
      return;
    }
    const next = new Set(yourSeats);
    for (const seat of seats) {
      if (seat.host === null) continue;
      if (seat.kind === "you") next.add(seat.host);
      else next.delete(seat.host);
    }
    setYourSeats([...next]);
    launch.mutate(
      { program: program.hash, params: body, hosts: chosen },
      { onSuccess: (reply) => setStarted(reply.execs) },
    );
  }

  // The session opens once its executions are in the replica and, if you hold
  // a seat, its first callout is too: the composer needs a callout row to
  // open on. A session that ends first opens without one.
  const opened = useRef(false);
  const youHosts = new Set(seats.filter((seat) => seat.kind === "you").map((seat) => seat.host));
  const keys = new Set(started?.map((e) => `${e.host}/${e.exec_id}`));
  const mine = executions.filter((e) => keys.has(e.key));
  const firstCallout = callouts.find(
    (c) => keys.has(`${c.host}/${c.exec_id}`) && youHosts.has(c.host),
  );
  const first = mine[0];
  const settled =
    first !== undefined &&
    (youHosts.size === 0 ||
      firstCallout !== undefined ||
      mine.every(
        (e) => e.lifecycle === "completed" || e.lifecycle === "aborted" || e.lifecycle === "failed",
      ));
  useEffect(() => {
    if (!settled || first === undefined || opened.current) return;
    opened.current = true;
    openDoc({ kind: "session", key: sessionKey(first) }, { pin: true });
    if (firstCallout !== undefined) composer.open(firstCallout.key);
  }, [settled, first, firstCallout, openDoc, composer]);

  const busy = launch.isPending || started !== null;

  // The session numbers participants by sorted peer id, not by seat order.
  const peerOf = (host: string | null) => hosts.find((row) => row.id === host)?.peer_id;
  const peers = chosen.flatMap((host) => peerOf(host) ?? []).sort(comparePeerIds);
  const participantOf = (host: string | null): number | null => {
    const peer = peerOf(host);
    return peer === undefined ? null : peers.indexOf(peer);
  };

  return (
    <div className="flex max-w-3xl flex-col gap-4">
      {!noParams && (
        <SchemaForm
          schema={program.schema.params}
          value={params}
          onChange={setParams}
          issues={paramIssues}
          disabled={busy}
        />
      )}
      <NumberField
        label="Participants"
        value={count}
        onChange={setCount}
        minValue={program.participants.min}
        maxValue={program.participants.max}
        isDisabled={busy || program.participants.min === program.participants.max}
        className="w-40"
      />
      <div className="flex flex-col gap-1.5">
        <div className="text-sm text-muted">Seats</div>
        <div
          inert={busy}
          className="grid grid-cols-[3.5rem_10rem_auto_minmax(0,1fr)] items-center gap-x-3 gap-y-1.5"
        >
          {seats.map((seat, index) => (
            <SeatRow
              key={index}
              index={index}
              participant={participantOf(seat.host)}
              seat={seat}
              hosts={capable.map((host) => ({
                id: host.id,
                label: host.id,
                detail: host.user_agent ?? undefined,
              }))}
              onChange={(change) => edit(index, change)}
            />
          ))}
        </div>
      </div>
      {attempted && problems.map((problem) => <FieldError key={problem}>{problem}</FieldError>)}
      {attempted && capable.length < program.participants.min && (
        <div>
          <AddHostButton />
        </div>
      )}
      {launch.error && <FieldError>{launch.error.message}</FieldError>}
      <div className="flex items-center gap-3">
        <Button variant="primary" isDisabled={busy || (attempted && invalid)} onPress={start}>
          {busy ? "Starting…" : "Launch"}
        </Button>
        {started !== null && first !== undefined && !settled && (
          <span className="text-sm text-subtle">Waiting for the first callout…</span>
        )}
      </div>
    </div>
  );
}

function SeatRow(props: {
  index: number;
  /** The participant index this seat's Host will have; null until it has a Host. */
  participant: number | null;
  seat: SeatDraft;
  hosts: { id: string; label: string; detail?: string }[];
  onChange: (change: Partial<SeatDraft>) => void;
}) {
  const { index, seat } = props;
  const n = index + 1;
  return (
    <>
      <span data-testid={`seat-${n}-participant`} className="text-sm text-subtle">
        {props.participant === null ? "—" : `P${props.participant}`}
      </span>
      <Select
        placeholder={`Host for seat ${n}`}
        items={props.hosts}
        value={seat.host}
        onChange={(host) => props.onChange({ host })}
      />
      <Segmented
        label={`Driver for seat ${n}`}
        size="md"
        items={DRIVERS}
        value={seat.kind}
        onChange={(kind) => props.onChange({ kind })}
      />
      <span className="text-sm text-subtle">
        {seat.kind === "you"
          ? "You answer this Host's callouts here"
          : "An agent answers this Host's callouts, over MCP or the CLI"}
      </span>
    </>
  );
}
