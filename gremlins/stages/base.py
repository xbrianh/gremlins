from __future__ import annotations

import dataclasses
import pathlib
import shlex
import subprocess
from typing import TYPE_CHECKING, Any, cast

from gremlins.clients.protocol import ClaudeClient, CompletedRun

if TYPE_CHECKING:
    from gremlins.pipeline import StageEntry


@dataclasses.dataclass
class StageContext:
    client: ClaudeClient
    session_dir: pathlib.Path
    gr_id: str | None
    child_key: str | None = None
    worktree: pathlib.Path | None = None

    @property
    def cwd(self) -> pathlib.Path:
        return self.worktree if self.worktree is not None else pathlib.Path.cwd()


class Stage:
    def __init__(self, entry: StageEntry, model: str | None) -> None:
        self.name = entry.name
        self.model = model
        self.prompt_paths = entry.prompt_paths
        self.options = entry.options
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

    def run(self, pipe: Any) -> Any:
        raise NotImplementedError
