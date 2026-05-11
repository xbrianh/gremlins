from __future__ import annotations

import dataclasses
import pathlib
from typing import TYPE_CHECKING, Any, cast

from gremlins.clients.client import Client
from gremlins.stages.base import Stage

BUNDLED_PROMPT_PREFIX = "gremlins:"


@dataclasses.dataclass
class Pipeline:
    name: str
    path: pathlib.Path
    stages: list[Stage]
    default_client: Client | None = None
    base_ref: str = "current"

    @classmethod
    def from_yaml(cls, path: pathlib.Path) -> Pipeline:
        from gremlins.pipeline.loader import _ensure_registered, parse_stage
        from gremlins.pipeline.preprocess import expand_pipeline

        _ensure_registered()
        path = path.resolve()
        if not path.exists():
            raise FileNotFoundError(f"pipeline file not found: {path}")

        raw = expand_pipeline(path)
        pipeline_name = str(raw.get("name") or path.stem)

        default_client: Client | None = None
        default_client_raw = raw.get("default_client")
        if default_client_raw is not None:
            if not isinstance(default_client_raw, str):
                raise ValueError(
                    f"default_client must be a string, got {type(default_client_raw)!r}"
                )
            default_client = Client.parse(default_client_raw)

        base_ref_raw = raw.get("base_ref")
        if base_ref_raw is not None:
            if not isinstance(base_ref_raw, str) or not base_ref_raw.strip():
                raise ValueError("base_ref must be a non-empty string")
            pipeline_base_ref = base_ref_raw.strip()
        else:
            pipeline_base_ref = "current"

        stages: list[Stage] = []
        for entry in cast(list[dict[str, Any]], raw.get("stages") or []):
            stages.append(parse_stage(entry))

        return cls(
            name=pipeline_name,
            path=path,
            stages=stages,
            default_client=default_client,
            base_ref=pipeline_base_ref,
        )
