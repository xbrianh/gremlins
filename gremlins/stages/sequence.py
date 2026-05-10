"""SequenceStage: run body stages in order, propagating parent state fields.

Each sub-stage's RuntimeState receives the parent's ``worktree``, ``child_key``,
and ``session_dir`` before its runner is invoked, so a SequenceStage used as a
parallel child correctly inherits the shard-specific worktree and session
directory and any bails route through ``parallel_bails[child_key]``.
"""

from __future__ import annotations

from collections.abc import Callable

from gremlins.stages.base import RuntimeState, Stage
from gremlins.stages.registry import register_stage


class SequenceStage(Stage):
    """Run body runners sequentially, propagating worktree/child_key/session_dir."""

    def __init__(
        self,
        name: str,
        *,
        body: list[tuple[RuntimeState, Callable[[], None]]],
    ) -> None:
        super().__init__(name, None, [], {})
        self._body = body

    def run(self, state: RuntimeState) -> None:
        for sub_state, runner in self._body:
            sub_state.worktree = state.worktree
            sub_state.child_key = state.child_key
            sub_state.session_dir = state.session_dir
            runner()


register_stage("sequence", SequenceStage)
