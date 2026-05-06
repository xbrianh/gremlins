"""Verify stage — runs cmds joined with &&; used by both gh and local pipelines."""

from __future__ import annotations

import logging
import pathlib
import subprocess
from typing import Any

from gremlins import git as _git_mod
from gremlins.pipeline import StageEntry
from gremlins.prompts import load_prompts
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.state import check_bail, emit_bail

logger = logging.getLogger(__name__)


def _diff_text(cwd: pathlib.Path, *, is_git: bool) -> str:
    if not is_git:
        return ""
    try:
        unstaged = _git_mod.diff_output(cwd=cwd)
        staged = _git_mod.diff_output(["--cached"], cwd=cwd)
        return (unstaged + staged).strip()
    except Exception:
        return ""


class Verify(Stage):
    def __init__(
        self,
        entry: StageEntry,
        model: str | None,
        *,
        is_git: bool,
        commit_after_fix: bool,
    ) -> None:
        super().__init__(entry, model)
        self._is_git = is_git
        self._commit_after_fix = commit_after_fix

    def run(self, pipe: Any) -> None:
        cmds = [c for c in self.options.get("cmds", []) if c.strip()]
        max_attempts = self.options.get("max_attempts", 3)

        if not cmds:
            logger.info("verify: no cmds configured; skipping")
            return

        if self._commit_after_fix and self._is_git:
            commit_instr = (
                "- After fixing, stage the changed files by name and create a single git "
                "commit titled 'Fix failing checks'. Do not push."
            )
        else:
            commit_instr = (
                "- After fixing, leave changes uncommitted — do not stage or commit. "
                "The next stage (commit-pr) will handle staging and committing."
            )

        template = load_prompts(self.prompt_paths)
        combined_cmd = " && ".join(cmds)
        commands_section = "**Commands run:**\n" + "\n".join(f"- `{c}`" for c in cmds)

        _exhausted = False
        _agent_bailed = False
        try:
            for attempt in range(1, max_attempts + 1):
                log_file = self.state.session_dir / f"verify-attempt-{attempt}.log"
                result = subprocess.run(
                    combined_cmd,
                    shell=True,
                    cwd=self.state.cwd,
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

                if attempt == max_attempts:
                    break

                diff = _diff_text(self.state.cwd, is_git=self._is_git)
                verify_output = log_file.read_text(encoding="utf-8")
                fix_prompt = template.format(
                    bail_command=self.bail_command(),
                    commands_section=commands_section,
                    verify_output=verify_output,
                    diff_text=diff,
                    commit_instr=commit_instr,
                )
                self.run_claude(
                    fix_prompt,
                    label=f"verify-fix-{attempt}",
                    raw_path=self.state.session_dir / f"stream-verify-{attempt}.jsonl",
                )
                _agent_bailed = True
                check_bail(
                    self.state.gr_id,
                    f"verify-fix-{attempt}",
                    child_key=self.state.child_key,
                )
                _agent_bailed = False

            _exhausted = True
            emit_bail(
                self.state.gr_id,
                "other",
                f"verify failed after {max_attempts} attempts",
                child_key=self.state.child_key,
            )
            raise RuntimeError(f"verify stage exhausted {max_attempts} attempts")
        except (SystemExit, Exception) as exc:
            if not _exhausted and not _agent_bailed:
                emit_bail(
                    self.state.gr_id,
                    "other",
                    f"verify stage failed: {exc}"[:200],
                    child_key=self.state.child_key,
                )
            raise


register_stage("verify", Verify)
