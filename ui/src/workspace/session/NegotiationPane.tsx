import { fmtClock, type Session, shortHash } from "~/model";
import type { ExecutionRow } from "~/sync";
import { Badge, HashChip, KeyValue, ParticipantChip } from "~/ui";

function HostNegotiation(props: { session: Session; execution: ExecutionRow }) {
  const { execution, session } = props;
  const activation = execution.activation;
  return (
    <section aria-label={`Negotiation on ${execution.host}`} className="flex flex-col gap-2">
      <h3 className="m-0 font-mono text-base font-medium">{execution.host}</h3>
      <div className="grid gap-x-8 gap-y-3 lg:grid-cols-2">
        <div className="min-w-0">
          <div className="mb-1 text-xs font-medium tracking-wide text-subtle uppercase">
            Observed
          </div>
          {execution.negotiation.length === 0 ? (
            <div className="text-sm text-subtle">Nothing observed live.</div>
          ) : (
            <ol className="m-0 flex list-none flex-col p-0">
              {execution.negotiation.map((mark, i) => (
                <li key={i} className="flex h-6 items-center gap-3 text-sm">
                  <span className="w-16 shrink-0 font-mono text-xs text-subtle tabular">
                    {fmtClock(mark.at_ms)}
                  </span>
                  <span className="w-32 shrink-0 text-fg">{mark.kind.replace("_", " ")}</span>
                  <span className="min-w-0 truncate text-muted">{mark.detail}</span>
                </li>
              ))}
            </ol>
          )}
        </div>
        <div role="group" aria-label={`Activation on ${execution.host}`} className="min-w-0">
          <div className="mb-1 text-xs font-medium tracking-wide text-subtle uppercase">
            Activation
          </div>
          {activation === null ? (
            <div className="text-sm text-subtle">Not activated yet.</div>
          ) : (
            <div className="flex flex-col gap-2">
              <KeyValue
                items={[
                  {
                    k: "state",
                    v: (
                      <Badge tone={activation.state === "committed" ? "ok" : "neutral"}>
                        {activation.state}
                      </Badge>
                    ),
                  },
                  { k: "offer", v: <HashChip hash={activation.offer_hash} copy /> },
                  { k: "target size", v: String(activation.target_size), mono: true },
                  { k: "initial state", v: <HashChip hash={activation.initial_state} /> },
                ]}
              />
              <ul
                aria-label="Participants and tickets"
                className="m-0 flex list-none flex-col gap-1 p-0"
              >
                {activation.participants.map((participant, index) => (
                  <li key={participant.peer_id} className="flex items-center gap-2 text-sm">
                    <ParticipantChip
                      index={index}
                      label={session.participants[index]?.host ?? shortHash(participant.peer_id, 6)}
                    />
                    <span className="text-subtle">ticket</span>
                    <HashChip hash={participant.ticket_hash} />
                  </li>
                ))}
              </ul>
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

export function NegotiationPane(props: { session: Session }) {
  return (
    <div className="flex flex-col gap-4 p-3">
      <p className="m-0 text-sm text-subtle">
        Observed live by this workspace; negotiations that ran while it was closed show only durable
        facts.
      </p>
      {props.session.executions.map((execution) => (
        <HostNegotiation key={execution.key} session={props.session} execution={execution} />
      ))}
    </div>
  );
}
