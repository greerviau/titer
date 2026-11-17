#!/bin/sh
# Builds a fixture repo for the Action self-test: a base commit on main and a change on
# branch "change". MODE=proven adds a test that fails on base; MODE=unproven adds one that
# never exercises the change.
# Usage: scripts/action-fixture.sh DIR TITER_BIN MODE PYTHON
set -eu
dir=$1; titer=$2; mode=$3; python=$4
rm -rf "$dir"; mkdir -p "$dir"; cd "$dir"
git init -q -b main
git config user.name fixture; git config user.email fixture@example.com
cat > titer.toml <<TOML
[[runner]]
kind = "pytest"
command = ["$python", "-m", "pytest"]
TOML
printf 'def clamp(x, hi):\n    return x\n' > app.py
printf 'from app import clamp\n\n\ndef test_low():\n    assert clamp(1, 3) == 1\n' > test_old.py
git add -A; git commit -qm base
git checkout -q -b change
printf 'def clamp(x, hi):\n    if x > hi:\n        return hi\n    return x\n' > app.py
if [ "$mode" = proven ]; then
  printf 'from app import clamp\n\n\ndef test_high():\n    assert clamp(5, 3) == 3\n' > test_new.py
else
  printf 'import app\n\n\ndef test_exists():\n    assert callable(app.clamp)\n' > test_new.py
fi
git add -A; git commit -qm change
"$titer" --version
