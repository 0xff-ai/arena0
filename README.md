# arena0

arena0 runs the same content-addressed Wasm program across two or more Hosts
and produces one independently signed receipt per Host. Each Host executes in a
deterministic sandbox, agrees on every public state transition, and stops at
the exact edge where replicas diverge.

The launch release is local: one persistent service supervises an `Ensemble`
of independent logical Hosts connected by an in-process virtual network. Each
Host has its own identity, program catalog, SQLite store, Unix socket, and
receipt. Running those Hosts on one machine demonstrates deterministic
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

- `arena0` opens the local workspace, coordinates runs, operates one Host, and
  verifies receipts.
- `arena0d` is the persistent service process behind `arena0 serve`.
- `cargo-arena0` builds a guest program and embeds its public metadata.

Windows is not supported because Host APIs use Unix domain sockets.

## Quick start

Open the local workspace:

```console
arena0
```

Use a disposable home when you do not want the run to retain identities,
programs, executions, or receipts:

```console
arena0 --tmp
```

`--tmp` creates a private temporary `ARENA0_HOME` below
`$ARENA0_HOME/tmp/` (normally `~/.arena0/tmp/`) and removes it after every
owned Host has stopped. Compiled Wasm remains in the global arena0 cache, so
disposable runs do not pay cold compilation repeatedly. It cannot be combined
with an explicit socket or `arena0 serve`.

The workspace starts a command-scoped local service when needed. Select a
program, choose the exact Host count, assign the human-controlled Host, edit
the program parameters, and choose light verification or full receipt replay.
Launching replaces the workspace with the focused run screen.

The run screen shows:

- `LOCAL`, the Host count, and `1 machine`;
- negotiation progress, then the activated session, current step, and N-of-N
  agreement progress;
- the program's current four-slot view: `Header`, `Agents`, `State`, and
  `StatusBar`;
- the canonical public trace, including schema-decoded program messages when
  available, alongside the redacted Host system-event stream;
- the current callout and JSON input; and
- light or full-replay progress for every producer receipt.

A Session does not exist during negotiation. It is formed only after every
participant has durably activated the same offer.

For a non-interactive proof, bind each Host to a deterministic built-in and
emit one JSON result. The command starts and stops its own local service unless
the requested Hosts are already served:

```console
arena0 --json run rock-paper-scissors \
  --builtin host-01=sample \
  --builtin host-02=sample \
  --replay
```

Bare `arena0` prints help when its standard streams are not terminals. Use
`arena0 serve` when an API or MCP client needs a persistent service.

## Run local agents

Bind a human, a small built-in policy, or an executable to each selected Host:

```console
arena0 run prisoner-dilemma \
  --agent host-01=./examples/agents/tit_for_tat.py \
  --agent host-02=./examples/agents/grim.py \
  --replay
```

An executable receives one JSONL callout at a time and returns one JSON value.
The CLI invokes it directly without a shell and bounds its line size, response
time, diagnostics, and shutdown. See [Run local agents](docs/run-local-agents.md)
and the [subprocess protocol](docs/subprocess-agent-protocol.md).

The only built-in policies are `sample`, which covers the bundled supported
programs, and `first-allowed`, which selects the first value allowed by a
closed enum schema. They are demonstration policies, not general agents.

## Connect an MCP harness

Expose one Streamable HTTP endpoint for the complete Ensemble:

```console
arena0 serve --mcp-listen 127.0.0.1:7330
```

Configure `http://127.0.0.1:7330/mcp` once. Every Host-scoped tool call carries
an explicit Host reference, so a harness does not open the same MCP server once
per Host. See [Connect one harness to an Ensemble](docs/connect-over-mcp.md).

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
into every selected local Host before admission. This is coordinated local
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
Host set:

```console
arena0 verify <session-id> --hosts host-01,host-02 --replay
```

Without `--hosts`, `arena0 verify` keeps the selected single-Host behavior.

Receipts prove agreement about the program's facts. They do not prove external
claims such as payment, task completion, identity, or asset custody unless an
external system separately establishes those facts.

Agent-facing params, callout answers, queries, and outcomes cross the Host
boundary as JSON. The Host treats deterministic program Borsh values as opaque
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
