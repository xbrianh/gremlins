from gremlins.pipeline.discovery import (
    BUNDLED_PIPELINE_DIR,
    list_pipelines,
    overlay_dir,
    resolve_pipeline_name,
    resolve_pipeline_path,
)
from gremlins.pipeline.loader import load_pipeline
from gremlins.pipeline.schema import BUNDLED_PROMPT_PREFIX, PipelineDef, StageEntry

_GH_STAGE_TYPES = frozenset(
    {
        "materialize-to-branch",
        "commit",
        "open-github-pr",
        "request-copilot",
        "wait-copilot",
        "ghaddress",
        "ghreview",
        "wait-ci",
    }
)


def pipeline_uses_gh(pipeline: PipelineDef) -> bool:
    return any(s.type in _GH_STAGE_TYPES for s in pipeline.stages)


__all__ = [
    "BUNDLED_PIPELINE_DIR",
    "BUNDLED_PROMPT_PREFIX",
    "PipelineDef",
    "StageEntry",
    "_GH_STAGE_TYPES",
    "list_pipelines",
    "load_pipeline",
    "overlay_dir",
    "pipeline_uses_gh",
    "resolve_pipeline_name",
    "resolve_pipeline_path",
]
