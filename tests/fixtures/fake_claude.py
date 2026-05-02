#!/usr/bin/env python
"""Drop-in fake `claude` binary for shell integration tests.

Parses `claude -p [flags] <prompt>` argv, classifies the prompt to identify
which pipeline stage spawned it, performs the side effects that stage's
post-conditions check for (write plan.md, create a commit, write a review
file, etc.), and emits minimal valid stream-json (or text) on stdout.

Each invocation appends an entry to ``$FAKE_CLAUDE_LOG`` (one JSON line
per call) so tests can assert what was invoked, with what model, in what
cwd. ``$FAKE_CLAUDE_FAIL_AT`` (a stage name) makes the matching invocation
exit non-zero — used to test rescue / resume paths.
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


def emit_minimal_stream(session_id: str, *, extra: list = None) -> None:
    emit_event({
        "type": "system", "subtype": "init",
        "session_id": session_id,
        "model": "fake", "cwd": os.getcwd(),
    })
    for evt in extra or []:
        emit_event(evt)
    emit_event({
        "type": "result", "subtype": "success",
        "num_turns": 1, "total_cost_usd": 0,
    })


def emit_pr_create_stream(session_id: str, pr_url: str) -> None:
    tu_id = "tu-" + uuid.uuid4().hex[:8]
    emit_minimal_stream(
        session_id,
        extra=[
            {"type": "assistant", "message": {"content": [
                {"type": "tool_use", "id": tu_id, "name": "Bash",
                 "input": {"command": "gh pr create --base main"}},
            ]}},
            {"type": "user", "message": {"content": [
                {"type": "tool_result", "tool_use_id": tu_id,
                 "content": pr_url},
            ]}},
        ],
    )


def emit_issue_create_stream(session_id: str, issue_url: str) -> None:
    tu_id = "tu-" + uuid.uuid4().hex[:8]
    emit_minimal_stream(
        session_id,
        extra=[
            {"type": "assistant", "message": {"content": [
                {"type": "tool_use", "id": tu_id, "name": "Bash",
                 "input": {"command": "gh issue create --title foo"}},
            ]}},
            {"type": "user", "message": {"content": [
                {"type": "tool_result", "tool_use_id": tu_id,
                 "content": issue_url},
            ]}},
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
            check=False, capture_output=True,
        )
        if r.returncode == 0:
            return False  # nothing staged
        subprocess.run(
            ["git", "commit", "-m", message],
            check=False, capture_output=True,
        )
        return True
    except OSError:
        return False


def handle_plan(prompt: str, session_id: str) -> int:
    plan_file = find_path_in_prompt(prompt, r"write it to the file `([^`]+)`")
    if plan_file:
        pathlib.Path(plan_file).parent.mkdir(parents=True, exist_ok=True)
        pathlib.Path(plan_file).write_text(
            "# Test Plan\n\n## Context\nFake claude generated plan.\n\n"
            "## Tasks\n- [ ] Touch a file\n",
            encoding="utf-8",
        )
    emit_minimal_stream(session_id)
    return 0


def handle_implement(prompt: str, session_id: str) -> int:
    # Create a new file so review-code's diff is non-empty.
    target = pathlib.Path("fake_impl.txt")
    target.write_text("fake implementation\n", encoding="utf-8")
    if os.environ.get("FAKE_CLAUDE_RENAME_GREMLINS") == "1":
        pd = pathlib.Path("gremlins")
        if pd.is_dir():
            pd.rename("gremlins-renamed")
    in_git = subprocess.run(
        ["git", "rev-parse", "--git-dir"],
        capture_output=True, check=False,
    ).returncode == 0
    if in_git:
        # Commit so HEAD advances (post-impl invariant for both local and gh).
        git_commit_changes("impl: fake implementation")
    emit_minimal_stream(session_id)
    return 0


def handle_review(prompt: str, session_id: str) -> int:
    out_file = find_path_in_prompt(prompt, r"`([^`]+\.md)`\s+is the canonical")
    if out_file:
        out_path = pathlib.Path(out_file)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(
            "# Review (fake)\n\n## Summary\nFake review.\n\n"
            "## Findings\nNo issues.\n",
            encoding="utf-8",
        )
    emit_minimal_stream(session_id)
    return 0


def handle_address(prompt: str, session_id: str) -> int:
    in_git = subprocess.run(
        ["git", "rev-parse", "--git-dir"],
        capture_output=True, check=False,
    ).returncode == 0
    if in_git:
        # Make a no-op edit so address commit lands.
        target = pathlib.Path("fake_impl.txt")
        if target.exists():
            target.write_text("fake implementation (addressed)\n", encoding="utf-8")
            git_commit_changes("Address review feedback")
    emit_minimal_stream(session_id)
    return 0


def handle_commit_pr(prompt: str, session_id: str) -> int:
    pr_url = os.environ.get(
        "FAKE_CLAUDE_PR_URL", "https://github.com/owner/repo/pull/101"
    )
    emit_pr_create_stream(session_id, pr_url)
    return 0


def handle_ghplan(prompt: str, session_id: str) -> int:
    issue_url = os.environ.get(
        "FAKE_CLAUDE_ISSUE_URL", "https://github.com/owner/repo/issues/42"
    )
    emit_issue_create_stream(session_id, issue_url)
    return 0


def handle_plan_title(prompt: str, session_id: str) -> int:
    # `--plan <file>` flow asks for a one-line GitHub issue title.
    emit_event({"type": "system", "subtype": "init", "session_id": session_id,
                "model": "fake", "cwd": os.getcwd()})
    emit_event({"type": "result", "subtype": "success", "num_turns": 1,
                "total_cost_usd": 0.0, "result": "Test issue title from fake claude"})
    return 0


def handle_rescue_diagnosis(prompt: str) -> int:
    """Rescue diagnosis-step prompt: write the marker file with status from env.

    The marker path is embedded in the prompt; we extract it and write a JSON
    object. Default verdict is "fixed" — tests override via env to exercise
    other branches.
    """
    m = re.search(r"(/[^\s`]+\.done)\b", prompt)
    marker_path = m.group(1) if m else ""
    status = os.environ.get("FAKE_CLAUDE_RESCUE_VERDICT", "fixed")
    summary = os.environ.get("FAKE_CLAUDE_RESCUE_SUMMARY", "fake diagnosis")
    if marker_path:
        try:
            pathlib.Path(marker_path).parent.mkdir(parents=True, exist_ok=True)
            pathlib.Path(marker_path).write_text(
                json.dumps({"status": status, "summary": summary}),
                encoding="utf-8",
            )
        except OSError:
            pass
    return 0


def classify_stage(prompt: str) -> str:
    """Classify the stage from the prompt string. Order matters — earlier
    matches win when prompts contain overlapping phrases."""
    if "diagnosing a failed background gremlin" in prompt:
        return "rescue-diagnosis"
    if "Produce a concise GitHub issue title" in prompt:
        return "plan-title"
    if "Print ONLY the PR URL on the final line" in prompt:
        return "commit-pr"
    if "/ghreview " in prompt:
        return "ghreview"
    if "/ghaddress " in prompt:
        return "ghaddress"
    if prompt.startswith("/ghplan") or "/ghplan" in prompt[:20]:
        return "ghplan"
    if "Create a detailed implementation plan" in prompt:
        return "plan"
    if "A code review of the most recent implementation follows" in prompt:
        return "address"
    if "is the canonical and required location for your review output" in prompt:
        return "review"
    if "Implement every task in the plan above" in prompt:
        return "implement-local"
    if "Implement the plan above by making the code changes" in prompt:
        return "implement-gh"
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
    # Last positional is the prompt; anything else is extra/unknown.
    prompt = rest[-1] if rest else ""
    return args, prompt


def main(argv):
    args, prompt = parse_argv(argv)
    stage = classify_stage(prompt)
    session_id = f"sess-{stage}-{uuid.uuid4().hex[:6]}"

    log_invocation({
        "stage": stage,
        "model": args.model,
        "output_format": args.output_format,
        "resume_session": args.resume,
        "cwd": os.getcwd(),
        "prompt_head": prompt[:200],
    })

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
        "commit-pr": handle_commit_pr,
        "ghplan": handle_ghplan,
        "ghreview": lambda p, s: (emit_minimal_stream(s), 0)[1],
        "ghaddress": lambda p, s: (emit_minimal_stream(s), 0)[1],
        "rescue-diagnosis": lambda p, s: handle_rescue_diagnosis(p),
    }
    h = handlers.get(stage)
    if h is None:
        sys.stderr.write(f"fake claude: unrecognized stage for prompt head: {prompt[:120]!r}\n")
        if os.environ.get("FAKE_CLAUDE_ALLOW_UNKNOWN_STAGE") == "1":
            emit_minimal_stream(session_id)
            return 0
        return 1
    return h(prompt, session_id)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
