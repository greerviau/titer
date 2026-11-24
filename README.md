# titer

titer checks whether the tests in a change actually test the change. Run it on a branch, and it compares the branch with its base:

- `scan` finds weakened tests: deleted tests, removed assertions, added skips, loosened matchers, and changed snapshots.
- `prove` runs the changed tests against the base code. A test that fails on base detects the change.
- `mutate` makes small edits to the changed source lines and checks that the changed tests catch them.
- `crap` scores each changed function by complexity and coverage. It is informational.

titer is a CLI that coding agents and CI jobs call. It contains no model and never wraps the agent. It prints a report and exits with a pass or fail code, and the caller decides what to do.

titer gathers evidence. It cannot show that a change is correct: a test that fails on base detects a difference from base, and a reviewer still checks that the difference is the intended one.

titer supports Python, TypeScript, TSX, JavaScript, and Rust, with pytest, unittest, vitest, and cargo test. It runs on Linux and macOS.

## Install

Add titer as a dev dependency of your repo, so every agent and CI job runs the same version.

npm:

```sh
npm install --save-dev @greerviau/titer
```

Python with uv:

```sh
uv add --dev titer
```

Rust:

```sh
cargo install titer
```

Each [GitHub release](https://github.com/greerviau/titer/releases) also has binaries for Linux (x64, arm64) and macOS (x64, arm64).

## Set up a repo

Run `init` once from the repo root:

```sh
npx titer init       # npm
uv run titer init    # uv
```

`init` detects your test runners and writes these files:

- `titer.toml`, with one `[[runner]]` per project. A monorepo gets several. It excludes tool configs such as `vite.config.ts` from mutation and adds test helpers outside the default globs, such as vitest setup files and a `testing.ts` that only tests import, to `support_globs`. `init` also names any `scripts/` or `tools/` dir you may want to exclude, and the repo's test scripts, whose environment variables you may need to copy into `env`.
- A pinned dev dependency in `package.json` or `pyproject.toml`.
- A repo skill in `.agents/skills/titer/SKILL.md` and `.claude/skills/titer/SKILL.md`. It tells agents when to run titer, how to read the report, and never to weaken a test to make it pass.

Review the files and commit them. Run `init --dry-run` first to see the plan without writing anything.

titer does not run in a repo without `titer.toml`. If `init` finds no runner, write `titer.toml` by hand with the [configuration reference](docs/configuration.md).

After you upgrade titer, run `init --update` to refresh the skill and the pin. `check` warns when they are out of date.

## Run a check

With your changes committed or still in the working tree, run:

```sh
npx titer check      # npm
uv run titer check   # uv
```

titer compares the working tree, including untracked files, with the merge-base of HEAD and the base branch. By default the base is the remote default branch; pass `--base origin/develop` to pick another. titer runs tests in temporary worktrees and never changes your checkout.

The output starts with the gate result:

```text
titer gate: pass
```

The exit code tells the caller what happened:

| Code | Meaning |
|---|---|
| 0 | The gate passes. |
| 1 | The gate fails. By default, this means the change is unproven: no changed test fails on base, and the tests catch none of the mutants. |
| 2 | The check could not run. Common causes are a changed test that fails on head, a config error, and a git error. |

Agents should run `titer check --json` and read the JSON report. See the [CLI reference](docs/cli.md) for all options and the report format.

Common options:

- `--base REF` sets the base ref.
- `--only scan,prove` runs only some tiers.
- `--budget 60s` ends mutation 60 s after the check starts. titer stops any canary, mutant, or survivor follow-up run still going then and reports what did not finish as `not_run`. It does not cut short the head and base test runs, so a check can run longer.

## Run in CI

Add the GitHub Action to a workflow that runs on pull requests:

```yaml
on: pull_request

permissions:
  contents: read
  pull-requests: write # only for comment: true

jobs:
  titer:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
          ref: ${{ github.event.pull_request.head.sha }}
      # Install your repo's dependencies here.
      - uses: greerviau/titer@v0.1
        with:
          comment: true
```

The Action runs the titer version your repo pins, writes a job summary, annotates the diff, and fails the job when the gate fails. See the [GitHub Action guide](docs/github-action.md) for inputs, outputs, and how it reports.

## Documentation

- [How titer works](docs/how-it-works.md): the tiers, test verdicts, mutation operators, and the gate.
- [Configuration](docs/configuration.md): every `titer.toml` setting.
- [CLI reference](docs/cli.md): commands, options, exit codes, agent mutants, and the JSON report.
- [GitHub Action](docs/github-action.md): inputs, outputs, job summary, annotations, and PR comments.
- [Glossary](GLOSSARY.md): the terms titer uses and what they mean.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
