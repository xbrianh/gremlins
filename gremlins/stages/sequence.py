"""SequenceStage: run body stages in order, inheriting parent state."""

from __future__ import annotations

from typing import Any, cast

from gremlins.protocols import GremlinProtocol
from gremlins.stages.base import Stage, get_client_from_dict
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
        from gremlins.pipeline.loader import parse_stages

        name = d.get("name") or ""
        raw_children: object = d.get("body") or []
        if not isinstance(raw_children, list):
            raise ValueError(f"stage {name!r}: 'body' must be a list")
        children = parse_stages(cast(list[dict[str, Any]], raw_children), depth=depth)
        stage = cls(name, body=children)
        stage.client = get_client_from_dict(d)
        return stage

    async def run(self, gremlin: GremlinProtocol) -> Outcome:
        from gremlins.executor.state import State

        state = gremlin if isinstance(gremlin, State) else gremlin.state
        key = self.path or self.name
        done = state.done_for(key)
        for child in self.body:
            if child.name in done:
                continue
            state.data.patch(active_children=[child.name])
            runner = _child_state(state, child).make_runner(
                child, scope=self.body, record_stage=False, gremlin=gremlin
            )
            try:
                await runner()
            finally:
                state.data.patch(_delete=("active_children",))
            state.mark_done(key, child.name)
        return Done()
