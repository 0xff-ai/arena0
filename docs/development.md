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
one of `docs`, `cli`, `daemon`, `programs`, `ui`, or `full` for CI. Unknown, shared,
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
| CLI | `just check-cli test-cli check-ui test-ui` |
| Web UI | `just check-ui test-ui` |
| Daemon | `just check-daemon test-daemon` |
| Programs | `just check-programs test-programs` |

The CLI test recipe builds program Wasm and the UI, builds the sibling `arena0d`
executable, and runs the real CLI package tests. The CLI route also checks the
browser's generated wire types and runs its live Playwright suites.

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

The full affected-change gate is `just build-programs check test doc check-ui test-ui`.
`just test` builds program Wasm and the UI, runs the full host suite, runs the SDK
and primitives doctests, and runs the guest-native program tests. `just check`
checks Rust; `check-ui test-ui` adds the browser checks and live suites.
`just release-check` remains the full release gate.

## Web UI

Use Node 22 or newer and enable Corepack (`corepack enable`) to select the pnpm
version pinned in `ui/package.json`.

- `just build-ui` installs locked dependencies and builds `ui/dist`. Release
  builds embed it and fail without it; debug builds serve it from disk, so a
  new UI build shows up without recompiling the daemon.
- `just ui-types` regenerates `ui/src/api/types.gen.ts`; run it after changing
  the daemon’s browser API types.
- `just check-ui` checks generated type freshness, lint, and TypeScript types.
- `just test-ui` builds the required binaries and runs the live Playwright
  suites against Vite dev servers, so they test the UI source and need no
  `ui/dist`. Evidence goes to `ui/e2e/artifacts/`. The suites use
  `/usr/bin/google-chrome`.

For development, build the UI with `just build-ui`, then run `just ui-dev`
and open the daemon's printed URL. For the component gallery, run
`cd ui && pnpm dev` and open `http://127.0.0.1:5173/gallery`.

## CI policy

The existing `CI` check job and status remain in place. Pull requests use the
focused route selected by `--scope`. Non-doc changes on `main` and release runs
retain the full integration gates. Docs-only changes on `main` skip Rust after
the Git diff whitespace check.

After stabilization, one integration owner runs the full gate. Workers run
their assigned checks, and successful evidence is reused until relevant inputs
change. See the [CI workflow](../.github/workflows/ci.yml) and
[release workflow](../.github/workflows/release.yml) for the job wiring.
