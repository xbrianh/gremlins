from __future__ import annotations

import argparse
import dataclasses
import pathlib
import shlex
import subprocess
from collections.abc import Callable
from typing import TYPE_CHECKING, Any, NamedTuple, Protocol, cast

from gremlins.clients.protocol import ClaudeClient, CompletedRun

if TYPE_CHECKING:
    from gremlins.clients import ClientSpec
    from gremlins.schema import PipelineDef, StageEntry


class StageRunner(Protocol):
    args: argparse.Namespace
    session_dir: pathlib.Path
    gr_id: str | None
    is_git: bool
    pipeline_data: PipelineDef
    repo: str
    state_file: pathlib.Path | None
    stage_specs: dict[str, ClientSpec]
    instructions: str
    current_scope: list[StageEntry]

    def get_client(self, spec: ClientSpec) -> ClaudeClient: ...

    def make_runner(
        self,
        entry: StageEntry,
        ctx: StageContext,
        spec: ClientSpec,
        scope: list[StageEntry] | None = None,
    ) -> Callable[[], None]: ...


class StageInput(NamedTuple):
    name: str
    type: type
    required: bool
    default: Any
    help: str


@dataclasses.dataclass
class StageContext:
    client: ClaudeClient
    session_dir: pathlib.Path
    gr_id: str | None
    child_key: str | None = None
    worktree: pathlib.Path | None = None

    @property
    def cwd(self) -> pathlib.Path:
        """Working directory for stage subprocess work.

        Defaults to the process cwd; parallel children get an isolated git
        worktree set on ``worktree`` so their subprocess calls (``claude -p``,
        ``git status``, verify ``cmds``, …) operate on the worktree.
        """
        return self.worktree if self.worktree is not None else pathlib.Path.cwd()


class Stage:
    def __init__(
        self, name: str, model: str | None, prompts: list[str], options: dict[str, Any]
    ) -> None:
        self.name = name
        self.model = model
        self.prompts = prompts
        self.options = options
        self._mutable_state: StageContext | None = None

    def bind(self, state: StageContext) -> None:
        self._mutable_state = state

    @property
    def state(self) -> StageContext:
        if self._mutable_state is None:
            raise RuntimeError(f"stage {self.name!r} not bound")
        return self._mutable_state

    def run_claude(
        self,
        prompt: str,
        *,
        label: str,
        raw_path: pathlib.Path | None = None,
        **kw: Any,
    ) -> CompletedRun:
        return self.state.client.run(
            prompt,
            label=label,
            model=self.model,
            raw_path=raw_path,
            cwd=self.state.worktree,
            **kw,
        )

    def bail_command(self) -> str:
        command = ["python", "-m", "gremlins.bail"]
        if self.state.child_key:
            command.extend(["--child-key", self.state.child_key])
        return shlex.join(command)

    def run_subprocess(
        self, argv: list[str], **kw: Any
    ) -> subprocess.CompletedProcess[Any]:
        kw.setdefault("cwd", str(self.state.cwd))
        return cast(subprocess.CompletedProcess[Any], subprocess.run(argv, **kw))

    @classmethod
    def orchestration_args(cls) -> list[StageInput]:
        return []

    def run(self, pipe: Any) -> Any:
        raise NotImplementedError
