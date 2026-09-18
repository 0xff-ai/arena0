# Remove the dual session execution path

Status: Implemented.

Scope: activated session execution at revision `282cb63`. Admission,
negotiation, and activation are unchanged.

## Decision

Replace the shared and local guest paths with one event dispatch against one
Wasm instance owned by the `ExecutionActor`.

Shared state and local state remain different because they have different
visibility and agreement rules. They are not different kinds of transition.
Every session `Event` receives one `Context`, may read and mutate both states,
and may emit any existing `Effect`. The Host hashes only the shared Wasm
memory. Agreement is applied after dispatch when the shared result belongs to
a session step; it is not encoded in separate guest entry points, context
types, event types, effect types, deltas, or commit paths.

The design reuses the existing session terms and types: `Event`, `Effect`,
`ExecutionActor`, `ExecutionState`, `SharedProposal`, `TraceEntry`,
`StepCommitment`, and the inbox, outbox, and timer tables. It does not add a
second protocol vocabulary for the transient result of a dispatch.

## Scope boundary

This proposal begins after a complete `Activation` has been committed and the
Host is ready to inject `Event::SessionStarted`.

The current `Offer`, `Ticket`, `Activation`, and `NegotiationDriver` behavior
stays in place. Initial-state validation during negotiation also stays in
place. This proposal adds no guest negotiation events or effects and does not
move negotiation into the program. A later proposal may reconsider that
boundary without blocking removal of the dual session path.

The participant set, `writer` projection, N-of-N step agreement, durable
inbox, durable outbox, timer table, and terminal proof remain. Remote transport
and participant discovery are also outside this change.

## Current path to remove

The implementation starts with a flat guest `Event` and `Effect`, then splits
both before invoking Wasm:

```text
CURRENT

Event
  ├── PublicEvent  -> SharedCall -> arena0_shared -> SharedDelta
  │                                             -> ProposeShared
  └── PrivateEvent -> LocalCall  -> arena0_local  -> PrivateDelta
                                                -> Private

ExecutionInput -> pure reducer -> CommitPlan
                              -> TimerMutation
                              -> DurableEffect[]
```

The split appears in each layer:

| Layer | Current split |
|---|---|
| SDK | `SharedContext` versus `Context`; shared callbacks cannot access local state, identity, randomness, or effects |
| Protocol | `PublicEvent` versus `PrivateEvent`; `PublicEffect` versus `PrivateEffect`; `SharedDelta` versus `PrivateDelta` |
| ABI | `arena0_shared` versus `arena0_local`; `SharedInput`/`SharedOutput` versus `LocalInput`/`LocalOutput` |
| Sandbox | `SharedCall`/`SharedCallResult` and `LocalCall`/`LocalCallResult`; `apply_shared` and `apply_local` |
| Actor | session start and messages use the shared path; input, timers, signatures, and reactions use the local path |
| Store | public and private commits, cursors, records, and reducer branches |

This split forces a program such as chess to validate a callout response in
`on_input`, emit `Effect::Broadcast`, wait for the Host to deliver that
broadcast back to itself, and mutate shared state only in `on_message`. The
causal operation is spread across two Wasm calls, an outbox row, a self-send,
and two protocol record types.

That detour is an architectural restriction, not a property required by
shared-state agreement. A callout response can update shared state, update
local state, emit a message, arm a timer, and request another callout in one
program dispatch. Other participants can apply the message and check that
their shared memory reaches the advertised hash.

## One session event path

```text
PROPOSED

Event
  -> ExecutionActor
  -> arena0_dispatch
       Context { shared, local, participant, session, entropy, effects }
  -> shared memory + local memory + Effect[]
  -> validate shared hash and agreement requirements
  -> atomic store boundary
  -> durable delivery
```

`ExecutionActor` remains the serial owner of one participant's execution. All
session event sources enqueue the existing `Event` type:

- committed activation enqueues `SessionStarted`;
- an accepted broadcast frame enqueues `MessageReceived`;
- a callout answer enqueues `InputReceived`;
- a timer enqueues `TimerFired` or `TypedTimerFired`;
- a completed signing request enqueues `Signed`;
- normal program progress enqueues `React`.

No source chooses a shared or local call. The actor gives every event to the
same Wasm export with the same context and the same limits. A rejected event or
guest failure restores both memories and emits no effect.

`React` remains an ordinary event. It is useful when a committed step makes a
participant eligible to act without an external input. The change is that a
reaction may now mutate shared state directly and emit a broadcast in the same
dispatch.

### SDK surface

The program keeps its existing `Shared`, `Local`, `Phase`, `Message`, `Input`,
`Callout`, `Params`, and `Outcome` associated types. Transition callbacks all
receive one context and can return the existing `Transition` values:

```rust
// CURRENT
fn on_message(
    ctx: &mut SharedContext<Self::Shared>,
    from: Participant,
    message: Self::Message,
) -> MessageApply<Self>;

fn on_input(
    ctx: &mut Context<Self::Shared, Self::Local>,
    input: Self::Input,
) -> Result<(), InputFault>;

// PROPOSED
fn on_message(
    ctx: &mut Context<Self::Shared, Self::Local>,
    from: Participant,
    message: Self::Message,
) -> MessageApply<Self>;

fn on_input(
    ctx: &mut Context<Self::Shared, Self::Local>,
    input: Self::Input,
) -> Result<ProgramTransition<Self>, InputFault>;
```

The same change applies to `on_session_started`, `on_react`, `on_timer`, the
typed timer route, and the signing continuation. `Context` exposes mutable
access to shared and local state together with the existing participant,
session, entropy, and effect APIs. Its state access is explicit:

```rust
fn shared(&self) -> &Shared;
fn shared_mut(&mut self) -> &mut Shared;
fn local(&self) -> &Local;
fn local_mut(&mut self) -> &mut Local;
fn state_mut(&mut self) -> (&mut Shared, &mut Local);
```

Every successful event callback can return `Transition::Stay`,
`Transition::To`, `Transition::End`, or `Transition::Abort`. Typed input faults
remain available for retry behavior, and `MessageApply` retains message
accept/reject behavior. This makes lifecycle effects available from every
event without adding lifecycle methods to the effect handle. `SharedContext`
is removed.

The macro still provides typed callback routing. It decodes the flat `Event`,
decodes the program-owned message or input bytes for the selected callback,
and encodes the resulting states and effects. Program authors do not need to
manually decode an enum of opaque byte payloads. The generated router has one
mutating ABI export: `arena0_dispatch`.

`initialize`, `writer`, `query`, `view`, and `outcome` remain projections or
pre-session operations rather than session event paths. Initialization receives
`&mut Shared` directly. Read-only projections receive `&Shared` and their
existing explicit arguments; they run against memory snapshots and cannot emit
effects. This removes the remaining `SharedContext` consumers without using
those operations to recreate a second mutating path.

### Program structure

A participant that receives private input may apply the corresponding program
operation immediately and broadcast the program message in the same handler.
The receiver applies that message in `on_message`. Both callbacks can call the
same program helper:

```rust
fn on_input(ctx: &mut Context, input: Input) -> Result<Transition<Phase>, InputFault> {
    let message = Message::from(input);
    apply_message(ctx, ctx.me(), &message)?;
    ctx.effects().broadcast(&message);
    Ok(Transition::Stay)
}

fn on_message(
    ctx: &mut Context,
    from: Participant,
    message: Message,
) -> MessageApply {
    apply_message(ctx, from, &message)?;
    Ok(ApplyDecision::Accept(Transition::Stay))
}
```

The originating participant does not receive its own broadcast as a second
program event. Its state change already happened in the event that emitted the
broadcast.

## Wasm instance and memory

Each active `ExecutionActor` owns one Wasm instance for the lifetime of the
execution. The instance has three bounded linear memories:

| Memory | Contents | Persisted | Included in `StateHash` |
|---|---|---:|---:|
| `memory` | Rust stack, heap, decoded event values, temporary state values, and encoded effects | No | No |
| `arena0_shared` | Canonical Borsh bytes for `Program::Shared` | Yes | Yes |
| `arena0_local` | Canonical Borsh bytes for `Program::Local` | Yes, by this Host only | No |

The existing `arena0_shared` and `arena0_local` export names become memory
exports instead of function exports. `arena0_dispatch` becomes the only
mutating function export. `arena0_alloc`, `arena0_dealloc`, and the read-only
projection exports remain.

This is an ABI break. The execution profile enables Wasm multi-memory and
raises the memory limit from one to three. `REQUIRED_FUNC_EXPORTS`, sandbox
validation, independent Wasm fixtures, program metadata, and the ABI version
must change together.

### Canonical memory representation

Both state memories use the same bounded representation:

```text
u32 little-endian encoded length
Borsh state bytes
zero-filled remainder of the allocated memory
```

The Host hashes the entire current `arena0_shared` memory, including its length
prefix and zero-filled remainder. Memory length is therefore part of the hash
input, stale bytes cannot survive a shorter value, and the Host does not need a
program-supplied range to decide what shared state means. The local memory is
never read while computing `StateHash`, a `StepCommitment`, a broadcast frame,
or a receipt.

The build step gives both state memories the same fixed canonical capacity.
The Rust work memory keeps the compiler-produced minimum and a bounded maximum;
ordinary Rust allocation needs `memory.grow` while the instance is prepared.
Before the Host captures a resident baseline, it calls the generated
`arena0_prepare` export. That export allocates and releases a fixed 56 MiB
reserve through Rust's normal allocator. The Host then lowers the instance's
memory limit to the observed work-memory size, so later `memory.grow` calls
cannot change the resident memory shape. Fresh instances used for
initialization and projections run the same bounded preparation before their
operation and are discarded afterward. The Host validates all three memory
exports, their capacities, and their distinct identities.

After each mutating guest call, the Host validates both state-memory frames:
the length must fit, the encoded range must be in bounds, and every byte after
that range must be zero. The Host continues to treat the encoded Borsh value as
opaque program-owned bytes.

### Generated adapter

Rust currently places ordinary values in its default linear memory. The SDK
therefore treats the two state memories as canonical storage and the default
memory as temporary working space:

1. Before a callback, generated code copies the encoded shared and local values
   into bounded buffers in `memory` and deserializes them.
2. It creates one `Context` over both typed values and dispatches the event.
3. On acceptance, it serializes both values, clears the two state memories,
   and writes their new canonical bytes.
4. It returns only the accepted/rejected status through the bounded result
   convention. Effects continue to enter the Host-owned per-dispatch queue
   through the existing bounded imports, so there is only one effect owner.

The program build step adds and exports `arena0_shared` and `arena0_local`
after Rust compilation. Small generated host imports copy bytes between those
memories and the default memory. Every copy has checked ranges, is charged
against the existing host-byte and host-call limits, and completes before any
guest reentry; the Host retains no borrowed memory slice across a Wasm call.
Finalization occurs before `ProgramHash` is computed, so the hash identifies
the executable module exactly as the sandbox runs it.

The default memory, globals, tables, segments, and imported resources cannot
become hidden session state. The execution profile requires:

- fixed-size shared and local memories;
- a bounded work memory frozen at its observed post-`arena0_prepare` size for
  resident dispatch;
- fixed-size tables with no guest table writes;
- no passive data or element segments and no segment-drop instructions;
- no GC or externally supplied reference state;
- only the existing bounded deterministic imports and the new bounded
  state-memory copy imports;
- export of every mutable global so the sandbox can restore it;
- one start-function execution before the baseline is captured.

The sandbox captures the post-instantiation work memory and mutable globals and
restores them before every dispatch. Fuel, effect buffers, logs, entropy state,
and host-call counters also reset for every dispatch. Only `arena0_shared` and
`arena0_local` survive from one event to the next.

On recovery, the actor instantiates the exact program, restores the persisted
shared and local memories, restores any existing `SharedProposal`, and resumes
the durable inbox and outbox. Recovery restores those durable images directly;
it does not rerun earlier events to reconstruct live state.

The sandbox initialization result stores the bounded shared-state payload.
Negotiation reconstructs the fixed canonical `arena0_shared` image—length
prefix, payload, and zero tail—before computing `Offer.initial_state`, and
session execution reconstructs the same image before `SessionStarted`.
`Offer`, `Ticket`, `Activation`, and `NegotiationDriver` remain unchanged; the
ABI adapter changes the bytes supplied at their existing initial-state
boundary.

## Shared-state agreement

One dispatch path does not mean that local state becomes shared. The agreement
rule is applied to the result of a dispatch:

- local memory and participant-specific effects are never signed by other
  participants;
- every broadcast frame carries its message, position, pre-state hash, and
  post-state hash;
- a receiver dispatches `Event::MessageReceived` and signs only if its shared
  memory reaches the advertised post-state hash and its complete
  `StepCommitment` matches;
- N-of-N signatures over that `StepCommitment` commit the shared step;
- each participant commits the local memory and effects produced by its own
  dispatch when that shared step commits.

The producer and receivers need not execute the same `Event`. For example, the
producer may execute `InputReceived`, while receivers execute
`MessageReceived`. They also need not produce identical local memory, effects,
entropy records, or fuel use. Agreement covers the program message and the
shared pre-state and post-state hashes.

`StepCommitment` therefore stops binding an identical guest event, effect list,
and fuel count across participants. It retains the session, position, chain
link, message identity, message bytes, pre-state hash, post-state hash, and
terminal kind and outcome needed for N-of-N agreement. Matching shared hashes
alone are not sufficient when participants disagree about termination.
`TraceEntry` records the agreed step and its aggregate signature.
Participant-specific events and effects stay in the participant's store and do
not enter the portable receipt.

The existing `SharedProposal` remains the durable state awaiting step
signatures. It is extended to hold the post-dispatch shared memory, this
participant's post-dispatch local memory, and this participant's effects. When
the N-of-N aggregate arrives, one transaction promotes both memories and
releases the effects that were not needed to collect signatures. If agreement
does not complete, the remaining effects are not delivered; the proposal is
retained or the execution enters the existing safe stop flow.

The proposal is prepared before this Host publishes its signature. Once the
store contains a `SharedProposal`, the actor restores the resident Wasm
memories to their last committed images, projections continue to read those
committed images, and later session events remain queued. Signature, aggregate,
and stop frames may still progress. When the proposal commits, the actor loads
its stored post-dispatch memories. A timeout does not authorize a different
proposal at the same position. Before this Host signs, an authenticated stop
may clear the proposal and cancels only the outbox frames derived from that
proposal. Cancellation is a distinct durable disposition, not a delivery
acknowledgement. After this Host signs, the proposal is irrevocable: a peer may
already be able to assemble N-of-N agreement, so the execution retains the
proposal and rejects a competing stop at that cursor.

### Agreement postconditions

These checks defend the shared-state invariant without restricting which
context a handler receives or which effect variants it may emit:

1. One dispatch may emit at most one `Broadcast`. This is an agreement
   cardinality bound, not an event/effect classification.
2. `SessionStarted` always uses the existing position-zero agreement and must
   produce the same complete `StepCommitment` on every participant.
3. `MessageReceived` must start at the frame's pre-state hash, end at its
   advertised post-state hash, and reproduce the frame's complete
   `StepCommitment` before this Host signs.
4. If any other event emits `Broadcast`, that dispatch starts a
   `SharedProposal` even when its shared hash is unchanged. The producer stores
   its current shared memory, local memory, and effects with that proposal and
   does not process its own broadcast as `MessageReceived`.
5. If an event that is already applying `SessionStarted` or `MessageReceived`
   emits `Broadcast`, the effect remains staged until the current step commits.
   It then starts a subsequent proposal whose pre-state and post-state are the
   now-committed shared hash. The producer signs that proposal without another
   guest dispatch. Receivers must also leave shared memory unchanged for that
   message. A program that needs another change to shared state enqueues `React`
   after the current step and performs the transition through
   `arena0_dispatch`. A terminal step has no subsequent position, so a
   lifecycle effect and such a deferred broadcast in the same dispatch are
   rejected.
6. A dispatch that changes shared memory or emits `SessionEnd`, `SessionAbort`,
   or `Fail` must already be applying `SessionStarted` or `MessageReceived`, or
   must emit one `Broadcast`. The broadcast gives every other participant the
   `MessageReceived` event from which it creates the matching proposal. Any
   result without that agreement path is rejected before persistence.
7. A program lifecycle effect enters the current `StepCommitment` and terminal
   evidence even when the shared hash is unchanged. A Host-local runtime
   failure uses the existing authenticated unilateral-stop flow rather than
   inventing a guest effect. A lifecycle effect never commits merely because
   the shared hash is unchanged.
8. A dispatch with no shared change, broadcast, or lifecycle effect may commit
   its local memory and other effects immediately.
9. Effects emitted while a step awaits signatures remain with the
   `SharedProposal`. The broadcast needed to obtain signatures for that same
   proposal is the only program effect delivered before commitment. A
   broadcast staged by an already-applied step is delivered only after that
   step commits.

Every event may still mutate either state and emit any existing `Effect`.
These rules reject results that cannot be assigned an unambiguous agreement
boundary. They do not restore event-specific context types or separate Wasm
entry points.

## Persistence without the reducer pipeline

The actor already serializes event handling and owns the live execution
capability. It can validate and persist the result directly instead of
constructing `ExecutionInput`, calling a pure reducer, and interpreting a
`CommitPlan`.

For an event that does not require signatures, one SQLite transaction:

1. compares the expected execution version;
2. writes the shared and local memory images;
3. writes the accepted event and emitted `Effect` values;
4. arms or consumes timer rows;
5. writes outbox rows for effects that require delivery;
6. marks the durable inbox item applied;
7. advances `ExecutionState`.

For an agreed step, the same store boundary first writes `SharedProposal` and
the broadcast frame needed to collect signatures. A later signature update
either retains the proposal or atomically commits both memories and releases
its effects after N-of-N agreement.

The proposal transaction also owns the initiating inbox row, any consumed
timer or pending callout/signing continuation, the event position, both
post-dispatch memory images, any replacement pending continuation, the exact
`StepCommitment`, and the protocol outbox rows. Recovery can therefore tell
that the initiating event has already run even though its memories are not yet
committed. The store also retains whether `React` has run after an agreed step,
so removing the private cursor cannot cause a reaction to run twice.

While a proposal is staged, outbox leasing exposes only protocol frames needed
to finish its agreement. Local program effects remain durable but unavailable
for delivery until the proposal clears. Likewise, the pending-request
projection hides a committed continuation when the staged proposal consumes or
replaces it; a replacement request is not visible until its producing proposal
commits. An unchanged callout may remain pending behind an unrelated proposal.
The actor reports that agreement wait as an expected typed result. The daemon
may retain and retry only that same submission while the serialized actor
continues processing inbound signatures, rather than treating every temporary
dependency failure as permission to replay agent input.

The durable inbox, outbox, timer rows, leases, compare-and-set version, and
single SQLite transaction remain. They provide crash recovery and exactly-once
state advancement. They do not require a second semantic effect enum.

Program effects are stored as `Effect` with their event position and effect
index. Destination and lease columns remain store metadata. Protocol frames
such as step signatures remain protocol frames in the transport outbox; they
are stored as one durable row per destination, and the producer is excluded
from its own broadcast deliveries. They are not guest effects. This removes
`TimerMutation` and `DurableEffect` from the program transition pipeline while
preserving durable delivery.

`RetryInput` reissues the one callout that is already pending; it does not name
or create a second continuation. When an answer consumes that continuation,
the same transaction acknowledges the originating `Callout` and cancels every
undelivered `RetryInput` row that referred to it. Entering any terminal status
likewise cancels obsolete `Callout`, `Sign`, and `RetryInput` rows without
deleting their history or claiming that delivery occurred.

Publishing a receipt is not yet the observer-visible completion boundary. The
actor first settles every pending or leased protocol-frame outbox row for that
execution, including its final `End`, `Abort`, or signature frame. Only then
does it emit `ReceiptPublished` and the terminal session message. This ordering
prevents the supervisor from shutting down the sole delivery owner while a
peer still lacks the terminal evidence.

A Host-local fatal failure uses the same actor loop while terminal delivery is
outstanding. The actor continues accepting inbound peer frames, retrying its
durable outbox, and advancing terminal proof; it does not enter a private
blocking delivery loop. A transport setup failure is therefore retried from
the durable row, and a peer can acknowledge or contribute terminal evidence
while this Host's own final frame is still awaiting acknowledgement.

The event row is immutable evidence: execution identity, event position,
event, effects, and digest are written once. Proposal signatures, agreed steps,
terminal evidence, pending continuations, and outbox delivery disposition each
remain in their existing authoritative records rather than being mirrored as a
mutable event status or version.

After a confirmed transaction rollback, the actor restores both pre-dispatch
memories. If the store response is interrupted and the outcome is unknown, the
actor does not assume rollback: it reloads the authoritative execution version,
committed memory images, and any `SharedProposal`, then restores the resident
instance from that durable state. Proposed memories become live only after the
store confirms the committed version. If a committed process stops before
delivery, the outbox and timer tables resume the work after restart.

An operational failure while terminal signatures are still incomplete cannot
replace that proof with an abort at the same cursor. A focused store operation
preserves the partial terminal proof as `Incomplete` and cancels active timers
in the same transaction.

## Receipt and verification consequence

The former verifier could execute public steps only because shared handlers had
no local state, participant identity, private input, or entropy. A unified
context deliberately removes that restriction. A receiver can use local state
while deciding whether it reaches the advertised shared hash, so a portable
receipt cannot independently reproduce the transition without disclosing that
participant's local memory and event history.

Portable verification therefore checks the activation, ordered
`StepCommitment` chain, N-of-N signatures, shared pre-state and post-state
hashes, terminal evidence, and receipt identity. It proves that every activated
participant accepted the same shared result. It does not claim that a verifier
can recreate participant-specific execution from public receipt bytes.

The existing light-verification behavior is the portable proof boundary. It is
the only receipt verification path: it checks the activation, ordered
`StepCommitment` chain, N-of-N signatures, shared pre-state and post-state
hashes, terminal evidence, and receipt identity. It does not load Wasm or
re-execute participant-specific history. Host-local event, effect, and entropy
records remain available only to that Host's diagnostics.

Changing `TraceEntry` changes its entry hash and the bytes covered by
`StepCommitment`. The implementation therefore uses explicit version boundaries:
ABI 21, execution profile 2, `TraceEntry` format 2, the v3 step-commitment
domain, receipt artifact/body 3, and the v3 receipt-identity domain. Wire,
trace, receipt, and proof decoders reject unsupported versions; old and new
signatures must never verify under the same version.

## Type and storage reduction

The target keeps one value for each session concept.

| Remove | Keep or revise |
|---|---|
| `SharedContext` | `Context` with mutable shared and local state |
| `PublicEvent`, `PrivateEvent` | `Event` |
| `PublicEffect`, `PrivateEffect` | `Effect` |
| `SharedCall`, `LocalCall` | one sandbox dispatch call |
| `SharedCallResult`, `LocalCallResult` | one accepted/rejected dispatch result |
| `apply_shared`, `apply_local` | `arena0_dispatch` through the actor-owned instance |
| shared/local function exports | shared/local memory exports |
| `SharedDelta`, `PrivateDelta`, `PrivateCause`, `PrivateContext` | direct validation of the dispatch result |
| `ExecutionInput` | actor methods for activation, event dispatch, signatures, and stop |
| `CommitPlan` | one transactional store operation per actor action |
| `TimerMutation` | direct timer-table updates from `Effect::SetTimer` |
| `DurableEffect` | persisted `Effect` rows plus protocol-frame outbox rows |
| `PrivateCommit`, `PrivateRecord` | local event/effect rows owned by the Host store |
| public/private cursors and commit tables | one event position plus the existing agreed-step position |

`ExecutionState`, `SharedProposal`, `TraceEntry`, `StepCommitment`, step and
terminal signatures, pending continuations, timers, inbox rows, and outbox rows
remain because they own distinct runtime or proof responsibilities.

## Migration order

1. Change the SDK callbacks to one `Context` and update programs to use shared
   helpers from both originating and receiving events.
2. Introduce the three-memory ABI and `arena0_dispatch`; validate canonical
   state-memory bytes, fixed capacities, work-memory and global reset,
   rollback, and recovery in the sandbox.
3. Make `ExecutionActor` own the Wasm instance and route every `Event` through
   the single dispatch method.
4. Extend broadcast frames, `SharedProposal`, `StepCommitment`, and
   `TraceEntry` for advertised post-state hashes and participant-specific local
   results.
5. Replace reducer plans with focused transactional store methods while
   retaining the durable inbox, outbox, timers, leases, and version checks.
6. Migrate stored executions at the ABI boundary or reject pre-change active
   executions explicitly. Do not attempt to resume one execution across both
   state models.
7. Delete the split types, exports, reducer branches, database tables, and
   self-apply broadcast path after all callers use the single path.
8. Keep `docs/protocol-architecture.md`, `docs/technical-overview.md`, SDK
   documentation, receipt documentation, actor lifecycle notes, and independent
   Wasm fixtures aligned while implementation proceeds. Remove any claim that
   allocator memory persists between calls or that recovery reruns an event tail.

## Acceptance cases

The refactor is complete when the following behavior is covered through the
real actor and store boundaries:

- `InputReceived` mutates shared and local state, emits `Broadcast` and
  `Callout`, reaches N-of-N agreement, commits both memories, and delivers the
  callout under one stable pending ID. Delivery may retry until acknowledged,
  but only one continuation result is accepted.
- `MessageReceived` mutates both states and emits any existing effect without
  entering a separate local call.
- timer, signature, and reaction events use the same dispatch and can produce
  the same state and effect combinations.
- a receiver that computes a different shared post-state hash withholds its
  signature and does not commit either memory or deliver effects.
- an event that is not already applying an agreed step and changes shared
  memory without a broadcast is rejected before persistence.
- changing local memory alone does not directly change `StateHash` or receipt
  bytes; a later message or shared result influenced by that memory may do so.
- `SessionEnd` with an unchanged shared hash still requires agreement on the
  terminal kind and outcome; when it is not produced while applying an agreed
  step, the same dispatch must emit the broadcast that other participants
  apply.
- an unchanged-state broadcast commits only when receivers also leave shared
  memory unchanged.
- a broadcast emitted by `MessageReceived` begins only after the current step
  commits and cannot smuggle another shared-state change into that step.
- rejected events, guest traps, fuel exhaustion, and failed store commits
  restore both memories.
- an unrecoverable guest input fault restores both memories and enters the same
  durable authenticated failure path as an unrecoverable fault from any other
  event; returning the command error to its caller does not suppress failure
  ownership in the actor.
- an interrupted store response reloads the durable version and memory images
  before another event runs.
- while `SharedProposal` exists, later session events remain queued and
  projections read the last committed memories.
- restart from each durable boundary restores the two memories, pending
  continuations, `SharedProposal`, timers, inbox, and outbox without rerunning
  earlier events.
- consuming or replacing a callout also retires every undelivered `RetryInput`
  for that callout, so delayed delivery cannot target a later continuation.
- local receipt publication does not emit terminal observer messages until the
  final protocol-frame outbox rows have been durably acknowledged or cancelled.
- a Host-local fatal failure retries a failed terminal transport setup through
  the durable outbox while the normal actor loop continues accepting peer
  terminal frames, including when the peer withholds this Host's acknowledgement
  until its own reverse-direction frame is accepted.
- a producer does not process its own broadcast as `MessageReceived`.
- stopping an unsigned proposal cancels its exact undelivered protocol frames,
  while late acknowledgement or retry is idempotent; a proposal carrying this
  Host's signature cannot be stopped.
- no active code path constructs `PublicEvent`, `PrivateEvent`, `PublicEffect`,
  `PrivateEffect`, `SharedCall`, `LocalCall`, `ExecutionInput`, `CommitPlan`,
  `TimerMutation`, or `DurableEffect`.
