# Local daemon API

The daemon exposes one typed JSON request surface at `$ARENA0_HOME/arena0.sock`.
`ARENA0_SOCKET` overrides this single endpoint. Every Host operation names its
local Host ID explicitly; identity and storage remain independent per Host.
`arena0-api` owns the DTOs, `arena0-daemon` dispatches them, and
`arena0-client::DaemonClient` is the shared Unix-socket client used by the CLI.

The same requests are available over [HTTP](http.md). `daemon.info` returns
`DaemonInfo { version, abi_version, uptime_secs, socket, http_url, ui }`;
`http_url` is the base HTTP URL, with MCP at `{http_url}/mcp`. The boolean
`ui` reports whether a UI directory was configured with `ARENA0_UI_DIR`;
it does not check whether its files still exist after startup.

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
`CalloutNotPending`, and `InputRejected`.

## Identity and custody

| Method | Params | Success |
|---|---|---|
| `id.show` | — | `Id` |

Each Host has exactly one identity, minted when the Host is created. A new
identity means a new Host (`hosts.open`).

Seeds never cross the socket. `IdInfo` contains public identity material only.

## Program catalog

| Method | Params | Success |
|---|---|---|
| `program.list` | — | `ProgramList` |
| `program.get` | `{program}` | `Program` |
| `program.import` | `{source}` | `Program` |
| `program.remove` | `{program}` | `Ack` |

`source` is either `{"path":"/absolute/file.wasm"}` (socket only) or
`{"upload":"<64-hex>"}` from `POST /uploads`. Programs require an
`application/wasm` upload. Missing uploads are `NotFound`; an octet-stream
upload is `BadRequest`.

`program` accepts a local name, an unambiguous content-hash prefix, or a full
`ProgramHash`. Program import validates the complete guest ABI and embedded
public metadata before it stores the exact Wasm bytes in the Host's SQLite
catalog. The local transport does not fetch programs; every selected Host must
have the same exact Wasm locally. `ProgramSummary.participants` declares
supported counts. Fixed programs use `{"kind":"exact","count":2}`,
while variable-size programs use `{"kind":"range","min":2,"max":64}`. The
requested participant count must fall within it. The `schema` of `program.get`
lists the program's declared `phases` in declaration order, each with `name`,
`description`, `is_default`, and `is_terminal`; the list is empty when the
program declares none. A session's `phase` is one of these names.

## Blobs

| Method | Params | Success |
|---|---|---|
| `blob.import` | `{source}` | `BlobImported` `{hash, length}` |
| `blob.export` | `{hash, path}` | `BlobExported` `{length}` |
| `blob.list` | — | `BlobList` `[{hash, length, linked}]` |

A blob is immutable content of at most 16 MiB, named by its BLAKE3 hash as 64
hex characters. Paths are absolute paths on the Host's machine.

`blob.import` with `{"source":{"path":"/absolute/file"}}` links the file in place: the Host hashes it once and records its
path, without copying it or reading it whole into memory. The file must stay
unchanged while executions use it; a program that checks what it receives
detects a changed file, but the Host does not. Importing the same content again
succeeds with the same hash and records the new path. A missing or unreadable
path, or content over the limit, is `BadRequest`.

`blob.import` with `{"source":{"upload":"<64-hex>"}}` copies an upload
into an owned file (`linked: false`), preferring an octet-stream upload over
a Wasm upload of the same hash. Content already held keeps its existing
record. Uploads are shared across Hosts and cleared at daemon start; the
owned copy survives that cleanup.

`blob.export` is socket-only and writes the blob to a new file at `path` and never replaces an
existing file (`BadRequest`). An unknown hash is `NotFound`; a blob whose file
is gone or shorter than its length is `BadRequest`.

`blob.list` returns every blob the Host stores, ordered by hash, without
filesystem paths. `linked` is true for a file imported in place and false
for an owned copy or a file received through execution.

An execution reads only the blobs its participant grants in `exec.new` and the
blobs it receives and commits. Pass a blob's `hash` and `length` to the program
as ordinary params. The Host never logs blob bytes. The CLI wraps these as
`arena0 blob import FILE`, `arena0 blob export HASH FILE`, `arena0 blob list`,
and `arena0 exec create PROGRAM --blob HASH` (repeatable).

## Open offers

`negotiation.offers` is a Host request with no params. It returns
`Offers(Vec<OpenOffer>)`, ordered by `(program_id, creator, negotiation_id)`.
Each `OpenOffer` contains `program_id`, `negotiation_id`, `creator`, `offer_seq`,
`target_size`, `params` (JSON), `deadline_unix_ms`, and `first_seen_ms` (the local
Unix-millisecond time this Host first observed that negotiation).

Each Host watches every program in its catalog, including programs imported
after startup. The list contains other peers' offers authenticated by the
creator's Active ticket; the Host's own offers are excluded. Discovery does
not initialize the guest: a Join still checks whether the offered params and
initial state are usable before accepting. Use the listed `creator` and
`negotiation_id` as an `exec.new` Join target.

New entries emit `negotiation.offer_seen`. Completion, expiration, and catalog
removal emit `negotiation.offer_closed` with `complete`, `expired`, or `unwatched`.
Listing removes expired entries immediately; otherwise a five-second sweep
expires them. The list holds at most 256 offers, evicting the oldest local
observation without a closure event when full. Refresh the list to reconcile
capacity eviction or a lagged event stream. A newer or equal offer sequence
updates an entry without changing its first-seen time or emitting another
`offer_seen` event.

## Execution

| Method | Params | Success |
|---|---|---|
| `exec.new` | `{exec_id, program, params?, ensemble, blobs?}` | `ExecCreated` |
| `exec.list` | — | `ExecList` |
| `exec.status` | `{exec_id}` | `Status` |
| `exec.inspect` | `{exec_id, events_from?, events_limit}` | `Inspection` |
| `exec.await` | `{exec_id, until}` | `Awaited` |
| `exec.next` | `{exec_id}` | `Next` |
| `exec.submit` | `{exec_id, pending_id, answer?}` | `Ack` |
| `exec.query` | `{exec_id, query?}` | `Query` |
| `exec.view` | `{exec, width, color, at_step?}` | `ExecView` |
| `exec.trace` | `{exec_id, from, to}` | `Trace` `[{certified_at_ms, entry, message}]` |
| `exec.cancel_creation` | `{exec_id}` | `Ack` |
| `exec.withdraw` | `{exec_id}` | `Ack` |
| `exec.terminate` | `{exec_id, reason}` | `Ack` |

`exec.view` renders the program's shared-state view during execution and after
termination. Terminal views use the saved shared state and remain available
after the live execution driver exits. Negotiating executions have no view yet.
The reply's `step` is the index of the latest agreed step the rendered state
includes, numbered from 0 like `exec.trace` entries and `exec.session.step`
events; `null` renders the initial state before step 0 is certified.

The reply's `view` has the four text `slots` and, for programs that provide
them, `blocks`: typed pieces a rich client can lay out, in the same order the
program produced them. Text slots stay the portable rendering; a client that
does not know a block ignores it. Every block is an object with a `kind`:

| `kind` | Fields |
| --- | --- |
| `facts` | `title?`, `items: [{label, value: Cell}]` |
| `table` | `title?`, `columns: [text]`, `rows: [[Cell]]` |
| `board` | `title?`, `rows`, `cols`, `cells: [Cell]` (row-major), `row_labels`, `col_labels` |
| `progress` | `label`, `value`, `max` |
| `roster` | `title?`, `entries: [{participant, status: Cell, detail?}]` |

A `Cell` is `{text, tone?, participant?}`. `tone` is one of `normal` (the
default), `muted`, `good`, `warn`, `bad`, `highlight`; clients choose the
colours. `participant` is an index into the committed ensemble, so a client
colours a participant's values consistently. Blocks appear only in the reply
when a program produces them; a view without blocks has no `blocks` field.

The Host validates blocks before replying. At most 16 blocks; tables of at
most 64 rows and 16 columns; boards of at most 32 rows and 32 columns whose
`cells` hold exactly `rows * cols` cells and whose label lists are empty or one
per row and column; rosters of at most 64 entries; every text at most 256
bytes; every `participant` below the ensemble size. A view that breaks a limit
fails the request with `Execution` and a message naming the limit. Nothing is
truncated.

`at_step` renders the shared state after that agreed step instead of the latest
state; `step` in the reply is then `at_step`. The latest agreed step is the
`step` of the last trace entry. The Host does not store past states. It
replays the agreed steps `0` through `at_step` in a fresh program instance,
checks the initial state and every step's `post_state` against the agreed
trace, and renders the result, so the cost grows with `at_step`. It works for
active and terminal executions. Errors:

- `BadRequest`: `at_step` is beyond the latest agreed step
  (`step 9 is beyond the latest agreed step 4`).
- `Execution`: the execution has no agreed step yet (a negotiating or
  activating execution returns the error a latest view returns), or the replay
  did not reproduce the agreed trace. The message names the step; nothing is
  rendered from a state that disagrees with the trace.

`blobs` lists hashes of imported blobs this participant grants the execution;
it defaults to none. An unknown hash is `NotFound`, before anything is
published. The grants are part of the request: retrying the same `exec_id`
with different grants is `BadRequest`. The MCP `start_execution` tool grants
no blobs yet.

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
receipt or stop report is durably available. It also carries the program's turn
at the agreed step: `writer`, the `PeerId` of the participant allowed to author
the next agreed message (`null` when the program admits none), and `phase`, the
declared name of the program's current phase (`null` for a program that
declares no phases). Every Host of the session reports the same `writer` and
`phase` at the same step. The program computes both from the agreed shared
state, and a status call fails rather than omit them when the program cannot
project them.

`exec.status` also reports two local times in Unix milliseconds:
`created_at_ms`, when the execution request was created, and `updated_at_ms`,
the execution's latest durable transition. Before the execution exists,
`updated_at_ms` is the activation record's latest change, and before that the
request's `created_at_ms`.

Terminal states retain their result: `Completed` carries `outcome` (JSON or
`null` when the program omits it), `Aborted` carries a `reason` string, and
`Failed` carries an optional `reason`. A failed request's reason takes precedence
over its execution's terminal cause when both exist.

`exec.list` returns `ExecList` entries as `ExecSummary`: `exec_id`,
`negotiation_id`, `program_id`, `lifecycle`, `session_id`, `step`,
`last_step_at_ms`, `participants`, `peers`, `pending_callout`,
`receipt_available`, `writer`, `phase`, `end`, `reason`, `outcome`,
`activation`, `created_at_ms`, and `updated_at_ms`. The lifecycle and session
facts match `exec.status`; `activation` matches `exec.inspect` and is `null`
until preparation starts. An open `pending_callout` contains `pending_id`,
`callout_index`, `name`, and the persisted local `opened_at_ms`; its prompt,
schema, and context remain in `exec.status`. `step`, `last_step_at_ms`, and
`participants` are `null` before an execution aggregate exists. `writer`
and `phase` are `null` for terminal executions in both list and status.
`last_step_at_ms` is also `null` before the first agreed step. `reason` is
populated for aborted or failed executions; `outcome` is completed program
JSON. List reads use durable index columns. A non-terminal execution's first
list projection at a new step may decode its state to fill the turn memo;
terminal entries never need this decode.

The top-level `end` object reports local confirmation of the terminal result:
`{"phase":"open","unconfirmed":[]}`, `{"phase":"ending","unconfirmed":["<peer-id>"]}`,
or `{"phase":"ended","unconfirmed":[]}`. `ended` can retain unconfirmed
peers when the confirmation window expires; receipt availability is independent
of this phase.
`exec.session.end_progress` events carry `{phase, unconfirmed}` after each
durable change to these local handshake facts.

`exec.inspect` is a bounded, Host-local diagnostic projection for operator
interfaces. It returns `exec.status`, durable activation facts, participant
peer IDs and ticket commitments, the offer `params` as JSON (every participant
of the offer signed them), and summaries of event dispatch records.
Its status includes the local participant's callout context and terminal outcome.
It never returns event payloads, replacement local state, signatures, or keys.
Event records are the latest
store-bounded window. The response exposes the page through `events_from`,
`events`, `events_total`, and `events_next`; `events_total` reveals when older
records are omitted.
Inspection data is local diagnostic evidence, not a protocol receipt or
semantic system-event stream.

`exec.trace` returns the agreed steps in `[from, to)`, each as
`{certified_at_ms, entry, message}`. `entry` is the portable trace entry.
`message` is `null` for session start. For message steps it is
`{"Json": <decoded JSON>}` or `{"Undecodable":{"error":"..."}}`, decoded
using the program's first Borsh message schema. Decode errors describe the
schema mismatch without payload bytes. A missing schema reports
`program declares no message schema`.
`certified_at_ms` is the local time, in Unix milliseconds, at which this Host
durably stored the step. It is a local observation that differs between Hosts
and is not part of the trace entry, the trace hashes, or any receipt.

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
the count.

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
signed counteroffer when the current offer differs. Create requests validate
and store offer parameters before negotiation.

### Agent values

`exec.submit` returns `CalloutNotPending` when its pending ID is no longer the
current callout. A competing human or agent answer may have consumed it. Fetch
the next decision point; do not resubmit the stale answer or terminate an
otherwise healthy execution. This category is also preserved for a stale
answer already queued at the execution actor. Other validation, storage, and
execution errors remain distinct.

`pending_id` is the open callout's `CalloutId`. When an ID is kept, replaced,
or consumed is specified once, in
[execution and agreement](../protocol-architecture.md#10-execution-and-agreement)
and [durable delivery](../protocol-architecture.md#durable-delivery). While an
answered result is staged for agreement, the committed callout stays visible; a
duplicate submission waits for agreement and then returns `CalloutNotPending`.
`pending_callout` in `exec.status` contains `pending_id`, `callout_index`,
`name`, `prompt`, `schema`, and `context`, with the same values as `exec.next`.
Names, prompts, and answer schemas come from the program schema; context is
guest-produced JSON for the local participant.

`exec.submit` returns `InputRejected` when the pending callout still belongs to
the execution but the program rejects the answer. The response message carries
the bounded program reason when one is available. The rejection does not change
state, advance the event position, consume the `pending_id`, or emit
`exec.session.callout_answered`; submit a corrected answer with the same
`pending_id`. Invalid JSON or schema values are rejected at the API boundary
with `Schema` before the program runs.

`params`, callout answers, query values, and terminal projections are JSON.
The daemon validates agent inputs against the program's public JSON Schema.
Only generated guest code performs concrete DTO conversion to and from Borsh.

`exec.next` blocks until it can return one of:

- `Callout { pending_id, callout_index, name, prompt, schema, context }`;
- `Completed { session_id, outcome? }`;
- `Failed { reason }`.

`pending_id` is always a decimal JSON string, including in `exec.next`,
`exec.status`, and `exec.submit`. It is an opaque callout identity; keep
the string unchanged when submitting an answer. This avoids precision loss in
JSON clients whose number type cannot represent every `u64` value.

Guest signing never reaches the client; the Host signs synchronously with its
custodied identity or execution key inside the local handler dispatch.

Peer delivery between Hosts is not part of this API; the protocol
architecture specifies it under
[durable delivery](../protocol-architecture.md#durable-delivery). A pending
callout is stored with execution state and keeps its `pending_id` and guest
context across restart, so `exec.next` can return the same callout again.

Receipt publication exposes the terminal result and portable artifact to
clients immediately. The `end` object in `exec.status` then tracks which remote
participants have not yet confirmed the same conclusion; the `open`, `ending`,
and `ended` phases and their confirmation rules are specified in
[terminal evidence and publication](../protocol-architecture.md#terminal-evidence-and-publication).

## Receipts

| Method | Params | Success |
|---|---|---|
| `receipt.get` | `{receipt}` | `Receipt` containing a `ReceiptArtifact` |
| `receipt.import` | `{receipt}` | `ReceiptList` |
| `receipt.list` | — | `ReceiptList` |
| `receipt.verify` | `{receipt}` | `Verified` |

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
replaced by these references and artifacts. Older evidence and databases
require their matching older release; the current format and schema versions
are listed with the
[preserved invariants](../protocol-architecture.md#14-preserved-invariants).

`receipt.verify` performs [portable verification](../protocol-architecture.md#11-receipts-and-verification)
only, without loading or executing Wasm. `Verified` carries the receipt summary:
`receipt_id`, `program_id`, `session_id`, the ordered `ensemble`, `steps`,
`terminal`, and `outcome_borsh`. `terminal` is the receipt's own termination,
`"Completed"` or `{"Stopped":{"cause":...}}` with the exact stop cause. The
Host keeps outcome Borsh bytes opaque, so a completion carries authenticated
`outcome_borsh` bytes and a stop carries `null`. There is no program execution
endpoint.

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
reconnecting starts at the current stream position. Frame order describes daemon observation,
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
