# arena0 local flow walkthrough

This walkthrough follows a two-Host rock-paper-scissors session from admission
to canonical receipts and unilateral stop reports. The formal rules live in
[`protocol-architecture.md`](protocol-architecture.md). The same flow works
for any N-party Ensemble.

## The local topology

```text
arena0 serve -> arena0d
└── Ensemble
    ├── Host Alice ─ state/Alice/arena0.sqlite ─ arena0-Alice.sock
    ├── Host Bob   ─ state/Bob/arena0.sqlite   ─ arena0-Bob.sock
    └── LocalTransport virtual network
```

One `arena0d` process supervises N Hosts and their N Unix sockets. Each Host
owns a persistent `PeerId`, identity keys, a program catalog, one SQLite store,
and its own Unix socket. One private execution actor owns the live guest and
transport capabilities for each `ExecId`. The Ensemble owns topology and
coordinated shutdown. `LocalTransport` delivers protocol frames between the
Hosts and does not choose participants or interpret program values.

The default `arena0 serve` command starts two Hosts. Pass `--hosts` for a
larger Ensemble. The launcher replaces itself with the installed `arena0d`
process, which remains the runtime owner. Give every Host its own state
directory and socket when a test or deployment needs explicit paths.

## 1. Import the program on every Host

Every selected Host must already have the exact `ProgramHash` in its local
catalog. Admission does not copy a program between Hosts. The daemon validates
the module and its required guest exports before it stores the content-addressed
Wasm artifact. Each execution admits that artifact again.

Agent-facing program values cross a Host socket as JSON. The Host validates
params against the program's JSON Schema and forwards the JSON to generated
guest code. The guest converts its concrete DTO with its stock Serde and Borsh
implementations. The Host carries the resulting Borsh bytes as opaque protocol
data.

The coordinated `arena0 run <wasm-path>` convenience explicitly imports the
same local bytes into every selected local Host before this admission step. It
does not add remote program transfer or implicit acquisition to admission.

## 2. Alice creates an explicit admission

Alice's client sends `exec.new` to Alice's Unix socket:

```rust
Request::ExecNew {
    exec_id: alice_exec_id,
    program: "rock-paper-scissors".into(),
    params: None,
    ensemble: EnsembleSpec::Explicit {
        peers: vec![bob_peer_id],
    },
}
```

`Explicit { peers }` creates a negotiation. The listed peers plus the creator
form the participant set. The daemon requires unique peers that exclude the
creator, canonicalizes the set, checks the 2-to-64 participant bound and that
the program supports the resulting count, and does not add any other Host.
Fixed-size programs accept one exact count; variable-size programs advertise an
inclusive range of supported counts.

Alice's client generates a unique `ExecId` before sending the request. The Host
uses that exact identifier, so the client can withdraw the attempted creation
by sending `exec.cancel_creation` with that ID if the request is interrupted or
its response is lost. That cleanup follows an activation race if necessary and
acknowledges only after the attempted execution is stopped. The Host allocates a
creator-chosen `NegotiationId`, computes the program's initial state in the
bounded sandbox, builds the offer, and issues Alice's signed ticket. The offer
fixes the program, opaque params bytes, target size, initial state, and deadline.
No session exists yet, so the response can identify the negotiation but cannot
contain a `SessionHash`.

## 3. Bob joins the exact negotiation

Bob sends his own `exec.new` request to Bob's Unix socket:

```rust
Request::ExecNew {
    exec_id: bob_exec_id,
    program: "rock-paper-scissors".into(),
    params: None,
    ensemble: EnsembleSpec::Join {
        creator: alice_peer_id,
        negotiation_id: negotiation_id,
    },
}
```

`Join { creator, negotiation_id }` accepts only the offer from that creator
with that exact negotiation ID. It does not discover peers or create an
implicit participant. Bob may omit `params` and accept the creator's
authenticated parameters. Bob may also provide local preferred `params`; he
then signs only an offer with matching parameters and publishes a counteroffer
when the current offer differs. Bob checks the offer fields, recomputes the
initial state, and signs a ticket for the exact offer.

Alice and Bob exchange the bounded negotiation facts through
`LocalTransport`. The creator remains the participant-set authority. Each
Host verifies ticket signatures, revisions, lifetimes, and the scope-bound
execution key binding before it can select a ticket.

## 4. Hosts prepare and commit activation

When the exact ticket set reaches the target size, every selected Host performs
the same durable sequence:

1. Compute and validate the candidate activation.
2. Compare-and-set a prepared activation record for its `ExecId`.
3. Create its activation signature only after prepare succeeds.
4. Validate the complete N-of-N activation aggregate.
5. Commit the complete activation.

The activation record permanently binds the local `ExecId` to one
`SessionHash`. A conflicting activation cannot replace it. A prepared record
can resume after restart, but it cannot run or produce a receipt until the
complete activation commits.

After its local commit, each Host creates the execution and injects
`Event::SessionStarted` at public position zero. Hosts may start at different
wall-clock times. There is no process-wide start barrier.

## 5. Hosts execute the program

Each Host's execution actor loads the durable state and runs one fresh bounded
Wasm instance for each guest call. Shared-state steps exchange protocol-domain
`ExecFrame` messages; transport converts them to the bounded, versioned raw
frames in `arena0-wire` and agrees on one `StepCommitment`. A step completes
only after the selected Hosts sign the same commitment.

The trace records the pre-state and post-state hashes, the deterministic event
and effect edge, one aggregate agreement with its signer bitmap, and
`fuel_used`. When the guest requests `ctx.random`, the Host records
replayable evidence. A state disagreement, invalid frame, sandbox limit, or
guest failure aborts at the failing edge.

Callout answers, queries, and terminal projections remain JSON at the Host
boundary. `Effect::SessionEnd` and `Effect::SessionAbort` become
`ExecFrame::End` and `ExecFrame::Abort` on the execution wire.

The actor accepts inbound frames into the SQLite inbox before it acknowledges
transport responsibility. It applies or consumes each frame through the
reducer. Reducer state, timers, and outbox effects commit atomically. Outbox
leases retry after delivery failure and recover after a restart. A pending
callout retains its context and `pending_id` in the store; recovery re-emits an
acknowledged callout. Signing requests stay inside the actor.

## 6. Each Host retains evidence

Alice and Bob independently assemble the same receipt after unanimous completion.
Its versioned canonical bytes and `ReceiptId` are identical. A shared program
abort also produces a canonical receipt. A unilateral failure instead produces
an authenticated `StopReport`; different observations can yield different
reports for the same session.

The receipt API exchanges a typed envelope:

```rust
enum ReceiptArtifact {
    Receipt(Receipt),
    StopReport(StopReport),
}
```

Each artifact contains activation, agreed params, the certified public trace,
and terminal evidence. Completed receipts also contain the opaque Borsh outcome.
Light verification checks signatures and commitments; full verification replays
the exact Wasm and projects the completed outcome to JSON. A stopped result
preserves its exact cause and has no outcome.

One store transaction persists the artifact, terminal status, production relation,
and publication outbox effect. There is no exporter seal. Local production and
import facts yield `Produced`, `Imported`, or `Both` provenance. Exact content
IDs retrieve any held artifact; a session reference resolves only the addressed
Host's local production relation.

## Observe the local run

For the coordinated operator path, `arena0 run rock-paper-scissors` displays
the guest-owned current `View`, execution lifecycle, and all-Host receipt
progress. Use `--no-tui` for inline output or `--json` with a driver bound to
every Host for one machine-readable result.

Use `events.subscribe` on the Host's Unix socket for sequenced `EventFrame`
values. For local operator diagnostics, enable structured records from the
`arena0::system_event` tracing target. Neither representation carries program
params, outcomes, callout context, or signatures.

## The one-line summary

One `arena0d` process supervises independent Hosts. `Explicit { peers }`
selects the participant set, `Join { creator, negotiation_id }` names one
exact negotiation, `LocalTransport` delivers the bounded protocol facts, and
each Host commits, executes, and retains its own copy of the canonical receipt or unilateral stop report.
