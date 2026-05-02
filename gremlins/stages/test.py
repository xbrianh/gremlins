"""Test stage.

Runs the user-supplied test command in a loop. On failure, invokes claude to
fix the code and retries. Succeeds when the command exits 0 or bails after
max_attempts.
"""

from __future__ import annotations

import os
import pathlib
import subprocess

from ..clients.claude import ClaudeClient
from ..state import check_bail, emit_bail

PROMPT_TEMPLATE_PATH = pathlib.Path(__file__).resolve().parent.parent / "prompts" / "test_fix.md"


def _diff_text(is_git: bool, cwd: pathlib.Path) -> str:
    if not is_git:
        return ""
    try:
        unstaged = subprocess.run(
            ["git", "diff"], capture_output=True, text=True, cwd=cwd, check=False,
        )
        staged = subprocess.run(
            ["git", "diff", "--cached"], capture_output=True, text=True, cwd=cwd, check=False,
        )
        return (unstaged.stdout + staged.stdout).strip()
    except Exception:
        return ""


def _escape_fmt(s: str) -> str:
    """Escape curly braces so str.format() treats content as literal text."""
    return s.replace("{", "{{").replace("}", "}}")


def run_test_stage(
    *,
    client: ClaudeClient,
    session_dir: pathlib.Path,
    test_cmd: str | None,
    max_attempts: int,
    test_fix_model: str,
    is_git: bool,
    cwd: pathlib.Path,
    code_style: str,
) -> None:
    """Run the user-supplied test command, looping on failure until green or exhausted.

    When test_cmd is None, logs a skip line and returns immediately (no-op stage).
    """
    if test_cmd is None:
        print("==> [5/5] test stage skipped (no --test)", flush=True)
        return

    commit_instr = ""
    if is_git:
        commit_instr = (
            "- After fixing, stage the changed files by name and create a single git "
            "commit titled 'Fix failing tests'. Do not push."
        )

    bail_section = ""
    if os.environ.get("GR_ID"):
        bail_section = (
            "\n\nIf you cannot fix the failure (e.g. the test checks behaviour you "
            "legitimately cannot implement), run:\n"
            "  `python -m gremlins.cli bail other \"<one-line reason>\"`\n"
            "before finishing."
        )

    template = PROMPT_TEMPLATE_PATH.read_text(encoding="utf-8")

    _exhausted = False
    _agent_bailed = False
    try:
        for attempt in range(1, max_attempts + 1):
            log_file = session_dir / f"test-attempt-{attempt}.log"
            result = subprocess.run(
                test_cmd, shell=True, cwd=cwd,
                capture_output=True, text=True,
            )
            log_file.write_text(result.stdout + result.stderr, encoding="utf-8")

            if result.returncode == 0:
                print(f"    test attempt {attempt}: green", flush=True)
                return

            print(f"    test attempt {attempt}: failed (exit {result.returncode})", flush=True)

            if attempt == max_attempts:
                break

            diff = _diff_text(is_git, cwd)
            test_output = log_file.read_text(encoding="utf-8")
            fix_prompt = template.format(
                code_style=_escape_fmt(code_style),
                test_cmd=test_cmd,
                test_output=test_output,
                diff_text=diff,
                commit_instr=commit_instr,
                bail_section=bail_section,
            )
            client.run(
                fix_prompt,
                label=f"test-fix-{attempt}",
                model=test_fix_model,
                raw_path=session_dir / f"stream-test-{attempt}.jsonl",
            )
            # Propagate immediately if the agent wrote a bail marker; don't
            # overwrite it by running another test attempt or exhaustion bail.
            _agent_bailed = True
            check_bail(f"test-fix-{attempt}")
            _agent_bailed = False

        _exhausted = True
        emit_bail("other", f"tests failed after {max_attempts} attempts")
        raise RuntimeError(f"test stage exhausted {max_attempts} attempts")
    except (SystemExit, Exception) as exc:
        if not _exhausted and not _agent_bailed:
            emit_bail("other", f"test stage failed: {exc}"[:200])
        raise
