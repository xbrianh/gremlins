"""RunCmd stage — run a list of shell commands; raise RunCmdFailed on non-zero exit."""

from __future__ import annotations

import subprocess
from typing import Any

from gremlins.stages import RunCmdFailed, Stage, register_stage


class RunCmd(Stage):
    def run(self, pipe: Any) -> None:  # noqa: ARG002
        cmds = [c for c in self.options.get("cmds", []) if c.strip()]
        if not cmds:
            return
        combined = " && ".join(cmds)
        result = subprocess.run(
            combined,
            shell=True,
            cwd=self.state.cwd,
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            output = result.stdout + result.stderr
            log_path = self.state.session_dir / "run-cmd.log"
            log_path.write_text(output, encoding="utf-8")
            raise RunCmdFailed(output)


register_stage("run-cmd", RunCmd)
