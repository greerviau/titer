#!/usr/bin/env node
// Runs the titer binary from the platform package that npm installed as an optional
// dependency, as esbuild and biome do.
"use strict";

const { spawnSync } = require("node:child_process");

const platforms = {
  "linux-x64": "titer-linux-x64",
  "linux-arm64": "titer-linux-arm64",
  "darwin-arm64": "titer-darwin-arm64",
  "darwin-x64": "titer-darwin-x64",
};

const key = `${process.platform}-${process.arch}`;
const pkg = platforms[key];
if (!pkg) {
  console.error(`titer: no prebuilt binary for ${key}; install with cargo install titer`);
  process.exit(2);
}

let binary;
try {
  binary = require.resolve(`${pkg}/bin/titer`);
} catch {
  console.error(
    `titer: the platform package ${pkg} is missing. Reinstall without --no-optional or --omit=optional.`,
  );
  process.exit(2);
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`titer: cannot run ${binary}: ${result.error.message}`);
  process.exit(2);
}
if (result.signal) {
  process.kill(process.pid, result.signal);
}
process.exit(result.status ?? 2);
