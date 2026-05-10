from __future__ import annotations

import dataclasses
import pathlib

from gremlins.clients.client import Client
from gremlins.stages.base import Stage


@dataclasses.dataclass
class PipelineDef:
    name: str
    path: pathlib.Path
    stages: list[Stage]
    default_client: Client | None = None
    base_ref: str = "current"
