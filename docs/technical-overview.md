# Technical overview

Status: current Phase 1 orientation

arena0 is a Rust system for deterministic, verifiable program co-execution. Two or more Participants, who can be agents, run the same content-addressed Wasm program, agree on each public state transition, and retain canonical receipts or authenticated unilateral stop reports.

This document explains the current system as a whole. It is not a second protocol specification. The following sources remain authoritative:

| Concern | Source of truth |
| --- | --- |
| Phase 1 protocol behavior and invariants | [Protocol architecture](protocol-architecture.md) |
| Reasons for the current boundaries | [Architectural motivation](architectural-motivation.md) |
| Rust packages, dependency versions, and build profiles | [`Cargo.toml`](../Cargo.toml) and crate manifests |
| Rust toolchain and Wasm target | [`rust-toolchain.toml`](../rust-toolchain.toml) |
| Local Host API | [Local Host API](api/json-rpc.md) |
| Event API | [Local daemon events](api/events/README.md) |
| Supported platforms and commands | [Project README](../README.md) |
| Build and verification commands | [`justfile`](../justfile) |

## System at a glance

Participants are the parties to the interaction. `Host` is the runtime type
that serves a Participant; `Ensemble` groups those runtime instances. These
implementation names are used below to explain ownership and local topology.

Phase 1 runs an `Ensemble` of independent logical Hosts in one `arena0d` process. The daemon owns the shared Unix API endpoint and MCP listener. Each Host has its own identity, program catalog, SQLite store, execution actors, and receipts. `LocalTransport` connects the Hosts through bounded in-process channels and the real protocol codec.

```text
human or agent
    |
    +-- arena0 CLI -----------+
    |                         |
    +-- Host-explicit MCP ----+--> arena0d
                                  |
                                  +-- Ensemble
                                      |
                                      +-- Host A
                                      |   +-- Unix API
                                      |   +-- SQLite store
                                      |   +-- execution actors
                                      |   +-- Wasm sandbox
                                      |
                                      +-- Host B ... Host N
                                      |
                                      +-- LocalTransport
```

One machine controls the process and all Host keys. The topology proves deterministic co-execution, agreement, persistence, and receipt construction. It does not provide independent machine or key custody.

## Implementation stack

The workspace manifests own dependency selection and exact versions. The table below explains what the main technologies do.

| Area | Technology | Role |
| --- | --- | --- |
| Language | Rust 2024 edition on the stable toolchain | Host libraries, executables, SDK, and guest programs |
| Guest target | `wasm32-unknown-unknown` | Portable deterministic program artifact |
| Wasm runtime | Wasmtime with Cranelift | Validation, compilation, caching, fuel accounting, memory limits, and guest execution |
| Async runtime | Tokio | Service tasks, Unix APIs, transport channels, cancellation, and shutdown |
| Durable storage | SQLite through `rusqlite` | Per-Host identities, programs, activation, execution state, inbox, outbox, timers, and receipts |
| Deterministic encoding | Borsh | Canonical protocol, wire, stored, and guest-owned program bytes |
| Agent encoding | Serde JSON | Host API requests, responses, callouts, queries, views, and outcomes |
| Agent schemas | JSON Schema Draft 2020-12 | Input validation and program introspection |
| Content identity | BLAKE3 | Program and protocol content hashes |
| Persistent identity | Ed25519 | Host identity, tickets, and unilateral stop reports |
| Execution agreement | BLS12-381 MinSig through `blst` | Per-execution keys and N-of-N aggregate agreements |
| Local Host API | Length-prefixed JSON over one daemon Unix socket | Explicit routing to independent Hosts |
| MCP API | RMCP and Axum over Streamable HTTP | One Host-explicit agent endpoint for an Ensemble |
| Observability | `tracing` and `tracing-subscriber` | Redacted semantic events and opt-in performance records |
| CLI | Clap, Ratatui, and Crossterm | Commands, the local workspace, and the execution observatory |
| Build orchestration | Cargo and `just` | Program builds, workspace builds, tests, checks, docs, audits, and release artifacts |

The release uses one Unix domain socket per daemon for its local API. Host requests name their target explicitly. The packaged targets and platform limits live in the [project README](../README.md).

## Dependency direction

The workspace builds from protocol foundations toward applications:

```text
protocol foundations
    arena0-crypto -> arena0-wire -> arena0-program -> arena0-protocol

host capabilities
    arena0-store + arena0-transport + arena0-sandbox + arena0-verify

guest authoring and Host runtime
    arena0-sdk-macros + arena0-sdk + arena0-primitives + programs
    arena0-node

applications
    arena0-home + arena0-api + arena0-client + arena0-daemon
    arena0 + arena0d + cargo-arena0
```

This drawing is an orientation, not an exact Cargo graph. The [crate ownership table](protocol-architecture.md#3-crate-ownership) and manifests define the precise boundaries.

Protocol crates do not depend on Tokio, SQLite, Wasmtime, a transport implementation, or a presentation layer. Application crates compose those capabilities around the protocol.

The three executables retain narrow dependency closures:

```text
arena0       -> arena0-home + arena0-client + arena0-verify
arena0d      -> arena0-home + arena0-daemon
cargo-arena0 -> arena0-sandbox
```

`scripts/check-deps.sh` checks these executable boundaries and rejects runtime or
sandbox dependencies in the CLI's normal dependency graph. The sandbox's
`engine_version` integration test checks that its resolved Wasmtime version
matches the engine identity recorded in the execution profile.

## Responsibility and state ownership

Each important fact has one owner:

| Fact or capability | Owner |
| --- | --- |
| Canonical protocol state and transitions | `arena0-protocol` |
| Receipt evidence | `arena0-protocol::Receipt` |
| One Host's durable data | `arena0-store::Store` |
| One execution's durable mutation capability | non-cloneable `arena0-store::ExecutionStore` |
| One execution's live guest, signer, and transport handles | private `arena0-node::ExecutionActor` |
| One participant | `arena0-node::Host` |
| Local Host set and coordinated shutdown | `arena0-node::Ensemble` |
| Program delivery | `arena0-transport::Transport` |
| Local delivery implementation | `arena0-transport::LocalTransport` |
| Wasm validation and execution | `arena0-sandbox` |
| Identity custody, catalogs, APIs, and process lifecycle | `arena0-daemon` |
| Guest data transfer objects and concrete conversion | generated guest code from `arena0-sdk` |

The Ensemble does not own execution state. The transport does not choose participants or authenticate protocol claims. API handlers do not keep a second mutable execution object. Stored projections do not become another protocol state machine.

## Representation boundaries

arena0 separates representations when they have different trust or compatibility rules:

- Domain types express validated identities, states, transitions, and evidence.
- Wire types carry bounded, versioned protocol frames.
- Stored records preserve durable state and support corruption checks and recovery.
- Guest Borsh bytes contain concrete program values that the Host treats as opaque.
- Agent JSON carries parameters, callout answers, queries, views, and outcomes.
- Event values expose bounded, redacted observations for operators and clients.

Generated guest ABI code is the only layer that knows both a concrete program type and its Serde and Borsh implementations. The Host may validate JSON Schema. It must not reconstruct guest values through schema-driven JSON-to-Borsh conversion.

The transport carries signed protocol facts and delivery metadata. Delivery metadata never becomes identity evidence.

## Execution lifecycle

A successful execution follows one protocol path:

```text
local admission
    -> negotiation
    -> durable prepare
    -> N-of-N activation
    -> local activation commit
    -> deterministic execution
    -> N-of-N agreement on each public step
    -> terminal proof
    -> atomic artifact publication
    -> canonical receipt or unilateral stop report
```

Admission selects an exact program, participant set, parameter set, execution profile, initial state, and deadline. Each Host validates those facts locally. Every selected Host must already have the exact `ProgramHash` in its catalog.

Each Host prepares its activation record before it signs. A Host starts execution only after it validates and commits the complete activation. Hosts may commit at different wall-clock times. The process has no global start barrier.

Every public transition binds the session, position, prior state, next state, event, effects, fuel, and replayable randomness. It commits only after every activated participant signs the same commitment.

The [local flow walkthrough](flow-walkthrough.md) traces this lifecycle through a two-Host session.

## Deterministic guest boundary

The guest program owns protocol-specific meaning. It defines roles, parameters, shared and local state, messages, callouts, transitions, views, and outcomes.

The Host owns deterministic execution conditions. It validates the module and required exports, enforces fuel and memory limits, supplies replayable randomness, persists results, and performs explicit effects.

Every semantic guest call uses a fresh bounded Wasm instance. Shared calls may replace only shared state. Local calls may replace only local state. Read-only calls must not change guest state or emit effects.

Content addressing binds an execution to exact Wasm bytes. The execution profile binds proof-relevant runtime configuration. Wasmtime compilation is cached within a process and in a persistent cache below the arena0 home.

## Persistence and recovery

Each Host owns one SQLite database and one blocking database-owner thread. The daemon reserves the store before opening or creating signing keys. The same store owns the latest agent software label (`user_agent`), separate from protocol state. Cloneable handles submit bounded operations to that owner. A non-cloneable `ExecutionStore` grants exclusive mutation authority for one execution.

The pure protocol reducer returns a commit plan rather than performing I/O. The store applies the plan with a version compare-and-set. One SQLite transaction writes the execution aggregate, trace or private records, timers, outbox effects, and the applied inbox status.

Inbound execution frames enter the durable inbox before the transport receives an acknowledgement. Reducer application happens later. This split lets a restarted Host recover accepted work without claiming that the work already changed protocol state.

Outbound effects use durable rows and leases. A send failure leaves recoverable work. Lease expiry allows a later actor to retry without deleting causal history.

Recovery validates stored state and every nested projection before it exposes the execution. Corruption produces an error. Recovery does not invent missing state or choose between conflicting histories.

## Concurrency and shutdown

Long-lived state uses one clear task or resource owner. Execution actors own live execution capabilities. The store thread owns its SQLite connection. The Ensemble owns coordinated local shutdown. The daemon owns its services and child task lifecycles.

MCP `open_host` adds a Host to the running Ensemble through serialized, supervised provisioning. A supplied local ID reuses its durable identity; an omitted ID creates a fresh namespace. The daemon publishes the service after recovery. Persisted Hosts outside the startup set reopen on demand. Shutdown settles pending provisioning before releasing stores.

Queues, channels, frames, blobs, guest calls, agent responses, pending work, and shutdown waits are bounded. Backpressure reaches the component that creates work instead of becoming unbounded memory growth.

Cancellation must preserve a recoverable durable boundary. A graceful shutdown stops new work, resolves or rejects pending work within its bounds, closes services and transports, and joins owned tasks and subprocesses.

## Trust and security boundaries

Phase 1 assumes one machine operator controls all supervised Hosts. Separate identities and stores do not defend against compromise of that machine.

Within that boundary, each Host still validates protocol facts independently. N-of-N agreement prevents the system from hiding one selected Host's disagreement inside a majority result. The same rule allows any selected Host to stop progress.

Guest code has no ambient access to the network, filesystem, credentials, clock, or process. It can request only the effects exposed by the ABI. The Host applies policy and bounds before it performs an effect.

System events and API projections exclude parameters, outcomes, callout context, signatures, private state, program bytes, and keys. Receipt evidence contains public protocol facts, but receipt publication is a separate action.

arena0 is pre-1.0 and has not had an independent security audit. Report vulnerabilities through the process in [SECURITY.md](../SECURITY.md).

## Receipts and verification

Each Host independently persists evidence. Completion and shared program stops
produce the same canonical receipt on every Host retaining the same agreed
execution. Unilateral stops produce distinct authenticated reports. Both are
exported through `ReceiptArtifact` and addressed by a content-derived `ReceiptId`.
The store owns local production and import provenance separately from the bytes.

Light verification checks the activation, identities, trace chain, aggregate agreements, terminal evidence, and receipt identity without loading Wasm.

Full verification first performs light verification. It then loads the exact program and execution profile, repeats initialization and every public call, and compares state hashes, effects, fuel, randomness, and the terminal result.

A receipt proves what the selected Hosts agreed under one program. It does not prove an external payment, task completion, legal identity, or asset transfer.

## Observability

arena0 keeps semantic and operational observations separate.

The daemon emits stable, redacted Host lifecycle events. It projects one validated occurrence into structured `arena0::system_event` records and local API `EventFrame` values. Events describe behavior but do not drive the state machine or become receipt evidence.

The opt-in `arena0::performance` target records aggregate and per-item timings. Performance fields do not enter protocol values, durable state, receipts, or semantic events.

Daemon information and event frames share a Host metadata projection: local ID, cryptographic peer ID, and optional user agent. Each event captures the metadata at emission, preserving earlier labels in buffered history.

The CLI, execution observatory, JSON output, and MCP tools project typed Host state. Presentation code does not own protocol or execution state.

`arena0 launch` performs headless coordinated setup and supervises configured
local drivers. `arena0 monitor` attaches independently to the existing daemon
and uses the same terminal observatory for multiparty execution tables,
guest-owned textual views, activity, and public agreement. The monitor can
answer one pending callout through the existing Host submission boundary;
competing stale answers receive `CalloutNotPending`. MCP activity has its own
bounded operational stream, separate from semantic Host events.

## Fixed Phase 1 constraints

The [protocol architecture](protocol-architecture.md) defines the complete invariant set. The constraints most relevant to implementation are:

- N-party execution is the default, and bilateral execution is the two-party case.
- Every public state transition requires N-of-N agreement.
- Every Host owns independent durable state and its own copy of the receipt or stop report.
- Admission is exact and local. It has no discovery fallback or implicit program acquisition.
- Activation uses prepare-before-sign and commit-before-`SessionStarted` ordering.
- The inbox and outbox preserve work across restart and duplicate delivery.
- Guest execution and every ABI crossing are bounded.
- Program Borsh values remain opaque to the Host.
- Agent-facing values remain JSON.
- Observations remain redacted and outside proof semantics.
- Remote discovery, addressing, relay, program transfer, and public event aggregation are outside Phase 1.

The project does not preserve obsolete APIs, formats, adapters, or migrations unless a current requirement needs them. A replacement moves every caller to the final design and removes the old path in the same change.

## Build and verification

The root `justfile` defines the canonical commands:

```bash
just build-programs
just build
just test
just check
```

`just build-programs` builds the separate guest-program workspace and embeds program metadata. `just build` then builds the public host workspace. `just test` runs both workspaces and SDK documentation tests. `just check` verifies dependency boundaries, formatting, and Clippy.

Release work adds documentation, security audit, license, package, and release-build checks. See the `release-check` recipe for the complete sequence.

## Where to read next

- Read the [Phase 1 brief](briefs/arena0.md) for the compact product and architecture contract.
- Read the [protocol architecture](protocol-architecture.md) before changing identity, admission, activation, execution, persistence, transport, or receipt behavior.
- Read the [architectural motivation](architectural-motivation.md) for the reasons behind the major boundaries.
- Follow the [local flow walkthrough](flow-walkthrough.md) for one complete execution.
- Read [Build an arena0 program](build-a-program.md) for guest authoring.
- Read the [Local Host API](api/json-rpc.md) or [MCP guide](connect-over-mcp.md) for integrations.
- Read [CONTRIBUTING.md](../CONTRIBUTING.md) for change rules and required checks.
