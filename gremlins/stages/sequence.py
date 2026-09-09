"""SequenceStage: run body stages in order, inheriting parent state."""

from __future__ import annotations

import logging
from typing import TYPE_CHECKING, Any

from _gremlins_core.stages import Done, Outcome

from gremlins.stages.base import Stage, get_client_from_dict
from gremlins.stages.composite import child_state as _child_state

logger = logging.getLogger(__name__)

if TYPE_CHECKING:
    from gremlins.executor.gremlin import Gremlin


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
        from _gremlins_core.schemas import parse_stages

        name = d.get("name") or ""
        raw_children: object = d.get("body") or []
        if not isinstance(raw_children, list):
            raise ValueError(f"stage {name!r}: 'body' must be a list")
        children = parse_stages(raw_children, depth=depth)
        stage = cls(name, body=children)
        client = get_client_from_dict(d)
        stage.client = client
        stage.client_explicit = client is not None
        return stage

    async def run(self, gremlin: Gremlin) -> Outcome:
        state = gremlin.state
        if state is None:
            raise RuntimeError(
                "sequence stage requires gremlin.state to be initialized"
            )
        key = self.path or self.name
        done = state.done_for(key)
        if logger.isEnabledFor(logging.DEBUG):
            logger.debug(
                "sequence %s: running %d children (skipping %d already done)",
                self.name,
                len(self.body),
                len(done),
            )
        for child in self.body:
            if child.name in done:
                if logger.isEnabledFor(logging.DEBUG):
                    logger.debug(
                        "sequence %s: child %s already done, skipping",
                        self.name,
                        child.name,
                    )
                continue
            if logger.isEnabledFor(logging.DEBUG):
                logger.debug(
                    "sequence %s: running child %s",
                    self.name,
                    child.name,
                )
            state.data.patch(active_children=[child.name])
            runner = _child_state(state, child).make_runner(
                child, gremlin, scope=self.body, record_stage=False
            )
            try:
                await runner()
            finally:
                state.data.patch(_delete=("active_children",))
            state.mark_done(key, child.name)
        return Done()
