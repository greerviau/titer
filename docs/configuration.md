# Configuration

titer reads `titer.toml` from the repo root. `titer init` writes one for you. titer does not run without it: `check` exits 2 and tells the caller to run `titer init`.

## Example

```toml
base = "origin/main"            # optional default for --base

[[runner]]
name = "api"                    # unique; defaults to the kind
kind = "unittest"               # pytest, unittest, vitest, or cargo
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

## Top level

| Key | Default | Meaning |
|---|---|---|
| `base` | remote default branch | The default for `--base`. |

## runner

Add one `[[runner]]` table per test project. A monorepo lists several.

| Key | Default | Meaning |
|---|---|---|
| `name` | the kind | A unique name for the runner. |
| `kind` | required | `pytest`, `unittest`, `vitest`, or `cargo`. |
| `cwd` | `.` | The repo-relative directory the command runs in. Globs and paths in this table are relative to it. |
| `command` | required | The repo's own command prefix. titer appends the runner's selection arguments. |
| `start_dir` | none | unittest only: the top-level dir that dotted ids are relative to. |
| `test_globs` | per kind | Files that contain tests. |
| `support_globs` | per kind | Other test-only files, such as fixtures and helpers. titer copies them onto base with the tests and never mutates them. |
| `deps` | per kind | Ignored dependency dirs that titer links from the checkout into each worktree. |
| `env` | `{}` | Extra environment variables for the command. |

Defaults per kind:

| Kind | `test_globs` | `support_globs` | `deps` |
|---|---|---|---|
| pytest | `**/test_*.py`, `**/*_test.py` | `**/conftest.py`, `tests/**`, `test/**` | `.venv`, `venv` |
| unittest | `**/test*.py` | `**/conftest.py`, `tests/**`, `test/**` | `.venv`, `venv` |
| vitest | `**/*.{test,spec}.{ts,tsx,mts,cts,js,jsx,mjs,cjs}` | `**/__tests__/**`, `**/__mocks__/**`, `**/__snapshots__/**`, `**/__fixtures__/**`, `**/test-utils/**`, `**/test-setup.*`, `**/setupTests.*`, `**/vitest.setup.*`, `test/**`, `tests/**` | `node_modules` |
| cargo | `**/tests/**/*.rs`, `**/tests.rs` | `**/tests/**`, `**/testdata/**`, `**/snapshots/**` | none |

Python commands should not sync the environment. Use `uv run --no-sync`, because worktrees share the checkout's `.venv`.

A cargo runner's command is `cargo test` plus any flags the repo needs, such as `["cargo", "test", "--features", "postgres"]`. Put the runner's `cwd` at the workspace root. A cargo runner also finds the unit tests inside source files, so its `test_globs` cover only integration tests and `tests.rs` files. Each worktree builds into its own target dir, so titer ignores `CARGO_TARGET_DIR` unless `env` sets it.

## gate

By default the gate fails only on an unproven change. Each key below adds a failure condition.

| Key | Default | Fails the gate when |
|---|---|---|
| `fail_on_survivors` | `false` | Any mutant is `survived` or `not_executed`. |
| `fail_on_passes_on_base` | `false` | Any changed test passes on base. |
| `fail_on_scan` | `false` | `scan` finds a weakened test. |
| `fail_on_crap` | `false` | A changed function scores above `[crap] threshold`. |

## mutate

| Key | Default | Meaning |
|---|---|---|
| `budget` | `"120s"` | The time budget for a whole check. `--budget` overrides it. |
| `exclude_operators` | `[]` | Mutation operators to skip, for example `["remove_call"]`. |
| `exclude_globs` | `[]` | Repo-relative source files that titer never mutates. |
| `exclude_calls` | `[]` | Callee names whose calls titer never removes, in addition to logging calls. |

## crap

| Key | Default | Meaning |
|---|---|---|
| `coverage_command` | none | A command that writes a coverage file. It runs from the repo root. |
| `coverage_file` | none | The repo-relative file that `coverage_command` writes: lcov, Cobertura XML, or coverage.py JSON. |
| `threshold` | `30` | The CRAP score above which titer flags a function. |

`--coverage FILE` on the command line takes a coverage file directly instead.
