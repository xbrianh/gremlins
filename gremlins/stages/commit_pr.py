"""Combined commit-and-open-PR stage for overlay pipelines."""

from __future__ import annotations

from typing import Any

from gremlins.stages import Stage, register_stage


class CommitPR(Stage):
    """Registry placeholder; execution handled by GHPipeline._make_runner."""

    def run(self, pipe: Any) -> None:  # pragma: no cover
        raise NotImplementedError


register_stage("commit-pr", CommitPR)
