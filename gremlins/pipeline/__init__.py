from __future__ import annotations

import dataclasses
import importlib
import pathlib
from typing import TYPE_CHECKING, Any, cast

from gremlins.clients.client import PACKAGE_DEFAULT, Client

if TYPE_CHECKING:
    from gremlins.pipeline.inputs import InputSources
    from gremlins.stages.base import Stage
    from gremlins.stages.exec import Exec

GREMLINS_PREFIX = "gremlins:"


def _fill_stage_clients(stages: list[Stage], default: Client) -> None:
    for stage in stages:
        if stage.type != "parallel":
            stage.client = stage.client or default
        body = getattr(stage, "body", [])
        if body:
            _fill_stage_clients(body, default)


@dataclasses.dataclass
class Pipeline:
    name: str
    path: pathlib.Path
    stages: list[Stage]
    default_client: Client | None = None
    base_ref: str = "current"
    inputs: Exec | None = None
    input_sources: InputSources | None = None
    land: Exec | None = None
    github_integration: bool = False

    def uses_loop_handoff(self) -> bool:
        first = self.stages[0] if self.stages else None
        return (
            first is not None
            and first.type == "loop"
            and any(b.name == "handoff" for b in (first.body or []))
        )

    @classmethod
    def from_yaml(cls, path: pathlib.Path) -> Pipeline:
        importlib.import_module("gremlins.clients")

        from gremlins.pipeline.loader import check_duplicate_producers, parse_stages
        from gremlins.pipeline.preprocess import expand_pipeline

        path = path.resolve()
        if not path.exists():
            raise FileNotFoundError(f"pipeline file not found: {path}")

        raw = expand_pipeline(path)
        pipeline_name = path.stem

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

        github_integration = bool(raw.get("github_integration", False))

        from gremlins.pipeline.inputs import InputSources
        from gremlins.stages.exec import Exec

        stages = parse_stages(cast(list[dict[str, Any]], raw.get("stages") or []))

        inputs_stage: Exec | None = None
        input_sources: InputSources | None = None
        inputs_raw = raw.get("inputs")
        if inputs_raw is not None:
            if not isinstance(inputs_raw, dict):
                raise ValueError("'inputs' must be a mapping")
            inputs_raw = cast(dict[str, Any], inputs_raw)
            sources_raw = inputs_raw.get("sources")
            if sources_raw is not None:
                if not isinstance(sources_raw, dict):
                    raise ValueError("'inputs.sources' must be a mapping")
                input_sources = InputSources.from_yaml(
                    cast(dict[str, Any], sources_raw)
                )
            inputs_stage = Exec.with_dict({"name": "inputs", **inputs_raw})

        land_stage: Exec | None = None
        land_raw = raw.get("land")
        if land_raw is not None:
            if not isinstance(land_raw, dict):
                raise ValueError("'land' must be a mapping")
            land_stage = Exec.with_dict({"name": "land", **land_raw})

        if inputs_stage is not None:
            stages = [inputs_stage, *stages]

        check_duplicate_producers(stages)

        _fill_stage_clients(stages, default_client or PACKAGE_DEFAULT)

        return cls(
            name=pipeline_name,
            path=path,
            stages=stages,
            default_client=default_client,
            base_ref=pipeline_base_ref,
            inputs=inputs_stage,
            input_sources=input_sources,
            land=land_stage,
            github_integration=github_integration,
        )
