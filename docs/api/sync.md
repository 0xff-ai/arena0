# Summary synchronization

`GET /sync` streams every open Host's execution, step, receipt, program and
blob rows over HTTP/1.1 Server-Sent Events. Each event is named `sync`; its
JSON data is an `arena0_api::SyncFrame`. The connection uses the same
keepalives and authorization as [the HTTP API](http.md).

Hosts load concurrently, with projection concurrency bounded by available
parallelism. Frames from different Hosts can interleave. For each Host a
fresh connection sends these frames in order:

1. `host`: `HostInfo`, `transport_key`, `online` and `boot_id`.
2. `reset`: discard all rows for this Host.
3. `rows`: the full snapshot and its `seq`, including an empty snapshot.
4. `synced`: the Host's snapshot is complete through `seq`.

Live `rows` follow. Hosts opened later join the existing connection with
the same initial sequence. Host start and stop events also send `host`
frames with the new online state.

## Applying rows

Row identity is scoped to the enclosing frame's `host`:

| `row` | Identity and application |
| --- | --- |
| `exec` | Upsert `exec_id`; equals the corresponding `exec.list` summary |
| `steps` | Overwrite the execution's range starting at `from_step`; preserve other steps |
| `receipt` | Upsert `receipt_id`; equals the corresponding `receipt.list` entry |
| `program` | Upsert `summary.program_hash`; carries the `program.get` detail, including its schema |
| `blob` | Upsert `hash`; equals the corresponding `blob.list` entry |
| `program_removed` | Remove `program_hash` from the active catalog |

Steps have parallel `certified_at_ms` and `state_prefix` arrays of equal
length. Array element `i` belongs to step `from_step + i`. Times are the
local Host's durable certification times, equal to `exec.trace` values.
Each prefix is the first four bytes of the post-state hash interpreted as
a big-endian unsigned integer. Prefix equality is not proof of full state
equality: collisions can hide a divergence. Use full hashes from the
session's trace to compare states conclusively.

Execution and receipt rows are never deleted. A full snapshot omits
removed programs. Snapshot sizes use the list read limits (4,096
executions and receipts, 1,024 active programs, all blobs).

## Resuming from applied state

After applying a `rows` frame completely, record that Host's `boot_id`
and `seq`. `synced` also establishes the cursor when catch-up was empty.
The cursor is a JSON object keyed by Host id:

```json
{"a":{"boot_id":"host-lifetime-id","seq":1842},"b":{"boot_id":"other-lifetime-id","seq":37}}
```

Encode the JSON bytes as lowercase hex and connect to
`GET /sync/{cursor}`. Invalid hex or invalid cursor JSON returns HTTP 400
with a `text/plain` parse error before a stream starts. The cursor belongs
to client state that has been **applied**, not merely received; SSE's
`Last-Event-ID` is not used.

For each Host, a matching lifetime and retained change history produce
only the current rows for keys changed after the cursor, then `synced`.
An empty catch-up omits `rows`. A missing Host cursor, a different
`boot_id`, a cursor ahead of the current head, or a gap older than the
8,192-key in-memory ring produces `reset`, snapshot `rows`, then `synced`.
Restarting the store changes its lifetime id and invalidates its cursors.

Sequences follow committed store writes. The daemon captures the head
before projecting rows, so a row may reflect a later commit than the
frame's sequence. That later commit is still replayed at a larger
sequence. Apply upserts idempotently and never interpret a frame as an
atomic cross-row or cross-Host database snapshot.

The connection channel holds 64 frames. Full channels block sending;
subsequent writes coalesce through the same ring-or-snapshot catch-up.
Slow readers do not receive a "lagged, reload" instruction. If projecting
a row fails, the connection ends without advancing past that projection;
reconnect using the last applied cursor.

## Live observations and lifetime

After a Host's `synced`, `observed` frames forward its semantic
`EventFrame`s subscribed at connection establishment. They never advance
the row cursor. Daemon activity is forwarded once per connection and can
interleave with initial Host frames.

`observed: "offers"` replaces the complete set of open offers for its
Host. The set arrives on connect and after `negotiation.offer_seen` and
`negotiation.offer_closed`. Semantic events and activity are live-only:
they are not replayed on resume. Broadcast lag discards lost observations
without synthesizing events; durable rows still catch up independently.

One connection task owns every per-Host task. Client disconnect or daemon
shutdown aborts and joins those tasks, including idle watches and blocked
sends. The stream owns no durable write operation.
