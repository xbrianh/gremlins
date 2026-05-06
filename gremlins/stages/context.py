"""Shared runtime context passed to every stage."""

from __future__ import annotations

import dataclasses
import pathlib

from gremlins.clients.protocol import ClaudeClient


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
