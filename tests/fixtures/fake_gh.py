#!/usr/bin/env python
"""Drop-in fake `gh` CLI for shell integration tests.

Stubs the handful of subcommands the gh orchestrator invokes:
- ``gh repo view --json defaultBranchRef``
- ``gh repo view --json nameWithOwner -q .nameWithOwner``
- ``gh issue create``
- ``gh issue view <ref> [--repo R] --json ...``
- ``gh pr edit``
- ``gh pr diff``
- ``gh api ...`` (review state polling)

Behavior is configurable via env vars:
- ``FAKE_GH_REPO`` (default ``owner/repo``)
- ``FAKE_GH_DEFAULT_BRANCH`` (default ``main``)
- ``FAKE_GH_ISSUE_BODY`` (default ``# Plan\\nDo stuff.\\n``)
- ``FAKE_GH_ISSUE_URL`` (default ``https://github.com/owner/repo/issues/42``)
- ``FAKE_GH_COPILOT_STATE`` (default ``APPROVED``)

Each invocation appends one JSON line to ``$FAKE_GH_LOG`` so tests can
assert what subcommands were called.
"""

from __future__ import annotations

import json
import os
import re
import sys


def log_invocation(argv) -> None:
    log_path = os.environ.get("FAKE_GH_LOG")
    if not log_path:
        return
    # Atomic append: a single os.write() of one bytes payload avoids
    # interleaved/corrupted lines if the log is ever shared across processes.
    payload = (json.dumps({"argv": list(argv)}) + "\n").encode("utf-8")
    fd = None
    try:
        fd = os.open(log_path, os.O_APPEND | os.O_CREAT | os.O_WRONLY, 0o666)
        os.write(fd, payload)
    except OSError:
        pass
    finally:
        if fd is not None:
            try:
                os.close(fd)
            except OSError:
                pass


def main(argv):
    log_invocation(argv)
    if not argv:
        return 0
    sub = argv[0]

    repo = os.environ.get("FAKE_GH_REPO", "owner/repo")
    default_branch = os.environ.get("FAKE_GH_DEFAULT_BRANCH", "main")

    if sub == "repo" and len(argv) >= 2 and argv[1] == "view":
        if "defaultBranchRef" in argv:
            sys.stdout.write(default_branch + "\n")
            return 0
        if "nameWithOwner" in argv:
            sys.stdout.write(repo + "\n")
            return 0
        sys.stdout.write(f"{repo}\n")
        return 0

    if sub == "issue" and len(argv) >= 2 and argv[1] == "create":
        url = os.environ.get(
            "FAKE_GH_ISSUE_URL", "https://github.com/owner/repo/issues/42"
        )
        # Match real `gh issue create`, which writes only the URL to stdout
        # so callers can use `URL=$(gh issue create ...)` directly.
        sys.stdout.write(f"{url}\n")
        return 0

    if sub == "issue" and len(argv) >= 2 and argv[1] == "view":
        body = os.environ.get("FAKE_GH_ISSUE_BODY", "# Plan\nDo stuff.\n")
        if "--jq" in argv:
            sys.stdout.write(body + "\n")
            return 0
        ref = argv[2] if len(argv) > 2 else "42"
        m = re.match(r"^#?(\d+)$", ref)
        num = int(m.group(1)) if m else 42
        url = f"https://github.com/{repo}/issues/{num}"
        out = json.dumps({"number": num, "url": url, "body": body})
        sys.stdout.write(out + "\n")
        return 0

    if sub == "pr" and len(argv) >= 2 and argv[1] == "edit":
        return 0
    if sub == "pr" and len(argv) >= 2 and argv[1] == "diff":
        sys.stdout.write("diff --git a/f b/f\n")
        return 0
    if sub == "pr" and len(argv) >= 2 and argv[1] == "merge":
        return 0

    if sub == "api":
        state = os.environ.get("FAKE_GH_COPILOT_STATE", "APPROVED")
        sys.stdout.write(state + "\n")
        return 0

    # Unknown subcommands: succeed quietly so tests don't fail on tangential calls.
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
