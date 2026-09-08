# Minimal arena0 program

This copyable example defines one deterministic two-participant program, a
four-slot program view, and a two-replica scenario test. Its manifest depends
only on released crates and contains no repository path overrides.

Install the arena0 toolchain, copy this directory, then run:

```console
cargo test
cargo arena0 build
```

Run `arena0` for the guided flow. For the agent flow, run `arena0 skill`
and keep the local service running:

```console
arena0 serve
```

Follow the skill instructions to bind a participant with `arena0 hello`,
inspect programs, start or join an execution, answer inputs, and verify the
result through CLI commands.

The program exchanges public choices in participant order. It is deliberately
small: it demonstrates program-owned DTOs, authenticated message application,
agent callouts, deterministic outcome projection, scenario tests, and the
program view without adding a template system.
