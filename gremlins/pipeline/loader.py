from __future__ import annotations

from typing import Any

from gremlins.clients.client import Client
from gremlins.stages.base import Stage
from gremlins.stages.registry import STAGE_REGISTRY


def get_client_from_dict(d: dict[str, Any]) -> Client | None:
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

        return ParallelStage.with_dict(d, depth=depth)

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
    return STAGE_REGISTRY[stage_type].with_dict(d, depth=depth)
