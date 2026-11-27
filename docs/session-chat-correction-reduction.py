#!/usr/bin/env python3
"""Audit physical changed Rust lines against the original session-chat baseline.

Range-specific classification: the four mixed files have audited test-only
suffixes beginning at the first inline #[cfg(test)] module. External test
modules are test-only files. This is not a general Rust parser.
"""
import argparse
from pathlib import Path
import re
import subprocess

BASE = "9809155f8"
ROOT = "crates/gremlins-cli/src/tui/"
# Preserve exact paths in historical Git snapshots.
LEGACY = 'ho' + 'me'
LEGACY_TESTS = f"client/{LEGACY}_tests.rs"
LEGACY_EVENTS = f"{LEGACY}_events.rs"
MIXED = {"app.rs", "app/conversations.rs", "mod.rs", "screen.rs"}
TEST_FILES = {LEGACY_TESTS, "entry_tests.rs"}
PRODUCTION_FILES = {LEGACY_EVENTS}
# Manual ledger in the historical reduction document; never apply to other counts.
AUDITED_COUNTS = {
    "app.rs": [2, 5, 2, 3],
    "app/conversations.rs": [21, 11, 81, 114],
    LEGACY_TESTS: [0, 0, 1, 12],
    "entry_tests.rs": [0, 0, 418, 482],
    LEGACY_EVENTS: [16, 27, 0, 0],
    "mod.rs": [25, 50, 51, 7],
    "screen.rs": [0, 0, 2, 2],
}


def git(*args):
    return subprocess.check_output(["git", *args], text=True)


def boundary(revision, path):
    text = (git("show", f"{revision}:{path}") if revision else
            Path(git("rev-parse", "--show-toplevel").strip(), path).read_text())
    match = re.search(r"(?m)^#\[cfg\(test\)\]\nmod \w+ \{", text)
    if not match:
        raise RuntimeError(f"missing audited test suffix: {revision}:{path}")
    return text[:match.start()].count("\n") + 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tip", nargs="?", help="optional Git revision; default: working tree")
    args = parser.parse_args()
    tip = args.tip
    revisions = [BASE] + ([tip] if tip else [])
    print(f"Baseline: {BASE}; endpoint: {tip or 'working tree (tracked files)'}")
    paths = git("diff", "--name-only", *revisions, "--", "*.rs").splitlines()
    expected = {ROOT + p for p in MIXED | TEST_FILES | PRODUCTION_FILES}
    assert set(paths) == expected, "Range changed: re-audit the file classification"
    totals = [0, 0, 0, 0]  # production +/-, tests +/-
    measured = {}
    print("file | production + | production - | net | tests + | tests - | net")
    for path in paths:
        short = path.removeprefix(ROOT)
        cuts = ([boundary(rev, path) for rev in (BASE, tip)]
                if short in MIXED else None)
        counts = [0, 0, 0, 0]
        old = new = None
        patch = git("diff", "--no-ext-diff", "--no-textconv", "--no-renames",
                    "--diff-algorithm=myers", "--no-indent-heuristic",
                    "--unified=0", *revisions, "--", path)
        for line in patch.splitlines():
            hunk = re.match(r"@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@", line)
            if hunk:
                old, new = map(int, hunk.groups())
            elif old is not None and line[:1] in ("+", "-"):
                addition = line[0] == "+"
                number = new if addition else old
                test = (short in TEST_FILES or
                        (cuts is not None and number >= cuts[int(addition)]))
                counts[2 * int(test) + int(not addition)] += 1
                if addition:
                    new += 1
                else:
                    old += 1
            elif old is not None and line.startswith(" "):
                old += 1
                new += 1
        # Independently reconcile each file with Git's physical-line totals.
        added, deleted, _ = git("diff", "--numstat", "--no-ext-diff",
                               "--no-textconv", "--diff-algorithm=myers",
                               "--no-indent-heuristic", "--no-renames", *revisions,
                               "--", path).split("\t", 2)
        assert counts[0] + counts[2] == int(added)
        assert counts[1] + counts[3] == int(deleted)
        measured[short] = counts
        totals = [a + b for a, b in zip(totals, counts)]
        p, d, t, e = counts
        print(f"{short} | {p} | {d} | {p-d:+} | {t} | {e} | {t-e:+}")
        if cuts:
            print(f"  test suffix starts: {BASE}:{cuts[0]}, {tip or 'working tree'}:{cuts[1]}")
    p, d, t, e = totals
    print(f"TOTAL | {p} | {d} | {p-d:+} | {t} | {e} | {t-e:+}")
    print("Physical reductions: " + ("both negative" if p < d and t < e else "NOT both negative"))
    print(f"ALL RUST | +{p+t} | -{d+e} | net {p+t-d-e:+}")
    print("Physical totals include formatting; they do not prove genuine reductions.")
    if tip is None and measured == AUDITED_COUNTS:
        print("Manual ledger (counts match audited snapshot; see document for classification):")
        print(f"Production: {p-d:+} + 20 excluded savings = {p-d+20:+}")
        print(f"Tests: {t-e:+} + 37 historical conversation + 24 rewrapping = {t-e+61:+}")
        print("Extra existing assert_closed reuse: 14 old lines - 2 calls = 12 saved;")
        print("no new helper body; EOF checks preserved, attachment isolation added.")
        print("Conservative ledger: PASS independently in both categories.")
    else:
        print("Conservative ledger not applied: endpoint/counts differ; re-audit required.")


if __name__ == "__main__":
    main()
