# Connect one harness to an Ensemble

Use one MCP server entry for the whole local arena0 Ensemble. The endpoint
returns explicit Host references; it does not bind the MCP client to a current
Host.

## Start the service

Start the default two-Host Ensemble and expose its MCP endpoint:

```bash
arena0 serve --mcp-listen 127.0.0.1:7330
```

Configure `http://127.0.0.1:7330/mcp` once in the harness. Do not configure a
separate MCP server entry for each Host. If `ARENA0_MCP_TOKEN` was set when the
service started, send its value as a bearer token on that one endpoint.

The endpoint is stateless. A Host is selected only by passing a `host`,
`program`, `execution`, or `session` reference returned by an arena0 tool.

## Select the Hosts and program

Call `list_hosts` first. Retain each returned `host` object instead of copying
its name into a new shape:

```json
{
  "hosts": [
    {"host": {"name": "host-01"}, "peer_id": "..."},
    {"host": {"name": "host-02"}, "peer_id": "..."}
  ]
}
```

Call `list_programs` once for each selected Host. Select the same
`program_id` from every result. Programs are admitted locally, so one Host's
`ProgramRef` cannot be used for another Host.

## Negotiate and activate

Start the creator with the exact other Hosts:

```json
{
  "program": {
    "host": {"name": "host-01"},
    "program_id": "<program-id>"
  },
  "params": null,
  "ensemble": {
    "mode": "explicit",
    "hosts": [{"name": "host-02"}]
  }
}
```

Retain the returned `execution` and `negotiation_id`. Start the other Host by
joining that exact negotiation with its own `ProgramRef`:

```json
{
  "program": {
    "host": {"name": "host-02"},
    "program_id": "<program-id>"
  },
  "params": null,
  "ensemble": {
    "mode": "join",
    "creator": {"name": "host-01"},
    "negotiation_id": "<negotiation-id>"
  }
}
```

The executions are negotiating until every participant commits the offer.
Poll `get_execution_status` for each returned `ExecRef` when the harness needs
to display this progress. A Session exists only after activation; active
statuses from every Host must contain the same `session_id`.

## Drive every Host through one client

Keep one driver state per Host and advance them through the same configured MCP
server. For each nonterminal Host:

1. `await_execution_event` with that Host's `ExecRef`.
2. If the event is `callout`, immediately call `answer_callout` with its
   `pending_id` and a JSON answer.
3. If the event is `waiting`, advance another Host before polling this one
   again.
4. Stop advancing that Host when the event is `completed` or `failed`.

`await_execution_event` returns `waiting` immediately when no durable event is
ready. This lets a harness interleave all Host drivers through one initialized
MCP client.
Do not wait for every Host to return a callout before answering the first one:
a program can expose participant decisions in sequence, and protocol progress
can depend on an earlier answer. This coordination does not create a second
server entry, a second MCP session, or a connection assigned to a Host.

The same rule applies to more than two participants. A five-party execution
has five independent Host driver loops and one MCP server entry.

## Verify the result

Every completed event returns a Host-specific `SessionRef`. The `session_id`
must match across the Ensemble, while the embedded Host reference identifies
which independently produced receipt to verify.

Call `verify_session` for every returned `SessionRef`. Use `light` to verify
the signed evidence without executing Wasm, or `full` to replay the receipt
with the exact admitted program. A successful result reports the participant
set, shared step count, and terminal evidence.

Keep the references returned by the tools throughout this flow. arena0 has no
seat token, implicit current Host, remote program transfer, or pre-activation
Session.
