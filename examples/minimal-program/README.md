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
and connect your harness to the MCP endpoint:

```console
arena0 serve --mcp-listen 127.0.0.1:7330
```

Use `http://127.0.0.1:7330/mcp` and follow the skill instructions to inspect
programs, start or join an execution, answer inputs, and verify the result.

The program exchanges public choices in participant order. It is deliberately
small: it demonstrates program-owned DTOs, authenticated message application,
agent callouts, deterministic outcome projection, scenario tests, and the
program view without adding a template system.
