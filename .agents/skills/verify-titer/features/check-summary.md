# check summary

## Sub-features

- Headline: the gate result (passed, failed, could not run) and the evidence it rests on.
- Survivors, with a note when the mutated code is unchanged from base.
- Changed tests table, with each test's failure on base.
- All mutants table, and GitHub annotations.

## How to reach it

`titer check --markdown FILE` writes the summary, and `--annotations FILE` writes the annotations. The GitHub Action (`action.yml`) passes both, posts the summary as the job summary and, with `comment: true`, as a PR comment, and prints the annotations so GitHub shows them on the diff.

## Driving it

```sh
.agents/skills/verify-titer/scripts/check.sh proven
.agents/skills/verify-titer/scripts/check.sh unproven
```

Read `summary.md` and `annotations.txt` in the printed evidence directory.

## Gotchas

- Without `--markdown`, no summary is written.
- The `proven` fixture has one survivor (`>` to `>=`) because no test pins the boundary. Its code is new, so the survivor has no unchanged-from-base note. To see the note, make a change that only reformats a base line, as in `tests/mutate.rs`.
