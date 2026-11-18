#!/bin/sh
# Builds a cargo fixture repo, runs this checkout's titer init --dry-run and titer check in
# it, and keeps the output. Head fixes clamp, adds double, and tests both in one test module.
# Usage: check-rust.sh [titer check args...]
set -eu
root=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
titer="$root/target/debug/titer"
cache="${XDG_CACHE_HOME:-$HOME/.cache}"
mkdir -p "$cache"
evidence="$cache/titer-verify/$(date -u +%Y%m%dT%H%M%SZ)-rust"
scratch=$(mktemp -d "$cache/titer-verify-run.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$evidence" "$scratch/tmp" "$scratch/repo/src"
repo="$scratch/repo"
git -C "$repo" init -q -b main
git -C "$repo" config user.name fixture
git -C "$repo" config user.email fixture@example.com
printf 'target\n' > "$repo/.gitignore"
printf '[package]\nname = "demo"\nversion = "0.1.0"\nedition = "2021"\n' > "$repo/Cargo.toml"
cat > "$repo/src/lib.rs" <<'EOF'
pub fn clamp(x: i32, _hi: i32) -> i32 {
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_low_values() {
        assert_eq!(clamp(1, 3), 1);
    }
}
EOF
(cd "$repo" && "$titer" init --dry-run) > "$evidence/init-dry-run.txt" 2>&1
printf '[[runner]]\nkind = "cargo"\ncommand = ["cargo", "test"]\n' > "$repo/titer.toml"
git -C "$repo" add -A
git -C "$repo" commit -q -m base
git -C "$repo" checkout -q -b fix
cat > "$repo/src/lib.rs" <<'EOF'
pub fn clamp(x: i32, hi: i32) -> i32 {
    if x > hi {
        return hi;
    }
    x
}

pub fn double(x: i32) -> i32 {
    x * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_low_values() {
        assert_eq!(clamp(1, 3), 1);
    }

    #[test]
    fn clamps_high_values() {
        assert_eq!(clamp(5, 3), 3);
    }

    #[test]
    fn doubles() {
        assert_eq!(double(2), 4);
    }
}
EOF
git -C "$repo" status --porcelain=v2 --untracked-files=all > "$scratch/status-before"
cd "$repo"
code=0
TITER_TMPDIR="$scratch/tmp" "$titer" check --json --markdown "$evidence/summary.md" \
  --annotations "$evidence/annotations.txt" "$@" \
  > "$evidence/report.json" 2> "$evidence/stderr.txt" || code=$?
echo "$code" > "$evidence/exit-code.txt"
git -C "$repo" status --porcelain=v2 --untracked-files=all > "$scratch/status-after"
if cmp -s "$scratch/status-before" "$scratch/status-after"; then
  echo unchanged > "$evidence/checkout-status.txt"
else
  diff "$scratch/status-before" "$scratch/status-after" > "$evidence/checkout-status.txt" || true
fi
ls -A "$scratch/tmp" > "$evidence/leftover-tmp.txt"
echo "$evidence"
