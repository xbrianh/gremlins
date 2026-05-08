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
from gremlins.stages.loop import LoopExhausted, LoopStage, RunCmdFailed
from gremlins.stages.registry import register_stage
from gremlins.state import check_bail

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
    ) -> None:
        super().__init__(entry, model)
        self._is_git = is_git

    def run(self, pipe: Any) -> None:
        cmds = [c for c in self.options.get("cmds", []) if c.strip()]
        max_attempts = self.options.get("max_attempts", 3)

        if not cmds:
            logger.info("verify: no cmds configured; skipping")
            return

        if self.options.get("commit_after_fix", True) and self._is_git:
            commit_instr = (
                "- After fixing, stage the changed files by name and create a single git "
                "commit titled 'Fix failing checks'. Do not push."
            )
        else:
            commit_instr = (
                "- After fixing, leave changes uncommitted — do not stage or commit. "
                "The next stage (commit) will handle staging and committing."
            )

        template = load_prompts(self.prompt_paths)
        combined_cmd = " && ".join(cmds)
        commands_section = "**Commands run:**\n" + "\n".join(f"- `{c}`" for c in cmds)

        state = self.state
        is_git = self._is_git
        attempt: list[int] = [0]
        last_output: list[str | None] = [None]

        def _run_cmd() -> None:
            attempt[0] += 1
            n = attempt[0]
            log_file = state.session_dir / f"verify-attempt-{n}.log"
            result = subprocess.run(
                combined_cmd,
                shell=True,
                cwd=state.cwd,
                capture_output=True,
                text=True,
            )
            log_file.write_text(result.stdout + result.stderr, encoding="utf-8")
            if result.returncode != 0:
                logger.info("verify attempt %d: failed (exit %d)", n, result.returncode)
                last_output[0] = log_file.read_text(encoding="utf-8")
                raise RunCmdFailed(result.returncode)
            logger.info("verify attempt %d: green", n)
            last_output[0] = None

        def _run_fix() -> None:
            if last_output[0] is None:
                return
            n = attempt[0]
            diff = _diff_text(state.cwd, is_git=is_git)
            fix_prompt = template.format(
                bail_command=self.bail_command(),
                commands_section=commands_section,
                verify_output=last_output[0],
                diff_text=diff,
                commit_instr=commit_instr,
            )
            self.run_claude(
                fix_prompt,
                label=f"verify-fix-{n}",
                raw_path=state.session_dir / f"stream-verify-{n}.jsonl",
            )
            check_bail(state.gr_id, f"verify-fix-{n}", child_key=state.child_key)

        loop = LoopStage.from_runners([_run_cmd, _run_fix], max_iterations=max_attempts)
        loop.bind(state)
        try:
            loop.run(pipe)
        except LoopExhausted:
            raise RuntimeError(f"verify stage exhausted {max_attempts} attempts")


register_stage("verify", Verify)
