"""RunCmd stage — run a list of shell commands; raise RunCmdFailed on non-zero exit."""

from __future__ import annotations

import subprocess

from gremlins.stages.base import Stage, StageState
from gremlins.stages.loop import RunCmdFailed
from gremlins.stages.registry import register_stage


class RunCmd(Stage):
    def run(self, state: StageState) -> None:
        cmds = [c for c in self.options.get("cmds", []) if c.strip()]
        if not cmds:
            return
        combined = " && ".join(cmds)
        result = subprocess.run(
            combined,
            shell=True,
            cwd=state.cwd,
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            output = result.stdout + result.stderr
            log_path = state.session_dir / "run-cmd.log"
            log_path.write_text(output, encoding="utf-8")
            raise RunCmdFailed(output)


register_stage("run-cmd", RunCmd)
