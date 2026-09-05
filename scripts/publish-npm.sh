#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
: "${ARENA0_RELEASE_VERSION:?set ARENA0_RELEASE_VERSION to the resolved release version}"
: "${NODE_AUTH_TOKEN:?set NODE_AUTH_TOKEN to publish arena0 npm packages}"

packages=(
  npm/arena0-darwin-arm64
  npm/arena0-linux-x64
  npm/arena0
)

cd "$repo_root"
for directory in "${packages[@]}"; do
  name=$(node -p "require('./$directory/package.json').name")
  version=$(node -p "require('./$directory/package.json').version")
  if [ "$version" != "$ARENA0_RELEASE_VERSION" ]; then
    echo "$name version $version does not match release version $ARENA0_RELEASE_VERSION" >&2
    exit 1
  fi

  published=$(npm view "$name@$version" version --json 2>/dev/null || true)
  if [ "$published" = "\"$version\"" ]; then
    echo "$name $version is already published"
    continue
  fi

  npm publish --access public --provenance "./$directory"
done
