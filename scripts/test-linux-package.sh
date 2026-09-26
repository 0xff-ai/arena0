#!/usr/bin/env bash
set -euo pipefail
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
packed=$(cd "${1:?usage: test-linux-package.sh PACKED_DIRECTORY}" && pwd)
# Build dependencies may use the network, but installing and running the
# already-packed candidate must work without registry access.
docker build -t arena0-npm-smoke:ubuntu22 -f "$repo_root/scripts/npm-smoke.Dockerfile" "$repo_root/scripts"
docker run --rm --network=none --platform linux/amd64 \
  -v "$repo_root:/repo:ro" -v "$packed:/packages:ro" \
  -e ARENA0_NPM_TARBALL_DIR=/packages arena0-npm-smoke:ubuntu22 \
  bash scripts/test-npm-package.sh linux-x64
