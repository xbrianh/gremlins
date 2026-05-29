from __future__ import annotations

from typing import Any

from gremlins.stages.agent import Agent
from gremlins.stages.base import Stage
from gremlins.stages.exec import Exec
from gremlins.stages.loop import LoopStage
from gremlins.stages.parallel import ParallelStage
from gremlins.stages.sequence import SequenceStage

STAGE_TYPES: dict[str, type[Stage]] = {
    "agent": Agent,
    "loop": LoopStage,
    "parallel": ParallelStage,
    "sequence": SequenceStage,
    "exec": Exec,
}


def fill_names(raw_stages: list[dict[str, Any]]) -> None:
    """Fill missing 'name' fields in-place; append -N suffix on collisions."""
    explicit: set[str] = {
        d["name"] for d in raw_stages if isinstance(d.get("name"), str) and d["name"]
    }
    used: set[str] = set(explicit)
    counts: dict[str, int] = {}
    for d in raw_stages:
        if isinstance(d.get("name"), str) and d["name"]:
            d.pop("_auto_name", None)
            continue
        auto_raw = d.pop("_auto_name", None)
        auto = str(auto_raw) if auto_raw is not None else None
        stage_type = (auto or "") or (
            "parallel" if "parallel" in d else str(d.get("type") or "")
        )
        counts[stage_type] = counts.get(stage_type, 0) + 1
        n = counts[stage_type]
        candidate = stage_type if n == 1 else f"{stage_type}-{n}"
        while candidate in used:
            n += 1
            candidate = f"{stage_type}-{n}"
        counts[stage_type] = n
        d["name"] = candidate
        used.add(candidate)


def parse_stages(raw: list[dict[str, Any]], depth: int = 0) -> list[Stage]:
    fill_names(raw)
    return [parse_stage(d, depth=depth) for d in raw]


def _parse_skip_if_exists(d: dict[str, Any], name: str) -> str:
    value = d.get("skip_if_exists") or ""
    if value and not isinstance(value, str):
        raise ValueError(
            f"stage {name!r}: 'skip_if_exists' must be a string, got {type(value).__name__!r}"
        )
    return value


def parse_stage(d: dict[str, Any], depth: int = 0) -> Stage:
    if "parallel" in d:
        stage = ParallelStage.with_dict(d, depth=depth)
        stage.raw_dict = d
        stage.skip_if_exists = _parse_skip_if_exists(d, d.get("name") or "<parallel>")
        return stage

    name = d.get("name") or ""
    if "max_concurrent" in d:
        raise ValueError(
            f"stage {name!r}: 'max_concurrent' is only valid on parallel groups"
        )
    stage_type = d.get("type")
    if not isinstance(stage_type, str) or not stage_type:
        raise ValueError(f"stage {name!r}: must have a 'type' field")
    if stage_type not in STAGE_TYPES:
        raise ValueError(f"stage {name!r}: unknown type {stage_type!r}")
    stage = STAGE_TYPES[stage_type].with_dict(d, depth=depth)
    stage.raw_dict = d
    stage.skip_if_exists = _parse_skip_if_exists(d, name)
    return stage
