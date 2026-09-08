#!/usr/bin/env bash
# Select existing verification commands from changed paths; unknown owners run all gates.
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/check-affected.sh [--plan | --scope] [BASE [HEAD]]

Check changes from BASE (default HEAD), including local untracked files.
With HEAD, check only the committed BASE..HEAD range.

  --plan   Print the selected commands without running checks.
  --scope  Print docs, cli, daemon, programs, or full for CI.
  --help   Show this help.

Shared, mixed code owners, unknown, empty, or unavailable changes select the full gate.
Documentation can accompany one code owner without expanding its scope.
EOF
}

mode=run
case ${1:-} in
    --plan) mode=plan; shift ;;
    --scope) mode=scope; shift ;;
    --help|-h) usage; exit 0 ;;
esac
if [[ $# -gt 2 || ${1:-} == -* || ${2:-} == -* ]]; then
    usage >&2
    exit 2
fi
base=${1:-HEAD}
head=${2:-}
cd "$(git rev-parse --show-toplevel)"

classify_paths() {
    # Prose does not expand a known code owner; every selected route checks whitespace.
    local scope=docs owner path seen=false
    while IFS= read -r -d '' path; do
        seen=true
        case "$path" in
            Cargo.toml|Cargo.lock|*/Cargo.toml|*/Cargo.lock|*/build.rs)
                owner=full ;;
            README.md|SECURITY.md|LICENSE|LICENSE-*|docs/*.md|docs/*.svg|docs/*.png|docs/*.jpg|docs/*.jpeg|docs/*.gif)
                owner=docs ;;
            crates/arena0-cli/*) owner=cli ;;
            crates/arena0-daemon/*|crates/arena0d/*) owner=daemon ;;
            programs/*) owner=programs ;;
            *) owner=full ;;
        esac
        if [[ $owner == full || ($scope != docs && $owner != docs && $scope != "$owner") ]]; then
            scope=full
        elif [[ $scope == docs ]]; then
            scope=$owner
        fi
    done
    if [[ $seen == false ]]; then
        scope=full
    fi
    printf '%s\n' "$scope"
}

resolved=true
if ! base_oid=$(git rev-parse --verify --end-of-options "${base}^{commit}" 2>/dev/null); then
    printf 'Cannot resolve base %q; selecting the full gate.\n' "$base" >&2
    resolved=false
fi
diff_args=()
if [[ $resolved == true ]]; then
    diff_args=("$base_oid")
    if [[ -n $head ]]; then
        if head_oid=$(git rev-parse --verify --end-of-options "${head}^{commit}" 2>/dev/null); then
            diff_args+=("$head_oid")
        else
            printf 'Cannot resolve head %q; selecting the full gate.\n' "$head" >&2
            resolved=false
        fi
    fi
fi

changed_paths() {
    # Both sides of a rename matter: moving code into docs is still a code change.
    git diff --name-only --no-renames -z "${diff_args[@]}" -- || return
    if [[ -z $head ]]; then
        git ls-files --others --exclude-standard -z || return
    fi
}

scope=full
if [[ $resolved == true ]]; then
    if ! scope=$(changed_paths | classify_paths); then
        printf 'Cannot enumerate changed paths; selecting the full gate.\n' >&2
        scope=full
        resolved=false
    fi
fi
if [[ $mode == scope ]]; then
    printf '%s\n' "$scope"
    exit 0
fi

commands=()
case $scope in
    cli) commands=(just check-cli test-cli) ;;
    daemon) commands=(just check-daemon test-daemon) ;;
    programs) commands=(just check-programs test-programs) ;;
    full) commands=(just build-programs check test doc) ;;
esac

check_untracked() {
    git ls-files --others --exclude-standard -z | (
        result=0
        while IFS= read -r -d '' path; do
            if [[ $mode == plan ]]; then
                printf 'git diff --no-index --check -- /dev/null %q; test "$?" -le 1\n' "$path"
            elif git diff --no-index --check -- /dev/null "$path"; then
                :
            else
                status=$?
                # --no-index returns 1 for an ordinary added file, 3 for whitespace errors.
                if [[ $status -gt 1 ]]; then
                    result=$status
                fi
            fi
        done
        exit "$result"
    )
}

if [[ $mode == plan ]]; then
    if [[ $resolved == true ]]; then
        printf 'git diff --check'
        printf ' %q' "${diff_args[@]}"
        printf ' --\n'
    fi
    if [[ -z $head ]]; then
        check_untracked
    fi
    if [[ ${#commands[@]} -gt 0 ]]; then
        printf '%s' "${commands[0]}"
        printf ' %q' "${commands[@]:1}"
        printf '\n'
    fi
    exit 0
fi

printf 'Selected checks: %s\n' "$scope" >&2
if [[ $resolved == true ]]; then
    git diff --check "${diff_args[@]}" --
fi
if [[ -z $head ]]; then
    check_untracked
fi
if [[ ${#commands[@]} -gt 0 ]]; then
    exec "${commands[@]}"
fi
