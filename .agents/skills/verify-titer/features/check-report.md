# check report

## Sub-features

- `gate`: status, exit code, and reasons.
- `tests`: one entry per changed test, with its verdict and base failure message.
- `mutants`: one entry per mutant, with its outcome and `unchanged_from_base`.
- `findings` and `crap`.

## How to reach it

`titer check --json` prints the report on stdout and exits with the gate's code: 0 pass, 1 fail, 2 could not run.

## Driving it

```sh
.agents/skills/verify-titer/scripts/check.sh proven      # exit 0
.agents/skills/verify-titer/scripts/check.sh unproven    # exit 1
```

Check `exit-code.txt` and `report.json`. The report must match `schema/report.schema.json`.

## Gotchas

- `--only` limits the tiers, which changes the headline and the gate reasons.
