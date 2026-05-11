"""RunCmd stage — run a list of shell commands; raise RunCmdFailed on non-zero exit."""

from __future__ import annotations

import subprocess
from typing import Any

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.loop import RunCmdFailed
from gremlins.stages.registry import register_stage


class RunCmd(Stage):
    type = "run-cmd"

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> RunCmd:
        from gremlins.pipeline.loader import get_client_from_dict

        stage = cls(d["name"], None, d.get("prompt") or [], d.get("options") or {})
        stage.client = get_client_from_dict(d)
        return stage

    def run(self, state: State) -> None:
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
