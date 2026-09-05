#!/bin/sh
set -eu

root=$(git rev-parse --show-toplevel)
common=$(git rev-parse --path-format=absolute --git-common-dir)
active=$(git rev-parse --path-format=absolute --git-path hooks)
managed="$common/arena0-hooks"

# Keep the guard active in linked worktrees and when checking out old branches.
# Preserve existing hooks without modifying a global hooks directory.
mkdir -p "$managed"
if [ "$active" != "$managed" ]; then
    for hook in "$active"/*; do
        [ -f "$hook" ] || continue
        name=$(basename "$hook")
        case "$name" in
            *.sample) continue ;;
            pre-push) name=pre-push.previous ;;
        esac
        if [ -e "$managed/$name" ] || [ -L "$managed/$name" ]; then
            printf >&2 'Hook installation stopped: %s already exists.\n' "$managed/$name"
            exit 1
        fi
        ln -s "$hook" "$managed/$name"
    done
fi
cp "$root/.githooks/pre-push" "$managed/pre-push"
chmod +x "$managed/pre-push"
git config --local core.hooksPath "$managed"
printf 'Installed archive push protection in %s\n' "$managed"
