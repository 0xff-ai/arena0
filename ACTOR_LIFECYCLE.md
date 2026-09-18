# Execution actor and Wasm lifecycle

Status: design decision; not yet implemented on `main`.

## Ownership

Each participant execution has one `ExecutionActor`. The actor owns one live
Wasm instance for the program and is the only component allowed to call that
instance. The instance remains allocated while the actor is alive, including
while the actor waits for messages, timers, callout answers, signatures, or
agreement from other participants.

A program call does not create a Wasm instance. It enters an exported function
on the instance already owned by the actor. Calls are serialized by the actor,
so the instance is never entered concurrently.

```text
ExecutionActor
├── live Wasm instance
├── durable execution store handle
├── transport handles
└── execution signing capability
```

The compiled `LoadedProgram` remains a process-wide immutable artifact that can
be shared by many executions. The live instance is execution-specific and must
not be shared between actors.

## Resident program state

Shared state, local state, the guest allocator, and other guest memory remain in
the live Wasm instance between calls. Routine call inputs contain only the data
for that call, such as an event, query, or viewport. They do not contain copies
of shared or local state.

```text
event arrives
    -> actor calls the existing instance
    -> program reads and changes resident state
    -> Host validates and persists the result
    -> actor keeps the same instance for the next call
```

The Host may obtain the canonical shared or local state representation needed
for hashing, agreement, evidence, and durable records. It does not send that
state back into the guest before the next routine call.

The same rule applies to `writer`, `query`, `view`, and `outcome`. They read the
resident shared state. Because the sandbox accepts hand-written Wasm as well as
SDK-generated programs, the Host must prevent a nominally read-only call from
changing the live instance. The first implementation can checkpoint memory
before such a call and restore it afterward. If the call grows memory, traps,
or fails its quiescence checks, the Host discards that instance and reconstructs
it from the pre-call checkpoint.

## Durable checkpoint

The Host checkpoints the guest instance at a quiescent boundary: an exported
function has returned, no guest stack is active, and the Host has validated the
call result. Each checkpoint records the durable execution version it represents.

The execution change is committed before a replacement checkpoint is saved. A
crash in between leaves an older valid checkpoint and a durable sequence of
later public and private guest commits. Recovery restores that checkpoint and
replays those commits in execution-version order. The checkpoint is therefore
a replaceable recovery cache, not a second source of protocol truth and not
part of receipts or traces. It must never be saved for a version that the
execution store has not committed.

The checkpoint contains every mutable part of the Wasm instance that may affect
later program behavior. Linear-memory bytes are necessary but may not be
sufficient. The execution profile must either include mutable globals and table
state in the checkpoint or prohibit mutable instance state that the checkpoint
cannot restore. Per-call Host accounting such as fuel, collected effects, logs,
and temporary capability usage is reset by the Host and is not program state.

The checkpoint is stored on the existing execution record as bounded opaque
bytes plus the execution version it represents. It is private Host storage. It
does not enter `arena0-protocol`, the public trace, a receipt, shared state, or a
guest call envelope.

Checkpoint size, memory growth, and storage use remain bounded by the execution
profile. A later optimization may store changed pages rather than a complete
memory image, but it must preserve the same recovery result.

## Recovery

The Host restores guest state only when it constructs an actor from durable
storage. Recovery creates a new Wasm instance from the exact program artifact
and execution profile, restores the newest valid checkpoint through the
sandbox, recreates Host-owned resources, and replays durable public and private
guest commits recorded after the checkpoint. The actor starts only after those
steps succeed.

```text
load durable execution
    -> load exact compiled program
    -> instantiate once
    -> restore newest valid Wasm checkpoint
    -> recreate Host resources
    -> replay public/private guest commits after checkpoint version
    -> compare reconstructed state with durable execution state
    -> start actor
```

Restoration is a sandbox operation. It is not a program event, a guest-visible
`resume` call, or a state-bearing input to the next handler. If the actor and
its instance are already alive, there is nothing to restore. During replay, the
Host checks the guest results against the stored results and does not repeat
external effects.

An idle policy may evict a live instance. Before a clean eviction, the Host
should save a checkpoint for the latest durable execution version. If that save
fails, it may discard the instance and later recover from an older checkpoint
plus the durable guest-commit tail. The next event reconstructs the actor state
using the same recovery path as process restart. The timeout is global runtime
configuration rather than program state.

## Failure boundaries

The actor keeps the live instance only when the Host can relate its memory to a
known durable execution version. If a call traps, validation fails, or storage
returns an outcome that leaves the Host uncertain which guest changes are
durable, the actor must not continue from the changed memory. It discards the
instance and recovers from a checkpoint plus the durable guest-commit tail
before processing more work.

If a guest call changes memory but its execution change is not committed, that
memory is discarded. If the execution change commits but the next checkpoint
does not, recovery uses the preceding checkpoint and replays the committed
input. The actor owns this sequence and never accepts another command while the
result of the current command is unresolved.

## Pending operations and agreement

Callouts, signing requests, timers, and other waits do not require a live Rust
or Wasm stack. The guest reaches a quiescent exported-function boundary, and the
existing durable pending record identifies how execution continues. The Wasm
instance stays allocated while the actor waits.

A proposed shared transition is distinct from committed shared state. The Host
runs a proposed shared event against a memory checkpoint, validates and records
the proposed result, and restores the checkpoint while agreement is pending.
Queries therefore continue to observe the last committed shared state.

When the final step signature commits the proposal, the actor runs the recorded
shared event again against that same committed state, checks the state, effects,
and fuel against the recorded proposal, and retains the resulting memory. This
needs no second live instance, no state-bearing call input, and no guest-visible
`commit` or `resume` export. A crash before the retained memory is checkpointed
is recovered from the durable `public_commits` row.

## Current implementation mismatch

At `282cb63bfe30cf71aa6ee106449109cd00830dce`, `LoadedProgram::invoke`
creates a fresh Wasmtime `Store` and `Instance` for every semantic call. The
generated guest ABI reconstructs shared and local Rust values from Borsh bytes
on each call, then returns encoded state to the Host. The execution actor owns a
compiled `LoadedProgram`, but it does not own a live Wasm instance.

That behavior conflicts with this design. The implementation must move instance
ownership into `ExecutionActor`, remove shared and local state from routine call
inputs, add sandbox-owned checkpoint and restoration, and update replay so it
uses the same instance-lifetime semantics as live execution.

## Constraints found in the current build

The current Rust guest artifacts use one linear memory. They also contain one
mutable Wasm global, Rust's stack pointer, and a fixed function table. The
execution profile already limits a guest to one memory. The simplest complete
checkpoint can therefore contain linear memory if the build exports the stack
pointer, every exported call must return it to its initial value, table-changing
instructions are rejected, additional mutable globals are rejected, and
passive data or element segments cannot retain hidden mutable state. These rules
make a returned export a checkable quiescent boundary.

The SDK reserves a 16 MiB linear-memory arena for call inputs and outputs. That
space is temporary and should be zero at a quiescent boundary. The current Host
frees the input before the output, although the guest allocates the output after
the input. A persistent instance requires last-in-first-out release: read and
free the output first, then free the input. Freed call buffers should be zeroed
so they do not enlarge checkpoints or preserve obsolete input bytes.

A checkpoint need not store an 18 MiB dense image. The first implementation can
record the memory page count and only nonzero 64 KiB pages. Restoring still
creates the instance normally, grows and clears its memory, and then writes the
saved pages directly through Wasmtime. Dirty-page tracking and checkpoint
deltas should wait for measurements.

## Consequences

- One participant execution has one actor and one live Wasm instance.
- Shared and local state stay resident between routine calls.
- Routine calls carry events and call-specific arguments, not state snapshots.
- The Host checkpoints instance state for durability; it does not rehydrate a
  live instance before each call.
- Read-only projections run against resident state and cannot leave memory
  changes behind.
- Restart and idle eviction create a new instance and restore its checkpoint
  and any durable input tail before the actor accepts work.
- Full replay must reproduce the same ordered calls in one instance.
- Multiple Wasm memories are not required by this lifecycle design.
