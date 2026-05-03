"""Shared runtime context passed to every stage."""

from __future__ import annotations

import dataclasses
import pathlib

from ..clients.claude import ClaudeClient


@dataclasses.dataclass
class StageContext:
    client: ClaudeClient
    session_dir: pathlib.Path
    gr_id: str | None
