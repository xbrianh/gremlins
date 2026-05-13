from __future__ import annotations

import pathlib
import shlex
import subprocess
from typing import Any, NamedTuple, cast

from gremlins.clients.client import Client
from gremlins.clients.protocol import CompletedRun
from gremlins.executor.state import State


class StageInput(NamedTuple):
    name: str
    type: type
    required: bool
    default: Any
    help: str


class Stage:
    type: str = ""
    needs_gh: bool = False

    def __init__(
        self, name: str, model: str | None, prompts: list[str], options: dict[str, Any]
    ) -> None:
        self.name = name
        self.model = model
        self.prompts = prompts
        self.options = options
        self.client: Client | None = None
        self.body: list[Stage] = []

    def run_claude(
        self,
        prompt: str,
        *,
        state: State,
        label: str,
        raw_path: pathlib.Path | None = None,
        **kw: Any,
    ) -> CompletedRun:
        assert state.client is not None
        model = self.model or state.client.model
        extra_env: dict[str, str] = {}
        if state.attempt and state.state_file is not None:
            extra_env["GREMLIN_ATTEMPT"] = state.attempt
            extra_env["GREMLIN_STATE_DIR"] = str(state.state_file.parent)
        return state.client.run(
            prompt,
            label=label,
            model=model,
            raw_path=raw_path,
            cwd=state.worktree,
            extra_env=extra_env or None,
            **kw,
        )

    def bail_command(self, state: State) -> str:
        script = (
            "import sys,json,os,pathlib; "
            "d=pathlib.Path(os.environ['GREMLIN_STATE_DIR']); "
            "a=os.environ['GREMLIN_ATTEMPT']; "
            "p=d/f'bail_{a}.json'; "
            "p.exists() or p.write_text(json.dumps({'class':sys.argv[1],'detail':sys.argv[2] if len(sys.argv)>2 else ''}))"
        )
        return f"python -c {shlex.quote(script)}"

    def run_subprocess(
        self, argv: list[str], state: State, **kw: Any
    ) -> subprocess.CompletedProcess[Any]:
        kw.setdefault("cwd", str(state.cwd))
        return cast(subprocess.CompletedProcess[Any], subprocess.run(argv, **kw))

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> Stage:
        raise NotImplementedError

    @classmethod
    def orchestration_args(cls) -> list[StageInput]:
        return []

    def run(self, state: State) -> Any:  # noqa: ARG002
        raise NotImplementedError
