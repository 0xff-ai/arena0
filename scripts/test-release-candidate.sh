#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
release_dir=${ARENA0_RELEASE_DIR:-$repo_root/target/optimized-release}
target=$(node -p 'process.platform + "-" + process.arch')
smoke_root=$(mktemp -d "${TMPDIR:-/tmp}/arena0-release-smoke.XXXXXX")
service_pid=

cleanup() {
  if [[ -n "$service_pid" ]] && kill -0 "$service_pid" 2>/dev/null; then
    child_pid=$(pgrep -P "$service_pid" | head -n 1 || true)
    kill -TERM "$service_pid" 2>/dev/null || true
    for ((attempt = 0; attempt < 50; attempt++)); do
      if ! kill -0 "$service_pid" 2>/dev/null; then
        break
      fi
      sleep 0.1
    done
    if [[ -n "$child_pid" ]] && kill -0 "$child_pid" 2>/dev/null; then
      kill -KILL -- "-$child_pid" 2>/dev/null || true
    fi
    kill -KILL "$service_pid" 2>/dev/null || true
    wait "$service_pid" 2>/dev/null || true
  fi
  rm -rf -- "$smoke_root"
}
trap cleanup EXIT

case "$target" in
  darwin-arm64 | linux-x64) ;;
  *)
    echo "unsupported release smoke target: $target" >&2
    exit 2
    ;;
esac

if [[ -z "${ARENA0_NPM_TARBALL_DIR:-}" ]]; then
  node "$repo_root/npm/assemble.mjs" "$target" "$release_dir"
fi
ARENA0_NPM_PREFIX="$smoke_root/npm" \
  "$repo_root/scripts/test-npm-package.sh" "$target"

bin_dir="$smoke_root/npm/install/node_modules/.bin"
example_dir="$smoke_root/npm/install/node_modules/@0xff-ai/arena0/examples/minimal-program"
wasm="$smoke_root/minimal.wasm"
ARENA0_CARGO_BIN="$bin_dir/cargo-arena0" ARENA0_EXAMPLE_DIR="$example_dir" \
  ARENA0_WASM_OUT="$wasm" \
  "$repo_root/scripts/test-packaged-sdk.sh"

home="$smoke_root/home"
socket="$home/arena0.sock"
log="$smoke_root/serve.log"
ARENA0_HOME="$home" "$bin_dir/arena0" serve \
  >"$log" 2>&1 &
service_pid=$!

for ((attempt = 0; attempt < 200; attempt++)); do
  if [[ -S "$socket" ]]; then
    break
  fi
  if ! kill -0 "$service_pid" 2>/dev/null; then
    cat "$log" >&2
    echo "installed arena0 serve exited before socket readiness" >&2
    exit 1
  fi
  sleep 0.1
done
if [[ ! -S "$socket" ]]; then
  cat "$log" >&2
  echo "installed arena0 serve did not become ready within 20 seconds" >&2
  exit 1
fi

ARENA0_HOME="$home" "$bin_dir/arena0" --json run "$wasm" \
  --builtin host-01=first-allowed --builtin host-02=sample --replay \
  >"$smoke_root/result.json"
python3 - "$smoke_root/result.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    result = json.load(handle)
verified = result["verified"]
receipts = verified["receipts"]
assert result["exec"] == "completed", result
assert verified["tier"] == "full", verified
assert verified["all_verified"] is True, verified
assert verified["shared_evidence_agrees"] is True, verified
assert len(receipts) == 2, receipts
assert len({receipt["peer_id"] for receipt in receipts}) == 2, receipts
assert {receipt["receipt_id"] for receipt in receipts} == {result["receipt_id"]}, receipts
assert all(receipt["result"] == "valid" for receipt in receipts), receipts
PY

session_id=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["session_id"])' \
  "$smoke_root/result.json")
ARENA0_HOME="$home" "$bin_dir/arena0" --json verify "$session_id" \
  --hosts host-01,host-02 --replay >"$smoke_root/verified.json"
python3 - "$smoke_root/verified.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    result = json.load(handle)
assert result["tier"] == "full", result
assert result["all_verified"] is True, result
assert result["shared_evidence_agrees"] is True, result
assert len(result["ensemble"]) == 2, result
assert len(result["producers"]) == 2, result
assert {entry["host"] for entry in result["producers"]} == {"host-01", "host-02"}, result
assert all(entry["result"] == "valid" for entry in result["producers"]), result
PY

kill -INT "$service_pid"
stopped=false
for ((attempt = 0; attempt < 100; attempt++)); do
  if ! kill -0 "$service_pid" 2>/dev/null; then
    stopped=true
    break
  fi
  sleep 0.1
done
if [[ "$stopped" != true ]]; then
  cat "$log" >&2
  echo "installed arena0 serve did not stop within 10 seconds" >&2
  exit 1
fi
if wait "$service_pid"; then
  status=0
else
  status=$?
fi
if [[ $status -ne 0 ]]; then
  cat "$log" >&2
  echo "installed arena0 serve exited with status $status" >&2
  exit 1
fi
service_pid=

if [[ -S "$socket" ]]; then
  cat "$log" >&2
  echo "installed arena0 serve left the daemon socket behind" >&2
  exit 1
fi
if [[ $(grep -c 'arena0d Host stopped' "$log") -ne 2 ]]; then
  cat "$log" >&2
  echo "installed arena0 serve did not report both Hosts stopped" >&2
  exit 1
fi

echo "release candidate built and replayed the external example, then shut down cleanly"
