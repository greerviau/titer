#!/usr/bin/env python3
"""Replays titer over the public corpus in replay/corpus.json.

Usage: replay/run.py TITER_BIN [--work DIR] [--check] [titer check args...]

Each PR runs as `titer check --base MERGE^1` with head at the merge commit, so the diff is
exactly the PR. pytest 9.2.0 with coverage.py 7.17.5, and vitest 5.1.0 with
@vitest/coverage-v8, are pinned for every commit, so mutants run with per-test coverage;
both corpora are pure libraries whose tests need nothing else. With --check, the run fails when a PR's
verdict counts or gate differ from replay/expected.json.
"""
import argparse, json, os, subprocess, sys, time
from collections import Counter
from pathlib import Path

here = Path(__file__).resolve().parent
parser = argparse.ArgumentParser()
parser.add_argument("titer")
parser.add_argument("--work", default=os.path.expanduser("~/.cache/titer-replay"))
parser.add_argument("--check", action="store_true")
args, extra = parser.parse_known_args()
corpus = json.loads((here / "corpus.json").read_text())
work = Path(args.work)
work.mkdir(parents=True, exist_ok=True)

def run(cmd, **kw):
    return subprocess.run(cmd, check=True, capture_output=True, text=True, **kw)

python = work / "venv/bin/python"
if not python.exists():
    run(["uv", "venv", "--quiet", "--python", "3.12", str(work / "venv")])
run(["uv", "pip", "install", "--quiet", "--python", str(python), "pytest==9.2.0", "coverage==7.17.5"])
node = work / "node"
if not (node / "node_modules/@vitest/coverage-v8").exists():
    node.mkdir(exist_ok=True)
    (node / "package.json").write_text('{"private": true}\n')
    run(["npm", "install", "--silent", "--no-audit", "--no-fund", "--prefix", str(node), "vitest@5.1.0",
         "@vitest/coverage-v8@5.1.0"])

for name, repo in corpus["repos"].items():
    clone = work / name
    if not clone.exists():
        run(["git", "clone", "--quiet", repo["url"], str(clone)])
    else:
        run(["git", "-C", str(clone), "fetch", "--quiet", "origin"])
    exclude = clone / ".git/info/exclude"
    if "titer.toml" not in exclude.read_text().split():
        exclude.write_text(exclude.read_text() + "\ntiter.toml\nnode_modules\n")

expected = json.loads((here / "expected.json").read_text()) if (here / "expected.json").exists() else {}
rows, failures = [], []
for pr in corpus["prs"]:
    clone = work / pr["repo"]
    run(["git", "-C", str(clone), "checkout", "--quiet", "--detach", pr["merge"]])
    run(["git", "-C", str(clone), "clean", "-qfdx", "-e", "titer.toml", "-e", "node_modules"])
    config = corpus["repos"][pr["repo"]]["runner"].replace("{python}", str(python))
    (clone / "titer.toml").write_text(config)
    if pr["repo"] == "ufo" and not (clone / "node_modules").exists():
        (clone / "node_modules").symlink_to(node / "node_modules")
    env = dict(os.environ, TITER_TMPDIR=str(work / "titer-tmp"))
    started = time.time()
    proc = subprocess.run([str(Path(args.titer).resolve()), "check", "--base", f"{pr['merge']}^1", "--json", *extra],
                          cwd=clone, env=env, capture_output=True, text=True)
    elapsed = time.time() - started
    try:
        report = json.loads(proc.stdout)
    except json.JSONDecodeError:
        print(proc.stderr, file=sys.stderr)
        report = {"gate": {"status": "crash"}, "tests": [], "mutants": []}
    verdicts = dict(Counter(t.get("verdict") or f"head_{t['head']}" for t in report["tests"]))
    mutants = dict(Counter(m["outcome"] for m in report.get("mutants", [])))
    key = f"{pr['repo']}#{pr['pr']}"
    row = {"pr": key, "seconds": round(elapsed, 1), "exit": proc.returncode, "gate": report["gate"]["status"],
           "verdicts": verdicts, "mutants": mutants}
    rows.append(row)
    print(json.dumps(row), flush=True)
    want = expected.get(key)
    if args.check and want and (want["verdicts"] != verdicts or want["gate"] != row["gate"]):
        failures.append(f"{key}: expected {want}, got gate {row['gate']} verdicts {verdicts}")

(work / "summary.json").write_text(json.dumps(rows, indent=1))
summary = os.environ.get("GITHUB_STEP_SUMMARY")
if summary:
    with open(summary, "a") as out:
        out.write("| PR | seconds | gate | verdicts | mutants |\n|---|---|---|---|---|\n")
        for r in rows:
            v = ", ".join(f"{k} {n}" for k, n in sorted(r["verdicts"].items()))
            m = ", ".join(f"{k} {n}" for k, n in sorted(r["mutants"].items()))
            out.write(f"| {r['pr']} | {r['seconds']} | {r['gate']} | {v} | {m} |\n")
if failures:
    print("\n".join(failures), file=sys.stderr)
    sys.exit(1)
