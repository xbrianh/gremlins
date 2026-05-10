"""Verify stage — runs cmds joined with &&; used by both gh and local pipelines."""

from __future__ import annotations

import logging
import pathlib
import subprocess
from typing import Any

from gremlins import git as _git_mod
from gremlins.stages.base import RuntimeState, Stage
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
    type = "verify"

    @classmethod
    def from_yaml(cls, d: dict[str, Any], depth: int = 0) -> Verify:
        from gremlins.pipeline.loader import get_client_from_yaml

        stage = cls(d["name"], None, d.get("prompt") or [], d.get("options") or {})
        stage.client = get_client_from_yaml(d)
        return stage

    def __init__(
        self,
        name: str,
        model: str | None,
        prompts: list[str],
        options: dict[str, Any],
    ) -> None:
        super().__init__(name, model, prompts, options)

    def run(self, state: RuntimeState) -> None:
        options = dict(self.options)
        if not state.repo:
            cmds_arg = getattr(state.args, "cmds", None)
            if cmds_arg is not None:
                options["cmds"] = cmds_arg
            options.setdefault(
                "max_attempts", getattr(state.args, "test_max_attempts", 3)
            )
        cmds = [c for c in options.get("cmds", []) if c.strip()]
        max_attempts = options.get("max_attempts", 3)

        if not cmds:
            logger.info("verify: no cmds configured; skipping")
            return

        is_git = state.is_git
        if options.get("commit_after_fix", True) and is_git:
            commit_instr = (
                "- After fixing, stage the changed files by name and create a single git "
                "commit titled 'Fix failing checks'. Do not push."
            )
        else:
            commit_instr = (
                "- After fixing, leave changes uncommitted — do not stage or commit. "
                "The next stage (commit) will handle staging and committing."
            )

        template = "\n\n".join(self.prompts).rstrip()
        combined_cmd = " && ".join(cmds)
        commands_section = "**Commands run:**\n" + "\n".join(f"- `{c}`" for c in cmds)
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
            output = result.stdout + result.stderr
            log_file.write_text(output, encoding="utf-8")
            if result.returncode != 0:
                logger.info("verify attempt %d: failed (exit %d)", n, result.returncode)
                last_output[0] = output
                raise RunCmdFailed(output)
            logger.info("verify attempt %d: green", n)
            last_output[0] = None

        def _run_fix() -> None:
            if last_output[0] is None:
                return
            n = attempt[0]
            diff = _diff_text(state.cwd, is_git=is_git)
            fix_prompt = template.format(
                bail_command=self.bail_command(state),
                commands_section=commands_section,
                verify_output=last_output[0],
                diff_text=diff,
                commit_instr=commit_instr,
            )
            self.run_claude(
                fix_prompt,
                state=state,
                label=f"verify-fix-{n}",
                raw_path=state.session_dir / f"stream-verify-{n}.jsonl",
            )
            check_bail(state.gr_id, f"verify-fix-{n}", child_key=state.child_key)

        loop = LoopStage.from_runners(
            [_run_cmd, _run_fix], name="verify", max_iterations=max_attempts
        )
        try:
            loop.run(state)
        except LoopExhausted:
            raise RuntimeError(f"verify stage exhausted {max_attempts} attempts")


register_stage("verify", Verify)
