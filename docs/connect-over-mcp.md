# Connect one harness to an Ensemble

Use one MCP server entry for the local arena0 Ensemble. The endpoint is
stateless and does not bind a client to a current Host.

## Start the service

Start the default two-Host Ensemble and expose its MCP endpoint:

```bash
arena0 serve --mcp-listen 127.0.0.1:7330
```

Configure `http://127.0.0.1:7330/mcp` once in the harness. If
`ARENA0_MCP_TOKEN` was set when the service started, send its value as a
bearer token on that connection. Authentication selects no Host.

## Assign a Host

Call `open_host` before any Host-scoped operation. Pass a stable caller
identity in `user_agent`; use the actual harness name and version. Omit `id` to
create a new Host namespace or include the id to reopen an existing one:

```json
{"user_agent":"claude-code/1.0"}
```

```json
{"id":"host-01","user_agent":"claude-code/1.0"}
```

The result is self-contained public Host metadata:

```json
{
  "host": {"id":"host-01"},
  "peer_id": "<peer-id>",
  "user_agent": "claude-code/1.0"
}
```

Retain the returned `host` object and pass it unchanged in every later
program, execution, admission, and session reference. If a later operation
reports that the retained id is absent, first call `open_host` again with the
retained id and the same user agent so a lazy namespace restart can recover
the durable identity. Only when that fails, and the user or task permits
replacing the participant, call `open_host` without `id` to create a fresh
Host and replace all references. Keep the returned `peer_id` and `user_agent`
for display and diagnostics.

Do not configure a separate MCP server entry for each Host. The endpoint is
selected by the server URL; Host selection is explicit in each tool argument.

Attach `arena0 monitor` in another terminal to inspect all Hosts, their program
views, pending callouts, and safe MCP call activity. Select an entry Host with
`arena0 --host NAME monitor` when the service does not include `host-01`.
The monitor does not require a particular harness or claim that a client owns
a Host.

A human can answer one pending callout from the monitor while an agent is
working. An agent whose retained answer loses that race receives
`CalloutNotPending`; fetch the next decision point instead of terminating the
execution or resubmitting that answer. There is no input reservation or pause
of the external harness. Other validation and execution errors remain errors.

## Select the Hosts and program

Call `list_programs` for the assigned Host:

```json
{"host":{"id":"host-01"}}
```

Select one exact returned `program_id` and inspect it with its Host reference:

```json
{
  "program": {
    "host": {"id":"host-01"},
    "program_id": "<program-id>"
  }
}
```

Every Host selected for an execution must already contain that exact program.
The local MCP surface does not transfer Wasm between Hosts.

## Negotiate and activate

The creator supplies the exact other Host ids:

```json
{
  "program": {
    "host": {"id":"host-01"},
    "program_id": "<program-id>"
  },
  "params": {"rounds":3},
  "ensemble": {
    "mode":"explicit",
    "hosts":[{"id":"host-02"}]
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
    "mode":"join",
    "creator":{"id":"host-01"},
    "negotiation_id":"<negotiation-id>"
  }
}
```

Retain each returned `execution` reference. The executions remain negotiating
until every participant commits the exact offer. A joiner normally omits
params and adopts the creator's immutable terms.

## Drive every Host through one client

Keep one driver state per execution reference. For each nonterminal execution:

1. Call `await_execution_event` with its retained `execution` reference.
2. On `callout`, read `pending_id`, `name`, `prompt`, `schema`, and `context`,
   then immediately call `answer_callout` with one matching JSON answer.
3. On `waiting`, poll again and advance another Host's execution when one is
   available. Waiting is an immediate polling result; this flow has no timeout
   argument or timeout guarantee.
4. Stop advancing after `completed` or `failed`.

Do not wait for every Host to produce a callout before answering the first one.
Protocol progress may depend on that answer. The same loop applies to any
number of participants through the one MCP endpoint.

## Verify the result

Every completed event returns a Host-specific `SessionRef`. The shared
`session_id` can match across the Ensemble, while the embedded Host reference
identifies which retained receipt to verify:

```json
{
  "session": {
    "host":{"id":"host-01"},
    "session_id":"<session-id>"
  },
  "mode":"light"
}
```

Call `verify_session` for each returned session. Use `light` for portable
cryptographic checks or `full` to replay the exact admitted Wasm. Keep all
references returned by the tools; a session id alone never silently selects a
receipt producer.
