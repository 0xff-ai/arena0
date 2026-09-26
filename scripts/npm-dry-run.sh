#!/usr/bin/env bash
set -euo pipefail
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_root"
directory=${1:?usage: npm-dry-run.sh BINARY_DIRECTORY}
platform=$(node -p 'process.platform + "-" + process.arch')
output=$(mktemp -d "${TMPDIR:-/tmp}/arena0-npm-dry-run.XXXXXX")
echo "Dry-run artifacts and logs are retained at $output"
node scripts/stage-release.mjs "$platform" "$directory" "$output/dist/$platform"
node npm/release.mjs pack "$output/dist" "$output/packed" "$platform"
scripts/publish-npm.sh "$output/packed" --dry-run
if [[ "$platform" == linux-x64 ]]; then
  scripts/test-linux-package.sh "$output/packed"
else
  ARENA0_NPM_TARBALL_DIR="$output/packed" scripts/test-npm-package.sh "$platform"
fi
echo "Dry run passed for $platform. No packages were published. Tarballs: $output/packed"
