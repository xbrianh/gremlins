"""SequenceStage: run body stages in order, propagating parent state fields.

Each sub-stage's State receives the parent's ``worktree``, ``child_key``,
and ``session_dir`` before its runner is invoked, so a SequenceStage used as a
parallel child correctly inherits the shard-specific worktree and session
directory and any bails route through ``parallel_bails[child_key]``.
"""

from __future__ import annotations

import dataclasses
from collections.abc import Callable
from typing import Any, cast

from gremlins.executor.state import State
from gremlins.stages.base import Stage


class SequenceStage(Stage):
    """Run body runners sequentially, propagating worktree/child_key/session_dir."""

    type = "sequence"

    def __init__(
        self,
        name: str,
        *,
        body: list[tuple[State, Callable[[], None]]] | None = None,
    ) -> None:
        super().__init__(name, None, [], {})
        self._pre_body = body

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> SequenceStage:
        from gremlins.pipeline.loader import get_client_from_dict, parse_stage

        raw_children: object = d.get("body") or []
        if not isinstance(raw_children, list):
            raise ValueError(f"stage {d['name']!r}: 'body' must be a list")
        children = [
            parse_stage(child_d, depth=depth)
            for child_d in cast(list[dict[str, Any]], raw_children)
        ]
        stage = cls(d["name"])
        stage.body = children
        stage.client = get_client_from_dict(d)
        return stage

    def run(self, state: State) -> None:
        if self._pre_body is not None:
            for sub_state, runner in self._pre_body:
                sub_state.worktree = state.worktree
                sub_state.child_key = state.child_key
                sub_state.session_dir = state.session_dir
                runner()
        else:
            for child in self.body:
                child_spec = state.stage_specs.get(child.name, state.client)
                if child.model is None:
                    child.model = child_spec.model
                child_state = dataclasses.replace(
                    state, client=state.get_client(child_spec)
                )
                child_state.make_runner(child, scope=self.body)()


