#!/usr/bin/env bash
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

declare -a patterns=(
  "peas-emulator"
  "peas_emulator"
  "PeasEmulator"
  "PEAS_EMULATOR"
  "https://github.com/sqrzl/peas-emulator"
)

found_match=0

for pattern in "${patterns[@]}"; do
  if git grep -n -I -F "$pattern" -- .; then
    found_match=1
  fi
done

if [[ "$found_match" -ne 0 ]]; then
  printf '%s\n' "Legacy Peas references remain in tracked files."
  exit 1
fi

printf '%s\n' "No legacy Peas references found in tracked files."
