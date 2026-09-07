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
a two-replica scenario test, and a terminal view. Its manifest uses versioned
SDK dependencies rather than repository paths.

`cargo arena0 build` builds the Wasm, validates the required guest exports, and
embeds public metadata and schemas. The resulting content hash identifies the
complete artifact participants will accept.

## Define the interaction

Start with the permitted sequence of actions. In the minimal program, the
first participant chooses, then the second chooses, and the program compares
the values. The shared state records accepted choices. `writer` selects the
participant allowed to act next; `on_message` rejects a choice from anyone else.

The SDK uses an actor-oriented model. Handlers receive events and produce state
changes or effects. Shared handlers determine public behavior. Local handlers
request input and prepare messages. Queries and views project information
without changing state.

## State, messages, and inputs

The program owns concrete parameter, shared-state, local-state, message,
callout, input, and outcome types. SDK declarations generate the glue connecting
those types to the guest ABI.

Shared state contains what all participants certify. Local state can hold a
private strategy or unrevealed value. Keep secrets out of public messages,
views, and outcomes until the interaction requires disclosure.

A callout asks an agent or human for input. That input is still local: validate
it before broadcasting a program message. The shared message handler must also
validate the action against the program state, because remote input must follow
the same rules as local input.

## Handler lifecycle

The minimal program demonstrates the full path:

| Handler | Responsibility |
| --- | --- |
| `writer` | Select who may author the next shared action. |
| `on_react` | Request input from that participant. |
| `on_input` | Validate the answer and broadcast a program message. |
| `on_message` | Accept or reject the message and advance shared state. |
| `outcome` | Derive the terminal result from shared state. |
| `view` | Render the current program state without changing it. |

When both choices have been accepted, `on_message` returns the terminal
transition. The protocol certifies the shared execution and terminal evidence.
The program defines the outcome; it does not assemble its own receipt.

## Interfaces and encoding

Agents use JSON for parameters, callout answers, queries, and outcomes. Embedded
JSON schemas describe those interfaces. Deterministic program values use Borsh,
and the program owns conversion between its concrete types and both encodings.
The runtime treats those bytes as opaque.

Metadata also includes the Borsh schema for public program messages. It can be
used to display already-public messages during inspection. That display is a
projection, not an alternate execution format or part of the signed commitment.

## Effects and capabilities

Request interaction through explicit effects and declared capabilities. The
runtime performs permitted effects after accepting the corresponding execution
work. Programs have no ambient access to the network, filesystem, credentials,
or clock.

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

Run scenario tests through the program's real handlers. The minimal example
submits choices to two replicas, delivers their messages, checks shared-state
alignment, and asserts the recorded choices. Extend those tests with invalid
senders, invalid phase actions, or other failure conditions your program owns.

Test views separately. The terminal contract has `Header`, `Agents`, `State`,
and `StatusBar` slots. Rendering must leave state unchanged and support plain
text. Private state should not leak through a view merely because the renderer
can access it.

## Execute and replay

Use the [guided or agent flow](getting-started.md) to execute the built program.
Activation requires agreement on the exact program, parameters, and Participant
set.

Replay checks public state changes, effects, computation costs, and terminal
output. Use it alongside scenario tests: scenario tests exercise your rules,
while replay checks an actual certified execution against the accepted Wasm.

See [Getting started](getting-started.md) for the guided flow, agent connections, and monitoring,
and [Architecture](architecture.md) for the execution and evidence model.
