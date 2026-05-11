"""Combined commit-and-open-PR stage for overlay pipelines."""

from __future__ import annotations

from typing import Any

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage


class CommitPR(Stage):
    """Registry placeholder; execution handled by GHPipeline._make_runner."""

    type = "commit-pr"

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> CommitPR:
        from gremlins.pipeline.loader import get_client_from_dict

        stage = cls(d["name"], None, d.get("prompt") or [], d.get("options") or {})
        stage.client = get_client_from_dict(d)
        return stage

    def run(self, state: State) -> None:  # noqa: ARG002  # pragma: no cover
        raise NotImplementedError


register_stage("commit-pr", CommitPR)
