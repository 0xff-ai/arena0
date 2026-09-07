# Local Host API

The daemon exposes one typed JSON request surface on each Host's Unix socket.
`arena0-api` owns the DTOs, `arena0-daemon` dispatches them, and
`arena0-client::DaemonClient` is the shared Unix-socket client used by the CLI.

## Framing

Each request and response is UTF-8 JSON preceded by a big-endian `u32`
length. A normal connection carries one request and one response.
`events.subscribe` sends `Subscribed` and then a bounded stream of `EventFrame`
values until the connection closes. `activity.subscribe` similarly sends
`ActivitySubscribed`, followed by daemon-wide `ActivityFrame` observations.

The request shape is:

```json
{"method":"exec.status","params":{"exec_id":"<64-hex>"}}
```

Requests reject unknown fields. Responses are the Serde representation of:

```rust
type Response = Result<ResponseOk, ApiError>;
```

`ApiError` contains a stable category and a human-readable message. Categories
are `NotFound`, `BadRequest`, `Ambiguous`, `Schema`, `Negotiation`,
`Execution`, `Verification`, `Storage`, `Timeout`, `Internal`, and
`CalloutNotPending`.

## Identity and custody

| Method | Params | Success |
|---|---|---|
| `id.new` | `{label?}` | `Id` |
| `id.list` | — | `IdList` |
| `id.show` | `{id}` | `Id` |
| `id.remove` | `{id}` | `Ack` |

The active identity is the Host's durable protocol identity and cannot be
removed through `id.remove`; rotation requires a lifecycle-aware operation.

Seeds never cross the socket. `IdInfo` contains public identity material only.

## Program catalog

| Method | Params | Success |
|---|---|---|
| `program.list` | — | `ProgramList` |
| `program.get` | `{program}` | `Program` |
| `program.import` | `{wasm}` | `Program` |
| `program.remove` | `{program}` | `Ack` |

`program` accepts a local name, an unambiguous content-hash prefix, or a full
`ProgramHash`. Program import validates the complete guest ABI and embedded
public metadata before it stores the exact Wasm bytes in the Host's SQLite
catalog. The local transport does not fetch programs; every selected Host must
have the same exact Wasm locally. `ProgramSummary.participants` declares
supported counts. Fixed programs use `{"kind":"exact","count":2}`,
while variable-size programs use `{"kind":"range","min":2,"max":64}`. The explicit
ensemble size must fall within it.

## Execution

| Method | Params | Success |
|---|---|---|
| `exec.new` | `{exec_id, program, params?, ensemble}` | `ExecCreated` |
| `exec.list` | — | `ExecList` |
| `exec.status` | `{exec_id}` | `Status` |
| `exec.inspect` | `{exec_id}` | `Inspection` |
| `exec.await` | `{exec_id, until}` | `Awaited` |
| `exec.next` | `{exec_id}` | `Next` |
| `exec.submit` | `{exec_id, pending_id, answer?}` | `Ack` |
| `exec.query` | `{exec_id, query?}` | `Query` |
| `exec.view` | `{exec, width, color}` | `ExecView` |
| `exec.trace` | `{exec_id, from, to}` | `Trace` |
| `exec.cancel_creation` | `{exec_id}` | `Ack` |
| `exec.withdraw` | `{exec_id}` | `Ack` |
| `exec.terminate` | `{exec_id, reason}` | `Ack` |

The client generates a unique `exec_id` before calling `exec.new`. The Host
uses that exact identifier, allowing the client to send
`exec.cancel_creation` with the same identifier if the creation response is
lost or interrupted. This cleanup follows an activation race if necessary and
acknowledges only after the attempted execution is stopped. `exec.withdraw`
remains negotiation-only; use `exec.terminate` for a formed session.
`exec.new` returns immediately while negotiation continues. `ExecCreated`
echoes `exec_id` and contains the required `negotiation_id`, optional
`session_id`, the public lifecycle, and an optional queue position.

`exec.status` exposes the committed `session_id` as soon as activation commits.
Once session progress exists, its `session` object also reports the public step,
committed participants, pending callout summary, and whether this Host's
receipt or stop report is durably available.

`exec.inspect` is a bounded, Host-local diagnostic projection for operator
interfaces. It returns `exec.status`, durable activation facts, participant
peer IDs and ticket commitments, and summaries of private handler crossings.
It never returns private payloads, replacement local state, signatures, keys,
parameters, outcomes, or callout context. Private summaries are the latest
store-bounded window; `private_total` reveals when older summaries are omitted.
Inspection data is local diagnostic evidence, not a protocol receipt or
semantic system-event stream.

### Admission

The socket shape is the externally tagged Serde representation of exactly two
variants:

```json
{"Explicit":{"peers":["<peer-id>","..."]}}
{"Join":{"creator":"<peer-id>","negotiation_id":"<negotiation-id>"}}
```

`Explicit` creates an offer for the exact peers. `Join` accepts only the exact
creator and negotiation. There is no implicit discovery or fallback. The
request's `params` value is optional for a join. Without it, the Host accepts
the creator's authenticated offer parameters. With it, the Host treats the
value as a local preference, signs only a matching offer, and emits a signed
counteroffer when the current offer differs. Explicit requests validate and
store the offer parameters before negotiation.

### Agent values

`exec.submit` returns `CalloutNotPending` when its pending ID is no longer the
current callout. A competing human or agent answer may have consumed it. Fetch
the next decision point; do not resubmit the stale answer or terminate an
otherwise healthy execution. This category is also preserved for a stale
answer already queued at the execution actor. Other validation, storage, and
execution errors remain distinct.

`params`, callout answers, query values, and terminal projections are JSON.
The daemon validates agent inputs against the program's public JSON Schema.
Only generated guest code performs concrete DTO conversion to and from Borsh.

`exec.next` blocks until it can return one of:

- `Callout { pending_id, callout_index, name, prompt, schema, context }`;
- `Completed { session_id, outcome? }`;
- `Failed { reason }`.

`pending_id` is always a decimal JSON string, including in `exec.next`,
`exec.status`, and `exec.submit`. It is an opaque continuation identity; keep
the string unchanged when submitting an answer. This avoids precision loss in
JSON clients whose number type cannot represent every `u64` value.

Signing requests never reach the client; the Host answers them with its
custodied execution key.

The Host persists an authenticated inbound execution frame before it
acknowledges transport responsibility. The execution actor resolves the frame
through the reducer, which commits state, trace or private records, timers, and
outbox effects in one SQLite transaction. Outbox delivery uses leases and
retries, and expired leases are recovered when the store opens. A pending
callout retains its `pending_id` and guest context across a restart, so
`exec.next` can return the same callout again. Terminal proof collection is internal; the socket exposes the terminal result
and the authenticated portable artifact.

## Receipts

| Method | Params | Success |
|---|---|---|
| `receipt.get` | `{receipt}` | `Receipt` containing a `ReceiptArtifact` |
| `receipt.import` | `{receipt}` | `ReceiptList` |
| `receipt.list` | — | `ReceiptList` |
| `receipt.verify` | `{receipt,full}` | `Verified` |

`ReceiptRef` has three externally tagged JSON forms:

- `{"Stored":"<receipt-id>"}` selects an exact content ID (64 lowercase hex digits).
- `{"Produced":"<session-id>"}` selects the addressed Host's locally produced
  artifact for that session. Imported reports do not participate in this lookup.
- `{"Inline":<artifact>}` supplies a portable artifact directly.

The artifact has `{"kind":"receipt"|"stop_report","body":...}` shape. Completion
and unanimously agreed program stops are canonical receipts. Unilateral stops
are signed reports; several can coexist for one session. `receipt.list` includes
`receipt_id`, `session_id`, `kind`, `program_id`, `completed`, and local `provenance`.
There is no artifact producer field. Identical imports deduplicate by content ID;
local production and import facts independently yield `Produced`, `Imported`, or
`Both`. Each `Verified` response includes the exact verified `receipt_id`.

The earlier `{key:{session_id,producer}}` API and producer-sealed JSON format are
replaced by these references and artifacts. Receipt format and store schema are
version 2; older evidence requires its matching older release.

Light verification returns cryptographically checked evidence without loading
Wasm. Full verification is served by the daemon and replays the exact program.
A completed JSON outcome is available only after full replay because the Host
otherwise treats the receipt's Borsh outcome bytes as opaque.

The `Verified` response carries a tier-specific `result`: `Light` contains a
completed `outcome_borsh` or an exact `Stopped { cause }`, while `Full` contains
both `outcome_borsh` and the replayed `outcome_json` for completion. Stopped
results never carry an outcome field; `cause` preserves either authenticated
unilateral evidence or a shared N-of-N stop commitment.

## Host lifecycle and events

| Method | Params | Success |
|---|---|---|
| `daemon.info` | — | `DaemonInfo` |
| `hosts.list` | — | `Hosts`, containing `DaemonInfo` for each supervised Host |
| `daemon.stop` | — | `Ack` |
| `events.subscribe` | `{filter}` | `Subscribed`, then `EventFrame` stream |
| `activity.subscribe` | — | `ActivitySubscribed`, then `ActivityFrame` stream |

`DaemonInfo` contains a `host` object with the selected Host's local `id`,
`PeerId`, and optional caller `user_agent`, followed by its public identity key,
version, ABI version, uptime, socket, program count, and active execution count.
When one Host asks to stop, the process supervisor coordinates shutdown of the
whole local Ensemble.

Event tags and filtering are documented in [events/README.md](events/README.md).

`activity.subscribe` observes MCP calls across the complete local daemon;
subscribe through one Host socket rather than once per Host. Each frame has
`boot_id`, `seq`, `ts`, `kind`, and `data`. Started records carry a call ID,
tool name, and optional Host/execution correlation. Finished records carry that call ID,
elapsed milliseconds, and a safe result class. `Interrupted` means the server
dispatch future ended before an outcome was observed; it is not proof that
an action failed or a client received no response. No arguments, answers, or
result bodies appear.

Activity is bounded and live-only. A lag record reports dropped observations;
reconnecting cannot replay them. Frame order describes daemon observation,
not multiparty protocol causality. Read current execution state and evidence
through the ordinary Host methods after a gap. MCP activity remains separate
from semantic `EventFrame` values and durable receipt facts.

## MCP projection

`arena0d` exposes one Streamable HTTP endpoint at `/mcp` for the complete local
Ensemble. MCP auth is daemon-wide and the endpoint is stateless.
Authentication never selects a Host; call `open_host` with
`{id?,user_agent}` to receive an assigned Host reference and public metadata.
Configure the endpoint once in an MCP harness and carry that reference through
the same client. Interleave the Host driver states when
`await_execution_event` returns `waiting`; do not open one MCP session per
Host. See
[Connect one harness to an Ensemble](../connect-over-mcp.md).

The stable tool set is:

- discovery: `open_host`, `list_programs`, `inspect_program`;
- execution: `start_execution`, `get_execution_status`,
  `await_execution_event`, `answer_callout`, `query_execution`,
  `stop_execution`;
- evidence: `verify_session`.

Host information is represented by the `open_host` result. Negotiation
withdrawal and active termination are one lifecycle-aware `stop_execution`
operation. Params updates are unsupported because negotiation terms are
immutable. Trace and raw receipt retrieval remain operator Unix API/CLI
operations rather than agent tools.

MCP admission names daemon-local Hosts and uses an adjacent tag:

```json
{"mode":"explicit","hosts":[{"id":"host-02"}]}
{"mode":"join","creator":{"id":"host-01"},"negotiation_id":"<negotiation-id>"}
```

Mode-specific unknown or conflicting fields are rejected before a socket
request is sent.

## Boundary invariants

1. The Host owns identity, signing, sandbox, and persistence.
2. MCP and Unix-socket adapters dispatch into the same Host service operations.
3. Program values cross as typed JSON; program Borsh remains opaque to the Host.
4. Receipt retrieval uses an exact content ID or the addressed Host's local session publication.
5. The public socket contains no remote discovery, addressing, or transfer
   surface.
