from __future__ import annotations

import dataclasses
import importlib
import pathlib
from typing import TYPE_CHECKING, Any, cast

from gremlins.clients.client import PACKAGE_DEFAULT, Client

if TYPE_CHECKING:
    from gremlins.stages.base import Stage

BUNDLED_PROMPT_PREFIX = "gremlins:"


def _fill_stage_clients(stages: list[Stage], default: Client) -> None:
    for stage in stages:
        if stage.type != "parallel":
            stage.client = stage.client or default
        if stage.body:
            _fill_stage_clients(stage.body, default)


def _stages_need_gh(stages: list[Stage]) -> bool:
    return any(
        s.needs_gh or (s.body and _stages_need_gh(s.body)) for s in stages
    )


@dataclasses.dataclass
class Pipeline:
    name: str
    path: pathlib.Path
    stages: list[Stage]
    default_client: Client | None = None
    base_ref: str = "current"

    def uses_loop_handoff(self) -> bool:
        first = self.stages[0] if self.stages else None
        return (
            first is not None
            and first.type == "loop"
            and any(b.type == "handoff" for b in (first.body or []))
        )

    def needs_gh(self) -> bool:
        return _stages_need_gh(self.stages)

    def setup_kind(self) -> str:
        if self.needs_gh() or self.uses_loop_handoff():
            return "worktree-detached"
        return "worktree-branch"

    @classmethod
    def from_yaml(cls, path: pathlib.Path) -> Pipeline:
        importlib.import_module("gremlins.clients")

        from gremlins.pipeline.loader import parse_stage
        from gremlins.pipeline.preprocess import expand_pipeline

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

        _fill_stage_clients(stages, default_client or PACKAGE_DEFAULT)

        return cls(
            name=pipeline_name,
            path=path,
            stages=stages,
            default_client=default_client,
            base_ref=pipeline_base_ref,
        )
