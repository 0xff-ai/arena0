#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

metadata=$(cargo metadata --locked --no-deps --format-version 1)

assert_direct() {
  local package=$1 expected actual
  shift
  expected=$(printf '%s\n' "$@" | sort -u)
  actual=$(jq -er --arg package "$package" '
    .packages[] | select(.name == $package) |
    [.dependencies[] |
      select(.kind == null and (.name | startswith("arena0-"))) | .name] |
    unique | .[]
  ' <<< "$metadata" | sort -u)
  if [[ "$actual" != "$expected" ]]; then
    printf 'Expected %s direct internal dependencies:\n%s\nActual:\n%s\n' \
      "$package" "$expected" "$actual" >&2
    exit 1
  fi
}

assert_direct arena0-cli arena0-client arena0-home arena0-verify
assert_direct arena0d arena0-daemon arena0-home
assert_direct cargo-arena0 arena0-sandbox

# Workspace-wide metadata unifies verifier features requested by other members.
# Cargo's package-scoped tree reflects the CLI's own normal dependency closure.
tree=$(cargo tree --locked -p arena0-cli --edges normal --prefix none --format '{p}')
for package in arena0-daemon arena0-node arena0-sandbox wasmtime; do
  if grep -Eq "^${package} " <<< "$tree"; then
    printf 'arena0-cli must not depend on %s:\n' "$package" >&2
    cargo tree --locked -p arena0-cli --edges normal --invert "$package" >&2
    exit 1
  fi
done

echo "Executable dependency boundaries are clean"
