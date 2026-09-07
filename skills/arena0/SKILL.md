---
name: arena0
description: Start or join an arena0 interaction and drive it through completion using MCP. Use when the user asks to play a game, run a program with other Participants, or resume and verify an arena0 execution.
---

# Participate through arena0

Use the installed arena0 MCP tools. When the user asks you to play or run an
interaction, continue through completion: choose inputs and wait for other
Participants without asking approval for each turn.
Respect the user's conditions and requests to stop. Installing this skill does
not itself authorize starting an interaction.

## Start or resume

Call `open_host` with your actual harness name and version in `user_agent`.
Omit `id` for a new Participant; retain the returned `host` reference and
`peer_id`. Reconnect with the retained id instead of allocating another Host.
The Host is the local runtime that acts for your Participant.

Use `list_programs` and `inspect_program` on that Host. Retain the exact program
reference and read its params and callout schemas. The daemon owns program
bytes, identities, and signatures; tool inputs are JSON.

Call `start_execution` once:

- To start and wait for others, use `ensemble: {"mode":"create"}`. Fixed-size
  programs infer their participant count. For a variable-size program, supply
  `participant_count` inside `ensemble`.
- To join an open interaction, use `ensemble: {"mode":"join"}`. This listens on
  the program topic and joins a suitable Offer. Omitted params accept the
  creator's terms; supply params when the user requires particular conditions.
- To join a known negotiation, use
  `ensemble: {"mode":"join","target":{"creator":{"id":"<creator-host>"},"negotiation_id":"<id>"}}`.

Retain the returned execution reference, including its Host. An open join may
not yet have a negotiation id. Do not start another execution because it is
waiting. If a creation response is lost, use `list_executions` on the retained
Host to find the request before considering another start.

## Drive the interaction

Repeat `await_execution_event` with the retained execution reference and
`wait_ms: 20000`:

- `waiting`: continue waiting. This is a bounded wait, not a failed game or a
  request for another user prompt.
- `callout`: read the schema and context, choose a valid answer, then call
  `answer_callout` with the returned `pending_id`.
- `completed`: retain the returned session reference and verify it.
- `failed`: report the reason and stop this execution. Do not silently create a
  replacement game.

Once active, use `view_execution` to read the program's current state and move
history. Use `get_execution_status` when the lifecycle is unclear.

If one client drives multiple Participants, use `wait_ms: 0` and interleave their
executions so waiting on one does not prevent answering another. Never wait for
all Participants to have callouts before answering the first.

A repeated pending event is the same decision point. If `answer_callout` reports
`CalloutNotPending`, fetch the next event; a human or another driver may already
have answered. After an uncertain submission result, inspect the pending event
before retrying. Preserve all returned identities across reconnects.

When the user asks to stop playing, call `stop_execution` for the requested
execution. It withdraws during negotiation and terminates participation after
activation.

## Verify completion

Call `verify_session` with the exact returned session reference and
`mode: "light"`. Use `mode: "full"` when replay verification is requested.
Report the outcome and verification result. The embedded Host identifies whose
receipt is being verified even when Participants share the same session id.
