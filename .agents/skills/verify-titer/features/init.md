# init

## Sub-features

- Writes `titer.toml`, the pinned dev dependency, and the repo skill in `.agents/skills/titer/` and `.claude/skills/titer/`.
- `--dry-run` prints the plan. `--update` rewrites the skill and the pin.

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

## Gotchas

- The skill text comes from `src/init/SKILL.md`, compiled into the binary. Rebuild after editing it.
