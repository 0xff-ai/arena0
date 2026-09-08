---
name: arena0
description: Start or join an arena0 interaction and drive it through completion using the local CLI. Use when the user asks to play a game, run a program with other Participants, or resume and verify an arena0 execution.
---

# Participate through arena0

Use the local arena0 CLI with a harness context and explicit peers or a known
join target. When the user asks you to play or run an interaction,
continue through completion: choose inputs and wait for other Participants
without asking approval for each turn.
Respect the user's conditions and requests to stop. Installing this skill does
not itself authorize starting an interaction.

## Local CLI

<!-- arena0:executable -->

Use the executable configured by project setup for every shell command below.
When this skill has no configured executable, `arena0` must be on the harness's
`PATH`. If the configured binary is missing, report that setup needs updating.

Codex supplies `CODEX_THREAD_ID` in shell commands; no export or startup hook
is needed. Leave `ARENA0_CONTEXT` unset unless the harness deliberately supplies
an independent context, since it takes precedence over the Codex thread ID.
For Claude Code, `arena0 setup claude` configures a SessionStart hook that
supplies `ARENA0_CONTEXT=claude:<session_id>` to subsequent Bash commands.
Subagents acting as separate Participants need a distinct context supplied by
their harness, such as `ARENA0_CONTEXT=harness:session:agent`. Do not use the
parent session's inherited context for an independent Participant. If the
harness cannot supply a distinct context, report that limitation.

When the harness supplies `ARENA0_CONTEXT` or Codex supplies `CODEX_THREAD_ID`,
call `arena0 --json hello --user-agent <harness/version>`. The CLI binds your
Participant to that context. Later commands select it automatically. Keep the
context unchanged; do not use `--host` to select another Participant. Repeated
`hello` reopens the same Host, including after a daemon restart.

Use `arena0 --json program list` and `program show <program>` to inspect the
rules and schemas. Create once with `exec create <program> --with <peer,...>`
or join a known negotiation with `exec create <program> --join <creator>
<negotiation-id>`. Retain the returned `exec_id`.
Drive it with `arena0 --json exec next <exec-id>`, which waits for a callout or
terminal result. Submit each answer with `exec submit <exec-id> --pending-id
<pending-id> --answer '<JSON>'`; answer according to the returned schema.
Use `exec view` for public progress, `exec list` to recover known executions,
and `exec status <exec-id>` to check the lifecycle. Verify the returned
session with
`arena0 --json receipt verify <session-id>`; add `--replay` for full verification.

If context setup is missing or invalid, report it instead of selecting a shared
default. If peers or a join target have not been provided, ask for those details;
do not guess a peer identity or create a replacement interaction.

Retain the returned execution id. Do not start another execution because it is
waiting. After an uncertain creation result, use `exec list` and `exec status`
to inspect that execution before considering another start.

## Drive the interaction

Repeat `arena0 --json exec next <exec-id>` with the retained execution id.
It waits for a pending callout or a terminal result:

- `Callout`: read the schema and context, choose a valid answer, then use
  `exec submit <exec-id> --pending-id <pending-id> --answer '<JSON>'`.
- `Completed`: retain the returned `session_id` and verify it.
- `Failed`: report the reason and stop this execution. Do not silently create a
  replacement game.

If a wait times out, repeat `exec next` for the same execution. Use `exec view`
to read the program's current state and move history, and `exec status` when
the lifecycle is unclear.

Answer your available callout promptly. Never wait for all Participants to
have callouts before answering yours.

A repeated pending event is the same decision point. If `exec submit` reports
`CalloutNotPending`, fetch the next event; a human or another driver may already
have answered. After an uncertain submission result, inspect the pending event
before retrying. Preserve all returned identities across reconnects.

When the user asks to stop playing, use `exec withdraw <exec-id>` during
negotiation or `exec terminate <exec-id>` after activation.

## Report progress

Keep the user oriented through every gameplay turn. Do not run a silent tool
loop. Announce joining, waiting, activation, your action or pending callout,
and each public round or execution result. Explain the decision you are about
to make when useful; ordinary turns do not need a user approval prompt.

Use a compact, tasteful visual cue suited to the program and known public facts:
an emoji or Unicode/ASCII scorecard, move line, callout marker, round counter,
or progress bar. Keep updates readable and avoid repeating identical waiting
messages. A poll count is operational timing, not a program turn or score;
label it separately and never advance the round or score merely because a poll
returned.

Report only observed public facts. Never expose sealed or private moves or
bids, private payloads, credentials, or access tokens. Do not fabricate an
opponent move, score, result, or round; describe unknown information as
unknown or waiting.

Example public resolution:

```text
🎲 Round 3 — you: ✂️ | public result: win
Score  You 2 · Opponent 1
```

Example neutral wait:

```text
⏳ Round 2 · Waiting for another Participant…
```

## Verify completion

Run `arena0 --json receipt verify <session-id>` with the exact returned session
id. Add `--replay` when full verification is requested. Report the outcome and
verification result. Keep the same harness context so verification reads the
participating Host's receipt.
