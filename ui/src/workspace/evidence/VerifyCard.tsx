import { useState } from "react";
import { useRead, type VerifyReply, type VerifyTarget } from "~/sync";
import { Button, Icons } from "~/ui";

interface Check {
  label: string;
  ok: boolean;
}

function sameList(a: string[], b: string[]): boolean {
  return a.length === b.length && a.every((item, i) => item === b[i]);
}

/** The checks beyond "the daemon accepted it": each compares the receipt to what the caller expects. */
function checks(
  reply: VerifyReply,
  expect: { program?: string; sessionId?: string; participants?: string[] } | undefined,
): Check[] {
  const list: Check[] = [{ label: "Structure and signatures verified", ok: true }];
  if (expect?.program !== undefined) {
    list.push({ label: "Program matches", ok: reply.program === expect.program });
  }
  if (expect?.sessionId !== undefined) {
    list.push({ label: "Session matches", ok: reply.session_id === expect.sessionId });
  }
  if (expect?.participants !== undefined) {
    list.push({
      label: "Ensemble matches the participants",
      ok: sameList(reply.ensemble, expect.participants),
    });
  }
  list.push({ label: `${reply.steps} steps`, ok: true });
  list.push({
    label:
      reply.termination.kind === "completed"
        ? "Termination: completed"
        : `Termination: stopped (${reply.termination.cause})`,
    ok: true,
  });
  return list;
}

/**
 * Verification is an explicit action: it asks the daemon to check every
 * signature, so it never runs on its own. A failed call shows the daemon's
 * message; a successful one lists what was compared.
 */
export function VerifyCard(props: {
  host: string;
  target: VerifyTarget;
  expect?: { program?: string; sessionId?: string; participants?: string[] };
}) {
  const [asked, setAsked] = useState(false);
  const verify = useRead("verify", asked ? { host: props.host, target: props.target } : null);

  return (
    <div className="flex flex-col gap-1.5">
      <div>
        <Button
          size="sm"
          icon={Icons.verify}
          onPress={() => (asked ? verify.refetch() : setAsked(true))}
          isDisabled={verify.isFetching}
        >
          {verify.isFetching ? "Verifying…" : asked ? "Verify again" : "Verify"}
        </Button>
      </div>
      {verify.error && (
        <div role="alert" className="flex items-start gap-1.5 text-sm text-bad">
          <Icons.error size={14} className="mt-0.5 shrink-0" aria-hidden />
          <span className="min-w-0 break-words">{verify.error.message}</span>
        </div>
      )}
      {verify.data && (
        <ul aria-label="Verification checks" className="m-0 flex list-none flex-col gap-0.5 p-0">
          {checks(verify.data, props.expect).map((check) => (
            <li key={check.label} className="flex items-center gap-1.5 text-sm">
              {check.ok ? (
                <Icons.check size={13} className="shrink-0 text-ok" aria-hidden />
              ) : (
                <Icons.error size={13} className="shrink-0 text-bad" aria-hidden />
              )}
              <span className={check.ok ? "text-fg" : "text-bad"}>{check.label}</span>
              <span className="sr-only">{check.ok ? "passed" : "failed"}</span>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
