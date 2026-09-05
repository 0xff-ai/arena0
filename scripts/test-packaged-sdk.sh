#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' "$repo_root/Cargo.toml")
builder=${ARENA0_CARGO_BIN:-$repo_root/target/debug/cargo-arena0}
example_source=${ARENA0_EXAMPLE_DIR:-$repo_root/examples/minimal-program}
packages=(
  arena0-crypto
  arena0-wire
  arena0-program
  arena0-protocol
  arena0-sdk-macros
  arena0-sdk
  arena0-primitives
)

if [[ -z "$version" ]]; then
  echo "could not read the workspace package version" >&2
  exit 1
fi
if [[ ! -x "$builder" ]]; then
  echo "cargo-arena0 is not executable at $builder" >&2
  echo "build it first or set ARENA0_CARGO_BIN" >&2
  exit 1
fi
if [[ ! -f "$example_source/Cargo.toml" || ! -f "$example_source/src/lib.rs" ]]; then
  echo "minimal-program example is incomplete at $example_source" >&2
  exit 1
fi
if grep -Eq 'arena0-[a-z-]+ = \{[^}]*path' "$example_source/Cargo.toml"; then
  echo "minimal-program must use released dependencies, not repository paths" >&2
  exit 1
fi
for package in "${packages[@]}"; do
  if ! cmp -s "$repo_root/LICENSE" "$repo_root/crates/$package/LICENSE"; then
    echo "$package packaged license differs from the repository license" >&2
    exit 1
  fi
done

smoke_root=$(mktemp -d "${TMPDIR:-/tmp}/arena0-packaged-sdk.XXXXXX")
cleanup() {
  rm -rf -- "$smoke_root"
}
trap cleanup EXIT

# Package the working sources without letting temporary registry patches rewrite
# the caller's lockfile. Native verification and guest builds use these archives.
staging="$smoke_root/workspace"
mkdir -p "$staging"
cp "$repo_root/Cargo.toml" "$repo_root/Cargo.lock" "$repo_root/rust-toolchain.toml" \
  "$repo_root/LICENSE" "$repo_root/LICENSE-MIT" "$repo_root/LICENSE-APACHE" \
  "$repo_root/README.md" "$staging/"
cp -R "$repo_root/crates" "$repo_root/.cargo" "$staging/"
package_root="$staging/target/package"
(
  cd "$staging"
  export CARGO_TARGET_DIR="$staging/target"
  # Complete the unpatched, locked roots before introducing temporary patches.
  cargo package -p arena0-sdk-macros --locked --allow-dirty
  cargo package -p arena0-crypto --locked --allow-dirty
  cargo --config "patch.crates-io.arena0-crypto.path=\"$package_root/arena0-crypto-$version\"" \
    package -p arena0-wire --allow-dirty
  cargo --config "patch.crates-io.arena0-crypto.path=\"$package_root/arena0-crypto-$version\"" \
    package -p arena0-program --allow-dirty
  cargo \
    --config "patch.crates-io.arena0-crypto.path=\"$package_root/arena0-crypto-$version\"" \
    --config "patch.crates-io.arena0-wire.path=\"$package_root/arena0-wire-$version\"" \
    --config "patch.crates-io.arena0-program.path=\"$package_root/arena0-program-$version\"" \
    package -p arena0-protocol --allow-dirty
  cargo \
    --config "patch.crates-io.arena0-crypto.path=\"$package_root/arena0-crypto-$version\"" \
    --config "patch.crates-io.arena0-wire.path=\"$package_root/arena0-wire-$version\"" \
    --config "patch.crates-io.arena0-program.path=\"$package_root/arena0-program-$version\"" \
    --config "patch.crates-io.arena0-protocol.path=\"$package_root/arena0-protocol-$version\"" \
    --config "patch.crates-io.arena0-sdk-macros.path=\"$package_root/arena0-sdk-macros-$version\"" \
    package -p arena0-sdk --allow-dirty
  cargo \
    --config "patch.crates-io.arena0-crypto.path=\"$package_root/arena0-crypto-$version\"" \
    --config "patch.crates-io.arena0-wire.path=\"$package_root/arena0-wire-$version\"" \
    --config "patch.crates-io.arena0-program.path=\"$package_root/arena0-program-$version\"" \
    --config "patch.crates-io.arena0-protocol.path=\"$package_root/arena0-protocol-$version\"" \
    --config "patch.crates-io.arena0-sdk-macros.path=\"$package_root/arena0-sdk-macros-$version\"" \
    --config "patch.crates-io.arena0-sdk.path=\"$package_root/arena0-sdk-$version\"" \
    package -p arena0-primitives --allow-dirty
)

for package in "${packages[@]}"; do
  archive="$package_root/$package-$version.crate"
  python3 - "$archive" <<'PY'
import sys
import tarfile

archive = sys.argv[1]
with tarfile.open(archive, mode="r:gz") as package:
    licenses = [name for name in package.getnames() if name.endswith("/LICENSE")]
if len(licenses) != 1:
    raise SystemExit(f"{archive} must contain exactly one combined LICENSE, found {licenses}")
PY
done

mkdir -p "$smoke_root/packages"
cp -R "$example_source" "$smoke_root/minimal-program"
rm -rf -- "$smoke_root/minimal-program/target" "$smoke_root/minimal-program/Cargo.lock"
for package in "${packages[@]}"; do
  python3 -m tarfile -e \
    "$package_root/$package-$version.crate" "$smoke_root/packages"
done

mkdir -p "$smoke_root/minimal-program/.cargo"
cat >"$smoke_root/minimal-program/.cargo/config.toml" <<EOF
[patch.crates-io]
arena0-crypto = { path = "$smoke_root/packages/arena0-crypto-$version" }
arena0-wire = { path = "$smoke_root/packages/arena0-wire-$version" }
arena0-program = { path = "$smoke_root/packages/arena0-program-$version" }
arena0-protocol = { path = "$smoke_root/packages/arena0-protocol-$version" }
arena0-sdk-macros = { path = "$smoke_root/packages/arena0-sdk-macros-$version" }
arena0-sdk = { path = "$smoke_root/packages/arena0-sdk-$version" }
arena0-primitives = { path = "$smoke_root/packages/arena0-primitives-$version" }
EOF

(
  cd "$smoke_root/minimal-program"
  cargo generate-lockfile
  cargo fetch --locked
  cargo test --offline --locked
  PATH="$(dirname "$builder"):$PATH" cargo arena0 build -- --offline --locked
)

wasm="$smoke_root/minimal-program/target/wasm32-unknown-unknown/release/arena0_minimal_program.wasm"
if [[ ! -s "$wasm" ]]; then
  echo "packaged SDK smoke test did not produce $wasm" >&2
  exit 1
fi
if [[ -n "${ARENA0_WASM_OUT:-}" ]]; then
  cp "$wasm" "$ARENA0_WASM_OUT"
fi
echo "packaged SDK smoke test built $wasm"
