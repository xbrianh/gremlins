"""SequenceStage: run body stages in order, sharing the parent StageContext."""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

from gremlins.stages.base import Stage, StageContext
from gremlins.stages.registry import register_stage


class SequenceStage(Stage):
    """Run body runners sequentially, propagating the parent worktree to each."""

    def __init__(
        self,
        name: str,
        *,
        body: list[tuple[StageContext, Callable[[], None]]],
    ) -> None:
        super().__init__(name, None, [], {})
        self._body = body

    def run(self, pipe: Any) -> None:  # noqa: ARG002
        for sub_ctx, runner in self._body:
            sub_ctx.worktree = self.state.worktree
            runner()


register_stage("sequence", SequenceStage)
