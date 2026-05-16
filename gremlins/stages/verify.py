"""Verify stage — runs cmds joined with &&; used by both gh and local pipelines."""

from __future__ import annotations

import logging
import pathlib
from typing import Any

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.cmd import Cmd
from gremlins.stages.loop import LoopStage
from gremlins.stages.outcome import Bail, Done, NeedsFix, Outcome
from gremlins.utils import git as _git_mod

logger = logging.getLogger(__name__)


def _diff_text(cwd: pathlib.Path) -> str:
    try:
        unstaged = _git_mod.diff_output(cwd=cwd)
        staged = _git_mod.diff_output(["--cached"], cwd=cwd)
        return (unstaged + staged).strip()
    except Exception:
        return ""


class Verify(Stage):
    type = "verify"

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> Verify:
        from gremlins.pipeline.loader import get_client_from_dict

        stage = cls(d["name"], None, d.get("prompt") or [], d.get("options") or {})
        stage.client = get_client_from_dict(d)
        return stage

    def run(self, state: State) -> Outcome:
        session_dir = state.session_dir
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

        template = "\n\n".join(self.prompts).rstrip()
        commands_section = "**Commands run:**\n" + "\n".join(f"- `{c}`" for c in cmds)

        cmd_stage = Cmd(
            "verify-cmd",
            None,
            [],
            {"cmds": cmds, "log_path": "verify-attempt-{n}.log"},
        )

        def _run_cmd() -> Outcome:
            outcome = cmd_stage.run(state)
            n = cmd_stage.n
            if isinstance(outcome, NeedsFix):
                logger.info("verify attempt %d: failed", n)
            else:
                logger.info("verify attempt %d: green", n)
            return outcome

        def _run_fix() -> Outcome:
            n = cmd_stage.n
            log_text = (session_dir / f"verify-attempt-{n}.log").read_text(
                encoding="utf-8"
            )
            diff = _diff_text(state.cwd)
            fix_prompt = template.format(
                bail_command=self.bail_command(state),
                commands_section=commands_section,
                verify_output=log_text,
                diff_text=diff,
            )
            self.run_claude(
                fix_prompt,
                state=state,
                label=f"verify-fix-{n}",
                raw_path=session_dir / f"stream-verify-{n}.jsonl",
            )
            try:
                state.data.check_bail(f"verify-fix-{n}", child_key=state.child_key)
            except RuntimeError as exc:
                return Bail(str(exc))
            return Done()

        loop = LoopStage.from_runners(
            [_run_cmd, _run_fix], name="verify", max_iterations=max_attempts
        )
        return loop.run(state)
