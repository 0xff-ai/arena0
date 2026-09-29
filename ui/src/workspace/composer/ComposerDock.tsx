import { useEffect, useRef, useState } from "react";
import { fmtDuration, useNow, useRows, useSeats, useSessions } from "~/model";
import { type CalloutRow, GatewayError, OUTCOME_UNKNOWN, useCall, useCollections } from "~/sync";
import {
  Button,
  FieldError,
  IconButton,
  Icons,
  type JsonLike,
  JsonView,
  KeyValue,
  ParticipantChip,
  SchemaForm,
  toasts,
  validate,
} from "~/ui";
import { draftStore, useComposer } from "./store";

/**
 * The dock answers one callout, chosen by `useComposer`. The callout row is
 * the authority on whether the answer is still wanted: the dock closes when
 * the row goes away, and reports what it knows about why.
 */
export function ComposerDock() {
  const { calloutKey } = useComposer();
  if (calloutKey === null) return null;
  // Keyed so that moving to another callout starts from that callout's own state.
  return <Composer key={calloutKey} calloutKey={calloutKey} />;
}

/**
 * editing: the form is live. submitting: the call is in flight. accepted: the
 * daemon took the answer; agreement is pending. unknown: the socket closed
 * before the reply, so the answer may or may not have been applied. elsewhere:
 * the callout is no longer pending.
 */
type Phase = "editing" | "submitting" | "accepted" | "unknown" | "elsewhere";

function isNullSchema(schema: JsonLike): boolean {
  return (
    typeof schema === "object" &&
    schema !== null &&
    !Array.isArray(schema) &&
    schema.type === "null"
  );
}

function Composer(props: { calloutKey: string }) {
  const { calloutKey } = props;
  const composer = useComposer();
  const collections = useCollections();
  const callouts = useRows(collections.callouts);
  const hosts = useRows(collections.hosts);
  const { sessions } = useSessions();
  const [seats] = useSeats(hosts.map((host) => host.id));
  const now = useNow();
  const answer = useCall("answer");

  const live = callouts.find((callout) => callout.key === calloutKey);
  // The row is gone once the answer is agreed; the header and the session lookup keep using the last one seen.
  const [seen, setSeen] = useState<CalloutRow | undefined>(live);
  if (live !== undefined && live !== seen) setSeen(live);
  const callout = live ?? seen;

  const [value, setValue] = useState<JsonLike | undefined>(() => draftStore.get(calloutKey));
  // The latest edit, readable synchronously: Mod+Enter blurs the focused field
  // and submits in the same event, before React re-renders with the value the
  // blur committed.
  const valueRef = useRef(value);
  const [attempted, setAttempted] = useState(false);
  const [rejection, setRejection] = useState<string | null>(null);
  const [phase, setPhase] = useState<Phase>("editing");
  // The session's latest step when the answer went out: the toast names the
  // step the answer produced, so it waits until one newer than this exists.
  const [stepAtSubmit, setStepAtSubmit] = useState(-1);

  const session = callout
    ? sessions.find((s) => s.executions.some((e) => e.key === `${callout.host}/${callout.exec_id}`))
    : undefined;
  const participant = session?.participants.find((p) => p.host === callout?.host);

  const gone = live === undefined;
  const step = session?.latestStep ?? null;
  // A callout only goes away by being answered, which certifies a step, or by
  // the session ending.
  const closing =
    gone &&
    (phase === "submitting" || phase === "accepted" || phase === "unknown") &&
    ((step ?? -1) > stepAtSubmit || session?.terminal != null);
  useEffect(() => {
    if (!closing) return;
    draftStore.clear(calloutKey);
    toasts.show({
      tone: "ok",
      title: step === null ? "Answered" : `Answered · step ${step}`,
    });
    composer.close();
  }, [closing, calloutKey, step, composer]);

  const seatCallouts = callouts
    .filter((c) => seats.has(c.host))
    .sort((a, b) => a.opened_ms - b.opened_ms || (a.key < b.key ? -1 : 1));
  const position = seatCallouts.findIndex((c) => c.key === calloutKey);
  const neighbour = (offset: number) =>
    seatCallouts[(position + offset + seatCallouts.length) % seatCallouts.length];

  const elsewhere = phase === "elsewhere" || (gone && phase === "editing");
  const noInput = callout !== undefined && isNullSchema(callout.schema);
  const issues =
    attempted && callout !== undefined && !noInput ? validate(callout.schema, value) : [];

  function edit(next: JsonLike | undefined) {
    valueRef.current = next;
    setValue(next);
    draftStore.set(calloutKey, next);
    setRejection(null);
  }

  function submit() {
    if (callout === undefined || phase !== "editing" || gone) return;
    // `SchemaForm` never emits null, so a callout without input answers null itself.
    const body = noInput ? null : valueRef.current;
    setAttempted(true);
    if (body === undefined || (!noInput && validate(callout.schema, body).length > 0)) return;
    setRejection(null);
    setStepAtSubmit(session?.latestStep ?? -1);
    setPhase("submitting");
    answer.mutate(
      {
        host: callout.host,
        exec_id: callout.exec_id,
        pending_id: callout.pending_id,
        answer: body,
      },
      {
        onSuccess: () => setPhase("accepted"),
        onError: (error) => {
          if (error instanceof GatewayError && error.code === "callout_not_pending") {
            draftStore.clear(calloutKey);
            setPhase("elsewhere");
          } else if (
            error instanceof GatewayError &&
            error.code === "gateway" &&
            error.message === OUTCOME_UNKNOWN
          ) {
            setPhase("unknown");
          } else {
            // A rejected answer changes nothing on the daemon; the draft stays for another try.
            setRejection(error.message);
            setPhase("editing");
          }
        },
      },
    );
  }

  const context = callout?.context;
  const contextItems =
    context !== null && typeof context === "object" && !Array.isArray(context)
      ? Object.entries(context).map(([k, v]) => ({
          k,
          v:
            typeof v === "object" && v !== null ? (
              <JsonView value={v} collapseDepth={1} />
            ) : (
              String(v)
            ),
          mono: true,
        }))
      : context === undefined || context === null
        ? []
        : [{ k: "context", v: String(context), mono: true }];

  return (
    <section
      aria-label="Composer"
      className="flex h-full min-h-0 flex-col border-t border-line bg-surface"
      onKeyDown={(event) => {
        if (event.key !== "Enter" || !(event.metaKey || event.ctrlKey)) return;
        event.preventDefault();
        if (document.activeElement instanceof HTMLElement) document.activeElement.blur();
        submit();
      }}
    >
      <header className="flex h-7 shrink-0 items-center gap-2 border-b border-line-soft pr-1 pl-2.5 text-sm">
        <Icons.callout size={14} className="shrink-0 text-accent" />
        <span className="shrink-0 text-fg">{session?.program?.display_name ?? "Callout"}</span>
        {participant !== undefined && callout !== undefined && (
          <ParticipantChip index={participant.index} label={callout.host} you />
        )}
        {callout !== undefined && (
          <>
            <span className="shrink-0 font-mono text-xs text-subtle">{callout.name}</span>
            <span className="min-w-0 flex-1 truncate text-muted" title={callout.prompt}>
              {callout.prompt}
            </span>
            <span className="shrink-0 font-mono text-xs text-subtle tabular">
              {fmtDuration(now - callout.opened_ms)}
            </span>
          </>
        )}
        {callout === undefined && <span className="flex-1" />}
        {seatCallouts.length > 1 && position >= 0 && (
          <>
            <IconButton
              icon={Icons.chevronRight}
              label="Previous callout"
              className="rotate-180"
              onPress={() => composer.open(neighbour(-1)?.key ?? calloutKey)}
            />
            <span className="shrink-0 font-mono text-xs text-subtle tabular">
              {position + 1} of {seatCallouts.length}
            </span>
            <IconButton
              icon={Icons.chevronRight}
              label="Next callout"
              onPress={() => composer.open(neighbour(1)?.key ?? calloutKey)}
            />
          </>
        )}
        <IconButton icon={Icons.close} label="Close composer" kbd="Esc" onPress={composer.close} />
      </header>

      <div className="min-h-0 flex-1 overflow-auto p-3">
        {elsewhere || callout === undefined ? (
          <div className="flex items-center gap-2 text-sm text-muted">
            <Icons.info size={14} className="text-subtle" />
            This callout was answered elsewhere.
            <Button size="sm" onPress={composer.close}>
              Close
            </Button>
          </div>
        ) : (
          <div className="grid min-w-0 grid-cols-[minmax(12rem,2fr)_minmax(0,3fr)] gap-6">
            <div className="min-w-0">
              <div className="mb-1.5 text-xs font-medium tracking-wide text-subtle uppercase">
                Context
              </div>
              {contextItems.length > 0 ? (
                <KeyValue items={contextItems} />
              ) : (
                <div className="text-sm text-subtle">No context.</div>
              )}
            </div>
            <div className="flex min-w-0 flex-col gap-3">
              {noInput ? (
                <div className="text-sm text-muted">This callout takes no input.</div>
              ) : (
                <SchemaForm
                  schema={callout.schema}
                  value={value}
                  onChange={edit}
                  issues={issues}
                  disabled={phase !== "editing"}
                  autoFocus
                />
              )}
              {rejection !== null && <FieldError>{rejection}</FieldError>}
              <div className="flex items-center gap-3">
                <Button
                  variant="primary"
                  kbd="Mod+Enter"
                  isDisabled={phase !== "editing"}
                  onPress={submit}
                >
                  Submit
                </Button>
                <PhaseNote phase={phase} />
              </div>
            </div>
          </div>
        )}
      </div>
    </section>
  );
}

function PhaseNote(props: { phase: Phase }) {
  switch (props.phase) {
    case "submitting":
      return <span className="text-sm text-subtle">Submitting…</span>;
    case "accepted":
      return (
        <span className="flex items-center gap-1 text-sm text-ok">
          <Icons.check size={13} />
          Accepted — waiting for agreement
        </span>
      );
    case "unknown":
      return <span className="text-sm text-warn">outcome unknown — reloading</span>;
    default:
      return null;
  }
}
