# CLI reference

## titer init

```sh
titer init [--dry-run] [--update]
```

`init` opts a repo in. It detects the repo's runners and writes:

- `titer.toml`, with one `[[runner]]` per project
- a pinned dev dependency: `@greerviau/titer` in `package.json`, or `titer` in `pyproject.toml`
- the repo skill in `.agents/skills/titer/SKILL.md` (Codex, Cursor, OpenCode) and `.claude/skills/titer/SKILL.md` (Claude Code)

| Option | Effect |
|---|---|
| `--dry-run` | Prints the config and the file list without writing. |
| `--update` | Rewrites the repo skill and the pin for the installed titer version. |

You can run `init` before you install titer:

```sh
npx @greerviau/titer init
uvx titer init
```

## titer check

```sh
titer check [OPTIONS]
```

| Option | Effect |
|---|---|
| `--base REF` | The base ref. Defaults to `base` in `titer.toml`, then to the remote default branch. |
| `--only TIERS` | Runs only these tiers, comma separated: `scan`, `prove`, `mutate`, `crap`. |
| `--json` | Prints the JSON report on stdout instead of the text summary. |
| `--budget DURATION` | The time budget for mutation, for example `120s` or `3m`. The head and base test runs keep their own timeouts. |
| `--workers N` | The number of mutation workers. Defaults to a value sized from cores and memory. |
| `--mutants FILE` | Adds agent-written mutants from a JSON lines file. |
| `--coverage FILE` | A coverage file for CRAP: lcov, Cobertura XML, or coverage.py JSON. |
| `--markdown FILE` | Also writes a Markdown summary to this file, for a CI job summary or PR comment. |
| `--link-base URL` | Links file locations in the Markdown summary under this URL, such as `https://github.com/owner/repo/blob/<sha>`. |
| `--commit SHA` | The commit the Markdown summary names in its heading. |
| `--annotations FILE` | Also writes GitHub workflow commands that annotate the diff to this file. |

## Exit codes

| Code | Meaning |
|---|---|
| 0 | The gate passes. |
| 1 | The gate fails. |
| 2 | The check could not run: a changed test fails on head, the canary survives, the config is invalid, or a git command fails. |

[How titer works](how-it-works.md#the-gate) describes what the gate checks.

## Agent mutants

`--mutants FILE` adds mutants that an agent writes. The file has one JSON object per line:

```json
{"file": "app.py", "line": 3, "end_line": 3, "replacement": "    return x", "id": "optional"}
```

`replacement` replaces lines `line` through `end_line` of `file`. `end_line` defaults to `line`, and `id` is optional.

## JSON report

`titer check --json` prints a report that follows [`schema/report.schema.json`](../schema/report.schema.json). Its main fields are:

| Field | Contents |
|---|---|
| `gate` | The status (pass, fail, or error), the exit code, and the reasons. |
| `tests` | One entry per changed test, with its verdict and failure message. |
| `mutants` | One entry per mutant, with its location, operator, and outcome. |
| `findings` | Weakened tests that `scan` found. |
| `crap` | CRAP scores of changed functions. |
| `warnings` | Problems that did not stop the check. |
| `not_analyzed` | Changed files titer does not analyze. |
| `timing` | How long each tier and the whole check took, and the number of mutation workers. |

## Environment variables

| Variable | Effect |
|---|---|
| `TITER_TMPDIR` | The directory for temporary worktrees. Defaults to the user cache dir. Use a disk-backed directory. |
