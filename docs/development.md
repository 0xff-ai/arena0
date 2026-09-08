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
one of `docs`, `cli`, `daemon`, `programs`, or `full` for CI. Unknown, shared,
mixed code owners, build files, manifests, locks, empty, or unavailable change
sets select `full`. Documentation can accompany one code owner on its focused
route; changed-file whitespace is checked in every resolved range.

A `docs` result runs the Git diff whitespace check only. It does not build or
test Rust, check Markdown links or facts, or verify a documented journey.

## Focused routes

Use the named recipes directly when the affected owner is known:

Each `check-*` recipe runs focused formatting and Clippy. Each `test-*` recipe
runs the owner's tests, including its existing external proofs. Run the pair
in one `just` invocation to prepare shared guest artifacts once.

| Area | Checks |
| --- | --- |
| CLI | `just check-cli test-cli` |
| Daemon | `just check-daemon test-daemon` |
| Programs | `just check-programs test-programs` |

The CLI test recipe builds program Wasm, builds the sibling `arena0d` executable,
and runs the existing real CLI package tests. Those tests do not provide the
missing external PTY navigation/schema-fetch journey; AR-1 remains pending.

The daemon test recipe includes an MCP two-client admission and receipt
scenario that waits at least 40 seconds, Unix `daemon_e2e`, and actual process
startup and shutdown tests. Daemon survival tests do not prove the full public
negotiation-deadline timeout.

The program test recipe builds program Wasm, runs host `arena0-tests`, and runs
the guest-native tests in `programs`.

The scoped recipes retain the configured compiler wrapper and cache and the
`NEXTEST_PROFILE` environment (`ci` in CI). Keep those settings unchanged.

## Full gates

`just test` remains the full test route: it builds program Wasm, runs the full
host suite, runs the SDK and primitives doctests, and runs the guest-native
program tests. `just check` remains full validation. `just release-check`
remains the full release gate.

## CI policy

The existing `CI` check job and status remain in place. Pull requests use the
focused route selected by `--scope`. Non-doc changes on `main` and release runs
retain the full integration gates. Docs-only changes on `main` skip Rust after
the Git diff whitespace check.

After stabilization, one integration owner runs the full gate. Workers run
their assigned checks, and successful evidence is reused until relevant inputs
change. See the [CI workflow](../.github/workflows/ci.yml) and
[release workflow](../.github/workflows/release.yml) for the job wiring.
