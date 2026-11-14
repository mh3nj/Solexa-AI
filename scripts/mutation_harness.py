#!/usr/bin/env python3
"""Crash-safe mutation-testing harness for the SOLEXA workspace.

WHY THIS EXISTS (rounds 58-65): every behavior change in this project is
checked by *mutating the code and confirming a test fails*. A hand-rolled loop
that was killed mid-run once left a mutated source file behind. This tool makes
that impossible to miss or lose:

  * the original file is copied to `<file>.mutbak` BEFORE any change;
  * on start, a leftover `.mutbak` (from a killed run) is restored FIRST;
  * the file is restored after every mutant, on normal exit, and on SIGINT/SIGTERM.

It also reports the three outcomes honestly (a compile error is NOT a kill):

  KILLED-BY-TEST            a test failed            -> the tests bite
  KILLED-BY-HANG            tests timed out/deadlock -> the tests bite (hang)
  COMPILE-ERROR(not a kill) the mutant didn't build  -> REWRITE THE MUTANT
  SURVIVED                  everything passed        -> a TEST IS MISSING

Usage:
  python3 scripts/mutation_harness.py mutants.json [--timeout 150] [--only SUBSTR]

mutants.json:
  [ {"name": "short description",
     "file": "crates/solexa-x/src/lib.rs",
     "old":  "exact text, must occur exactly once",
     "new":  "replacement text",
     "crate": "solexa-x"} , ... ]

Run it from the repository root. For long runs, detach it so a tool timeout
cannot kill it:   setsid nohup python3 scripts/mutation_harness.py m.json > log 2>&1 &
Exit status: 0 if no mutant survived, 1 if any survived or didn't compile, 2 on usage error.
"""
import json, os, shutil, signal, subprocess, sys

def restore(path):
    bak = path + ".mutbak"
    if os.path.exists(bak):
        shutil.move(bak, path)

def run_tests(crate, timeout):
    cmd = f"cargo test -p {crate} 2>&1 | grep -E '^test .*FAILED|^error(\\[|:)' | head -4"
    try:
        r = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        subprocess.run(f"pkill -f '{crate.replace('-', '_')}-' || true", shell=True)
        return "KILLED-BY-HANG", ""
    out = r.stdout.strip()
    if not out:
        return "SURVIVED", ""
    if out.startswith("error"):
        return "COMPILE-ERROR(not a kill)", out
    return "KILLED-BY-TEST", out

def main():
    args = sys.argv[1:]
    if not args or args[0].startswith("-"):
        print(__doc__); sys.exit(2)
    spec_path, timeout, only = args[0], 150, None
    if "--timeout" in args: timeout = int(args[args.index("--timeout") + 1])
    if "--only" in args: only = args[args.index("--only") + 1]
    mutants = json.load(open(spec_path))
    files = sorted({m["file"] for m in mutants})
    for f in files:                      # crash recovery from a previous killed run
        restore(f)
    current = {"file": None}
    def bail(signum, frame):
        if current["file"]: restore(current["file"])
        sys.exit(130)
    signal.signal(signal.SIGINT, bail); signal.signal(signal.SIGTERM, bail)
    bad = 0
    try:
        for m in mutants:
            if only and only not in m["name"]: continue
            src = open(m["file"]).read()
            if src.count(m["old"]) != 1:
                print(f"SPEC-ERROR   {m['name']}: 'old' occurs {src.count(m['old'])} times (need exactly 1)", flush=True)
                bad += 1; continue
            shutil.copyfile(m["file"], m["file"] + ".mutbak"); current["file"] = m["file"]
            open(m["file"], "w").write(src.replace(m["old"], m["new"]))
            try:
                kind, detail = run_tests(m["crate"], timeout)
            finally:
                restore(m["file"]); current["file"] = None
            print(f"{kind:26s} {m['name']}", flush=True)
            if kind in ("SURVIVED", "COMPILE-ERROR(not a kill)"):
                bad += 1
                if detail: print("    " + detail.replace("\n", "\n    "), flush=True)
    finally:
        for f in files: restore(f)
    print("DONE: all mutants killed" if bad == 0 else f"DONE: {bad} mutant(s) need attention", flush=True)
    sys.exit(0 if bad == 0 else 1)

if __name__ == "__main__":
    main()
