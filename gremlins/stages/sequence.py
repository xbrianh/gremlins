"""SequenceStage: run body stages in order, propagating parent context fields.

Each sub-stage's StageContext receives the parent's ``worktree``, ``child_key``,
and ``session_dir`` before its runner is invoked, so a SequenceStage used as a
parallel child correctly inherits the shard-specific worktree and session
directory and any bails route through ``parallel_bails[child_key]``.
"""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

from .base import Stage, StageContext
from .registry import register_stage


class SequenceStage(Stage):
    """Run body runners sequentially, propagating worktree/child_key/session_dir."""

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
            sub_ctx.child_key = self.state.child_key
            sub_ctx.session_dir = self.state.session_dir
            runner()


register_stage("sequence", SequenceStage)
