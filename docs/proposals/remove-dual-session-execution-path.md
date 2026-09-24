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
and may emit `SessionEnd`, `SessionAbort`, `Fail`, `Broadcast`, or `SetTimer`.
The Host hashes only the shared Wasm memory. Agreement is applied after dispatch
when the shared result belongs to a session step; it is not encoded in separate
guest entry points, context types, event types, effect types, deltas, or commit
paths.

The design reuses the existing session terms and types: `Event`, `Effect`,
`ExecutionActor`, `ExecutionState`, `SharedProposal`, `TraceEntry`,
`StepCommitment`, and the execution record's timer state. It does not add a
second protocol vocabulary for the transient result of a dispatch.

## Scope boundary

This proposal begins after a complete `Activation` has been committed and the
Host is ready to inject `Event::SessionStarted`.

The current `Offer`, `Ticket`, `Activation`, and `NegotiationDriver` behavior
stays in place. Initial-state validation during negotiation also stays in
place. This proposal adds no guest negotiation events or effects and does not
move negotiation into the program. A later proposal may reconsider that
boundary without blocking removal of the dual session path.

The participant set, `writer` projection, N-of-N step agreement, timer state,
and terminal proof remain. Remote transport and participant discovery are also
outside this change.

## Current path to remove

Before the change, the runtime split one session event into separate public and
private guest calls, then reconstructed a transition plan outside the actor.
The split appeared in each layer:

| Layer | Current split |
|---|---|
| SDK | separate shared and local contexts; shared callbacks cannot access local state, identity, randomness, or effects |
| Protocol | separate public and private events, effects, deltas, and commit branches |
| ABI | separate shared and local state calls and result records |
| Sandbox | separate shared and local dispatch paths |
| Actor | session start and messages use the shared path; input, timers, signatures, and reactions use the local path |
| Store | public and private commits, cursors, records, and reducer branches |

This split forces a program such as chess to validate a callout response in
`on_input`, emit a broadcast, wait for the Host to deliver that broadcast back
to itself, and mutate shared state only in `on_message`. The causal operation
is spread across two Wasm calls, a self-send, and two protocol record types.

That detour is an architectural restriction, not a property required by
shared-state agreement. A callout response can update shared state, update
local state, emit a message, and arm a timer in one program dispatch. The
read-only `callout` function then derives the next question from the resulting
state. Other participants can apply the message and check that their shared
memory reaches the advertised hash.

## One session event path

```text
PROPOSED

Event
  -> ExecutionActor
  -> arena0_dispatch
       Context { shared, local, participant, session, entropy, effects }
  -> shared memory + local memory + Effect[] + derived callout
  -> validate shared hash and agreement requirements
  -> atomic store boundary
  -> current-frame delivery
```

`ExecutionActor` remains the serial owner of one participant's execution. All
session event sources use the same `Event` type:

- committed activation supplies `SessionStarted`;
- an accepted broadcast frame supplies `MessageReceived`;
- a callout answer supplies `InputReceived`;
- a timer supplies one `TimerFired` event with a typed payload;
- normal program progress supplies `React`.

No source chooses a shared or local call. The actor gives every event to the
same Wasm export with the same context and the same limits. An accepted
dispatch derives at most one open callout from the resulting state. A rejected
event or guest failure restores both memories and emits no effect.

`React` remains an ordinary event. It is useful when a committed step makes a
participant eligible to act without an external input. The change is that a
reaction may now mutate shared state directly and emit a broadcast in the same
dispatch. It runs once per agreed step even when a callout is open; a callout
is not a lock. The read-only `callout` function computes at most one callout
from the accepted post-dispatch state. An unchanged index and context retain
the same `PendingId`, a changed callout gets a new one, and no callout
withdraws it. Terminal state always has no open callout.

### SDK surface

The program keeps its `Shared`, `Local`, `Phase`, `Message`, `Input`, `Callout`,
`Params`, and `Outcome` associated types. Every mutating callback receives one
context over shared and local state. The input callback returns a plain error
when the answer is invalid:

```rust
fn on_message(
    ctx: &mut Context<Self::Shared, Self::Local>,
    from: Participant,
    message: Self::Message,
) -> MessageApply<Self>;

fn on_input(
    ctx: &mut Context<Self::Shared, Self::Local>,
    input: Self::Input,
) -> anyhow::Result<ProgramTransition<Self>>;

fn on_timer(
    ctx: &mut Context<Self::Shared, Self::Local>,
    timer: TimerPayload,
) -> ProgramTransition<Self>;

fn callout(
    ctx: &Context<Self::Shared, Self::Local>,
) -> Option<Self::Callout>;
```

`on_timer` is the one timer callback; the timer payload is the unit value for an
untyped timer. `callout` is read-only and runs after an accepted dispatch.
`Context` exposes mutable access to shared and local state together with the
existing participant, session, entropy, and effect APIs. Its state access is
explicit:

```rust
fn shared(&self) -> &Shared;
fn shared_mut(&mut self) -> &mut Shared;
fn local(&self) -> &Local;
fn local_mut(&mut self) -> &mut Local;
fn state_mut(&mut self) -> (&mut Shared, &mut Local);
```

Every successful event callback can return `Transition::Stay`,
`Transition::To`, `Transition::End`, or `Transition::Abort`. An `on_input`
error rejects the answer, restores both state memories, and leaves the current
callout open; it is not a retry effect and does not end the session.
`MessageApply` retains message accept/reject behavior. Lifecycle effects remain
available from every event without adding lifecycle methods to the effect
handle. Guest signing is a synchronous `ctx.sign(scheme, payload)` call that
is available only in `InputReceived`, `TimerFired`, and `React` handlers. It
builds a versioned, execution-bound `GuestSignData` value and returns the exact
signed bytes and signature. The supported schemes are deterministic, so a
crash rerun produces the same signature. Signing is unavailable to
`SessionStarted`, `MessageReceived`, and read-only projections.

The macro still provides typed callback routing. It decodes the flat `Event`,
decodes the program-owned message or input bytes for the selected callback,
and encodes the resulting states and effects. Program authors do not need to
manually decode an enum of opaque byte payloads. The generated router has one
mutating ABI export: `arena0_dispatch`.

Handlers complete synchronously within that dispatch. The guest ABI does not
lower asynchronous functions into resumable session work.

`initialize`, `writer`, `query`, `view`, and `outcome` remain projections or
pre-session operations rather than session event paths. Initialization receives
`&mut Shared` directly. Read-only projections receive `&Shared` and their
existing explicit arguments; they run against memory snapshots and cannot emit
effects. The `callout` function is a read-only function over both resulting
shared and local memories. The runtime invokes it after an accepted dispatch
and before storing that result; it may derive at most one open callout and
cannot emit effects or mutate either memory.

### Program structure

A participant that receives private input may apply the corresponding program
operation immediately and broadcast the program message in the same handler.
The receiver applies that message in `on_message`. Both callbacks can call the
same program helper:

```rust
fn on_input(ctx: &mut Context, input: Input) -> anyhow::Result<Transition<Phase>> {
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

The runtime invokes `callout` after the handler accepts the transition. It
serializes the returned callout as bounded JSON context and validates that
context against the program's input schema. An answer must name the exact open
`PendingId`; a mismatch returns `CalloutNotPending`. If decoding, `on_input`,
the input handler, or its resource limits reject the answer, the runtime
restores both memories, persists nothing, keeps the same ID open, and returns
`InputRejected` with the bounded reason. While an answered result is staged for
agreement, the callout remains open and a resubmission returns
`AgreementPending`.

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
shared and local memories, restores any existing `SharedProposal`, timers,
terminal evidence, and current protocol frames, and resends those frames.
Recovery restores those durable images directly; it does not rerun earlier
events to reconstruct live state.

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
participant's post-dispatch local memory, this participant's effects, and the
derived open callout. When the N-of-N aggregate arrives, one transaction
promotes both memories and installs that callout with them. A deferred broadcast
successor carries the same derived callout until it is certified. If agreement
does not complete, the proposal remains the current execution state or the
execution enters the authenticated stop flow.

The proposal is prepared before this Host publishes its signature. Once the
store contains a `SharedProposal`, projections continue to read the last
committed images until certification. A participant signs only after the
proposal is durable. A timeout does not authorize a different proposal at the
same position. Before this Host signs, an authenticated stop may clear the
proposal. After this Host signs, the proposal is irrevocable: a peer may
already be able to assemble N-of-N agreement, so the execution refuses its own
stop at that cursor. It still accepts an authenticated peer abort or failure at
the cursor unless that peer's signature is already in the staged proposal.

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
   proposal is the current staged message. A broadcast staged by an already-
   applied step is delivered only after that step commits.

Every event may still mutate either state and emit the five defined effects.
These rules reject results that cannot be assigned an unambiguous agreement
boundary. They do not restore event-specific context types or separate Wasm
entry points.

## Persistence without the reducer pipeline

The actor serializes event handling, owns the live execution capability, and
is the only writer of that execution's data. It validates a dispatch and hands
the complete transition record to the store instead of constructing a second
reducer plan.

For a transition that does not stage a shared proposal, one SQLite transaction:

1. checks the expected execution version as a corruption tripwire;
2. writes the shared and local memory images;
3. writes the accepted event, emitted effects, timer changes, and callout;
4. advances `ExecutionState` and any terminal proof facts.

For an agreed step, the same store boundary writes the `SharedProposal`, its
post-dispatch memories, effects, derived callout, and current protocol frames.
When N-of-N agreement is certified, one transaction promotes the proposal and
installs its callout with the committed memories. A participant signs only
after the proposal is durable.

There are no inbox or outbox tables, leases, store owner thread, command queue,
write-through working set, or compare-and-set retry loop. The version check
detects a corrupted or unexpectedly changed record; it does not coordinate
competing execution writers. The actor remains the sole authority for applying
the protocol transition.

While a proposal is staged, projections read the last committed memories and
the proposal retains its complete post-dispatch record. Current frames are
resent independently to each peer. A receiver acknowledges a frame after its
committed apply or a duplicate/stale decision; a frame that is not yet
applicable receives a retryable `not yet` response and is not stored. An
answered callout remains open while its result awaits agreement, so a
resubmission returns `AgreementPending` rather than dispatching the answer a
second time.

Program effects are part of the transition record. A broadcast staged by an
already-applied step becomes current only after that step commits, and the
producer does not apply its own broadcast as a second guest event. Timer state
is updated in the same transaction as the transition; after restart, the actor
reads due timers from execution state and supplies one typed `TimerFired` event.

Publishing the receipt is the observer-visible completion boundary. The actor
then remains alive while the execution's end phase is `Ending`, until every
peer confirms the same conclusion, and retires at `Ended`. Startup resumes an
execution that is still `Ending`; the daemon supervisor does not stop
the actor merely because it observed the finished receipt.

The event record is immutable evidence: execution identity, event position,
event, effects, derived callout, and digest are written once. Proposal
signatures, agreed steps, terminal evidence, and final-frame acknowledgement
remain in their authoritative execution-state records rather than being
mirrored as a mutable event status.

After a confirmed transaction rollback, the actor restores both pre-dispatch
memories. If the store response is interrupted and the outcome is unknown, the
actor reloads the authoritative execution record, committed memory images, and
any `SharedProposal`, then restores the resident instance from that durable
state. Proposed memories become live only after the store confirms the committed
transition. The certified final step is the terminal evidence, so there is no
partial terminal state to preserve.

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
ABI 22, execution profile 3, `TraceEntry` format 2, the v3 step-commitment
domain, receipt artifact/body 3, and the v3 receipt-identity domain. Wire,
trace, receipt, and proof decoders reject unsupported versions; old and new
signatures must never verify under the same version.

## Type and storage reduction

The target keeps one value for each session concept.

| Remove | Keep or revise |
|---|---|
| Separate shared and local contexts | `Context` with mutable shared and local state |
| Separate public and private events | one `Event` |
| Separate public and private effects | one `Effect` vocabulary |
| Separate shared and local calls | one sandbox dispatch call |
| Separate dispatch results | one accepted/rejected dispatch result |
| Separate dispatch functions | `arena0_dispatch` through the actor-owned instance |
| shared/local function exports | shared/local memory exports |
| Separate state deltas and private commit records | direct validation of the dispatch result |
| Pure reducer input and commit plan | one transactional store operation per actor action |
| Timer mutation plan | direct timer-state updates from `SetTimer` |
| Durable delivery-effect records | current protocol frames held in execution state |
| public/private cursors and commit tables | one event position plus the existing agreed-step position |

`ExecutionState`, `SharedProposal`, `TraceEntry`, `StepCommitment`, step
signatures, the open callout, timers, and current protocol frames
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
5. Replace reducer plans with one complete transactional store record per actor
   transition while retaining timers, terminal evidence, and execution-state
   protocol frames.
6. Rewrite the stored execution, ABI, and profile formats in place. There is no
   compatibility path or migration between the two state models.
7. Delete the split types, exports, reducer branches, delivery tables, and
   self-apply broadcast path after all callers use the single path.
8. Keep `docs/protocol-architecture.md`, `docs/technical-overview.md`, SDK
   documentation, receipt documentation, actor lifecycle notes, and independent
   Wasm fixtures aligned while implementation proceeds. Remove any claim that
   allocator memory persists between calls or that recovery reruns an event tail.

## Acceptance cases

The refactor is complete when the following behavior is covered through the
real actor and store boundaries:

- `InputReceived` mutates shared and local state, emits `Broadcast` when the
  program needs to inform peers, reaches N-of-N agreement, and derives its
  callout from the resulting state. An unchanged callout keeps one stable
  `PendingId`; a changed callout gets a new ID.
- `MessageReceived` mutates both states and emits any defined effect without
  entering a separate local call.
- the one typed `TimerFired` event and `React` use the same dispatch and can
  produce the same state and effect combinations; `React` continues while a
  callout is open and runs once per agreed step.
- synchronous guest signing succeeds only in `InputReceived`, `TimerFired`,
  and `React` handlers, returns the exact signed bytes and signature, and is
  unavailable in `SessionStarted`, `MessageReceived`, and projections.
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
- a rejected answer—decode error, plain `on_input` error, guest trap, or input
  handler resource limit—returns `InputRejected`, persists nothing, restores
  both memories, keeps the same open `PendingId`, and allows a later valid
  answer to commit without ending the session.
- an answer with the wrong `PendingId` returns `CalloutNotPending`; a
  resubmission while the answered result is staged returns `AgreementPending`.
  A later accepted dispatch can replace the open callout without ending the
  session.
- an authenticated writer-message rejection, trap, or post-state mismatch is
  divergence: the detecting participant records a Host-signed `Fail` occurrence
  and peers receive it as `Abort`. Wrong-writer, stale, pre-state, or message
  identity mismatches are dropped instead.
- rejected events, guest traps, fuel exhaustion, and failed store commits
  restore both memories; an interrupted store response reloads the durable
  execution record before another event runs.
- `SharedProposal` stores its complete post-dispatch record and derived
  callout; projections read the last committed memories until certification.
- restart restores the two memories, `SharedProposal`, timers, open callout,
  terminal evidence, and current protocol frames without rerunning earlier
  events. The actor resends those frames independently to each peer.
- a receiver acknowledges a committed apply or duplicate/stale decision. A
  frame that is not yet applicable receives a retryable `not yet` response and
  is not stored. The producer does not process its own broadcast as
  `MessageReceived`.
- local receipt publication emits the finished observation immediately. The
  end phase is `Ending` until every peer confirms the same conclusion (by
  acknowledging the terminal evidence or sending its own) or the
  end-confirmation window elapses; the actor retires at `Ended`. The supervisor
  does not stop the actor on publication alone.
- a peer abort or failure occurrence at the agreed cursor is accepted after
  local signing unless that peer signed the staged proposal; a participant's
  own stop remains refused after it signs.
- the actor is the only execution-data writer and persists one complete record
  per transition; no store owner thread, working-set cache, command queue,
  inbox, outbox, or delivery lease is needed.
