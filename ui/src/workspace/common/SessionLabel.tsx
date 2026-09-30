import { type Session, shortHash } from "~/model";
import { ParticipantChip } from "~/ui";

/** Eight characters of the key's last segment: keys are a negotiation id or `e:<host>/<exec id>`. */
export function shortSessionKey(key: string): string {
  return shortHash(key.slice(key.lastIndexOf("/") + 1).replace(/^e:/, ""));
}

/** Participant `index` of the session, named by its local Host or else a short peer id. */
export function participantChip(session: Session, index: number) {
  const participant = session.participants[index];
  return (
    <ParticipantChip
      index={index}
      label={participant?.host ?? shortHash(participant?.peerId ?? "", 6)}
    />
  );
}

/** A session's program name and its short key. */
export function SessionLabel(props: { session: Session }) {
  const { session } = props;
  return (
    <span className="inline-flex min-w-0 items-baseline gap-1.5">
      <span className="truncate text-fg">
        {session.program?.display_name ?? shortHash(session.programHash)}
      </span>
      <span className="shrink-0 font-mono text-xs text-subtle">{shortSessionKey(session.key)}</span>
    </span>
  );
}
