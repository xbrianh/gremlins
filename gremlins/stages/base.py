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
        model = self.model or state.client.model
        return state.client.run(
            prompt,
            label=label,
            model=model,
            raw_path=raw_path,
            cwd=state.worktree,
            **kw,
        )

    def bail_command(self, state: State) -> str:
        command = ["python", "-m", "gremlins.bail"]
        if state.child_key:
            command.extend(["--child-key", state.child_key])
        return shlex.join(command)

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
