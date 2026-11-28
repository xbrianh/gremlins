#!/usr/bin/env python3
"""Gate all touched text files from 30eb67f6b to the working tree.

Rust test files and Python test_*.py files are reported separately. Mixed
files are wholly non-test. Test savings cannot offset non-test growth.
Blank lines and comments count; binary changes fail explicitly.
"""

import os
from pathlib import Path, PurePosixPath
import subprocess
import sys

BASELINE = "30eb67f6b"


def is_test_only(path):
    path = PurePosixPath(path)
    return (path.suffix == ".rs" and (
        path.name.endswith("_tests.rs") or path.name == "tests.rs"
        or "tests" in path.parts[:-1])) or (
        path.suffix == ".py" and path.name.startswith("test_"))


def git(root, *args):
    return subprocess.check_output(["git", "-C", str(root), *args])


def changes(root, baseline=BASELINE):
    """Yield (path, added, deleted) for all tracked and non-ignored untracked files."""
    raw = git(root, "diff", "--numstat", "-z", "--no-renames",
              "--no-ext-diff", "--no-textconv", "--diff-algorithm=myers",
              "--no-indent-heuristic", baseline, "--")
    for record in raw.split(b"\0"):
        if record:
            added, deleted, path = record.split(b"\t", 2)
            if added == b"-" or deleted == b"-":
                raise ValueError(f"cannot count binary file: {os.fsdecode(path)}")
            yield os.fsdecode(path), int(added), int(deleted)
    raw = git(root, "ls-files", "--others", "--exclude-standard", "-z")
    for path in raw.split(b"\0"):
        if path:
            name = os.fsdecode(path)
            data = (Path(root) / name).read_bytes()
            if b"\0" in data:
                raise ValueError(f"cannot count binary file: {name}")
            lines = data.count(b"\n") + int(bool(data) and not data.endswith(b"\n"))
            yield name, lines, 0


def totals(records):
    result = {"non-test": [0, 0], "Rust test-only": [0, 0],
              "Python test-only": [0, 0]}
    for path, added, deleted in records:
        category = "non-test"
        if is_test_only(path):
            category = "Python test-only" if path.endswith(".py") else "Rust test-only"
        counts = result[category]
        counts[0] += added
        counts[1] += deleted
    return result


def report(counts):
    for category, (added, deleted) in counts.items():
        print(f"{category}: added={added} deleted={deleted} net={added - deleted:+d}")
    added, deleted = counts["non-test"]
    passed = added < deleted
    print(f"{'PASS' if passed else 'FAIL'}: non-test net must be strictly < 0; "
          "test-only savings are independent")
    return 0 if passed else 1


def main():
    root = Path(__file__).resolve().parent.parent
    print(f"Baseline: {BASELINE}; endpoint: working tree + all non-ignored untracked files")
    try:
        return report(totals(changes(root)))
    except (OSError, subprocess.CalledProcessError, ValueError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
