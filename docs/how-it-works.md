# How titer works

`titer check` answers one question about a diff: do the tests in this change test the change? It cannot show that the change is correct. A test that fails on base detects a difference from base, and a reviewer still checks that the difference is the intended one.

The [glossary](../GLOSSARY.md) defines the terms on this page.

## Head, base, and worktrees

titer snapshots the working tree as head, including untracked files that are not ignored. It diffs head against base, the merge-base of HEAD and `--base`. `--base` defaults to `base` in `titer.toml`, then to the remote default branch.

Tests run in temporary worktrees under the user cache dir. Set `$TITER_TMPDIR` to use another disk-backed directory. Each worktree links the checkout's dependency dirs, such as `.venv` or `node_modules`. So base runs with head's dependencies. When the change edits `uv.lock`, `poetry.lock`, `requirements*.txt`, `package-lock.json`, `pnpm-lock.yaml`, or `yarn.lock` under a runner that links dependency dirs, or a JS lockfile at the repo root when the checkout has a root `node_modules`, prove adds a warning to the report. Cargo runners link nothing, so base builds from its own `Cargo.lock`.

titer runs tests through the repo's own command and the runner's native selection:

| Runner | Selection |
|---|---|
| pytest | node ids |
| unittest | dotted ids |
| vitest | `file:line` |
| cargo | the package and target (`-p`, `--lib`, `--bin`, or `--test`) and exact libtest names |

The checkout's status, index, stash, and worktree list never change. titer kills its own process groups on exit, on timeout, and on SIGINT.

## Changed tests

A changed test is a test whose span holds a changed line. Table-driven tests often get a new case as data outside any test, such as a row in a `tests` array that a `for` loop runs, a list passed to `@pytest.mark.parametrize`, or a `const CASES` slice in a Rust test module. So a test also counts as changed when it reads a declaration in the same file that holds a changed line outside every test span:

- TypeScript and JavaScript: `const`, `let`, and `var` declarations, functions, classes, and enums.
- Python: assignments, functions (including fixtures), and classes.
- Rust: `const`, `static`, functions, types, `impl` blocks of a type, traits, and `macro_rules!`. In a source file, only declarations in test code count.

A test reads a declaration when its own span names it, including decorators such as `parametrize`, or when an enclosing construct names it outside the statements it holds, such as the iterable of a `for` loop, the receiver of `cases.forEach`, or the table of `describe.each`. Python tests also read `self.name` and `cls.name`, and a fixture by its parameter name. A declaration that reads a changed declaration, such as a table that spreads another, also counts as changed. The declaration must be visible where the test is, so a table in one `describe` callback does not change the tests in another. Imports are not test data, and titer does not resolve shadowing: a name that an inner block binds again still counts.

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
| `missing_symbol` | The test fails on base only because a name exists only in head, or, in Rust, because it does not compile against base. It is not proof. |
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
- removed raise and throw, and in Rust removed `panic!` and `return Err(...)`
- negated `if`
- JSX attributes set to undefined

The head-green changed tests run against each mutant in parallel workers. If the change has no changed tests, the whole suite runs instead.

Before any mutant, a canary raises at the top of a changed module. Just before it raises, it writes a marker file. The canary must be killed, or leave its marker: a test that imports the module inside `try` and falls back on failure swallows the raise, but the marker shows the module loaded. A canary that survives without a marker shows that the selected tests never load that module, so its mutants are `not_executed`. When that holds for every module, the worktree is not loading the code titer mutates, for example because of an editable install, and the check exits 2. Code that runs outside Node, such as vitest browser mode, cannot write the marker. A Rust module runs no code when it loads, so its canary is a `compile_error!` instead, and a build that fails counts as killed.

A mutant whose code also appears in the base lines its hunk replaces gets `unchanged_from_base: true`. The change may only have moved or reformatted that code.

Each survivor gets two follow-up runs:

1. titer inserts a sentinel raise before the line's statement. On a line of an `elif` or `else if` condition that shares its lines with no other code, the sentinel goes inside the condition instead, so it raises only when a test evaluates that condition. A condition line that cannot take the probe gets no sentinel, such as a Rust `else if let` or a line that also holds an earlier branch, and neither does a line that starts with `}`, since a sentinel there would land in the previous arm. In Rust the sentinel is `std::process::abort()`, which `#[should_panic]` and `catch_unwind` cannot catch. If no test fails, the outcome is `not_executed`: nothing runs the line.
2. titer reruns the whole changed test files. If another test kills the mutant, the outcome is `caught_by_other`.

Mutant outcomes are `killed`, `timed_out`, `survived`, `not_executed`, `caught_by_other`, `not_run` (the budget ran out first), and `no_result`.

A mutant survives only when every selected test ran and passed. A mutant that leaves any selected target unbuildable or unlistable, or a selected test missing, gets `no_result`, unless a selected test fails. A compile or link error is never a kill.

Agents can add their own mutants with `--mutants`. See the [CLI reference](cli.md#agent-mutants).

### crap

`crap` scores each changed function:

```text
CRAP = complexity^2 * (1 - coverage)^3 + complexity
```

Coverage comes from `--coverage` (lcov, Cobertura XML, or coverage.py JSON) or from `[crap] coverage_command` and the `coverage_file` it writes in `titer.toml`. Without a coverage source, `crap` reports complexity only. Functions that score above `[crap] threshold` (default 30) are flagged.

## Rust

Rust keeps unit tests in the source file they test. titer reads `#[cfg(test)]` items and test functions (`#[test]`, `#[tokio::test]`, `#[rstest]`, and the like) as test code:

- A changed test inside a source file is a changed test. `#[cfg_attr(condition, test)]` also marks a test.
- Test code is never mutated, and a change that edits only test code does not touch source. Code that shares a line with test code is still source.
- `prove` keeps base's source code and replaces its test items with head's. It rebuilds each module in head's item order: items both sides have keep base's text, items only base has stay, test items come from head, and other items only head has are left out. So every test sees the `macro_rules!` definitions it sees in head. A test module inside a module that base lacks goes into an empty module of that name. A file that cargo compiles only as test code, such as one under `#[cfg(test)] mod unit;` or `#![cfg(test)]`, goes onto base whole.

titer asks `cargo metadata` for packages and targets, and maps each file to the targets that compile it by following `mod` declarations, including `#[path]`, from each target's root. A module that several targets declare, such as `tests/common/mod.rs`, belongs to each of them. It lists each target's tests and selects exact names; a test that a macro such as `rstest` expands into cases selects every case.

titer skips a target whose `required-features` the runner's command does not enable, as cargo does. Two files that can fill one module path, such as `#[cfg]` variants under `#[path]`, give their tests the same libtest name, so titer selects neither and they get `no_result`. titer reads a test binary's results only when they agree with its own `test result:` counts; a test that prints result lines of its own leaves the run without a result.

A test that uses a name only head has stops its whole target from compiling on base. `prove` gives such a test the `missing_symbol` verdict, removes it and the import of that exact path from base, and builds again, so the other changed tests in that target still run.

Worktrees hold the tree without `.git`, so a build script that runs git fails there, and the check exits 2.

Doc tests run with the suite, but a changed doc test is not a changed test: libtest names a doc test by its line, which differs between base and head.

Each worktree builds its own target dir, and the first build in each one is cold. The runner's timeouts and durations count only the test binaries, so a slow cold build does not stretch mutant timeouts. Builds are also outside the budget: on a large workspace, the head, base, and canary builds can take minutes each, and a check can run that long past `--budget`. `--workers` limits how many cold builds run at once.

## The gate

By default the gate fails only on an unproven change. A change is unproven when all of these are true:

- It touches supported source lines.
- No changed test is proven.
- The selected tests kill none of its mutants.

Survivors, passes on base, scan findings, and CRAP scores fail the gate only when `[gate]` in `titer.toml` asks. See [configuration](configuration.md#gate).

The gate exits 2 when the check cannot run: a changed test fails on head, the canary survives, the config is invalid, or a git command fails.
