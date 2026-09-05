#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' "$repo_root/Cargo.toml")
: "${ARENA0_RELEASE_VERSION:?set ARENA0_RELEASE_VERSION to the resolved release version}"
: "${CARGO_REGISTRY_TOKEN:?set CARGO_REGISTRY_TOKEN to publish arena0 crates}"
if [ "$version" != "$ARENA0_RELEASE_VERSION" ]; then
  echo "release version $ARENA0_RELEASE_VERSION does not match workspace version $version" >&2
  exit 1
fi

crates=(
  arena0-crypto
  arena0-wire
  arena0-program
  arena0-protocol
  arena0-sdk-macros
  arena0-sdk
  arena0-primitives
)

published() {
  cargo info --registry crates-io "$1@$version" >/dev/null 2>&1
}

cd "$repo_root"
for crate in "${crates[@]}"; do
  if published "$crate"; then
    echo "$crate $version is already published"
    continue
  fi

  cargo publish --locked -p "$crate"
  for attempt in $(seq 1 30); do
    if published "$crate"; then
      echo "$crate $version is available from crates.io"
      break
    fi
    if [ "$attempt" -eq 30 ]; then
      echo "$crate $version did not become available from crates.io" >&2
      exit 1
    fi
    sleep 10
  done
done
