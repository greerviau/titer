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

- TypeScript and JavaScript: each declarator of `const`, `let`, and `var`, and functions, classes, and enums.
- Python: assignments, including `self.name` and `cls.name` in a method such as `setUp`, and functions (including fixtures) and classes.
- Rust: `const`, `static`, functions, types, traits, and `macro_rules!`, each method of an `impl`, and the rest of an `impl`, which stands for its type. In a source file, only declarations in test code count, and a changed line that test code shares with other code does not.

A test reads a declaration when its own span names it, including decorators such as `parametrize`, or when an enclosing construct names it beside the test, such as the iterable of a `for` loop, the receiver of `cases.forEach`, or the table of `describe.each`. Other statements and branches around the test do not count, so a table that only an `if` branch reads does not change a test in the `else` branch. Python tests also read `self.name` and `cls.name`, and a fixture by its parameter name; a parameter reads nothing else. A declaration that reads a changed declaration, such as a table that spreads another, also counts as changed. The declaration must be visible where the test is, so a table in one `describe` callback does not change the tests in another. Imports are not test data. titer matches names by text within that scope, so it does not resolve shadowing or Rust paths through another module.

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

With per-test coverage, each mutant runs first against only the changed tests that cover its line, and a survivor on a line that no changed test runs is `not_executed` without a sentinel. titer gathers the coverage itself, with the repo's own test command:

- pytest and unittest: when coverage.py 7.13 or later is installed in the environment of the interpreter the runner's command names (or, for `uv run`, its `.venv`), the head-green changed tests run once more with `COVERAGE_PROCESS_START` set, so coverage.py starts in the test process, its subprocesses, and its forked children, and records which test function ran each line. The uninstrumented head run still decides which tests are green. Without coverage.py, titer sets nothing and skips the run. If a changed test, or any of its parameters, has another result under coverage, for example one that checks `sys.gettrace()`, titer warns and mutants run every changed test.
- vitest: when `@vitest/coverage-v8` or `@vitest/coverage-istanbul` is installed, the head-green changed tests run once more together with `--coverage.enabled`, which shows the code they never run, and each test runs alone, which shows the tests that run each statement and each arm of a ternary or `&&`/`||`/`??` operand. The runs spread across the workers and stop at the budget.
- cargo: no per-test coverage. Every mutant runs every changed test.

Narrowing assumes that a changed test behaves the same alone as after the other changed tests, as any per-test selection does. Coverage only narrows a selection when it is sure, and a narrowed run decides only a kill:

- A line that ran outside any changed test, such as at import or in a fixture or `setUp`, runs every changed test. So does a vitest statement that the tests reach together but none reaches alone.
- A line the data does not measure, such as the continuation of a multi-line statement, a multi-line vitest mutant, and an agent mutant run every changed test, as without coverage.
- A mutant on a line no changed test runs still runs every changed test, since a process coverage missed, or an edit that changes how unexecuted code compiles, such as removing a `yield`, can still kill it. Its survivors are `not_executed`. Where no sentinel could probe the line, they first rerun against the whole changed test files, as without coverage.
- Each distinct narrowed selection first runs on unmutated head. A selection that is not green there, for example because a test needs state that a left-out test module sets, is dropped, and its mutants run every changed test.
- A narrowed run that kills nothing or times out runs again with every changed test, and that run decides the outcome. If the budget runs out first, the mutant is `not_run`.
- If coverage.py's data has no record of a changed test that passed, titer does not trust it, warns, and runs every changed test. A vitest report without statement counters is not used either.

These extra runs count against the budget. The `crap` tier's coverage sums every test, so it cannot say which test ran a line, and mutate does not use it.

Before any mutant, a canary raises at the top of a changed module. Just before it raises, it writes a marker file. The canary must be killed, or leave its marker: a test that imports the module inside `try` and falls back on failure swallows the raise, but the marker shows the module loaded. A canary that survives without a marker shows that the selected tests never load that module, so its mutants are `not_executed`. When that holds for every module, the worktree is not loading the code titer mutates, for example because of an editable install, and the check exits 2. Code that runs outside Node, such as vitest browser mode, cannot write the marker. A Rust module runs no code when it loads, so its canary is a `compile_error!` instead, and a build that fails counts as killed.

A mutant whose code also appears in the base lines its hunk replaces gets `unchanged_from_base: true`. The change may only have moved or reformatted that code.

Each survivor gets two follow-up runs:

1. Unless coverage showed whether the changed tests run the line, titer inserts a sentinel raise before the line's statement. On a line of an `elif` or `else if` condition that shares its lines with no other code, the sentinel goes inside the condition instead, so it raises only when a test evaluates that condition. A condition line that cannot take the probe gets no sentinel, such as a Rust `else if let` or a line that also holds an earlier branch, and neither does a line that starts with `}`, since a sentinel there would land in the previous arm. In Rust the sentinel is `std::process::abort()`, which `#[should_panic]` and `catch_unwind` cannot catch. If no test fails, the outcome is `not_executed`: nothing runs the line.
2. titer reruns the whole changed test files. If another test kills the mutant, the outcome is `caught_by_other`.

Mutant outcomes are `killed`, `timed_out`, `survived`, `not_executed`, `caught_by_other`, `not_run` (the budget ran out first), and `no_result`.

The budget is a hard deadline for mutation. titer starts a canary, mutant, or follow-up run only when the head run's duration still fits before the deadline, and kills the process group of any run still going at the deadline. A mutant whose run the deadline stops is `not_run`, never killed, timed out, or survived. So is a mutant whose canary the deadline stops, and a survivor whose follow-up runs it stops. A mutant that reaches its own timeout first is `timed_out`. A mutant's own timeout, 3x the head run plus 10 s, covers every attempt of its run: a runner that retries without a missing test does not restart it.

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

A test that uses a name only head has stops its whole target from compiling on base. `prove` gives such a test the `missing_symbol` verdict, removes it and the import of that exact path from base, and builds again, so the other changed tests in that target still run. Test data that uses such a name, such as a table row, gets the same treatment: the tests that read it get `missing_symbol`, and the data goes with them.

Worktrees hold the tree without `.git`, so a build script that runs git fails there, and the check exits 2.

Doc tests run with the suite, but a changed doc test is not a changed test: libtest names a doc test by its line, which differs between base and head.

Each worktree builds its own target dir, and the first build in each one is cold. The runner's timeouts and durations count only the test binaries, so a slow cold build does not stretch mutant timeouts. The head and base builds are also outside the budget: on a large workspace, they can take minutes each, and a check can run that long past `--budget`. Canary and mutant builds stop at the deadline, so a slow cold build can leave every mutant `not_run`. `--workers` limits how many cold builds run at once.

## The gate

By default the gate fails only on an unproven change. A change is unproven when all of these are true:

- It touches supported source lines.
- No changed test is proven.
- The selected tests kill none of its mutants.

Survivors, passes on base, scan findings, and CRAP scores fail the gate only when `[gate]` in `titer.toml` asks. See [configuration](configuration.md#gate).

The gate exits 2 when the check cannot run: a changed test fails on head, the canary survives, the config is invalid, or a git command fails.
