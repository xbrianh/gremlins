"""SequenceStage: run body stages in order, inheriting parent state."""

from __future__ import annotations

import inspect
from typing import Any, cast

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.composite import child_state as _child_state
from gremlins.stages.outcome import Done, Outcome


class SequenceStage(Stage):
    """Run body stages sequentially using child state derived from parent."""

    type = "sequence"

    def __init__(self, name: str, *, body: list[Stage] | None = None) -> None:
        super().__init__(name)
        self.body = body or []
        for c in self.body:
            c.path = f"{name}/{c.name}"

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
        stage = cls(d["name"], body=children)
        stage.client = get_client_from_dict(d)
        return stage

    def run(self, state: State) -> Outcome:
        key = self.path or self.name
        done = state.done_for(key)
        for child in self.body:
            if child.name in done:
                continue
            runner = _child_state(state, child).make_runner(
                child, scope=self.body, record_stage=False
            )
            if inspect.iscoroutinefunction(runner):
                raise TypeError(
                    f"async stage {child.name!r} cannot be nested inside a sequence stage"
                )
            runner()
            state.mark_done(key, child.name)
        return Done()
