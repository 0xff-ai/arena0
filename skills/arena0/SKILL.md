---
name: arena0
description: Run and verify an arena0 co-execution through arena0d's Host-explicit MCP tools. Use when an agent should choose a local Host and program, join an exact Ensemble, answer callouts, and verify the resulting session.
---

# Running an arena0 session

Connect once to the Streamable HTTP endpoint exposed by `arena0d`, normally
`http://127.0.0.1:7330/mcp`. If `ARENA0_MCP_TOKEN` was set when the daemon
started, send it as the bearer token for this connection.

The endpoint and token cover the complete local Ensemble. They never select a
Host. Carry the structured Host reference returned by `list_hosts` in every
later call. The Host owns identity, signing, admission, execution, persistence,
sandbox limits, and receipt production. Agent-facing values are JSON; never
hand-encode program bytes or sign proof material.

## Tools

| Tool | Purpose |
|---|---|
| `list_hosts` | List every Host supervised by this daemon. |
| `list_programs` | List programs installed on one Host. |
| `inspect_program` | Read one program's summary and full public JSON Schema. |
| `start_execution` | Create or join one exact execution on a Host. |
| `get_execution_status` | Read a durable lifecycle snapshot without waiting. |
| `await_execution_event` | Wait for a callout, completion, or failure. |
| `answer_callout` | Answer the pending callout using its inline schema. |
| `query_execution` | Run a guest-defined read-only JSON query. |
| `stop_execution` | Withdraw during negotiation or terminate after activation. |
| `verify_session` | Light-verify proof evidence or fully replay the exact Wasm. |

## Workflow

1. Call `list_hosts {}` and choose a returned `host` object.
2. Call `list_programs {"host": <host>}`.
3. Call `inspect_program` with the `program` reference returned by step 2.
4. Start or join an execution. Keep the returned structured `execution`
   reference; it includes both Host and execution id.
5. Alternate `await_execution_event` and `answer_callout` until completion.
6. Keep the returned structured `session` reference and call `verify_session`.

Optional fields may be omitted. IDs are lowercase hex strings. Params, answers,
queries, and outcomes must match the inspected program schema.

## Exact admission

The creator names the exact other daemon-local Hosts:

```json
{
  "program": {
    "host":{"name":"default"},
    "program_id":"<program-id>"
  },
  "params": {"rounds":3},
  "ensemble": {
    "mode":"explicit",
    "hosts":[{"name":"host-2"}]
  }
}
```

A joiner names the creator Host and the creator's negotiation id:

```json
{
  "program": {
    "host":{"name":"host-2"},
    "program_id":"<program-id>"
  },
  "ensemble": {
    "mode":"join",
    "creator":{"name":"default"},
    "negotiation_id":"<negotiation-id>"
  }
}
```

Every selected Host must already have the exact program. A joiner normally
omits params and adopts the creator's immutable terms.

## Drive and verify

Call `await_execution_event {"execution":<execution-ref>}` repeatedly.

- On `callout`, read `schema`, `context`, `prompt`, and `pending_id`. The
  pending id is a decimal string; preserve it exactly. Submit one matching
  JSON answer with the same execution reference and pending id.
- On `completed`, retain the returned `session` reference and stop driving.
- On `failed`, stop and report the reason.

Use `get_execution_status` for non-blocking lifecycle inspection and
`query_execution` for guest-defined read-only state. `stop_execution` is safe
to repeat after success and chooses withdrawal or termination from lifecycle.

Call `verify_session` with `mode:"light"` for portable cryptographic checks or
`mode:"full"` to replay the registered Wasm and recover terminal evidence. The
Host in the session reference identifies the receipt producer; a session id by
itself never silently selects evidence.
