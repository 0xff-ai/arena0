---
name: arena0
description: Run and verify an arena0 co-execution through arena0d's MCP tools. Use when an agent needs an assigned Host, an exact program and Ensemble, callout handling, and receipt verification.
---

# Run an arena0 execution

Connect one MCP client to the Streamable HTTP endpoint exposed by `arena0d`,
normally `http://127.0.0.1:7330/mcp`. If the daemon requires a bearer token,
send it on this connection. Authentication selects no Host.

Start by assigning this agent a Host:

```json
{
  "user_agent": "claude-code/1.0"
}
```

Call `open_host` with that object. To reopen a known Host, include its id:

```json
{
  "id": "host-01",
  "user_agent": "claude-code/1.0"
}
```

Use the actual harness name and version in `user_agent`; `claude-code/1.0` is
only an example.

The result contains the Host reference and public metadata:

```json
{
  "host": {"id": "host-01"},
  "peer_id": "<peer-id>",
  "user_agent": "claude-code/1.0"
}
```

Retain the returned `host` object exactly and pass it in every later
Host-scoped reference. Keep the `peer_id` and `user_agent` with the result for
diagnostics; they identify the assigned Host at this point in the session.
If a later call says that the retained Host id is absent, first call `open_host`
again with that retained `id` and the same `user_agent`; a daemon may be
recovering a lazily started namespace. Only if that cannot recover the
namespace, and the user or task permits replacing the participant, call
`open_host` without `id` to allocate a fresh Host. Replace the Host reference
in all later program, execution, and session references only after that
intentional replacement.

## Tools

| Tool | Purpose |
|---|---|
| `open_host` | Assign or reopen one local Host and return its `{id}` reference and public metadata. |
| `list_programs` | List programs installed on one Host. |
| `inspect_program` | Read one program's summary and public JSON Schema. |
| `start_execution` | Create or join one exact execution on a Host. |
| `get_execution_status` | Read a durable lifecycle snapshot without waiting. |
| `await_execution_event` | Return the next callout, waiting state, completion, or failure. |
| `answer_callout` | Submit one JSON answer for a pending callout. |
| `query_execution` | Run a guest-defined read-only JSON query. |
| `stop_execution` | Withdraw during negotiation or terminate after activation. |
| `verify_session` | Verify the Host-produced receipt with light checks or full replay. |

## Select and inspect a program

List programs on the assigned Host and retain one exact `program` reference:

```json
{"host":{"id":"host-01"}}
```

Use the returned program content id in both the program reference and
`inspect_program`:

```json
{
  "program": {
    "host": {"id":"host-01"},
    "program_id": "<program-id>"
  }
}
```

Read the public params, callout, query, and message schemas before sending
values. Params, answers, queries, and outcomes are JSON. The exact Wasm and
its public metadata stay owned by the Host.

## Exact admission

The creator supplies the exact other Host ids. Every selected Host must
already contain the same exact program:

```json
{
  "program": {
    "host": {"id":"host-01"},
    "program_id": "<program-id>"
  },
  "params": {"rounds": 3},
  "ensemble": {
    "mode": "explicit",
    "hosts": [{"id":"host-02"}]
  }
}
```

A joiner names the creator Host and the creator's negotiation id:

```json
{
  "program": {
    "host": {"id":"host-02"},
    "program_id": "<program-id>"
  },
  "ensemble": {
    "mode": "join",
    "creator": {"id":"host-01"},
    "negotiation_id": "<negotiation-id>"
  }
}
```

Keep each returned `execution` reference, including its Host id and execution
id. A joiner normally omits params and adopts the creator's immutable terms.

## Drive callouts

Call `await_execution_event` repeatedly with the retained execution reference.
There is no timeout argument or timeout guarantee in this tool flow.

- `{"event":"waiting"}` means no durable event is ready now. Poll again and
  advance another Host's execution if the Ensemble has other drivers.
- `{"event":"callout", "pending_id": ..., "name": ..., "prompt": ..., "schema": ..., "context": ...}`
  requires one immediate `answer_callout` call. Read the inline `schema` and
  `context`; submit exactly one JSON `answer` with the same execution and
  pending id.
- `{"event":"completed", "session": ..., "outcome": ...}` is terminal.
  Retain its Host-specific session reference.
- `{"event":"failed", "reason": ...}` is terminal and should be reported.

Use `get_execution_status` for non-blocking lifecycle inspection and
`query_execution` for a guest-defined read-only query. Use `stop_execution`
only when the operator asks to withdraw or terminate an execution.

## Verify the receipt

Verify every completed session using the Host reference embedded in the
returned session object:

```json
{
  "session": {
    "host": {"id":"host-01"},
    "session_id": "<session-id>"
  },
  "mode": "light"
}
```

`light` checks the portable signed evidence. `full` also replays the exact
registered Wasm. A successful result reports the program, participant set,
steps, and terminal evidence. A session id alone never selects a receipt
producer.

Keep Host, program, execution, and session references from tool results. Do
not invent ids, transfer program bytes, or sign proof material.
