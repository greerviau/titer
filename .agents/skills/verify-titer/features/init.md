# init

## Sub-features

- Writes `titer.toml`, the pinned dev dependency, and the repo skill in `.agents/skills/titer/` and `.claude/skills/titer/`.
- `--dry-run` prints the plan. `--update` rewrites the skill and the pin.
- Detects pytest, unittest, vitest, and cargo projects. A cargo workspace gets one runner at its root; titer pins nothing for cargo.

## How to reach it

`titer init` in a git repo without `titer.toml`.

## Driving it

```sh
d=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/titer-init.XXXXXX")
git -C "$d" init -q -b main
printf '[project]\nname = "x"\nversion = "0"\n' > "$d/pyproject.toml"
(cd "$d" && "$OLDPWD/target/debug/titer" init --dry-run)
rm -rf "$d"
```

For a cargo project, `.agents/skills/verify-titer/scripts/check-rust.sh` saves `init-dry-run.txt`.

## Gotchas

- The skill text comes from `src/init/SKILL.md`, compiled into the binary. Rebuild after editing it.
