#!/bin/sh
# Builds a fixture repo, runs this checkout's titer check in it, and keeps the output.
# Usage: check.sh proven|unproven [titer check args...]
set -eu
mode=$1; shift
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
titer="$root/target/debug/titer"
python="$root/.fixture-deps/venv/bin/python"
cache="${XDG_CACHE_HOME:-$HOME/.cache}"
mkdir -p "$cache"
evidence="$cache/titer-verify/$(date -u +%Y%m%dT%H%M%SZ)-$mode"
scratch=$(mktemp -d "$cache/titer-verify-run.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$evidence" "$scratch/tmp"
"$root/scripts/action-fixture.sh" "$scratch/repo" "$titer" "$mode" "$python" >/dev/null
cd "$scratch/repo"
code=0
TITER_TMPDIR="$scratch/tmp" "$titer" check --json --markdown "$evidence/summary.md" \
  --annotations "$evidence/annotations.txt" "$@" \
  > "$evidence/report.json" 2> "$evidence/stderr.txt" || code=$?
echo "$code" > "$evidence/exit-code.txt"
echo "$evidence"
