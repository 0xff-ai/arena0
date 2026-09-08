#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
native_target=$(node -p 'process.platform + "-" + process.arch')
target=${1:-$native_target}
platform_package="$repo_root/npm/arena0-$target"
main_package="$repo_root/npm/arena0"
expected_version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\([^"]*\)"/\1/p' "$repo_root/Cargo.toml")

case "$target" in
  darwin-arm64 | linux-x64) ;;
  *)
    echo "unsupported npm package target: $target" >&2
    exit 2
    ;;
esac
if [[ "$target" != "$native_target" ]]; then
  echo "cannot execute $target binaries on $native_target" >&2
  exit 2
fi
if [[ -z "${ARENA0_NPM_TARBALL_DIR:-}" ]]; then
  for binary in arena0 arena0d cargo-arena0; do
    if [[ ! -x "$platform_package/bin/$binary" ]]; then
      echo "missing packaged executable: $platform_package/bin/$binary" >&2
      exit 1
    fi
  done
fi

if [[ -n "${ARENA0_NPM_PREFIX:-}" ]]; then
  smoke_root=$ARENA0_NPM_PREFIX
  mkdir -p "$smoke_root"
else
  smoke_root=$(mktemp -d "${TMPDIR:-/tmp}/arena0-npm-smoke.XXXXXX")
fi
service_pid=
cleanup() {
  if [[ -n "$service_pid" ]]; then
    child_pid=$(pgrep -P "$service_pid" | head -n 1 || true)
    kill -TERM "$service_pid" 2>/dev/null || true
    for ((attempt = 0; attempt < 50; attempt++)); do
      if ! kill -0 "$service_pid" 2>/dev/null; then break; fi
      sleep 0.1
    done
    if [[ -n "$child_pid" ]] && kill -0 "$child_pid" 2>/dev/null; then
      kill -KILL -- "-$child_pid" 2>/dev/null || true
    fi
    kill -KILL "$service_pid" 2>/dev/null || true
    wait "$service_pid" || true
  fi
  if [[ -z "${ARENA0_NPM_PREFIX:-}" ]]; then
    rm -rf -- "$smoke_root"
  fi
}
trap cleanup EXIT

if [[ -n "${ARENA0_NPM_TARBALL_DIR:-}" ]]; then
  # Consume the exact candidate tarballs; never re-pack after validation.
  platform_tarball="$ARENA0_NPM_TARBALL_DIR/0xff-ai-arena0-$target-$expected_version.tgz"
  main_tarball="$ARENA0_NPM_TARBALL_DIR/0xff-ai-arena0-$expected_version.tgz"
else
  platform_tarball="$smoke_root/$(npm pack --silent --pack-destination "$smoke_root" "$platform_package")"
  main_tarball="$smoke_root/$(npm pack --silent --pack-destination "$smoke_root" "$main_package")"
fi
npm install --offline --ignore-scripts --omit=optional --no-audit --no-fund --prefix "$smoke_root/install" \
  "$platform_tarball" "$main_tarball"

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

# Exercise the installed CLI, sibling daemon, embedded program and replay
# verifier. The private home also prevents interference with another daemon.
env -u ARENA0_CONTEXT -u CODEX_THREAD_ID -u ARENA0_SOCKET -u ARENA0_HOST \
  ARENA0_HOME="$smoke_root/home" "$bin_dir/arena0" serve > "$smoke_root/serve.log" 2>&1 &
service_pid=$!
for ((attempt = 0; attempt < 200; attempt++)); do
  if [[ -S "$smoke_root/home/arena0.sock" ]]; then break; fi
  if ! kill -0 "$service_pid" 2>/dev/null; then
    cat "$smoke_root/serve.log" >&2
    echo "installed arena0 serve exited before readiness" >&2
    exit 1
  fi
  sleep 0.1
done
if [[ ! -S "$smoke_root/home/arena0.sock" ]]; then
  cat "$smoke_root/serve.log" >&2
  echo "installed arena0 serve did not become ready within 20 seconds" >&2
  exit 1
fi
env -u ARENA0_CONTEXT -u CODEX_THREAD_ID -u ARENA0_SOCKET -u ARENA0_HOST \
  ARENA0_HOME="$smoke_root/home" "$bin_dir/arena0" --json run rock-paper-scissors \
  --builtin host-01=sample --builtin host-02=sample --replay \
  > "$smoke_root/interaction.json"
node - "$smoke_root/interaction.json" <<'JS'
const assert = require('node:assert/strict');
const result = JSON.parse(require('node:fs').readFileSync(process.argv[2], 'utf8'));
assert.equal(result.exec, 'completed');
assert.equal(result.verified.tier, 'full');
assert.equal(result.verified.all_verified, true);
assert.equal(result.verified.shared_evidence_agrees, true);
assert.equal(result.verified.receipts.length, 2);
assert.equal(new Set(result.verified.receipts.map(receipt => receipt.peer_id)).size, 2);
JS
echo "npm smoke test completed and replay-verified an interaction between two participants"
