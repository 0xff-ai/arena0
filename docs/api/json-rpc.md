# Local daemon API

The daemon exposes one typed JSON request surface at `$ARENA0_HOME/arena0.sock`.
`ARENA0_SOCKET` overrides this single endpoint. Every Host operation names its
local Host ID explicitly; identity and storage remain independent per Host.
`arena0-api` owns the DTOs, `arena0-daemon` dispatches them, and
`arena0-client::DaemonClient` is the shared Unix-socket client used by the CLI.

## Framing

Each request and response is UTF-8 JSON preceded by a big-endian `u32`
length. A normal connection carries one request and one response.
`events.subscribe` sends `Subscribed` and then a bounded stream of `EventFrame`
values until the connection closes. `activity.subscribe` similarly sends
`ActivitySubscribed`, followed by daemon-wide `ActivityFrame` observations.

A Host request wraps the existing operation:

```json
{"method":"host.call","params":{"host":"host-01","request":{"method":"exec.status","params":{"exec_id":"<64-hex>"}}}}
```

Daemon operations such as `{"method":"daemon.info"}` have no Host target.
The identity, program, execution, and receipt methods below are inner
`HostRequest` operations sent in `host.call`. An unknown or malformed Host ID
fails without selecting a different Host or creating a namespace. Use
`hosts.open` to provision or reopen a Host explicitly.

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

`exec.view` renders the program's shared-state view during execution and after
termination. Terminal views use the saved shared state and remain available
after the live execution driver exits. Negotiating executions have no view yet.

The client generates a unique `exec_id` before calling `exec.new`. The Host
uses that exact identifier, allowing the client to send
`exec.cancel_creation` with the same identifier if the creation response is
lost or interrupted. This cleanup follows an activation race if necessary and
acknowledges only after the attempted execution is stopped. `exec.withdraw`
remains negotiation-only; use `exec.terminate` for a formed session.
`exec.new` returns immediately while negotiation continues. `ExecCreated`
echoes `exec_id` and contains an optional `negotiation_id`, optional
`session_id`, the public lifecycle, and an optional queue position. Create and
targeted Join requests have a negotiation ID in this response. An open Join
returns `negotiation_id: null` until an authenticated offer is selected; the
same field becomes available through `exec.status` after the durable target
binding. The `exec.created` event omits the field while it is unknown.

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

The socket shape is the externally tagged Serde representation of the current
admission forms:

```json
{"Create":{"participant_count":2}}
{"Join":{"target":null}}
{"Join":{"target":{"creator":"<peer-id>","negotiation_id":"<negotiation-id>"}}}
```

`Create` chooses the total participant count, including the local Host. The
daemon creates the negotiation ID and checks that the selected program accepts
the count. The older `Explicit` form, `{"Explicit":{"peers":[...]}}`, remains
available for launcher compatibility.

`Join` with `target: null` discovers the first usable offer on the program
topic. A supplied target restricts discovery to that creator and negotiation.
The joiner validates the offer and the creator's signed Active ticket before
the daemon durably binds an open request to the target. Binding is compare and
set: retrying the same target is safe, while a different target cannot replace
the first one. The local ticket is signed only after this authenticated,
durable binding.

The request's `params` value is optional for a join. Without it, the Host
accepts the creator's authenticated offer parameters. With it, the Host treats
the value as a local preference, signs only a matching offer, and emits a
signed counteroffer when the current offer differs. Create and Explicit
requests validate and store offer parameters before negotiation.

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

## Daemon lifecycle and Host information

| Method | Scope | Params | Success |
|---|---|---|---|
| `daemon.info` | Daemon | — | `DaemonInfo` |
| `hosts.list` | Daemon | — | `Hosts`, containing `HostStatus` entries |
| `hosts.open` | Daemon | `{id?,user_agent}` | `HostOpened`, containing `HostInfo` |
| `daemon.stop` | Daemon | — | `Ack` |
| `activity.subscribe` | Daemon | — | `ActivitySubscribed`, then `ActivityFrame` stream |
| `host.info` | Host | — | `HostStatus` |
| `events.subscribe` | Host | `{filter}` | `Subscribed`, then `EventFrame` stream |

The CLI projects process version, ABI version, uptime, and the Unix socket
from `DaemonInfo`. Internal adapter endpoints are omitted from CLI output.
`HostStatus` contains `host` (`id`, cryptographic `peer_id`, and optional
`user_agent`), public `transport_key`, program count, and active execution count.
Host metadata contains no socket path.

`hosts.open` uses the daemon's serialized provisioning owner. A supplied ID
reopens its durable namespace; an omitted ID creates a fresh one. The required
user agent identifies caller software: it must be nonblank, contain no control
characters, and fit within 256 UTF-8 bytes. Opening is acknowledged only after
recovery and publication in the daemon's authoritative roster.

The local CLI exposes this operation as `arena0 hello --user-agent NAME/VERSION`.
It uses a deterministic Host name derived from `ARENA0_CONTEXT`, or from
`codex:<CODEX_THREAD_ID>` when the explicit context is absent. `--json` returns
`HostInfo` with `id`, `peer_id`, and `user_agent`. No access token is returned on
this Unix-socket path. Repeated calls reuse the context's durable namespace;
ordinary CLI commands address that Host automatically without `--host`.
See [context selection](../protocol-architecture.md#12-daemon-and-agent-api)
for validation and precedence.

One daemon owns each home. Its single listener serves concurrent requests and
Host subscriptions. `daemon.stop` acknowledges before beginning coordinated
shutdown; shutdown drains owned connection tasks, Hosts, transport, and stores.

Event tags and filtering are documented in [events/README.md](events/README.md).

`activity.subscribe` observes internal adapter calls across the complete local daemon;
subscribe once through the shared daemon endpoint. Each frame has
`boot_id`, `seq`, `ts`, `kind`, and `data`. Started records carry a call ID,
tool name, and optional Host/execution correlation. Finished records carry that call ID,
elapsed milliseconds, and a safe result class. `Interrupted` means the server
dispatch future ended before an outcome was observed; it is not proof that
an action failed or a client received no response. No arguments, answers, or
result bodies appear.

Activity is bounded and live-only. A lag record reports dropped observations;
reconnecting cannot replay them. Frame order describes daemon observation,
not multiparty protocol causality. Read current execution state and evidence
through the ordinary Host methods after a gap. Adapter activity remains separate
from semantic `EventFrame` values and durable receipt facts.

## Boundary invariants

1. The Host owns identity, signing, sandbox, and persistence.
2. Adapters dispatch into the same Host service operations.
3. Program values cross as typed JSON; program Borsh remains opaque to the Host.
4. Receipt retrieval uses an exact content ID or the addressed Host's local session publication.
5. The public socket contains no remote discovery, addressing, or transfer
   surface.
