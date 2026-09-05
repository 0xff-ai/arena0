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

```bash
arena0 serve
```

In another terminal:

```bash
arena0 demo --replay
```

Each Host has its own identity, durable state directory, and Unix socket below
`$ARENA0_HOME/hosts/`. Use `arena0 serve --hosts default,host-2,host-3` for a
larger local Ensemble. The service exposes MCP at `http://127.0.0.1:7330/mcp`.

`ARENA0_HOME`, `HOME`, and `ARENA0_SOCKET` must be absolute paths. An explicit
`--socket` may be cwd-relative when selecting a socket directly.

The Phase 1 package contains no remote discovery or remote program transfer.

## Build a program

The package includes one copyable program with released, version-only SDK
dependencies:

```bash
cp -R "$(npm root --global)/@0xff-ai/arena0/examples/minimal-program" my-program
cd my-program
cargo test
cargo arena0 build
```

With `arena0 serve` running in another terminal, execute the resulting Wasm:

```bash
arena0 run target/wasm32-unknown-unknown/release/arena0_minimal_program.wasm \
  --human default --builtin host-2=sample --replay
```

## Platform support

Host APIs use Unix domain sockets, so Windows is unsupported. Build other Unix
targets from source at <https://github.com/0xff-ai/arena0>.

## License

MIT OR Apache-2.0
