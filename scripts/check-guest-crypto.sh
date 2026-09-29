#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."
metadata=$(cargo metadata --manifest-path programs/Cargo.toml --no-deps --format-version 1)
members=$(python3 -c 'import json, sys
metadata = json.load(sys.stdin)
members = set(metadata["workspace_members"])
for package in metadata["packages"]:
    if package["id"] in members:
        print(package["name"])' <<< "$metadata")

failed=0
while IFS= read -r member; do
    tree=$(cargo tree --manifest-path programs/Cargo.toml --target wasm32-unknown-unknown -e normal -p "$member")
    if ! python3 -c 'import re, sys
blocked = {"blake3", "ed25519-dalek", "curve25519-dalek", "sha2", "tiny-keccak", "rand_chacha", "blst", "bao"}
found = set(re.findall(r"\b([a-zA-Z0-9_-]+) v[0-9]", sys.stdin.read())) & blocked
for crate in sorted(found):
    print(f"{sys.argv[1]}: forbidden guest dependency {crate}", file=sys.stderr)
sys.exit(bool(found))' "$member" <<< "$tree"; then
        failed=1
    fi
done <<< "$members"
exit "$failed"
