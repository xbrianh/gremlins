from __future__ import annotations

import dataclasses
import pathlib
from typing import Any

from gremlins.clients.resolve import ClientSpec


def _empty_stage_list() -> list[StageEntry]:
    return []


@dataclasses.dataclass
class StageEntry:
    name: str
    type: str
    client: ClientSpec | None
    prompts: list[str]
    options: dict[str, Any]
    body: list[StageEntry] = dataclasses.field(default_factory=_empty_stage_list)
    max_concurrent: int | None = None
    cancel_on_bail: bool = False
    bail_policy: str = "any"


@dataclasses.dataclass
class PipelineDef:
    name: str
    path: pathlib.Path
    stages: list[StageEntry]
    default_client: ClientSpec | None = None
    base_ref: str = "current"
