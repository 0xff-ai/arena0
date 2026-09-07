# arena0

One program. N autonomous agents. One verifiable result.
Wasm + p2p + state machines

availability: runtime ✅ protocol ✅ network 🚧

<!-- Suggested wording: Runs locally today. Cross-machine P2P networking is in development. -->

arena0 lets agents agree on a program, run it together, and verify what happened. You can think of it as p2p co-execution, or a form of multi-party compute.
Each participant runs the same exact deterministic Wasm program, and checks each shared state transition against the other participants. The program is content-addressed and acts like a shared state machine.

Use arena0 to coordinate independent agents around shared goals, with strict rules they cannot diverge from without leaving evidence.
Agents can use arena0 to negotiate terms, allocate work, run auctions, trace autoresearch contributions, make joint decisions, and much more.

For example, an auction program defines when bids can be submitted, when they
are revealed, and how the winner is chosen. Every agent has explicitly committed to being bound by the rules, Each participant checks the
same public transitions. arena0 is unopinionated about identity, Moving money or holding assets requires an external
system connected to those rules.

all of this happens in p2p. arena0 offers reusable primitives for cryptography (commit-reveal, joint randomness), choreography (turn-taking), decision-making (voting). These can be composed into new programs easily.


## Install and run

This opens a terminal workspace with bundled example programs:

```sh
npm install -g @0xff-ai/arena0
arena0
```

## arena0 programs

An arena0 program is a shared state machine. It defines the rules of an interaction: who can act, what they can do, and how each action changes the shared state.

Agents are bound by its rules, and must behave exactly how the program dictates. Every interaction is cryptographically authenticated via signatures and legitimized through N-of-N quorum. Divergences are captured, recorded, and can interrupt execution. At the end, all agents generate the same outcome/result, and produce an identical proof and trace of execution that anyone can verify.

arena0 programs are Wasm bytecode. They can be generated on-the-fly and introspected by agents. Two or more agents can come together, negotiate some shared outcome they want to achieve, and collaborately author a program that will bind them to their own terms and will govern the behaviour. Meta-programming is also a possibility: one program can generate and spawn other programs. The possibilities are endless.

## arena0 runtime

The runtime executes programs in a Wasmtime sandbox with bounded memory and
execution fuel, and a well-defined and versioned WIT interface.

<< add ABI table >>

Each call starts with explicit state in a fresh Wasm instance.
Public execution can be replayed with the same program to check state changes,
effects, fuel use, and the final outcome.

## arena0 protocol

<< work through the lifecycle of an execution from negotiation, session to closure, including messages exchanged >>

execution, sessions, etc.

<!-- Suggested wording:
Before execution, participants agree on the exact program, its parameters, and who is
participating. A session starts only after all participants have signed the
same offer and the activation has been durably committed. During execution,
every shared state transition requires all participants' signatures.
-->


## arena0 SDK

All programs and primitives are built with the arena0 SDK, which offers an actor-oriented programming model and syntactic sugar to make the authoring experience straightforward for agents, and easily interpretable by humans.

<!-- Suggested wording:
The Rust SDK provides the building blocks for arena0 programs. Authors define
program state, messages, and handlers; the SDK generates the code that connects
them to the runtime. Reusable primitives handle common patterns such as
commit-reveal, turn-taking, and voting.
-->











arena0 is written in Rust and uses Wasmtime. arena0 programs are WIT components conforming to a well-defined interface.

<!-- Suggested wording:
arena0 is written in Rust and uses Wasmtime. Programs are Wasm modules that
implement arena0's guest interface.
Editorial note: The current interface is a custom ABI, not WIT components.
-->


arena0 is made up of several components:

- the arena0 runtime 
- the arena0 

Arena0 : multiple participants run the same content-addressed Wasm program under deterministic limits, agree on public state transitions, and each produce a portable signed receipt.


The runtime and protocol are the first rollout stage. Cross-machine P2P networking is in progress and belongs to the full product vision, but is not a capability of the current public Phase 1 runtime.

Arena0 makes multi-party protocols portable and independently verifiable. Its long-term ambition is neutral infrastructure for coordination and economic activity between autonomous agents: participants inspect and consent to the same exact program, co-execute it across independently operated runtimes, and retain their own evidence.

arena0 runs the same content-addressed Wasm program across two or more participants
and produces one independently signed receipt per participant. Each participant executes in a
deterministic sandbox, agrees on every public state transition, and stops at
the exact edge where replicas diverge.

The launch release is local: one persistent service supervises an `Ensemble`
of independent logical participants connected by an in-process virtual network. Each
participant has its own identity, program catalog, SQLite store, Unix socket, and
receipt. Running those participants on one machine demonstrates deterministic
agreement and proof construction; it does not provide independent machine
custody or protection from compromise of that machine.

arena0 is pre-1.0 and has not had an independent security audit.

## Install

The npm package supports macOS 14 or newer on arm64 and Linux x64 with glibc
2.35 or newer, and installs all three commands. Alpine Linux is not a supported
prebuilt target.

```console
npm install --global @0xff-ai/arena0
arena0 --version
cargo arena0 --version
```

- `arena0` opens the local workspace, coordinates runs, operates one participant, and
  verifies receipts.
- `arena0d` is the persistent service process behind `arena0 serve`.
- `cargo-arena0` builds a guest program and embeds its public metadata.

Windows is not supported because participant APIs use Unix domain sockets.

## Quick start

Open the local workspace:

```console
arena0 --tmp
```

This starts a disposable home for testing, it does not retain identities, executions, or receipts.

The workspace starts a command-scoped local service when needed. Select a
program, choose the exact participant count, assign the human-controlled participant, edit
the program parameters, and choose light verification or full receipt replay.
Launching replaces the workspace with the focused run screen.

The run screen shows:

- `LOCAL`, the participant count, and `1 machine`;
- negotiation progress, then the activated session, current step, and N-of-N
  agreement progress;
- the program's current four-slot view: `Header`, `Agents`, `State`, and
  `StatusBar`;
- the canonical public trace, including schema-decoded program messages when
  available, alongside the redacted participant system-event stream;
- the current callout and JSON input; and
- light or full-replay progress for every producer receipt.

A Session does not exist during negotiation. It is formed only after every
participant has durably activated the same offer.

For a non-interactive proof, bind each participant to a deterministic built-in and
emit one JSON result. The command starts and stops its own local service unless
the requested participants are already served:

```console
arena0 --json run rock-paper-scissors \
  --builtin host-01=sample \
  --builtin host-02=sample \
  --replay
```

Bare `arena0` prints help when its standard streams are not terminals. Use
`arena0 serve` when an API or MCP client needs a persistent service.

## Launch and monitor

Launch a headless emulation and attach the observatory from another terminal:

```console
arena0 launch vickrey-auction --hosts alpha,beta,gamma,delta \
  --param item=widget --param reserve=10
arena0 --host alpha monitor
```

`launch` stays in the foreground while the execution runs. Hosts without a
`--builtin HOST=STRATEGY` or `--agent HOST=EXECUTABLE` binding wait for answers
from MCP clients or the monitor. A newly started daemon exposes MCP at
`http://127.0.0.1:7330/mcp`; choose another loopback port with `--mcp-listen`.
A reused daemon retains its own listener configuration. Launch stops only a
daemon it started when the command ends. Use `arena0 serve` first to retain the
service independently of a launch.

`monitor` discovers the Ensemble through the selected Host socket. Its overview
shows multiple Host executions and the selected execution's full-width,
guest-produced textual program view. Activity and public agreement details are
available in the same observatory. Select a pending callout and press `a` to
answer that one callout on behalf of its Host; this does not reserve input or
take ownership of the execution. If an agent answers first, the draft is kept
and the monitor reports that the callout is no longer pending. Quitting the
monitor detaches without stopping the daemon or execution.

Use `Enter` to inspect the selected execution, `4` for public agreement, and
`6` for activity. Arrow keys move through the focused view; `Esc` returns to
the overview. `/` toggles the selected session filter, `Space` freezes the
display while observation continues, and `q` detaches.

Use the same `ARENA0_HOME` in both terminals. `monitor` requires a terminal and
does not accept `--tmp` or `--json`; `arena0 watch --json` remains available for
Host event streams. MCP observations contain safe call metadata, not agent
identity, prompts, answers, or result bodies. Missing activity history after
attachment or disconnection is not reconstructed from snapshots.

## Connect a local agent

Coming soon:

- MCP

## Joining the p2p network

Coming soon.



## Build a program

The copyable [minimal program](examples/minimal-program) uses released crate
versions, includes a two-replica scenario test, and fills all four program-view
slots:

```console
cp -R "$(npm root --global)/@0xff-ai/arena0/examples/minimal-program" my-program
cd my-program
cargo test
cargo arena0 build
arena0 run target/wasm32-unknown-unknown/release/arena0_minimal_program.wasm \
  --human host-01 --builtin host-02=sample --replay
```

When a run names a local Wasm path, the coordinator imports those exact bytes
into every selected local participant before admission. This is coordinated local
import, not remote transfer or implicit acquisition. See
[Build an arena0 program](docs/build-a-program.md).

## What verification proves

Every producer receipt binds the exact program hash, ordered participant set,
activation, initial state, one aggregate agreement per shared step, fuel,
replayable randomness, and terminal evidence.

Light verification checks the cryptographic evidence without loading Wasm. It
returns an authenticated opaque Borsh outcome or the exact stop cause. Full
verification first performs those checks, then replays every public call using
the exact program and compares state hashes, effects, fuel, and terminal
output. The replay result also includes the guest-produced JSON outcome.

Re-verify every producer of a completed local session by naming its complete
participant set:

```console
arena0 verify <session-id> --hosts host-01,host-02 --replay
```

Without `--hosts`, `arena0 verify` keeps the selected single-participant behavior.

Receipts prove agreement about the program's facts. They do not prove external
claims such as payment, task completion, identity, or asset custody unless an
external system separately establishes those facts.

Agent-facing params, callout answers, queries, and outcomes cross the participant
boundary as JSON. The participant treats deterministic program Borsh values as opaque
bytes; generated guest code owns conversion to concrete program types.

See the [technical overview](docs/technical-overview.md) for the stack, system
shape, and ownership map. The [protocol architecture](docs/protocol-architecture.md)
defines the normative Phase 1 lifecycle and invariants.

## Develop arena0

The checked-in Rust toolchain includes `wasm32-unknown-unknown`, rustfmt, and
Clippy. The canonical checks are:

```bash
just build-programs
just build
just test
just check
just doc
just audit
just build-release
```

The public workspace is layered from cryptography, wire types, program ABI,
protocol, storage, transport, sandbox, and verification through the SDK,
runtime, API, clients, service, and three executable boundaries. The guest
author dependency closure is limited to `arena0-crypto`, `arena0-wire`,
`arena0-program`, `arena0-protocol`, `arena0-sdk-macros`, `arena0-sdk`, and
`arena0-primitives`.

## License

Licensed under either [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at
your option.
