---
name: verify-titer
description: Use to prove a change to the titer CLI works by running `titer check` and `titer init` on fixture git repos, the way an agent or CI job calls them, and capturing the JSON report, Markdown summary, annotations, and exit code.
---

# verify-titer

titer is a CLI. A user runs it inside a git repo that has `titer.toml`. Verify a change by building this checkout's binary, building a fixture repo, running the command the change affects, and keeping its output as evidence.

## Launch

Build the binary and the fixture runners once per session:

```sh
cargo build
scripts/setup-fixture-deps.sh
```

Ready signal: `cargo build` prints `Finished`, and the setup script prints `fixture deps ready in .../.fixture-deps`. Each drive is a short-lived process, so there is no PID to record.

## Doctor

```sh
target/debug/titer --version
grep -m1 '^version' Cargo.toml
```

The two versions match. `.fixture-deps/venv/bin/python -m pytest --version` prints `pytest 9.2.0`.

## Drive

`scripts/check.sh MODE [titer check args...]` builds a fixture repo with `scripts/action-fixture.sh`, runs `titer check --json --markdown` in it, and writes the evidence. MODE is `proven` (a new test fails on base) or `unproven` (a new test never exercises the change). Extra arguments go to `titer check`.

```sh
.agents/skills/verify-titer/scripts/check.sh proven
.agents/skills/verify-titer/scripts/check.sh unproven --only prove
```

It prints the evidence directory. For a scenario the two modes do not cover, build a repo by hand the way `scripts/action-fixture.sh` does: `git init -b main`, write `titer.toml`, commit a base, change files on a branch, then run `target/debug/titer check --json --markdown FILE`.

A real repo also works, such as a clone of a project that already has `titer.toml`. Clone it outside this checkout and run `target/debug/titer check --base <base sha> --json --markdown FILE` from its root. titer never modifies the checkout it runs in.

## Evidence

`check.sh` writes to `${XDG_CACHE_HOME:-$HOME/.cache}/titer-verify/<UTC timestamp>-<mode>/`:

- `report.json`: the JSON report from stdout.
- `summary.md`: the `--markdown` summary, the same text the Action posts as a PR comment.
- `stderr.txt` and `exit-code.txt`.

## Cleanup

`check.sh` removes its fixture repo on exit. titer removes its own worktrees under `$TITER_TMPDIR`, which `check.sh` sets to a directory it also removes. Keep the evidence directory.

## Features

See [features/README.md](features/README.md).
