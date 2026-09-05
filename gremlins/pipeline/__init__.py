from __future__ import annotations

import dataclasses
import pathlib
from typing import TYPE_CHECKING

from _gremlins_core.clients import RustClient as Client

from gremlins.pipeline.bootstrap import Bootstrap

if TYPE_CHECKING:
    from gremlins.stages.base import Stage
    from gremlins.stages.exec import Exec

GREMLINS_PREFIX = "gremlins:"


def _fill_stage_clients(stages: list[Stage], default: Client) -> None:
    for stage in stages:
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
    bootstrap: Bootstrap = dataclasses.field(default_factory=Bootstrap)
    land: Exec | None = None

    def uses_loop_handoff(self) -> bool:
        first = self.stages[0] if self.stages else None
        return (
            first is not None
            and first.type == "loop"
            and any(b.name == "handoff" for b in (first.body or []))
        )

    @classmethod
    def from_yaml(
        cls, path: pathlib.Path, *, default_client_override: str | None = None
    ) -> Pipeline:
        from _gremlins_core.discovery import (
            resolve_pipeline_name as _resolve_pipeline_name,
        )
        from _gremlins_core.schemas import (
            check_duplicate_producers,
            parse_stages,
        )
        from _gremlins_core.schemas import (
            expand_pipeline as _expand_pipeline,
        )

        import gremlins._clients_init  # noqa: F401  # pyright: ignore[reportUnusedImport] — registers built-in providers
        from gremlins.pipelines import BUNDLED_PIPELINE_DIR
        from gremlins.prompts import BUNDLED_PROMPT_DIR
        from gremlins.recipes import BUNDLED_STAGE_DEF_DIR

        path = path.resolve()
        if not path.exists():
            raise FileNotFoundError(f"pipeline file not found: {path}")

        def _resolve(n, pr):
            return _resolve_pipeline_name(n, pr, BUNDLED_PIPELINE_DIR)

        raw = _expand_pipeline(
            str(path),
            None,
            str(BUNDLED_STAGE_DEF_DIR),
            str(BUNDLED_PROMPT_DIR),
            _resolve,
        )
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

        from gremlins.stages.exec import Exec

        if "inputs" in raw:
            raise ValueError(
                "'inputs' is not a valid pipeline key; declare CLI arguments under bootstrap.source"
            )

        stages = parse_stages(raw.get("stages") or [])
        bootstrap = Bootstrap.from_yaml(raw.get("bootstrap"))

        land_stage: Exec | None = None
        land_raw = raw.get("land")
        if land_raw is not None:
            if not isinstance(land_raw, dict):
                raise ValueError("'land' must be a mapping")
            land_stage = Exec.with_dict({"name": "land", **land_raw})

        check_duplicate_producers(stages, extra_out=bootstrap.cli_out)

        if default_client is None and default_client_override is not None:
            default_client = Client.parse(default_client_override)

        if default_client is None:
            raise ValueError(
                "pipeline is missing 'default_client' — set a 'default_client' in the pipeline "
                "YAML or pass --client on the command line"
            )
        _fill_stage_clients(stages, default_client)

        return cls(
            name=pipeline_name,
            path=path,
            stages=stages,
            default_client=default_client,
            base_ref=pipeline_base_ref,
            bootstrap=bootstrap,
            land=land_stage,
        )
