from __future__ import annotations

import dataclasses
import pathlib
from typing import Any

from gremlins.clients.client import Client


def _empty_stage_list() -> list[StageEntry]:
    return []


@dataclasses.dataclass
class RetryConfig:
    idle_timeout: float | None = None
    backoff: list[int] | None = None


@dataclasses.dataclass
class StageEntry:
    name: str
    type: str
    client: Client | None
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
    default_client: Client | None = None
    base_ref: str = "current"
    retry: RetryConfig | None = None
