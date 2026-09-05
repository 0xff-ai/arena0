# arena0 Phase 1 brief

## Product

arena0 is a protocol and local runtime for verifiable program co-execution
between agents. Every participant Host loads the same content-addressed Wasm,
runs it under deterministic limits, and co-signs the public state commitment
after each shared step.

The public release is a local sandbox and runtime product. One daemon process
supervises multiple independent Hosts on an in-process virtual network. Each
Host owns its identity, program catalog, SQLite store, execution actors, and
Unix socket.

## User promise

- A program runs identically across N local participant Hosts.
- Divergence aborts at the exact public edge.
- Every Host persists its own signed receipt.
- A third party can light-verify a receipt without the program or fully replay
  it with the exact Wasm. Both verification tiers return a typed terminal
  result for completion and stop cases.
- Agents interact with JSON and JSON Schema, never raw program bytes.

## Trust boundary

All Hosts run on one machine under one process supervisor. They retain separate
identities, stores, sandboxes, and evidence, but they do not provide independent
custody against compromise of that machine. Phase 1 proves deterministic
agreement and proof construction, not remote-party independence.

## Shape

```text
arena0d
└── Ensemble
    ├── Host A -> home A + socket A + receipt A
    ├── Host B -> home B + socket B + receipt B
    └── Host N -> home N + socket N + receipt N
         └── LocalTransport

arena0     -> arena0-home + arena0-client + light verifier
arena0d    -> arena0-home + arena0-daemon + Ensemble MCP
```

`Host` owns one participant. `Ensemble` owns the bounded local topology. The
daemon owns persistent namespaces and Unix services. One private execution
actor owns the live admitted guest, execution signer, and transport handles for
each `ExecId`; the exclusive `ExecutionStore` remains the sole durable writer.
State-changing store methods require `&mut self`. The local
transport owns delivery only.

## Admission

There are exactly two forms:

- `Explicit { peers }`: create an offer for the exact other participants.
- `Join { creator, negotiation_id }`: join one exact creator offer.

Every selected Host must already have the exact program in its local catalog.
No public operation discovers peers or transfers Wasm. A join request may omit
its local params and accept the creator's authenticated offer, or supply params
as a preference. A Host signs a ticket only after the offer's params and
recomputed initial state satisfy that preference.

## Evidence invariants

- Canonical offer, ticket, activation, session, trace, and receipt formats stay
  unchanged.
- Each Host prepares durable activation state before signing.
- Each Host commits the complete activation before `SessionStarted`.
- Every shared step records one BLS aggregate agreement and signer bitmap.
- Fuel usage and replayable randomness are recorded.
- Receipts are addressed by `(SessionHash, producer PeerId)`.
- Authenticated inbound frames enter a durable inbox before transport
  acknowledgement.
- Reducer state, trace commits, timers, and outbox effects commit in one SQLite
  transaction.
- Leased outbox effects retry after transport failure and recover after a
  process restart.
- Terminal proof collection and the producer seal remain internal. Public
  receipts are published only after the seal is durable.
- Receipt artifacts, import facts, and production relations are separate; the
  public provenance projection retains `Produced`, `Imported`, or `Both`.

## Program boundary

The guest owns concrete DTOs. Params, callout answers, queries, and terminal
projections cross as JSON. Generated guest code alone performs stock
Serde/Borsh conversion. The Host may validate JSON Schema but cannot use schema
interpretation to define serialization.

## Public packages

- Core: `arena0-crypto`, `arena0-wire`, `arena0-program`, `arena0-protocol`,
  `arena0-store`, `arena0-transport`, `arena0-sandbox`, and `arena0-verify`.
- Guest authoring: `arena0-sdk-macros`, `arena0-sdk`, `arena0-primitives`,
  and `programs/`.
- Host runtime: `arena0-node`.
- Application: `arena0-home`, `arena0-api`, `arena0-client`, `arena0-daemon`,
  `arena0-cli`, `arena0d`, and `cargo-arena0`.

The release executables are `arena0`, `arena0d`, and `cargo-arena0`. `arena0d`
exposes one Host-explicit MCP endpoint for the Ensemble. `arena0 serve` is a
thin launcher for that installed daemon executable. Bare `arena0` on a human
terminal opens the local program and Host workspace; nonterminal use prints
help. The workspace and coordinated `run` command can start an owned `arena0d`
child for the exact local Host set. A focused run TUI projects the current guest
`View`, lifecycle, durable trace, system events, input, and receipt progress
without owning runtime state.

## Deferred from Phase 1

- remote peer discovery and addressing;
- remote program transfer;
- remote transport adapters;
- public event aggregation and browser view;

These features are outside the public workspace. They do not change the guest
ABI, protocol evidence, runtime lifecycle, agent JSON boundary, or receipt
identity.
