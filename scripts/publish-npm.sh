#!/usr/bin/env bash
set -euo pipefail
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# Only --publish permits registry writes; the default is a dry run of the
# same checksummed tarballs already exercised by the package smoke test.
exec node "$repo_root/npm/release.mjs" publish "${1:?usage: publish-npm.sh PACKED_DIRECTORY [--dry-run|--publish]}" "${2:---dry-run}"
