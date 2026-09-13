# npm distribution

arena0 ships as a wrapper package and two prebuilt-binary packages.

- `arena0/` is `@0xff-ai/arena0`, the package users install. Its launchers
  resolve the matching platform package and execute `arena0`, `arena0d`, or
  `cargo-arena0`, forwarding arguments and standard input/output. Exact-version
  optional dependencies select the platform package.
- `arena0-darwin-arm64/` and `arena0-linux-x64/` each ship all three
  executables, constrained by `os` and `cpu`. Generated binaries stay ignored.

The daemon uses Unix domain sockets. Windows is not supported.

## Build and promotion

The [CI workflow](../.github/workflows/ci.yml) builds release artifacts after
its checks pass on a push to `main`. The
[artifact workflow](../.github/workflows/build-artifacts.yml) builds guests
before the public executables, checks executable versions, records SHA-256
checksums and the source commit, and exercises the installed npm toolchain
with an external program built from the packaged Rust SDK.

macOS builds run natively on Apple silicon. Linux builds use Zig 0.15.2 and
cargo-zigbuild 0.22.3 with the explicit target
`x86_64-unknown-linux-gnu.2.35`. The build host does not set the minimum glibc
version. Linux artifacts must pass both the symbol-version check and an
Ubuntu 22.04 container test: offline tarball installation, all three commands,
the embedded skill, and a replay-verified interaction between two participants.

The [Release workflow](../.github/workflows/release.yml) promotes artifacts;
it does not rebuild the public executables. It requires a successful
`ci.yml` run triggered by a `main` push in this repository at the exact
checked-out commit. A supplied `ci_run_id` must meet the same conditions.
There is no fallback to another commit.

Push a `v<VERSION>` tag only after that commit's CI run succeeds. The tag and
an optional manual `version` input must match `Cargo.toml`. A tag that arrives
before CI finishes fails selection; rerun Release at the same tag once CI
passes. Artifacts are retained for 90 days. Expired or missing artifacts require
a new successful CI run for that exact commit.

Release requires both platform manifests and every binary checksum. It packs
the packages once, records tarball SHA-512 integrity, dry-runs publication,
and tests the resulting Linux tarballs on Ubuntu 22.04. It retains those
candidate tarballs even for manual dry runs. Actual npm publication consumes
those same files, with both platform packages before the wrapper.

A manual run defaults to `publish: false` and makes no registry writes.
A tag push or explicit `publish: true` publishes the seven guest-author Rust
crates in dependency order, tests the example against crates.io, then publishes
npm packages with provenance. Publication requires `CARGO_REGISTRY_TOKEN`
and `NPM_TOKEN`. Each npm name/version is checked before the first npm write:
an identical existing tarball is skipped on retry; different bytes or a failed
lookup stop publication. Publication across registries is not atomic.

## Local dry run

On Linux x64, install the pinned Zig compiler and cargo-zigbuild, then run:

```bash
cargo install --locked --version 0.22.3 cargo-zigbuild
just build-linux-release
just npm-dry-run target/x86_64-unknown-linux-gnu/optimized-release
```

The Linux smoke test requires Docker. It may download its container image and
build dependencies; package installation and execution inside the container
have no network access. The build preserves the configured Rust compiler
wrapper and flags.

On Apple silicon macOS:

```bash
just build-release
just npm-dry-run target/optimized-release
```

The dry run checks native executable versions and checksums, stages package
sources outside the checkout, inspects packed contents, runs `npm publish
--dry-run` on the tarballs, installs them into an empty prefix, and executes
the smoke test. It prints the retained artifact directory. No registry
credentials are required and no packages are published.

A local candidate contains only the native platform and the wrapper. Dirty
tracked source is recorded and permitted for a dry run. Publication requires
both supported platforms, clean artifact manifests, and a clean tracked
checkout. These local checks do not establish CI provenance; the Release
workflow owns that selection.

To exercise the external SDK example with those same tarballs:

```bash
ARENA0_NPM_TARBALL_DIR=/path/printed/by/dry-run/packed scripts/test-release-candidate.sh
just test-release-scripts
```

## Script interfaces

`assemble.mjs <target> <binary-directory> [version] [output-directory]` copies
the executables, license files, wrapper launchers, and minimal program example.
It synchronizes package versions and optional-dependency pins. Without an
output directory it writes into this npm tree; release packing always supplies
a temporary staging directory.

`node npm/release.mjs pack <artifacts> <new-output-directory> [target]` verifies
artifact identities and hashes before assembly. Omitting the target requires
both platforms. The output directory must not exist.

`scripts/publish-npm.sh <packed-directory> [--dry-run|--publish]` verifies the
packed manifest and tarball integrity before invoking npm. The default is
`--dry-run`. It never assembles or repacks a package.
