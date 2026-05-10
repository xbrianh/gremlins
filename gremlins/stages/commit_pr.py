"""Combined commit-and-open-PR stage for overlay pipelines."""

from __future__ import annotations

from gremlins.stages.base import Stage, StageState
from gremlins.stages.registry import register_stage


class CommitPR(Stage):
    """Registry placeholder; execution handled by GHPipeline._make_runner."""

    def run(self, state: StageState) -> None:  # noqa: ARG002  # pragma: no cover
        raise NotImplementedError


register_stage("commit-pr", CommitPR)
