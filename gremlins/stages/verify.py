"""Verify stage — runs cmds joined with &&; used by both gh and local pipelines."""

from __future__ import annotations

import dataclasses
import logging
import pathlib
import subprocess

from gremlins import git as _git_mod
from gremlins.prompts import load_prompts
from gremlins.stages.context import StageContext
from gremlins.stages.registry import register_stage
from gremlins.state import check_bail, emit_bail

logger = logging.getLogger(__name__)

_PROMPT = pathlib.Path(__file__).resolve().parent / "verify_fix.md"


@dataclasses.dataclass
class VerifyOptions:
    fix_model: str
    cwd: pathlib.Path
    code_style: str
    is_git: bool
    commit_after_fix: bool
    cmds: list[str] = dataclasses.field(default_factory=lambda: [])
    max_attempts: int = 3


def _diff_text(cwd: pathlib.Path, *, is_git: bool) -> str:
    if not is_git:
        return ""
    try:
        unstaged = _git_mod.diff_output(cwd=cwd)
        staged = _git_mod.diff_output(["--cached"], cwd=cwd)
        return (unstaged + staged).strip()
    except Exception:
        return ""


def _escape_fmt(s: str) -> str:
    """Escape curly braces so str.format() treats content as literal text."""
    return s.replace("{", "{{").replace("}", "}}")


def run(ctx: StageContext, options: VerifyOptions) -> None:
    if options.commit_after_fix and options.is_git:
        commit_instr = (
            "- After fixing, stage the changed files by name and create a single git "
            "commit titled 'Fix failing checks'. Do not push."
        )
    else:
        commit_instr = (
            "- After fixing, leave changes uncommitted — do not stage or commit. "
            "The next stage (commit-pr) will handle staging and committing."
        )

    bail_section = ""
    if ctx.gr_id:
        bail_section = (
            "\n\nIf you cannot fix the failure (e.g. the check reports a violation "
            "you legitimately cannot resolve), run:\n"
            '  `python -m gremlins.bail other "<one-line reason>"`\n'
            "before finishing."
        )

    template = load_prompts([_PROMPT])

    cmds = [c for c in options.cmds if c.strip()]
    if not cmds:
        logger.info("verify: no cmds configured; skipping")
        return
    combined_cmd = " && ".join(cmds)
    commands_section = "**Commands run:**\n" + "\n".join(f"- `{c}`" for c in cmds)

    _exhausted = False
    _agent_bailed = False
    try:
        for attempt in range(1, options.max_attempts + 1):
            log_file = ctx.session_dir / f"verify-attempt-{attempt}.log"
            result = subprocess.run(
                combined_cmd,
                shell=True,
                cwd=options.cwd,
                capture_output=True,
                text=True,
            )
            log_file.write_text(result.stdout + result.stderr, encoding="utf-8")

            if result.returncode == 0:
                logger.info("verify attempt %d: green", attempt)
                return

            logger.info(
                "verify attempt %d: failed (exit %d)", attempt, result.returncode
            )

            if attempt == options.max_attempts:
                break

            diff = _diff_text(options.cwd, is_git=options.is_git)
            verify_output = log_file.read_text(encoding="utf-8")
            fix_prompt = template.format(
                code_style=_escape_fmt(options.code_style),
                commands_section=commands_section,
                verify_output=verify_output,
                diff_text=diff,
                commit_instr=commit_instr,
                bail_section=bail_section,
            )
            ctx.client.run(
                fix_prompt,
                label=f"verify-fix-{attempt}",
                model=options.fix_model,
                raw_path=ctx.session_dir / f"stream-verify-{attempt}.jsonl",
                cwd=ctx.worktree,
            )
            _agent_bailed = True
            check_bail(ctx.gr_id, f"verify-fix-{attempt}", child_key=ctx.child_key)
            _agent_bailed = False

        _exhausted = True
        emit_bail(
            ctx.gr_id,
            "other",
            f"verify failed after {options.max_attempts} attempts",
            child_key=ctx.child_key,
        )
        raise RuntimeError(f"verify stage exhausted {options.max_attempts} attempts")
    except (SystemExit, Exception) as exc:
        if not _exhausted and not _agent_bailed:
            emit_bail(
                ctx.gr_id,
                "other",
                f"verify stage failed: {exc}"[:200],
                child_key=ctx.child_key,
            )
        raise


register_stage("verify", run)
