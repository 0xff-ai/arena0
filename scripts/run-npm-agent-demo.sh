#!/usr/bin/env bash
set -euo pipefail

program=${1:?usage: run-npm-agent-demo PROGRAM}
session=arena0-e2e
evidence=/evidence
timeout_seconds=${ARENA0_DEMO_TIMEOUT_SECONDS:-600}

if [[ ! "$timeout_seconds" =~ ^[1-9][0-9]*$ ]]; then
  echo "ARENA0_DEMO_TIMEOUT_SECONDS must be a positive integer" >&2
  exit 2
fi

case "$program" in
  chess)
    display_name=Chess
    selection_steps=0
    ;;
  prisoner-dilemma)
    display_name="Prisoner's Dilemma"
    selection_steps=3
    ;;
  rock-paper-scissors)
    display_name="Rock-Paper-Scissors"
    selection_steps=4
    ;;
  *)
    echo "unsupported agent demo: $program" >&2
    exit 2
    ;;
esac

if [[ ! -s /run/secrets/openai_api_key ]]; then
  echo "the OpenAI API key secret is empty or unavailable" >&2
  exit 1
fi
mkdir -p "$evidence"
chmod 0700 "$evidence"

# Codex stores its authenticated state in this disposable container. Feeding the
# key on stdin keeps it out of the process arguments, environment, and evidence.
codex login --with-api-key < /run/secrets/openai_api_key >/dev/null 2>&1
codex login status >/dev/null 2>&1
printf 'model_reasoning_effort = "low"\n' > /root/.codex/config.toml

arena0_version=$(npm list --global --depth=0 @0xff-ai/arena0 --json | node -e '
  let input = "";
  process.stdin.on("data", chunk => input += chunk);
  process.stdin.on("end", () => process.stdout.write(JSON.parse(input).dependencies["@0xff-ai/arena0"].version));
')
codex_version=$(codex --version | awk '{print $NF}')
printf 'program=%s\narena0=%s\ncodex=%s\nreasoning=low\n' \
  "$program" "$arena0_version" "$codex_version" > "$evidence/versions.txt"

strip_ansi() {
  python3 - "$1" "$2" <<'PY'
import pathlib
import re
import sys

source, destination = map(pathlib.Path, sys.argv[1:])
text = source.read_text(errors="replace")
text = re.sub(r"\x1b(?:[@-_][0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1b\\))", "", text)
destination.write_text(text)
PY
}

capture_pane() {
  local target=$1
  local destination=$2
  tmux capture-pane -p -e -S - -t "$target" > "$destination.ansi"
  strip_ansi "$destination.ansi" "$destination.txt"
}

cat > /tmp/arena0-launch.sh <<'SH'
#!/usr/bin/env bash
set +e
arena0 launch --agents
status=$?
printf '%s\n' "$status" > /evidence/launcher.status
exec sleep infinity
SH
chmod 0755 /tmp/arena0-launch.sh

tmux new-session -d -x 180 -y 55 -s "$session" /tmp/arena0-launch.sh
tmux set-option -t "$session" remain-on-exit on
tmux set-option -t "$session" history-limit 100000

launcher="$session:0.0"
for ((attempt = 0; attempt < 60; attempt++)); do
  capture_pane "$launcher" "$evidence/launcher-selection"
  if grep -Fq 'PROGRAMS' "$evidence/launcher-selection.txt"; then
    break
  fi
  sleep 1
done
if ! grep -Fq 'PROGRAMS' "$evidence/launcher-selection.txt"; then
  echo "$program: launcher did not show the program catalog" >&2
  exit 1
fi

# The published catalog is sorted by display name. Home plus a bounded number
# of Down keys exercises the same selection path a user follows in the TUI.
tmux send-keys -t "$launcher" g
for ((step = 0; step < selection_steps; step++)); do
  tmux send-keys -t "$launcher" Down
done
sleep 1
capture_pane "$launcher" "$evidence/launcher-selected"
if ! grep -Fq "▸ $display_name" "$evidence/launcher-selected.txt"; then
  echo "$program: launcher selected a program other than $display_name" >&2
  exit 1
fi

socket=
for ((attempt = 0; attempt < 120; attempt++)); do
  socket=$(find /root/.arena0/tmp -type s -name arena0.sock -print -quit 2>/dev/null || true)
  [[ -n "$socket" ]] && break
  sleep 1
done
if [[ -z "$socket" ]]; then
  echo "$program: isolated arena0 daemon socket did not appear" >&2
  exit 1
fi
demo_home=$(dirname "$socket")

case "$program" in
  chess)
    cat > "$evidence/AGENTS.md" <<'EOF'
# Bounded demo strategy

Follow the arena0 launcher instructions and use this legal Fool's Mate sequence
to keep the CI run bounded. Follow the authored position: submit `f2f3` from the
starting position, `e7e5` after `f3`, `g2g4` after `e5`, and `d8h4` after `g4`.
Submit only when the runtime gives your execution the move callout.
EOF
    ;;
  prisoner-dilemma)
    cat > "$evidence/AGENTS.md" <<'EOF'
# Bounded demo strategy

Follow the arena0 launcher instructions. Choose `Cooperate` on every turn and
proceed immediately after printing the required authored view.
EOF
    ;;
  rock-paper-scissors)
    cat > "$evidence/AGENTS.md" <<'EOF'
# Bounded demo strategy

Follow the arena0 launcher instructions. The creator chooses `Rock` and the
joiner chooses `Paper` on every turn. Proceed immediately after printing the
required authored view; two joiner wins will end the game early.
EOF
    ;;
esac
cp "$evidence/AGENTS.md" "$demo_home/workspace/AGENTS.md"

tmux send-keys -t "$launcher" Enter

right_pane=
for ((attempt = 0; attempt < 120; attempt++)); do
  mapfile -t pane_ids < <(tmux list-panes -t "$session:0" -F '#{pane_id}')
  if (( ${#pane_ids[@]} == 2 )); then
    right_pane=${pane_ids[1]}
    break
  fi
  if [[ -s "$evidence/launcher.status" ]]; then
    break
  fi
  sleep 1
done
if [[ -z "$right_pane" ]]; then
  capture_pane "$launcher" "$evidence/launcher-failed"
  echo "$program: launcher did not create the second tmux pane" >&2
  exit 1
fi

tmux new-window -d -t "$session" -n inspector \
  "env ARENA0_HOME='$demo_home' arena0 monitor"
inspector="$session:1.0"
for ((attempt = 0; attempt < 60; attempt++)); do
  capture_pane "$inspector" "$evidence/inspector-initial"
  if grep -Fq 'MONITOR' "$evidence/inspector-initial.txt"; then
    break
  fi
  sleep 1
done
if ! grep -Fq 'MONITOR' "$evidence/inspector-initial.txt"; then
  echo "$program: inspector did not attach to the isolated run" >&2
  exit 1
fi

for ((attempt = 0; attempt < 180; attempt++)); do
  capture_pane "$inspector" "$evidence/inspector-ready"
  if grep -Fq '2 executions' "$evidence/inspector-ready.txt" \
      && grep -Fq 'ACTIVE' "$evidence/inspector-ready.txt"; then
    break
  fi
  if [[ -s "$evidence/launcher.status" ]]; then
    break
  fi
  sleep 1
done
if ! grep -Fq '2 executions' "$evidence/inspector-ready.txt" \
    || ! grep -Fq 'ACTIVE' "$evidence/inspector-ready.txt"; then
  echo "$program: inspector did not observe both active executions" >&2
  exit 1
fi

# Visit and retain every inspector surface while the interaction is live.
view_markers=('▸ Program' 'Negotiation stage' 'GUEST VIEW' 'PUBLIC TRACE' 'Wasm handlers' 'SYSTEM EVENTS')
for view in 1 2 3 4 5 6; do
  tmux send-keys -t "$inspector" "$view"
  sleep 1
  capture_pane "$inspector" "$evidence/inspector-view-$view"
  if ! grep -Fq "${view_markers[$((view - 1))]}" "$evidence/inspector-view-$view.txt"; then
    echo "$program: inspector view $view did not render its expected surface" >&2
    exit 1
  fi
done

started_at=$SECONDS
next_report=0
while [[ ! -s "$evidence/launcher.status" ]]; do
  elapsed=$((SECONDS - started_at))
  if (( elapsed >= timeout_seconds )); then
    capture_pane "$launcher" "$evidence/agent-left-timeout"
    capture_pane "$right_pane" "$evidence/agent-right-timeout"
    capture_pane "$inspector" "$evidence/inspector-timeout"
    echo "$program: timed out after ${timeout_seconds}s" >&2
    exit 1
  fi
  if (( elapsed >= next_report )); then
    capture_pane "$launcher" "$evidence/agent-left-progress"
    capture_pane "$right_pane" "$evidence/agent-right-progress"
    capture_pane "$inspector" "$evidence/inspector-progress"
    printf '%s: participants still active after %ss\n' "$program" "$elapsed"
    next_report=$((elapsed + 30))
  fi
  sleep 2
done

capture_pane "$launcher" "$evidence/agent-left-final"
capture_pane "$right_pane" "$evidence/agent-right-final"
tmux send-keys -t "$inspector" Escape
sleep 1
capture_pane "$inspector" "$evidence/inspector-final"

status=$(<"$evidence/launcher.status")
if [[ "$status" != 0 ]]; then
  echo "$program: arena0 launch --agents exited with status $status" >&2
  exit 1
fi
if ! grep -Fq 'both Codex Participants finished' "$evidence/agent-left-final.txt"; then
  echo "$program: launcher did not report both participants finished" >&2
  exit 1
fi
for transcript in "$evidence/agent-left-final.txt" "$evidence/agent-right-final.txt"; do
  if ! grep -Fq 'arena0' "$transcript" \
      || ! grep -Fq 'exec view' "$transcript" \
      || ! grep -Fq 'receipt verify' "$transcript" \
      || ! grep -Fq '"tier": "Light"' "$transcript" \
      || ! grep -Fq '"Completed"' "$transcript"; then
    echo "$program: participant transcript lacks authored views, completion, or Light receipt verification" >&2
    exit 1
  fi
done
if ! grep -Fq '2 Hosts' "$evidence/inspector-final.txt" \
    || ! grep -Fq '2 executions' "$evidence/inspector-final.txt" \
    || ! grep -Fq 'COMPLETED' "$evidence/inspector-final.txt"; then
  echo "$program: inspector did not observe both completed executions" >&2
  exit 1
fi

common_session=$(comm -12 \
  <(grep -oE '"session_id": "[0-9a-f]{64}"' "$evidence/agent-left-final.txt" | sort -u) \
  <(grep -oE '"session_id": "[0-9a-f]{64}"' "$evidence/agent-right-final.txt" | sort -u) \
  | head -n 1)
common_receipt=$(comm -12 \
  <(grep -oE '"receipt_id": "[0-9a-f]{64}"' "$evidence/agent-left-final.txt" | sort -u) \
  <(grep -oE '"receipt_id": "[0-9a-f]{64}"' "$evidence/agent-right-final.txt" | sort -u) \
  | head -n 1)
if [[ -z "$common_session" || -z "$common_receipt" ]]; then
  echo "$program: participant transcripts did not report the same session and receipt" >&2
  exit 1
fi

printf '%s: published npm demo completed with two agents, inspector coverage, and verified Light receipts\n' "$program"
