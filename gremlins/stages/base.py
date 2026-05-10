from __future__ import annotations

import argparse
import dataclasses
import pathlib
import shlex
import subprocess
from collections.abc import Callable
from typing import TYPE_CHECKING, Any, NamedTuple, Protocol, cast

from gremlins.clients.client import Client
from gremlins.clients.protocol import CompletedRun
from gremlins.schema import RetryConfig

if TYPE_CHECKING:
    from gremlins.schema import PipelineDef, StageEntry


class StageRunner(Protocol):
    args: argparse.Namespace
    session_dir: pathlib.Path
    gr_id: str | None
    is_git: bool
    pipeline_data: PipelineDef
    repo: str
    state_file: pathlib.Path | None
    stage_specs: dict[str, Client]
    instructions: str
    current_scope: list[StageEntry]

    def get_client(self, spec: Client) -> Client: ...

    def make_runner(
        self,
        entry: StageEntry,
        ctx: StageContext,
        spec: Client,
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
    client: Client
    session_dir: pathlib.Path
    gr_id: str | None
    child_key: str | None = None
    worktree: pathlib.Path | None = None
    pipeline_retry: RetryConfig | None = None

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

    def _resolve_retry(self) -> dict[str, Any]:
        stage_retry = self.options.get("retry") or {}
        pr = self.state.pipeline_retry

        idle_timeout = stage_retry.get("idle_timeout")
        if idle_timeout is None and pr is not None:
            idle_timeout = pr.idle_timeout

        backoff = stage_retry.get("backoff")
        if backoff is None and pr is not None:
            backoff = pr.backoff

        out: dict[str, Any] = {}
        if idle_timeout is not None:
            out["idle_timeout"] = float(idle_timeout)
        if backoff is not None:
            out["backoff"] = list(backoff)
        return out

    def run_claude(
        self,
        prompt: str,
        *,
        label: str,
        raw_path: pathlib.Path | None = None,
        **kw: Any,
    ) -> CompletedRun:
        retry = self._resolve_retry()
        retry.update(kw)
        return self.state.client.run(
            prompt,
            label=label,
            model=self.model,
            raw_path=raw_path,
            cwd=self.state.worktree,
            **retry,
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
