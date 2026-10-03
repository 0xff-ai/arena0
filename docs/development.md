# Development checks

Use the affected-change route during development. It selects existing
repository checks from changed paths and uses the full gate when ownership is unclear.

## Select a route

`just check-affected` inspects the current checkout against `HEAD`. It includes
tracked and staged changes and untracked files. `just check-affected BASE HEAD`
checks the committed `BASE..HEAD` range; it does not include local untracked
files.

The selector behind [`justfile`](../justfile) accepts the same optional range:

```sh
scripts/check-affected.sh --plan [BASE [HEAD]]
scripts/check-affected.sh --scope [BASE [HEAD]]
```

`--plan` prints `git diff --check` and the selected recipes. `--scope` prints
one of `docs`, `cli`, `daemon`, `programs`, `client`, or `full` for CI. Unknown, shared,
mixed code owners, build files, manifests, locks, empty, or unavailable change
sets select `full`. Documentation can accompany one code owner on its focused
route; changed-file whitespace is checked in every resolved range.

A `docs` result runs the Git diff whitespace check only. It does not build or
test Rust, check Markdown links or facts, or verify a documented journey.

## Focused routes

Use the named recipes directly when the affected owner is known:

Each Rust `check-*` recipe runs focused formatting and Clippy. Each `test-*` recipe
runs the owner's tests, including its existing external proofs. Run the pair
in one `just` invocation to prepare shared guest artifacts once.

| Area | Checks |
| --- | --- |
| CLI | `just check-cli test-cli check-client` |
| TypeScript client | `just check-client test-client` |
| Daemon | `just check-daemon test-daemon check-client` |
| Programs | `just check-programs test-programs` |

The CLI test recipe builds program Wasm, builds the sibling `arena0d`
executable, and runs the real CLI package tests. The CLI and daemon routes also
check the TypeScript client's generated wire types and typecheck the client.

The daemon test recipe includes an MCP two-client admission and receipt
scenario that waits at least 40 seconds, Unix `daemon_e2e`, and actual process
startup and shutdown tests. Daemon survival tests do not prove the full public
negotiation-deadline timeout.

The program test recipe builds program Wasm (`just build-programs` first), runs
host `arena0-tests`, and runs the guest-native tests in `programs`. Pure logic
gets native unit tests in the program crate; behavior gets Arena tests in
`crates/arena0-tests`. Test runs share one Wasmtime compilation cache per process under `<target dir>/wasmtime-cache` (preserved by the CI target-dir cache; override with `$ARENA0_WASMTIME_TEST_CACHE`), so the first run compiles each guest once and later runs reuse it. A relative override or `CARGO_TARGET_DIR` resolves against the workspace root, never the process working directory, so every package's test process uses the same directory.

The scoped recipes retain the configured compiler wrapper and cache and the
`NEXTEST_PROFILE` environment (`ci` in CI). Keep those settings unchanged.

## Full gates

The full affected-change gate is `just build-programs check test doc check-client test-client`.
`just test` builds program Wasm, runs the full host suite, runs the SDK
and primitives doctests, and runs the guest-native program tests. `just check`
checks Rust; `check-client test-client` adds type freshness, TypeScript checks,
and a real-daemon HTTP client journey.
`just release-check` remains the full release gate.

## TypeScript client

Use Node 22 or newer and enable Corepack (`corepack enable`) to select the pnpm
version pinned in `npm/arena0-client/package.json`.

- `just client-types` regenerates `npm/arena0-client/src/types.gen.ts`; run it
  after changing the daemon API types.
- `just check-client` checks generated type freshness, installs locked
  dependencies, and typechecks the client.
- `just test-client` builds the guests, daemon, and client, then runs Node's
  tests against that daemon in an isolated home with an ephemeral HTTP port.
  Tests preserve daemon traces in temporary log files and print their paths.

The package version equals the arena0 release whose API it describes.
Publish with the `client-v<version>` tag via
[release-client.yml](../.github/workflows/release-client.yml), before releasing
the [UI](https://github.com/0xff-ai/arena0-ui) and then arena0. A manual workflow
run previews publication unless its `publish` input is enabled.

## CI policy

The existing `CI` check job and status remain in place. Pull requests use the
focused route selected by `--scope`. Non-doc changes on `main` and release runs
retain the full integration gates. Docs-only changes on `main` skip Rust after
the Git diff whitespace check.

After stabilization, one integration owner runs the full gate. Workers run
their assigned checks, and successful evidence is reused until relevant inputs
change. See the [CI workflow](../.github/workflows/ci.yml) and
[release workflow](../.github/workflows/release.yml) for the job wiring.
