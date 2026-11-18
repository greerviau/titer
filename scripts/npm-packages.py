#!/usr/bin/env python3
"""Builds the npm packages for a release: one per platform plus the `titer` launcher.

Usage: scripts/npm-packages.py VERSION BINARIES_DIR OUT_DIR

BINARIES_DIR holds one binary per npm platform, named titer-<platform> (for example
titer-linux-x64). Platforms without a binary are left out of optionalDependencies.
"""
import json, shutil, sys
from pathlib import Path

PLATFORMS = {
    "linux-x64": ("linux", "x64"),
    "linux-arm64": ("linux", "arm64"),
    "darwin-arm64": ("darwin", "arm64"),
    "darwin-x64": ("darwin", "x64"),
}

root = Path(__file__).resolve().parent.parent
version, binaries, out = sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3])
out.mkdir(parents=True, exist_ok=True)
common = {
    "version": version,
    "license": "MIT OR Apache-2.0",
    "repository": {"type": "git", "url": "git+https://github.com/greerviau/titer.git"},
    "homepage": "https://github.com/greerviau/titer",
}
built = {}
for platform, (os_name, cpu) in PLATFORMS.items():
    binary = binaries / f"titer-{platform}"
    if not binary.exists():
        continue
    name = f"titer-{platform}"
    pkg = out / name
    (pkg / "bin").mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, pkg / "bin/titer")
    (pkg / "bin/titer").chmod(0o755)
    manifest = {"name": name, **common, "description": f"The titer binary for {platform}.",
                "os": [os_name], "cpu": [cpu], "files": ["bin/titer"]}
    (pkg / "package.json").write_text(json.dumps(manifest, indent=2) + "\n")
    built[name] = version

launcher = out / "titer"
shutil.copytree(root / "npm/titer", launcher, dirs_exist_ok=True)
for doc in ["README.md", "LICENSE-MIT", "LICENSE-APACHE"]:
    shutil.copy2(root / doc, launcher / doc)
manifest = {
    "name": "titer",
    **common,
    "description": "Prove that a change's tests test the change: fail-on-base, diff-scoped mutation, weakened-test scan, and CRAP.",
    "keywords": ["testing", "mutation-testing", "ci", "agents", "vitest", "pytest"],
    "bin": {"titer": "bin/titer.js"},
    "files": ["bin/titer.js", "README.md", "LICENSE-MIT", "LICENSE-APACHE"],
    "engines": {"node": ">=18"},
    "optionalDependencies": built,
}
(launcher / "package.json").write_text(json.dumps(manifest, indent=2) + "\n")
print("\n".join([*built, "titer"]))
