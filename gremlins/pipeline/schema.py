from __future__ import annotations

import dataclasses
import pathlib
from typing import Any

from gremlins.clients import ClientSpec

BUNDLED_PROMPT_PREFIX = "gremlins:"


@dataclasses.dataclass
class StageEntry:
    name: str
    type: str
    client: ClientSpec | None
    prompt_paths: list[pathlib.Path]
    options: dict[str, Any]
    children: list[StageEntry] = dataclasses.field(  # pyright: ignore[reportUnknownVariableType]
        default_factory=list
    )
    max_concurrent: int | None = None
    cancel_on_bail: bool = False
    bail_policy: str = "any"


@dataclasses.dataclass
class Pipeline:
    name: str
    path: pathlib.Path
    stages: list[StageEntry]
    default_client: ClientSpec | None = None
