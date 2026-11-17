# Coding standards

These rules apply to every change in this repo, in addition to `docs/UBIQUITOUS-LANGUAGE.md`.

## Safety

- Never modify the user's checkout. Do not write to its working tree, index, stash, refs, or worktree list. Read git state with plumbing commands, and build trees with a temporary `GIT_INDEX_FILE`.
- Start every test process in its own process group through `process::run`. Kill only process groups that titer started.
- Put temporary files in a `tmp::RunDir`, which is disk-backed and removed on exit and on SIGINT.

## Results

- Never report a kill or a survivor for a run that executed zero tests or only skipped tests. Report no result.
- Run tests through the repo's own command and the runner's native selection flags. Never use a custom loader.
- Add a field to the report only with a schema update: regenerate `schema/report.schema.json` with `cargo run -- schema > schema/report.schema.json`.

## Code

- Rust stable, edition 2024. `cargo fmt`, and `cargo clippy --all-targets -- -D warnings` must pass.
- Return `anyhow::Result` from command-level code and typed errors where a caller branches on the cause.
- Test behavior through fixture git repos built inside the tests (`tests/common`). Unit-test pure logic next to the code.
- Comments say why, not what. ASCII only.
