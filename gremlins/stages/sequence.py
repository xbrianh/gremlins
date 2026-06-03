"""SequenceStage: run body stages in order, inheriting parent state."""

from __future__ import annotations

from typing import Any, cast

from gremlins.executor.state import State
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

    async def run(self, state: State) -> Outcome:
        key = self.path or self.name
        done = state.done_for(key)
        for child in self.body:
            if child.name in done:
                continue
            state.data.patch(active_children=[child.name])
            child_state = _child_state(state, child)
            base_runner = child_state.make_runner(
                child, scope=self.body, record_stage=False
            )

            async def _set_state_and_run() -> Any:
                if child.gremlin is not None:
                    child.gremlin.state = child_state
                return await base_runner()

            try:
                await _set_state_and_run()
            finally:
                state.data.patch(_delete=("active_children",))
            state.mark_done(key, child.name)
        return Done()
