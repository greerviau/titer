#!/bin/sh
# Installs the runners that the fixture tests use into .fixture-deps/.
# Requires uv and npm. Safe to rerun.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
deps="$root/.fixture-deps"
mkdir -p "$deps"

if [ ! -x "$deps/venv/bin/python" ]; then
  uv venv --quiet --python 3.12 "$deps/venv"
fi
uv pip install --quiet --python "$deps/venv/bin/python" pytest==9.2.0

mkdir -p "$deps/node"
if [ ! -f "$deps/node/package.json" ]; then
  echo '{"private": true}' > "$deps/node/package.json"
fi
npm install --silent --no-audit --no-fund --prefix "$deps/node" vitest@5.1.0 jsdom@31.0.1 >/dev/null

echo "fixture deps ready in $deps"
