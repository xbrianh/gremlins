from __future__ import annotations

import importlib
import pathlib
from typing import Any, cast

from gremlins.clients.client import Client
from gremlins.pipeline.preprocess import expand_pipeline
from gremlins.schema import PipelineDef
from gremlins.stages.base import Stage
from gremlins.stages.registry import STAGE_REGISTRY


def _ensure_registered() -> None:
    importlib.import_module("gremlins.stages.all")
    importlib.import_module("gremlins.clients")


def get_client_from_yaml(d: dict[str, Any]) -> Client | None:
    raw = d.get("client")
    if raw is None:
        return None
    if not isinstance(raw, str):
        raise ValueError(
            f"stage {d.get('name', '?')!r}: 'client' must be a string, got {type(raw)!r}"
        )
    return Client.parse(raw)


def parse_stage(d: dict[str, Any], depth: int = 0) -> Stage:
    if "parallel" in d:
        from gremlins.stages.parallel import ParallelStage

        return ParallelStage.from_yaml(d, depth=depth)

    name = d.get("name")
    if not isinstance(name, str) or not name:
        raise ValueError("stage entry must have a 'name' field")
    if "max_concurrent" in d:
        raise ValueError(
            f"stage {name!r}: 'max_concurrent' is only valid on parallel groups"
        )
    stage_type = d.get("type")
    if not isinstance(stage_type, str) or not stage_type:
        raise ValueError(f"stage {name!r}: must have a 'type' field")
    if stage_type not in STAGE_REGISTRY:
        raise ValueError(f"stage {name!r}: unknown type {stage_type!r}")
    return STAGE_REGISTRY[stage_type].from_yaml(d)


def load_pipeline(path: pathlib.Path) -> PipelineDef:
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

    return PipelineDef(
        name=pipeline_name,
        path=path,
        stages=stages,
        default_client=default_client,
        base_ref=pipeline_base_ref,
    )
