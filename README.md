# titer

Prove that a change's tests test the change: fail-on-base, diff-scoped mutation, weakened-test scan, and CRAP, as a CLI any agent can call.

`titer check` answers one question about the diff against a base ref: do the tests in this change prove the change? It prints a JSON report and exits with the gate's code.

| Tier | What it does |
|---|---|
| `scan` | Finds weakened tests: deleted tests, removed assertions, added skips, loosened matchers, changed snapshots. |
| `prove` | Runs the changed tests against the base code and classifies each one. |
| `mutate` | Mutates changed source lines and runs the changed tests against each mutant. |
| `crap` | Scores each changed function by complexity and coverage. Informational. |

titer supports Python and TypeScript/TSX sources and the pytest, unittest, and vitest runners. It is a single Rust binary. It never wraps the agent and contains no model.

## Status

Under construction. See the work items in the pull requests.

## Exit codes

- 0: the gate passes.
- 1: the gate fails.
- 2: the check could not run: head is not green, the canary survives, a config error, or a git error.

## Report

`titer check --json` prints a report that follows [`schema/report.schema.json`](schema/report.schema.json).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
