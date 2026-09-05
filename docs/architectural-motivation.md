# Architectural motivation

## Why the release is local

arena0's differentiating work is deterministic guest execution, multiparty
agreement, portable evidence, program authoring, and an agent-safe interface.
The public release keeps those semantics independent from discovery,
addressing, relay, transfer, and service operations. Those delivery concerns do
not change what a valid execution means.

The release therefore treats same-machine delivery as a product topology, not
only a test double. It exposes the real N-party negotiation, activation,
execution, and receipt paths through a bounded virtual network. The topology
also keeps each Host's identity, durable evidence, and failure boundary
explicit.

## Host and Ensemble

The two primary runtime nouns are intentionally domain-level:

- `Host` owns one participant identity, its protocol machinery, and its SQLite
  store.
- `Ensemble` owns a bounded set of Hosts and their local topology; it does not
  own execution state or protocol facts.

Names that encode deployment accidents such as “participant host” or “local
host” obscure the ownership boundary. The Host/Ensemble split keeps identity,
execution, and connectivity ownership explicit.

The daemon mirrors that ownership. One process supervises the Ensemble, while
every Host receives its own persistent namespace and Unix socket. After
activation, one private `ExecutionActor` owns the admitted guest, execution
signer, and transport capabilities for that execution. API handlers and the
supervisor communicate with the actor or read durable projections; they do not
hold a second mutable execution object.

## Why keep the Transport seam

`Transport` isolates protocol machinery from delivery while preserving the
concerns runtime needs: bounded program-topic facts, convergence fetches, and
execution streams. `LocalTransport` implements that seam with a shared bounded
virtual network. It serializes through the real codec and retains bounded
queues, cancellation, topic membership, content addressing, and sender
metadata. It does not own admission or protocol truth.

Any transport implementation must preserve activation, execution, trace, and
receipt semantics. A conformance result that changes those outputs is a
protocol defect, not a transport-specific behavior.

## Why admission is exact

Automatic discovery is not necessary to validate the protocol model. Exact
admission makes caller intent and failure modes legible:

```rust
Explicit { peers }
Join { creator, negotiation_id }
```

The creator chooses every participant. A joiner names one exact creator offer.
Nothing implicitly expands the set or fetches a missing program. An explicit
request validates and stores its `params` before publishing the offer. A join
request may omit `params` to accept the authenticated creator offer, or provide
a local preference and sign only a matching offer. A mismatch produces a
signed counteroffer; the creator can re-offer only values supported by enough
participants. This keeps caller intent and failure modes legible without a
second admission protocol.

## Why activation durability remains per Host

One process could atomically prepare all participants and start them behind a
global barrier. That would make startup semantics depend on supervisor
coordination rather than Host protocol facts.

Instead, every Host independently compare-and-sets its activation record in
SQLite, signs only after prepare, and starts only after its own complete
commit. A prepared record is resumable but cannot execute. After the commit,
the Host admits the exact local Wasm artifact and starts one execution actor;
each guest call uses a fresh bounded Wasm instance. The process supervisor
coordinates lifecycle, not protocol authority.

## Why receipts include the producer

Every participant independently owns its evidence. `SessionHash` alone names
the shared activation but does not identify which Host produced a stored
receipt. Retrieval by session alone either discards evidence or silently
selects one producer.

The durable address is therefore `(SessionHash, producer PeerId)`. The store
keeps the receipt artifact, the fact that this Host imported it, and the local
execution relation as separate records. Listing derives `Produced`, `Imported`,
or `Both` from those facts, so publishing an exact imported artifact does not
erase its import history. Verification can state exactly which evidence it
consumed. Reimporting the same artifact is idempotent, while a different
artifact cannot replace an existing producer key.

## Why executables are split

Each executable has one owner and one dependency boundary:

```text
arena0     -> client + light verifier
arena0d    -> daemon + Ensemble MCP projection
cargo-arena0 -> sandbox tooling
```

The MCP projection therefore belongs to `arena0d`: one Streamable HTTP
endpoint exposes a stable catalog for every supervised Host, and each call
names its Host explicitly. The MCP projection needs no session state, and its
authentication token never selects a Host. Full replay stays daemon-side; the
CLI requests it through the Host API instead of embedding Wasmtime.

## Why program values stay guest-owned

The Host cannot reconstruct concrete program DTO semantics from a schema
without creating a second serializer. JSON Schema is an introspection and
validation contract, while Borsh is the deterministic program-byte contract.

Generated guest code is the only layer that knows both the concrete Rust type
and its stock Serde/Borsh implementations. Keeping conversion there prevents
schema-tool limitations from leaking into domain types and guarantees that
host refactors do not change program bytes.

## What would change this design

The recommendation should be revisited if:

- a Transport implementation cannot preserve runtime semantics;
- local participants need isolation from the machine operator rather than only
  independent protocol state;
- a conformance run yields different activation, trace, or receipt bytes across
  delivery implementations;
- one daemon cannot bound or shut down N Host services deterministically.

Until one of those is demonstrated, the coherent public release is the local
Host/Ensemble model with remote delivery outside the public tree.
