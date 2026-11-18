# How titer works

`titer check` answers one question about a diff: do the tests in this change test the change? It cannot show that the change is correct. A test that fails on base detects a difference from base, and a reviewer still checks that the difference is the intended one.

[Ubiquitous language](UBIQUITOUS-LANGUAGE.md) defines the terms on this page.

## Head, base, and worktrees

titer snapshots the working tree as head, including untracked files that are not ignored. It diffs head against base, the merge-base of HEAD and `--base`. `--base` defaults to `base` in `titer.toml`, then to the remote default branch.

Tests run in temporary worktrees under the user cache dir. Set `$TITER_TMPDIR` to use another disk-backed directory. Each worktree links the checkout's dependency dirs, such as `.venv` or `node_modules`.

titer runs tests through the repo's own command and the runner's native selection:

| Runner | Selection |
|---|---|
| pytest | node ids |
| unittest | dotted ids |
| vitest | `file:line` |

The checkout's status, index, stash, and worktree list never change. titer kills its own process groups on exit, on timeout, and on SIGINT.

## Tiers

| Tier | What it does |
|---|---|
| `scan` | Finds weakened tests: deleted tests, removed assertions, added skips, loosened matchers, and changed snapshots. |
| `prove` | Runs the changed tests against the base code and classifies each one. |
| `mutate` | Mutates changed source lines and runs the changed tests against each mutant. |
| `crap` | Scores each changed function by complexity and coverage. Informational. |

All tiers run by default. `--only` selects a subset, for example `--only scan,prove`.

### prove

`prove` runs the changed tests on head, then on base with head's test files copied in. Each changed test gets a verdict:

| Verdict | Meaning |
|---|---|
| `assertion` | The test fails on base with an assertion. The gate counts this as proof. |
| `exception` | The test fails on base with an exception from the code under test. The gate counts this as proof. |
| `missing_symbol` | The test fails on base only because a name exists only in head. It is not proof. |
| `passes_on_base` | The test passes on base. It can still be a useful regression guard. |
| `skipped` | The test skipped. |
| `no_result` | Zero tests executed. |
| `timeout` | The test timed out. |

Read the failure message of each proven test and check that it is the behavior the change targets. A test can also fail on base for an unrelated reason.

### mutate

`mutate` applies one mutant at a time to the changed source lines. The operators are:

- comparison, boolean, and arithmetic operator swaps
- bool flips and int +1
- removed `not` and `!`
- `return None` and `return undefined`
- removed calls (never logging calls)
- removed raise and throw
- negated `if`
- JSX attributes set to undefined

The head-green changed tests run against each mutant in parallel workers. If the change has no changed tests, the whole suite runs instead.

Before any mutant, a canary raises at the top of a changed module. The canary must be killed. If it survives, the worktree is not loading the code titer mutates, for example because of an editable install, and the check exits 2.

A mutant whose code also appears in the base lines its hunk replaces gets `unchanged_from_base: true`. The change may only have moved or reformatted that code.

Each survivor gets two follow-up runs:

1. titer inserts a sentinel raise before the line's statement. If no test fails, the outcome is `not_executed`: nothing runs the line.
2. titer reruns the whole changed test files. If another test kills the mutant, the outcome is `caught_by_other`.

Mutant outcomes are `killed`, `timed_out`, `survived`, `not_executed`, `caught_by_other`, `not_run` (the budget ran out first), and `no_result`.

Agents can add their own mutants with `--mutants`. See the [CLI reference](cli.md#agent-mutants).

### crap

`crap` scores each changed function:

```text
CRAP = complexity^2 * (1 - coverage)^3 + complexity
```

Coverage comes from `--coverage` (lcov, Cobertura XML, or coverage.py JSON) or from `[crap] coverage_command` in `titer.toml`. Functions that score above `[crap] threshold` (default 30) are flagged.

## The gate

By default the gate fails only on an unproven change. A change is unproven when all of these are true:

- It touches supported source lines.
- No changed test is proven.
- The selected tests kill none of its mutants.

Survivors, passes on base, scan findings, and CRAP scores fail the gate only when `[gate]` in `titer.toml` asks. See [configuration](configuration.md#gate).

The gate exits 2 when the check cannot run: a changed test fails on head, the canary survives, the config is invalid, or a git command fails.
