# Minimal arena0 program

This copyable example defines one deterministic two-participant program, a
four-slot program view, and a two-replica scenario test. Its manifest depends
only on released crates and contains no repository path overrides.

Install the arena0 toolchain, copy this directory, then run:

```console
cargo test
cargo arena0 build
arena0 serve
```

In another terminal, run the generated Wasm:

```console
arena0 run target/wasm32-unknown-unknown/release/arena0_minimal_program.wasm \
  --human default --builtin host-2=sample --replay
```

The coordinated run imports those exact bytes into every selected local Host
before admission. It does not transfer the program to remote machines.

The program exchanges public choices in participant order. It is deliberately
small: it demonstrates program-owned DTOs, authenticated message application,
agent callouts, deterministic outcome projection, scenario tests, and the
program view without adding a template system.
