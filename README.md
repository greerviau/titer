# titer

Check whether a change's tests test the change: fail-on-base, diff-scoped mutation, weakened-test scan, and CRAP, as a CLI any agent can call.

`titer check` gathers evidence on one question about the diff against a base ref: do the tests in this change test the change? It cannot show that the change is correct. A test that fails on base detects a difference from base, and a reviewer still checks that the difference is the intended one. titer prints a JSON report and exits with the gate's code. It never wraps the agent and contains no model. Agents call it, read the JSON, and decide what to do.

| Tier | What it does |
|---|---|
| `scan` | Finds weakened tests: deleted tests, removed assertions, added skips, loosened matchers, and changed snapshots. |
| `prove` | Runs the changed tests against the base code and classifies each one. |
| `mutate` | Mutates changed source lines and runs the changed tests against each mutant. |
| `crap` | Scores each changed function by complexity and coverage. Informational. |

titer parses Python and TypeScript/TSX (and JavaScript) with tree-sitter, and runs pytest, unittest, and vitest. It is one Rust binary for Linux and macOS.

## Install

Install titer as a dev dependency of the repo, so every agent and CI job runs the version the repo pins:

| Ecosystem | Install | Run |
|---|---|---|
| npm | `npm install --save-dev @greerviau/titer` | `npx titer check` |
| Python with uv | `uv add --dev titer` | `uv run titer check` |
| Rust | `cargo install titer` | `titer check` |

Each [GitHub release](https://github.com/greerviau/titer/releases) also has binaries for Linux x64 and arm64 and macOS arm64 and x64.

## Opt in

```sh
npx @greerviau/titer init   # or: uvx titer init
```

`init` detects the repo's runners and writes four things for you to review and commit:

- `titer.toml`, with one `[[runner]]` per project (a monorepo gets several)
- a pinned dev dependency: `@greerviau/titer` in `package.json`, or `titer` in `pyproject.toml`
- the repo skill in `.agents/skills/titer/SKILL.md` (Codex, Cursor, OpenCode) and `.claude/skills/titer/SKILL.md` (Claude Code)

The skill tells agents when to run `check`, how to read the report, how to triage survivors, and never to weaken a test to pass. `init --dry-run` prints the plan without writing. `init --update` rewrites the skill and the pin after an upgrade; `check` warns when they are out of date.

titer never runs in a repo without `titer.toml`: `check` exits 2 and tells the caller to run `titer init`.

## Check

```sh
titer check [--base REF] [--only scan,prove,mutate,crap] [--json]
            [--budget 120s] [--workers N] [--mutants FILE] [--coverage FILE] [--markdown FILE]
```

titer snapshots the working tree, including untracked files that are not ignored, and diffs it against the merge-base of HEAD and `--base`. `--base` defaults to `base` in `titer.toml`, then to the remote default branch. Tests run in temporary worktrees under the user cache dir (`$TITER_TMPDIR` overrides it), through the repo's own command and the runner's native selection: pytest node ids, unittest dotted ids, and vitest `file:line`. The checkout's status, index, stash, and worktree list never change, and titer kills its own process groups on exit, timeout, and SIGINT.

- `prove` runs the changed tests on head, then on base with head's test files copied in. Each changed test gets a verdict: `assertion` or `exception` (the test fails on base, which the gate counts as proof), `missing_symbol` (it fails on base only because a name exists only in head), `passes_on_base`, `skipped`, `no_result`, or `timeout`.
- `mutate` applies one mutant at a time to changed source lines: comparison, boolean, and arithmetic swaps, bool flips, int +1, removed `not`/`!`, `return None`/`undefined`, removed calls (never logging calls), removed raise/throw, negated `if`, and JSX attributes set to undefined. The head-green changed tests run against each mutant in parallel workers; with no changed tests, the suite runs instead. A canary first checks that the worktree loads the mutated code. A mutant whose code also appears in the base lines its hunk replaces gets `unchanged_from_base: true`, because the change may only have moved or reformatted that code. Survivors get two follow-ups: a sentinel raise before the line's statement (`not_executed` when nothing runs it), and a rerun of the whole changed test files (`caught_by_other`).
- `--mutants FILE` adds agent-written mutants, one JSON object per line: `{"file": "app.py", "line": 3, "end_line": 3, "replacement": "    return x", "id": "optional"}`.
- `crap` scores changed functions with `CRAP = comp^2 * (1 - cov)^3 + comp`, from `--coverage` (lcov, Cobertura XML, or coverage.py JSON) or `[crap] coverage_command`.

## Exit codes and the gate

- 0: the gate passes.
- 1: the gate fails.
- 2: the check could not run: a changed test fails on head, the canary survives, a config error, or a git error.

By default the gate fails only on an unproven change: the change touches supported source lines, no changed test is proven, and the selected tests kill none of its mutants. Survivors, passes on base, scan findings, and CRAP fail the gate only when `[gate]` asks.

## Configuration

```toml
base = "origin/main"            # optional default for --base

[[runner]]
name = "api"                    # unique; defaults to the kind
kind = "unittest"               # pytest, unittest, or vitest
cwd = "services/api"            # repo-relative; globs and paths below are relative to it
command = ["uv", "run", "--no-sync", "python", "-m", "unittest"]
start_dir = "tests"             # unittest only: the dir dotted ids are relative to
test_globs = ["**/test*.py"]    # files that contain tests (defaults per kind)
support_globs = ["tests/**"]    # other test-only files: copied to base, never mutated
deps = [".venv"]                # dependency dirs linked into worktrees (defaults per kind)
env = { PYTHON_DOTENV_DISABLED = "1" }

[[runner]]
name = "web"
kind = "vitest"
cwd = "clients/web"
command = ["npx", "--no-install", "vitest", "run"]

[gate]
fail_on_survivors = false
fail_on_passes_on_base = false
fail_on_scan = false
fail_on_crap = false

[mutate]
budget = "120s"                 # time budget for a whole check
exclude_operators = []          # for example ["remove_call"]
exclude_globs = []              # repo-relative source files never mutated
exclude_calls = []              # callees never removed, beyond logging calls

[crap]
coverage_command = ["uv", "run", "--no-sync", "coverage", "json", "-o", "coverage.json"]
coverage_file = "coverage.json" # written by coverage_command, relative to the repo root
threshold = 30
```

Python commands should not sync the environment (`uv run --no-sync`), because worktrees share the checkout's `.venv`.

## GitHub Action

```yaml
permissions:
  contents: read
  pull-requests: write # only for comment: true

# One run per pull request at a time, so two runs never post at once.
concurrency:
  group: titer-${{ github.event.pull_request.number }}
  cancel-in-progress: true

steps:
  - uses: actions/checkout@v4
    with:
      fetch-depth: 0
      # The pull request head, so annotations land on the right diff lines.
      ref: ${{ github.event.pull_request.head.sha }}
  # Install the repo's dependencies here.
  - uses: greerviau/titer@v0.1
    with:
      comment: true
```

The Action runs the repo's pinned titer (`npx --no-install titer` or `uv run --no-sync titer`) against the pull request's base branch and fails on exit 1 or 2. It reports in three places:

- The job summary leads with the gate result (passed, failed, or could not run) and the evidence it rests on. It then lists survivors, lines no test runs, and weakened tests, with links to the pull request's head commit. A survivor whose code is unchanged from base says so. The full test and mutant tables are collapsed. The test table shows each changed test's failure on base, so a reviewer can check that the failure is the behavior the change targets.
- With `annotations: true` (the default), survivors and weakened tests are warnings on their lines in the diff, and lines no test runs are notices.
- With `comment: true`, the same summary is posted as a pull request comment whose heading names the commit checked. Each new head commit gets a new comment, and earlier titer comments collapse as outdated, so the newest result sits next to the newest push. A rerun on the same commit updates that commit's comment. This needs `pull-requests: write`. On pull requests from forks of a public repo, GitHub gives the token read access only, so the Action warns and skips the comment.

Annotations need the pull request head checked out, as above. The default `pull_request` checkout is a merge commit whose line numbers can differ, so the Action skips annotations there.

Other inputs: `base`, `args`, `working-directory`, `command`, and `github-token`. Outputs: `exit-code`, `report` (the JSON report's path), and `summary` (the Markdown's path).

## Report

`titer check --json` prints a report that follows [`schema/report.schema.json`](schema/report.schema.json): `gate`, `tests`, `mutants`, `findings`, `crap`, `warnings`, `not_analyzed`, and `timing`.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
