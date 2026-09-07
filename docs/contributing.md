# Contributing to arena0

Thanks for helping improve arena0.

## Before you start

Read [Architecture](architecture.md) for the conceptual model and
[Programming](programming.md) for guest authoring. Read [docs/technical-overview.md](technical-overview.md) for the crate map and
[docs/protocol-architecture.md](protocol-architecture.md) before changing
identity, admission, wire, trace, or agreement behavior. Those formats define
what receipts and peers mean, so protocol changes need an explicit version and
focused format tests.

Open an issue before a large API, protocol, persistence, or dependency change.
Small fixes and docs changes can go straight to a pull request.

## Local checks

Install the repository's Git hooks after cloning:

```bash
./scripts/install-git-hooks.sh
```

The pre-push hook blocks publishing `archive/` source or destination branches to
`origin` or `0xff-ai/arena0`, including explicit refspecs and alternate remote
names. Historical branches belong in `0xff-ai/arena0-history` (the `history`
remote). Existing hooks, including Git LFS, are preserved. Installation applies
to all linked worktrees; rerun the installer after changing the hook. Hooks are
local safeguards and can be bypassed with `--no-verify`.

Install the Rust toolchain from `rust-toolchain.toml` and
[just](https://github.com/casey/just), and `jq`, then run:

```bash
just build-programs
just build
just check
just test
just doc
```

The checked-in toolchain includes `wasm32-unknown-unknown`, rustfmt, and Clippy.
Build guests before diagnosing integration failures after SDK, macro, or ABI
changes. Missing Wasm artifacts must fail tests rather than silently skip them.
The [justfile](../justfile) owns the exact commands.

Use `just build-release` to build optimized public executables and `just audit`
to check both dependency lockfiles. These are also part of the release workflow.

For release-facing changes, also run:

```bash
cargo install cargo-audit --version 0.22.2 --locked
just release-check
```

The programs form a separate Cargo workspace. The `just` recipes check both the
host workspace and the program workspace and build the Wasm artifacts needed by
integration tests.

## Change rules

- Keep the co-signed public entry content deterministic and byte-identical
  across peers. Keep per-node telemetry outside that consensus projection.
- Keep private state out of shared hashes and signed public entries.
- Preserve bounds on frames, pending work, connections, and streams.
- Add or update rustdoc and tests when caller-visible behavior changes.
- Do not weaken a test or lower a dependency version to hide a failure.
- Use Conventional Commits when a maintainer asks you to prepare a commit.

Do not include private keys, daemon homes, receipt data from real users, or
network tokens in issues, tests, or commits.

Unless you state otherwise, contributions you submit for inclusion are licensed
under the same MIT OR Apache-2.0 terms as the project.
