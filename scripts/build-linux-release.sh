#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"
zig_version=0.15.2
zigbuild_version=0.22.3
target=x86_64-unknown-linux-gnu

if [[ $(zig version) != "$zig_version" ]]; then
  echo "Linux release builds require Zig $zig_version" >&2
  exit 1
fi
if ! cargo-zigbuild --version | grep -Fqx "cargo-zigbuild $zigbuild_version"; then
  echo "install cargo-zigbuild with: cargo install --locked --version $zigbuild_version cargo-zigbuild" >&2
  exit 1
fi
rustup target add "$target"
# Keep the configured compiler wrapper and tracing rustflags. The explicit
# target makes Zig supply the glibc baseline independently of the build host.
(
  cd programs
  cargo run --locked --manifest-path ../Cargo.toml -p cargo-arena0 -- build
)
cargo zigbuild --locked --profile optimized-release --target "$target.2.35" \
  -p arena0-cli -p arena0d -p cargo-arena0
target_dir=$(cargo metadata --no-deps --format-version 1 | node -e \
  'let s=""; process.stdin.on("data", b => s += b); process.stdin.on("end", () => console.log(JSON.parse(s).target_directory))')
directory="$target_dir/$target/optimized-release"
"$repo_root/scripts/check-linux-artifacts.sh" "$directory"
echo "Linux release binaries: $directory"
if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  echo "directory=$directory" >> "$GITHUB_OUTPUT"
fi
