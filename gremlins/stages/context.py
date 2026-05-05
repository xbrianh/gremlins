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
