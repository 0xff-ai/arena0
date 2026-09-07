#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
target=${1:-$(node -p '`${process.platform}-${process.arch}`')}
platform_package="$repo_root/npm/arena0-$target"
main_package="$repo_root/npm/arena0"
expected_version=$(node -p "require('$main_package/package.json').version")

case "$target" in
  darwin-arm64 | linux-x64) ;;
  *)
    echo "unsupported npm package target: $target" >&2
    exit 2
    ;;
esac
if [[ "$target" != "$(node -p '`${process.platform}-${process.arch}`')" ]]; then
  echo "cannot execute $target binaries on $(node -p '`${process.platform}-${process.arch}`')" >&2
  exit 2
fi
for binary in arena0 arena0d cargo-arena0; do
  if [[ ! -x "$platform_package/bin/$binary" ]]; then
    echo "missing packaged executable: $platform_package/bin/$binary" >&2
    exit 1
  fi
done

if [[ -n "${ARENA0_NPM_PREFIX:-}" ]]; then
  smoke_root=$ARENA0_NPM_PREFIX
  mkdir -p "$smoke_root"
else
  smoke_root=$(mktemp -d "${TMPDIR:-/tmp}/arena0-npm-smoke.XXXXXX")
  cleanup() {
    rm -rf -- "$smoke_root"
  }
  trap cleanup EXIT
fi

platform_tarball=$(npm pack --silent --pack-destination "$smoke_root" "$platform_package")
main_tarball=$(npm pack --silent --pack-destination "$smoke_root" "$main_package")
npm install --silent --ignore-scripts --omit=optional --prefix "$smoke_root/install" \
  "$smoke_root/$platform_tarball" "$smoke_root/$main_tarball"

bin_dir="$smoke_root/install/node_modules/.bin"
example_dir="$smoke_root/install/node_modules/@0xff-ai/arena0/examples/minimal-program"
for file in Cargo.toml README.md rust-toolchain.toml src/lib.rs; do
  if [[ ! -f "$example_dir/$file" ]]; then
    echo "installed npm package is missing examples/minimal-program/$file" >&2
    exit 1
  fi
done
for binary in arena0 arena0d cargo-arena0; do
  output=$("$bin_dir/$binary" --version)
  if [[ "$output" != "$binary $expected_version" ]]; then
    echo "$binary reported an unexpected version: $output" >&2
    exit 1
  fi
done
PATH="$bin_dir:$PATH" cargo arena0 --version | grep -Fx "cargo-arena0 $expected_version"

skill_file="$smoke_root/arena0-skill.md"
"$bin_dir/arena0" skill > "$skill_file"
if ! cmp -s "$skill_file" "$repo_root/skills/arena0/SKILL.md"; then
  echo "packaged arena0 skill does not match the canonical SKILL.md" >&2
  exit 1
fi
skill_json_file="$smoke_root/arena0-skill.json"
"$bin_dir/arena0" --json skill > "$skill_json_file"
node -e '
const fs = require("node:fs");
const [jsonPath, markdownPath] = process.argv.slice(1);
const value = JSON.parse(fs.readFileSync(jsonPath, "utf8"));
const markdown = fs.readFileSync(markdownPath, "utf8");
if (value.name !== "arena0" || value.markdown !== markdown) {
  throw new Error("packaged arena0 skill JSON has the wrong name or markdown");
}
' "$skill_json_file" "$repo_root/skills/arena0/SKILL.md"
echo "npm smoke test installed and ran arena0, arena0d, cargo-arena0, and arena0 skill"
