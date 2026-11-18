# Ubiquitous language

Code, docs, the report, and the repo skill use these terms with these meanings.

- Base: the merge-base of the head commit and `--base`. `--base` defaults to `base` in `titer.toml`, then to the remote default branch.
- Head: a snapshot of the working tree, including untracked files that are not ignored. It is never just HEAD.
- Changed line: a line added or modified in head relative to base.
- Test file: a file that matches a runner's `test_globs`.
- Test path: a test file or a file that matches a runner's `support_globs`. titer copies changed test paths onto base and never mutates them.
- Source file: a file in a supported language that is not a test path.
- Changed test: a test function or `it`/`test` call whose span intersects a changed line in a test file.
- Head-green: every selected test passes on head. Skipped tests do not count as passing.
- Proven test: a changed test that fails on base with an assertion, or with an exception from the code under test. It shows that the test detects a difference from base, not that the difference is the intended one.
- Missing symbol: a failure on base caused by a name that exists only in head. Such a test is not proven.
- Verdict: the prove tier's classification of a changed test: assertion, exception, missing symbol, passes on base, skipped, no result, or timeout.
- Mutant: one edit to a changed line in a source file. A generated mutant comes from an operator; an agent mutant comes from `--mutants`.
- Killed: a mutant that makes a selected test fail.
- Survivor: a mutant that the changed tests do not kill.
- Unchanged from base: a mutant whose code also appears in the base lines that its hunk replaces. The change may only have moved or reformatted that code.
- Not executed: a survivor whose line the changed tests never run. The sentinel check detects this.
- Sentinel: a raise or throw inserted before the statement that encloses a survivor's line.
- Caught by other tests: a survivor that the other tests in the changed test files kill.
- Not run: a mutant that the budget did not reach.
- No result: a run in which zero tests executed or every test skipped. It is never a kill or a survivor.
- Canary: a mutant that raises at the top of a changed module. It must be killed, or the worktree is not loading the code it mutates.
- Worktree: a temporary directory that holds a materialized tree (head or base) plus symlinks to the checkout's dependency dirs.
- Gate: the pass, fail, or error decision and its exit code.
