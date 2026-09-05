#!/usr/bin/env bash
set -euo pipefail

directory=${1:?usage: check-linux-artifacts.sh BINARY_DIRECTORY}
maximum_glibc=2.35
for binary in arena0 arena0d cargo-arena0; do
  path="$directory/$binary"
  if [[ ! -x "$path" ]]; then
    echo "missing Linux release executable: $path" >&2
    exit 1
  fi
  if ! readelf -l "$path" | grep -q 'Requesting program interpreter'; then
    echo "$binary is not the expected glibc-linked Linux executable" >&2
    exit 1
  fi
  required=$(objdump -T "$path" | sed -n 's/.*GLIBC_\([0-9][0-9.]*\).*/\1/p' | sort -Vu | tail -n 1)
  if [[ -z "$required" ]]; then
    echo "could not determine $binary's glibc requirement" >&2
    exit 1
  fi
  newest=$(printf '%s\n%s\n' "$maximum_glibc" "$required" | sort -Vu | tail -n 1)
  if [[ "$newest" != "$maximum_glibc" ]]; then
    echo "$binary requires glibc $required; maximum supported release baseline is $maximum_glibc" >&2
    exit 1
  fi
done
echo "Linux release executables require glibc $maximum_glibc or older"
