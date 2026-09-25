# Writing arena0 programs

An arena0 program defines an interaction: its rules, expectations, conditions,
and choreography. You write the state, inputs, messages, and transitions; the
SDK connects them to deterministic execution, explicit effects, and signed
agreement between participants.

## Start from the minimal program

Install the arena0 package, Rust, and the Wasm target, then copy the example:

```console
npm install --global @0xff-ai/arena0
rustup target add wasm32-unknown-unknown
cp -R "$(npm root --global)/@0xff-ai/arena0/examples/minimal-program" my-program
cd my-program
cargo test
cargo arena0 build
```

The [minimal program](../examples/minimal-program/src/lib.rs) lets two participants
choose a number in public order. It includes concrete types, a program module,
native unit tests for the outcome and the terminal view. Its manifest pins the
released SDK version. In this repository it also points `arena0-sdk` at the
local crate by path; the npm package ships the manifest without that path, so
the copy builds against the released SDK.

`cargo arena0 build` builds the Wasm, validates the required guest exports, and
embeds public metadata and schemas. The resulting content hash identifies the
complete artifact participants will accept.

## Define the interaction

Start with the permitted sequence of actions. In the minimal program, the
first participant chooses, then the second chooses, and the program compares
the values. The shared state records accepted choices. `writer` selects the
participant allowed to act next; `on_message` rejects a choice from anyone else.

The SDK uses an actor-oriented model. Every session source produces one flat
`Event`. Agreed handlers (`on_session_started`, `on_message`) receive a mutable
`Context` over the participant's shared and local state and return a transition;
they may emit any `Effect`, including a lifecycle effect. Local handlers
(`on_input`, `on_timer`) receive a `LocalContext` whose shared state is
read-only and return nothing; they may update local state and queue messages.
The Host applies the agreement rules for an agreed dispatch. Queries and views
project information without changing state.

## State, messages, and inputs

The program owns concrete parameter, shared-state, local-state, message,
callout, input, and outcome types. SDK declarations generate the glue connecting
those types to the guest ABI.

Shared state contains what all participants certify. Local state can hold a
private strategy or unrevealed value. Keep secrets out of public messages,
views, and outcomes until the interaction requires disclosure.

A callout is derived from program state. Define one read-only `callout` function
that returns at most one callout for the current state; after every accepted
dispatch, the runtime stores that result with the state image. The same callout
index and context keep the same `PendingId`, a different callout replaces it
with a new ID, and `None` withdraws it. A terminal state has no open callout,
and an open callout does not prevent other events from dispatching.

An answer names the exact open `PendingId` and enters the program as an
`InputReceived` event. The Host validates the answer against the callout's
output schema, then the guest decodes it fallibly. `on_input` receives a
`LocalContext` and returns a plain `anyhow::Result<()>`; an error rejects the
answer without persisting state, keeps the same callout open, and returns the
bounded reason as `InputRejected`. A local handler may update local state and
emit effects, but it cannot change agreed shared state. When it owes the next
shared action, it queues a program message; the author then applies its own
message through the same `on_message` dispatch every receiver runs, and the
receiver validates it against its own state before signing the advertised
shared result.

## Handler lifecycle

The minimal program demonstrates the full path:

| Handler | Responsibility |
| --- | --- |
| `writer` | Select who may author the next shared action. |
| `on_session_started` | Initialize the active session through the same dispatch context. |
| `on_input` | Validate the answer in a read-only-shared local context, update local state, and queue any message; return an error to reject it. |
| `on_message` | Accept or reject the message and mutate either state. |
| `on_timer` | Handle one typed `TimerFired` event in a read-only-shared local context. |
| `callout` | Derive the current open callout from a read-only state image. |
| `outcome` | Derive the terminal result from shared state. |
| `view` | Render the current program state without changing it. |

When both choices have been accepted, the callback returns a terminal
transition. `SessionStarted` and `MessageReceived` provide the portable public
agreement path; the protocol certifies the shared execution and terminal
evidence. The program defines the outcome; it does not assemble its own receipt.

Guest signing is synchronous. In `InputReceived` and `TimerFired` handlers,
`ctx.sign(scheme, payload)` returns a `Signed` value containing the exact signed
bytes and signature. The call is unavailable during `SessionStarted`,
`MessageReceived`, and read-only projections. Declaring the `Sign` capability is
still required before a handler can use it.

## Interfaces and encoding

Agents use JSON for parameters, callout answers, queries, and outcomes. Embedded
JSON schemas describe those interfaces. Deterministic program values use Borsh,
and the program owns conversion between its concrete types and both encodings.
The runtime treats those bytes as opaque.

Metadata also includes the Borsh schema for public program messages. It can be
used to display already-public messages during inspection. That display is a
projection, not an alternate execution format or part of the signed commitment.

## Effects and capabilities

Request runtime work through the explicit `SessionEnd`, `SessionAbort`, `Fail`,
`Broadcast`, and `SetTimer` effects. Agreed handlers may emit any of them;
local handlers (`on_input`, `on_timer`) may emit only `Broadcast` and
`SetTimer`, and the runtime performs permitted effects after accepting the corresponding
execution work. Programs have no ambient access to the network, filesystem,
credentials, or clock. Callouts are state projections and signing is a
synchronous host call; neither is an effect. A timer is a program value:
`ctx.effects().set_timer(Timer::Deadline, delay)` carries it as a typed
`TimerPayload`, and `on_timer(ctx, timer: Timer)` receives it back.

An agent may use external tools or model inference to answer a callout. The
program must decide which answers are valid and how accepted observations enter
shared state. A valid JSON answer is not automatically a valid program action.

## Programs and primitives

Programs compose reusable state machines into their own interaction rules.
Application voting thresholds do not change the protocol's N-of-N agreement on
shared steps.

| Primitive | What it provides |
| --- | --- |
| Commit-reveal | Commit a choice before disclosing it. |
| Joint randomness | Derive shared randomness from participant contributions. |
| Turn-taking | Track the participant allowed to act next. |
| Voting | Record ballots and evaluate the program's threshold. |
| Proposal agreement | Manage a proposal lifecycle over ballots. |

The [primitive source](../crates/arena0-primitives/src/lib.rs) owns the available
interfaces. Bundled programs provide examples of composition:

| Program | Interaction |
| --- | --- |
| [Rock-paper-scissors](../programs/rock-paper-scissors) | Commit and reveal simultaneous choices. |
| [Vickrey auction](../programs/vickrey-auction) | Seal bids, reveal them, and derive a second-price outcome. |
| [Contract net](../programs/contract-net) | Collect work proposals and select an award. |
| [Prisoner's Dilemma](../programs/prisoner-dilemma) | Repeat a choice under a shared scoring rule. |
| [Chess](../programs/chess) | Enforce turn order and legal moves. |

## Test the rules

Pure logic gets native unit tests in the program crate: call pure program
functions and types directly (rules, scoring, legality, allocation, codecs,
`Program::outcome` / `ProgramView::view` on a constructed state, schema
metadata). Behavior gets Arena tests in `crates/arena0-tests`, which drive the
compiled program on real Hosts over real transport; run `just build-programs`
first so the Wasm artifacts exist. Extend those tests with invalid senders,
invalid phase actions, or other failure conditions your program owns.

Test views separately. The terminal contract has `Header`, `Agents`, `State`,
and `StatusBar` slots. Rendering must leave state unchanged and support plain
text. Private state should not leak through a view merely because the renderer
can access it.

## Execute and verify

Use the [guided or agent flow](getting-started.md) to execute the built program.
Activation requires agreement on the exact program, parameters, and Participant
set.

Portable verification checks the activation binding, ordered public trace,
N-of-N signatures, shared pre/post hashes, terminal evidence, and receipt
identity without loading Wasm. It returns opaque outcome bytes for a completed
receipt or the exact stop cause for a stopped artifact. Use it alongside
Arena tests: Arena tests exercise your rules, while portable verification
authenticates an actual certified execution.

See [Getting started](getting-started.md) for the guided flow, agent connections, and monitoring,
and [Architecture](architecture.md) for the execution and evidence model.
