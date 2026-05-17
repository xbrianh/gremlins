"""Verify stage — runs cmds joined with &&; used by both gh and local pipelines."""

from __future__ import annotations

import logging
import pathlib
import re
from typing import Any

from gremlins.executor.state import State
from gremlins.stages.agent import bail_command, run_agent
from gremlins.stages.base import Stage
from gremlins.stages.cmd import Cmd
from gremlins.stages.loop import LoopStage
from gremlins.stages.outcome import Done, Outcome
from gremlins.utils import git as _git_mod

logger = logging.getLogger(__name__)


def _diff_text(cwd: pathlib.Path) -> str:
    try:
        unstaged = _git_mod.diff_output(cwd=cwd)
        staged = _git_mod.diff_output(["--cached"], cwd=cwd)
        return (unstaged + staged).strip()
    except Exception:
        return ""


def _latest_verify_log(session_dir: pathlib.Path) -> tuple[pathlib.Path, int] | None:
    best: tuple[int, pathlib.Path] | None = None
    for p in session_dir.glob("verify-attempt-*.log"):
        m = re.fullmatch(r"verify-attempt-(\d+)\.log", p.name)
        if m:
            n = int(m.group(1))
            if best is None or n > best[0]:
                best = (n, p)
    return (best[1], best[0]) if best else None


class VerifyFix(Stage):
    """Reads the latest verify-attempt-N.log and runs the fix agent."""

    type = "verify-fix"

    def __init__(self, name: str, prompts: list[str], commands_section: str) -> None:
        super().__init__(name)
        self.prompts = prompts
        self._commands_section = commands_section

    def run(self, state: State) -> Outcome:
        result = _latest_verify_log(state.session_dir)
        if result is None:
            return Done()
        log_path, n = result
        log_text = log_path.read_text(encoding="utf-8")
        diff = _diff_text(state.cwd)
        template = "\n\n".join(self.prompts).rstrip()
        fix_prompt = template.format(
            bail_command=bail_command(state),
            commands_section=self._commands_section,
            verify_output=log_text,
            diff_text=diff,
        )
        run_agent(
            state,
            fix_prompt,
            label=f"verify-fix-{n}",
            raw_path=state.session_dir / f"stream-verify-{n}.jsonl",
        )
        return Done()


class Verify(Stage):
    type = "verify"

    def __init__(self, name: str, prompts: list[str], options: dict[str, Any]) -> None:
        super().__init__(name)
        self.prompts = prompts
        self.options = options

    def run(self, state: State) -> Outcome:
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
            return Done()

        commands_section = "**Commands run:**\n" + "\n".join(f"- `{c}`" for c in cmds)
        cmd_stage = Cmd(
            "cmd", [], {"cmds": cmds, "log_path": "verify-attempt-{n}.log"}
        )
        fix_stage = VerifyFix("fix", self.prompts, commands_section)
        return LoopStage(
            "verify", body=[cmd_stage, fix_stage], max_iterations=max_attempts
        ).run(state)
