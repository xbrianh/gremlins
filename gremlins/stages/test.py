"""Test stage."""

from __future__ import annotations

import dataclasses
import logging
import pathlib
import subprocess

from ..prompts import BUNDLED_PROMPT_DIR, load_prompts
from ..state import check_bail, emit_bail
from .context import StageContext

logger = logging.getLogger(__name__)


@dataclasses.dataclass
class TestOptions:
    test_cmd: str | None
    max_attempts: int
    test_fix_model: str
    is_git: bool
    cwd: pathlib.Path
    code_style: str


def _diff_text(is_git: bool, cwd: pathlib.Path) -> str:
    if not is_git:
        return ""
    try:
        unstaged = subprocess.run(
            ["git", "diff"],
            capture_output=True,
            text=True,
            cwd=cwd,
            check=False,
        )
        staged = subprocess.run(
            ["git", "diff", "--cached"],
            capture_output=True,
            text=True,
            cwd=cwd,
            check=False,
        )
        return (unstaged.stdout + staged.stdout).strip()
    except Exception:
        return ""


def _escape_fmt(s: str) -> str:
    """Escape curly braces so str.format() treats content as literal text."""
    return s.replace("{", "{{").replace("}", "}}")


def run(ctx: StageContext, options: TestOptions) -> None:
    """Run the user-supplied test command, looping on failure until green or exhausted."""
    if options.test_cmd is None:
        logger.info("[5/5] test stage skipped (no --test)")
        return

    commit_instr = ""
    if options.is_git:
        commit_instr = (
            "- After fixing, stage the changed files by name and create a single git "
            "commit titled 'Fix failing tests'. Do not push."
        )

    bail_section = ""
    if ctx.gr_id:
        bail_section = (
            "\n\nIf you cannot fix the failure (e.g. the test checks behaviour you "
            "legitimately cannot implement), run:\n"
            '  `python -m gremlins.cli bail other "<one-line reason>"`\n'
            "before finishing."
        )

    template = load_prompts([BUNDLED_PROMPT_DIR / "test_fix.md"])

    _exhausted = False
    _agent_bailed = False
    try:
        for attempt in range(1, options.max_attempts + 1):
            log_file = ctx.session_dir / f"test-attempt-{attempt}.log"
            result = subprocess.run(
                options.test_cmd,
                shell=True,
                cwd=options.cwd,
                capture_output=True,
                text=True,
            )
            log_file.write_text(result.stdout + result.stderr, encoding="utf-8")

            if result.returncode == 0:
                logger.info("test attempt %d: green", attempt)
                return

            logger.info("test attempt %d: failed (exit %d)", attempt, result.returncode)

            if attempt == options.max_attempts:
                break

            diff = _diff_text(options.is_git, options.cwd)
            test_output = log_file.read_text(encoding="utf-8")
            fix_prompt = template.format(
                code_style=_escape_fmt(options.code_style),
                test_cmd=options.test_cmd,
                test_output=test_output,
                diff_text=diff,
                commit_instr=commit_instr,
                bail_section=bail_section,
            )
            ctx.client.run(
                fix_prompt,
                label=f"test-fix-{attempt}",
                model=options.test_fix_model,
                raw_path=ctx.session_dir / f"stream-test-{attempt}.jsonl",
            )
            _agent_bailed = True
            check_bail(f"test-fix-{attempt}")
            _agent_bailed = False

        _exhausted = True
        emit_bail("other", f"tests failed after {options.max_attempts} attempts")
        raise RuntimeError(f"test stage exhausted {options.max_attempts} attempts")
    except (SystemExit, Exception) as exc:
        if not _exhausted and not _agent_bailed:
            emit_bail("other", f"test stage failed: {exc}"[:200])
        raise
