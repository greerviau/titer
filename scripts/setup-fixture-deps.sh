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

# A second environment with coverage.py, for the per-test coverage tests. The first stays
# without it, for the tests of behavior without coverage.
if [ ! -x "$deps/venv-cov/bin/python" ]; then
  uv venv --quiet --python 3.12 "$deps/venv-cov"
fi
uv pip install --quiet --python "$deps/venv-cov/bin/python" pytest==9.2.0 coverage==7.17.5

mkdir -p "$deps/node"
if [ ! -f "$deps/node/package.json" ]; then
  echo '{"private": true}' > "$deps/node/package.json"
fi
npm install --silent --no-audit --no-fund --prefix "$deps/node" vitest@5.1.0 jsdom@31.0.1 >/dev/null

mkdir -p "$deps/node-cov"
if [ ! -f "$deps/node-cov/package.json" ]; then
  echo '{"private": true}' > "$deps/node-cov/package.json"
fi
npm install --silent --no-audit --no-fund --prefix "$deps/node-cov" vitest@5.1.0 @vitest/coverage-v8@5.1.0 >/dev/null

echo "fixture deps ready in $deps"
