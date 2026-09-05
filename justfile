# arena0 task runner. `just --list` to see recipes.
# Build the program Wasm that `arena0d` embeds (see crates/arena0-daemon/build.rs),
# then embed each program's borsh `ProgramDefinition` custom section (read at import
# time, no compile). Idempotent: the extractor skips wasm that already has the section.
build-programs:
    cd programs && cargo run --manifest-path ../Cargo.toml -p cargo-arena0 -- build

# Build the whole workspace (programs first, so the embed is populated).
build: build-programs
    cargo build

# Build the optimized public executables.
build-release: build-programs
    cargo build --profile optimized-release -p arena0-cli -p arena0d -p cargo-arena0

# Run host and program tests (programs wasm first so integration tests do not skip).
# The doctest line covers the crate doc-tests (incl. the arena0-sdk compile_fail
# doctest), which `cargo nextest run` does not run by default.
test: build-programs
    cargo nextest run --workspace
    cargo test --doc -p arena0-primitives -p arena0-sdk
    cargo nextest run --manifest-path programs/Cargo.toml

# Format + clippy both Cargo workspaces at the project's warn level.
check:
    ./scripts/check-deps.sh
    cargo test --locked -p arena0-sandbox --test engine_version
    cargo fmt --all --check
    cargo fmt --manifest-path programs/Cargo.toml --all --check
    cargo clippy --workspace --all-targets
    cargo clippy --manifest-path programs/Cargo.toml --all-targets

# Build all public API docs and reject broken links and rustdoc warnings.
doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features --lib

# Check both lockfiles against the current RustSec advisory database.
audit:
    cargo audit --file Cargo.lock
    cargo audit --file programs/Cargo.lock

# Regenerate the dependency notices shipped with each prebuilt binary package.
licenses:
    cargo about generate about.hbs --manifest-path Cargo.toml --all-features --locked --offline --fail -o THIRD_PARTY_LICENSES.txt
    cargo about generate about-programs.hbs --manifest-path programs/Cargo.toml --all-features --locked --offline --fail -o EMBEDDED_PROGRAM_LICENSES.txt
    perl -pi -e 's/\r$//; s/[ \t]+$//' THIRD_PARTY_LICENSES.txt EMBEDDED_PROGRAM_LICENSES.txt
    perl -0pi -e 's/\n+\z/\n/' THIRD_PARTY_LICENSES.txt EMBEDDED_PROGRAM_LICENSES.txt

# Reject stale committed notices without changing the working tree.
licenses-check:
    ./scripts/check-licenses.sh

# All local gates for a release-facing change.
release-check: audit licenses-check check test doc build-release

# Start the default two-Host local Ensemble.
dev: build-programs
    cargo run -p arena0d

# Copy a built binary into its npm platform package and sync versions across packages.
#   just npm-assemble darwin-arm64 target/optimized-release
npm-assemble target directory:
    node npm/assemble.mjs {{target}} {{directory}}

# Install one assembled platform tarball and the meta-package into an empty
# prefix, then execute all three public commands through npm's launchers.
npm-smoke target directory:
    node npm/assemble.mjs {{target}} {{directory}}
    ./scripts/test-npm-package.sh {{target}}

# Assemble the host (darwin-arm64) platform package and `npm pack` the main package locally.
npm-pack-local: build-release
    node npm/assemble.mjs darwin-arm64 target/optimized-release
    cd npm/arena0-darwin-arm64 && npm pack --dry-run
    cd npm/arena0 && npm pack --dry-run

# Package the public guest-author crates, then test and build the copyable
# example outside the repository using only the resulting package sources.
sdk-package-smoke:
    cargo build -p cargo-arena0
    ./scripts/test-packaged-sdk.sh

# Exercise the packed npm toolchain and packaged SDK together from fresh state.
release-candidate-smoke: build-release
    ./scripts/test-release-candidate.sh
