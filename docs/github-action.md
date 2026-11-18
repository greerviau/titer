# GitHub Action

The Action runs the titer version that the repo pins against the pull request's base branch. It fails the job when titer exits 1 or 2.

## Workflow

```yaml
name: titer

on: pull_request

permissions:
  contents: read
  pull-requests: write # only for comment: true

# One run per pull request at a time, so two runs never post at once.
concurrency:
  group: titer-${{ github.event.pull_request.number }}
  cancel-in-progress: true

jobs:
  titer:
    runs-on: ubuntu-latest
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

`fetch-depth: 0` gives titer the history it needs to find the merge-base. Install the repo's dependencies before the titer step, because titer runs the repo's own test command.

The Action runs `npx --no-install titer` when `package.json` pins titer, else `uv run --no-sync titer` when `pyproject.toml` does. Set `command` to override this.

## Inputs

| Input | Default | Meaning |
|---|---|---|
| `base` | the pull request's base branch | The base ref. |
| `args` | none | Extra arguments for `titer check`, such as `--budget 90s`. |
| `working-directory` | `.` | The repo root, where `titer.toml` is. |
| `annotations` | `true` | Annotates survivors, weakened tests, and lines no test runs in the diff. |
| `comment` | `false` | Posts the summary as a pull request comment. Needs `pull-requests: write`. |
| `github-token` | `github.token` | The token that posts the comment. |
| `command` | the pinned version | The command that runs titer. |

## Outputs

| Output | Meaning |
|---|---|
| `exit-code` | titer's exit code: 0 pass, 1 fail, 2 could not run. |
| `report` | The path of the JSON report. |
| `summary` | The path of the Markdown summary. |

## Job summary

The job summary starts with the gate result (passed, failed, or could not run) and the evidence it rests on. Then it lists survivors, lines no test runs, and weakened tests, with links to the pull request's head commit. A survivor whose code is unchanged from base says so.

The full test and mutant tables are collapsed. The test table shows each changed test's failure on base, so a reviewer can check that the failure is the behavior the change targets.

## Annotations

With `annotations: true`, survivors and weakened tests appear as warnings on their lines in the diff. Lines that no test runs appear as notices.

Annotations need the pull request head checked out, as in the workflow above. The default `pull_request` checkout is a merge commit whose line numbers can differ, so the Action skips annotations there.

## Pull request comments

With `comment: true`, the Action posts the summary as a pull request comment. The comment heading names the commit that titer checked.

- Each new head commit gets a new comment.
- Earlier titer comments collapse as outdated, so the newest result is next to the newest push.
- A rerun on the same commit updates that commit's comment.

On pull requests from forks of a public repo, GitHub gives the token read access only. The Action warns and skips the comment.
