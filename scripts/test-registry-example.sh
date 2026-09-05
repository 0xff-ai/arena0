#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
builder=${ARENA0_CARGO_BIN:?set ARENA0_CARGO_BIN to the release cargo-arena0 executable}
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' "$repo_root/Cargo.toml")
example="$repo_root/examples/minimal-program"
if ! grep -Fqx "arena0-sdk = \"=$version\"" "$example/Cargo.toml"; then
  echo "minimal-program must pin arena0-sdk to the exact workspace release $version" >&2
  exit 1
fi

smoke_root=$(mktemp -d "${TMPDIR:-/tmp}/arena0-registry-example.XXXXXX")
trap 'rm -rf -- "$smoke_root"' EXIT
mkdir -p "$smoke_root/minimal-program/src"
for file in Cargo.toml README.md rust-toolchain.toml src/lib.rs; do
  cp "$example/$file" "$smoke_root/minimal-program/$file"
done

(
  cd "$smoke_root/minimal-program"
  cargo generate-lockfile
  cargo test --locked
  PATH="$(dirname "$builder"):$PATH" cargo arena0 build -- --locked
)
echo "registry-backed minimal program tested and built with arena0-sdk $version"
