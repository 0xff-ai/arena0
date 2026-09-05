#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
check_root=$(mktemp -d)
trap 'rm -rf -- "$check_root"' EXIT
third_party="$check_root/third-party.txt"
programs="$check_root/programs.txt"

cd "$repo_root"
cargo about generate about.hbs --manifest-path Cargo.toml \
  --all-features --locked --offline --fail -o "$third_party"
cargo about generate about-programs.hbs --manifest-path programs/Cargo.toml \
  --all-features --locked --offline --fail -o "$programs"
perl -pi -e 's/\r$//; s/[ \t]+$//' "$third_party" "$programs"
perl -0pi -e 's/\n+\z/\n/' "$third_party" "$programs"
cmp THIRD_PARTY_LICENSES.txt "$third_party"
cmp EMBEDDED_PROGRAM_LICENSES.txt "$programs"
