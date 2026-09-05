# npm distribution

`arena0` ships to npm as a meta-package plus one prebuilt-binary package per
platform, the same shape esbuild and swc use.

- **`arena0/`** is `@0xff-ai/arena0`, the package users install. Its three
  launchers resolve the platform package whose `os`/`cpu` match the machine and
  execute `arena0`, `arena0d`, or `cargo-arena0`, forwarding argv and stdio.
  The platform packages are `optionalDependencies`, so npm downloads only the
  one that matches.
- **`arena0-darwin-arm64/`**, **`arena0-linux-x64/`** are the platform packages
  (`@0xff-ai/arena0-<target>`). Each ships all three executables, constrained by
  `os` and `cpu`. The binaries are gitignored; the release workflow drops them in.

The daemon uses unix domain sockets, so Windows is out of scope.

## Publishing

The [`Release` workflow](../.github/workflows/release.yml) is the supported path:
push a `v*` tag and it builds the binary for each target (programs wasm first; the
`arena0` build fails if any program wasm is missing, so the embed can't be empty),
assembles the platform packages, checks each tarball, and publishes all three
packages with npm provenance. Before npm publication it publishes the seven
guest-author Rust crates in dependency order and proves the minimal program
against crates.io. It needs both the `CARGO_REGISTRY_TOKEN` and `NPM_TOKEN`
secrets. The tag, Cargo workspace, and npm package versions must match. A manual
run (`workflow_dispatch`) builds without publishing unless `publish: true`.

## Local dry run

```bash
just build-release
just npm-smoke darwin-arm64 target/optimized-release # on Apple silicon macOS
just npm-smoke linux-x64 target/optimized-release    # on Ubuntu 22.04
```

The Linux assembler rejects binaries requiring newer than glibc 2.35. The
release workflow builds that artifact on Ubuntu 22.04; a newer Linux host can
run the Rust release build but cannot package it as the published Linux target.

`assemble.mjs <target> <binary-directory> [version]` copies the three executables
into the platform package and syncs the version across all three `package.json`
files (defaulting to the workspace version in `Cargo.toml`), including the main package's
`optionalDependencies` pins. It also copies arena0's two license files and the
generated Rust dependency notices for both the host binary and its embedded Wasm
programs into every package. Run `just licenses` after changing Rust
dependencies. `npm-smoke` installs the packed platform and meta packages into
an empty prefix and runs all three commands.
