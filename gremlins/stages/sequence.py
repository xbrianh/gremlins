"""SequenceStage: run body stages in order, propagating parent state fields.

Each sub-stage's RuntimeState receives the parent's ``worktree``, ``child_key``,
and ``session_dir`` before its runner is invoked, so a SequenceStage used as a
parallel child correctly inherits the shard-specific worktree and session
directory and any bails route through ``parallel_bails[child_key]``.
"""

from __future__ import annotations

import dataclasses
from collections.abc import Callable
from typing import Any

from gremlins.stages.base import RuntimeState, Stage
from gremlins.stages.registry import register_stage


class SequenceStage(Stage):
    """Run body runners sequentially, propagating worktree/child_key/session_dir."""

    type = "sequence"

    def __init__(
        self,
        name: str,
        *,
        body: list[tuple[RuntimeState, Callable[[], None]]] | None = None,
    ) -> None:
        super().__init__(name, None, [], {})
        self._pre_body = body

    @classmethod
    def from_yaml(cls, d: dict[str, Any]) -> SequenceStage:
        from gremlins.pipeline.loader import _parse_stage, get_client_from_yaml

        children = [_parse_stage(child_d) for child_d in (d.get("body") or [])]
        stage = cls(d["name"])
        stage.body = children
        stage.client = get_client_from_yaml(d)
        return stage

    def run(self, state: RuntimeState) -> None:
        if self._pre_body is not None:
            for sub_state, runner in self._pre_body:
                sub_state.worktree = state.worktree
                sub_state.child_key = state.child_key
                sub_state.session_dir = state.session_dir
                runner()
        else:
            for child in self.body:
                child_spec = state.stage_specs.get(child.name, state.client)
                child_state = dataclasses.replace(
                    state, client=state.get_client(child_spec)
                )
                child_state.make_runner(child, scope=self.body)()


register_stage("sequence", SequenceStage)
