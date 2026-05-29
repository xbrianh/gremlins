#!/usr/bin/env python
"""Drop-in fake `claude` binary for shell integration tests.

Parses `claude -p [flags] <prompt>` argv, classifies the prompt to identify
which pipeline stage spawned it, performs the side effects that stage's
post-conditions check for (write plan.md, create a commit, write a review
file, etc.), and emits minimal valid stream-json (or text) on stdout.

Each invocation appends an entry to ``$FAKE_CLAUDE_LOG`` (one JSON line
per call) so tests can assert what was invoked, with what model, in what
cwd. ``$FAKE_CLAUDE_FAIL_AT`` (a stage name) makes the matching invocation
exit non-zero — used to test resume paths.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import subprocess
import sys
import uuid


def emit_event(evt: dict) -> None:
    sys.stdout.write(json.dumps(evt) + "\n")
    sys.stdout.flush()


def emit_minimal_stream(*, extra: list = None) -> None:
    emit_event(
        {
            "type": "system",
            "subtype": "init",
            "model": "fake",
            "cwd": os.getcwd(),
        }
    )
    for evt in extra or []:
        emit_event(evt)
    emit_event(
        {
            "type": "result",
            "subtype": "success",
            "num_turns": 1,
            "total_cost_usd": 0,
        }
    )


def emit_pr_create_stream(pr_url: str) -> None:
    tu_id = "tu-" + uuid.uuid4().hex[:8]
    emit_minimal_stream(
        extra=[
            {
                "type": "assistant",
                "message": {
                    "content": [
                        {
                            "type": "tool_use",
                            "id": tu_id,
                            "name": "Bash",
                            "input": {"command": "gh pr create --base main"},
                        },
                    ]
                },
            },
            {
                "type": "user",
                "message": {
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": tu_id,
                            "content": pr_url,
                        },
                    ]
                },
            },
        ],
    )


def emit_issue_create_stream(issue_url: str) -> None:
    tu_id = "tu-" + uuid.uuid4().hex[:8]
    emit_minimal_stream(
        extra=[
            {
                "type": "assistant",
                "message": {
                    "content": [
                        {
                            "type": "tool_use",
                            "id": tu_id,
                            "name": "Bash",
                            "input": {"command": "gh issue create --title foo"},
                        },
                    ]
                },
            },
            {
                "type": "user",
                "message": {
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": tu_id,
                            "content": issue_url,
                        },
                    ]
                },
            },
        ],
    )


def log_invocation(record: dict) -> None:
    log_path = os.environ.get("FAKE_CLAUDE_LOG")
    if not log_path:
        return
    # Atomic append: a single os.write() of one bytes payload avoids
    # interleaved/corrupted lines under the parallel review workers
    # (3 concurrent claude subprocesses write to this log).
    payload = (json.dumps(record) + "\n").encode("utf-8")
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


def maybe_fail_at(stage: str) -> None:
    if os.environ.get("FAKE_CLAUDE_FAIL_AT") == stage:
        sys.stderr.write(f"fake claude: forced failure at {stage}\n")
        sys.exit(1)


def find_path_in_prompt(prompt: str, pattern: str) -> str:
    m = re.search(pattern, prompt)
    return m.group(1) if m else ""


def git_commit_changes(message: str) -> bool:
    """Stage all changes and commit. Returns True if a commit was made."""
    try:
        subprocess.run(["git", "add", "-A"], check=False, capture_output=True)
        r = subprocess.run(
            ["git", "diff", "--cached", "--quiet"],
            check=False,
            capture_output=True,
        )
        if r.returncode == 0:
            return False  # nothing staged
        subprocess.run(
            ["git", "commit", "-m", message],
            check=False,
            capture_output=True,
        )
        return True
    except OSError:
        return False


def handle_plan(prompt: str) -> int:
    plan_file = (
        find_path_in_prompt(prompt, r"write it to the file `([^`]+)`")
        or find_path_in_prompt(prompt, r"Write the plan to `([^`]+)`")
        or find_path_in_prompt(prompt, r"(/[^\s`]+/plan\.md)")
    )
    if plan_file:
        pathlib.Path(plan_file).parent.mkdir(parents=True, exist_ok=True)
        if (
            not pathlib.Path(plan_file).exists()
            or pathlib.Path(plan_file).stat().st_size == 0
        ):
            pathlib.Path(plan_file).write_text(
                "# Test Plan\n\n## Context\nFake claude generated plan.\n\n"
                "## Tasks\n- [ ] Touch a file\n",
                encoding="utf-8",
            )
    emit_minimal_stream()
    return 0


def handle_implement(prompt: str) -> int:
    # Create a new file so review-code's diff is non-empty.
    target = pathlib.Path("fake_impl.txt")
    target.write_text("fake implementation\n", encoding="utf-8")
    if os.environ.get("FAKE_CLAUDE_RENAME_GREMLINS") == "1":
        pd = pathlib.Path("gremlins")
        if pd.is_dir():
            pd.rename("gremlins-renamed")
    in_git = (
        subprocess.run(
            ["git", "rev-parse", "--git-dir"],
            capture_output=True,
            check=False,
        ).returncode
        == 0
    )
    if in_git:
        # Commit so HEAD advances (post-impl invariant for both local and gh).
        git_commit_changes("impl: fake implementation")
    emit_minimal_stream()
    return 0


def handle_review(prompt: str) -> int:
    out_file = find_path_in_prompt(prompt, r"`([^`]+\.md)`\s+is the canonical")
    if out_file:
        out_path = pathlib.Path(out_file)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(
            "# Review (fake)\n\n## Summary\nFake review.\n\n## Findings\nNo issues.\n",
            encoding="utf-8",
        )
    emit_minimal_stream()
    return 0


def handle_address(prompt: str) -> int:
    in_git = (
        subprocess.run(
            ["git", "rev-parse", "--git-dir"],
            capture_output=True,
            check=False,
        ).returncode
        == 0
    )
    if in_git:
        # Make a no-op edit so address commit lands.
        target = pathlib.Path("fake_impl.txt")
        if target.exists():
            target.write_text("fake implementation (addressed)\n", encoding="utf-8")
            git_commit_changes("Address review feedback")
    emit_minimal_stream()
    return 0


def handle_compose_pr(prompt: str) -> int:
    m = re.search(r"- `(/[^`]+)/pr-branch\.txt`", prompt)
    if m:
        session = pathlib.Path(m.group(1))
        session.mkdir(parents=True, exist_ok=True)
        (session / "pr-branch.txt").write_text("issue-42-test-slug\n")
        (session / "pr-title.txt").write_text("Test PR Title\n")
        (session / "pr-body.md").write_text("Test PR body.\n")
    emit_minimal_stream()
    return 0


def handle_ghplan(prompt: str) -> int:
    issue_url = os.environ.get(
        "FAKE_CLAUDE_ISSUE_URL", "https://github.com/owner/repo/issues/42"
    )
    emit_issue_create_stream(issue_url)
    return 0


def handle_plan_title(prompt: str) -> int:
    # `--plan <file>` flow asks for a one-line GitHub issue title.
    emit_event(
        {
            "type": "system",
            "subtype": "init",
            "model": "fake",
            "cwd": os.getcwd(),
        }
    )
    emit_event(
        {
            "type": "result",
            "subtype": "success",
            "num_turns": 1,
            "total_cost_usd": 0.0,
            "result": "Test issue title from fake claude",
        }
    )
    return 0


def handle_fix(prompt: str) -> int:
    # Create a minimal passing test so make test succeeds on the next verify iteration.
    tests_dir = pathlib.Path("tests")
    tests_dir.mkdir(exist_ok=True)
    placeholder = tests_dir / "test_placeholder.py"
    if not placeholder.exists():
        placeholder.write_text("def test_placeholder():\n    pass\n", encoding="utf-8")
    in_git = (
        subprocess.run(
            ["git", "rev-parse", "--git-dir"],
            capture_output=True,
            check=False,
        ).returncode
        == 0
    )
    if in_git:
        git_commit_changes("Fix failing checks")
    emit_minimal_stream()
    return 0


def classify_stage(prompt: str) -> str:
    """Classify the stage from the prompt string. Order matters — earlier
    matches win when prompts contain overlapping phrases."""
    if "Produce a concise GitHub issue title" in prompt:
        return "plan-title"
    if "Do NOT push or call" in prompt:
        return "compose-pr"
    if (
        "stage all changes, and commit" in prompt
        or "Rename the current branch" in prompt
    ):
        return "commit"
    if "post the review directly to GitHub as a PR review" in prompt:
        return "github-review-pull-request"
    if "addressing review comments on a GitHub pull request" in prompt:
        return "github-address-pull-request-reviews"
    if prompt.startswith("/ghplan") or "/ghplan" in prompt[:20]:
        return "ghplan"
    if (
        "create a detailed implementation plan" in prompt.lower()
        or "You are creating an implementation plan" in prompt
    ):
        return "plan"
    if "A code review of the most recent implementation follows" in prompt:
        return "address"
    if "is the canonical and required location for your review output" in prompt:
        return "review"
    if "Implement every task in the plan above" in prompt:
        return "implement-local"
    if "Implement the plan above by making the code changes" in prompt:
        return "implement-gh"
    if "The verify step failed." in prompt:
        return "fix"
    return "unknown"


def parse_argv(argv):
    """Mimic enough of `claude -p`'s flag handling to extract what we care about.

    Returns (output_format, model, prompt, extra_flags).
    """
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("-p", action="store_true")
    parser.add_argument("--model", default=None)
    parser.add_argument("--output-format", default="stream-json")
    parser.add_argument("--permission-mode", default=None)
    parser.add_argument("--verbose", action="store_true")
    parser.add_argument("--resume", default=None)
    args, rest = parser.parse_known_args(argv)
    del rest  # prompt always comes from stdin; ignore leftover argv tokens
    prompt = sys.stdin.read()
    return args, prompt


def main(argv):
    args, prompt = parse_argv(argv)
    stage = classify_stage(prompt)

    log_invocation(
        {
            "stage": stage,
            "model": args.model,
            "output_format": args.output_format,
            "resume_session": args.resume,
            "cwd": os.getcwd(),
            "prompt_head": prompt[:200],
        }
    )

    maybe_fail_at(stage)

    if args.output_format == "text":
        sys.stdout.write("(fake text output)\n")
        return 0

    handlers = {
        "plan": handle_plan,
        "plan-title": handle_plan_title,
        "implement-local": handle_implement,
        "implement-gh": handle_implement,
        "review": handle_review,
        "address": handle_address,
        "commit": lambda p: (emit_minimal_stream(), 0)[1],
        "compose-pr": handle_compose_pr,
        "ghplan": handle_ghplan,
        "github-review-pull-request": lambda p: (emit_minimal_stream(), 0)[1],
        "github-address-pull-request-reviews": lambda p: (emit_minimal_stream(), 0)[1],
        "fix": handle_fix,
    }
    h = handlers.get(stage)
    if h is None:
        sys.stderr.write(
            f"fake claude: unrecognized stage for prompt head: {prompt[:120]!r}\n"
        )
        if os.environ.get("FAKE_CLAUDE_ALLOW_UNKNOWN_STAGE") == "1":
            emit_minimal_stream()
            return 0
        return 1
    return h(prompt)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
