# Host event design

The daemon records each Host occurrence at the code that owns it. One internal
`HostEvent` produces a redacted structured trace record and a typed `EventFrame`
for clients on that Host's Unix socket. These observations describe state
changes. They never drive the state machine.

## Keep four contracts

| Contract | Purpose | Delivery |
|---|---|---|
| `arena0_node::SessionMessage` | Reliable runtime-to-supervisor work and state handoff | Bounded, reliable `mpsc` |
| daemon `HostEvent` | One validated negotiation or execution occurrence | Synchronous input to `Events::emit` |
| `arena0_protocol::system_event::SystemEvent` | Redacted structured tracing projection | Process-local tracing subscriber |
| `arena0_api::EventFrame` and `EventData` | Local JSON event contract | Bounded feed on one Unix socket |

The guest `arena0_protocol::Event<M>` remains the Wasm input contract. It is not
a Host observation event.

Performance tracing is not a fifth event contract. The opt-in
`arena0::performance` target records structured operation timings at the code
that owns each operation. It does not create an enum, feed the API, or change
the semantic `arena0::system_event` target.

`SessionMessage` is not an observation feed. Dropping a signature request,
callout request, terminal result, or committed-step notice can leave the
daemon with the wrong state. The supervisor consumes these messages, validates
or persists their data, updates the owned lifecycle, and then emits a
`HostEvent`.

## Emit once and project locally

```rust
events.emit(HostEvent::SessionCompleted { source, outcome });
```

`Events::emit` always creates and sequences the local `EventFrame`. When the
occurrence has a safe system representation, it also redacts the occurrence
into a `SystemEvent` and calls the daemon-private tracing emitter.

Producers never select the projections themselves. Events such as
`host.stopped` remain in the local API contract until another real consumer
needs a safe system representation.

## Preserve these invariants

- An event reports a completed decision or mutation. It never drives the state
  machine.
- The owner emits after the relevant mutation commits.
- The reliable `SessionMessage` channel carries work and state handoff.
  Structured tracing and the API feed are observations, not state handoff.
- System events never contain opaque program params, outcomes, callout
  context, signatures, invite tokens, or private key material.
- `SystemEvent` and `EventFrame` are separate contracts with separate
  payload rules.
- A slow observer can lag without blocking the Host or daemon.

## Implement the event junction

1. Construct the narrowest valid `EventSource` for the occurrence.
2. Apply the protocol decision or durable mutation.
3. Construct one `HostEvent`.
4. Call `Events::emit` at the owner boundary.

The daemon-private emitter records the typed system value on the
`arena0::system_event` tracing target. It does not own a parallel event bus.

The Unix API exposes `events.subscribe` on each Host socket. The daemon
sequences `EventFrame` values with a self-contained emission-time `HostInfo`
snapshot, boot ID, sequence number, timestamp, correlation IDs, and typed
`EventData`. Consumers order frames by `(host.id, boot_id, seq)`. An
occurrence does not require a parallel system-event variant; projection
remains internal to `Events::emit`.

## Assert events in greybox tests

Runtime tests inject the negotiation event callback and collect typed values
locally. Daemon greybox tests subscribe to the Host API event stream. Test the
structured tracing projection only when its formatting or redaction is the
behavior under test.

Assert only the order guaranteed by the state machine or mutation boundary.
Two concurrent events may arrive in either order when no dependency defines
their order.

For a mutation test, assert all three results:

1. the fault hook fired at the intended boundary;
2. the expected event described the resulting mutation or rejection; and
3. the public result, trace, receipt, or reopened store proves the final state.

Treat API receiver lag as a test failure unless the test exercises lag
behavior. Keep payloads small and drain concurrently so the test does not cause
its own overrun.

When a change crosses event contracts, update the owning types and their
exhaustive matches together. Check the system-event tracing projection, daemon
API projection, API filtering and wire tests, client consumers, and greybox
assertions.
