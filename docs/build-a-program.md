# Build an arena0 program

Use the copyable [minimal program](../examples/minimal-program) as the starting
point. It has a conventional Cargo manifest, one program module, a
two-replica scenario test, and no repository path dependencies.

## Copy and test it

Install the arena0 toolchain, then copy the example:

```console
npm install --global @0xff-ai/arena0
cp -R "$(npm root --global)/@0xff-ai/arena0/examples/minimal-program" my-program
cd my-program
cargo test
cargo arena0 build
```

The manifest depends on versioned `arena0-sdk`, Borsh, and Serde crates. The
build command targets `wasm32-unknown-unknown`, validates the required guest
exports, and embeds the program's public metadata.

## Keep the guest boundary explicit

The Wasm guest owns its concrete params, messages, callouts, inputs, shared and
local state, outcome, and their normal Serde and Borsh implementations.

Use the SDK macros to declare those types and the program module. Agent-facing
params, callout answers, queries, and outcomes are JSON. Deterministic program
values are Borsh, but the Host treats those bytes as opaque. Generated guest
ABI code performs conversion because it is the boundary that knows the
concrete DTO types.

Program metadata embeds JSON Schema Draft 2020-12 for agent introspection. It
also embeds the Borsh schema for the program's peer-message enum. The Host may
use that layout to show a bounded, best-effort diagnostic rendering of message
bytes that are already public. This rendering does not participate in program
execution, commitments, receipts, or replay verification.

## Define execution and presentation

The minimal example demonstrates the required ownership:

- `writer` selects the participant allowed to author the next shared action;
- `on_react` requests local input only from that participant;
- `on_input` validates the local answer and broadcasts a program message;
- `on_message` deterministically accepts or rejects that message;
- `outcome` derives terminal evidence from agreed shared state; and
- `view` projects `Header`, `Agents`, `State`, and `StatusBar` without changing
  guest state.

Scenario tests run multiple guest replicas and should assert that their shared
states align. View tests should cover all four slots and plain-text rendering.

## Run the Wasm

Run the built artifact. The command starts and stops the required local Hosts
unless the exact Host set is already served:

```console
arena0 run target/wasm32-unknown-unknown/release/arena0_minimal_program.wasm \
  --human host-01 \
  --builtin host-02=sample \
  --replay
```

The run coordinator imports that exact Wasm into each selected local Host
before exact admission. This convenience does not implement remote transfer,
program discovery, or implicit acquisition. Activation still requires N-of-N
agreement, and the Session exists only after that activation commits.

The final receipt authenticates the opaque Borsh outcome. Full replay also
returns the guest-generated JSON outcome after comparing every public step,
effect, state hash, and fuel count.
