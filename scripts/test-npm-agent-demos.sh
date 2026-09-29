#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
credential_file=${1:?usage: test-npm-agent-demos.sh OPENAI_API_KEY_FILE [EVIDENCE_DIR]}
evidence_base=${2:-$repo_root/output/npm-agent-demos}
arena0_version=${ARENA0_NPM_VERSION:-latest}
codex_version=${CODEX_NPM_VERSION:-0.153.4}
demo_timeout=${ARENA0_DEMO_TIMEOUT_SECONDS:-600}
run_id=${GITHUB_RUN_ID:-local}-$$
image="arena0-npm-agent-demos:$run_id"
programs=(chess prisoner-dilemma rock-paper-scissors)
containers=()
log_pids=()

if [[ ! -s "$credential_file" ]]; then
  echo "OpenAI API key file is empty or unavailable: $credential_file" >&2
  exit 1
fi
credential_dir=$(cd "$(dirname "$credential_file")" && pwd -P)
credential_file="$credential_dir/$(basename "$credential_file")"
if [[ ! "$demo_timeout" =~ ^[1-9][0-9]*$ ]]; then
  echo "ARENA0_DEMO_TIMEOUT_SECONDS must be a positive integer" >&2
  exit 2
fi
if ! command -v docker >/dev/null; then
  echo "Docker is required for the published npm agent demos" >&2
  exit 1
fi

cleanup() {
  for pid in "${log_pids[@]}"; do
    kill "$pid" 2>/dev/null || true
  done
  if (( ${#containers[@]} > 0 )); then
    docker rm -f "${containers[@]}" >/dev/null 2>&1 || true
  fi
  docker image rm "$image" >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM

mkdir -p "$evidence_base"
evidence_base=$(cd "$evidence_base" && pwd -P)
evidence_root="$evidence_base/$run_id"
mkdir "$evidence_root"

docker build --pull \
  --build-arg "ARENA0_VERSION=$arena0_version" \
  --build-arg "CODEX_VERSION=$codex_version" \
  --file "$repo_root/scripts/npm-agent-demo-e2e.Dockerfile" \
  --tag "$image" \
  "$repo_root"

for program in "${programs[@]}"; do
  destination="$evidence_root/$program"
  mkdir -p "$destination"
  container="arena0-npm-demo-${run_id//[^a-zA-Z0-9_.-]/-}-${program}"
  containers+=("$container")
  # Codex uses a nested sandbox for shell tools. Docker's default seccomp and
  # AppArmor profiles block the required namespace and mount operations.
  docker run -d --init \
    --name "$container" \
    --hostname "arena0-$program" \
    --cap-add SYS_ADMIN \
    --security-opt seccomp=unconfined \
    --security-opt apparmor=unconfined \
    --env "ARENA0_DEMO_TIMEOUT_SECONDS=$demo_timeout" \
    --mount "type=bind,src=$credential_file,dst=/run/secrets/openai_api_key,readonly" \
    --mount "type=bind,src=$destination,dst=/evidence" \
    "$image" "$program" >/dev/null
  docker logs --follow "$container" 2>&1 | sed -u "s/^/[$program] /" &
  log_pids+=("$!")
done

failed=0
for index in "${!containers[@]}"; do
  container=${containers[$index]}
  program=${programs[$index]}
  status=$(docker wait "$container")
  wait "${log_pids[$index]}" || true
  if [[ "$status" != 0 ]]; then
    echo "$program demo failed with container status $status" >&2
    failed=1
  fi
done
if (( failed )); then
  exit 1
fi

echo "published @0xff-ai/arena0@$arena0_version completed all autonomous agent demos"
echo "evidence: $evidence_root"
