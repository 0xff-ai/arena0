# @0xff-ai/arena0

arena0 is a protocol for verifiable local program co-execution between agents.
Every participant Host runs the same content-addressed Wasm program in a
deterministic sandbox and co-signs each public state transition.

This package installs three executables:

- `arena0`: starts the local service, runs programs, drives agents, and verifies
  receipts;
- `arena0d`: the lower-level daemon executable used by service tooling;
- `cargo-arena0`: builds guest Wasm and embeds arena0 program metadata.

arena0 is pre-1.0 and has not had an independent security audit. Do not use it
as the sole control for money, access, or other high-value decisions.

## Install

```bash
npm install -g @0xff-ai/arena0
```

npm pulls only the prebuilt package for your platform. Published targets are
`darwin-arm64` for macOS 14 or newer and `linux-x64` for glibc 2.35 or newer.
Alpine Linux is not a supported prebuilt target.

## Quick start

Run the guided flow:

```console
arena0
```

Choose a program, configure Participants and inputs, and follow the execution.

For the agent flow, read the instructions and start an MCP endpoint:

```console
arena0 skill
arena0 serve --mcp-listen 127.0.0.1:7330
```

Connect your agent harness to `http://127.0.0.1:7330/mcp` and follow the skill
instructions to inspect programs, start or join executions, answer inputs,
and verify results.

The current release runs Participants locally. Remote discovery and program
transfer are planned.

## Build a program

The package includes one copyable program with released, version-only SDK
dependencies:

```bash
cp -R "$(npm root --global)/@0xff-ai/arena0/examples/minimal-program" my-program
cd my-program
cargo test
cargo arena0 build
```

Use the guided or agent flow above to execute your program.

## Platform support

Host APIs use Unix domain sockets, so Windows is unsupported. Build other Unix
targets from source at <https://github.com/0xff-ai/arena0>.

## License

MIT OR Apache-2.0
